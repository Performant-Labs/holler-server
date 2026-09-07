//! Unix I/O & Stdio Behavior group (hlrsvr-1300-1399, holler-server#98).
//! Redirection, piping, /dev/null, no-TTY, and signal handling for
//! `holler-server`'s real binary -- not unit-level, these all exercise
//! actual OS-level stdio/process semantics a human operator or a process
//! supervisor (systemd, docker) would actually hit.

mod support;

use std::process::Stdio;
use std::time::Duration;

use support::StateDir;

/// hlrsvr-1300: a one-shot JSON command's stdout, redirected to a file,
/// contains exactly the JSON document and nothing else -- no log noise
/// mixed in, stderr stays empty on the success path.
#[test]
fn stdout_redirect_captures_clean_json() {
    let state = StateDir::new();
    let out = support::holler_server_cmd(&state)
        .args(["status", "--json"])
        .output()
        .expect("failed to run holler-server status");
    assert!(out.status.success());
    assert!(
        out.stderr.is_empty(),
        "stderr should be empty on the success path, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout was not clean JSON ({e}): {:?}", out.stdout));
}

/// hlrsvr-1301: piping stdout into a downstream reader that closes early
/// (the classic `| head -1` shape) must not hang or crash the writer --
/// a broken pipe on a one-shot command's single write is a non-event.
#[test]
fn piped_output_does_not_hang_on_closed_downstream() {
    let state = StateDir::new();
    let mut child = support::holler_server_cmd(&state)
        .args(["status", "--json"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn holler-server status");

    // Drop the read end immediately, before the child has necessarily
    // finished writing -- this is what triggers a real SIGPIPE/EPIPE on
    // the child's next write, the actual condition `| head -1` creates.
    drop(child.stdout.take());

    let status = support::wait_for(Duration::from_secs(5), || child.try_wait().ok().flatten())
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("holler-server status hung after its stdout pipe was closed early");
        });
    // A one-shot command racing a closed pipe may see success (it finished
    // writing before the pipe closed) or a broken-pipe error -- either is
    // fine. Hanging is the only failure this case guards against.
    let _ = status;
}

/// hlrsvr-1302: redirecting stdout+stderr to /dev/null does not hang or
/// error -- a common non-interactive invocation shape (cron, a supervisor
/// health probe).
#[test]
fn redirect_to_dev_null_does_not_hang() {
    let state = StateDir::new();
    let out = support::holler_server_cmd(&state)
        .args(["status", "--json"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("failed to run holler-server status with /dev/null stdio");
    assert!(out.status.success());
}

/// hlrsvr-1303: no controlling TTY / stdin already closed must not make
/// a one-shot command block waiting for input it will never receive --
/// this is exactly the invocation shape a process supervisor uses.
#[test]
fn no_tty_stdin_closed_does_not_block() {
    let state = StateDir::new();
    let out = support::holler_server_cmd(&state)
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .output()
        .expect("failed to run holler-server status with stdin closed");
    assert!(out.status.success());
}

/// hlrsvr-1304: SIGINT (the same signal Ctrl-C sends) triggers
/// `holler-server serve`'s real graceful-shutdown path -- confirmed via
/// its own "shutting down" announcement and a clean exit 0, not just "it
/// eventually died."
#[cfg(unix)]
#[test]
fn sigint_triggers_graceful_shutdown() {
    use std::io::{BufRead, BufReader};

    let state = StateDir::new();
    let mut child = support::holler_server_cmd(&state)
        .args(["serve", "--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn holler-server serve");

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("failed to read the listen-address announcement");
    assert!(line.contains("listening on"), "unexpected first line: {line}");

    // SAFETY: `child.id()` names a live child process this test owns
    // exclusively; SIGINT does not touch memory.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }

    let status = support::wait_for(Duration::from_secs(5), || child.try_wait().ok().flatten())
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("holler-server serve did not exit within 5s of SIGINT");
        });
    assert!(status.success(), "SIGINT should produce a clean exit 0, got {status:?}");

    let mut rest = String::new();
    std::io::Read::read_to_string(&mut reader, &mut rest).ok();
    assert!(
        rest.contains("shutting down"),
        "expected the graceful-shutdown announcement after SIGINT, got: {rest:?}"
    );
}

/// hlrsvr-1305: SIGTERM is a DIFFERENT signal from SIGINT, and
/// `holler-server serve` currently only awaits `tokio::signal::ctrl_c()`
/// (SIGINT) -- it does not install a SIGTERM handler. This test pins
/// TODAY's real, verified behavior: SIGTERM kills the process via the OS
/// default disposition (no graceful-shutdown announcement, a
/// signal-terminated exit status), NOT the graceful path SIGINT gets.
///
/// This is a real, notable gap for anything that stops this process with
/// SIGTERM by default (systemd, `docker stop`, most process supervisors)
/// -- flagged in this test's own ticket (hlrsvr-1305) as a candidate
/// follow-up, not silently treated as correct-by-design. If SIGTERM
/// handling is added later, this test's expectations must change with it
/// -- that is the point of pinning current behavior explicitly.
#[cfg(unix)]
#[test]
fn sigterm_is_not_currently_caught() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::ExitStatusExt;

    let state = StateDir::new();
    let mut child = support::holler_server_cmd(&state)
        .args(["serve", "--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn holler-server serve");

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("failed to read the listen-address announcement");
    assert!(line.contains("listening on"), "unexpected first line: {line}");

    // SAFETY: same as the SIGINT case above.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }

    let status = support::wait_for(Duration::from_secs(5), || child.try_wait().ok().flatten())
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("holler-server serve did not exit within 5s of SIGTERM");
        });

    assert_eq!(
        status.signal(),
        Some(libc::SIGTERM),
        "expected SIGTERM to terminate the process via the OS default disposition \
         (uncaught), got {status:?} -- if this now fails because SIGTERM IS caught, \
         update this test to assert the new graceful behavior instead"
    );

    let mut rest = String::new();
    std::io::Read::read_to_string(&mut reader, &mut rest).ok();
    assert!(
        !rest.contains("shutting down"),
        "SIGTERM should NOT go through the graceful-shutdown path today; if it now does, \
         this is a real behavior change worth its own changelog entry, not a silent pass"
    );
}
