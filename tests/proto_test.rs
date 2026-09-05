//! Issue #28: Holler protocol v1 — types + JSON codec.
//!
//! Canonical spec: `docs/protocol/v1.md`. These tests pin the wire
//! shape (round-trips, the v1 invariant, unknown type handling, and the
//! query_ok flat union against the spec's exact examples).

use holler_server::proto::{
    error_for, error_for_unknown_type, decode, encode, AckBody, AuthBody, Body, DecodeError,
    Envelope, ErrorBody, HelloBody, HelloSession, InterruptBody, MessageType, PingBody, PongBody,
    PresenceBody, PromptBody, QueryBody, QueryOkBody, ReplyBody, Role,
};
use serde_json::{json, Value};

const TS: &str = "2026-09-05T19:30:00Z";

fn make_envelope(msg_type: MessageType, id: &str, from: &str, body: Body) -> Envelope {
    Envelope {
        v: 1,
        msg_type,
        id: id.to_string(),
        ts: TS.to_string(),
        from: from.to_string(),
        body,
    }
}

// --------------------------------------------------------------------------
// Round trips
// --------------------------------------------------------------------------

#[test]
fn round_trip_representative_bodies() {
    // hello (client) — the spec §6 client example, plus the envelope
    // fields the spec shows in §3.
    let hello = make_envelope(
        MessageType::Hello,
        "01JHELLOCLIENT000000000000",
        "tok_7f3a",
        Body::Hello(HelloBody {
            protocol: 1,
            protocol_min: 1,
            protocol_max: 1,
            role: Role::Client,
            hostname: "kiwi".into(),
            token_id: Some("tok_7f3a".into()),
            client_id: Some("cli_19".into()),
            harnesses: vec!["opencode".into()],
            harnesses_known: vec![],
            harnesses_confirmed: vec![],
            features: vec![
                "interrupt".into(),
                "presence".into(),
                "ping".into(),
                "query".into(),
            ],
            sessions: vec![
                HelloSession { name: "alpha".into(), harness: "opencode".into() },
                HelloSession { name: "beta".into(), harness: "opencode".into() },
            ],
        }),
    );

    // query support — spec §7 request.
    let query = make_envelope(
        MessageType::Query,
        "01JQUERYSUPPORT0000000000",
        "server",
        Body::Query(QueryBody { cmd: "support".into(), args: vec!["opencode".into()] }),
    );

    // query_ok protocol — spec §7 (min/max only, no args). Everything
    // the spec does not close lives in `rest`.
    let qok_protocol = make_envelope(
        MessageType::QueryOk,
        "01JQUERYPROTOCOL000000000",
        "tok_7f3a",
        Body::QueryOk(Box::new(QueryOkBody {
            cmd: "protocol".into(),
            rest: json!({ "session": 1, "min": 1, "max": 1 }),
        })),
    );

    let prompt = make_envelope(
        MessageType::Prompt,
        "01JPROMPT0000000000000000",
        "server",
        Body::Prompt(PromptBody {
            session: "alpha".into(),
            text: "status please".into(),
            meta: None,
        }),
    );

    let reply = make_envelope(
        MessageType::Reply,
        "01JPROMPT0000000000000000",
        "tok_7f3a",
        Body::Reply(ReplyBody {
            session: "alpha".into(),
            text: Some("all up".into()),
            chunks: vec!["all ".into(), "up".into()],
            done: true,
            exit: Some(0),
        }),
    );

    let interrupt = make_envelope(
        MessageType::Interrupt,
        "01JINTERRUPT0000000000000",
        "server",
        Body::Interrupt(InterruptBody { session: "alpha".into() }),
    );

    // error — the spec §11 example.
    let error = Envelope {
        v: 1,
        msg_type: MessageType::Error,
        id: "01JQUERYSUPPORT0000000000".into(),
        ts: TS.to_string(),
        from: "tok_7f3a".into(),
        body: Body::Error(ErrorBody {
            code: "unknown_cmd".into(),
            cmd: Some("summarize".into()),
            message: Some("unknown query cmd".into()),
        }),
    };

    let ping = make_envelope(
        MessageType::Ping,
        "01JPING00000000000000000",
        "server",
        Body::Ping(PingBody { hostname: Some("uranus".into()) }),
    );

    let pong = make_envelope(
        MessageType::Pong,
        "01JPING00000000000000000",
        "tok_7f3a",
        Body::Pong(PongBody { hostname: None }),
    );

    let ack = make_envelope(
        MessageType::Ack,
        "01JACK000000000000000000",
        "server",
        Body::Ack(AckBody { of: Some("01JPONG000000000000000".into()) }),
    );

    let presence = make_envelope(
        MessageType::Presence,
        "01JPRESENCE00000000000000",
        "tok_7f3a",
        Body::Presence(PresenceBody {
            // Spec §10 does not pin a heartbeat field beyond sessions;
            // the spec's status example shows session rows as objects.
            sessions: vec![json!({ "name": "alpha", "harness": "opencode", "busy": false })],
        }),
    );

    // auth — the spec §4 first frame.
    let auth = Envelope {
        v: 1,
        msg_type: MessageType::Auth,
        id: "01JAUTH000000000000000000".into(),
        ts: TS.to_string(),
        from: "tok_7f3a".into(),
        body: Body::Auth(AuthBody {
            credential: json!({ "kind": "client", "secret": "s3cr3t" }),
        }),
    };

    for (envelope, label) in [
        (&hello, "hello"),
        (&query, "query"),
        (&qok_protocol, "query_ok"),
        (&prompt, "prompt"),
        (&reply, "reply"),
        (&interrupt, "interrupt"),
        (&error, "error"),
        (&ping, "ping"),
        (&pong, "pong"),
        (&ack, "ack"),
        (&presence, "presence"),
        (&auth, "auth"),
    ] {
        let encoded = encode(envelope).expect("encode should not fail");
        let decoded = decode(&encoded).expect("decode of an encoded frame should not fail");
        assert_eq!(&decoded, envelope, "round trip failed for `{label}`; wire: {encoded}");
    }

    // A known envelope encodes with `"v":1` (serde_json::to_string is
    // compact — no space after the colon).
    let encoded = encode(&hello).expect("encode hello");
    assert!(encoded.contains("\"v\":1"), "expected compact \"v\":1 in {encoded}");
}

// --------------------------------------------------------------------------
// The v1 invariant + unknown type
// --------------------------------------------------------------------------

#[test]
fn decode_rejects_v2_with_unsupported_version() {
    let raw = r#"{"v":2,"type":"ping","id":"x","ts":"2026-09-05T19:30:00Z","from":"tok_1","body":{}}"#;
    let err = decode(raw).expect_err("a v2 frame must be rejected");
    assert_eq!(err, DecodeError::UnsupportedVersion(2));
}

#[test]
fn decode_rejects_unknown_type_and_helper_builds_error() {
    let raw = r#"{"v":1,"type":"nonsense","id":"req-1","ts":"2026-09-05T19:30:00Z","from":"tok_1","body":{}}"#;
    let err = decode(raw).expect_err("an unknown type must be rejected");
    assert_eq!(err, DecodeError::UnknownType);

    // The codec must be able to answer an unknown type with a well-
    // formed error envelope (spec §3: "Do not ignore it as success.").
    let reply = error_for_unknown_type("nonsense", "req-1", "server");
    assert_eq!(reply.v, 1);
    assert_eq!(reply.msg_type, MessageType::Error);
    assert_eq!(reply.id, "req-1");
    assert_eq!(reply.from, "server");
    match &reply.body {
        Body::Error(body) => {
            assert_eq!(body.code, "unknown_type");
            assert_eq!(body.cmd, None);
            assert_eq!(body.message.as_deref(), Some("unknown type: nonsense"));
        }
        other => panic!("expected an error body, got {other:?}"),
    }
    // And the reply itself must re-encode cleanly.
    let _ = encode(&reply).expect("an error envelope must encode");
}

#[test]
fn decode_rejects_malformed_frames() {
    // Not JSON at all — must be a Malformed, with a message (serde's
    // exact wording is not part of the wire contract, so we only check
    // the variant).
    let err = decode("not json").unwrap_err();
    assert!(
        matches!(err, DecodeError::Malformed(_)),
        "expected Malformed, got {err:?}"
    );

    // JSON, but missing the required `from` — also Malformed.
    let raw = r#"{"v":1,"type":"ping","id":"x","body":{}}"#;
    assert!(matches!(decode(raw), Err(DecodeError::Malformed(_))));

    // JSON with a `type` that is not one of the 12 — UnknownType,
    // not Malformed.
    let raw = r#"{"v":1,"type":"bananas","id":"x","from":"t","body":{}}"#;
    assert!(
        matches!(decode(raw), Err(DecodeError::UnknownType)),
        "an unknown `type` must decode as UnknownType, not Malformed"
    );
}

// --------------------------------------------------------------------------
// The query_ok flat union — the spec's exact examples
// --------------------------------------------------------------------------

#[test]
fn query_ok_protocol_examples_deserialize() {
    // Spec §7 `protocol`, no args — report range. The spec's example
    // omits `ts`; an omitted `ts` must decode (it is opaque), not fail.
    let raw = r#"{
        "v": 1,
        "type": "query_ok",
        "id": "01JQUERYPROTOCOL000000000",
        "from": "tok_7f3a",
        "ts": "2026-09-05T19:30:00Z",
        "body": {
            "cmd": "protocol",
            "session": 1,
            "min": 1,
            "max": 1
        }
    }"#;
    let env = decode(raw).expect("spec protocol example must decode");
    let Body::QueryOk(body) = &env.body else {
        panic!("expected query_ok body");
    };
    assert_eq!(body.cmd, "protocol");
    // Everything the spec does not close lands in `rest`.
    assert_eq!(body.rest["session"], json!(1));
    assert_eq!(body.rest["min"], json!(1));
    assert_eq!(body.rest["max"], json!(1));
    assert_eq!(body.rest["args"], json!(Value::Null));
    assert_eq!(body.rest["asked"], json!(Value::Null));
    assert_eq!(body.rest["ok"], json!(Value::Null));
}

#[test]
fn query_ok_protocol_with_arg_deserializes() {
    // Spec §7 `protocol` with args: ["2"] — "can you handle this version?".
    let raw = r#"{
        "v": 1,
        "type": "query_ok",
        "id": "01JQUERYPROTOCOL000000000",
        "from": "tok_7f3a",
        "ts": "2026-09-05T19:30:00Z",
        "body": {
            "cmd": "protocol",
            "args": ["2"],
            "ok": false,
            "asked": 2,
            "session": 1,
            "min": 1,
            "max": 1
        }
    }"#;
    let env = decode(raw).expect("spec protocol-with-arg example must decode");
    let Body::QueryOk(body) = &env.body else {
        panic!("expected query_ok body");
    };
    assert_eq!(body.rest["ok"], json!(false));
    assert_eq!(body.rest["asked"], json!(2));
    assert_eq!(body.rest["args"], json!(["2"]));
}

#[test]
fn query_ok_support_examples_deserialize() {
    // Spec §7 `support`, ok: true (client answering for a harness it has).
    let raw_ok = r#"{
        "v": 1,
        "type": "query_ok",
        "id": "01JQUERYSUPPORT0000000000",
        "from": "tok_7f3a",
        "ts": "2026-09-05T19:30:00Z",
        "body": {
            "cmd": "support",
            "args": ["opencode"],
            "ok": true,
            "feature": "opencode",
            "kind": "harness",
            "how": "opencode acp"
        }
    }"#;
    let env = decode(raw_ok).expect("spec support ok example must decode");
    let Body::QueryOk(body) = &env.body else {
        panic!("expected query_ok body");
    };
    assert_eq!(body.rest["ok"], json!(true));
    assert_eq!(body.rest["feature"], json!("opencode"));
    assert_eq!(body.rest["kind"], json!("harness"));
    assert_eq!(body.rest["how"], json!("opencode acp"));
    assert_eq!(body.rest["reason"], json!(Value::Null));
    // The flat union must keep `args` alongside the other fields.
    assert_eq!(body.rest["args"], json!(["opencode"]));

    // Spec §7 `support`, ok: false (with a reason).
    let raw_no = r#"{
        "v": 1,
        "type": "query_ok",
        "id": "01JQUERYSUPPORT0000000001",
        "from": "tok_7f3a",
        "ts": "2026-09-05T19:30:00Z",
        "body": {
            "cmd": "support",
            "args": ["claude"],
            "ok": false,
            "feature": "claude",
            "kind": "harness",
            "reason": "no adapter"
        }
    }"#;
    let env = decode(raw_no).expect("spec support no example must decode");
    let Body::QueryOk(body) = &env.body else {
        panic!("expected query_ok body");
    };
    assert_eq!(body.rest["ok"], json!(false));
    assert_eq!(body.rest["reason"], json!("no adapter"));
    assert_eq!(body.rest["how"], json!(Value::Null));
}

// --------------------------------------------------------------------------
// Error helpers
// --------------------------------------------------------------------------

#[test]
fn error_for_builds_minimal_error_envelope() {
    let reply = error_for("unsupported_version", "req-9", "server");
    assert_eq!(reply.from, "server");
    match &reply.body {
        Body::Error(body) => {
            assert_eq!(body.code, "unsupported_version");
            assert_eq!(body.cmd, None);
            assert_eq!(body.message, None);
        }
        other => panic!("expected an error body, got {other:?}"),
    }
}

// --------------------------------------------------------------------------
// Round-trip through raw JSON (encode → decode) for the spec's hello
// --------------------------------------------------------------------------

#[test]
fn encoded_hello_omits_none_fields() {
    // A server hello omits token_id / client_id / sessions — those
    // fields must not appear in the wire JSON (skip_serializing_if).
    let server_hello = make_envelope(
        MessageType::Hello,
        "01JHELLOSERVER00000000000",
        "server",
        Body::Hello(HelloBody {
            protocol: 1,
            protocol_min: 1,
            protocol_max: 1,
            role: Role::Server,
            hostname: "uranus".into(),
            token_id: None,
            client_id: None,
            harnesses: vec![],
            harnesses_known: vec!["opencode".into()],
            harnesses_confirmed: vec!["opencode".into()],
            features: vec![
                "interrupt".into(),
                "presence".into(),
                "ping".into(),
                "query".into(),
                "roster".into(),
                "token".into(),
            ],
            sessions: vec![],
        }),
    );

    let encoded = encode(&server_hello).expect("encode server hello");
    assert!(!encoded.contains("token_id"), "server hello must not carry token_id: {encoded}");
    assert!(!encoded.contains("client_id"), "server hello must not carry client_id: {encoded}");
    assert!(!encoded.contains("sessions"), "server hello must not carry sessions: {encoded}");

    // And decoding that wire JSON still yields the same in-memory shape
    // (absent fields default to None / empty).
    let decoded = decode(&encoded).expect("decode server hello");
    assert_eq!(decoded, server_hello);
    let _value: Value = serde_json::from_str(&encoded).expect("valid JSON");
}

#[test]
fn decode_of_wrong_shape_body_fails_cleanly() {
    // `type: "ping"` with `body: {}` is fine; but a body that is the
    // wrong shape (e.g. a string where an object is expected) is a
    // malformed frame, not a panic.
    let raw = r#"{"v":1,"type":"ping","id":"x","from":"t","body":"hi"}"#;
    assert!(matches!(decode(raw), Err(DecodeError::Malformed(_))));
}

#[test]
fn decode_rejects_missing_ts() {
    // Spec §3 requires `ts` on the envelope. A frame without it is
    // malformed (fail-closed), even though the spec's own §7/§11
    // examples omit it — the strict reading of §3 wins.
    let raw = r#"{"v":1,"type":"ping","id":"x","from":"t","body":{}}"#;
    assert!(
        matches!(decode(raw), Err(DecodeError::Malformed(_))),
        "a frame missing the required `ts` must be Malformed, got {raw:?}"
    );
}
