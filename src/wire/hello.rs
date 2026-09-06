//! Envelope builders shared by the connection handler and the registry:
//! this process's `hello`/`status` self-description, the `ping`
//! envelope the registry sends to drive an RTT round trip, and the
//! generalized outbound `query` envelope (issue #37).
//!
//! Advertise only what this process actually implements
//! (`docs/protocol/v1.md` §6: "Advertise only what is real"): `query`
//! (the full dispatcher, issue #37), `ping`, `token`, `presence`/`roster`
//! (issue #32), and, as of issue #34, `interrupt`. Not `wait`, which no
//! story has wired yet — see [`PROTOCOL_FEATURES`] for the full
//! vocabulary those unimplemented ids come from.

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::proto::{
    Body, Envelope, HelloBody, InterruptBody, MessageType, PingBody, PongBody, PromptBody,
    QueryBody, Role,
};

/// This binary's protocol version (spec §2): every process today has
/// `min = 1`, `max = 1`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Protocol features this server process actually implements: `query
/// status` and `ping`/`pong` (issue #31), `presence`/`roster` (issue
/// #32), `interrupt` (issue #34, ADR 0005). `token` describes the CLI's
/// own token-management surface, which is real regardless of the wire.
pub const SERVER_FEATURES: &[&str] = &["query", "ping", "token", "presence", "roster", "interrupt"];

/// The full v1 protocol-feature vocabulary (spec §9) — every id `holler
/// caps`/`holler support` knows to report on, independent of what this
/// process actually implements (see [`SERVER_FEATURES`], always a
/// subset of this list).
pub const PROTOCOL_FEATURES: &[&str] =
    &["interrupt", "presence", "ping", "query", "roster", "token", "wait"];

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

/// A `query` envelope the server sends to a live connection to ask it
/// something (issue #37: `holler status <id>` / `support <id> ...` /
/// `caps <id>` / `query <id> ...`) — the outbound counterpart of
/// [`new_ping_envelope`], generalized to carry any `cmd`/`args`. The
/// answer shapes (`status`/`caps`/`support`/`protocol` bodies) live in
/// [`super::query`], which builds the same documents whether asked over
/// the wire or by a local CLI invocation with no live connection at all.
pub fn new_query_envelope(id: &str, cmd: String, args: Vec<String>) -> Envelope {
    Envelope {
        v: 1,
        msg_type: MessageType::Query,
        id: id.to_string(),
        ts: now_ts(),
        from: "server".to_string(),
        body: Body::Query(QueryBody { cmd, args }),
    }
}

/// A `prompt` envelope the registry sends to a live connection to route
/// `holler say <session> <text>` (issue #33) by session name — the
/// outbound counterpart of [`new_query_envelope`], addressed by whichever
/// live connection the roster says currently hosts `session` (ADR 0007).
pub fn new_prompt_envelope(id: &str, session: String, text: String, meta: Option<Value>) -> Envelope {
    Envelope {
        v: 1,
        msg_type: MessageType::Prompt,
        id: id.to_string(),
        ts: now_ts(),
        from: "server".to_string(),
        body: Body::Prompt(PromptBody { session, text, meta }),
    }
}

/// An `interrupt` envelope the registry sends to a live connection to
/// route `holler interrupt <session>` (issue #34, ADR 0005) by session
/// name — a **control** frame, not a `prompt`: it never touches
/// [`super::registry::Registry`]'s `pending_prompts` map, so it reaches
/// the connection immediately even while a `prompt` round trip for the
/// same (or a sibling) session is still in flight on the same
/// connection's unbounded outbound channel.
pub fn new_interrupt_envelope(id: &str, session: String) -> Envelope {
    Envelope {
        v: 1,
        msg_type: MessageType::Interrupt,
        id: id.to_string(),
        ts: now_ts(),
        from: "server".to_string(),
        body: Body::Interrupt(InterruptBody { session }),
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
