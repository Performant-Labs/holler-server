//! OS-portable process harness (issue #40, phase 1: "Design + stub +
//! runner scaffold").
//!
//! This is the machinery the end-to-end runner (issue #31: spawn the real
//! `holler-server` + real `holler-client`, mint a token, drive `holler
//! status`) will slot into. Today it is a **scaffold**: each `#[test]`
//! below proves one piece of the harness machinery works, so the scaffold is
//! itself a regression test — and #31 fills in the real wiring (left as the
//! clearly-marked non-test helper at the bottom).
//!
//! Ground rules (docs/testing.md, and the canary `tests/wire/selftest.rs`):
//!   - Rust. Spawn via `std::process::Command` (NOT tokio, NOT a shell: no
//!     `bash`, `sh -c`, `cmd /c` — the stub is spawned DIRECTLY with its args
//!     piped in; the child's own program/args never go through a shell).
//!   - IPv4 `127.0.0.1` only for the required path; `127.0.0.1`, never
//!     `localhost` (on Windows `localhost` often resolves to `::1` first, but
//!     the required listen is IPv4).
//!   - IPv6 `[::1]` is an OPTIONAL extra case that SKIPs (does not fail) when
//!     `::1` is unavailable — a graceful skip with a reason, never the
//!     required path.
//!   - Kill the child WITHOUT relying on SIGTERM alone (the portable "kill the
//!     whole thing" idiom, OS-branched in `common::kill_tree`).
//!   - A silent skip is a fail: a REQUIRED case must either pass or panic.
//!     An OPTIONAL case may skip, but must `eprintln!` why.
//!   - The token (a secret) never appears in any log / panic / assert message.
//!     This scaffold holds no token yet; #31 must keep it that way.
//!   - Must pass on Linux, macOS, and Windows.
//!
//! Stub-spawn mechanism: `CARGO_BIN_EXE_stub_acp` is NOT set by cargo for a
//! `[[bin]]` whose path is under `tests/` (cargo only emits it for
//! `src/`-rooted bin targets; verified empirically). So the stub is located
//! via `common::stub_acp_path` — `CARGO_MANIFEST_DIR` +
//! `target/{debug,release}/stub-acp` (+ `.exe` on Windows).
//!
//! Kill-tree idiom: on Unix the child is started in its own process group
//! (`setpgid`) and killed with `killpg(-pid, SIGKILL)` (SIGKILL to the whole
//! group). macOS may refuse that `setpgid` (EACCES, when the parent is not a
//! process-group leader — common on CI), in which case `kill_tree` falls back
//! to a direct `SIGKILL` of the pid. Both arms are SIGKILL (uncatchable), so
//! the harness never "relies on SIGTERM alone". On Windows the child is
//! started with `CREATE_NEW_PROCESS_GROUP` (a `std`-native flag) and killed
//! with `taskkill /F /T` (force, whole process tree). Both OS-branched.

#![allow(unreachable_code)] // `#[cfg]`-branded kill arms leave a platform-dead path documenting the other OS's idiom

mod common;

use std::io::{self, BufRead, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use common::{free_port_is_available, kill_tree, spawn_bin, stub_acp_path, BUDGET, ProcessGroup};

/// `spawn_stub_acp_and_drive_prompt` — the core harness primitive: spawn the
/// ACP stub DIRECTLY (no shell), write a `session/prompt` JSON-RPC line to its
/// stdin, read the canned `session/update` back off its stdout and assert it is
/// parseable JSON with the expected ACP shape; then send `session/cancel` and
/// assert the process EXITS 0 — proving "turn ends" (process exited) is
/// observable cross-OS. REQUIRED: must pass on all 3.
#[test]
fn spawn_stub_acp_and_drive_prompt() {
    let path = stub_acp_path();

    // Spawn the stub directly: piped stdin (we drive it) + piped stdout (we
    // read the canned replies). No shell wraps this — `Command::new(path)`
    // with the program path + its args is the portable idiom.
    let mut child = spawn_bin(&path, &[], ProcessGroup::Same);
    let mut stdin = child.stdin.take().expect("stub-acp stdin was piped at spawn");
    let stdout = child.stdout.take().expect("stub-acp stdout was piped at spawn");
    let mut stderr = child.stderr.take().expect("stub-acp stderr was piped at spawn");

    // Drain the stub's stdout on a thread for the WHOLE exchange. We read
    // EVERY line until the stub closes stdout (EOF): the first is the canned
    // `session/update`, the last is the terminal `turn/end`.
    //
    // WHY a dedicated draining thread (and why it stays alive for the whole
    // exchange): the harness writes to the stub's stdin on the main thread.
    // If it also blocked on reading stdout here, it could dead-lock on a full
    // pipe (write blocks, read is busy — or vice versa). The thread removes
    // that coupling. It ALSO must keep the stub's stdout open until the stub
    // exits: if this reader closed the stub's stdout after one line, the
    // stub's terminal `turn/end` write would race against that close (an EPIPE
    // race) — keeping the drain alive until EOF makes "turn ends == the
    // process exits 0" deterministic instead of a pipe race.
    //
    // The first reply is handed back on a channel; the main thread's
    // `recv_timeout` is the fail-fast guard (a stub that never answers
    // panics within the budget, per the canary's rule). A panic inside this
    // reader thread would abort the whole test with an unhelpful message, so
    // we never `expect`/`panic!` here — a missing line just means no send,
    // which the main thread turns into a clear timeout panic.
    let (reply_tx, reply_rx) = mpsc::channel::<Vec<String>>();
    {
        let stdout = stdout;
        std::thread::spawn(move || {
            let mut lines = std::io::BufReader::new(stdout).lines();
            let mut all: Vec<String> = Vec::new();
            // First reply (the `session/update`): hand it back, then keep
            // draining the rest (the terminal `turn/end`) so stdout stays
            // open for the stub until it actually exits.
            // `lines()` yields `Result<String, _>`; `.flatten()` keeps only
            // the successfully-read lines (a mid-stream read error on a fake
            // is a benign end-of-stream, so we just stop).
            for s in lines.by_ref().flatten() {
                all.push(s);
                // Once the FIRST non-empty reply (the `session/update`) has
                // arrived, hand a CLONE to the main thread — then keep
                // draining the rest (the terminal `turn/end`) so the stub's
                // stdout stays open until it actually exits. A clone (not a
                // move) so this loop can keep filling `all`; the check is
                // AFTER the push, so the first real line is in `all` when we
                // fire (a clone at len==1 is just that line).
                if all.len() == 1 && !all[0].is_empty() {
                    let _ = reply_tx.send(all.clone());
                }
            }
            // Stub closed stdout (it exited): drop the channel so the main
            // thread's recv (if still pending) returns Err, not hang.
        });
    }

    // (1) Drive a prompt: one JSON-RPC request line, newline-terminated.
    let prompt = jsonrpc_request("req-prompt", "session/prompt", &json!({ "sessionId": "stub", "text": "hello" }));
    write_line(&mut stdin, &prompt).expect("harness: could not write `session/prompt` to stub-acp stdin");

    // (2) Read the canned reply (the FIRST line the stub sent) and assert
    //     its ACP shape. The reader thread sends a `Vec` of every stdout line
    //     it saw; the first element is the `session/update` reply.
    let reply_lines = match reply_rx.recv_timeout(BUDGET) {
        Ok(l) => l,
        Err(e) => panic!("harness: no `session/update` reply within {BUDGET:?}: {e} — the stub did not answer a prompt (runner swallowed it?)"),
    };
    let reply_line = match reply_lines.into_iter().next() {
        Some(l) => l,
        None => panic!("harness: the stub's first stdout line was empty/missing — cannot assert the `session/update` shape"),
    };
    let reply: serde_json::Value =
        serde_json::from_str(&reply_line).unwrap_or_else(|e| panic!("canned reply is not valid JSON: {e} (got {reply_line:?})"));
    assert_eq!(
        reply.get("jsonrpc").and_then(|v| v.as_str()),
        Some("2.0"),
        "a canned ACP reply must be JSON-RPC 2.0"
    );
    assert_eq!(
        reply.get("method").and_then(|v| v.as_str()),
        Some("session/update"),
        "ADR 0012: `session/prompt` streams back a `session/update`"
    );
    let sessions = reply
        .get("params")
        .and_then(|p| p.get("sessions"))
        .and_then(|s| s.as_array())
        .expect("canned `session/update` params must list the stub's sessions");
    let names: Vec<_> = sessions.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        names,
        vec![Some("alpha"), Some("beta")],
        "the stub advertises the default sessions `alpha,beta` (docs/testing.md line 30)"
    );

    // (3) `session/cancel` ends the turn. Per the harness design (pack),
    //     the harness signals "the turn is over" by CLOSING the stub's stdin
    //     — the stub, blocked reading its next message, sees EOF and exits
    //     0. (The `session/cancel` message itself is a "keep running" ACP
    //     method: the stub writes a `turn/end` marker for it and, if it were
    //     still alive, would wait for the next message. So the observable
    //     "turn ends" cross-OS signal is the process EXITING 0 on the
    //     harness's EOF, not on cancel alone.)
    let cancel = jsonrpc_request("req-cancel", "session/cancel", &json!({ "sessionId": "stub" }));
    write_line(&mut stdin, &cancel).expect("harness: could not write `session/cancel` to stub-acp stdin");
    // Close the stub's stdin: EOF -> the stub exits cleanly (exit 0).
    drop(stdin);

    // Wait for the exit, and drain stderr off the pipe on a thread (a blocked
    // reader thread would otherwise leave `wait` unable to reap on a full
    // stderr pipe). A non-empty stderr means the stub hit a genuine read
    // ERROR (exit 1) rather than the benign EOF — surface it in the message.
    let stderr_res = (|| -> io::Result<String> {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut stderr, &mut buf)?;
        Ok(buf)
    })();
    let stderr_text = match stderr_res {
        Ok(s) => s,
        Err(e) => e.to_string(),
    };

    let status = child
        .wait()
        .expect("harness: could not wait on the stub after closing its stdin");
    assert!(
        status.success(),
        "the stub must EXIT 0 when its turn ends (EOF on stdin); got {:?} — a non-zero exit means the \
         'turn ends' signal is not observable (stub stderr: {:?})",
        status.code(),
        stderr_text.trim()
    );
}

/// `kill_tree_without_sigterm_alone` — spawn a long-lived child in its own
/// process group, stop it with the portable non-SIGTERM idiom
/// (`kill_tree`: Unix `killpg(-pid, SIGKILL)`, Windows `taskkill /F /T`),
/// and assert it is actually GONE within the 2s budget. The liveness-within-
/// budget is the real cross-OS signal: if we had only sent SIGTERM to a
/// child that ignored it, the wait below would hang past the budget — that
/// hang is the failure (the canary's rule: a silent skip is a fail).
///
/// The child is deliberately NOT asserted to have a particular exit code:
/// `sleep 30` (Unix) and `timeout /t 30 /nobreak` (Windows) both exit 0 on
/// their own, so exit code is not a portable discriminator. "Gone within
/// budget" is. REQUIRED: must pass on all 3.
#[test]
fn kill_tree_without_sigterm_alone() {
    // A long-lived child (30s, far beyond the 2s budget), started in its own
    // process group so `kill_tree` can take down the whole tree.
    let (program, args) = long_lived_cmd();
    let mut child = spawn_bin(&program, &args, ProcessGroup::New);
    // `Child::id()` is an infallible `u32` (the child is already running);
    // no `expect` needed.
    let pid = child.id();

    // Portable kill: Unix -> killpg(-pgid, SIGKILL) to the whole group;
    // Windows -> taskkill /F /T. Neither relies on SIGTERM alone.
    kill_tree(&child);

    // The child must be gone within the budget. If `wait` blocks past the
    // budget, the tree was not killed (SIGTERM ignored / not escalated) —
    // that is exactly the failure this test exists to catch.
    let started = Instant::now();
    let status = child.wait();
    let elapsed = started.elapsed();
    assert!(
        elapsed < BUDGET,
        "harness: child pid {pid} was still alive {elapsed:?} after kill_tree (budget {BUDGET:?}) — \
         the process tree was NOT killed; we likely relied on SIGTERM/kill() alone",
    );
    status.expect("wait on a killed child should return a status (not a spawn error)");
}

/// `port_probe_ipv4_and_optional_ipv6` — (a) REQUIRED: bind
/// `127.0.0.1:41807` (the real server's required port), confirm success, drop
/// it, then assert the same address binds again (the probe/cleanup round-
/// trips — a leaked bind would make #31's "is my port free?" check lie).
/// (b) OPTIONAL: attempt `[::1]:41807`; if `::1` is unavailable, `eprintln!`
/// the reason and continue (the one allowed skip — it must NOT fail the
/// required path). Mirrors the canary's message style.
#[test]
fn port_probe_ipv4_and_optional_ipv6() {
    // (a) REQUIRED, IPv4. The required listen port must be bindable…
    assert!(
        free_port_is_available("127.0.0.1:41807").is_ok(),
        "127.0.0.1:41807 must be bindable (the required listen port is free)"
    );
    // …then, after that bind+drop, the SAME address must be bindable AGAIN:
    // the probe must not leak a listener (otherwise #31 would see the port
    // as 'free' when it is not). We insert a short gap between the two
    // binds: a same-address rebind immediately after a drop can spuriously
    // fail if the kernel is still recycling the socket (TIME_WAIT) or if a
    // parallel test happens to hold the port — the gap lets the first bind
    // fully release the address, so this assert tests "no LEAK" (the thing
    // we care about) rather than a kernel timing race.
    thread::sleep(Duration::from_millis(50));
    assert!(
        free_port_is_available("127.0.0.1:41807").is_ok(),
        "after a bind+drop, 127.0.0.1:41807 must be bindable again (the probe leaked a listener)"
    );

    // (b) OPTIONAL, IPv6 — a graceful skip when `::1` is unavailable (the
    // one allowed no-fail path, with a reason; never a silent skip).
    match free_port_is_available("[::1]:41807") {
        Ok(()) => eprintln!(
            "  harness: optional IPv6 case — [::1]:41807 is available; bind+drop round-tripped"
        ),
        Err(e) => eprintln!(
            "  harness: optional IPv6 case SKIPPED (not a failure) — [::1]:41807 is unavailable \
             on this host ({e}). The required path is IPv4 127.0.0.1; do not fail the matrix on an \
             off-by-configuration IPv6 loopback."
        ),
    }
}

/// `harness_budget_holds` — a trivially-green guard (mirrors the canary's
/// `whole_suite_stays_within_budget`) asserting the harness's overall budget
/// constant is sane, so "did the harness run at least one case?" is
/// unambiguous in CI logs.
#[test]
fn harness_budget_holds() {
    assert!(
        BUDGET.as_secs() == 2,
        "harness budget drifted; docs/testing.md pins the runner at <2s"
    );
}

// ---------------------------------------------------------------------------
// #31 scaffold — deliberately NOT a #[test]. A passing claim or a silent
// no-op skip are both forbidden by the canary's rule, so the real end-to-end
// wiring is a documented non-test helper: obviously scaffold, not a passing
// test. Issue #31 turns this into a real `#[test]` that mints a token, spawns
// the real `holler-server` + `holler-client`, drives `holler status`, and
// asserts on their `--json` stdout — keeping the token out of every
// log / panic / assert message.
// ---------------------------------------------------------------------------

/// WIRED AT #31: the real end-to-end. Not a test yet — see the marker above.
///
/// TODO(#31):
///   1. Mint a token (opaque handle; the value never reaches a log/panic).
///   2. `spawn_bin` the real `holler-server` in its own process group,
///      `--listen 127.0.0.1:41807` (+ optional `[::1]:41807`), first
///      asserting `free_port_is_available("127.0.0.1:41807")` is Ok.
///   3. `spawn_bin` the real `holler-client` with the minted token.
///   4. Drive `holler status` / `holler token ping` and assert on `--json`
///      stdout.
///   5. `kill_tree` the whole thing; assert gone within `BUDGET`.
///   6. Convert this to `#[test]` and remove this scaffolding comment.
#[allow(dead_code)]
fn wire_first_talk_at_31() {
    eprintln!(
        "harness scaffold: `wire_first_talk_at_31` is not yet wired (lands with #31). \
         It is intentionally not a #[test] — a silent no-op skip is a fail."
    );
}

// ---------------------------------------------------------------------------
// Local scaffolding for the tests above.
// ---------------------------------------------------------------------------

/// The long-lived command the kill-tree test drives, per OS. No shell:
/// the program + args go straight to CreateProcess/execve.
///   - Unix:    `sleep 30`   (dies to SIGKILL / SIGKILL-of-group; exits non-zero)
///   - Windows: `timeout /t 30 /nobreak` (idles 30s ignoring keystrokes; exits 0)
///
/// Both are far longer than the 2s budget, so the test's outcome is decided
/// by our kill, not by the child finishing on its own.
fn long_lived_cmd() -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "timeout".to_string(),
            vec!["/t".to_string(), "30".to_string(), "/nobreak".to_string()],
        )
    } else {
        ("sleep".to_string(), vec!["30".to_string()])
    }
}

/// Build one JSON-RPC 2.0 *request* (id + method + params) as a
/// newline-terminated line, the shape the stub speaks over stdio.
fn jsonrpc_request(id: &str, method: &str, params: &serde_json::Value) -> String {
    let v = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let mut s = serde_json::to_string(&v).expect("`json!` cannot fail to serialize here");
    s.push('\n');
    s
}

/// Write a newline-terminated line to a piped `Stdio` and flush.
fn write_line<W: Write>(w: &mut W, line: &str) -> std::io::Result<()> {
    w.write_all(line.as_bytes())?;
    w.flush()
}
