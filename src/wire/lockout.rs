//! Failed-auth lockout policy (issue #58): per-source-IP throttle for
//! repeated failed `auth`/`join` attempts.
//!
//! ## Why this exists, and why the numbers below are what they are
//!
//! `docs/research-security-hijack-dos.md` (branch `research/security-hijack-dos`,
//! §3.1 and §4 item 6) frames this as a genuine team call, not a clear-cut
//! gap: Holler's client credential is a 256-bit random secret behind a
//! constant-time HMAC compare ([`crate::token::TokenStore::verify_credential`]),
//! so brute-forcing it is not a practical threat the way SSH password-guessing
//! is. What this control actually defends against is **connection
//! churn/noise** — a hung or misbehaving client (or a scanner) hammering the
//! accept loop with bad `auth`/`join` frames — which a concurrent-connection
//! cap (issue #57) already substantially covers. This module is the
//! remaining, cheap increment: temporarily refuse new connections from a
//! source that has just demonstrated it can't authenticate.
//!
//! The thresholds mirror fail2ban's own `sshd` jail defaults (the closest
//! documented precedent the research memo found for "a daemon on a box,
//! authenticated by a credential, meant to resist casual/automated abuse"):
//! **5 failures within a 10-minute window triggers a 10-minute lockout**.
//! These are starting points, not load-bearing constants derived from
//! Holler-specific measurement — tune [`MAX_FAILURES`], [`FAILURE_WINDOW`],
//! and [`LOCKOUT_DURATION`] freely if the team wants different numbers; nothing
//! else in this codebase depends on their exact values.
//!
//! ## Design decisions (so the "team decision" framing has a paper trail)
//!
//! - **Keyed by peer IP**, not `token_id`. A bad actor probing `auth` rarely
//!   presents a real `token_id` at all, so a per-token key would be trivial to
//!   evade by cycling ids; per-IP is what SSH/fail2ban do for the same reason.
//! - **In-memory only, no persistence.** A server restart clears every
//!   lockout. That's the right behavior for this threat model (single
//!   operator, not a hostile multi-tenant box) — the same restart also drops
//!   every live connection, so there is nothing meaningfully "remembered"
//!   across a restart to begin with.
//! - **A successful auth does *not* reset a source's failure count.** There is
//!   deliberately no `record_success` method here. Two reasons: (1) it keeps
//!   the tracker to one code path (`record_failure` / `is_locked_out`) instead
//!   of two that both need to agree on bookkeeping; (2) the failure window is
//!   short (10 minutes by default), so unrelated failures age out on their own
//!   shortly after a clean success — there is no long-term "this IP is
//!   permanently tainted" effect to worry about.
//! - **Enforcement point: refuse the raw TCP connection before the WebSocket
//!   handshake even starts** (see `connection::handle_connection`), by
//!   dropping the socket outright rather than completing the handshake and
//!   replying with an `error` frame. This is both the cheapest place to
//!   enforce it (skips the handshake and any frame parsing entirely) and the
//!   best fail-closed shape: it never tells a locked-out caller *why* the
//!   connection failed, which avoids handing a probing client a distinct
//!   "you are rate-limited" signal to react to.
//! - **Any failed `auth` or `join` counts**, not just a credential/secret
//!   mismatch — an unknown `token_id`, an unbound/invalidated/revoked token,
//!   etc. all count too. All of these are "this source cannot get in right
//!   now" outcomes; distinguishing them for lockout purposes would add
//!   complexity without changing the threat this defends against (noise).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Failed `auth`/`join` attempts from one source, within [`FAILURE_WINDOW`],
/// before that source is locked out. Matches fail2ban's `sshd` jail default
/// `maxretry` (`docs/research-security-hijack-dos.md` §3.1).
pub const MAX_FAILURES: u32 = 5;

/// The sliding window failures are counted within. Matches fail2ban's
/// `findtime` default.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(10 * 60);

/// How long a source stays locked out once [`MAX_FAILURES`] is reached within
/// [`FAILURE_WINDOW`]. Matches fail2ban's `bantime` default (some guides
/// tighten this to an hour; kept at 10 minutes here to track the same cited
/// source rather than inventing a second, undocumented number).
pub const LOCKOUT_DURATION: Duration = Duration::from_secs(10 * 60);

/// Per-source bookkeeping: failures still inside the window (oldest first),
/// plus an active lockout expiry, if any.
#[derive(Default)]
struct IpState {
    failures: Vec<Instant>,
    locked_until: Option<Instant>,
}

/// Process-wide, in-memory failed-auth tracker (issue #58). Shared via `Arc`
/// across every connection task, same pattern as [`super::registry::Registry`].
pub struct LockoutTracker {
    by_ip: Mutex<HashMap<IpAddr, IpState>>,
    max_failures: u32,
    window: Duration,
    lockout_duration: Duration,
}

impl Default for LockoutTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl LockoutTracker {
    /// A tracker using the module's documented defaults.
    pub fn new() -> Self {
        Self::with_params(MAX_FAILURES, FAILURE_WINDOW, LOCKOUT_DURATION)
    }

    fn with_params(max_failures: u32, window: Duration, lockout_duration: Duration) -> Self {
        LockoutTracker {
            by_ip: Mutex::new(HashMap::new()),
            max_failures,
            window,
            lockout_duration,
        }
    }

    /// True if `ip` is currently locked out. Opportunistically drops the
    /// entry once an expired lockout is observed, so a source that stops
    /// reconnecting after its ban doesn't linger in the map forever.
    pub fn is_locked_out(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut map = self.by_ip.lock().unwrap();
        match map.get(&ip).and_then(|s| s.locked_until) {
            Some(until) if until > now => true,
            Some(_) => {
                map.remove(&ip);
                false
            }
            None => false,
        }
    }

    /// Record one failed `auth`/`join` attempt from `ip`. Returns `true` if
    /// this attempt just triggered a new lockout (useful for logging at the
    /// call site).
    ///
    /// A no-op (returns `false`, no bookkeeping change) if `ip` is already
    /// locked out — callers are expected to check [`Self::is_locked_out`]
    /// before ever attempting auth for that source, so this branch is
    /// defensive, not the normal path.
    pub fn record_failure(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut map = self.by_ip.lock().unwrap();
        let state = map.entry(ip).or_default();

        if let Some(until) = state.locked_until {
            if until > now {
                return false;
            }
        }

        state.failures.retain(|&t| now.duration_since(t) < self.window);
        state.failures.push(now);

        if state.failures.len() as u32 >= self.max_failures {
            state.failures.clear();
            state.locked_until = Some(now + self.lockout_duration);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::thread::sleep;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, n))
    }

    #[test]
    fn failures_below_threshold_do_not_lock_out() {
        let tracker = LockoutTracker::with_params(3, Duration::from_secs(60), Duration::from_secs(60));
        let addr = ip(1);

        assert!(!tracker.record_failure(addr));
        assert!(!tracker.record_failure(addr));
        assert!(!tracker.is_locked_out(addr));
    }

    #[test]
    fn n_failures_within_window_triggers_lockout() {
        let tracker = LockoutTracker::with_params(3, Duration::from_secs(60), Duration::from_secs(60));
        let addr = ip(2);

        assert!(!tracker.record_failure(addr));
        assert!(!tracker.record_failure(addr));
        assert!(tracker.record_failure(addr));
        assert!(tracker.is_locked_out(addr));
    }

    #[test]
    fn failures_outside_the_window_do_not_count() {
        let tracker =
            LockoutTracker::with_params(3, Duration::from_millis(50), Duration::from_secs(60));
        let addr = ip(3);

        assert!(!tracker.record_failure(addr));
        assert!(!tracker.record_failure(addr));
        sleep(Duration::from_millis(80));
        // Both earlier failures have aged out of the window, so this third
        // call is really "failure #1 of a fresh window" — not enough to lock.
        assert!(!tracker.record_failure(addr));
        assert!(!tracker.is_locked_out(addr));
    }

    #[test]
    fn lockout_expires_after_its_window() {
        let tracker =
            LockoutTracker::with_params(1, Duration::from_secs(60), Duration::from_millis(50));
        let addr = ip(4);

        assert!(tracker.record_failure(addr));
        assert!(tracker.is_locked_out(addr));

        sleep(Duration::from_millis(80));
        assert!(!tracker.is_locked_out(addr));
    }

    #[test]
    fn different_ips_are_tracked_independently() {
        let tracker = LockoutTracker::with_params(1, Duration::from_secs(60), Duration::from_secs(60));
        let a = ip(5);
        let b = ip(6);

        assert!(tracker.record_failure(a));
        assert!(tracker.is_locked_out(a));
        assert!(!tracker.is_locked_out(b));
    }

    #[test]
    fn failures_while_locked_out_do_not_extend_or_reset_the_ban() {
        let tracker =
            LockoutTracker::with_params(1, Duration::from_secs(60), Duration::from_millis(80));
        let addr = ip(7);

        assert!(tracker.record_failure(addr));
        let extended = tracker.record_failure(addr);
        assert!(!extended);

        sleep(Duration::from_millis(100));
        assert!(!tracker.is_locked_out(addr));
    }
}
