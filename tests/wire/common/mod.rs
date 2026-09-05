//! Shared, OS-portable helpers for the `tests/wire` harness (issue #40).
//!
//! These are the *mechanical* bits of the process harness that #31 will
//! reuse to spawn/kill the real `holler-server` + `holler-client`:
//!   - `spawn_bin`      — spawn a child directly (no shell), piped stdio,
//!     optionally in its own process group.
//!   - `kill_tree`      — kill the whole process group, not SIGTERM alone.
//!   - `free_port_is_available` — bind+drop a loopback port (the probe).
//!   - `stub_acp_path`  — locate the sibling `stub-acp` binary.
//!
//! The canary (`tests/wire/selftest.rs`) keeps its own local copies and is
//! deliberately NOT refactored onto these — it must stay standalone.
//!
//! Ground rules (docs/testing.md): no bash / `sh -c` / `cmd /c` for the
//! spawn itself, no Unix sockets, `127.0.0.1` for the required path.
//!
//! ## Stub-spawn mechanism (and why not `CARGO_BIN_EXE_stub_acp`)
//!
//! Cargo only sets `CARGO_BIN_EXE_<name>` for `[[bin]]` targets whose path
//! is under `src/`. Our stub is pinned at `tests/wire/stub_acp.rs`, so that
//! macro is NOT defined (verified empirically: absent both as `env!` at
//! compile time and as `std::env::var` at runtime; `CARGO_TARGET_DIR` /
//! `CARGO_TARGET_TMPDIR` are also absent in the test process). The fallback
//! therefore reconstructs the path from `CARGO_MANIFEST_DIR` (which IS set
//! for test targets) + `target/{debug,release}/stub-acp` (+ `.exe` on
//! Windows), picking the profile from the `--test <name>` argv entry cargo
//! passes to the compiled test binary.

use std::env;
use std::io;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Unix: did `setpgid(child, child)` succeed, so the child is actually in its
/// own process group (pgid == its pid)? Captured once in [`spawn_bin`] and
/// read by [`kill_tree`]. macOS refuses `setpgid` to a *new* group with
/// EACCES unless the parent is itself a process-group leader, so this is NOT
/// guaranteed true on macOS and `kill_tree` must not assume it is.
#[cfg(unix)]
static CHILD_IN_OWN_PG: AtomicBool = AtomicBool::new(false);

/// The harness's overall time budget (docs/testing.md: "<2s"). Reused by
/// the per-case assertions so a hang is a fail, not a silent skip.
pub const BUDGET: Duration = Duration::from_secs(2);

/// How to position a spawned child with respect to a process group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessGroup {
    /// Join the harness's own process group (the normal case).
    Same,
    /// Put the child in its *own* new process group so the whole tree can
    /// be signalled at once (see [`kill_tree`]).
    New,
}

/// Spawn a child DIRECTLY (no shell): `Command::new(program).args(args)`
/// with piped stdin/stdout/stderr. The optional `process_group` puts the
/// child in its own process group (Unix `setpgid`) or, on Windows, under
/// `CREATE_NEW_PROCESS_GROUP` (a `std`-native flag — no `libc` needed for
/// the spawn side).
///
/// `program` is a path (e.g. the stub binary) or an absolute/PATH name.
/// No `/bin/sh`, no `cmd /c`: this is the portable "spawn the binary with
/// its args, read its stdout" idiom the harness is built on.
pub fn spawn_bin(
    program: impl AsRef<std::ffi::OsStr> + std::fmt::Debug,
    args: &[String],
    group: ProcessGroup,
) -> Child {
    // Borrow (not move) so `program` is still available for the panic
    // message below if spawn fails.
    let mut cmd = Command::new(&program);
    cmd.args(args);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if group == ProcessGroup::New {
        // std-native "own process group" on Windows; on Unix we instead put
        // the child in its own group via setpgid(pid, pid) after spawn.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
    }
    let child = cmd.spawn().unwrap_or_else(|e| panic!("harness: could not spawn `{program:?}`: {e}"));
    if group == ProcessGroup::New {
        #[cfg(unix)]
        {
            use libc::{pid_t, setpgid};
            // Try to put the child in its own group (pgid == pid) so
            // `kill_tree` can signal the whole tree with `killpg`. BUT macOS
            // refuses `setpgid` to a *new* group (EACCES) unless the parent is
            // a process-group leader, and CI runners are often not. So we CHECK
            // the result and record whether it worked — `kill_tree` falls back
            // to a direct `SIGKILL` of the pid when it did not, instead of
            // `killpg`-ing a non-existent group (which is a no-op and leaves
            // the child alive). A group of one is still "the whole tree" for
            // the children this harness spawns (the stub is a leaf; the
            // long-lived child is a bare `sleep`/`timeout` with no children).
            let ok = unsafe { setpgid(child.id() as pid_t, child.id() as pid_t) } == 0;
            CHILD_IN_OWN_PG.store(ok, Ordering::SeqCst);
        }
    }
    child
}

/// Kill the whole tree started by `child` WITHOUT relying on SIGTERM alone.
///
/// - **Unix**: prefer `killpg(-pid, SIGKILL)` to the child's own process
///   group (set up by `spawn_bin`), which also takes down any detached
///   grandchildren. If `spawn_bin`'s `setpgid` was refused — macOS returns
///   EACCES when the parent is not a process-group leader, which is the
///   common case on CI — fall back to a direct `SIGKILL` of the pid. Both
///   arms are SIGKILL (uncatchable), so neither "relies on SIGTERM alone";
///   the distinction is only *group* vs *single pid*.
/// - **Windows**: `taskkill /F /T` (force-terminate the process *and* its
///   tree). `child.kill()` (TerminateProcess) would also work but leaves a
///   detached grandchild if any, so the documented `taskkill /T` idiom is
///   preferred and is what `docs/testing.md` calls out.
///
/// We send the signal(s) here but do NOT block waiting: the *caller* waits
/// and asserts the child is gone within [`BUDGET`] — that wait is what turns
/// "we only SIGTERM'd and it was ignored" into a visible failure (a hang
/// past the budget) instead of a silent success.
pub fn kill_tree(child: &Child) {
    let pid = child.id();
    #[cfg(unix)]
    {
        kill_tree_unix(pid as i32);
    }
    #[cfg(windows)]
    {
        // `taskkill` is a normal PATH program on Windows; spawn it directly
        // (no shell), force-terminating the whole tree for this pid.
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID"])
            .arg(pid.to_string())
            .output();
        // The caller's wait+assert is authoritative. (We take the pid, not a
        // `&mut Child` handle, so we cannot also call `child.kill()` here.)
    }
}

#[cfg(unix)]
fn kill_tree_unix(pid: i32) {
    use libc::SIGKILL;
    // Preferred: the child was placed in its own process group at spawn, so
    // its group id == its pid; `killpg(-pid, SIGKILL)` signals the WHOLE
    // group (any detached grandchildren included), not just the leader.
    let pgid_ok = CHILD_IN_OWN_PG.load(Ordering::SeqCst);
    let group_signaled = if pgid_ok {
        // `libc::killpg` returns `()` and stashes its errno in
        // `io::Error::last_os_error()`. A raw_os_error of `None` means "no
        // error" (the signal reached a real group); an `Some(_)` (e.g.
        // `ESRCH`, no such group) means it was a no-op and we must fall back.
        unsafe { libc::killpg(-pid, SIGKILL) };
        io::Error::last_os_error().raw_os_error().is_none()
    } else {
        false
    };
    if !group_signaled {
        // `setpgid` was refused (macOS EACCES when the parent is not a
        // process-group leader) and/or `killpg` found no such group — the
        // child is still in the parent's group, so a group signal to "-pid"
        // is a no-op. Fall back to a direct SIGKILL of the pid. SIGKILL is
        // uncatchable, so this is still NOT "relying on SIGTERM alone" (the
        // child cannot ignore it); and it is not a lone `kill(SIGTERM)`. For
        // the tree this harness drives (a leaf stub / a bare sleep|timeout)
        // the direct SIGKILL of the pid takes down the whole thing.
        unsafe { libc::kill(pid, SIGKILL) };
    }
}

/// Best-effort: is `addr` (e.g. `"127.0.0.1:41807"`) currently bindable?
/// Binds a throwaway listener, then DROPS it (scope ends) so the address
/// is free again. `Ok(())` = it was free; `Err` = something is holding it
/// (or, for the optional IPv6 case, the family is unavailable).
///
/// The bind-then-drop-then-rebind round-trip is what the
/// `port_probe_ipv4_and_optional_ipv6` test asserts: a leaked bind would
/// make #31's "is my port free?" check lie.
pub fn free_port_is_available(addr: &str) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    drop(listener);
    Ok(())
}

/// Locate the sibling `stub-acp` binary the harness drives.
///
/// Prefers `CARGO_BIN_EXE_stub_acp` (set only for `src/`-rooted bins, which
/// this is not — see the file-level note); falls back to reconstructing
/// `<CARGO_MANIFEST_DIR>/target/{profile}/stub-acp[.exe]`. The profile is
/// read from the `--test <name>` argv entry cargo passes to the test binary
/// (the test is named `first_talk`, so the entry is `--test first_talk`).
///
/// Panics with a clear message if it cannot be located — a missing stub is a
/// build-order failure the harness must surface loudly, not silently skip.
pub fn stub_acp_path() -> PathBuf {
    if let Ok(p) = env::var("CARGO_BIN_EXE_stub_acp") {
        return PathBuf::from(p);
    }
    let manifest = env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is set for cargo test targets");
    let profile = infer_profile();
    let mut base = PathBuf::from(manifest);
    base.push("target");
    base.push(profile);
    base.push(if cfg!(windows) { "stub-acp.exe" } else { "stub-acp" });
    if base.exists() {
        return base;
    }
    panic!(
        "harness: could not locate the `stub-acp` binary (looked at `{base:?}`). \
         Run `cargo build --bins` first, or check the [[bin]] pin in Cargo.toml."
    );
}

/// Which target profile is this test binary built under? Derived from the
/// `--test <name>` / `--lib` / default argv cargo hands the test process.
fn infer_profile() -> &'static str {
    for a in env::args() {
        if a == "--test" || a == "--lib" {
            return "debug";
        }
        if a == "--release" {
            return "release";
        }
    }
    "debug"
}
