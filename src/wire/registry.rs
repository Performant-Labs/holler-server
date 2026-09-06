//! Live-connection registry (issue #31): tracks which bound `token_id`s
//! currently have an open WebSocket, so `holler token ping` can report a
//! real hostname + RTT instead of always [`ConnectionStatus::Disconnected`].
//!
//! One [`Registry`] is shared (via `Arc`) by every connection task in a
//! `serve()` process. A connection registers itself on a successful
//! `auth` and deregisters when the socket closes. [`Registry::ping`]
//! drives a real `ping`/`pong` round trip over that connection's outbound
//! channel and resolves once the matching `pong` arrives (or times out).
//!
//! [`Registry::query`] (issue #37) generalizes that same request/response
//! pattern to an arbitrary outbound `query`/`args`, for `holler status
//! <id>` / `support <id> <feature>` / `caps <id>` / `query <id> <cmd>
//! [args...]` — sending a `query` to one already-connected client and
//! relaying back whatever `query_ok` (or `error`, or nothing within
//! [`QUERY_TIMEOUT`]) comes back.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::proto::{Envelope, ErrorBody, QueryOkBody, ReplyBody};
use crate::token::ConnectionStatus;

use super::query::HarnessConfirmation;

/// How long [`Registry::ping`] waits for the matching `pong` before
/// reporting the connection disconnected. Generous for a loopback round
/// trip, short enough that `holler token ping` does not hang.
pub const PING_TIMEOUT: Duration = Duration::from_secs(3);

/// How long [`Registry::query`] waits for the matching `query_ok`/`error`
/// before reporting the connection disconnected. Same budget as
/// [`PING_TIMEOUT`] — both are one round trip over an already-open
/// loopback socket.
pub const QUERY_TIMEOUT: Duration = PING_TIMEOUT;

/// How long [`Registry::prompt`] waits between `reply` frames (partial or
/// final) before giving up. Reset on every partial reply, not a single
/// deadline for the whole exchange — a session streaming output every few
/// seconds should not time out just because a full turn takes minutes.
/// Far more generous than [`QUERY_TIMEOUT`]: a `query` is a local
/// peer-to-peer round trip, a `prompt` may wait on a real model turn.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long [`Registry::interrupt`] waits for the matching `ack` before
/// reporting the outcome as [`InterruptOutcome::TimedOut`] (issue #34,
/// ADR 0005; the two-state distinction is issue #54's). This is its own
/// constant, not a reuse of [`PING_TIMEOUT`]/[`QUERY_TIMEOUT`], because it
/// answers a different question ("did the client *apply* the cancel?",
/// spec-note from the message-integrity memo, issue #59(b)) even though
/// today's value happens to match: a real `session/cancel` is still a
/// single local round trip (no model turn to wait on, unlike
/// [`PROMPT_TIMEOUT`]), so the same "generous for a loopback round trip"
/// budget applies. Deliberately far shorter than the roster's ~45s
/// dead-connection/heartbeat-miss threshold (`super::roster::RosterConfig
/// ::reconnect_after`) — issue #54 requires these stay two separate
/// clocks, never conflated into one error. Must also stay comfortably
/// under `super::control::CLIENT_TIMEOUT` (5s), which bounds the whole
/// control-channel round trip a `holler interrupt` CLI process waits on,
/// or the CLI would report "no live server" before this timeout ever gets
/// to fire. The exact number is a judgment call (message-integrity memo:
/// "pick a clip 2–3x normal RTT and revisit after real usage") — 3s here,
/// same figure this codebase already treats as "generous" for one
/// loopback hop; revisit once real interrupt latency is observed.
pub const INTERRUPT_ACK_TIMEOUT: Duration = Duration::from_secs(3);

/// The outcome of [`Registry::query`]. `Disconnected` covers every
/// failure mode uniformly (no such connection, the outbound channel is
/// closed, or no reply arrived within [`QUERY_TIMEOUT`]) — mirrors
/// [`Registry::ping`]'s `ConnectionStatus::Disconnected`.
pub enum QueryOutcome {
    Ok(QueryOkBody),
    Err(ErrorBody),
    Disconnected,
}

/// A reply this process is waiting on for one outstanding outbound
/// `query` — either side of what the target answered.
enum PendingQueryReply {
    Ok(QueryOkBody),
    Err(ErrorBody),
}

/// The outcome of [`Registry::prompt`]: the concatenated text of every
/// `reply` frame received up to and including the one with `done: true`
/// (spec §10: `text` and/or `chunks`, joined in arrival order), the
/// target's own `error` in place of a reply, `Disconnected` — covering
/// "no such connection", "the outbound channel is closed", and "no
/// `reply` arrived within [`PROMPT_TIMEOUT`]" uniformly, same as
/// [`QueryOutcome::Disconnected`] — or `Cancelled` (issue #82): a
/// **different** CLI invocation's `holler interrupt` for this exact
/// session landed (and was acked) while this prompt was still waiting.
/// Kept apart from `Disconnected` on purpose — the connection (and the
/// server) are both still very much alive; only this one turn was cut
/// short deliberately, which is a different fact than "unreachable" and
/// deserves its own CLI-facing message rather than the misleading
/// generic one (see `main.rs`'s `run_say`).
pub enum PromptOutcome {
    Done { text: String, exit: Option<i64> },
    Err(ErrorBody),
    Disconnected,
    Cancelled,
}

/// One event this process's pending `prompt` is waiting on: a `reply`
/// frame (partial or final), the target's own `error` in place of one, or
/// `Cancelled` (issue #82) — pushed by [`Registry::interrupt`] once its
/// `ack` arrives for the very same session, so a concurrently waiting
/// [`Registry::prompt`] wakes immediately instead of idling out to
/// [`PROMPT_TIMEOUT`] or a real (but here irrelevant) disconnect.
enum PromptEvent {
    Reply(ReplyBody),
    Err(ErrorBody),
    Cancelled,
}

/// The outcome of [`Registry::interrupt`] (issue #34, ADR 0005). Unlike
/// [`QueryOutcome`]/[`PromptOutcome`], failure is **not** collapsed into
/// one `Disconnected` case: issue #54 requires telling "the connection
/// died while the interrupt was outstanding" apart from "the connection
/// is still alive, but its `ack` didn't arrive in time" — the operator-
/// facing difference between "not connected" and "the cancel may not
/// have landed."
pub enum InterruptOutcome {
    /// The client sent `ack` (`of` matching this interrupt's id) within
    /// [`INTERRUPT_ACK_TIMEOUT`] — the turn is cancelled.
    Acked,
    /// [`INTERRUPT_ACK_TIMEOUT`] elapsed with no matching `ack`, but the
    /// connection is (as of the check right after) still registered —
    /// the socket looks fine, but the cancel may not have taken effect.
    TimedOut,
    /// No such connection at all, or it closed (registry entry removed)
    /// before or while this interrupt was outstanding.
    Disconnected,
}

/// One live connection's outward-facing handle: enough for the registry
/// to push frames at it (`ping`, `query`) and to answer `holler
/// status`'s client count / hostnames.
struct Entry {
    hostname: String,
    client_id: String,
    out_tx: mpsc::UnboundedSender<Envelope>,
    /// Outstanding `ping`s this connection has not yet answered,
    /// keyed by the envelope `id` the pong must echo back.
    pending: Mutex<HashMap<String, (Instant, oneshot::Sender<Duration>)>>,
    /// Outstanding outbound `query`s this connection has not yet
    /// answered, keyed by the envelope `id` the reply must echo back.
    pending_queries: Mutex<HashMap<String, oneshot::Sender<PendingQueryReply>>>,
    /// Outstanding outbound `prompt`s this connection has not yet
    /// finished answering, keyed by the envelope `id` every `reply`
    /// (partial or final) must echo back, alongside the session name it
    /// was sent for. An `mpsc`, not a `oneshot` (unlike
    /// `pending`/`pending_queries`): a `prompt` may see several
    /// `done: false` replies before the one that finishes it. The session
    /// name (issue #82) lets [`Registry::interrupt`] find and wake the
    /// pending prompt(s) for the session it just got an `ack` for, without
    /// knowing their prompt ids.
    pending_prompts: Mutex<HashMap<String, (String, mpsc::UnboundedSender<PromptEvent>)>>,
    /// Outstanding outbound `interrupt`s this connection has not yet
    /// `ack`ed, keyed by the interrupt envelope's `id` (which the `ack`
    /// body's `of` must echo back). Deliberately its own map, never
    /// `pending_prompts`: an `interrupt` is a control frame (ADR 0005),
    /// not a queued prompt, and must resolve/expire independently of any
    /// `prompt` in flight on the same connection.
    pending_interrupts: Mutex<HashMap<String, oneshot::Sender<()>>>,
}

/// Shared, process-wide table of live connections, keyed by `token_id`,
/// plus the harness ids a live client has actually confirmed (ADR 0001:
/// "known vs. confirmed" — never populated from a `hello` advertisement
/// alone, only from a successful `support` round trip; see
/// [`Registry::record_harness_confirmed`]).
#[derive(Default)]
pub struct Registry {
    entries: Mutex<HashMap<String, Entry>>,
    confirmed_harnesses: Mutex<HashMap<String, HashSet<String>>>,
}

impl Registry {
    pub fn new() -> Self {
        Registry::default()
    }

    /// Register a newly authenticated connection. Replaces any prior
    /// entry for the same `token_id` (a reconnect from the same client,
    /// or a replayed credential, supersedes its old, possibly still-live
    /// socket) — and actually closes that old socket: the superseded
    /// connection is sent a `session_superseded` `error` first, then its
    /// `out_tx` is dropped, closing the channel its `handle_connection`
    /// task is selecting on; that task's next `out_rx.recv()` sees the
    /// close and breaks its loop, dropping the WebSocket (issue #56).
    pub fn insert(
        &self,
        token_id: &str,
        client_id: String,
        hostname: String,
        out_tx: mpsc::UnboundedSender<Envelope>,
    ) {
        let previous = {
            let mut entries = self.entries.lock().expect("registry mutex poisoned");
            entries.insert(
                token_id.to_string(),
                Entry {
                    hostname,
                    client_id,
                    out_tx,
                    pending: Mutex::new(HashMap::new()),
                    pending_queries: Mutex::new(HashMap::new()),
                    pending_prompts: Mutex::new(HashMap::new()),
                    pending_interrupts: Mutex::new(HashMap::new()),
                },
            )
        };
        if let Some(previous) = previous {
            let notice = crate::proto::error_with_message(
                crate::proto::CODE_SESSION_SUPERSEDED,
                "a new connection authenticated for this token; closing",
                "",
                "server",
            );
            let _ = previous.out_tx.send(notice);
            // `previous` drops here, taking its `out_tx` with it.
        }
    }

    /// Update the recorded hostname for a connection once its `hello`
    /// arrives (auth alone only knows the redeem-time `machine` name).
    pub fn set_hostname(&self, token_id: &str, hostname: String) {
        let mut entries = self.entries.lock().expect("registry mutex poisoned");
        if let Some(entry) = entries.get_mut(token_id) {
            entry.hostname = hostname;
        }
    }

    /// Remove a connection — socket closed/errored, or a live force-close
    /// requested over the control channel (`holler token delete` / `client
    /// detach`, issue #78). Returns whether an entry was actually present.
    /// Dropping the removed [`Entry`] here drops its `out_tx`, which is
    /// what actually closes a still-live socket: `handle_connection`'s
    /// session loop is selecting on that channel, and its next
    /// `out_rx.recv()` sees the close and breaks the loop, same as the
    /// supersede path in [`Registry::insert`].
    pub fn remove(&self, token_id: &str) -> bool {
        let mut entries = self.entries.lock().expect("registry mutex poisoned");
        entries.remove(token_id).is_some()
    }

    /// How many live connections are currently registered (for `holler
    /// status`'s `clients` count).
    pub fn len(&self) -> usize {
        self.entries.lock().expect("registry mutex poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Called from a connection task when a `pong` arrives: resolves the
    /// matching pending ping (if any) with the elapsed RTT.
    pub fn resolve_pong(&self, token_id: &str, reply_to: &str) {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        if let Some(entry) = entries.get(token_id) {
            let mut pending = entry.pending.lock().expect("pending-ping mutex poisoned");
            if let Some((sent_at, tx)) = pending.remove(reply_to) {
                let _ = tx.send(sent_at.elapsed());
            }
        }
    }

    /// Drive a real `ping` round trip against `token_id`'s live
    /// connection. `Disconnected` covers every failure mode uniformly
    /// (no such connection, the outbound channel is closed, or no
    /// `pong` arrived within [`PING_TIMEOUT`]) — `holler token ping`
    /// does not need to distinguish them.
    pub async fn ping(
        &self,
        token_id: &str,
        ping_id: String,
        server_hostname: &str,
    ) -> ConnectionStatus {
        let (out_tx, hostname) = {
            let entries = self.entries.lock().expect("registry mutex poisoned");
            match entries.get(token_id) {
                Some(entry) => (entry.out_tx.clone(), entry.hostname.clone()),
                None => return ConnectionStatus::Disconnected,
            }
        };

        let (tx, rx) = oneshot::channel();
        {
            let entries = self.entries.lock().expect("registry mutex poisoned");
            let Some(entry) = entries.get(token_id) else {
                return ConnectionStatus::Disconnected;
            };
            let mut pending = entry.pending.lock().expect("pending-ping mutex poisoned");
            pending.insert(ping_id.clone(), (Instant::now(), tx));
        }

        let envelope = crate::wire::hello::new_ping_envelope(&ping_id, server_hostname);
        if out_tx.send(envelope).is_err() {
            self.forget_pending(token_id, &ping_id);
            return ConnectionStatus::Disconnected;
        }

        match tokio::time::timeout(PING_TIMEOUT, rx).await {
            Ok(Ok(rtt)) => ConnectionStatus::Connected {
                hostname,
                rtt_ms: rtt.as_millis() as u64,
            },
            _ => {
                self.forget_pending(token_id, &ping_id);
                ConnectionStatus::Disconnected
            }
        }
    }

    fn forget_pending(&self, token_id: &str, ping_id: &str) {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        if let Some(entry) = entries.get(token_id) {
            let mut pending = entry.pending.lock().expect("pending-ping mutex poisoned");
            pending.remove(ping_id);
        }
    }

    /// Snapshot of `(token_id, hostname, client_id)` for every live
    /// connection, for `holler status`'s client listing.
    pub fn snapshot(&self) -> Vec<(String, String, String)> {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        entries
            .iter()
            .map(|(token_id, e)| (token_id.clone(), e.hostname.clone(), e.client_id.clone()))
            .collect()
    }

    /// Resolve a `holler status/support/caps/query <id>` target to the
    /// live `token_id` it names (spec §8: "token id, client id,
    /// hostname, or session → hosting client" — sessions are not wired
    /// up by any story yet, so only the first three are checked here).
    /// `None` means no live connection matches `id` at all.
    pub fn resolve_target(&self, id: &str) -> Option<String> {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        if entries.contains_key(id) {
            return Some(id.to_string());
        }
        entries
            .iter()
            .find(|(_, e)| e.hostname == id || e.client_id == id)
            .map(|(token_id, _)| token_id.clone())
    }

    /// The recorded hostname for a live connection, for attributing a
    /// confirmed `support` answer to the client that gave it.
    pub fn hostname_of(&self, token_id: &str) -> Option<String> {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        entries.get(token_id).map(|e| e.hostname.clone())
    }

    /// Record that `hostname` has confirmed it supports `harness`
    /// (ADR 0001: only ever called after a live `support` round trip
    /// answered `ok: true` — never from a `hello` advertisement).
    pub fn record_harness_confirmed(&self, harness: &str, hostname: &str) {
        let mut confirmed = self
            .confirmed_harnesses
            .lock()
            .expect("confirmed-harnesses mutex poisoned");
        confirmed
            .entry(harness.to_string())
            .or_default()
            .insert(hostname.to_string());
    }

    /// Snapshot of every harness this process has, over its lifetime,
    /// seen a live client confirm — for `status`/`caps`'s
    /// `harnesses_confirmed`. Sorted for stable output.
    pub fn confirmed_harnesses_snapshot(&self) -> Vec<HarnessConfirmation> {
        let confirmed = self
            .confirmed_harnesses
            .lock()
            .expect("confirmed-harnesses mutex poisoned");
        let mut rows: Vec<HarnessConfirmation> = confirmed
            .iter()
            .map(|(id, hosts)| {
                let mut clients: Vec<String> = hosts.iter().cloned().collect();
                clients.sort();
                HarnessConfirmation {
                    id: id.clone(),
                    clients,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        rows
    }

    /// Drive a real outbound `query` round trip against `token_id`'s
    /// live connection (issue #37's generalization of [`Registry::ping`]
    /// to arbitrary `cmd`/`args`). `Disconnected` covers every failure
    /// mode uniformly, same as `ping`.
    pub async fn query(
        &self,
        token_id: &str,
        cmd: String,
        args: Vec<String>,
        query_id: String,
    ) -> QueryOutcome {
        let out_tx = {
            let entries = self.entries.lock().expect("registry mutex poisoned");
            match entries.get(token_id) {
                Some(entry) => entry.out_tx.clone(),
                None => return QueryOutcome::Disconnected,
            }
        };

        let (tx, rx) = oneshot::channel();
        {
            let entries = self.entries.lock().expect("registry mutex poisoned");
            let Some(entry) = entries.get(token_id) else {
                return QueryOutcome::Disconnected;
            };
            let mut pending = entry
                .pending_queries
                .lock()
                .expect("pending-query mutex poisoned");
            pending.insert(query_id.clone(), tx);
        }

        let envelope = crate::wire::hello::new_query_envelope(&query_id, cmd, args);
        if out_tx.send(envelope).is_err() {
            self.forget_pending_query(token_id, &query_id);
            return QueryOutcome::Disconnected;
        }

        match tokio::time::timeout(QUERY_TIMEOUT, rx).await {
            Ok(Ok(PendingQueryReply::Ok(body))) => QueryOutcome::Ok(body),
            Ok(Ok(PendingQueryReply::Err(body))) => QueryOutcome::Err(body),
            _ => {
                self.forget_pending_query(token_id, &query_id);
                QueryOutcome::Disconnected
            }
        }
    }

    fn forget_pending_query(&self, token_id: &str, query_id: &str) {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        if let Some(entry) = entries.get(token_id) {
            let mut pending = entry
                .pending_queries
                .lock()
                .expect("pending-query mutex poisoned");
            pending.remove(query_id);
        }
    }

    /// Called from a connection task when a `query_ok` arrives:
    /// resolves the matching pending outbound `query` (if any).
    pub fn resolve_query_ok(&self, token_id: &str, reply_to: &str, body: QueryOkBody) {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        if let Some(entry) = entries.get(token_id) {
            let mut pending = entry
                .pending_queries
                .lock()
                .expect("pending-query mutex poisoned");
            if let Some(tx) = pending.remove(reply_to) {
                let _ = tx.send(PendingQueryReply::Ok(body));
            }
        }
    }

    /// Called from a connection task when an `error` arrives: resolves
    /// the matching pending outbound `query` (if any) — this is how a
    /// remote client's own `unknown_cmd` (or any other fail-closed
    /// answer) reaches `holler support/status/caps/query <id>`.
    pub fn resolve_query_err(&self, token_id: &str, reply_to: &str, body: ErrorBody) {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        if let Some(entry) = entries.get(token_id) {
            let mut pending = entry
                .pending_queries
                .lock()
                .expect("pending-query mutex poisoned");
            if let Some(tx) = pending.remove(reply_to) {
                let _ = tx.send(PendingQueryReply::Err(body));
            }
        }
    }

    /// Drive a `prompt` -> `reply`* round trip against `token_id`'s live
    /// connection (issue #33: routing `holler say <session>` by session
    /// name via the roster). Unlike [`Registry::query`], the target may
    /// answer with several `done: false` replies before the one that
    /// finishes the exchange (spec §10) — this collects `text`/`chunks`
    /// from every reply, in arrival order, and returns the concatenation
    /// once `done: true` arrives.
    pub async fn prompt(
        &self,
        token_id: &str,
        prompt_id: String,
        session: String,
        text: String,
        meta: Option<Value>,
    ) -> PromptOutcome {
        let out_tx = {
            let entries = self.entries.lock().expect("registry mutex poisoned");
            match entries.get(token_id) {
                Some(entry) => entry.out_tx.clone(),
                None => return PromptOutcome::Disconnected,
            }
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        {
            let entries = self.entries.lock().expect("registry mutex poisoned");
            let Some(entry) = entries.get(token_id) else {
                return PromptOutcome::Disconnected;
            };
            let mut pending = entry
                .pending_prompts
                .lock()
                .expect("pending-prompt mutex poisoned");
            pending.insert(prompt_id.clone(), (session.clone(), tx));
        }

        let envelope = crate::wire::hello::new_prompt_envelope(&prompt_id, session, text, meta);
        if out_tx.send(envelope).is_err() {
            self.forget_pending_prompt(token_id, &prompt_id);
            return PromptOutcome::Disconnected;
        }

        let mut pieces: Vec<String> = Vec::new();
        loop {
            match tokio::time::timeout(PROMPT_TIMEOUT, rx.recv()).await {
                Ok(Some(PromptEvent::Reply(reply))) => {
                    if let Some(t) = reply.text {
                        pieces.push(t);
                    }
                    pieces.extend(reply.chunks);
                    if reply.done {
                        self.forget_pending_prompt(token_id, &prompt_id);
                        return PromptOutcome::Done {
                            text: pieces.concat(),
                            exit: reply.exit,
                        };
                    }
                }
                Ok(Some(PromptEvent::Err(body))) => {
                    self.forget_pending_prompt(token_id, &prompt_id);
                    return PromptOutcome::Err(body);
                }
                Ok(Some(PromptEvent::Cancelled)) => {
                    self.forget_pending_prompt(token_id, &prompt_id);
                    return PromptOutcome::Cancelled;
                }
                Ok(None) | Err(_) => {
                    self.forget_pending_prompt(token_id, &prompt_id);
                    return PromptOutcome::Disconnected;
                }
            }
        }
    }

    fn forget_pending_prompt(&self, token_id: &str, prompt_id: &str) {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        if let Some(entry) = entries.get(token_id) {
            let mut pending = entry
                .pending_prompts
                .lock()
                .expect("pending-prompt mutex poisoned");
            pending.remove(prompt_id);
        }
    }

    /// Called from a connection task when a `reply` arrives: forwards it
    /// to the matching pending outbound `prompt` (if any). Uses `get`,
    /// not `remove` — a `done: false` reply is not the last one, so
    /// [`Registry::prompt`]'s own loop (not this call) decides when to
    /// stop listening (see [`Registry::forget_pending_prompt`]).
    pub fn resolve_reply(&self, token_id: &str, reply_to: &str, body: ReplyBody) {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        if let Some(entry) = entries.get(token_id) {
            let pending = entry
                .pending_prompts
                .lock()
                .expect("pending-prompt mutex poisoned");
            if let Some((_, tx)) = pending.get(reply_to) {
                let _ = tx.send(PromptEvent::Reply(body));
            }
        }
    }

    /// Called from a connection task when an `error` arrives: forwards it
    /// to the matching pending outbound `prompt` (if any) — this is how a
    /// remote client's own `unknown_session` (a stale `presence` row: the
    /// roster thought it hosted this session, the client disagrees)
    /// reaches `holler say`. A no-op if `reply_to` matches no outstanding
    /// prompt, the same way [`Registry::resolve_query_err`] is a no-op
    /// for an unmatched `query`.
    pub fn resolve_prompt_err(&self, token_id: &str, reply_to: &str, body: ErrorBody) {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        if let Some(entry) = entries.get(token_id) {
            let pending = entry
                .pending_prompts
                .lock()
                .expect("pending-prompt mutex poisoned");
            if let Some((_, tx)) = pending.get(reply_to) {
                let _ = tx.send(PromptEvent::Err(body));
            }
        }
    }

    /// Send `interrupt` for `session` to `token_id`'s live connection and
    /// wait for the matching `ack` (issue #34, ADR 0005). A **control**
    /// frame: this never touches `pending_prompts`, so it is sent over the
    /// connection's outbound channel immediately, even while a `prompt`
    /// for this (or a sibling) session is still awaiting its `reply` on
    /// the very same connection — the unbounded `mpsc` outbound channel
    /// carries both independently, in send order, with no queueing beyond
    /// that.
    ///
    /// Unlike [`Registry::ping`]/[`Registry::query`]/[`Registry::prompt`],
    /// failure is not collapsed into one `Disconnected` outcome — see
    /// [`InterruptOutcome`]'s doc comment for why (issue #54).
    pub async fn interrupt(
        &self,
        token_id: &str,
        interrupt_id: String,
        session: String,
    ) -> InterruptOutcome {
        let out_tx = {
            let entries = self.entries.lock().expect("registry mutex poisoned");
            match entries.get(token_id) {
                Some(entry) => entry.out_tx.clone(),
                None => return InterruptOutcome::Disconnected,
            }
        };

        let (tx, rx) = oneshot::channel();
        {
            let entries = self.entries.lock().expect("registry mutex poisoned");
            let Some(entry) = entries.get(token_id) else {
                return InterruptOutcome::Disconnected;
            };
            let mut pending = entry
                .pending_interrupts
                .lock()
                .expect("pending-interrupt mutex poisoned");
            pending.insert(interrupt_id.clone(), tx);
        }

        let envelope = crate::wire::hello::new_interrupt_envelope(&interrupt_id, session.clone());
        if out_tx.send(envelope).is_err() {
            self.forget_pending_interrupt(token_id, &interrupt_id);
            return InterruptOutcome::Disconnected;
        }

        match tokio::time::timeout(INTERRUPT_ACK_TIMEOUT, rx).await {
            Ok(Ok(())) => {
                // The client just confirmed this session's turn is
                // cancelled (issue #82): wake any `holler say` still
                // waiting on this same session's `prompt` right now,
                // rather than leaving it to idle out to `PROMPT_TIMEOUT`
                // or a real disconnect that never actually happens.
                self.cancel_pending_prompts_for_session(token_id, &session);
                InterruptOutcome::Acked
            }
            // The `oneshot::Sender` was dropped without sending — that
            // only happens when `Registry::remove` drops this
            // connection's whole `Entry` (and every pending map in it)
            // out from under an in-flight `interrupt`. Resolves
            // immediately, not after the full timeout: a torn-down
            // connection is known right away, not a "may not have
            // landed" ambiguity.
            Ok(Err(_)) => InterruptOutcome::Disconnected,
            Err(_) => {
                self.forget_pending_interrupt(token_id, &interrupt_id);
                // The timeout elapsed with no `ack` — but is the
                // connection still there? If so, this is the narrower
                // "socket's fine, cancel may not have landed" signal
                // (issue #54); if the entry is gone too, both failure
                // modes coincide here, and `Disconnected` (rather than a
                // stale `TimedOut`) is the honest report.
                let entries = self.entries.lock().expect("registry mutex poisoned");
                if entries.contains_key(token_id) {
                    InterruptOutcome::TimedOut
                } else {
                    InterruptOutcome::Disconnected
                }
            }
        }
    }

    fn forget_pending_interrupt(&self, token_id: &str, interrupt_id: &str) {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        if let Some(entry) = entries.get(token_id) {
            let mut pending = entry
                .pending_interrupts
                .lock()
                .expect("pending-interrupt mutex poisoned");
            pending.remove(interrupt_id);
        }
    }

    /// Wake every pending [`Registry::prompt`] on `token_id`'s connection
    /// whose session matches `session` with [`PromptEvent::Cancelled`]
    /// (issue #82), called right after an `interrupt` for that session is
    /// `ack`ed. Does not remove the entries itself — `Registry::prompt`'s
    /// own loop does that via [`Registry::forget_pending_prompt`] once it
    /// observes the event, the same way it does for every other outcome.
    fn cancel_pending_prompts_for_session(&self, token_id: &str, session: &str) {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        let Some(entry) = entries.get(token_id) else {
            return;
        };
        let pending = entry
            .pending_prompts
            .lock()
            .expect("pending-prompt mutex poisoned");
        for (prompt_session, tx) in pending.values() {
            if prompt_session == session {
                let _ = tx.send(PromptEvent::Cancelled);
            }
        }
    }

    /// Called from a connection task when an `ack` arrives: resolves the
    /// matching pending outbound `interrupt`, if any. `of_id` is the
    /// `ack` body's `of` field (spec note, issue #59(b)) — an `ack` with
    /// no `of`, or one that matches no outstanding interrupt, is a no-op
    /// (fail-closed: silence, not a spurious `Acked`).
    pub fn resolve_interrupt_ack(&self, token_id: &str, of_id: &str) {
        let entries = self.entries.lock().expect("registry mutex poisoned");
        if let Some(entry) = entries.get(token_id) {
            let mut pending = entry
                .pending_interrupts
                .lock()
                .expect("pending-interrupt mutex poisoned");
            if let Some(tx) = pending.remove(of_id) {
                let _ = tx.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ping_against_unknown_token_is_disconnected() {
        let registry = Registry::new();
        let status = registry.ping("tok_nope", "id-1".to_string(), "srv").await;
        assert!(matches!(status, ConnectionStatus::Disconnected));
    }

    #[tokio::test]
    async fn ping_round_trip_reports_hostname_and_rtt() {
        let registry = Registry::new();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let registry = std::sync::Arc::new(registry);
        let registry2 = registry.clone();
        let responder = tokio::spawn(async move {
            let envelope = out_rx.recv().await.expect("ping envelope sent");
            registry2.resolve_pong("tok_1", &envelope.id);
        });

        let status = registry.ping("tok_1", "ping-1".to_string(), "srv").await;
        responder.await.unwrap();
        match status {
            ConnectionStatus::Connected { hostname, .. } => assert_eq!(hostname, "kiwi"),
            ConnectionStatus::Disconnected => panic!("expected Connected"),
        }
    }

    #[tokio::test]
    async fn ping_times_out_when_no_pong_arrives() {
        let registry = Registry::new();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        // Keep `_out_rx` alive (so the send succeeds) but never answer.
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let status = tokio::time::timeout(
            PING_TIMEOUT + Duration::from_secs(1),
            registry.ping("tok_1", "ping-1".to_string(), "srv"),
        )
        .await
        .expect("ping itself must resolve within its own timeout budget");
        assert!(matches!(status, ConnectionStatus::Disconnected));
    }

    #[tokio::test]
    async fn ping_after_disconnect_is_disconnected() {
        let registry = Registry::new();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);
        drop(out_rx); // simulate the connection task exiting
        registry.remove("tok_1");

        let status = registry.ping("tok_1", "ping-1".to_string(), "srv").await;
        assert!(matches!(status, ConnectionStatus::Disconnected));
    }

    #[tokio::test]
    async fn insert_for_an_already_live_token_id_closes_the_old_connection() {
        let registry = Registry::new();
        let (out_tx_1, mut out_rx_1) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx_1);

        // A second `auth` for the same `token_id` (reconnect, or a
        // replayed credential) supersedes the first connection.
        let (out_tx_2, _out_rx_2) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_2".to_string(), "mango".to_string(), out_tx_2);

        // The old connection's outbound channel gets one message — the
        // supersede notice — then closes.
        let notice = out_rx_1
            .recv()
            .await
            .expect("superseded connection is sent a notice");
        let crate::proto::Body::Error(ErrorBody { code, .. }) = notice.body else {
            panic!("expected an error envelope");
        };
        assert_eq!(code, crate::proto::CODE_SESSION_SUPERSEDED);

        assert!(
            out_rx_1.recv().await.is_none(),
            "old connection's channel must close so its select loop breaks"
        );

        // The new connection is the one now tracked.
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.snapshot()[0].1, "mango");
    }

    #[test]
    fn len_reflects_inserts_and_removes() {
        let registry = Registry::new();
        assert_eq!(registry.len(), 0);
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);
        assert_eq!(registry.len(), 1);
        registry.remove("tok_1");
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn resolve_target_matches_token_id_hostname_or_client_id() {
        let registry = Registry::new();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        assert_eq!(registry.resolve_target("tok_1"), Some("tok_1".to_string()));
        assert_eq!(registry.resolve_target("kiwi"), Some("tok_1".to_string()));
        assert_eq!(registry.resolve_target("cli_1"), Some("tok_1".to_string()));
        assert_eq!(registry.resolve_target("nope"), None);
    }

    #[test]
    fn confirmed_harnesses_snapshot_is_empty_until_recorded() {
        let registry = Registry::new();
        assert!(registry.confirmed_harnesses_snapshot().is_empty());

        registry.record_harness_confirmed("opencode", "kiwi");
        registry.record_harness_confirmed("opencode", "mango");
        let snapshot = registry.confirmed_harnesses_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, "opencode");
        assert_eq!(snapshot[0].clients, vec!["kiwi".to_string(), "mango".to_string()]);
    }

    #[tokio::test]
    async fn query_against_unknown_token_is_disconnected() {
        let registry = Registry::new();
        let outcome = registry
            .query("tok_nope", "status".to_string(), vec![], "id-1".to_string())
            .await;
        assert!(matches!(outcome, QueryOutcome::Disconnected));
    }

    #[tokio::test]
    async fn query_round_trip_resolves_with_the_reply_ok_body() {
        let registry = Registry::new();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let registry = std::sync::Arc::new(registry);
        let registry2 = registry.clone();
        let responder = tokio::spawn(async move {
            let envelope = out_rx.recv().await.expect("query envelope sent");
            assert_eq!(envelope.from, "server");
            let crate::proto::Body::Query(crate::proto::QueryBody { cmd, args }) = envelope.body
            else {
                panic!("expected a Query body");
            };
            assert_eq!(cmd, "support");
            assert_eq!(args, vec!["opencode".to_string()]);
            registry2.resolve_query_ok(
                "tok_1",
                &envelope.id,
                QueryOkBody {
                    cmd: "support".to_string(),
                    rest: serde_json::json!({ "ok": true }),
                },
            );
        });

        let outcome = registry
            .query(
                "tok_1",
                "support".to_string(),
                vec!["opencode".to_string()],
                "q-1".to_string(),
            )
            .await;
        responder.await.unwrap();
        match outcome {
            QueryOutcome::Ok(body) => assert_eq!(body.rest["ok"], true),
            _ => panic!("expected QueryOutcome::Ok"),
        }
    }

    #[tokio::test]
    async fn query_round_trip_resolves_with_the_reply_error_body() {
        let registry = Registry::new();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let registry = std::sync::Arc::new(registry);
        let registry2 = registry.clone();
        let responder = tokio::spawn(async move {
            let envelope = out_rx.recv().await.expect("query envelope sent");
            registry2.resolve_query_err(
                "tok_1",
                &envelope.id,
                ErrorBody {
                    code: "unknown_cmd".to_string(),
                    cmd: Some("bogus".to_string()),
                    message: None,
                },
            );
        });

        let outcome = registry
            .query("tok_1", "bogus".to_string(), vec![], "q-1".to_string())
            .await;
        responder.await.unwrap();
        match outcome {
            QueryOutcome::Err(body) => assert_eq!(body.code, "unknown_cmd"),
            _ => panic!("expected QueryOutcome::Err"),
        }
    }

    #[tokio::test]
    async fn query_times_out_when_no_reply_arrives() {
        let registry = Registry::new();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let outcome = tokio::time::timeout(
            QUERY_TIMEOUT + Duration::from_secs(1),
            registry.query("tok_1", "status".to_string(), vec![], "q-1".to_string()),
        )
        .await
        .expect("query itself must resolve within its own timeout budget");
        assert!(matches!(outcome, QueryOutcome::Disconnected));
    }

    #[tokio::test]
    async fn prompt_against_unknown_token_is_disconnected() {
        let registry = Registry::new();
        let outcome = registry
            .prompt(
                "tok_nope",
                "id-1".to_string(),
                "alpha".to_string(),
                "hi".to_string(),
                None,
            )
            .await;
        assert!(matches!(outcome, PromptOutcome::Disconnected));
    }

    #[tokio::test]
    async fn prompt_round_trip_resolves_once_done_is_true() {
        let registry = Registry::new();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let registry = std::sync::Arc::new(registry);
        let registry2 = registry.clone();
        let responder = tokio::spawn(async move {
            let envelope = out_rx.recv().await.expect("prompt envelope sent");
            let crate::proto::Body::Prompt(crate::proto::PromptBody { session, text, .. }) =
                envelope.body
            else {
                panic!("expected a Prompt body");
            };
            assert_eq!(session, "alpha");
            assert_eq!(text, "hello");
            registry2.resolve_reply(
                "tok_1",
                &envelope.id,
                ReplyBody {
                    session: "alpha".to_string(),
                    text: Some("hi ".to_string()),
                    chunks: vec![],
                    done: false,
                    exit: None,
                },
            );
            registry2.resolve_reply(
                "tok_1",
                &envelope.id,
                ReplyBody {
                    session: "alpha".to_string(),
                    text: Some("there".to_string()),
                    chunks: vec![],
                    done: true,
                    exit: Some(0),
                },
            );
        });

        let outcome = registry
            .prompt(
                "tok_1",
                "p-1".to_string(),
                "alpha".to_string(),
                "hello".to_string(),
                None,
            )
            .await;
        responder.await.unwrap();
        match outcome {
            PromptOutcome::Done { text, exit } => {
                assert_eq!(text, "hi there");
                assert_eq!(exit, Some(0));
            }
            _ => panic!("expected PromptOutcome::Done"),
        }
    }

    #[tokio::test]
    async fn prompt_round_trip_resolves_with_the_target_error() {
        let registry = Registry::new();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let registry = std::sync::Arc::new(registry);
        let registry2 = registry.clone();
        let responder = tokio::spawn(async move {
            let envelope = out_rx.recv().await.expect("prompt envelope sent");
            registry2.resolve_prompt_err(
                "tok_1",
                &envelope.id,
                ErrorBody {
                    code: "unknown_session".to_string(),
                    cmd: None,
                    message: Some("no such session".to_string()),
                },
            );
        });

        let outcome = registry
            .prompt(
                "tok_1",
                "p-1".to_string(),
                "alpha".to_string(),
                "hello".to_string(),
                None,
            )
            .await;
        responder.await.unwrap();
        match outcome {
            PromptOutcome::Err(body) => assert_eq!(body.code, "unknown_session"),
            _ => panic!("expected PromptOutcome::Err"),
        }
    }

    #[tokio::test]
    async fn interrupt_against_unknown_token_is_disconnected() {
        let registry = Registry::new();
        let outcome = registry
            .interrupt("tok_nope", "id-1".to_string(), "alpha".to_string())
            .await;
        assert!(matches!(outcome, InterruptOutcome::Disconnected));
    }

    #[tokio::test]
    async fn interrupt_round_trip_resolves_once_ack_arrives() {
        let registry = Registry::new();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let registry = std::sync::Arc::new(registry);
        let registry2 = registry.clone();
        let responder = tokio::spawn(async move {
            let envelope = out_rx.recv().await.expect("interrupt envelope sent");
            let crate::proto::Body::Interrupt(crate::proto::InterruptBody { session }) =
                envelope.body
            else {
                panic!("expected an Interrupt body");
            };
            assert_eq!(session, "alpha");
            registry2.resolve_interrupt_ack("tok_1", &envelope.id);
        });

        let outcome = registry
            .interrupt("tok_1", "i-1".to_string(), "alpha".to_string())
            .await;
        responder.await.unwrap();
        assert!(matches!(outcome, InterruptOutcome::Acked));
    }

    #[tokio::test]
    async fn interrupt_is_not_acked_by_an_unrelated_ack() {
        // An `ack` whose `of` matches nothing outstanding is a no-op
        // (fail-closed), not a spurious `Acked` for some other pending
        // interrupt.
        let registry = Registry::new();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let registry = std::sync::Arc::new(registry);
        let registry2 = registry.clone();
        let responder = tokio::spawn(async move {
            let _envelope = out_rx.recv().await.expect("interrupt envelope sent");
            registry2.resolve_interrupt_ack("tok_1", "not-the-right-id");
        });

        let outcome = registry
            .interrupt("tok_1", "i-1".to_string(), "alpha".to_string())
            .await;
        responder.await.unwrap();
        assert!(matches!(outcome, InterruptOutcome::TimedOut));
    }

    #[tokio::test]
    async fn interrupt_times_out_but_reports_timed_out_when_still_connected() {
        let registry = Registry::new();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        // Keep `_out_rx` alive (so the send succeeds) but never ack.
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let outcome = tokio::time::timeout(
            INTERRUPT_ACK_TIMEOUT + Duration::from_secs(1),
            registry.interrupt("tok_1", "i-1".to_string(), "alpha".to_string()),
        )
        .await
        .expect("interrupt itself must resolve within its own timeout budget");
        assert!(matches!(outcome, InterruptOutcome::TimedOut));
    }

    #[tokio::test]
    async fn interrupt_reports_disconnected_immediately_when_the_connection_closes_mid_flight() {
        // The connection tears down (registry.remove) *while* the
        // interrupt is outstanding — this must resolve right away as
        // `Disconnected`, not wait out the full `INTERRUPT_ACK_TIMEOUT`
        // only to report a misleading `TimedOut` (issue #54).
        let registry = Registry::new();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let registry = std::sync::Arc::new(registry);
        let registry2 = registry.clone();
        let closer = tokio::spawn(async move {
            let _envelope = out_rx.recv().await.expect("interrupt envelope sent");
            registry2.remove("tok_1");
        });

        let outcome = tokio::time::timeout(
            INTERRUPT_ACK_TIMEOUT,
            registry.interrupt("tok_1", "i-1".to_string(), "alpha".to_string()),
        )
        .await
        .expect(
            "a mid-flight disconnect must resolve well before the ack timeout, \
             not time out itself",
        );
        closer.await.unwrap();
        assert!(matches!(outcome, InterruptOutcome::Disconnected));
    }

    #[tokio::test]
    async fn interrupt_after_disconnect_is_disconnected() {
        let registry = Registry::new();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);
        drop(out_rx);
        registry.remove("tok_1");

        let outcome = registry
            .interrupt("tok_1", "i-1".to_string(), "alpha".to_string())
            .await;
        assert!(matches!(outcome, InterruptOutcome::Disconnected));
    }

    #[tokio::test]
    async fn prompt_after_disconnect_is_disconnected() {
        let registry = Registry::new();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);
        drop(out_rx);
        registry.remove("tok_1");

        let outcome = registry
            .prompt(
                "tok_1",
                "p-1".to_string(),
                "alpha".to_string(),
                "hi".to_string(),
                None,
            )
            .await;
        assert!(matches!(outcome, PromptOutcome::Disconnected));
    }
}
