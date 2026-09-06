//! End-to-end integration test for the `join`/`join_ok` wire frames
//! (ADR 0015, `docs/protocol/v1.md` §4.1): the real `holler-server serve`
//! listener, driven by a real `tokio-tungstenite` WebSocket client
//! through `connect -> join -> join_ok -> (server closes)`, plus the
//! fail-closed paths (wrong secret, already-bound token) and the
//! "first frame decides, no mixing" invariant (a `join`-then-`auth` on
//! the same socket cannot work because the socket is already closed).
//!
//! Mirrors `tests/wire_first_talk_test.rs`'s process-harness pattern:
//! a real `holler-server serve` child on an ephemeral loopback port, `holler
//! token mint` for the fixture secret, and a real WebSocket client.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

fn holler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_holler-server"))
}

/// A fresh, isolated state dir + pepper per test — mirrors
/// `tests/wire_first_talk_test.rs`'s `Env` so parallel tests never
/// share a token store or race on process env.
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
            .env("HOLLER_SERVER_PEPPER", "wire-join-test-pepper");
        cmd
    }
}

/// A running `holler-server serve` child on an OS-assigned loopback port,
/// killed on drop so a failing assertion never leaks the process.
struct ServerProcess {
    child: Child,
    port: u16,
}

impl ServerProcess {
    fn spawn(env: &Env) -> Self {
        let mut cmd = env.cmd();
        cmd.args(["serve", "--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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

fn join_envelope(token_id: &str, secret: &str, hostname: &str) -> Value {
    json!({
        "v": 1, "type": "join", "id": "id-join", "ts": "2026-09-06T00:00:00Z",
        "from": token_id, "body": { "secret": secret, "hostname": hostname }
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

/// After `join_ok`/`error`, the server must actually close the socket —
/// the next read either sees a `Close` frame or the stream ending
/// (`None`), never another data frame.
async fn assert_socket_closes(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) {
    let outcome = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("the server closes within 5s");
    match outcome {
        None => {}
        Some(Ok(Message::Close(_))) => {}
        other => panic!("expected the connection to close, got {other:?}"),
    }
}

#[test]
fn join_with_correct_secret_succeeds_and_server_closes_the_connection() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .expect("client connects to the real listener");

        send_json(&mut ws, &join_envelope(&token_id, &secret, "kiwi")).await;

        let reply = recv_json(&mut ws).await;
        assert_eq!(reply["type"], "join_ok");
        assert_eq!(reply["id"], "id-join");
        assert_eq!(reply["from"], "server");
        let client_id = reply["body"]["client_id"]
            .as_str()
            .expect("join_ok carries a client_id");
        let credential = reply["body"]["credential"]
            .as_str()
            .expect("join_ok carries a credential");
        assert!(client_id.starts_with("cli_"));
        assert!(credential.starts_with("hlr_live_"));

        assert_socket_closes(&mut ws).await;
    });

    // The token store recorded the bind: `holler-server token list` should now
    // show this token as `bound`, proving `redeem` actually ran (not
    // just that the wire replied with a plausible-looking body).
    let list_out = env.cmd().args(["token", "list"]).output().unwrap();
    assert!(list_out.status.success(), "{list_out:?}");
    let list_stdout = String::from_utf8(list_out.stdout).unwrap();
    assert!(list_stdout.contains(&token_id));
    assert!(list_stdout.contains("bound"));
}

#[test]
fn join_reconnect_via_auth_then_works() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let credential = rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws, &join_envelope(&token_id, &secret, "kiwi")).await;
        let reply = recv_json(&mut ws).await;
        assert_eq!(reply["type"], "join_ok");
        reply["body"]["credential"].as_str().unwrap().to_string()
    });

    // The client persists the credential and reconnects via the
    // ordinary `auth` -> `hello` flow (ADR 0015) — a fresh connection,
    // not a continuation of the `join` socket.
    rt.block_on(async {
        let (mut ws2, _resp2) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(
            &mut ws2,
            &json!({
                "v": 1, "type": "auth", "id": "id-auth", "ts": "2026-09-06T00:00:00Z",
                "from": token_id, "body": { "credential": credential }
            }),
        )
        .await;
        let hello = recv_json(&mut ws2).await;
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["from"], "server");
    });
}

#[test]
fn join_with_wrong_secret_fails_closed_and_does_not_mutate_the_token() {
    let env = Env::new();
    let (token_id, _secret) = mint(&env, "kiwi");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();

        send_json(
            &mut ws,
            &join_envelope(&token_id, "hlr_not-the-real-secret", "kiwi"),
        )
        .await;

        let err = recv_json(&mut ws).await;
        assert_eq!(err["type"], "error");
        assert_eq!(err["body"]["code"], "join_failed");
        assert!(err["body"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()));

        assert_socket_closes(&mut ws).await;
    });

    // Not mutated: the token is still `unused` and can be joined for
    // real afterwards.
    let list_out = env.cmd().args(["token", "list"]).output().unwrap();
    assert!(list_out.status.success(), "{list_out:?}");
    let list_stdout = String::from_utf8(list_out.stdout).unwrap();
    assert!(list_stdout.contains("unused"));
}

#[test]
fn join_on_an_already_bound_token_fails_with_join_failed() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // First join: succeeds and binds the token.
        let (mut ws1, _resp1) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws1, &join_envelope(&token_id, &secret, "kiwi")).await;
        let first = recv_json(&mut ws1).await;
        assert_eq!(first["type"], "join_ok");
        assert_socket_closes(&mut ws1).await;

        // Second join attempt with the same (now-consumed) secret, on a
        // brand-new connection, must fail — the secret is single-use.
        let (mut ws2, _resp2) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws2, &join_envelope(&token_id, &secret, "kiwi")).await;
        let second = recv_json(&mut ws2).await;
        assert_eq!(second["type"], "error");
        assert_eq!(second["body"]["code"], "join_failed");

        assert_socket_closes(&mut ws2).await;
    });
}

#[test]
fn join_then_auth_on_the_same_socket_does_not_work() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws, &join_envelope(&token_id, &secret, "kiwi")).await;
        let reply = recv_json(&mut ws).await;
        assert_eq!(reply["type"], "join_ok");
        let credential = reply["body"]["credential"].as_str().unwrap().to_string();

        // The server has already closed (or is closing) the connection
        // after `join_ok` — sending `auth` on the SAME socket must not
        // yield a working session. Either the send itself fails (socket
        // already gone) or, if the write still succeeds because the
        // close hasn't propagated to this side yet, no `hello` ever
        // arrives before the socket closes for real.
        let send_result = ws
            .send(Message::Text(
                json!({
                    "v": 1, "type": "auth", "id": "id-auth-same-socket",
                    "ts": "2026-09-06T00:00:00Z",
                    "from": token_id, "body": { "credential": credential }
                })
                .to_string()
                .into(),
            ))
            .await;

        if send_result.is_ok() {
            let outcome = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
            match outcome {
                Err(_) => panic!("neither a reply nor a close arrived within 5s"),
                Ok(None) => {}
                Ok(Some(Ok(Message::Close(_)))) => {}
                Ok(Some(other)) => panic!(
                    "expected the closed `join` socket to yield no session, got {other:?}"
                ),
            }
        }
    });
}
