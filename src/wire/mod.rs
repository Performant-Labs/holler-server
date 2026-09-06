//! WebSocket listener + connection lifecycle (issue #31): "first talk".
//!
//! `serve()` binds `--listen`/`HOLLER_LISTEN` addresses (default
//! `127.0.0.1:41807`, ADR 0004) and, for each accepted connection, runs
//! `connect -> auth -> hello (both ways) -> query / ping`
//! (`docs/protocol/v1.md` §4) via [`connection::handle_connection`].
//!
//! ## Scope cut: loopback `ws` only, no `wss`/TLS yet
//!
//! Full `wss`/TLS 1.3 (ADR 0010: AES-256-GCM or ChaCha20-Poly1305,
//! X25519MLKEM768 off a private network) is a separate, large yak-shave
//! — certificate management and cipher-suite pinning that this story's
//! own acceptance criteria do not require ("Loopback `ws` is enough
//! here"). [`serve`] therefore implements the loopback `ws://` path in
//! full and **fails closed** on any non-loopback `--listen` address
//! ([`NonLoopbackWithoutTls`]) rather than silently serving plaintext
//! off a public interface, which would violate ADR 0004/0010's whole
//! point. A follow-on story should add real `wss` support and lift this
//! restriction.
//!
//! ## Scope cut: the local control channel
//!
//! `holler token ping <id>` / `holler status` / `holler roster` are
//! separate, one-shot CLI processes — they do not share memory with a
//! long-running `holler serve` process, so they cannot read its
//! in-memory [`registry::Registry`] or [`roster::Roster`] directly.
//! [`control`] adds a small Unix-domain-socket side channel (not part of
//! Holler v1) for exactly this: a live server, and only a live server on
//! the same machine, answers `ping`/`status`/`roster` queries from this
//! repo's own CLI. See `control`'s module doc for the Windows cut.

pub mod connection;
pub mod control;
pub mod hello;
pub mod lockout;
pub mod query;
pub mod registry;
pub mod roster;
pub mod talklog;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;

use crate::debug::DebugLevel;
use crate::token::TokenStore;

use connection::{ConnectionContext, ConnectionLimits};
use lockout::LockoutTracker;
use registry::Registry;
use roster::{Roster, RosterConfig};
use talklog::TalkLog;

/// A `--listen`/`HOLLER_LISTEN` value did not parse as `[host:]port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenParseError(pub String);

impl std::fmt::Display for ListenParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid --listen value {:?} (expected `host:port`, `[ipv6]:port`, or a bare port)",
            self.0
        )
    }
}
impl std::error::Error for ListenParseError {}

/// Parse one `--listen`/`HOLLER_LISTEN` entry. Accepts anything
/// [`SocketAddr`]'s `FromStr` accepts (`127.0.0.1:41807`,
/// `[::1]:41807`, `[::]:41807`) plus a bare port (`41807`), which binds
/// the IPv4 loopback default host (ADR 0004: "Do not default to the
/// name `localhost`").
pub fn parse_listen_spec(spec: &str) -> Result<SocketAddr, ListenParseError> {
    if let Ok(addr) = spec.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(port) = spec.parse::<u16>() {
        return Ok(SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)));
    }
    Err(ListenParseError(spec.to_string()))
}

/// This host's hostname, as advertised in `hello`/`status` (spec §6).
pub fn local_hostname() -> io::Result<String> {
    Ok(hostname::get()?.to_string_lossy().to_string())
}

/// A `--listen` address that is not loopback, with no TLS implementation
/// to protect it (see the module-level scope-cut note).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonLoopbackWithoutTls(pub SocketAddr);

impl std::fmt::Display for NonLoopbackWithoutTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to bind non-loopback address {} as plain `ws`: TLS (`wss`) is not yet \
             implemented (ADR 0004/0010 require `wss` off loopback); use a loopback address \
             (127.0.0.1 / ::1) for now",
            self.0
        )
    }
}
impl std::error::Error for NonLoopbackWithoutTls {}

/// Configuration for [`serve`].
pub struct ServeConfig {
    pub listen_addrs: Vec<SocketAddr>,
    pub debug: DebugLevel,
}

/// A running server: the addresses it actually bound (useful when a
/// test asks for port `0` and needs the OS-assigned port back) and a
/// handle to shut it down.
pub struct ServerHandle {
    pub addrs: Vec<SocketAddr>,
    shutdown_tx: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl ServerHandle {
    /// Stop accepting new connections and wait for the accept-loop and
    /// control-socket tasks to exit. Does not forcibly close
    /// already-open connections (they end when their sockets close).
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

/// Bind every address in `config.listen_addrs`, start the local control
/// socket, and start accepting Holler v1 connections. Returns once every
/// listener is bound; connections are then serviced on spawned tasks
/// until [`ServerHandle::shutdown`] is called.
///
/// Fails closed (returns `Err`) if any listen address is not loopback
/// (see the module-level scope-cut note) or if a bind fails.
pub async fn serve(config: ServeConfig) -> io::Result<ServerHandle> {
    for addr in &config.listen_addrs {
        if !addr.ip().is_loopback() {
            return Err(io::Error::other(NonLoopbackWithoutTls(*addr)));
        }
    }

    let hostname = local_hostname()?;
    let store = Arc::new(TokenStore::open().map_err(io::Error::other)?);
    let registry = Arc::new(Registry::new().with_debug(config.debug));
    let roster = Arc::new(Roster::new(RosterConfig::from_env()));
    let lockout = Arc::new(LockoutTracker::new());
    let state_dir = store.dir().to_path_buf();
    let talklog = Arc::new(TalkLog::new(state_dir.clone()));

    // Issue #89: probe-and-bind the control socket before doing anything
    // else observable (binding a TCP port, writing the advertise file) so
    // a second `holler serve` against the same `HOLLER_STATE_DIR` fails
    // fast and cleanly if a live instance already owns it, rather than
    // silently stealing the control socket out from under it.
    let control_listener = control::bind_control_socket(&state_dir)?;

    let mut listeners = Vec::with_capacity(config.listen_addrs.len());
    for addr in &config.listen_addrs {
        listeners.push(TcpListener::bind(addr).await?);
    }
    let bound_addrs: Vec<SocketAddr> = listeners
        .iter()
        .map(|l| l.local_addr())
        .collect::<io::Result<_>>()?;
    let listening_urls: Arc<Vec<String>> =
        Arc::new(bound_addrs.iter().map(|a| format!("ws://{a}")).collect());

    let limits = ConnectionLimits::from_env();
    let unauth_slots = Arc::new(Semaphore::new(limits.max_unauth_connections));

    let ctx = Arc::new(ConnectionContext {
        store: store.clone(),
        registry: registry.clone(),
        roster: roster.clone(),
        lockout: lockout.clone(),
        talklog: talklog.clone(),
        server_hostname: Arc::from(hostname.as_str()),
        listening: listening_urls.clone(),
        debug: config.debug,
        limits,
        unauth_slots,
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks = Vec::new();
    for listener in listeners {
        let ctx = ctx.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        tasks.push(tokio::spawn(async move {
            accept_loop(listener, ctx, &mut shutdown_rx).await;
        }));
    }

    tasks.push(tokio::spawn(control::serve_control_socket(
        control_listener,
        control::control_socket_path(&state_dir),
        registry,
        roster.clone(),
        talklog,
        ctx.server_hostname.clone(),
        listening_urls,
        shutdown_rx.clone(),
    )));

    tasks.push(tokio::spawn(roster_prune_loop(roster, shutdown_rx)));

    Ok(ServerHandle {
        addrs: bound_addrs,
        shutdown_tx,
        tasks,
    })
}

/// Periodically drop `Gone` roster rows old enough to prune (memory
/// hygiene only — see `roster`'s module doc; never affects what
/// `holler roster` reports for a row still within its prune window).
async fn roster_prune_loop(roster: Arc<Roster>, mut shutdown_rx: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(roster.sweep_interval());
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                match changed {
                    Ok(()) if *shutdown_rx.borrow() => break,
                    Ok(()) => continue,
                    Err(_) => break,
                }
            }
            _ = interval.tick() => {
                roster.prune();
            }
        }
    }
}

/// `peer` (the accepted socket's remote address) is captured once here and
/// threaded through to [`connection::handle_connection`], which uses it
/// both to key the failed-auth lockout tracker (issue #58, [`lockout`]) and
/// to log against the unauthenticated-connection cap's `permit` (issue
/// #57) — a single capture point for both.
async fn accept_loop(
    listener: TcpListener,
    ctx: Arc<ConnectionContext>,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
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
                match accepted {
                    Ok((stream, peer)) => {
                        // Reject before completing the WebSocket handshake
                        // (issue #57): the cheapest point to turn away a
                        // connection once the unauthenticated-connection cap
                        // is hit. Dropping `stream` here closes the raw TCP
                        // socket with no reply. (A locked-out peer, issue
                        // #58, is checked separately inside
                        // `handle_connection` itself, after this cap check.)
                        let permit = match ctx.unauth_slots.clone().try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                connection::trace(
                                    &ctx,
                                    &format!(
                                        "[{peer}] rejecting: unauthenticated-connection cap reached"
                                    ),
                                );
                                continue;
                            }
                        };
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            connection::handle_connection(stream, peer, permit, ctx).await;
                        });
                    }
                    Err(_) => continue,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_listen_spec_accepts_documented_forms() {
        assert_eq!(
            parse_listen_spec("127.0.0.1:41807").unwrap(),
            "127.0.0.1:41807".parse().unwrap()
        );
        assert_eq!(
            parse_listen_spec("[::1]:41807").unwrap(),
            "[::1]:41807".parse().unwrap()
        );
        assert_eq!(
            parse_listen_spec("[::]:41807").unwrap(),
            "[::]:41807".parse().unwrap()
        );
        assert_eq!(
            parse_listen_spec("41807").unwrap(),
            "127.0.0.1:41807".parse().unwrap()
        );
    }

    #[test]
    fn parse_listen_spec_rejects_garbage() {
        assert!(parse_listen_spec("not-an-address").is_err());
        assert!(parse_listen_spec("").is_err());
    }
}
