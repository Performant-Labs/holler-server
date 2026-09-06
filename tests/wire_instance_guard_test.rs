//! Integration tests for issue #89: guard against two `holler-server serve`
//! processes sharing the same `HOLLER_STATE_DIR`.
//!
//! Before this fix, `serve_control_socket` unconditionally deleted and
//! rebound the control-socket file with no check that a live process
//! still owned it — a second instance silently stole every local CLI
//! command (`roster`, `say`, `interrupt`, `status`) away from the first,
//! still-running instance. These tests drive the real `holler` binary as
//! two separate OS processes (the shape the actual bug takes), the same
//! style as `tests/wire_first_talk_test.rs`'s `ServerProcess`.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;

fn holler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_holler-server"))
}

/// A fresh, isolated state dir + pepper per test — mirrors
/// `tests/wire_first_talk_test.rs`'s `Env` so parallel tests never share
/// a token store or race on process env.
struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Env {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = holler();
        cmd.env("HOLLER_STATE_DIR", self.dir.path())
            .env("HOLLER_SERVER_PEPPER", "instance-guard-test-pepper");
        cmd
    }
}

/// A `holler-server serve` child that has actually bound successfully, on an
/// OS-assigned loopback port. Killed on drop so a failing assertion
/// never leaks the process.
struct ServerProcess {
    child: Child,
}

impl ServerProcess {
    fn spawn(env: &Env) -> Self {
        let mut cmd = env.cmd();
        cmd.args(["serve", "--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn `holler-server serve`");
        let stdout = child.stdout.take().expect("stdout was piped");

        // Read the "listening on: ws://127.0.0.1:PORT" line off a
        // dedicated thread so a server that never prints it fails this
        // test with a clear timeout instead of hanging the suite.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(stdout).lines();
            if let Some(Ok(line)) = lines.next() {
                let _ = tx.send(line);
            }
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("`holler-server serve` printed its listening line within 5s");

        ServerProcess { child }
    }

    /// Kill the process the hard way (`SIGKILL` on Unix — what
    /// `std::process::Child::kill` sends) and reap it, without giving it
    /// any chance to run its own control-socket cleanup. Leaves the
    /// control socket **file** behind, exactly what an unclean shutdown
    /// (a crash, an OOM kill, `kill -9`) does in production.
    fn kill_uncleanly(mut self) {
        self.child.kill().expect("SIGKILL the server process");
        self.child.wait().expect("reap the killed process");
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run a second `holler-server serve` against `env`'s state dir to completion
/// and return its output — used only where the process under test is
/// expected to exit (refused startup), so there is no "listening on"
/// line to wait for the way [`ServerProcess::spawn`] does.
fn run_serve_to_completion(env: &Env) -> Output {
    env.cmd()
        .args(["serve", "--listen", "127.0.0.1:0"])
        .output()
        .expect("run the second `holler-server serve`")
}

#[test]
fn a_second_instance_against_the_same_state_dir_refuses_to_start() {
    let env = Env::new();
    let _first = ServerProcess::spawn(&env);

    let second = run_serve_to_completion(&env);
    assert!(
        !second.status.success(),
        "a second `holler-server serve` against the same HOLLER_STATE_DIR must refuse to start: {second:?}"
    );
    let stderr = String::from_utf8(second.stderr).unwrap().to_lowercase();
    assert!(
        stderr.contains("already running"),
        "expected a clear \"already running\" refusal, got: {stderr:?}"
    );
}

#[test]
fn the_first_instance_keeps_answering_after_a_second_instance_is_refused() {
    let env = Env::new();
    let _first = ServerProcess::spawn(&env);

    let second = run_serve_to_completion(&env);
    assert!(!second.status.success(), "{second:?}");

    // The first, still-running instance's control socket must still be
    // answering real commands after the second instance's failed steal
    // attempt — the whole point of refusing instead of silently
    // rebinding out from under it.
    let status_out = env.cmd().args(["status", "--json"]).output().unwrap();
    assert!(status_out.status.success(), "{status_out:?}");
    let status_json: Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(status_json["role"], "server");
    assert_eq!(
        status_json["listening"].as_array().map(|a| a.len()),
        Some(1),
        "the first instance's own listener, not an empty/no-server doc: {status_json:?}"
    );

    let roster_out = env.cmd().args(["roster", "--json"]).output().unwrap();
    assert!(
        roster_out.status.success(),
        "`roster` must still reach the live first instance: {roster_out:?}"
    );
    // An empty roster (no sessions advertised) is expected here — the
    // point is that the command round-trips through the control socket
    // at all, not any particular roster content.
    let roster_json: Value = serde_json::from_slice(&roster_out.stdout).unwrap();
    assert!(roster_json.as_array().is_some(), "{roster_json:?}");
}

#[test]
fn a_fresh_instance_recovers_a_stale_socket_left_by_an_unclean_shutdown() {
    let env = Env::new();
    let first = ServerProcess::spawn(&env);

    // Simulate a crash / `kill -9`: nothing runs `serve_control_socket`'s
    // own cleanup, so the control socket *file* is left behind with
    // nothing listening on it any more.
    first.kill_uncleanly();
    // Give the OS a moment to fully tear the killed process down (closed
    // fds, freed port) before the next instance starts.
    std::thread::sleep(Duration::from_millis(200));

    // A fresh instance against the very same state dir must correctly
    // detect the leftover socket is stale (nothing answers a probe) and
    // start normally — this is the "don't falsely refuse to start after
    // a real crash" case, just as important as detecting a genuinely
    // live instance.
    let second = ServerProcess::spawn(&env);

    let status_out = env.cmd().args(["status", "--json"]).output().unwrap();
    assert!(status_out.status.success(), "{status_out:?}");
    let status_json: Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(status_json["role"], "server");

    drop(second);
}
