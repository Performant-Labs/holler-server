//! Logging & Debug Levels group (holler-server#98, hlrsvr-1200 range).
//!
//! `holler-server` has no `RUST_LOG` -- verbosity is its own two-axis
//! `--debug`/`--log-format` contract (`src/debug.rs`), which already has
//! extensive unit coverage for parsing/precedence (see that module's own
//! `mod tests`; several `hlrsvr-12xx` cases point straight at those real
//! functions instead of duplicating them here). This file holds the two
//! properties that can only be proven at the real-process level: that an
//! invalid value actually fails the process closed, and that debug output
//! actually lands on stderr, not stdout, for a real running server.

use assert_cmd::Command;
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::{Duration, Instant};

fn holler() -> Command {
    Command::cargo_bin("holler-server").expect("holler-server binary not built")
}

/// hlrsvr-1200: an invalid `--debug` value fails the whole process closed
/// -- non-zero exit, a clear message on stderr, nothing on stdout -- for
/// any subcommand, not just `serve`.
#[test]
fn invalid_debug_value_fails_closed() {
    holler()
        .args(["--debug=bogus", "token", "list"])
        .assert()
        .failure()
        .code(1)
        .stdout("")
        .stderr(predicates::str::contains(
            "invalid debug level \"bogus\": expected one of none, quiet, noisy",
        ));
}

/// hlrsvr-1203: at `--debug=noisy`, log lines go to stderr only -- a real
/// running server's stdout carries just its one human-facing "listening
/// on" announcement, never a log line.
#[test]
fn noisy_debug_logs_go_to_stderr_not_stdout() {
    let state_dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_holler-server"))
        .env("HOLLER_STATE_DIR", state_dir.path())
        .args(["--debug=noisy", "serve", "--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn holler-server serve");

    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut stderr = BufReader::new(child.stderr.take().unwrap());

    // Block until the real listen-address announcement arrives on stdout,
    // or fail fast rather than hang if it never does.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stdout_line = String::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "server did not announce a listen address in time"
        );
        stdout_line.clear();
        let n = stdout.read_line(&mut stdout_line).unwrap();
        assert_ne!(n, 0, "server exited before announcing a listen address");
        if stdout_line.starts_with("holler-server listening on:") {
            break;
        }
    }

    // Give the already-running server a moment to emit its startup log
    // line (`logging_started`, unconditional whenever debug is on) before
    // stopping it.
    std::thread::sleep(Duration::from_millis(200));

    #[cfg(unix)]
    {
        // SAFETY: `child.id()` names a live child process this test owns
        // exclusively; SIGINT does not touch memory.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGINT);
        }
    }
    let _ = child.wait();

    let mut stderr_text = String::new();
    std::io::Read::read_to_string(&mut stderr, &mut stderr_text).unwrap();
    assert!(
        stderr_text.contains("logging_started"),
        "expected the startup log line on stderr, got:\n{stderr_text}"
    );

    // Drain any remaining stdout and confirm it never grew a log line.
    let mut rest_of_stdout = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut rest_of_stdout).unwrap();
    let full_stdout = stdout_line + &rest_of_stdout;
    assert!(
        !full_stdout.contains("logging_started"),
        "a log line leaked onto stdout:\n{full_stdout}"
    );
}
