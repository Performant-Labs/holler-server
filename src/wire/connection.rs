//! Per-connection handling (issue #31): one task per accepted WebSocket,
//! implementing `connect -> auth -> hello (both ways) -> query / ping`
//! (`docs/protocol/v1.md` §4) and the fail-closed error paths of ADR 0009.

use std::sync::Arc;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::debug::{redact, DebugLevel};
use crate::proto::{
    self, AuthBody, Body, DecodeError, Envelope, MessageType, QueryBody, CODE_UNAUTHENTICATED,
    CODE_UNKNOWN_TYPE,
};
use crate::token::TokenStore;

use super::hello::{new_pong_envelope, server_hello, status_query_ok_body};
use super::registry::Registry;

/// Shared, read-only context every connection task needs. Grouped so
/// `handle_connection`'s signature does not grow a parameter per field.
pub struct ConnectionContext {
    pub store: Arc<TokenStore>,
    pub registry: Arc<Registry>,
    pub server_hostname: Arc<str>,
    pub listening: Arc<Vec<String>>,
    pub debug: DebugLevel,
}

/// Drive one accepted TCP connection through the WebSocket handshake and
/// the Holler v1 session. Never panics on a misbehaving peer — every
/// failure path logs (at `noisy`, redacted) and returns, dropping the
/// connection.
pub async fn handle_connection(stream: TcpStream, ctx: Arc<ConnectionContext>) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(s) => s,
        Err(e) => {
            trace(&ctx, &format!("websocket handshake failed: {e}"));
            return;
        }
    };
    let (mut write, mut read) = ws_stream.split();

    // 1. First frame must be `auth` (spec §4). Anything else — wrong
    // type, undecodable, or a version/type the codec already rejects —
    // fails closed: reply `error` and close, per ADR 0009.
    let raw = match next_text_frame(&mut read).await {
        Some(t) => t,
        None => return,
    };
    let envelope = match proto::decode(&raw) {
        Ok(e) => e,
        Err(err) => {
            let _ = write
                .send(Message::Text(encode(&decode_error_envelope(&err)).into()))
                .await;
            trace(&ctx, &format!("first frame did not decode: {err}"));
            return;
        }
    };
    let Body::Auth(AuthBody { credential }) = &envelope.body else {
        let _ = write
            .send(Message::Text(
                encode(&proto::error_with_message(
                    CODE_UNAUTHENTICATED,
                    "first frame must be `auth`",
                    &envelope.id,
                    "server",
                ))
                .into(),
            ))
            .await;
        return;
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
            trace(
                &ctx,
                &format!("auth rejected for {}: {e}", redact("token_id", &token_id)),
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
            "client {} authenticated",
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
                        if !handle_frame(&text, &verified.token_id, &ctx, &mut write).await {
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
            "client {} disconnected",
            redact("token_id", &verified.token_id)
        ),
    );
}

/// Handle one decoded-or-not frame received after auth. Returns `false`
/// when the connection should close (malformed frame / unsupported
/// version — ADR 0009 fail-closed).
async fn handle_frame(
    raw: &str,
    token_id: &str,
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
        Body::Query(QueryBody { cmd, .. }) if cmd == "status" => {
            let listening = ctx.listening.as_slice();
            let clients = ctx.registry.len();
            let body = status_query_ok_body(&ctx.server_hostname, listening, clients);
            let reply = Envelope {
                v: 1,
                msg_type: MessageType::QueryOk,
                id: envelope.id.clone(),
                ts: envelope.ts.clone(),
                from: "server".to_string(),
                body: Body::QueryOk(Box::new(body)),
            };
            let _ = write.send(Message::Text(encode(&reply).into())).await;
        }
        Body::Query(QueryBody { cmd, .. }) => {
            let _ = write
                .send(Message::Text(
                    encode(&proto::error_for_unknown_cmd(cmd, &envelope.id, "server")).into(),
                ))
                .await;
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
        Body::Auth(_) => {
            // A second `auth` mid-session is not part of this story's
            // scope (reconnect is a *new* connection); ignore rather
            // than tearing down an otherwise-good session over it.
            trace(ctx, "ignoring mid-session `auth` frame");
        }
        Body::Prompt(_)
        | Body::Reply(_)
        | Body::Interrupt(_)
        | Body::Presence(_)
        | Body::Ack(_)
        | Body::Error(_)
        | Body::QueryOk(_) => {
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

fn trace(ctx: &ConnectionContext, msg: &str) {
    if ctx.debug != DebugLevel::None {
        eprintln!("holler-server: {msg}");
    }
}
