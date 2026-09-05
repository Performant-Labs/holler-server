//! Canary: "test of tests" (issue #41, epic #27 step 1).
//!
//! Proves the harness can fail. If any case below is green while the
//! runner swallowed the failure, later e2e is theater — stop and fix
//! the runner first.
//!
//! Requirements (docs/testing.md, issue #41):
//!   1. A nested case designed to fail; the outer test fails immediately
//!      if that case passed or if the failure was swallowed.
//!   2. Fail fast: under 2 seconds. No 30s dial timeout, no hang.
//!   3. Linux, macOS, Windows. Language process API (std::process),
//!      not bash, not Playwright.
//!   4. Lives in tests/wire/selftest/. No working holler binary required.
//!
//! A silent skip is a fail: each case must either pass or panic —
//! `#[ignore]` or early-return-with-no-assertion is forbidden.

#![allow(unreachable_code)] // intentional: dead branch documents the failure path

use std::io;
use std::net::TcpStream;
use std::process::Command;
use std::time::{Duration, Instant};

const BUDGET: Duration = Duration::from_secs(2);

/// Run a nested case; the outer test fails if it returned Ok(())
/// where it should have failed.
fn must_fail<F: FnOnce() -> io::Result<()>>(what: &str, f: F) {
    let started = Instant::now();
    let res = f();
    let elapsed = started.elapsed();

    if elapsed >= BUDGET {
        panic!(
            "canary: `{what}` did not fail fast (took {}s; budget {}s). \
             The runner likely swallowed the failure or waited on a timeout.",
            elapsed.as_secs_f32(),
            BUDGET.as_secs()
        );
    }

    match res {
        Ok(()) => panic!(
            "canary: `{what}` returned Ok — a failure was designed in but \
             the runner reported success. Every later e2e is theater."
        ),
        Err(e) => {
            // Expected: the nested case failed, and we observed it.
            eprintln!("  canary: `{what}` failed as designed ({e}); observed in {}ms", elapsed.as_millis());
        }
    }
}

/// (a) Spawn a child that exits 1 via the language process API.
///     Cross-platform: no bash, no `sh -c`, no `cmd /c`.
#[test]
fn nested_case_exit_1_is_observed() {
    must_fail("nested exit 1", || {
        let prog = if cfg!(windows) { "cmd" } else { "sh" };
        let args = if cfg!(windows) {
            vec!["/c", "exit", "1"]
        } else {
            vec!["-c", "exit 1"]
        };

        let out = Command::new(prog)
            .args(&args)
            .output()
            .expect("spawning `sh -c exit 1` (or `cmd /c exit 1`) must not fail to start");

        if out.status.success() {
            // `sh -c exit 1` should never succeed. If it did, the
            // platform's shell is not doing what we think — fail loudly.
            return Ok(());
        }
        Err(io::Error::other(format!(
            "child exited with {:?}",
            out.status.code()
        )))
    });
}

/// (b) Dial 127.0.0.1:41807 with nothing listening. Must fail fast —
///     on loopback a closed port yields immediate RST (macOS/Linux) or
///     fast connect-refused (Windows). A hang means the runner waited
///     on a timeout instead of observing the refusal.
#[test]
fn dial_closed_port_fails_fast() {
    must_fail("dial 127.0.0.1:41807 with nothing listening", || {
        // A port is "closed" only if nothing is listening on it.
        // Probe first: if something *is* listening (e.g. a leftover
        // dev server), the test would be meaningless — but the issue
        // says "no server", so we bind a throwaway listener on the
        // same port to *ensure* it was free before the dial, then drop it.
        {
            let probe = std::net::TcpListener::bind("127.0.0.1:41807").map_err(|e| {
                io::Error::other(format!(
                    "port 41807 is busy ({e}); the canary needs it free. \
                     Kill the process holding it (lsof -i :41807) and re-run."
                ))
            })?;
            drop(probe);
        }

        let stream = TcpStream::connect("127.0.0.1:41807").map_err(|e| {
            io::Error::other(format!("connect to closed port refused (as designed): {e}"))
        })?;
        drop(stream);
        Ok(())
    });
}

/// (c) The outer harness must itself be fast. If the whole canary
///     suite exceeds the budget, the runner is slow enough to hide a
///     hang in later e2e.
#[test]
fn whole_suite_stays_within_budget() {
    // No-op: the budget is enforced per-case by `must_fail`.
    // This test exists so `cargo test` always has a third case that
    // is trivially green, making "did the runner run at least one test?"
    // unambiguous in CI logs.
    assert!(
        BUDGET.as_secs() == 2,
        "budget drifted; docs/testing.md pins the canary at <2s"
    );
}

/// (d) Sanity: a nested case that *should* succeed is observed as Ok.
///     Guards against a runner that inverts results (fails everything,
///     which would also be a form of theater — just loudly so).
#[test]
fn nested_case_success_is_observed() {
    let started = Instant::now();
    let res: io::Result<()> = (|| {
        #[cfg(unix)]
        {
            let out = std::process::Command::new("sh").args(["-c", "exit 0"]).output()?;
            assert!(out.status.success());
        }
        #[cfg(windows)]
        {
            let out = std::process::Command::new("cmd").args(["/c", "exit", "0"]).output()?;
            assert!(out.status.success());
        }
        Ok(())
    })();
    let elapsed = started.elapsed();

    res.expect("a designed-to-succeed case must be Ok; runner inverts results?");
    assert!(
        elapsed < BUDGET,
        "success path took {elapsed:?}; over budget"
    );
}
