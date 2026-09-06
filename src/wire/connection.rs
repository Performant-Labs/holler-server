//! Per-connection handling (issue #31): one task per accepted WebSocket,
//! implementing `connect -> auth -> hello (both ways) -> query / ping`
//! (`docs/protocol/v1.md` §4) and the fail-closed error paths of ADR 0009.
//! `reply` is routed by [`super::registry::Registry`] (issue #33: `holler
//! say <session>`, resolved by [`super::roster::Roster`]); `prompt` /
//! `interrupt` / `ack` inbound (this server never sends the former two,
//! and defines no use for the latter) remain accepted-but-ignored.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use serde::Deserialize;

use crate::debug::{redact, DebugLevel};
use crate::proto::{
    self, AuthBody, Body, DecodeError, Envelope, JoinBody, JoinOkBody, MessageType, PresenceBody,
    QueryBody, CODE_JOIN_FAILED, CODE_UNAUTHENTICATED, CODE_UNKNOWN_TYPE,
};
use crate::token::TokenStore;

use super::hello::{new_pong_envelope, server_hello};
use super::lockout::LockoutTracker;
use super::query;
use super::registry::Registry;
use super::roster::Roster;
use super::talklog::TalkLog;

/// Connection/message hygiene limits (issue #57, `docs/research-security-hijack-dos.md`
/// §4 "do now" cluster): confirmed-absent gaps in the accept/auth path that #31
/// ("first talk") deliberately cut as follow-on hardening, not an oversight.
///
/// Mirrors `RosterConfig`'s "env override, sane compiled-in default" convention
/// (`super::roster::RosterConfig`) so nothing needs to be configured to get the safe
/// behavior, and integration tests can exercise these paths without waiting out real
/// multi-second/hundred-connection windows.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionLimits {
    /// Ceiling passed as both `max_message_size` and `max_frame_size` to
    /// `tokio_tungstenite::accept_async_with_config`, replacing the library's 64
    /// MiB/16 MiB defaults. v1's entire vocabulary is small JSON control envelopes —
    /// the largest legitimate frame is a `hello` with a long session list or a
    /// `reply` with many `chunks` — sized generously above either. Default 2 MiB.
    pub max_frame_bytes: usize,
    /// How long a connection may sit open before its first frame (`auth`/`join`,
    /// spec §4/§4.1) arrives before it is closed with no reply — mirrors SSH's
    /// `LoginGraceTime` (default 120s), tightened for a loopback/LAN client.
    /// Default 20s.
    pub pre_auth_timeout: Duration,
    /// Ceiling on connections accepted but not yet past `auth`/`join` (mirrors SSH's
    /// `MaxStartups`). Never a limit on already-authenticated live sessions: each
    /// connection's slot is released the moment auth succeeds (or, for `join`, when
    /// the one-shot bootstrap finishes) — see [`handle_connection`]. Default 75.
    pub max_unauth_connections: usize,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        ConnectionLimits {
            max_frame_bytes: 2 * 1024 * 1024,
            pre_auth_timeout: Duration::from_secs(20),
            max_unauth_connections: 75,
        }
    }
}

impl ConnectionLimits {
    /// Reads `HOLLER_MAX_FRAME_BYTES` / `HOLLER_PRE_AUTH_TIMEOUT_MS` /
    /// `HOLLER_MAX_UNAUTH_CONNECTIONS`, falling back to
    /// [`ConnectionLimits::default`] for any that are unset or unparseable.
    pub fn from_env() -> Self {
        let default = ConnectionLimits::default();
        ConnectionLimits {
            max_frame_bytes: env_parsed("HOLLER_MAX_FRAME_BYTES")
                .unwrap_or(default.max_frame_bytes),
            pre_auth_timeout: env_parsed::<u64>("HOLLER_PRE_AUTH_TIMEOUT_MS")
                .map(Duration::from_millis)
                .unwrap_or(default.pre_auth_timeout),
            max_unauth_connections: env_parsed("HOLLER_MAX_UNAUTH_CONNECTIONS")
                .unwrap_or(default.max_unauth_connections),
        }
    }
}

fn env_parsed<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Shared, read-only context every connection task needs. Grouped so
/// `handle_connection`'s signature does not grow a parameter per field.
pub struct ConnectionContext {
    pub store: Arc<TokenStore>,
    pub registry: Arc<Registry>,
    pub roster: Arc<Roster>,
    pub lockout: Arc<LockoutTracker>,
    pub talklog: Arc<TalkLog>,
    pub server_hostname: Arc<str>,
    pub listening: Arc<Vec<String>>,
    pub debug: DebugLevel,
    pub limits: ConnectionLimits,
    /// Slots for connections still working through their first frame
    /// (issue #57's `max_unauth_connections`) — `accept_loop` acquires one
    /// per accepted socket before spawning `handle_connection`, which
    /// releases it once auth/`join` finishes (see `handle_connection`'s doc
    /// comment).
    pub unauth_slots: Arc<Semaphore>,
}

/// One `presence.sessions[]` entry (`docs/protocol/v1.md` §10):
/// mirrors `HelloSession`'s `{name, harness}` shape, which the spec
/// only informally implies for presence (the codec leaves each row an
/// opaque `Value` — see `proto::PresenceBody`'s doc comment) since it
/// never pins one down formally. A row that doesn't fit this shape is
/// skipped, not fatal to the frame (issue #32).
#[derive(Deserialize)]
struct PresenceSession {
    name: String,
    harness: String,
}

/// Drive one accepted TCP connection through the WebSocket handshake and
/// the Holler v1 session. Never panics on a misbehaving peer — every
/// failure path logs (at `noisy`, redacted) and returns, dropping the
/// connection.
///
/// `peer` is the raw TCP peer address: used to key the failed-auth lockout
/// tracker (issue #58, [`super::lockout`]) and to release `permit`'s slot
/// in `ctx`'s unauthenticated-connection cap (issue #57) once this
/// connection is done working through its first frame — on successful
/// `auth` (just before the connection is registered and enters its
/// session loop) or when `handle_join` returns (`join` is a one-shot
/// bootstrap, never a session). A source currently locked out is refused
/// before the WebSocket handshake even starts: the socket is simply
/// dropped, with no reply of any kind, which is both the cheapest
/// enforcement point and the safest fail-closed shape — it never gives a
/// probing client a distinguishable "you're rate-limited" signal to react
/// to. Every other early-return path (handshake failure, pre-auth
/// timeout, decode failure, bad credential) also releases `permit`, via
/// ordinary drop-on-return.
pub async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    permit: OwnedSemaphorePermit,
    ctx: Arc<ConnectionContext>,
) {
    let peer_ip = peer.ip();
    if ctx.lockout.is_locked_out(peer_ip) {
        trace(
            &ctx,
            &format!("[{peer}] refusing connection from locked-out peer"),
        );
        return;
    }

    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(ctx.limits.max_frame_bytes))
        .max_frame_size(Some(ctx.limits.max_frame_bytes));
    let ws_stream = match tokio_tungstenite::accept_async_with_config(stream, Some(ws_config)).await
    {
        Ok(s) => s,
        Err(e) => {
            trace(&ctx, &format!("[{peer}] websocket handshake failed: {e}"));
            return;
        }
    };
    let (mut write, mut read) = ws_stream.split();

    // 1. First frame must arrive within the pre-auth window (issue #57) and
    // be `auth` or `join` (spec §4, §4.1; ADR 0015). A connection that never
    // sends one is closed with no reply — nothing has authenticated, so
    // there is no one to usefully send an error to, and a distinguishable
    // response would only help a slow-loris-style prober. Anything else —
    // wrong type, undecodable, or a version/type the codec already rejects
    // — fails closed: reply `error` and close, per ADR 0009. `join` is a
    // one-shot bootstrap: it never falls through to the `auth` path below,
    // on success or failure alike (see `handle_join`), so a connection
    // sends `join` or `auth`, never both.
    let raw = match tokio::time::timeout(ctx.limits.pre_auth_timeout, next_text_frame(&mut read))
        .await
    {
        Ok(Some(t)) => t,
        Ok(None) => return,
        Err(_) => {
            trace(
                &ctx,
                &format!(
                    "[{peer}] closing: no first frame within {:?}",
                    ctx.limits.pre_auth_timeout
                ),
            );
            return;
        }
    };
    let envelope = match proto::decode(&raw) {
        Ok(e) => e,
        Err(err) => {
            let _ = write
                .send(Message::Text(encode(&decode_error_envelope(&err)).into()))
                .await;
            trace(&ctx, &format!("[{peer}] first frame did not decode: {err}"));
            return;
        }
    };
    let credential = match &envelope.body {
        Body::Join(JoinBody { secret, hostname }) => {
            handle_join(&ctx, peer, &envelope, secret, hostname, &mut write).await;
            return;
        }
        Body::Auth(AuthBody { credential }) => credential,
        _ => {
            let _ = write
                .send(Message::Text(
                    encode(&proto::error_with_message(
                        CODE_UNAUTHENTICATED,
                        "first frame must be `auth` or `join`",
                        &envelope.id,
                        "server",
                    ))
                    .into(),
                ))
                .await;
            return;
        }
    };
    let Some(credential) = credential.as_str() else {
        let _ = write
            .send(Message::Text(
                encode(&proto::error_with_message(
                    CODE_UNAUTHENTICATED,
                    "auth credential must be a string",
                    &envelope.id,
                    "server",
                ))
                .into(),
            ))
            .await;
        return;
    };

    let token_id = envelope.from.clone();
    let verified = match ctx.store.verify_credential(&token_id, credential) {
        Ok(v) => v,
        Err(e) => {
            // Any failed `auth` counts toward the lockout (issue #58) — not
            // just a credential mismatch. See `lockout`'s module doc for why
            // that's the deliberate choice, not an oversight.
            ctx.lockout.record_failure(peer_ip);
            trace(
                &ctx,
                &format!(
                    "[{peer}] auth rejected for {}: {e}",
                    redact("token_id", &token_id)
                ),
            );
            let _ = write
                .send(Message::Text(
                    encode(&proto::error_with_message(
                        CODE_UNAUTHENTICATED,
                        "authentication failed",
                        &envelope.id,
                        "server",
                    ))
                    .into(),
                ))
                .await;
            return;
        }
    };
    // Auth succeeded: the accept-before-auth window is over for this
    // connection, so its slot in the unauthenticated-connection cap is
    // released now, before it becomes a live session (issue #57).
    drop(permit);

    // 2. Authenticated: register the connection and exchange `hello`.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Envelope>();
    ctx.registry.insert(
        &verified.token_id,
        verified.client_id.clone(),
        verified.machine.clone(),
        out_tx,
    );
    trace(
        &ctx,
        &format!(
            "[{peer}] client {} authenticated",
            redact("token_id", &verified.token_id)
        ),
    );

    if write
        .send(Message::Text(
            encode(&server_hello(&ctx.server_hostname)).into(),
        ))
        .await
        .is_err()
    {
        ctx.registry.remove(&verified.token_id);
        return;
    }

    // 3. Session loop: service inbound client frames and outbound frames
    // the registry wants pushed at this connection (server-initiated
    // `ping`, for `holler token ping`).
    loop {
        tokio::select! {
            incoming = read.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if !handle_frame(
                            &text,
                            &verified.token_id,
                            &verified.client_id,
                            &ctx,
                            &mut write,
                        )
                        .await
                        {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = write.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Binary(_)) | Ok(Message::Frame(_))) => {
                        // Spec §13: "Binary frames... not in v1." Fail
                        // closed rather than silently ignore.
                        let _ = write
                            .send(Message::Text(
                                encode(&proto::error_with_message(
                                    CODE_UNKNOWN_TYPE,
                                    "binary frames are not part of Holler v1",
                                    "",
                                    "server",
                                ))
                                .into(),
                            ))
                            .await;
                        break;
                    }
                    Some(Err(e)) => {
                        trace(&ctx, &format!("websocket read error: {e}"));
                        break;
                    }
                }
            }
            outgoing = out_rx.recv() => {
                match outgoing {
                    Some(env) => {
                        if write.send(Message::Text(encode(&env).into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    ctx.registry.remove(&verified.token_id);
    trace(
        &ctx,
        &format!(
            "[{peer}] client {} disconnected",
            redact("token_id", &verified.token_id)
        ),
    );
}

/// Handle a `join` first frame (spec §4.1, ADR 0015): redeem the
/// one-time secret via [`TokenStore::redeem`], reply `join_ok` or an
/// `error`/`join_failed` (reason in `message`), then return — the
/// caller (`handle_connection`) closes the connection right after this
/// returns, on either outcome. `join` never proceeds into `hello`/the
/// session loop on the same socket: it is a one-shot bootstrap, not a
/// session (the client reconnects via `auth` to actually start one).
async fn handle_join(
    ctx: &Arc<ConnectionContext>,
    peer: SocketAddr,
    envelope: &Envelope,
    secret: &str,
    hostname: &str,
    write: &mut (impl Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
) {
    let peer_ip = peer.ip();
    let token_id = envelope.from.clone();
    match ctx.store.redeem(&token_id, secret, hostname.to_string()) {
        Ok(result) => {
            trace(
                ctx,
                &format!(
                    "[{peer}] join succeeded for {}",
                    redact("token_id", &token_id)
                ),
            );
            let reply = Envelope {
                v: 1,
                msg_type: MessageType::JoinOk,
                id: envelope.id.clone(),
                ts: envelope.ts.clone(),
                from: "server".to_string(),
                body: Body::JoinOk(JoinOkBody {
                    client_id: result.client_id,
                    credential: result.credential,
                }),
            };
            let _ = write.send(Message::Text(encode(&reply).into())).await;
        }
        Err(e) => {
            // Same lockout accounting as a failed `auth` (issue #58) — a
            // bad join secret is exactly the kind of connection noise this
            // control targets.
            ctx.lockout.record_failure(peer_ip);
            trace(
                ctx,
                &format!(
                    "[{peer}] join rejected for {}: {e}",
                    redact("token_id", &token_id)
                ),
            );
            let _ = write
                .send(Message::Text(
                    encode(&proto::error_with_message(
                        CODE_JOIN_FAILED,
                        &e.to_string(),
                        &envelope.id,
                        "server",
                    ))
                    .into(),
                ))
                .await;
        }
    }
    // ADR 0015: the server closes the connection right after replying,
    // success or failure alike. A real WebSocket close handshake (not
    // just dropping the TCP stream) so a well-behaved client sees a
    // clean `Close`, not a reset.
    let _ = write.send(Message::Close(None)).await;
    let _ = write.close().await;
}

/// Handle one decoded-or-not frame received after auth. Returns `false`
/// when the connection should close (malformed frame / unsupported
/// version — ADR 0009 fail-closed).
async fn handle_frame(
    raw: &str,
    token_id: &str,
    client_id: &str,
    ctx: &Arc<ConnectionContext>,
    write: &mut (impl Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
) -> bool {
    let envelope = match proto::decode(raw) {
        Ok(e) => e,
        Err(err) => {
            let _ = write
                .send(Message::Text(encode(&decode_error_envelope(&err)).into()))
                .await;
            trace(ctx, &format!("frame did not decode: {err}"));
            return false;
        }
    };

    match &envelope.body {
        Body::Hello(hello) => {
            ctx.registry.set_hostname(token_id, hello.hostname.clone());
            trace(ctx, &format!("client hello: hostname={}", hello.hostname));
        }
        Body::Query(QueryBody { cmd, args }) => {
            // A connected client asking the server the same peer
            // questions the server can ask it (spec §7: "The server
            // answers as a peer") — status/caps/support/protocol,
            // dispatched by the same `query` module the control channel
            // uses for `holler status`/`support`/`caps`/`query` (#37).
            let confirmed = ctx.registry.confirmed_harnesses_snapshot();
            let reply = match query::dispatch(
                cmd,
                args,
                &ctx.server_hostname,
                ctx.listening.as_slice(),
                ctx.registry.len(),
                &confirmed,
            ) {
                Ok(body) => Envelope {
                    v: 1,
                    msg_type: MessageType::QueryOk,
                    id: envelope.id.clone(),
                    ts: envelope.ts.clone(),
                    from: "server".to_string(),
                    body: Body::QueryOk(Box::new(body)),
                },
                Err(err_body) => Envelope {
                    v: 1,
                    msg_type: MessageType::Error,
                    id: envelope.id.clone(),
                    ts: envelope.ts.clone(),
                    from: "server".to_string(),
                    body: Body::Error(err_body),
                },
            };
            let _ = write.send(Message::Text(encode(&reply).into())).await;
        }
        Body::QueryOk(body) => {
            // A reply to a `query` **this server sent** the client
            // (`holler status/support/caps/query <id>`, issue #37) —
            // resolve the matching outbound request, if any.
            ctx.registry
                .resolve_query_ok(token_id, &envelope.id, body.as_ref().clone());
        }
        Body::Ping(ping) => {
            let hostname = ping.hostname.clone().unwrap_or_default();
            trace(ctx, &format!("client ping from {hostname}"));
            let _ = write
                .send(Message::Text(
                    encode(&new_pong_envelope(&envelope.id, &ctx.server_hostname)).into(),
                ))
                .await;
        }
        Body::Pong(_) => {
            ctx.registry.resolve_pong(token_id, &envelope.id);
        }
        Body::Error(err_body) => {
            // Either a reply to a `query` this server sent (issue #37 —
            // e.g. the remote client's own `unknown_cmd`), a reply to a
            // `prompt` this server sent (issue #33 — e.g. a stale
            // `presence` row: the client no longer hosts the session the
            // roster said it did), or an unsolicited `error` this story
            // has no other use for. Both `resolve_*_err` calls are
            // no-ops if `envelope.id` matches no outstanding request of
            // that kind — a given id is only ever pending in one of the
            // two maps, so exactly one call (if either) actually fires.
            ctx.registry
                .resolve_query_err(token_id, &envelope.id, err_body.clone());
            ctx.registry
                .resolve_prompt_err(token_id, &envelope.id, err_body.clone());
        }
        Body::Auth(_) => {
            // A second `auth` mid-session is not part of this story's
            // scope (reconnect is a *new* connection); ignore rather
            // than tearing down an otherwise-good session over it.
            trace(ctx, "ignoring mid-session `auth` frame");
        }
        Body::Join(_) | Body::JoinOk(_) => {
            // `join` is legal only as the first frame (ADR 0015); a
            // connection that reaches `handle_frame` has already
            // authenticated, so a `join`/`join_ok` here is out of
            // sequence. Ignore rather than tearing down an otherwise-good
            // session over it, mirroring the mid-session `auth` case.
            trace(
                ctx,
                &format!("ignoring mid-session {:?} frame", envelope.msg_type),
            );
        }
        Body::Presence(PresenceBody { sessions }) => {
            // No ack (research memo, message-integrity §3: `presence`
            // is a self-healing heartbeat) and no wire error on a
            // malformed row or a name collision — either way, the next
            // heartbeat tick is the retry (issue #32).
            for raw_session in sessions {
                match serde_json::from_value::<PresenceSession>(raw_session.clone()) {
                    Ok(session) => {
                        if let Err(conflict) = ctx.roster.advertise(
                            session.name.clone(),
                            session.harness,
                            token_id,
                            client_id,
                        ) {
                            trace(ctx, &format!("presence: {conflict}, ignoring advertise"));
                        }
                    }
                    Err(e) => {
                        trace(ctx, &format!("presence: skipping malformed session row: {e}"));
                    }
                }
            }
        }
        Body::Reply(reply_body) => {
            // A client answering a `prompt` this server sent (issue #33:
            // `holler say <session>` routing). Persisted unconditionally
            // (the durable talk log a later `holler wait` reads), then
            // forwarded to whichever pending `Registry::prompt` call is
            // waiting on this correlation id, if any — a reply with no
            // matching pending prompt (e.g. this process restarted
            // mid-exchange) is logged but otherwise a no-op, same
            // fail-open tolerance `resolve_query_err` already has for an
            // unmatched `query` reply.
            ctx.talklog
                .record_reply(&envelope.id, &reply_body.session, reply_body);
            ctx.registry
                .resolve_reply(token_id, &envelope.id, reply_body.clone());
        }
        Body::Prompt(_) | Body::Interrupt(_) | Body::Ack(_) => {
            // Not wired up by issue #31 (talk/roster land in later
            // stories); accept the frame without erroring so a client
            // speaking ahead of this server's capabilities does not get
            // disconnected over it.
            trace(
                ctx,
                &format!("ignoring unimplemented frame type {:?}", envelope.msg_type),
            );
        }
    }
    true
}

/// Read the next `Message::Text` off the stream, treating anything else
/// (close, error, EOF) as "no auth frame arrived" — the caller closes.
async fn next_text_frame(
    read: &mut (impl Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Option<String> {
    loop {
        match read.next().await {
            Some(Ok(Message::Text(t))) => return Some(t.to_string()),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            _ => return None,
        }
    }
}

fn decode_error_envelope(err: &DecodeError) -> Envelope {
    match err {
        DecodeError::UnsupportedVersion(v) => proto::error_with_message(
            proto::CODE_UNSUPPORTED_VERSION,
            &format!("unsupported protocol version {v}"),
            "",
            "server",
        ),
        DecodeError::UnknownType => {
            proto::error_with_message(CODE_UNKNOWN_TYPE, "unknown message type", "", "server")
        }
        DecodeError::Malformed(msg) => proto::error_with_message(
            CODE_UNKNOWN_TYPE,
            &format!("malformed frame: {msg}"),
            "",
            "server",
        ),
    }
}

fn encode(envelope: &Envelope) -> String {
    proto::encode(envelope).expect("v1 envelope types always serialize")
}

pub(crate) fn trace(ctx: &ConnectionContext, msg: &str) {
    if ctx.debug != DebugLevel::None {
        eprintln!("holler-server: {msg}");
    }
}
