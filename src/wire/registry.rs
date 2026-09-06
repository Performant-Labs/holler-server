//! Live-connection registry (issue #31): tracks which bound `token_id`s
//! currently have an open WebSocket, so `holler token ping` can report a
//! real hostname + RTT instead of always [`ConnectionStatus::Disconnected`].
//!
//! One [`Registry`] is shared (via `Arc`) by every connection task in a
//! `serve()` process. A connection registers itself on a successful
//! `auth` and deregisters when the socket closes. [`Registry::ping`]
//! drives a real `ping`/`pong` round trip over that connection's outbound
//! channel and resolves once the matching `pong` arrives (or times out).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use crate::proto::Envelope;
use crate::token::ConnectionStatus;

/// How long [`Registry::ping`] waits for the matching `pong` before
/// reporting the connection disconnected. Generous for a loopback round
/// trip, short enough that `holler token ping` does not hang.
pub const PING_TIMEOUT: Duration = Duration::from_secs(3);

/// One live connection's outward-facing handle: enough for the registry
/// to push frames at it (`ping`) and to answer `holler status`'s client
/// count / hostnames.
struct Entry {
    hostname: String,
    client_id: String,
    out_tx: mpsc::UnboundedSender<Envelope>,
    /// Outstanding `ping`s this connection has not yet answered,
    /// keyed by the envelope `id` the pong must echo back.
    pending: Mutex<HashMap<String, (Instant, oneshot::Sender<Duration>)>>,
}

/// Shared, process-wide table of live connections, keyed by `token_id`.
#[derive(Default)]
pub struct Registry {
    entries: Mutex<HashMap<String, Entry>>,
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
}
