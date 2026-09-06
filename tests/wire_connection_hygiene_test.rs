//! Connection/message hygiene integration tests (issue #57,
//! `docs/research-security-hijack-dos.md` §4 "do now" cluster): the real
//! `holler-server serve` listener, driven by real `tokio-tungstenite` clients,
//! exercising the frame-size cap, the pre-auth read timeout, and the
//! concurrent-unauthenticated-connection cap. `HOLLER_MAX_FRAME_BYTES` /
//! `HOLLER_PRE_AUTH_TIMEOUT_MS` / `HOLLER_MAX_UNAUTH_CONNECTIONS` let each
//! test use a small, fast-to-exercise value instead of the real defaults
//! (2 MiB / 20s / 75).
//!
//! Mirrors `tests/wire_first_talk_test.rs`'s harness (`Env`, `ServerProcess`,
//! `mint`/`redeem`/`send_json`/`recv_json`) rather than sharing it, since
//! that file is not a library and pulling a shared helper crate in for one
//! extra test file is not worth the churn.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

fn holler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_holler-server"))
}

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
            .env("HOLLER_SERVER_PEPPER", "hygiene-test-pepper");
        cmd
    }
}

/// A running `holler-server serve` child on an OS-assigned loopback port, with
/// hygiene limits overridden via env, killed on drop so a failing
/// assertion never leaks the process.
struct ServerProcess {
    child: Child,
    port: u16,
}

impl ServerProcess {
    fn spawn(env: &Env, extra_env: &[(&str, &str)]) -> Self {
        let mut cmd = env.cmd();
        cmd.args(["serve", "--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn `holler-server serve`");
        let stdout = child.stdout.take().expect("stdout was piped");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(stdout).lines();
            if let Some(Ok(line)) = lines.next() {
                let _ = tx.send(line);
            }
        });
        let line = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("`holler-server serve` printed its listening line within 5s");
        let port = line
            .rsplit(':')
            .next()
            .and_then(|p| p.trim().parse::<u16>().ok())
            .unwrap_or_else(|| panic!("could not parse a port out of {line:?}"));

        ServerProcess { child, port }
    }

    fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn mint(env: &Env, label: &str) -> (String, String) {
    let out = env
        .cmd()
        .args(["token", "mint", "--label", label])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let token_id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("token_id:").map(|s| s.trim().to_string()))
        .unwrap();
    let secret = stdout
        .lines()
        .find_map(|l| l.strip_prefix("secret:").map(|s| s.trim().to_string()))
        .unwrap();
    (token_id, secret)
}

fn redeem(env: &Env, token_id: &str, secret: &str, machine: &str) -> (String, String) {
    let out = env
        .cmd()
        .args([
            "token", "redeem", token_id, "--secret", secret, "--machine", machine,
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let client_id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("client_id:").map(|s| s.trim().to_string()))
        .unwrap();
    let credential = stdout
        .lines()
        .find_map(|l| l.strip_prefix("credential:").map(|s| s.trim().to_string()))
        .unwrap();
    (client_id, credential)
}

fn auth_envelope(token_id: &str, credential: &str) -> Value {
    json!({
        "v": 1, "type": "auth", "id": "id-auth", "ts": "2026-09-05T00:00:00Z",
        "from": token_id, "body": { "credential": credential }
    })
}

async fn send_json(
    ws: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    v: &Value,
) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

async fn recv_json(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Value {
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("a reply arrives within 5s")
        .expect("the socket did not close")
        .expect("no websocket-level error");
    serde_json::from_str(msg.to_text().expect("text frame")).expect("valid JSON")
}

// ---------------------------------------------------------------------
// Frame-size cap
// ---------------------------------------------------------------------

#[test]
fn oversized_first_frame_closes_the_connection_instead_of_being_accepted() {
    let env = Env::new();
    // A tiny cap (well under a real `auth` envelope) makes even a
    // legitimate-shaped first frame oversized, so the connection must be
    // torn down rather than answered.
    let server = ServerProcess::spawn(&env, &[("HOLLER_MAX_FRAME_BYTES", "64")]);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .expect("client connects to the real listener");

        let oversized = json!({
            "v": 1, "type": "auth", "id": "id-auth", "ts": "2026-09-05T00:00:00Z",
            "from": "tok_whatever", "body": { "credential": "x".repeat(4096) }
        });
        // The frame is larger than the server's 64-byte cap, so the send
        // itself may succeed (it's a client-side write) but the server
        // must never answer it as a normal frame.
        let _ = send_json(&mut ws, &oversized).await;

        let outcome = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
        match outcome {
            // The connection closed (with or without a clean close frame)
            // rather than being processed.
            Ok(None) => {}
            Ok(Some(Err(_))) => {}
            Ok(Some(Ok(Message::Close(_)))) => {}
            other => panic!("expected the oversized frame to close the connection, got {other:?}"),
        }
    });
}

#[test]
fn a_normally_sized_frame_still_works_under_the_same_cap() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (_client_id, credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    // A cap well above what `auth` actually needs, but far below the
    // library's 64 MiB default, proving the cap doesn't collateral-damage
    // legitimate traffic.
    let server = ServerProcess::spawn(&env, &[("HOLLER_MAX_FRAME_BYTES", "65536")]);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws, &auth_envelope(&token_id, &credential)).await;
        let hello = recv_json(&mut ws).await;
        assert_eq!(hello["type"], "hello");
    });
}

// ---------------------------------------------------------------------
// Pre-auth read timeout
// ---------------------------------------------------------------------

#[test]
fn a_connection_that_never_sends_a_first_frame_is_closed_after_the_timeout() {
    let env = Env::new();
    let server = ServerProcess::spawn(&env, &[("HOLLER_PRE_AUTH_TIMEOUT_MS", "300")]);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .expect("client connects to the real listener");

        // Send nothing. The server must close this connection on its own
        // within the (short, test-only) pre-auth window, with no reply —
        // a distinguishable error would only help a slow-loris prober.
        let outcome = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
        match outcome {
            Ok(None) => {}
            Ok(Some(Err(_))) => {}
            Ok(Some(Ok(Message::Close(_)))) => {}
            other => panic!("expected the idle connection to close, got {other:?}"),
        }
    });
}

#[test]
fn a_connection_that_authenticates_within_the_timeout_is_unaffected() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (_client_id, credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    // Generous relative to how fast a real auth round trip completes, but
    // still far short of the real 20s default.
    let server = ServerProcess::spawn(&env, &[("HOLLER_PRE_AUTH_TIMEOUT_MS", "3000")]);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws, &auth_envelope(&token_id, &credential)).await;
        let hello = recv_json(&mut ws).await;
        assert_eq!(hello["type"], "hello");

        // The session must survive well past the pre-auth window — the
        // timeout only ever bounds the wait for the *first* frame.
        tokio::time::sleep(Duration::from_millis(500)).await;
        send_json(
            &mut ws,
            &json!({
                "v": 1, "type": "ping", "id": "id-ping", "ts": "2026-09-05T00:00:00Z",
                "from": token_id, "body": {}
            }),
        )
        .await;
        let pong = recv_json(&mut ws).await;
        assert_eq!(pong["type"], "pong");
    });
}

// ---------------------------------------------------------------------
// Concurrent-unauthenticated-connection cap
// ---------------------------------------------------------------------

#[test]
fn the_nth_plus_one_unauthenticated_connection_is_rejected_while_n_are_held_open() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (_client_id, credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    // A generous pre-auth timeout so the two held-open connections below
    // don't get reaped by the timeout mid-test, isolating this test to the
    // connection-cap behavior specifically.
    let server = ServerProcess::spawn(
        &env,
        &[
            ("HOLLER_MAX_UNAUTH_CONNECTIONS", "2"),
            ("HOLLER_PRE_AUTH_TIMEOUT_MS", "10000"),
        ],
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Fill both unauthenticated slots: connect but send nothing.
        let (ws1, _resp1) = tokio_tungstenite::connect_async(server.url())
            .await
            .expect("first unauthenticated connection is accepted");
        let (ws2, _resp2) = tokio_tungstenite::connect_async(server.url())
            .await
            .expect("second unauthenticated connection is accepted");

        // A third connection attempt must be rejected outright — the
        // server drops the raw TCP socket before completing the WebSocket
        // handshake, so the client-side connect either errors or never
        // completes.
        let third = tokio::time::timeout(
            Duration::from_secs(3),
            tokio_tungstenite::connect_async(server.url()),
        )
        .await;
        let rejected = match third {
            Err(_) => true,           // timed out waiting on a handshake that never comes
            Ok(Err(_)) => true,       // handshake failed
            Ok(Ok(_)) => false,
        };
        assert!(rejected, "a 3rd unauthenticated connection must be rejected while 2 are held open");

        // Authenticate the first held-open connection: this releases its
        // slot (auth succeeded, so it is no longer "unauthenticated") even
        // though `ws2` is still sitting open, unauthenticated.
        let mut ws1 = ws1;
        send_json(&mut ws1, &auth_envelope(&token_id, &credential)).await;
        let hello = recv_json(&mut ws1).await;
        assert_eq!(hello["type"], "hello");

        // A new connection now succeeds: the cap tracks pre-auth
        // connections only, not live/authenticated sessions.
        let fourth = tokio::time::timeout(
            Duration::from_secs(3),
            tokio_tungstenite::connect_async(server.url()),
        )
        .await;
        assert!(
            matches!(fourth, Ok(Ok(_))),
            "a slot freed by a completed auth must admit a new connection, got {fourth:?}"
        );

        drop(ws2);
    });
}
