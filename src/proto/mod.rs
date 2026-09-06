//! Holler protocol v1 — types + JSON codec (issue #28).
//!
//! Canonical spec: `docs/protocol/v1.md`. One JSON object per WebSocket
//! text frame: [`Envelope`] wraps a `Body` that is an extensible enum
//! over the 12 v1 message types.

use serde::de::Error as _;
use serde::Deserializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// --------------------------------------------------------------------------
// Error codes (spec §11)
// --------------------------------------------------------------------------

/// `v` on the wire is not 1 (spec §2, §11).
pub const CODE_UNSUPPORTED_VERSION: &str = "unsupported_version";
/// Unknown envelope `type` (spec §3, §11).
pub const CODE_UNKNOWN_TYPE: &str = "unknown_type";
/// `auth` missing, malformed, or the credential does not verify (spec §11).
pub const CODE_UNAUTHENTICATED: &str = "unauthenticated";
/// Unknown `query` `cmd` (spec §11).
pub const CODE_UNKNOWN_CMD: &str = "unknown_cmd";
/// Server `query`/`ping` to a bound token with no live socket (spec §11).
pub const CODE_NOT_CONNECTED: &str = "not_connected";

// --------------------------------------------------------------------------
// Envelope (spec §3)
// --------------------------------------------------------------------------

/// Who is speaking (spec §6).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Role {
    /// The client side of the circuit.
    #[serde(rename = "client")]
    #[default]
    Client,
    /// The hub (server).
    #[serde(rename = "server")]
    Server,
}

/// The 12 v1 message types (spec §5).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum MessageType {
    /// client → server: present credential.
    #[serde(rename = "auth")]
    Auth,
    /// both: protocol version, hostname, features, harnesses, sessions.
    #[serde(rename = "hello")]
    Hello,
    /// both: command + args (control, not a prompt).
    #[serde(rename = "query")]
    Query,
    /// both: structured answer to `query`.
    #[serde(rename = "query_ok")]
    QueryOk,
    /// server → client: prompt a named session.
    #[serde(rename = "prompt")]
    Prompt,
    /// client → server: session output / turn result.
    #[serde(rename = "reply")]
    Reply,
    /// server → client: cancel the turn; session lives.
    #[serde(rename = "interrupt")]
    Interrupt,
    /// client → server: session advertise + heartbeat.
    #[serde(rename = "presence")]
    Presence,
    /// both: bound-socket aliveness.
    #[serde(rename = "ping")]
    Ping,
    /// both: bound-socket aliveness.
    #[serde(rename = "pong")]
    Pong,
    /// both: optional receipt.
    #[serde(rename = "ack")]
    Ack,
    /// both: typed failure.
    #[serde(rename = "error")]
    Error,
}

/// One WebSocket text frame: the JSON envelope (spec §3).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Envelope {
    /// Version of this connection; always `1` in v1 (spec §2).
    pub v: u32,
    /// Reserved word in JSON-RPC-ish shapes; renamed (spec §3).
    #[serde(rename = "type")]
    pub msg_type: MessageType,
    /// Correlation id (ULID or UUID); replies reuse the request id.
    pub id: String,
    /// Opaque RFC 3339 string (spec §3): round-tripped, never parsed
    /// or compared by the codec.
    pub ts: String,
    /// Client: public `token_id`. Server peer: `"server"`.
    pub from: String,
    /// Shape depends on `type` (spec §3).
    pub body: Body,
}

/// One body variant per [`MessageType`]. The variant name matches the
/// wire `type` string.
///
/// The wire shape is a **bare object** — no variant-name tag — so a
/// `query_ok` body is `{"cmd":"protocol","min":1,...}`, not
/// `{"QueryOk":{...}}`. serde's enum tagging cannot express that (the
/// discriminator lives in the *envelope* `type`, not in `body`), so
/// [`Body`] gets manual [`Serialize`]/[`Deserialize`] impls below that
/// serialize/deserialize the inner struct directly. The `Body` enum
/// itself is never (de)serialized as a serde enum.
#[derive(Clone, PartialEq, Debug)]
pub enum Body {
    Auth(AuthBody),
    Hello(HelloBody),
    Query(QueryBody),
    QueryOk(Box<QueryOkBody>),
    Prompt(PromptBody),
    Reply(ReplyBody),
    Interrupt(InterruptBody),
    Presence(PresenceBody),
    Ping(PingBody),
    Pong(PongBody),
    Ack(AckBody),
    Error(ErrorBody),
}

// --------------------------------------------------------------------------
// Bodies (spec §4–§11)
// --------------------------------------------------------------------------

/// Spec leaves the auth body shape open (issue #28); modeled as a thin
/// wrapper around an opaque credential object so we can parse/echo it
/// without inventing a field name.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct AuthBody {
    pub credential: Value,
}

/// `hello` body (spec §6). Role-specific fields are optional; the
/// client lists its sessions, the server omits them.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct HelloBody {
    pub protocol: u32,
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub role: Role,
    pub hostname: String,
    // client-only:
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<String>,
    // server-only:
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses_known: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses_confirmed: Vec<String>,
    // shared:
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    // client hello lists its sessions; server hello omits them:
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<HelloSession>,
}

/// A named session in a client `hello` (spec §6).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct HelloSession {
    pub name: String,
    pub harness: String,
}

/// `query` body (spec §7). `cmd` is kept as `String` (not a closed
/// enum) so an unknown cmd can be echoed into an `unknown_cmd` error.
/// The four known cmds are status / caps / support / protocol.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct QueryBody {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// `query_ok` body (spec §7). The spec commits to exactly one field —
/// `cmd`, which selects the answer — and leaves the rest of the shape
/// (the flat union of status / caps / support / protocol fields) open,
/// so it is carried as a [`Value`] the caller parses per-cmd.
///
/// `cmd` is always present on the wire (it selects the field group) —
/// it is a required field here, not an `Option`, so a `query_ok` frame
/// missing `cmd` is a malformed frame, not a silent `None`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct QueryOkBody {
    pub cmd: String,
    #[serde(flatten)]
    pub rest: Value,
}

/// `prompt` body (spec §10).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct PromptBody {
    pub session: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// `reply` body (spec §10).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ReplyBody {
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<String>,
    #[serde(default)]
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i64>,
}

/// `interrupt` body (spec §10).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct InterruptBody {
    pub session: String,
}

/// `presence` body (spec §10). The spec does not pin a heartbeat field
/// beyond `sessions`, so each session row is left as an opaque
/// [`Value`] rather than inventing a shape.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct PresenceBody {
    pub sessions: Vec<Value>,
}

/// `ping` body (spec §10): empty or `{hostname}`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct PingBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

/// `pong` body (spec §10): empty or `{hostname}`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct PongBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

/// `ack` body (spec §10): optional receipt referencing the
/// acknowledged frame id. Shape unspecified; kept minimal.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct AckBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<String>,
}

/// `error` body (spec §11).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ErrorBody {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// --------------------------------------------------------------------------
// Codec
// --------------------------------------------------------------------------

/// Failures from [`decode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Envelope `v` is present but not `1` (spec §2: a v1 server
    /// requires `v == 1`; no silent downgrade).
    UnsupportedVersion(u32),
    /// Envelope `type` is not one of the 12 v1 types (spec §3).
    UnknownType,
    /// Frame is not valid JSON, or JSON that does not fit the
    /// envelope schema. `serde_json::Error` is not `Clone`, so we
    /// carry the rendered message.
    Malformed(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::UnsupportedVersion(v) => write!(
                f,
                "unsupported protocol version {v} (v1 requires v == 1)"
            ),
            DecodeError::UnknownType => f.write_str("unknown message type"),
            DecodeError::Malformed(msg) => write!(f, "malformed frame: {msg}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Serialize an envelope to its wire form (one WebSocket text frame).
///
/// Fails only on serializer errors (e.g. a non-string map key); with
/// the v1 types this does not happen in practice.
pub fn encode(envelope: &Envelope) -> serde_json::Result<String> {
    serde_json::to_string(envelope)
}

/// Parse one WebSocket text frame into an [`Envelope`], then enforce
/// the v1 invariant: envelope `v` must be `1`.
///
/// The envelope is deserialized by hand (not via serde's derive) because
/// [`Body`] is selected by the *envelope* `type`, which serde's enum
/// tagging cannot reach.
///
/// * A frame that is not JSON is [`DecodeError::Malformed`].
/// * A frame whose `type` is not one of the 12 v1 types is
///   [`DecodeError::UnknownType`] (spec §3: "Do not ignore it as
///   success.").
/// * A frame whose JSON is valid and `type` is known, but whose `v` is
///   not `1`, is [`DecodeError::UnsupportedVersion`] (spec §2 — no
///   silent downgrade).
/// * A frame whose `body` does not fit the schema for its `type`
///   (or whose required envelope fields are missing) is
///   [`DecodeError::Malformed`].
pub fn decode(raw: &str) -> Result<Envelope, DecodeError> {
    let parsed = parse_envelope(raw)?;
    if parsed.v != 1 {
        return Err(DecodeError::UnsupportedVersion(parsed.v));
    }
    parsed.try_into()
}

/// Intermediate form for decoding: an envelope whose `body` is still a
/// raw `serde_json::Value`. Lets us classify the failure (`unknown type`
/// vs `bad version` vs `malformed`) with a single pass.
#[derive(Debug)]
struct ParsedEnvelope {
    v: u32,
    msg_type: MessageType,
    id: String,
    ts: String,
    from: String,
    body: Value,
}

impl TryFrom<ParsedEnvelope> for Envelope {
    type Error = DecodeError;

    fn try_from(parsed: ParsedEnvelope) -> Result<Self, DecodeError> {
        let body = Body::deserialize_value(parsed.body, parsed.msg_type).map_err(|e| match e {
            DecodeError::Malformed(msg) => DecodeError::Malformed(format!(
                "            body for `type: {}`: {msg}",
                parsed.msg_type.as_wire_str()
            )),
            other => other,
        })?;
        Ok(Envelope {
            v: parsed.v,
            msg_type: parsed.msg_type,
            id: parsed.id,
            ts: parsed.ts,
            from: parsed.from,
            body,
        })
    }
}

/// Decode a raw frame into a [`ParsedEnvelope`], classifying errors
/// before the v-invariant check.
fn parse_envelope(raw: &str) -> Result<ParsedEnvelope, DecodeError> {
    // First, confirm it is JSON at all (a non-JSON frame is malformed,
    // not an unknown type).
    let value: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            // A leading `[` or scalar is the most common "not an
            // envelope" failure.
            return Err(DecodeError::Malformed(e.to_string()));
        }
    };
    let obj = match value {
        Value::Object(o) => o,
        _ => {
            return Err(DecodeError::Malformed(
                "envelope must be a JSON object".into(),
            ));
        }
    };

    // `type` is the discriminator: read it first so we can report an
    // unknown type before spending effort on the other fields.
    let type_str = match obj.get("type") {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(DecodeError::Malformed(
                "missing or non-string `type`".into(),
            ));
        }
    };
    let msg_type = match MessageType::from_wire(&type_str) {
        Some(t) => t,
        None => return Err(DecodeError::UnknownType),
    };

    // Now read the required scalar fields.
    let v = match obj.get("v").and_then(|v| v.as_u64()) {
        Some(v) => v as u32,
        None => {
            return Err(DecodeError::Malformed("missing or non-numeric `v`".into()));
        }
    };
    let id = match obj.get("id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return Err(DecodeError::Malformed("missing or non-string `id`".into()));
        }
    };
    // `ts` is an opaque RFC 3339 string: round-tripped, never parsed or
    // compared by the codec. It is a required envelope field (spec §3),
    // so a frame without one is malformed. (The spec's §7/§11 examples
    // omit it, but §3 requires it — the spec is inconsistent on this and
    // the strict reading wins; see issue #28.)
    let ts = match obj.get("ts").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return Err(DecodeError::Malformed("missing or non-string `ts`".into()));
        }
    };
    let from = match obj.get("from").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return Err(DecodeError::Malformed("missing or non-string `from`".into()));
        }
    };
    let body = match obj.get("body") {
        Some(b) => b.clone(),
        None => {
            return Err(DecodeError::Malformed("missing `body`".into()));
        }
    };

    Ok(ParsedEnvelope {
        v,
        msg_type,
        id,
        ts,
        from,
        body,
    })
}

impl MessageType {
    /// Map a wire `type` string to a [`MessageType`]; `None` if the
    /// string is not one of the 12 v1 types.
    fn from_wire(s: &str) -> Option<Self> {
        match s {
            "auth" => Some(Self::Auth),
            "hello" => Some(Self::Hello),
            "query" => Some(Self::Query),
            "query_ok" => Some(Self::QueryOk),
            "prompt" => Some(Self::Prompt),
            "reply" => Some(Self::Reply),
            "interrupt" => Some(Self::Interrupt),
            "presence" => Some(Self::Presence),
            "ping" => Some(Self::Ping),
            "pong" => Some(Self::Pong),
            "ack" => Some(Self::Ack),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// The wire `type` string for this message type (the inverse of
    /// [`MessageType::from_wire`]).
    fn as_wire_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Hello => "hello",
            Self::Query => "query",
            Self::QueryOk => "query_ok",
            Self::Prompt => "prompt",
            Self::Reply => "reply",
            Self::Interrupt => "interrupt",
            Self::Presence => "presence",
            Self::Ping => "ping",
            Self::Pong => "pong",
            Self::Ack => "ack",
            Self::Error => "error",
        }
    }
}

// --------------------------------------------------------------------------
// Body (de)serialization
// --------------------------------------------------------------------------

/// Serialize a [`Body`] to its wire form — the **bare inner object**,
/// with no variant-name tag. The envelope's `type` carries the
/// discriminator; `body` is just the object.
///
/// This is a hand-written impl (not serde's derive) because serde's
/// enum tagging would wrap the body in a `{"Variant": ...}` object,
/// which is not the wire shape.
impl Serialize for Body {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Body::Auth(b) => b.serialize(serializer),
            Body::Hello(b) => b.serialize(serializer),
            Body::Query(b) => b.serialize(serializer),
            Body::QueryOk(b) => b.serialize(serializer),
            Body::Prompt(b) => b.serialize(serializer),
            Body::Reply(b) => b.serialize(serializer),
            Body::Interrupt(b) => b.serialize(serializer),
            Body::Presence(b) => b.serialize(serializer),
            Body::Ping(b) => b.serialize(serializer),
            Body::Pong(b) => b.serialize(serializer),
            Body::Ack(b) => b.serialize(serializer),
            Body::Error(b) => b.serialize(serializer),
        }
    }
}

/// Deserialize a [`Body`] from a raw JSON value, given the envelope
/// `type` that selected it. Called by [`ParsedEnvelope::try_into`].
///
/// Returns a [`DecodeError`] (not a serde error) so the codec can
/// report `unknown type` vs `malformed body` distinctly.
impl Body {
    fn deserialize_value(body: Value, msg_type: MessageType) -> Result<Self, DecodeError> {
        let malformed = |msg: String| DecodeError::Malformed(msg);
        match msg_type {
            MessageType::Auth => Ok(Body::Auth(
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?,
            )),
            MessageType::Hello => Ok(Body::Hello(
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?,
            )),
            MessageType::Query => Ok(Body::Query(
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?,
            )),
            MessageType::QueryOk => Ok(Body::QueryOk(Box::new(
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?,
            ))),
            MessageType::Prompt => Ok(Body::Prompt(
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?,
            )),
            MessageType::Reply => Ok(Body::Reply(
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?,
            )),
            MessageType::Interrupt => Ok(Body::Interrupt(
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?,
            )),
            MessageType::Presence => Ok(Body::Presence(
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?,
            )),
            MessageType::Ping => Ok(Body::Ping(
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?,
            )),
            MessageType::Pong => Ok(Body::Pong(
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?,
            )),
            MessageType::Ack => Ok(Body::Ack(
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?,
            )),
            MessageType::Error => Ok(Body::Error(
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?,
            )),
        }
    }
}

/// A `Deserialize` impl for `Body` is not needed by the codec (it
/// dispatches via `Body::deserialize_value`); it exists only so `Body`
/// remains a well-formed serde type. Decoding a `Body` in isolation is
/// ambiguous — the discriminator lives in the envelope `type`, not in
/// the body — so this impl drains the value and returns an error
/// directing callers to [`decode`], which carries the type.
impl<'de> Deserialize<'de> for Body {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let _value: Value = Deserialize::deserialize(deserializer)?;
        Err(D::Error::custom(
            "a `body` is selected by the envelope `type`; decode the whole frame, not the body",
        ))
    }
}

// --------------------------------------------------------------------------
// Error-envelope constructors (spec §11)
// --------------------------------------------------------------------------

fn new_error_envelope(
    code: &str,
    cmd: Option<&str>,
    message: Option<String>,
    reply_id: &str,
    from: &str,
) -> Envelope {
    Envelope {
        v: 1,
        msg_type: MessageType::Error,
        id: reply_id.to_string(),
        ts: "1970-01-01T00:00:00Z".to_string(),
        from: from.to_string(),
        body: Body::Error(ErrorBody {
            code: code.to_string(),
            cmd: cmd.map(|s| s.to_string()),
            message,
        }),
    }
}

/// Build the `error` envelope for an unknown envelope `type`
/// (spec §3, §11): `code: "unknown_type"`, message names the type.
pub fn error_for_unknown_type(unknown_type: &str, reply_id: &str, from: &str) -> Envelope {
    new_error_envelope(
        CODE_UNKNOWN_TYPE,
        None,
        Some(format!("unknown type: {unknown_type}")),
        reply_id,
        from,
    )
}

/// Build a generic `error` envelope with just a `code` (spec §11).
pub fn error_for(code: &str, reply_id: &str, from: &str) -> Envelope {
    new_error_envelope(code, None, None, reply_id, from)
}

/// Build an `error` envelope with a human-readable `message` (spec §11).
pub fn error_with_message(code: &str, message: &str, reply_id: &str, from: &str) -> Envelope {
    new_error_envelope(code, None, Some(message.to_string()), reply_id, from)
}

/// Build the `error` envelope for an unknown `query` `cmd` (spec §7, §11):
/// `code: "unknown_cmd"`, echoing the offending `cmd`.
pub fn error_for_unknown_cmd(cmd: &str, reply_id: &str, from: &str) -> Envelope {
    new_error_envelope(
        CODE_UNKNOWN_CMD,
        Some(cmd),
        Some(format!("unknown query cmd: {cmd}")),
        reply_id,
        from,
    )
}
