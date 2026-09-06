//! Envelope builders shared by the connection handler and the registry:
//! this process's `hello`/`status` self-description, and the `ping`
//! envelope the registry sends to drive an RTT round trip.
//!
//! Advertise only what is actually implemented (`docs/protocol/v1.md`
//! §6: "Advertise only what is real"): `query` (only `status` today —
//! the full dispatcher is #37), `ping`, and, as of issue #32,
//! `presence`/`roster`. Not `interrupt`, which no story has wired yet.

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use serde_json::json;

use crate::proto::{Body, Envelope, HelloBody, MessageType, PingBody, PongBody, QueryOkBody, Role};

/// This binary's protocol version (spec §2): every process today has
/// `min = 1`, `max = 1`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Protocol features this server process actually implements: `query
/// status` and `ping`/`pong` (issue #31), `presence`/`roster` (issue
/// #32). `token` describes the CLI's own token-management surface,
/// which is real regardless of the wire.
pub const SERVER_FEATURES: &[&str] = &["query", "ping", "token", "presence", "roster"];

/// The v1 harness-id vocabulary (spec §9). `holler-server` never runs a
/// harness itself — this is the set of ids it *knows about*, not ids it
/// can confirm (that requires a live client's `support` answer, #37).
pub const HARNESSES_KNOWN: &[&str] = &[
    "opencode", "claude", "codex", "grok", "hermes", "pi", "cursor", "copilot", "droid", "kimi",
    "qwen", "kilo", "goose",
];

fn now_ts() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// `n` random hex bytes as a correlation id. Not a real ULID (no
/// monotonic timestamp component) — v1 only requires *unique*, not
/// sortable, and `docs/protocol/v1.md` §3 just says "ULID or UUID".
pub fn new_id() -> String {
    let mut buf = [0u8; 16];
    // The OS CSPRNG failing here is as fatal as it is for token minting;
    // fall back to a process-time-derived id rather than panicking a
    // live connection handler over a correlation id.
    if getrandom::getrandom(&mut buf).is_err() {
        return format!("id-{}", OffsetDateTime::now_utc().unix_timestamp_nanos());
    }
    let mut s = String::with_capacity(32);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// This process's `hello` (spec §6): `role: server`, real hostname,
/// only the features/harnesses this story actually implements.
pub fn server_hello(hostname: &str) -> Envelope {
    Envelope {
        v: 1,
        msg_type: MessageType::Hello,
        id: new_id(),
        ts: now_ts(),
        from: "server".to_string(),
        body: Body::Hello(HelloBody {
            protocol: PROTOCOL_VERSION,
            protocol_min: PROTOCOL_VERSION,
            protocol_max: PROTOCOL_VERSION,
            role: Role::Server,
            hostname: hostname.to_string(),
            token_id: None,
            client_id: None,
            harnesses: Vec::new(),
            harnesses_known: HARNESSES_KNOWN.iter().map(|s| s.to_string()).collect(),
            harnesses_confirmed: Vec::new(),
            features: SERVER_FEATURES.iter().map(|s| s.to_string()).collect(),
            sessions: Vec::new(),
        }),
    }
}

/// A `ping` envelope the registry sends to a live connection to drive an
/// RTT round trip for `holler token ping`.
pub fn new_ping_envelope(id: &str, server_hostname: &str) -> Envelope {
    Envelope {
        v: 1,
        msg_type: MessageType::Ping,
        id: id.to_string(),
        ts: now_ts(),
        from: "server".to_string(),
        body: Body::Ping(PingBody {
            hostname: Some(server_hostname.to_string()),
        }),
    }
}

/// Body of a `query_ok` answer to `query status` (spec §7 "status
/// document") — same shape whether asked over the wire by a client or
/// printed locally by `holler status`. `listening` is the list of
/// addresses this process actually bound; empty when this is a
/// `holler status` invocation with no live server on this host (see
/// `wire::control`).
pub fn status_query_ok_body(hostname: &str, listening: &[String], clients: usize) -> QueryOkBody {
    QueryOkBody {
        cmd: "status".to_string(),
        rest: json!({
            "role": "server",
            "protocol": PROTOCOL_VERSION,
            "protocol_min": PROTOCOL_VERSION,
            "protocol_max": PROTOCOL_VERSION,
            "hostname": hostname,
            "listening": listening,
            "features": SERVER_FEATURES,
            "harnesses_known": HARNESSES_KNOWN,
            "harnesses_confirmed": [],
            "clients": clients,
            "sessions": 0,
        }),
    }
}

/// The `pong` this server sends back when a client `ping`s it.
pub fn new_pong_envelope(reply_id: &str, server_hostname: &str) -> Envelope {
    Envelope {
        v: 1,
        msg_type: MessageType::Pong,
        id: reply_id.to_string(),
        ts: now_ts(),
        from: "server".to_string(),
        body: Body::Pong(PongBody {
            hostname: Some(server_hostname.to_string()),
        }),
    }
}
