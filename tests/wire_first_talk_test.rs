//! End-to-end integration test for issue #31 ("first talk"): the real
//! `holler serve` listener, driven by a real `tokio-tungstenite`
//! WebSocket client through `auth -> hello -> query status -> ping`,
//! plus the CLI's own `token ping` / `status` reaching the live process
//! over its local control channel, and the fail-closed paths (bad
//! credential, wrong first frame, unknown `query` cmd, non-loopback
//! bind).
//!
//! Distinct from `tests/wire/` (owned by the wire-harness story,
//! issues #40/#41) — this drives the real `holler-server` binary's wire
//! behavior directly, on its own ephemeral port, so it cannot collide
//! with that harness's fixed-port assumptions.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

fn holler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_holler"))
}

/// A fresh, isolated state dir + pepper per test — mirrors
/// `tests/token_cli_test.rs`'s `Env` so parallel tests never share a
/// token store or race on process env.
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
            .env("HOLLER_SERVER_PEPPER", "wire-test-pepper");
        cmd
    }
}

/// A running `holler serve` child on an OS-assigned loopback port,
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
        let mut child = cmd.spawn().expect("spawn `holler serve`");
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
        let line = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("`holler serve` printed its listening line within 5s");
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
            "token",
            "redeem",
            token_id,
            "--secret",
            secret,
            "--machine",
            machine,
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

fn client_hello_envelope(token_id: &str, client_id: &str) -> Value {
    json!({
        "v": 1, "type": "hello", "id": "id-hello", "ts": "2026-09-05T00:00:00Z",
        "from": token_id,
        "body": {
            "protocol": 1, "protocol_min": 1, "protocol_max": 1,
            "role": "client", "hostname": "kiwi", "token_id": token_id,
            "client_id": client_id, "features": [], "harnesses": [], "sessions": []
        }
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

#[test]
fn first_talk_auth_hello_status_ping_and_cli_probe() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (client_id, credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .expect("client connects to the real listener");

        send_json(&mut ws, &auth_envelope(&token_id, &credential)).await;

        // Mutual hello: the server hellos first (spec §4: "each side
        // sends hello"), unprompted by the client's own hello.
        let server_hello = recv_json(&mut ws).await;
        assert_eq!(server_hello["type"], "hello");
        assert_eq!(server_hello["from"], "server");
        assert_eq!(server_hello["body"]["role"], "server");
        assert_eq!(server_hello["body"]["protocol"], 1);
        assert!(server_hello["body"]["hostname"]
            .as_str()
            .is_some_and(|h| !h.is_empty()));

        send_json(&mut ws, &client_hello_envelope(&token_id, &client_id)).await;

        // `query status` answers with a `query_ok` carrying this live
        // connection in `clients`.
        send_json(
            &mut ws,
            &json!({
                "v": 1, "type": "query", "id": "id-status", "ts": "2026-09-05T00:00:00Z",
                "from": token_id, "body": { "cmd": "status", "args": [] }
            }),
        )
        .await;
        let status = recv_json(&mut ws).await;
        assert_eq!(status["type"], "query_ok");
        assert_eq!(status["id"], "id-status");
        assert_eq!(status["body"]["cmd"], "status");
        assert_eq!(status["body"]["role"], "server");
        assert_eq!(status["body"]["clients"], 1);
        assert!(status["body"]["listening"]
            .as_array()
            .is_some_and(|l| l.iter().any(|v| v.as_str() == Some(server.url().as_str()))));

        // Client-initiated `ping` / `pong` (reuses the request id).
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
        assert_eq!(pong["id"], "id-ping");

        // From here on, the server may independently push a `ping` at
        // this connection (that is exactly how `holler token ping`'s
        // RTT round trip works — see `wire::registry::Registry::ping`).
        // A real client answers with a matching `pong`; this test client
        // needs the same auto-responder, running concurrently with the
        // blocking CLI subprocess calls below.
        let (mut write, mut read) = ws.split();
        let responder_token_id = token_id.clone();
        let responder = tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if v["type"] == "ping" {
                    let pong = json!({
                        "v": 1, "type": "pong", "id": v["id"], "ts": "2026-09-05T00:00:00Z",
                        "from": responder_token_id, "body": {}
                    });
                    if write
                        .send(Message::Text(pong.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });

        // `holler token ping <id>`, run as a SEPARATE process, reaches
        // the live server over the control channel and reports the
        // real hostname + RTT — this is the acceptance criterion the
        // old `AlwaysDisconnected` probe could never satisfy.
        let ping_out = env
            .cmd()
            .args(["token", "ping", &token_id])
            .output()
            .unwrap();
        assert!(ping_out.status.success(), "{ping_out:?}");
        let ping_stdout = String::from_utf8(ping_out.stdout).unwrap();
        // The registry's hostname was refreshed by the client `hello`
        // above (`kiwi`), which supersedes the redeem-time `machine`
        // name (`kiwi.local`) — the live hello is the more current
        // source of truth once a session has one.
        assert!(ping_stdout.contains("kiwi"), "{ping_stdout:?}");
        assert!(ping_stdout.contains("rtt="), "{ping_stdout:?}");

        // `holler status`, also a separate process, reports healthy
        // with `role: server` and the live client counted.
        let status_out = env.cmd().args(["status", "--json"]).output().unwrap();
        assert!(status_out.status.success(), "{status_out:?}");
        let status_json: Value = serde_json::from_slice(&status_out.stdout).unwrap();
        assert_eq!(status_json["role"], "server");
        assert_eq!(status_json["clients"], 1);

        // Actually close the socket (not just stop answering) so the
        // disconnect assertion below observes a real closed connection.
        responder.abort();
        let _ = responder.await;
    });

    // After the socket actually closes, `token ping` must report the
    // token bound-but-disconnected, not silently keep reporting stale
    // liveness.
    std::thread::sleep(Duration::from_millis(200));
    let ping_after = env
        .cmd()
        .args(["token", "ping", &token_id])
        .output()
        .unwrap();
    assert!(!ping_after.status.success());
    let stderr = String::from_utf8(ping_after.stderr).unwrap();
    assert!(
        stderr.to_lowercase().contains("not connected"),
        "{stderr:?}"
    );
}

#[test]
fn auth_with_wrong_credential_is_rejected_and_server_stays_up() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (_client_id, _credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(
            &mut ws,
            &auth_envelope(&token_id, "hlr_live_not-the-real-one"),
        )
        .await;

        let err = recv_json(&mut ws).await;
        assert_eq!(err["type"], "error");
        assert_eq!(err["body"]["code"], "unauthenticated");

        // The rejected connection must not take the server down: a
        // second, correctly authenticated connection still works.
        let (second_token_id, second_secret) = mint(&env, "second");
        let (_second_client_id, second_credential) =
            redeem(&env, &second_token_id, &second_secret, "second.local");
        let (mut ws2, _resp2) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(
            &mut ws2,
            &auth_envelope(&second_token_id, &second_credential),
        )
        .await;
        let hello = recv_json(&mut ws2).await;
        assert_eq!(hello["type"], "hello");
    });
}

#[test]
fn first_frame_must_be_auth() {
    let env = Env::new();
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(
            &mut ws,
            &json!({
                "v": 1, "type": "hello", "id": "id-1", "ts": "2026-09-05T00:00:00Z",
                "from": "tok_whatever",
                "body": {
                    "protocol": 1, "protocol_min": 1, "protocol_max": 1,
                    "role": "client", "hostname": "kiwi"
                }
            }),
        )
        .await;
        let err = recv_json(&mut ws).await;
        assert_eq!(err["type"], "error");
        assert_eq!(err["body"]["code"], "unauthenticated");
    });
}

#[test]
fn unknown_query_cmd_gets_unknown_cmd_error() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (client_id, credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws, &auth_envelope(&token_id, &credential)).await;
        let _server_hello = recv_json(&mut ws).await;
        send_json(&mut ws, &client_hello_envelope(&token_id, &client_id)).await;

        send_json(
            &mut ws,
            &json!({
                "v": 1, "type": "query", "id": "id-bogus", "ts": "2026-09-05T00:00:00Z",
                "from": token_id, "body": { "cmd": "summarize", "args": [] }
            }),
        )
        .await;
        let err = recv_json(&mut ws).await;
        assert_eq!(err["type"], "error");
        assert_eq!(err["body"]["code"], "unknown_cmd");
        assert_eq!(err["body"]["cmd"], "summarize");
    });
}

#[test]
fn non_loopback_bind_is_refused() {
    let env = Env::new();
    let out = env
        .cmd()
        .args(["serve", "--listen", "0.0.0.0:0"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap().to_lowercase();
    assert!(
        stderr.contains("wss") || stderr.contains("tls"),
        "{stderr:?}"
    );
}

#[test]
fn ping_on_a_never_redeemed_token_is_unbound() {
    let env = Env::new();
    let (token_id, _secret) = mint(&env, "kiwi");
    let out = env
        .cmd()
        .args(["token", "ping", &token_id])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap().to_lowercase();
    assert!(
        stderr.contains("not been redeemed")
            || stderr.contains("unbound")
            || stderr.contains("nothing to ping")
    );
}

#[test]
fn status_with_no_server_running_reports_not_running() {
    let env = Env::new();
    let out = env.cmd().args(["status", "--json"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["role"], "server");
    assert_eq!(doc["clients"], 0);
    assert_eq!(doc["listening"].as_array().unwrap().len(), 0);
}
