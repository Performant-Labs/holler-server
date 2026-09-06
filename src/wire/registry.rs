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

use tokio::sync::{mpsc, oneshot};

use crate::proto::{Envelope, ErrorBody, QueryOkBody};
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
    /// entry for the same `token_id` (a reconnect from the same client
    /// supersedes its old, presumably now-dead, socket).
    pub fn insert(
        &self,
        token_id: &str,
        client_id: String,
        hostname: String,
        out_tx: mpsc::UnboundedSender<Envelope>,
    ) {
        let mut entries = self.entries.lock().expect("registry mutex poisoned");
        entries.insert(
            token_id.to_string(),
            Entry {
                hostname,
                client_id,
                out_tx,
                pending: Mutex::new(HashMap::new()),
                pending_queries: Mutex::new(HashMap::new()),
            },
        );
    }

    /// Update the recorded hostname for a connection once its `hello`
    /// arrives (auth alone only knows the redeem-time `machine` name).
    pub fn set_hostname(&self, token_id: &str, hostname: String) {
        let mut entries = self.entries.lock().expect("registry mutex poisoned");
        if let Some(entry) = entries.get_mut(token_id) {
            entry.hostname = hostname;
        }
    }

    /// Remove a connection (socket closed / errored).
    pub fn remove(&self, token_id: &str) {
        let mut entries = self.entries.lock().expect("registry mutex poisoned");
        entries.remove(token_id);
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
}
