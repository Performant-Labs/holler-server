//! ACP stub: a tiny fake coding agent that speaks ACP v1 (JSON-RPC 2.0,
//! newline-delimited JSON) over stdio. Used by the wire harness
//! (`tests/wire/first_talk.rs`, and later the `holler-client`) as the
//! `stub-acp` row of the harness table in `docs/testing.md`.
//!
//! This is a **fake**, not OpenCode. Its only job is to be a process the
//! harness can spawn, drive, and observe exiting. The contract the harness
//! (and ACP, ADR 0012) relies on:
//!   - `session/prompt`  -> one canned `session/update` notification, keep running
//!   - `session/cancel`  -> a `turn/end` response (the turn is done), keep running
//!   - any other method  -> a JSON-RPC error response, keep running (no crash)
//!   - EOF on stdin      -> the host closed the pipe, so the turn ends:
//!     - emit a terminal `turn/end`, then EXIT 0 ("turn ended" == process exited)
//!
//! ACP v1 is JSON-RPC 2.0 (ADR 0012). Notifications (no `id`) get back a
//! JSON-RPC *response* here (not a notification) so the harness can match
//! the reply to the request it sent.
//!
//! Why EOF is the authoritative "turn ended" signal: a blocking stdin read
//! cannot distinguish "a cancel arrived" from "the pipe just closed". The
//! harness writes `session/cancel` and then CLOSES stdin, so the stub's next
//! read returns EOF *before* the buffered cancel is ever read. The stub
//! therefore keeps running on cancel (writing a `turn/end` marker) and treats
//! the subsequent EOF as the real turn-end: it emits a terminal `turn/end`
//! and exits 0. Only a genuine read *error* (torn pipe / EIO — distinct from
//! EOF) exits 1.
//!
//! Spawns: no shell. `std::io` blocking loop, no tokio. Cross-OS.

use std::env;
use std::io::{self, BufRead, Write};
use std::process;

use serde_json::{json, Value};

/// Default advertised sessions (docs/testing.md line 30: "sessions
/// alpha, beta"). Overridable via `--sessions <csv>`.
const DEFAULT_SESSIONS: &str = "alpha,beta";

/// JSON-RPC 2.0 "method not found" error code (jsonrpc.org spec, §errors).
const METHOD_NOT_FOUND: i64 = -32601;

fn main() {
    let sessions = parse_sessions(&env::args().collect::<Vec<_>>());

    // Drive the turn: read and service JSON-RPC messages on stdin until the
    // host closes the pipe (EOF). A genuine read *error* exits 1 from inside
    // `run_turn`; a clean EOF just returns.
    run_turn(&sessions);

    // The turn has ended cleanly (the host closed the pipe). Emit the
    // terminal ACP marker so a harness that reads to the end observes the
    // turn boundary, then exit 0: "turn ends" is observable as the process
    // exiting. (On cancel the stub keeps running; the EOF that follows the
    // harness's `drop(stdin)` is what actually ends the turn here.)
    emit(&json!({
        "jsonrpc": "2.0",
        "method": "turn/end",
        "params": { "sessionId": "stub", "reason": "input closed" },
    }));
}

/// Read and service JSON-RPC messages on stdin until the host closes the
/// pipe (EOF). Returns on EOF; exits 1 on a genuine (non-EOF) read error.
/// The `lines()` reader is owned here so a clean EOF unwinds it without
/// poisoning the (then-exiting) process.
fn run_turn(sessions: &[String]) {
    for line in io::stdin().lock().lines() {
        // A *line* is `Ok` when the peer sent one and `Err(UnexpectedEof)`
        // when the peer closed the pipe (EOF) without a trailing newline.
        // EOF is the normal end of a turn (the host closed the pipe) — a
        // CLEAN return, not a failure. Only a genuine read *error* (a torn
        // pipe, EIO — distinct from EOF) is a failure: surface it as a
        // non-zero exit so the harness can tell "the stub broke" from
        // "the turn ended".
        let line = match line {
            Ok(l) => l,
            Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => return,
            Err(e) => {
                eprintln!("stub-acp: stdin read error: {e}");
                process::exit(1);
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            // Malformed input: a JSON-RPC "parse error", then keep running
            // (per ACP: do not crash the agent on a bad frame).
            Err(e) => {
                emit(&json!({
                    "jsonrpc": "2.0",
                    "error": {
                        "code": -32700,
                        "message": format!("parse error: {e}"),
                    },
                }));
                continue;
            }
        };

        let method = msg
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let id = msg.get("id").cloned();

        match method {
            "session/prompt" => {
                // Canned agent output. ACP streams `session/update`
                // notifications during a turn; the stub emits one, then
                // stays up so the harness can send `session/cancel`.
                emit(&json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "stub",
                        "sessions": sessions,
                        "kind": "agent_message_chunk",
                        "text": "stub: turn running",
                    },
                    "id": id,
                }));
            }
            "session/cancel" => {
                // The turn ends on cancel: `main` will emit the terminal
                // `turn/end` marker and exit 0 as soon as `run_turn` returns
                // (here). We return (not `process::exit`) so `main` runs and
                // the `turn/end` is written to stdout before we leave.
                return;
            }
            // `session/new` (and anything else the harness may send) is a
            // no-op: ack it and keep running. The stub always advertises
            // the same sessions, so it needs no real session state.
            "session/new" => {
                if let Some(id) = id {
                    emit(&json!({
                        "jsonrpc": "2.0",
                        "result": { "sessionId": "stub", "sessions": sessions },
                        "id": id,
                    }));
                }
            }
            // Unknown / unsupported method: JSON-RPC "method not found",
            // but do NOT crash — a real agent keeps serving other methods.
            _ => {
                emit(&json!({
                    "jsonrpc": "2.0",
                    "error": {
                        "code": METHOD_NOT_FOUND,
                        "message": format!("method not found: {method}"),
                    },
                    "id": id,
                }));
            }
        }
    }
    // Loop exited on EOF (host closed the pipe): the turn ended cleanly.
}

/// Parse `--sessions <csv>` out of argv (default `alpha,beta`). No
/// required args; unknown args are ignored (this is a fake).
fn parse_sessions(argv: &[String]) -> Vec<String> {
    let mut it = argv.iter();
    let mut csv = DEFAULT_SESSIONS.to_string();
    while let Some(a) = it.next() {
        if a == "--sessions" {
            if let Some(v) = it.next() {
                csv = v.clone();
            }
        }
    }
    csv.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Write one JSON object (newline-terminated) to stdout and flush so the
/// peer's blocking read unblocks immediately.
///
/// A write that fails because the peer CLOSED its end of the pipe (EPIPE /
/// "Broken pipe") is a NORMAL, clean turn-end: the host (harness) has moved
/// on and there is nobody left to read from. We therefore exit 0 for that
/// case, NOT 1 — a non-zero exit would make the harness think the stub broke
/// when the turn had simply ended. Only a write failure that is NOT a peer-
/// close (a real I/O fault) exits 1.
fn emit(v: &Value) {
    let mut s = match serde_json::to_string(v) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("stub-acp: encode error: {e}");
            process::exit(1);
        }
    };
    s.push('\n');
    let mut out = io::stdout();
    if let Err(e) = out.write_all(s.as_bytes()).and_then(|_| out.flush()) {
        // The peer closed stdout (EOF on the write end): the turn is over.
        // EPIPE is the platform-agnostic signature of "peer went away".
        if e.kind() == io::ErrorKind::BrokenPipe {
            eprintln!("stub-acp: peer closed stdout; turn ended cleanly");
            process::exit(0);
        }
        eprintln!("stub-acp: stdout write error: {e}");
        process::exit(1);
    }
}
