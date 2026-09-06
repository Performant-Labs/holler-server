//! Shared test-support harness for `holler-server` integration tests
//! (holler-server#98's test-catalog pilot). Real subprocess orchestration
//! against the built binary and its actual CLI surface — not stubs, not a
//! reimplementation of server logic — so a passing test proves the same
//! thing a human operator driving the real binary would see.
//!
//! Scope note: this module only drives `holler-server` itself (state dir,
//! `serve`, `token mint`, `roster`). A true end-to-end `join`/`run` smoke
//! test needs a real `holler-client` binary too, which lives in a separate
//! repo with no workspace/path dependency between them (deliberately, to
//! keep each crate's own `cargo test` self-contained by default) — the
//! mirror of this module in `holler-client`'s own `tests/support/mod.rs`
//! carries `interop_smoke_test.rs`, gated behind `HOLLER_SERVER_BIN`.
//!
//! Not every test binary in this repo uses every helper here (e.g.
//! `wait_for` is for the Process Lifecycle / Concurrency groups this pilot
//! doesn't fill yet) -- `dead_code` is allowed at module scope for that
//! reason, the same way a shared test-support module does in most crates.
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;

/// Default timeout for anything that waits on a subprocess to do something
/// (announce a listen address, exit after a stop signal). Generous enough
/// for a loaded CI runner, short enough that a genuinely hung process
/// fails a test in seconds, not minutes.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// A fresh, empty `HOLLER_STATE_DIR` for one test. Never shared between
/// tests, so parallel `cargo test` runs can't race each other's token
/// store or control socket.
pub struct StateDir(tempfile::TempDir);

impl StateDir {
    pub fn new() -> Self {
        Self(tempfile::tempdir().expect("failed to create ephemeral HOLLER_STATE_DIR"))
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Default for StateDir {
    fn default() -> Self {
        Self::new()
    }
}

/// Poll `check` every 20ms until it returns `Some(_)` or `timeout` elapses.
/// The one place any harness wait (a port appearing, a roster row
/// appearing) goes through, instead of ad hoc `sleep`s in individual tests.
pub fn wait_for<T>(timeout: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = check() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn holler_server_cmd(state_dir: &StateDir) -> Command {
    let mut cmd = Command::cargo_bin("holler-server").expect(
        "holler-server binary not built -- run `cargo build` (or `cargo test`, which builds it \
         automatically) before invoking harness helpers directly",
    );
    cmd.env("HOLLER_STATE_DIR", state_dir.path());
    // Token minting fails closed without this (ADR 0009) -- a fixed test
    // value is fine, every harness-spawned server/CLI invocation in a given
    // test uses its own ephemeral `StateDir`, so there's no real secret to
    // protect and no cross-test collision risk.
    cmd.env("HOLLER_SERVER_PEPPER", "harness-test-pepper-not-a-real-secret");
    cmd
}

/// A live `holler-server serve` subprocess bound to an OS-assigned loopback
/// port (`--listen 127.0.0.1:0`), so parallel tests never collide on a
/// fixed port.
pub struct ServerHandle {
    child: Child,
    /// The real bound address (e.g. `127.0.0.1:54213`), read back from the
    /// server's own stdout announcement -- not assumed, not reconstructed.
    pub addr: String,
    pub state_dir: StateDir,
}

impl ServerHandle {
    /// Starts a real `holler-server serve` and blocks until its own stdout
    /// announces the bound listen address, or `timeout` elapses (in which
    /// case this panics with a clear message rather than returning a handle
    /// to a server nothing has confirmed is actually listening).
    pub fn start(timeout: Duration) -> Self {
        let state_dir = StateDir::new();
        let mut child = holler_server_cmd(&state_dir)
            .args(["serve", "--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn `holler-server serve`");

        let stdout = child.stdout.take().expect("stdout was piped");
        let mut reader = BufReader::new(stdout);
        let deadline = Instant::now() + timeout;
        let mut line = String::new();
        let addr = loop {
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("`holler-server serve` did not announce a listen address within {timeout:?}");
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = child.kill();
                    panic!("`holler-server serve` exited before announcing a listen address");
                }
                Ok(_) => {
                    if let Some(addr) = parse_listen_line(&line) {
                        break addr;
                    }
                }
                Err(e) => {
                    let _ = child.kill();
                    panic!("failed reading `holler-server serve` stdout: {e}");
                }
            }
        };

        // Keep draining stdout in the background so the child's pipe
        // buffer never fills and blocks it once the test stops actively
        // reading -- otherwise a chatty `serve` would deadlock on write().
        std::thread::spawn(move || {
            let mut discard = String::new();
            while reader.read_line(&mut discard).unwrap_or(0) > 0 {
                discard.clear();
            }
        });

        Self { child, addr, state_dir }
    }

    /// Graceful stop (SIGINT on Unix -- the same signal a Ctrl-C sends,
    /// which `holler-server serve` already handles as a clean shutdown),
    /// falling back to a forced kill if the process doesn't exit within
    /// `timeout`. Windows has no SIGINT equivalent, so there the forced
    /// path is the only path -- both live in this one method rather than
    /// scattered per-test cleanup, per the "one place" requirement.
    pub fn stop(mut self, timeout: Duration) {
        #[cfg(unix)]
        {
            // SAFETY: `self.child.id()` names a live child process this
            // struct owns exclusively; SIGINT does not touch memory.
            unsafe {
                libc::kill(self.child.id() as libc::pid_t, libc::SIGINT);
            }
            let deadline = Instant::now() + timeout;
            loop {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // Unix fallback (graceful stop above didn't exit in time) and the
        // entire Windows path: std's `Child::kill()` is TerminateProcess
        // there and SIGKILL here -- already the right primitive on both.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_listen_line(line: &str) -> Option<String> {
    // "holler-server listening on: ws://127.0.0.1:41807[, ws://[::1]:41807]"
    let rest = line.trim().strip_prefix("holler-server listening on: ")?;
    let first = rest.split(',').next()?.trim();
    first.strip_prefix("ws://").map(str::to_string)
}

/// `holler-server token mint --label <label>` against `state_dir`. Returns
/// `(token_id, secret)` parsed from the real CLI output -- the same text a
/// human operator reads, not an in-process shortcut into `TokenStore`.
pub fn mint_token(state_dir: &StateDir, label: &str) -> (String, String) {
    let out = holler_server_cmd(state_dir)
        .args(["token", "mint", "--label", label])
        .output()
        .expect("failed to run `holler-server token mint`");
    assert!(
        out.status.success(),
        "`token mint` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let token_id = extract_field(&stdout, "token_id:")
        .unwrap_or_else(|| panic!("`token mint` output missing token_id:\n{stdout}"));
    let secret = extract_field(&stdout, "secret:")
        .unwrap_or_else(|| panic!("`token mint` output missing secret:\n{stdout}"));
    (token_id, secret)
}

/// `holler-server roster --json` against `state_dir` -- the live
/// control-socket roster, parsed as JSON. Real IPC round-trip, not a
/// direct call into `wire::control::query_roster`.
pub fn roster_json(state_dir: &StateDir) -> serde_json::Value {
    let out = holler_server_cmd(state_dir)
        .args(["roster", "--json"])
        .output()
        .expect("failed to run `holler-server roster`");
    assert!(
        out.status.success(),
        "`roster --json` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("`roster --json` did not print valid JSON ({e}):\n{}", String::from_utf8_lossy(&out.stdout)))
}

/// `holler-server status --json` against `state_dir`.
pub fn status_json(state_dir: &StateDir) -> serde_json::Value {
    let out = holler_server_cmd(state_dir)
        .args(["status", "--json"])
        .output()
        .expect("failed to run `holler-server status`");
    assert!(
        out.status.success(),
        "`status --json` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("`status --json` did not print valid JSON ({e}):\n{}", String::from_utf8_lossy(&out.stdout)))
}

fn extract_field(text: &str, prefix: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix(prefix).map(|v| v.trim().to_string()))
}
