//! Roster + presence TTL (issue #32): "who can be hollered at" (ADR
//! 0006), distinct from [`super::registry::Registry`] ("is this
//! `token_id`'s socket open"). A client's socket can be open with zero
//! advertised sessions; the roster only knows about a session once a
//! `presence` frame names it.
//!
//! Keyed by **session name**, not host or token (ADR 0007: "the unit of
//! addressing is a session name... unique on that holler-server").
//!
//! ## State is derived, never stored
//!
//! Every row keeps only `last_seen` (an [`Instant`]); [`RosterState`] is
//! computed fresh from the elapsed time on every read
//! ([`RosterEntry::state_at`]), the same discipline
//! `token::TokenRecord::display_state` already uses for `stale`. This
//! means the tri-state can never drift out of sync with a background
//! sweep that forgot to run, and a caller can never observe a stale
//! "connected" a tick after it should have flipped. The one thing a
//! background sweep is still useful for is bounding memory — pruning
//! rows nobody will ever read again — so [`Roster::prune`] exists
//! for that alone; it does not compute or cache state.
//!
//! ## The numbers, and where they come from
//!
//! `docs/research-dropped-connections.md` (research memo, not yet an
//! ADR) recommends a ~15s client heartbeat and "3 missed heartbeats ==
//! dead" (a near-universal convention it cites in SSH's
//! `ClientAliveCountMax` default and Buzz's `SLOW_CLIENT_GRACE_LIMIT`),
//! i.e. **~45s** of silence before a row stops being trusted as live.
//! That memo explicitly left the `reconnecting -> gone` TTL to this
//! issue, recommending only that it be "a few backoff cycles longer
//! than the dead-socket threshold." The one concrete total-TTL number
//! any prior-art tool in that memo's survey actually ships is Buzz's
//! Redis presence key: `EX 180` (180s from last heartbeat). This
//! implementation adopts that as the `reconnecting -> gone` point
//! measured from the *same* `last_seen`, which works out to ~135s past
//! the 45s dead threshold — roughly four to five of the memo's
//! recommended 30s-cap backoff cycles, matching its "a few cycles
//! longer" guidance while landing on a total that mirrors a real
//! shipped convention rather than an invented number.
//!
//! `docs/research-message-integrity.md` confirms `presence` needs no
//! ack: it is a self-healing heartbeat, and a missed one is tolerated
//! by this TTL, not treated as a delivery failure — which is exactly
//! why state derivation, not acknowledgment, is the right tool here.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `presence`/roster tri-state (ADR 0006, research memo §5(f)).
/// Never fabricated: a row is `Connected` only while a `presence` frame
/// has arrived within [`RosterConfig::reconnect_after`], `Reconnecting`
/// while within [`RosterConfig::gone_after`] beyond that, and `Gone`
/// past it — matching this codebase's existing fail-closed discipline
/// (see `token::ConnectionProbe`'s doc comment for the same tone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterState {
    Connected,
    Reconnecting,
    Gone,
}

impl fmt::Display for RosterState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            RosterState::Connected => "connected",
            RosterState::Reconnecting => "reconnecting",
            RosterState::Gone => "gone",
        };
        f.write_str(s)
    }
}

/// Tunable TTLs, overridable via env for tests (waiting out a real 45s/
/// 180s window in CI is not viable) — mirrors `token`'s
/// `HOLLER_STATE_DIR` convention of an env override for exactly this
/// reason.
#[derive(Debug, Clone, Copy)]
pub struct RosterConfig {
    /// `Connected -> Reconnecting`: no `presence` naming this session
    /// for this long. Default 45s (3 x the research memo's recommended
    /// 15s heartbeat).
    pub reconnect_after: Duration,
    /// `Reconnecting -> Gone`, measured from the same `last_seen` (not
    /// from when `Reconnecting` started). Default 180s, matching Buzz's
    /// cited presence-TTL convention (see module doc).
    pub gone_after: Duration,
    /// A `Gone` row older than this is dropped from memory entirely by
    /// [`Roster::prune`] — bounds memory for a long-running server with
    /// churny sessions while still giving an operator a real window to
    /// see a session went `gone` via `holler roster` before it vanishes.
    /// Default: twice `gone_after`.
    pub prune_after: Duration,
    /// How often the background prune sweep runs. Default 30s; this is
    /// pure memory hygiene, not a correctness knob (state is always
    /// derived fresh — see module doc), so a slow sweep only delays
    /// freeing memory, never produces a wrong answer.
    pub sweep_interval: Duration,
}

impl Default for RosterConfig {
    fn default() -> Self {
        let gone_after = Duration::from_secs(180);
        RosterConfig {
            reconnect_after: Duration::from_secs(45),
            gone_after,
            prune_after: gone_after * 2,
            sweep_interval: Duration::from_secs(30),
        }
    }
}

impl RosterConfig {
    /// Reads `HOLLER_ROSTER_RECONNECT_MS` / `HOLLER_ROSTER_GONE_MS` /
    /// `HOLLER_ROSTER_PRUNE_MS` / `HOLLER_ROSTER_SWEEP_MS` (all in
    /// milliseconds), falling back to [`RosterConfig::default`] for any
    /// that are unset or unparseable. Existing only so integration
    /// tests can exercise `reconnecting`/`gone` transitions in
    /// milliseconds instead of the real 45s/180s window.
    pub fn from_env() -> Self {
        let default = RosterConfig::default();
        let reconnect_after =
            env_duration_ms("HOLLER_ROSTER_RECONNECT_MS").unwrap_or(default.reconnect_after);
        let gone_after = env_duration_ms("HOLLER_ROSTER_GONE_MS").unwrap_or(default.gone_after);
        let prune_after =
            env_duration_ms("HOLLER_ROSTER_PRUNE_MS").unwrap_or(gone_after * 2);
        let sweep_interval =
            env_duration_ms("HOLLER_ROSTER_SWEEP_MS").unwrap_or(default.sweep_interval);
        RosterConfig {
            reconnect_after,
            gone_after,
            prune_after,
            sweep_interval,
        }
    }
}

fn env_duration_ms(key: &str) -> Option<Duration> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
}

/// One session's roster row. `last_seen` is the only mutable fact;
/// everything else about "is it connected" is derived from it.
struct RosterEntry {
    harness: String,
    token_id: String,
    client_id: String,
    last_seen: Instant,
}

impl RosterEntry {
    fn state_at(&self, now: Instant, config: &RosterConfig) -> RosterState {
        let elapsed = now.saturating_duration_since(self.last_seen);
        if elapsed >= config.gone_after {
            RosterState::Gone
        } else if elapsed >= config.reconnect_after {
            RosterState::Reconnecting
        } else {
            RosterState::Connected
        }
    }
}

/// A read-only snapshot row for `holler roster` / the control channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterRow {
    pub name: String,
    pub harness: String,
    pub client_id: String,
    pub state: RosterState,
    pub last_seen_ms_ago: u128,
}

/// Advertising a session name already held by a **different**, still
/// not-`Gone` client (ADR 0007: "collision policy: fail mint/advertise
/// if the name is taken"). A `Gone` prior owner's name is free to
/// reclaim — a session that has aged out is no longer "held" by anyone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterConflict {
    pub session: String,
    pub held_by_client_id: String,
}

impl fmt::Display for RosterConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "session {:?} is already advertised by client {}",
            self.session, self.held_by_client_id
        )
    }
}

impl std::error::Error for RosterConflict {}

/// Shared, process-wide table of advertised sessions, keyed by session
/// name (ADR 0007). One [`Roster`] is shared (via `Arc`) by every
/// connection task in a `serve()` process, the same way
/// [`super::registry::Registry`] is.
pub struct Roster {
    entries: Mutex<HashMap<String, RosterEntry>>,
    config: RosterConfig,
}

impl Roster {
    pub fn new(config: RosterConfig) -> Self {
        Roster {
            entries: Mutex::new(HashMap::new()),
            config,
        }
    }

    pub fn sweep_interval(&self) -> Duration {
        self.config.sweep_interval
    }

    /// Record (or refresh) a session advertisement: sets `last_seen` to
    /// now, which is what makes the row `Connected` again regardless of
    /// what state it had drifted to (including `Gone` — a session can
    /// always come back under the same client_id).
    ///
    /// Rejects (without mutating anything) a name already held by a
    /// different, still-live (`Connected`/`Reconnecting`) client — see
    /// [`RosterConflict`]. The caller (connection.rs) does not surface
    /// this as a wire `error`: `presence` is a one-way, unacked
    /// heartbeat (research memo, message integrity, §3) and a rejected
    /// advertise is simply retried on the next heartbeat tick, the same
    /// way a dropped `presence` frame self-heals.
    pub fn advertise(
        &self,
        name: String,
        harness: String,
        token_id: &str,
        client_id: &str,
    ) -> Result<(), RosterConflict> {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("roster mutex poisoned");
        if let Some(existing) = entries.get(&name) {
            if existing.token_id != token_id && existing.state_at(now, &self.config) != RosterState::Gone
            {
                return Err(RosterConflict {
                    session: name,
                    held_by_client_id: existing.client_id.clone(),
                });
            }
        }
        entries.insert(
            name,
            RosterEntry {
                harness,
                token_id: token_id.to_string(),
                client_id: client_id.to_string(),
                last_seen: now,
            },
        );
        Ok(())
    }

    /// Resolve a session name to the `token_id` of the live connection
    /// currently hosting it (ADR 0007: "address sessions, not hosts") —
    /// for `holler say`/`prompt` routing (issue #33). `None` if no row
    /// exists at all, or the row has aged past [`RosterState::Gone`]: a
    /// stale route is refused the same as no route (the caller surfaces
    /// `unknown_session`, not a prompt into the void).
    pub fn resolve_session(&self, name: &str) -> Option<String> {
        let now = Instant::now();
        let entries = self.entries.lock().expect("roster mutex poisoned");
        entries.get(name).and_then(|e| {
            if e.state_at(now, &self.config) == RosterState::Gone {
                None
            } else {
                Some(e.token_id.clone())
            }
        })
    }

    /// Count of rows not yet [`RosterState::Gone`] (issue #81's `query
    /// status`/`caps` `sessions` field) — distinct from
    /// [`super::registry::Registry::len`], which counts open sockets,
    /// not advertised sessions; a socket can be open with zero sessions,
    /// or a session can still be `Reconnecting` after its socket drops.
    pub fn live_count(&self) -> usize {
        let now = Instant::now();
        let entries = self.entries.lock().expect("roster mutex poisoned");
        entries
            .values()
            .filter(|e| e.state_at(now, &self.config) != RosterState::Gone)
            .count()
    }

    /// Snapshot of every row, with state derived as of now (for `holler
    /// roster` / the control channel).
    pub fn snapshot(&self) -> Vec<RosterRow> {
        let now = Instant::now();
        let entries = self.entries.lock().expect("roster mutex poisoned");
        let mut rows: Vec<RosterRow> = entries
            .iter()
            .map(|(name, e)| RosterRow {
                name: name.clone(),
                harness: e.harness.clone(),
                client_id: e.client_id.clone(),
                state: e.state_at(now, &self.config),
                last_seen_ms_ago: now.saturating_duration_since(e.last_seen).as_millis(),
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Drop rows that have been `Gone` for longer than
    /// `config.prune_after` — pure memory hygiene (see module doc);
    /// never changes what [`Roster::snapshot`] reports for a row still
    /// within that window.
    pub fn prune(&self) {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("roster mutex poisoned");
        entries.retain(|_, e| now.saturating_duration_since(e.last_seen) < self.config.prune_after);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().expect("roster mutex poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wide margins between thresholds (and between a sleep and the
    /// threshold it's meant to cross) on purpose: a loaded, shared CI
    /// runner can stretch a `thread::sleep` well past its nominal
    /// duration under scheduler contention (observed in CI: a 50ms
    /// sleep landing past a 90ms threshold). These numbers are sized so
    /// that even several-hundred-ms of scheduling slop still lands on
    /// the intended side of each threshold.
    fn tiny_config() -> RosterConfig {
        RosterConfig {
            reconnect_after: Duration::from_millis(300),
            gone_after: Duration::from_millis(2000),
            prune_after: Duration::from_millis(5000),
            sweep_interval: Duration::from_millis(10),
        }
    }

    #[test]
    fn advertise_is_connected_immediately() {
        let roster = Roster::new(tiny_config());
        roster
            .advertise("alpha".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();
        let rows = roster.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "alpha");
        assert_eq!(rows[0].harness, "opencode");
        assert_eq!(rows[0].client_id, "cli_1");
        assert_eq!(rows[0].state, RosterState::Connected);
    }

    #[test]
    fn missed_heartbeats_transition_to_reconnecting_then_gone() {
        let roster = Roster::new(tiny_config());
        roster
            .advertise("alpha".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();

        std::thread::sleep(Duration::from_millis(700));
        let rows = roster.snapshot();
        assert_eq!(rows[0].state, RosterState::Reconnecting);

        std::thread::sleep(Duration::from_millis(2000));
        let rows = roster.snapshot();
        assert_eq!(rows[0].state, RosterState::Gone);
    }

    #[test]
    fn live_count_excludes_gone_but_includes_reconnecting() {
        let roster = Roster::new(tiny_config());
        roster
            .advertise("alpha".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();
        roster
            .advertise("beta".into(), "opencode".into(), "tok_2", "cli_2")
            .unwrap();
        assert_eq!(roster.live_count(), 2);

        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(roster.snapshot()[0].state, RosterState::Reconnecting);
        // Reconnecting still counts as live — only `Gone` is excluded.
        assert_eq!(roster.live_count(), 2);

        std::thread::sleep(Duration::from_millis(2000));
        assert_eq!(roster.live_count(), 0);
    }

    #[test]
    fn re_advertise_from_reconnecting_goes_back_to_connected() {
        let roster = Roster::new(tiny_config());
        roster
            .advertise("alpha".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();

        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(roster.snapshot()[0].state, RosterState::Reconnecting);

        roster
            .advertise("alpha".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();
        assert_eq!(roster.snapshot()[0].state, RosterState::Connected);
    }

    #[test]
    fn sibling_sessions_on_the_same_client_are_independent() {
        let roster = Roster::new(tiny_config());
        roster
            .advertise("alpha".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();
        roster
            .advertise("beta".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();

        std::thread::sleep(Duration::from_millis(700));
        // Refresh only `alpha`; `beta` keeps aging on its own.
        roster
            .advertise("alpha".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();

        let rows = roster.snapshot();
        let alpha = rows.iter().find(|r| r.name == "alpha").unwrap();
        let beta = rows.iter().find(|r| r.name == "beta").unwrap();
        assert_eq!(alpha.state, RosterState::Connected);
        assert_eq!(beta.state, RosterState::Reconnecting);
    }

    #[test]
    fn a_different_still_live_client_cannot_steal_a_name() {
        let roster = Roster::new(tiny_config());
        roster
            .advertise("alpha".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();

        let err = roster
            .advertise("alpha".into(), "opencode".into(), "tok_2", "cli_2")
            .unwrap_err();
        assert_eq!(err.session, "alpha");
        assert_eq!(err.held_by_client_id, "cli_1");

        // The original owner's row is untouched by the failed steal.
        assert_eq!(roster.snapshot()[0].client_id, "cli_1");
    }

    #[test]
    fn a_name_can_be_reclaimed_once_the_prior_owner_is_gone() {
        let roster = Roster::new(tiny_config());
        roster
            .advertise("alpha".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();

        std::thread::sleep(Duration::from_millis(2700));
        assert_eq!(roster.snapshot()[0].state, RosterState::Gone);

        roster
            .advertise("alpha".into(), "opencode".into(), "tok_2", "cli_2")
            .unwrap();
        let rows = roster.snapshot();
        assert_eq!(rows[0].client_id, "cli_2");
        assert_eq!(rows[0].state, RosterState::Connected);
    }

    #[test]
    fn resolve_session_finds_the_hosting_token_id() {
        let roster = Roster::new(tiny_config());
        roster
            .advertise("alpha".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();
        assert_eq!(roster.resolve_session("alpha"), Some("tok_1".to_string()));
        assert_eq!(roster.resolve_session("nope"), None);
    }

    #[test]
    fn resolve_session_is_none_once_the_row_is_gone() {
        let roster = Roster::new(tiny_config());
        roster
            .advertise("alpha".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();

        std::thread::sleep(Duration::from_millis(2700));
        assert_eq!(roster.snapshot()[0].state, RosterState::Gone);
        assert_eq!(roster.resolve_session("alpha"), None);
    }

    #[test]
    fn prune_removes_only_rows_gone_past_prune_after() {
        // A short `gone_after` and a far-off `prune_after`, so there's
        // a wide, jitter-tolerant window between "already gone" and
        // "old enough to prune" (see `tiny_config`'s doc comment on
        // why these margins matter under CI scheduling contention).
        let roster = Roster::new(RosterConfig {
            reconnect_after: Duration::from_millis(50),
            gone_after: Duration::from_millis(200),
            prune_after: Duration::from_millis(3000),
            sweep_interval: Duration::from_millis(10),
        });
        roster
            .advertise("alpha".into(), "opencode".into(), "tok_1", "cli_1")
            .unwrap();

        std::thread::sleep(Duration::from_millis(600));
        roster.prune();
        assert_eq!(roster.len(), 1, "gone but within prune_after stays");

        std::thread::sleep(Duration::from_millis(3200));
        roster.prune();
        assert_eq!(roster.len(), 0, "past prune_after is removed");
    }
}
