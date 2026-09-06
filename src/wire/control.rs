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
use serde_json::Value;

use crate::proto::{ErrorBody, QueryOkBody, CODE_UNKNOWN_SESSION};
use crate::token::{ConnectionProbe, ConnectionStatus};
use super::roster::Roster;
use super::talklog::TalkLog;

use super::query;
use super::registry::{InterruptOutcome, PromptOutcome, QueryOutcome};

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
    Ping {
        token_id: String,
    },
    Roster,
    /// `holler status/support/caps/query [<id>] ...` (issue #37):
    /// `target: None` asks this live process itself; `Some(id)` asks
    /// the server to relay a `query` to the connection `id` names
    /// (token id, client id, or hostname — see
    /// `Registry::resolve_target`). The inner protocol `cmd`/`args` are
    /// named `query_cmd`/`args` (not `cmd`) so they do not collide with
    /// this enum's own `#[serde(tag = "cmd")]` discriminator key.
    Query {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        query_cmd: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// `holler say <session> <text>` (issue #33): prompt a session by
    /// name, resolved via the roster (ADR 0007) to whichever live
    /// connection currently hosts it.
    Say { session: String, text: String },
    /// `holler interrupt <session>` (issue #34, ADR 0005): cancel a
    /// session's in-flight turn — a **control** frame, resolved via the
    /// roster the same way `Say` is, but never queued behind a `prompt`.
    Interrupt { session: String },
    /// `holler token delete` / `client detach` (issue #78): after the
    /// on-disk revoke (`TokenStore::delete`) succeeds, ask a live `holler
    /// serve` process to force-close `token_id`'s live connection too, so
    /// it stops looking connected on `holler status`/`roster`. Best-effort
    /// from the CLI's point of view: the on-disk revoke is the durable,
    /// required step and already happened by the time this is sent; no
    /// live server reachable (`run_client_query` returns `None`) is not an
    /// error, same as `Say`/`Interrupt`.
    Revoke { token_id: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct PingReply {
    connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rtt_ms: Option<u64>,
}

/// Answer to a [`Request::Query`], from the CLI's point of view:
/// either the target's `query_ok` body, its `error` body (e.g. the
/// remote client's own `unknown_cmd`), or `NotConnected` when `target`
/// names no live connection at all, or a `Some(id)` target's round trip
/// otherwise fails ([`QueryOutcome::Disconnected`]).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum QueryReply {
    Ok { query_ok: QueryOkBody },
    Err { error: ErrorBody },
    NotConnected,
}

/// Answer to a [`Request::Say`]: the concatenated `reply` text once
/// `done: true` arrives, an `error` — the target's own (e.g. a stale
/// `presence` row) or this server's `unknown_session` when the roster
/// names no live connection for the session at all — or `Interrupted`
/// (issue #82): a **different** CLI invocation's `holler interrupt` for
/// this exact session landed (and was acked) while this `say` was still
/// waiting on its reply. Kept apart from `Err`'s `unknown_session` (used
/// for a real dropped connection) on purpose — the server and the
/// connection are both still alive; only this one turn was cut short on
/// purpose, and `main.rs`'s `run_say` reports that as its own accurate
/// message rather than folding it into either "connection is gone" or
/// "no live server."
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SayReply {
    Ok {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit: Option<i64>,
    },
    Interrupted,
    Err {
        error: ErrorBody,
    },
}

/// Answer to a [`Request::Interrupt`] (issue #34, ADR 0005; the
/// three-outcome split is issue #54's): the client `ack`ed the cancel
/// (`Ok`), the connection is alive but no `ack` arrived within
/// [`super::registry::INTERRUPT_ACK_TIMEOUT`] (`TimedOut` — "may not have
/// landed," not "not connected"), the connection is gone (`Disconnected`),
/// or this server's own `unknown_session` when the roster names no live
/// connection for the session at all (`Err`).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum InterruptReply {
    Ok,
    TimedOut,
    Disconnected,
    Err { error: ErrorBody },
}

/// Answer to a [`Request::Revoke`] (issue #78): whether this live server
/// actually had a connection for `token_id` to force-close. `closed:
/// false` just means there was nothing live for this process to close —
/// not an error, and not a sign the on-disk revoke (already performed by
/// the time this request is sent) failed.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevokeReply {
    pub closed: bool,
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

/// One `holler roster` row, as answered over the control channel. The
/// roster only exists in the live server's memory (unlike `token`,
/// which is file-backed) — [`query_roster`] on an unreachable server
/// returns an empty list, not an error, the same way an unbound token
/// store legitimately has zero rows.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RosterRowDoc {
    pub name: String,
    pub harness: String,
    pub client_id: String,
    pub state: String,
    pub last_seen_ms: u128,
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
    roster: Arc<Roster>,
    talklog: Arc<TalkLog>,
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
                let roster = roster.clone();
                let talklog = talklog.clone();
                let server_hostname = server_hostname.clone();
                let listening = listening.clone();
                tokio::spawn(async move {
                    let _ = handle_control_conn(stream, registry, roster, talklog, server_hostname, listening).await;
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
    _roster: Arc<Roster>,
    _talklog: Arc<TalkLog>,
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
    roster: Arc<Roster>,
    talklog: Arc<TalkLog>,
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
            Request::Roster => {
                let rows: Vec<RosterRowDoc> = roster
                    .snapshot()
                    .into_iter()
                    .map(|r| RosterRowDoc {
                        name: r.name,
                        harness: r.harness,
                        client_id: r.client_id,
                        state: r.state.to_string(),
                        last_seen_ms: r.last_seen_ms_ago,
                    })
                    .collect();
                serde_json::to_string(&rows).expect("roster rows always serialize")
            }
            Request::Query {
                target,
                query_cmd,
                args,
            } => {
                let reply = match target {
                    None => {
                        let confirmed = registry.confirmed_harnesses_snapshot();
                        match query::dispatch(
                            &query_cmd,
                            &args,
                            &server_hostname,
                            &listening,
                            registry.len(),
                            &confirmed,
                        ) {
                            Ok(body) => QueryReply::Ok { query_ok: body },
                            Err(body) => QueryReply::Err { error: body },
                        }
                    }
                    Some(id) => match registry.resolve_target(&id) {
                        None => QueryReply::NotConnected,
                        Some(token_id) => {
                            let target_hostname = registry.hostname_of(&token_id);
                            let query_id = super::hello::new_id();
                            match registry
                                .query(&token_id, query_cmd.clone(), args.clone(), query_id)
                                .await
                            {
                                QueryOutcome::Ok(body) => {
                                    // ADR 0001: record a harness
                                    // confirmation the moment a live
                                    // `support` round trip answers
                                    // `ok: true` — never from `hello`.
                                    if query_cmd == "support" {
                                        if let (Some(feature), Some(host)) =
                                            (args.first(), target_hostname.as_deref())
                                        {
                                            if body.rest.get("ok").and_then(Value::as_bool)
                                                == Some(true)
                                            {
                                                registry
                                                    .record_harness_confirmed(feature, host);
                                            }
                                        }
                                    }
                                    QueryReply::Ok { query_ok: body }
                                }
                                QueryOutcome::Err(body) => QueryReply::Err { error: body },
                                QueryOutcome::Disconnected => QueryReply::NotConnected,
                            }
                        }
                    },
                };
                serde_json::to_string(&reply).expect("QueryReply always serializes")
            }
            Request::Say { session, text } => {
                let reply = match roster.resolve_session(&session) {
                    None => SayReply::Err {
                        error: ErrorBody {
                            code: CODE_UNKNOWN_SESSION.to_string(),
                            cmd: None,
                            message: Some(format!("unknown session: {session}")),
                        },
                    },
                    Some(token_id) => {
                        let prompt_id = super::hello::new_id();
                        talklog.record_prompt(&prompt_id, &session, &text);
                        match registry
                            .prompt(&token_id, prompt_id, session.clone(), text.clone(), None)
                            .await
                        {
                            PromptOutcome::Done { text, exit } => SayReply::Ok { text, exit },
                            PromptOutcome::Err(body) => SayReply::Err { error: body },
                            PromptOutcome::Cancelled => SayReply::Interrupted,
                            PromptOutcome::Disconnected => SayReply::Err {
                                error: ErrorBody {
                                    code: CODE_UNKNOWN_SESSION.to_string(),
                                    cmd: None,
                                    message: Some(format!(
                                        "session {session:?}'s connection is gone"
                                    )),
                                },
                            },
                        }
                    }
                };
                serde_json::to_string(&reply).expect("SayReply always serializes")
            }
            Request::Interrupt { session } => {
                let reply = match roster.resolve_session(&session) {
                    None => InterruptReply::Err {
                        error: ErrorBody {
                            code: CODE_UNKNOWN_SESSION.to_string(),
                            cmd: None,
                            message: Some(format!("unknown session: {session}")),
                        },
                    },
                    Some(token_id) => {
                        let interrupt_id = super::hello::new_id();
                        match registry
                            .interrupt(&token_id, interrupt_id, session.clone())
                            .await
                        {
                            InterruptOutcome::Acked => InterruptReply::Ok,
                            InterruptOutcome::TimedOut => InterruptReply::TimedOut,
                            InterruptOutcome::Disconnected => InterruptReply::Disconnected,
                        }
                    }
                };
                serde_json::to_string(&reply).expect("InterruptReply always serializes")
            }
            Request::Revoke { token_id } => {
                let closed = registry.remove(&token_id);
                let reply = RevokeReply { closed };
                serde_json::to_string(&reply).expect("RevokeReply always serializes")
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

/// Ask a live server for its roster. Unlike [`query_status`], no live
/// server reachable is not distinguished from "a live server with an
/// empty roster" — both legitimately answer "nothing to holler at",
/// and `holler roster` has no other document shape to fall back to
/// (there is no file-backed roster the way `token` has one).
pub fn query_roster(state_dir: &Path) -> Vec<RosterRowDoc> {
    let Some(line) = run_client_query(state_dir, &Request::Roster) else {
        return Vec::new();
    };
    serde_json::from_str(&line).unwrap_or_default()
}

/// `holler status/support/caps/query [<id>] <cmd> [args...]` (issue
/// #37): ask a live server (if any) to answer `query_cmd`/`args`,
/// either about itself (`target: None`) or by relaying to the
/// connection `target` names. `None` means no live server is reachable
/// at all — callers (see `main.rs`) fall back to a local-only answer
/// for an untargeted query, or report an error for a targeted one
/// (there is nothing local to say about a specific remote client).
pub fn run_query(
    state_dir: &Path,
    target: Option<String>,
    cmd: &str,
    args: Vec<String>,
) -> Option<QueryReply> {
    let req = Request::Query {
        target,
        query_cmd: cmd.to_string(),
        args,
    };
    let line = run_client_query(state_dir, &req)?;
    serde_json::from_str(&line).ok()
}

/// `holler say <session> <text>` (issue #33): ask a live server (if any)
/// to route a `prompt` to whichever connection currently hosts `session`
/// and wait for its `reply` to finish. `None` means no live server is
/// reachable at all — `holler say` has no local fallback (there is
/// nothing to route to without a live roster).
pub fn run_say(state_dir: &Path, session: String, text: String) -> Option<SayReply> {
    let req = Request::Say { session, text };
    let line = run_client_query(state_dir, &req)?;
    serde_json::from_str(&line).ok()
}

/// `holler interrupt <session>` (issue #34, ADR 0005): ask a live server
/// (if any) to send a control-frame `interrupt` to whichever connection
/// currently hosts `session` and wait for its outcome. `None` means no
/// live server is reachable at all — `holler interrupt` has no local
/// fallback, the same as `holler say`.
pub fn run_interrupt(state_dir: &Path, session: String) -> Option<InterruptReply> {
    let req = Request::Interrupt { session };
    let line = run_client_query(state_dir, &req)?;
    serde_json::from_str(&line).ok()
}

/// `holler token delete` / `client detach` (issue #78): ask a live server
/// (if any) to force-close `token_id`'s live connection, right after the
/// caller has already performed the durable on-disk revoke. `None` means
/// no live server is reachable at all — there is nothing live to close,
/// and the on-disk revoke is sufficient on its own; callers must not
/// treat `None` here as a failure of the revoke itself.
pub fn run_revoke(state_dir: &Path, token_id: String) -> Option<RevokeReply> {
    let req = Request::Revoke { token_id };
    let line = run_client_query(state_dir, &req)?;
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
    use crate::wire::roster::RosterConfig;
    use tokio::sync::{mpsc, watch};

    #[tokio::test]
    async fn status_round_trips_over_the_control_socket() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::new());
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);

        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let listening = Arc::new(vec!["ws://127.0.0.1:41807".to_string()]);
        let roster = Arc::new(Roster::new(RosterConfig::default()));
        let talklog = Arc::new(TalkLog::new(dir.path().to_path_buf()));
        let server = tokio::spawn(serve_control_socket(
            dir.path().to_path_buf(),
            registry,
            roster,
            talklog,
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

    /// Spawn `serve_control_socket` over a fresh registry/state dir and
    /// wait for its socket to exist, for the `Request::Query` tests
    /// below. Returns the shutdown sender too — dropping it would flip
    /// `shutdown_rx.changed()` to `Err`, which the accept loop treats as
    /// "shut down", so the caller must hold it for the test's duration.
    async fn spawn_control_server(
        dir: &std::path::Path,
        registry: Arc<Registry>,
    ) -> (tokio::task::JoinHandle<()>, watch::Sender<bool>) {
        let (handle, shutdown_tx, _talklog) =
            spawn_control_server_with_roster(dir, registry, Arc::new(Roster::new(RosterConfig::default()))).await;
        (handle, shutdown_tx)
    }

    /// Like [`spawn_control_server`], but the caller supplies the roster
    /// (so a `Request::Say` test can `advertise` a session onto it before
    /// or after spawning) and gets back the [`TalkLog`] the server writes
    /// to, so a test can read back what `Request::Say` persisted.
    async fn spawn_control_server_with_roster(
        dir: &std::path::Path,
        registry: Arc<Registry>,
        roster: Arc<Roster>,
    ) -> (tokio::task::JoinHandle<()>, watch::Sender<bool>, Arc<TalkLog>) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let listening = Arc::new(vec!["ws://127.0.0.1:41807".to_string()]);
        let talklog = Arc::new(TalkLog::new(dir.to_path_buf()));
        let server = tokio::spawn(serve_control_socket(
            dir.to_path_buf(),
            registry,
            roster,
            talklog.clone(),
            Arc::from("uranus"),
            listening,
            shutdown_rx,
        ));
        for _ in 0..50 {
            if control_socket_path(dir).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (server, shutdown_tx, talklog)
    }

    #[tokio::test]
    async fn local_query_status_round_trips_with_no_target() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::new());
        let (server, _shutdown_tx) = spawn_control_server(dir.path(), registry).await;

        let reply = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || run_query(&dir, None, "status", vec![])
        })
        .await
        .unwrap()
        .expect("a live server must answer a local `status` query");
        match reply {
            QueryReply::Ok { query_ok } => {
                assert_eq!(query_ok.cmd, "status");
                assert_eq!(query_ok.rest["role"], "server");
            }
            other => panic!("expected QueryReply::Ok, got {other:?}"),
        }

        server.abort();
    }

    #[tokio::test]
    async fn remote_query_to_an_unknown_target_is_not_connected() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::new());
        let (server, _shutdown_tx) = spawn_control_server(dir.path(), registry).await;

        let reply = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || run_query(&dir, Some("nope".to_string()), "status", vec![])
        })
        .await
        .unwrap()
        .expect("the control socket answers even for an unknown target");
        assert!(matches!(reply, QueryReply::NotConnected));

        server.abort();
    }

    #[tokio::test]
    async fn remote_support_round_trip_records_a_harness_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::new());
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);
        let (server, _shutdown_tx) = spawn_control_server(dir.path(), registry.clone()).await;

        let responder = tokio::spawn(async move {
            let envelope = out_rx.recv().await.expect("query envelope sent");
            registry.resolve_query_ok(
                "tok_1",
                &envelope.id,
                QueryOkBody {
                    cmd: "support".to_string(),
                    rest: serde_json::json!({ "ok": true, "feature": "opencode" }),
                },
            );
        });

        let reply = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || {
                run_query(
                    &dir,
                    Some("tok_1".to_string()),
                    "support",
                    vec!["opencode".to_string()],
                )
            }
        })
        .await
        .unwrap()
        .expect("a live server relays the remote reply");
        match reply {
            QueryReply::Ok { query_ok } => assert_eq!(query_ok.rest["ok"], true),
            other => panic!("expected QueryReply::Ok, got {other:?}"),
        }
        responder.await.unwrap();

        // The confirmation this round trip recorded now shows up in a
        // fresh local `status` query, without another `support` ask.
        let status = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || run_query(&dir, None, "status", vec![])
        })
        .await
        .unwrap()
        .expect("status still answers after the support round trip");
        match status {
            QueryReply::Ok { query_ok } => {
                assert_eq!(query_ok.rest["harnesses_confirmed"][0]["id"], "opencode");
                assert_eq!(query_ok.rest["harnesses_confirmed"][0]["clients"][0], "kiwi");
            }
            other => panic!("expected QueryReply::Ok, got {other:?}"),
        }

        server.abort();
    }

    #[test]
    fn run_query_with_no_server_running_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run_query(dir.path(), None, "status", vec![]).is_none());
    }

    #[tokio::test]
    async fn say_to_an_unrostered_session_is_unknown_session() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::new());
        let roster = Arc::new(Roster::new(RosterConfig::default()));
        let (server, _shutdown_tx, _talklog) =
            spawn_control_server_with_roster(dir.path(), registry, roster).await;

        let reply = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || run_say(&dir, "alpha".to_string(), "hello".to_string())
        })
        .await
        .unwrap()
        .expect("the control socket answers even for an unrostered session");
        match reply {
            SayReply::Err { error } => assert_eq!(error.code, "unknown_session"),
            other => panic!("expected SayReply::Err(unknown_session), got {other:?}"),
        }

        server.abort();
    }

    #[tokio::test]
    async fn say_round_trip_returns_the_concatenated_reply_and_persists_the_exchange() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::new());
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);
        let roster = Arc::new(Roster::new(RosterConfig::default()));
        roster
            .advertise(
                "alpha".to_string(),
                "opencode".to_string(),
                "tok_1",
                "cli_1",
            )
            .unwrap();
        let (server, _shutdown_tx, talklog) =
            spawn_control_server_with_roster(dir.path(), registry.clone(), roster).await;

        let responder_talklog = talklog.clone();
        let responder = tokio::spawn(async move {
            let envelope = out_rx.recv().await.expect("prompt envelope sent");
            let crate::proto::Body::Prompt(crate::proto::PromptBody { session, text, .. }) =
                envelope.body
            else {
                panic!("expected a Prompt body");
            };
            assert_eq!(session, "alpha");
            assert_eq!(text, "hello");
            let reply_body = crate::proto::ReplyBody {
                session: session.clone(),
                text: Some("hi there".to_string()),
                chunks: vec![],
                done: true,
                exit: Some(0),
            };
            // Mirrors what `connection::handle_frame`'s `Body::Reply` arm
            // does for a real inbound frame: persist, then resolve the
            // pending prompt. This test drives the registry directly
            // (there is no real socket here), so it does both by hand.
            responder_talklog.record_reply(&envelope.id, &session, &reply_body);
            registry.resolve_reply("tok_1", &envelope.id, reply_body);
        });

        let reply = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || run_say(&dir, "alpha".to_string(), "hello".to_string())
        })
        .await
        .unwrap()
        .expect("a live server relays the routed reply");
        match reply {
            SayReply::Ok { text, exit } => {
                assert_eq!(text, "hi there");
                assert_eq!(exit, Some(0));
            }
            other => panic!("expected SayReply::Ok, got {other:?}"),
        }
        responder.await.unwrap();

        // The talk log actually recorded the exchange — read it back
        // directly rather than trusting the write happened.
        let entries = talklog.read("alpha");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].prompt_text, "hello");
        assert_eq!(entries[0].replies.len(), 1);
        assert_eq!(entries[0].replies[0].text.as_deref(), Some("hi there"));
        assert!(entries[0].replies[0].done);

        server.abort();
    }

    #[tokio::test]
    async fn interrupt_to_an_unrostered_session_is_unknown_session() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::new());
        let roster = Arc::new(Roster::new(RosterConfig::default()));
        let (server, _shutdown_tx, _talklog) =
            spawn_control_server_with_roster(dir.path(), registry, roster).await;

        let reply = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || run_interrupt(&dir, "alpha".to_string())
        })
        .await
        .unwrap()
        .expect("the control socket answers even for an unrostered session");
        match reply {
            InterruptReply::Err { error } => assert_eq!(error.code, "unknown_session"),
            other => panic!("expected InterruptReply::Err(unknown_session), got {other:?}"),
        }

        server.abort();
    }

    #[tokio::test]
    async fn interrupt_round_trip_reports_ok_once_acked() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::new());
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);
        let roster = Arc::new(Roster::new(RosterConfig::default()));
        roster
            .advertise(
                "alpha".to_string(),
                "opencode".to_string(),
                "tok_1",
                "cli_1",
            )
            .unwrap();
        let (server, _shutdown_tx, _talklog) =
            spawn_control_server_with_roster(dir.path(), registry.clone(), roster).await;

        let responder = tokio::spawn(async move {
            let envelope = out_rx.recv().await.expect("interrupt envelope sent");
            let crate::proto::Body::Interrupt(crate::proto::InterruptBody { session }) =
                envelope.body
            else {
                panic!("expected an Interrupt body");
            };
            assert_eq!(session, "alpha");
            registry.resolve_interrupt_ack("tok_1", &envelope.id);
        });

        let reply = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || run_interrupt(&dir, "alpha".to_string())
        })
        .await
        .unwrap()
        .expect("a live server relays the interrupt and its ack");
        assert!(matches!(reply, InterruptReply::Ok));
        responder.await.unwrap();

        server.abort();
    }

    #[tokio::test]
    async fn interrupt_reports_disconnected_for_a_session_whose_connection_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::new());
        let roster = Arc::new(Roster::new(RosterConfig::default()));
        // Advertise `alpha` against a token that was never actually
        // registered as a live connection — mirrors a stale roster row
        // whose owning connection has already dropped.
        roster
            .advertise(
                "alpha".to_string(),
                "opencode".to_string(),
                "tok_gone",
                "cli_gone",
            )
            .unwrap();
        let (server, _shutdown_tx, _talklog) =
            spawn_control_server_with_roster(dir.path(), registry, roster).await;

        let reply = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || run_interrupt(&dir, "alpha".to_string())
        })
        .await
        .unwrap()
        .expect("the control socket answers even for a dead-connection session");
        assert!(matches!(reply, InterruptReply::Disconnected));

        server.abort();
    }

    #[tokio::test]
    async fn revoke_force_closes_the_live_connection_and_reports_closed_true() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::new());
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        registry.insert("tok_1", "cli_1".to_string(), "kiwi".to_string(), out_tx);
        let (server, _shutdown_tx) = spawn_control_server(dir.path(), registry.clone()).await;

        let reply = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || run_revoke(&dir, "tok_1".to_string())
        })
        .await
        .unwrap()
        .expect("a live server answers a revoke request");
        assert!(reply.closed);

        // The registry entry is actually gone, and dropping it dropped
        // `out_tx` — a real connection task's `out_rx.recv()` would see
        // that close and break its session loop (see `Registry::remove`'s
        // doc comment).
        assert_eq!(registry.len(), 0);
        assert!(out_rx.recv().await.is_none());

        server.abort();
    }

    #[tokio::test]
    async fn revoke_of_an_unknown_token_reports_closed_false() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(Registry::new());
        let (server, _shutdown_tx) = spawn_control_server(dir.path(), registry).await;

        let reply = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || run_revoke(&dir, "tok_nope".to_string())
        })
        .await
        .unwrap()
        .expect("the control socket answers even for an unknown token");
        assert!(!reply.closed);

        server.abort();
    }

    #[test]
    fn run_revoke_with_no_server_running_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run_revoke(dir.path(), "tok_1".to_string()).is_none());
    }
}
