//! Local admin control channel (issue #31 scope note): a Unix-domain
//! socket, private to the state directory, letting a separate one-shot
//! `holler` CLI invocation (`holler token ping`, `holler status`) ask a
//! *live* `holler serve` process about its in-memory
//! [`super::registry::Registry`] — which the CLI cannot otherwise reach,
//! since the registry is per-process state and `holler token ping` /
//! `holler status` run as separate OS processes from the server.
//!
//! This is **not** part of Holler v1 (`docs/protocol/v1.md`): it never
//! carries a credential, is unreachable off the local machine (a Unix
//! domain socket has no network path at all), and is filesystem-gated
//! the same way the token store itself is (`HOLLER_STATE_DIR`, socket
//! mode `0600`).
//!
//! **Scope cut:** Windows has no Unix domain sockets. On that platform
//! [`LiveProbe`] and [`query_status`] always report "no live server
//! reachable" (the same answer as "the server isn't running") rather
//! than a real probe — documented here, not silently absent. A
//! cross-platform channel (named pipes on Windows) is a reasonable
//! follow-on if a Windows operator needs a live `ping`/`status`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::token::{ConnectionProbe, ConnectionStatus};

const CONTROL_SOCKET_NAME: &str = "control.sock";

/// How long the CLI side waits for a control-channel round trip before
/// reporting "no live server" — generous for a loopback UDS hop, short
/// enough `holler token ping` / `holler status` never hang. Only the
/// Unix `run_client_query` uses this (see the module scope note for the
/// non-Unix fallback), so it is cfg-gated to avoid a dead-code warning
/// on other platforms.
#[cfg(unix)]
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

pub fn control_socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join(CONTROL_SOCKET_NAME)
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    Status,
    Ping { token_id: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct PingReply {
    connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rtt_ms: Option<u64>,
}

/// The live server's self-report, as answered over the control channel.
/// `None` from [`query_status`] means no live server is reachable —
/// callers fall back to a local-only report (see `main.rs`'s `status`
/// command).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct StatusDoc {
    pub hostname: String,
    pub listening: Vec<String>,
    pub clients: usize,
}

// ---------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------

/// Run the control-socket accept loop until `shutdown_rx` fires. Removes
/// any stale socket file left by a prior crash before binding, and sets
/// the socket to mode `0600` (owner-only) once bound.
#[cfg(unix)]
pub async fn serve_control_socket(
    state_dir: PathBuf,
    registry: Arc<super::registry::Registry>,
    server_hostname: Arc<str>,
    listening: Arc<Vec<String>>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    let path = control_socket_path(&state_dir);
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("holler-server: could not bind control socket at {path:?}: {e}");
            return;
        }
    };
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        eprintln!("holler-server: could not restrict control socket permissions: {e}");
    }

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                match changed {
                    Ok(()) if *shutdown_rx.borrow() => break,
                    Ok(()) => continue,
                    Err(_) => break,
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let registry = registry.clone();
                let server_hostname = server_hostname.clone();
                let listening = listening.clone();
                tokio::spawn(async move {
                    let _ = handle_control_conn(stream, registry, server_hostname, listening).await;
                });
            }
        }
    }
    let _ = std::fs::remove_file(&path);
}

#[cfg(not(unix))]
pub async fn serve_control_socket(
    _state_dir: PathBuf,
    _registry: Arc<super::registry::Registry>,
    _server_hostname: Arc<str>,
    _listening: Arc<Vec<String>>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    // No Unix domain sockets on this platform (see module scope note).
    // Idle until shutdown so this still behaves like the other tasks in
    // `ServerHandle`'s task list.
    let _ = shutdown_rx.changed().await;
}

#[cfg(unix)]
async fn handle_control_conn(
    stream: tokio::net::UnixStream,
    registry: Arc<super::registry::Registry>,
    server_hostname: Arc<str>,
    listening: Arc<Vec<String>>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Request>(&line) else {
            continue;
        };
        let response = match req {
            Request::Status => {
                let doc = StatusDoc {
                    hostname: server_hostname.to_string(),
                    listening: (*listening).clone(),
                    clients: registry.len(),
                };
                serde_json::to_string(&doc).expect("StatusDoc always serializes")
            }
            Request::Ping { token_id } => {
                let ping_id = super::hello::new_id();
                let reply = match registry.ping(&token_id, ping_id, &server_hostname).await {
                    ConnectionStatus::Connected { hostname, rtt_ms } => PingReply {
                        connected: true,
                        hostname: Some(hostname),
                        rtt_ms: Some(rtt_ms),
                    },
                    ConnectionStatus::Disconnected => PingReply {
                        connected: false,
                        hostname: None,
                        rtt_ms: None,
                    },
                };
                serde_json::to_string(&reply).expect("PingReply always serializes")
            }
        };
        write_half.write_all(response.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Client side (used from the synchronous CLI; spins its own tiny
// runtime for the one round trip rather than requiring the whole `holler`
// binary to be async).
// ---------------------------------------------------------------------

#[cfg(unix)]
fn run_client_query(state_dir: &Path, req: &Request) -> Option<String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let path = control_socket_path(state_dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let stream = tokio::time::timeout(CLIENT_TIMEOUT, UnixStream::connect(&path))
            .await
            .ok()?
            .ok()?;
        let (read_half, mut write_half) = stream.into_split();
        let line = serde_json::to_string(req).ok()?;
        write_half.write_all(line.as_bytes()).await.ok()?;
        write_half.write_all(b"\n").await.ok()?;

        let mut reader = BufReader::new(read_half);
        let mut response = String::new();
        let read = tokio::time::timeout(CLIENT_TIMEOUT, reader.read_line(&mut response))
            .await
            .ok()?
            .ok()?;
        if read == 0 || response.trim().is_empty() {
            None
        } else {
            Some(response)
        }
    })
}

#[cfg(not(unix))]
fn run_client_query(_state_dir: &Path, _req: &Request) -> Option<String> {
    None
}

/// Ask a live server (if any) for its status doc. `None` means
/// unreachable — no server running on this host, or (see the module
/// scope note) this platform has no control-channel support.
pub fn query_status(state_dir: &Path) -> Option<StatusDoc> {
    let line = run_client_query(state_dir, &Request::Status)?;
    serde_json::from_str(&line).ok()
}

/// [`ConnectionProbe`] backed by the control channel: the "real"
/// liveness check that replaces [`crate::token::AlwaysDisconnected`] as
/// `holler token ping`'s default (issue #31).
pub struct LiveProbe {
    state_dir: PathBuf,
}

impl LiveProbe {
    pub fn new(state_dir: PathBuf) -> Self {
        LiveProbe { state_dir }
    }
}

impl ConnectionProbe for LiveProbe {
    fn probe(&self, token_id: &str) -> ConnectionStatus {
        let Some(line) = run_client_query(
            &self.state_dir,
            &Request::Ping {
                token_id: token_id.to_string(),
            },
        ) else {
            return ConnectionStatus::Disconnected;
        };
        match serde_json::from_str::<PingReply>(&line) {
            Ok(reply) if reply.connected => ConnectionStatus::Connected {
                hostname: reply.hostname.unwrap_or_default(),
                rtt_ms: reply.rtt_ms.unwrap_or_default(),
            },
            _ => ConnectionStatus::Disconnected,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::wire::registry::Registry;
    use tokio::sync::{mpsc, watch};

    #[tokio::test]
    async fn status_round_trips_over_the_control_socket() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::new());
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let listening = Arc::new(vec!["ws://127.0.0.1:41807".to_string()]);
        let server = tokio::spawn(serve_control_socket(
            dir.path().to_path_buf(),
            registry,
            Arc::from("uranus"),
            listening,
            shutdown_rx,
        ));

        // Give the listener a moment to bind before the client dials.
        for _ in 0..50 {
            if control_socket_path(dir.path()).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let doc = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || query_status(&dir)
        })
        .await
        .unwrap()
        .expect("a live server must answer `status`");
        assert_eq!(doc.hostname, "uranus");
        assert_eq!(doc.clients, 1);
        assert_eq!(doc.listening, vec!["ws://127.0.0.1:41807".to_string()]);

        server.abort();
    }

    #[test]
    fn query_status_with_no_server_running_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(query_status(dir.path()), None);
    }

    #[test]
    fn live_probe_with_no_server_running_is_disconnected() {
        let dir = tempfile::tempdir().unwrap();
        let probe = LiveProbe::new(dir.path().to_path_buf());
        assert!(matches!(
            probe.probe("tok_x"),
            ConnectionStatus::Disconnected
        ));
    }
}
