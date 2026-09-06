//! Query dispatch (issue #37): answer `query status` / `caps` /
//! `support` / `protocol` the same way whether asked
//!
//! - locally, with no live server at all (`holler-server status` when nothing
//!   is running — zero clients, no confirmed harnesses),
//! - locally, via the control channel, against a live `holler-server serve`
//!   process's real registry state, or
//! - over the wire, by a live peer asking this server the same
//!   questions it can ask a peer (`docs/protocol/v1.md` §7: "The server
//!   answers as a peer").
//!
//! One function, [`dispatch`], used by `connection::handle_frame` (a
//! connected client asking the server) and by `control` (an operator
//! CLI invocation asking the server, either about itself or, via
//! `Registry::query`, about a specific connected client).
//!
//! ADR 0001's "known vs. confirmed": a harness is only ever `confirmed`
//! once a **live** client has answered `support` with `ok: true`
//! ([`Registry::record_harness_confirmed`](super::registry::Registry::record_harness_confirmed),
//! called from `control` after a successful remote `support` round
//! trip). Hello's advertised `harnesses` is a hint, never confirmation.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::proto::{ErrorBody, QueryOkBody, CODE_UNKNOWN_CMD, CODE_UNKNOWN_FEATURE};

use super::hello::{HARNESSES_KNOWN, PROTOCOL_FEATURES, PROTOCOL_VERSION, SERVER_FEATURES};

/// One row of a status/caps document's `harnesses_confirmed` (spec §7):
/// a harness id and the live client hostnames that confirmed it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct HarnessConfirmation {
    pub id: String,
    pub clients: Vec<String>,
}

fn error_body(code: &str, cmd: Option<&str>, message: impl Into<String>) -> ErrorBody {
    ErrorBody {
        code: code.to_string(),
        cmd: cmd.map(|s| s.to_string()),
        message: Some(message.into()),
    }
}

fn confirmed_hosts<'a>(confirmed: &'a [HarnessConfirmation], id: &str) -> Option<&'a [String]> {
    confirmed
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.clients.as_slice())
}

fn status_rest(
    hostname: &str,
    listening: &[String],
    clients: usize,
    sessions: usize,
    confirmed: &[HarnessConfirmation],
) -> Value {
    json!({
        "role": "server",
        "protocol": PROTOCOL_VERSION,
        "protocol_min": PROTOCOL_VERSION,
        "protocol_max": PROTOCOL_VERSION,
        "hostname": hostname,
        "listening": listening,
        "features": SERVER_FEATURES,
        "harnesses_known": HARNESSES_KNOWN,
        "harnesses_confirmed": confirmed,
        "clients": clients,
        "sessions": sessions,
    })
}

/// `query status` (spec §7 "status document"): who is this process, is
/// it up, which harnesses are **confirmed** right now. `sessions` is a
/// live count (issue #81) — rows the roster does not yet consider
/// [`super::roster::RosterState::Gone`], the same "known vs. confirmed"
/// discipline as `harnesses_confirmed` — never a static config number.
pub fn status_body(
    hostname: &str,
    listening: &[String],
    clients: usize,
    sessions: usize,
    confirmed: &[HarnessConfirmation],
) -> QueryOkBody {
    QueryOkBody {
        cmd: "status".to_string(),
        rest: status_rest(hostname, listening, clients, sessions, confirmed),
    }
}

/// `query caps` (spec §7): `status` plus an explicit map of every known
/// protocol feature and harness id to `{ok, kind, reason?}`.
pub fn caps_body(
    hostname: &str,
    listening: &[String],
    clients: usize,
    sessions: usize,
    confirmed: &[HarnessConfirmation],
) -> QueryOkBody {
    let mut rest = status_rest(hostname, listening, clients, sessions, confirmed);

    let mut capabilities = serde_json::Map::new();
    for feature in PROTOCOL_FEATURES {
        let ok = SERVER_FEATURES.contains(feature);
        let mut entry = json!({ "ok": ok, "kind": "feature" });
        if !ok {
            entry["reason"] = json!("not implemented by this server process");
        }
        capabilities.insert((*feature).to_string(), entry);
    }
    for harness in HARNESSES_KNOWN {
        let hosts = confirmed_hosts(confirmed, harness).unwrap_or(&[]);
        let ok = !hosts.is_empty();
        let mut entry = json!({ "ok": ok, "kind": "harness" });
        if ok {
            entry["clients"] = json!(hosts);
        } else {
            entry["reason"] = json!("not confirmed by any live client");
        }
        capabilities.insert((*harness).to_string(), entry);
    }
    rest["capabilities"] = Value::Object(capabilities);

    QueryOkBody {
        cmd: "caps".to_string(),
        rest,
    }
}

/// `query support [feature]` (spec §7): boolean — do you (this server
/// process) support this protocol feature or harness right now.
///
/// A protocol feature is `ok` iff this process implements it
/// ([`SERVER_FEATURES`]). A harness is `ok` iff at least one live client
/// has confirmed it (never from hello's advertisement alone) — see the
/// module doc's ADR 0001 note. `holler-server` never runs a harness
/// itself, so a harness answer here is always about what a *connected
/// client* can host, not this process.
pub fn support_body(
    args: &[String],
    confirmed: &[HarnessConfirmation],
) -> Result<QueryOkBody, ErrorBody> {
    let Some(feature) = args.first() else {
        return Err(error_body(
            CODE_UNKNOWN_FEATURE,
            Some("support"),
            "support requires a feature or harness id",
        ));
    };
    let feature = feature.as_str();

    if SERVER_FEATURES.contains(&feature) {
        return Ok(QueryOkBody {
            cmd: "support".to_string(),
            rest: json!({
                "args": [feature], "ok": true, "feature": feature, "kind": "feature",
                "how": "implemented by this server process",
            }),
        });
    }
    if PROTOCOL_FEATURES.contains(&feature) {
        return Ok(QueryOkBody {
            cmd: "support".to_string(),
            rest: json!({
                "args": [feature], "ok": false, "feature": feature, "kind": "feature",
                "reason": "not implemented by this server process",
            }),
        });
    }
    if HARNESSES_KNOWN.contains(&feature) {
        let hosts = confirmed_hosts(confirmed, feature).unwrap_or(&[]);
        let ok = !hosts.is_empty();
        let mut rest = json!({ "args": [feature], "ok": ok, "feature": feature, "kind": "harness" });
        if ok {
            rest["how"] = json!("confirmed by a live client");
            rest["clients"] = json!(hosts);
        } else {
            rest["reason"] = json!("not confirmed by any live client");
        }
        return Ok(QueryOkBody {
            cmd: "support".to_string(),
            rest,
        });
    }

    Err(error_body(
        CODE_UNKNOWN_FEATURE,
        Some("support"),
        format!("unknown feature or harness id: {feature}"),
    ))
}

/// `query protocol [version]` (spec §7): highest protocol version this
/// binary can handle, or (with an argument) whether it can speak a
/// specific version. Every process in this repo has `min == max == 1`
/// today, so `session` (the connection's actual `v`) is always `1` too
/// — a local, connection-less invocation reports the same thing, since
/// there is no other version to report.
pub fn protocol_body(args: &[String]) -> Result<QueryOkBody, ErrorBody> {
    let Some(raw) = args.first() else {
        return Ok(QueryOkBody {
            cmd: "protocol".to_string(),
            rest: json!({
                "session": PROTOCOL_VERSION, "min": PROTOCOL_VERSION, "max": PROTOCOL_VERSION,
            }),
        });
    };

    let asked = match raw.parse::<u32>() {
        Ok(n) if n >= 1 => n,
        _ => {
            return Err(error_body(
                CODE_UNKNOWN_FEATURE,
                Some("protocol"),
                format!("protocol version must be a positive integer, got {raw:?}"),
            ))
        }
    };
    // Spelled as a range check (not `==`) because the spec defines `ok`
    // as `asked >= min && asked <= max`; `min == max == 1` today makes
    // it degenerate, but a later `max > 1` should not need this line
    // rewritten.
    #[allow(clippy::double_comparisons)]
    let ok = asked >= PROTOCOL_VERSION && asked <= PROTOCOL_VERSION;
    Ok(QueryOkBody {
        cmd: "protocol".to_string(),
        rest: json!({
            "args": [raw], "ok": ok, "asked": asked,
            "session": PROTOCOL_VERSION, "min": PROTOCOL_VERSION, "max": PROTOCOL_VERSION,
        }),
    })
}

/// Dispatch one `query` `cmd` (spec §7's four commands). Unknown `cmd`
/// fails closed with `unknown_cmd` (ADR 0009) — never invents an answer.
pub fn dispatch(
    cmd: &str,
    args: &[String],
    hostname: &str,
    listening: &[String],
    clients: usize,
    sessions: usize,
    confirmed: &[HarnessConfirmation],
) -> Result<QueryOkBody, ErrorBody> {
    match cmd {
        "status" => Ok(status_body(hostname, listening, clients, sessions, confirmed)),
        "caps" => Ok(caps_body(hostname, listening, clients, sessions, confirmed)),
        "support" => support_body(args, confirmed),
        "protocol" => protocol_body(args),
        other => Err(error_body(
            CODE_UNKNOWN_CMD,
            Some(other),
            format!("unknown query cmd: {other}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confirmed(id: &str, hosts: &[&str]) -> Vec<HarnessConfirmation> {
        vec![HarnessConfirmation {
            id: id.to_string(),
            clients: hosts.iter().map(|s| s.to_string()).collect(),
        }]
    }

    #[test]
    fn status_reports_role_and_confirmed_harnesses() {
        let body = status_body(
            "uranus",
            &["ws://x".to_string()],
            2,
            0,
            &confirmed("opencode", &["kiwi"]),
        );
        assert_eq!(body.cmd, "status");
        assert_eq!(body.rest["role"], "server");
        assert_eq!(body.rest["clients"], 2);
        assert_eq!(body.rest["harnesses_confirmed"][0]["id"], "opencode");
        assert_eq!(body.rest["harnesses_confirmed"][0]["clients"][0], "kiwi");
    }

    #[test]
    fn status_reports_a_real_session_count_not_the_hardcoded_zero() {
        let body = status_body("uranus", &[], 2, 3, &[]);
        assert_eq!(body.rest["sessions"], 3);
    }

    #[test]
    fn caps_maps_every_known_feature_and_harness() {
        let body = caps_body("uranus", &[], 0, 0, &[]);
        let caps = body.rest["capabilities"].as_object().unwrap();
        assert_eq!(caps.len(), PROTOCOL_FEATURES.len() + HARNESSES_KNOWN.len());
        assert_eq!(caps["query"]["ok"], true);
        assert_eq!(caps["query"]["kind"], "feature");
        assert_eq!(caps["interrupt"]["ok"], true);
        assert_eq!(caps["opencode"]["ok"], false);
        assert_eq!(caps["opencode"]["kind"], "harness");
    }

    #[test]
    fn caps_reflects_a_confirmed_harness() {
        let body = caps_body("uranus", &[], 1, 1, &confirmed("opencode", &["kiwi"]));
        let caps = body.rest["capabilities"].as_object().unwrap();
        assert_eq!(caps["opencode"]["ok"], true);
        assert_eq!(caps["opencode"]["clients"][0], "kiwi");
    }

    #[test]
    fn caps_reports_a_real_session_count_not_the_hardcoded_zero() {
        let body = caps_body("uranus", &[], 0, 5, &[]);
        assert_eq!(body.rest["sessions"], 5);
    }

    #[test]
    fn support_is_true_for_an_implemented_feature() {
        let body = support_body(&["query".to_string()], &[]).unwrap();
        assert_eq!(body.rest["ok"], true);
        assert_eq!(body.rest["kind"], "feature");
    }

    #[test]
    fn support_is_false_for_a_vocabulary_only_feature() {
        // `wait` is in the v1 vocabulary but no story has wired it up yet
        // (see `hello::SERVER_FEATURES`'s doc comment) — unlike
        // `interrupt`, which issue #34 moved into `SERVER_FEATURES`.
        let body = support_body(&["wait".to_string()], &[]).unwrap();
        assert_eq!(body.rest["ok"], false);
        assert_eq!(body.rest["kind"], "feature");
    }

    #[test]
    fn support_is_true_for_interrupt_now_that_issue_34_implements_it() {
        let body = support_body(&["interrupt".to_string()], &[]).unwrap();
        assert_eq!(body.rest["ok"], true);
        assert_eq!(body.rest["kind"], "feature");
    }

    #[test]
    fn support_is_false_for_an_unconfirmed_harness() {
        let body = support_body(&["opencode".to_string()], &[]).unwrap();
        assert_eq!(body.rest["ok"], false);
        assert_eq!(body.rest["kind"], "harness");
    }

    #[test]
    fn support_is_true_for_a_confirmed_harness() {
        let body =
            support_body(&["opencode".to_string()], &confirmed("opencode", &["kiwi"])).unwrap();
        assert_eq!(body.rest["ok"], true);
        assert_eq!(body.rest["clients"][0], "kiwi");
    }

    #[test]
    fn support_with_no_args_is_unknown_feature() {
        let err = support_body(&[], &[]).unwrap_err();
        assert_eq!(err.code, CODE_UNKNOWN_FEATURE);
    }

    #[test]
    fn support_with_unknown_id_is_unknown_feature() {
        let err = support_body(&["not-a-real-thing".to_string()], &[]).unwrap_err();
        assert_eq!(err.code, CODE_UNKNOWN_FEATURE);
    }

    #[test]
    fn protocol_with_no_args_reports_the_range() {
        let body = protocol_body(&[]).unwrap();
        assert_eq!(body.rest["min"], 1);
        assert_eq!(body.rest["max"], 1);
        assert_eq!(body.rest["session"], 1);
        assert!(body.rest.get("ok").is_none());
    }

    #[test]
    fn protocol_asking_the_current_version_is_ok() {
        let body = protocol_body(&["1".to_string()]).unwrap();
        assert_eq!(body.rest["ok"], true);
        assert_eq!(body.rest["asked"], 1);
    }

    #[test]
    fn protocol_asking_a_future_version_is_not_ok() {
        let body = protocol_body(&["2".to_string()]).unwrap();
        assert_eq!(body.rest["ok"], false);
        assert_eq!(body.rest["asked"], 2);
    }

    #[test]
    fn protocol_with_a_non_integer_arg_is_unknown_feature() {
        let err = protocol_body(&["abc".to_string()]).unwrap_err();
        assert_eq!(err.code, CODE_UNKNOWN_FEATURE);
    }

    #[test]
    fn protocol_with_zero_is_unknown_feature() {
        let err = protocol_body(&["0".to_string()]).unwrap_err();
        assert_eq!(err.code, CODE_UNKNOWN_FEATURE);
    }

    #[test]
    fn dispatch_routes_known_cmds() {
        assert!(dispatch("status", &[], "uranus", &[], 0, 0, &[]).is_ok());
        assert!(dispatch("caps", &[], "uranus", &[], 0, 0, &[]).is_ok());
        assert!(dispatch("protocol", &[], "uranus", &[], 0, 0, &[]).is_ok());
        assert!(dispatch("support", &["query".to_string()], "uranus", &[], 0, 0, &[]).is_ok());
    }

    #[test]
    fn dispatch_threads_the_session_count_through_to_status() {
        let body = dispatch("status", &[], "uranus", &[], 0, 4, &[]).unwrap();
        assert_eq!(body.rest["sessions"], 4);
    }

    #[test]
    fn dispatch_fails_closed_on_unknown_cmd() {
        let err = dispatch("summarize", &[], "uranus", &[], 0, 0, &[]).unwrap_err();
        assert_eq!(err.code, CODE_UNKNOWN_CMD);
        assert_eq!(err.cmd.as_deref(), Some("summarize"));
    }
}
