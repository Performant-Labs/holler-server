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

fn presence_envelope(token_id: &str, sessions: Value) -> Value {
    json!({
        "v": 1, "type": "presence", "id": "id-presence", "ts": "2026-09-05T00:00:00Z",
        "from": token_id, "body": { "sessions": sessions }
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
fn repeated_bad_credential_attempts_from_one_source_get_locked_out() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (_client_id, _credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // The first `MAX_FAILURES` bad-credential attempts each fail the
        // normal way: a real WebSocket handshake completes and the server
        // replies with an `unauthenticated` error frame before closing.
        for _ in 0..holler_server::wire::lockout::MAX_FAILURES {
            let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
                .await
                .expect("connection succeeds before this source is locked out");
            send_json(
                &mut ws,
                &auth_envelope(&token_id, "hlr_live_not-the-real-one"),
            )
            .await;
            let err = recv_json(&mut ws).await;
            assert_eq!(err["type"], "error");
            assert_eq!(err["body"]["code"], "unauthenticated");
        }

        // The next connection attempt from the same source (127.0.0.1, the
        // only address a loopback test client can dial from) is refused
        // before the WebSocket handshake even starts (issue #58): the
        // server drops the raw TCP connection outright, so the client-side
        // handshake itself fails rather than succeeding and then getting
        // an `error` frame like the attempts above did.
        let result = tokio_tungstenite::connect_async(server.url()).await;
        assert!(
            result.is_err(),
            "a locked-out source's connection should fail the WebSocket \
             handshake itself, not just receive an `error` frame"
        );

        // A brand-new, never-before-used token from the SAME source is
        // also refused — the lockout is keyed by peer IP, not by
        // `token_id`, so cycling tokens does not evade it.
        let (other_token_id, other_secret) = mint(&env, "other");
        let (_other_client_id, _other_credential) =
            redeem(&env, &other_token_id, &other_secret, "other.local");
        let result2 = tokio_tungstenite::connect_async(server.url()).await;
        assert!(result2.is_err());
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

// ---------------------------------------------------------------------
// Capability query (issue #37): `holler status/support/caps/query <id>`
// relayed to a live client, with a scripted responder standing in for
// that client's own dispatcher.
// ---------------------------------------------------------------------

/// Build the `query_ok`/`error` a scripted test client answers with for
/// one `query` envelope, mirroring the shapes `docs/protocol/v1.md` §7
/// specifies (this is *not* the real client dispatcher — just enough to
/// exercise the server's outbound relay end to end).
fn scripted_query_reply(token_id: &str, request: &Value) -> Value {
    let id = request["id"].clone();
    let cmd = request["body"]["cmd"].as_str().unwrap_or("");
    let args: Vec<String> = request["body"]["args"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let body = match cmd {
        "status" => json!({
            "cmd": "status", "role": "client", "protocol": 1, "protocol_min": 1,
            "protocol_max": 1, "hostname": "kiwi", "connected": true,
            "token_id": token_id, "features": [], "harnesses": [], "sessions": [],
        }),
        "caps" => json!({
            "cmd": "caps", "role": "client", "protocol": 1, "hostname": "kiwi",
            "connected": true, "capabilities": {},
        }),
        "support" => {
            let feature = args.first().cloned().unwrap_or_default();
            let ok = feature == "opencode";
            json!({ "cmd": "support", "args": [feature], "ok": ok, "feature": feature, "kind": "harness" })
        }
        "protocol" => match args.first() {
            Some(raw) => {
                let asked: u32 = raw.parse().unwrap_or(0);
                json!({
                    "cmd": "protocol", "args": [raw], "ok": asked == 1, "asked": asked,
                    "session": 1, "min": 1, "max": 1,
                })
            }
            None => json!({ "cmd": "protocol", "session": 1, "min": 1, "max": 1 }),
        },
        other => {
            return json!({
                "v": 1, "type": "error", "id": id, "ts": "2026-09-05T00:00:00Z", "from": token_id,
                "body": { "code": "unknown_cmd", "cmd": other, "message": format!("unknown query cmd: {other}") }
            });
        }
    };
    json!({
        "v": 1, "type": "query_ok", "id": id, "ts": "2026-09-05T00:00:00Z", "from": token_id,
        "body": body,
    })
}

/// Answer every `ping` (so `holler token ping` keeps working) and
/// `query` (via [`scripted_query_reply`]) the server sends this
/// connection, until the socket closes or is aborted.
fn spawn_scripted_responder(
    mut write: impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin + Send + 'static,
    mut read: impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin + Send + 'static,
    token_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = read.next().await {
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let reply = match v["type"].as_str() {
                Some("ping") => json!({
                    "v": 1, "type": "pong", "id": v["id"], "ts": "2026-09-05T00:00:00Z", "from": token_id,
                    "body": {}
                }),
                Some("query") => scripted_query_reply(&token_id, &v),
                _ => continue,
            };
            if write
                .send(Message::Text(reply.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    })
}

#[test]
fn remote_query_relay_covers_status_support_caps_protocol() {
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
        let (write, read) = ws.split();
        let responder = spawn_scripted_responder(write, read, token_id.clone());

        // `holler status <id>` relays `query status` and reports the
        // remote client's own document, not the server's.
        let out = env
            .cmd()
            .args(["status", &token_id, "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["role"], "client");
        assert_eq!(doc["hostname"], "kiwi");

        // `holler support <id> <feature>`: both `ok: true` and
        // `ok: false` round trip.
        let out = env
            .cmd()
            .args(["support", &token_id, "opencode", "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["ok"], true);
        assert_eq!(doc["feature"], "opencode");

        let out = env
            .cmd()
            .args(["support", &token_id, "claude", "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["ok"], false);

        // `holler caps <id>`.
        let out = env
            .cmd()
            .args(["caps", &token_id, "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["role"], "client");

        // `holler query <id> protocol` and `holler query <id> protocol <n>`.
        let out = env
            .cmd()
            .args(["query", &token_id, "protocol", "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["min"], 1);
        assert_eq!(doc["max"], 1);
        assert!(doc.get("ok").is_none());

        let out = env
            .cmd()
            .args(["query", &token_id, "protocol", "2", "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["ok"], false);
        assert_eq!(doc["asked"], 2);

        responder.abort();
        let _ = responder.await;
    });
}

#[test]
fn remote_query_unknown_cmd_is_relayed_as_a_failure() {
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
        let (write, read) = ws.split();
        let responder = spawn_scripted_responder(write, read, token_id.clone());

        // The remote client's own `unknown_cmd` answer must surface as
        // a failure on the CLI side, not a silent success.
        let out = env
            .cmd()
            .args(["query", &token_id, "summarize"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{out:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains("unknown_cmd"), "{stderr:?}");

        responder.abort();
        let _ = responder.await;
    });
}

#[test]
fn local_query_unknown_cmd_is_fail_closed() {
    let env = Env::new();
    let _server = ServerProcess::spawn(&env);

    let out = env.cmd().args(["query", "summarize"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("unknown_cmd"), "{stderr:?}");
}

#[test]
fn query_to_an_unbound_token_is_an_error() {
    let env = Env::new();
    let (token_id, _secret) = mint(&env, "kiwi");
    let _server = ServerProcess::spawn(&env);

    let out = env
        .cmd()
        .args(["status", &token_id, "--json"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("no live connection"), "{stderr:?}");
}

#[test]
fn query_to_a_disconnected_client_is_an_error() {
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
        // Actually close the socket rather than just going quiet, so
        // the server's registry has really dropped this connection.
        ws.close(None).await.unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));

    let out = env
        .cmd()
        .args(["status", &token_id, "--json"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("no live connection"), "{stderr:?}");
}

// ---------------------------------------------------------------------
// Route prompt/reply by session name (issue #33): `holler say <session>`
// against a real ACP-stub-like WS client that advertises via `presence`
// then answers a `prompt` with a `reply`.
// ---------------------------------------------------------------------

/// Answer every `ping` (so the connection stays alive under the
/// server's own liveness checks) and every `prompt` addressed to
/// `session` with a single `done: true` `reply` echoing `text` back
/// with a `" (heard)"` suffix — standing in for a real ACP-backed
/// client's session output, until the socket closes or is aborted.
fn spawn_prompt_responder(
    mut write: impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin + Send + 'static,
    mut read: impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin + Send + 'static,
    token_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = read.next().await {
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let reply = match v["type"].as_str() {
                Some("ping") => json!({
                    "v": 1, "type": "pong", "id": v["id"], "ts": "2026-09-05T00:00:00Z", "from": token_id,
                    "body": {}
                }),
                Some("prompt") => {
                    let session = v["body"]["session"].as_str().unwrap_or_default();
                    let heard = format!("{} (heard)", v["body"]["text"].as_str().unwrap_or_default());
                    json!({
                        "v": 1, "type": "reply", "id": v["id"], "ts": "2026-09-05T00:00:00Z", "from": token_id,
                        "body": { "session": session, "text": heard, "done": true, "exit": 0 }
                    })
                }
                _ => continue,
            };
            if write
                .send(Message::Text(reply.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    })
}

#[test]
fn say_routes_a_prompt_by_session_name_and_persists_the_talk_log() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (client_id, credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    // `#[allow]`: the async block's own tail expression is a
    // `JoinHandle` (itself awaitable) — clippy's `async_yields_async`
    // flags that shape, but here it is exactly the point: we want the
    // handle back, not to await the spawned task in place.
    #[allow(clippy::async_yields_async)]
    let responder = rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws, &auth_envelope(&token_id, &credential)).await;
        let _server_hello = recv_json(&mut ws).await;
        send_json(&mut ws, &client_hello_envelope(&token_id, &client_id)).await;

        // Advertise `alpha` (ADR 0006/0007), the ACP-stub-standin's one
        // hosted session, then start answering prompts.
        send_json(
            &mut ws,
            &presence_envelope(
                &token_id,
                json!([{ "name": "alpha", "harness": "opencode" }]),
            ),
        )
        .await;
        // Give the server a moment to have processed the `presence`
        // before `holler say` (below, a separate process) races it.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (write, read) = ws.split();
        spawn_prompt_responder(write, read, token_id.clone())
    });

    // `holler say alpha "hello"`, a separate CLI process, resolves
    // `alpha` via the live roster and gets the routed `reply` back.
    let say_out = env
        .cmd()
        .args(["say", "alpha", "hello"])
        .output()
        .unwrap();
    assert!(say_out.status.success(), "{say_out:?}");
    let say_stdout = String::from_utf8(say_out.stdout).unwrap();
    assert_eq!(say_stdout.trim(), "hello (heard)");

    // A name the roster has never heard of fails closed with
    // `unknown_session`, not a hang or a silent no-op.
    let unknown_out = env
        .cmd()
        .args(["say", "nope", "hi"])
        .output()
        .unwrap();
    assert!(!unknown_out.status.success());
    let unknown_stderr = String::from_utf8(unknown_out.stderr).unwrap();
    assert!(unknown_stderr.contains("unknown_session"), "{unknown_stderr:?}");

    // The exchange is durably persisted (issue #33's "enough talk log
    // for `holler wait`") — read it back directly rather than trusting
    // the write happened.
    let talklog = holler_server::wire::talklog::TalkLog::new(env.dir.path().to_path_buf());
    let entries = talklog.read("alpha");
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].session, "alpha");
    assert_eq!(entries[0].prompt_text, "hello");
    assert_eq!(entries[0].replies.len(), 1, "{entries:?}");
    assert_eq!(entries[0].replies[0].text.as_deref(), Some("hello (heard)"));
    assert!(entries[0].replies[0].done);
    assert_eq!(entries[0].replies[0].exit, Some(0));

    rt.block_on(async {
        responder.abort();
        let _ = responder.await;
    });
}

// ---------------------------------------------------------------------
// Interrupt control path (issue #34, ADR 0005): `holler interrupt
// <session>` against a real WS client that advertises two sibling
// sessions on one connection, then answers (or withholds) `ack`.
// ---------------------------------------------------------------------

/// Answer every `ping` normally. For `interrupt`, only reply with an
/// `ack` (`of` echoing the interrupt frame's id) when the body's
/// `session` is in `ack_sessions` — lets a test exercise "acked" and
/// "silently withheld" on two sibling sessions of the very same
/// connection. Every `interrupt` seen is also pushed to `seen`, a shared
/// collector the test reads back *after* aborting the task — a
/// `JoinHandle`'s own return value is lost on abort, so the record has to
/// live outside it.
fn spawn_interrupt_responder(
    mut write: impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin
        + Send
        + 'static,
    mut read: impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
    token_id: String,
    ack_sessions: Vec<&'static str>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = read.next().await {
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            match v["type"].as_str() {
                Some("ping") => {
                    let pong = json!({
                        "v": 1, "type": "pong", "id": v["id"], "ts": "2026-09-05T00:00:00Z",
                        "from": token_id, "body": {}
                    });
                    if write
                        .send(Message::Text(pong.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Some("interrupt") => {
                    seen.lock().unwrap().push(v.clone());
                    let session = v["body"]["session"].as_str().unwrap_or_default();
                    if ack_sessions.contains(&session) {
                        let ack = json!({
                            "v": 1, "type": "ack", "id": v["id"], "ts": "2026-09-05T00:00:00Z",
                            "from": token_id, "body": { "of": v["id"] }
                        });
                        if write
                            .send(Message::Text(ack.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    // Sessions not in `ack_sessions` are deliberately left
                    // hanging — no ack, no error — to exercise the
                    // "connection alive, no ack in time" timeout path.
                }
                _ => continue,
            }
        }
    })
}

#[test]
fn interrupt_acks_and_scopes_to_the_named_session_only() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (client_id, credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    let server = ServerProcess::spawn(&env);

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let rt = tokio::runtime::Runtime::new().unwrap();
    #[allow(clippy::async_yields_async)]
    let responder = rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws, &auth_envelope(&token_id, &credential)).await;
        let _server_hello = recv_json(&mut ws).await;
        send_json(&mut ws, &client_hello_envelope(&token_id, &client_id)).await;

        // Two sibling sessions on one connection (ADR 0007) — only
        // `alpha` will ever be interrupted in this test.
        send_json(
            &mut ws,
            &presence_envelope(
                &token_id,
                json!([
                    { "name": "alpha", "harness": "opencode" },
                    { "name": "beta", "harness": "opencode" }
                ]),
            ),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (write, read) = ws.split();
        spawn_interrupt_responder(write, read, token_id.clone(), vec!["alpha"], seen.clone())
    });

    // `holler interrupt alpha`, a separate CLI process, is acked.
    let out = env.cmd().args(["interrupt", "alpha"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("alpha"), "{stdout:?}");

    // The session survives on the roster (ADR 0005: "the session
    // stays") and stays promptable/interruptible — a second interrupt
    // to the SAME session still works.
    let out2 = env.cmd().args(["interrupt", "alpha"]).output().unwrap();
    assert!(out2.status.success(), "{out2:?}");

    // A name the roster has never heard of fails closed.
    let unknown_out = env.cmd().args(["interrupt", "nope"]).output().unwrap();
    assert!(!unknown_out.status.success());
    let unknown_stderr = String::from_utf8(unknown_out.stderr).unwrap();
    assert!(
        unknown_stderr.contains("unknown_session"),
        "{unknown_stderr:?}"
    );

    rt.block_on(async {
        responder.abort();
        let _ = responder.await;
    });
    // Assert per-session scoping directly against what the responder
    // actually saw: `beta` (the sibling session on this same connection)
    // must never have received an `interrupt`.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "{seen:?}");
    for envelope in seen.iter() {
        assert_eq!(envelope["body"]["session"], "alpha", "{seen:?}");
    }
}

#[test]
fn interrupt_reports_timed_out_when_the_connection_never_acks() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (client_id, credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    #[allow(clippy::async_yields_async)]
    let responder = rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws, &auth_envelope(&token_id, &credential)).await;
        let _server_hello = recv_json(&mut ws).await;
        send_json(&mut ws, &client_hello_envelope(&token_id, &client_id)).await;
        send_json(
            &mut ws,
            &presence_envelope(
                &token_id,
                json!([{ "name": "alpha", "harness": "opencode" }]),
            ),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (write, read) = ws.split();
        // No session ever gets an ack.
        spawn_interrupt_responder(
            write,
            read,
            token_id.clone(),
            vec![],
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        )
    });

    // The connection stays open and answers `ping` throughout, but never
    // acks — this must be reported distinctly from "not connected"
    // (issue #54), and the CLI call itself must still return (bounded by
    // `INTERRUPT_ACK_TIMEOUT`), not hang.
    let out = env.cmd().args(["interrupt", "alpha"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap().to_lowercase();
    assert!(
        stderr.contains("may not have landed") && !stderr.contains("gone"),
        "{stderr:?}"
    );

    rt.block_on(async {
        responder.abort();
        let _ = responder.await;
    });
}

#[test]
fn interrupt_reports_disconnected_when_the_connection_is_already_gone() {
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
            &presence_envelope(
                &token_id,
                json!([{ "name": "alpha", "harness": "opencode" }]),
            ),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Actually close the socket, same as `query_to_a_disconnected_client_is_an_error`.
        ws.close(None).await.unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));

    // This must fail quickly (well under `INTERRUPT_ACK_TIMEOUT`) and
    // with a message distinct from the timeout case above. Since issue
    // #80, the explicit close above marks the roster row `gone`
    // immediately, so this now fails at the roster lookup itself
    // (`unknown_session`) rather than reaching the registry and finding
    // no live connection there (`InterruptOutcome::Disconnected`) — both
    // are "the session is definitely gone," just surfaced a step
    // earlier now that the roster knows it with certainty too.
    let started = std::time::Instant::now();
    let out = env.cmd().args(["interrupt", "alpha"]).output().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a dead connection must be reported promptly, not after the ack timeout"
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap().to_lowercase();
    assert!(
        stderr.contains("unknown session") || stderr.contains("gone"),
        "{stderr:?}"
    );
}

// ---------------------------------------------------------------------
// A `say` racing an `interrupt` for the very same session, from a
// *different* CLI invocation (issue #82): the `say` process must report
// its own turn was interrupted, not the misleading "no live `holler
// serve` process is reachable" it used to print when the in-flight
// `prompt` it was waiting on got cut out from under it elsewhere. The
// server (and the connection) are alive throughout.
// ---------------------------------------------------------------------

/// Answer `ping` normally, `interrupt` with an immediate `ack`, and
/// silently withhold any `reply` to `prompt` — standing in for a real
/// model turn still in flight when a separate `holler interrupt` cancels
/// it out from under the `holler say` that is waiting on it.
fn spawn_prompt_withholding_interrupt_responder(
    mut write: impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin
        + Send
        + 'static,
    mut read: impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
    token_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = read.next().await {
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            match v["type"].as_str() {
                Some("ping") => {
                    let pong = json!({
                        "v": 1, "type": "pong", "id": v["id"], "ts": "2026-09-05T00:00:00Z",
                        "from": token_id, "body": {}
                    });
                    if write
                        .send(Message::Text(pong.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Some("interrupt") => {
                    let ack = json!({
                        "v": 1, "type": "ack", "id": v["id"], "ts": "2026-09-05T00:00:00Z",
                        "from": token_id, "body": { "of": v["id"] }
                    });
                    if write
                        .send(Message::Text(ack.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                // `prompt` is deliberately left unanswered — the point of
                // this responder.
                _ => continue,
            }
        }
    })
}

#[test]
fn say_reports_interrupted_not_no_live_server_when_cancelled_mid_flight() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (client_id, credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    #[allow(clippy::async_yields_async)]
    let responder = rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws, &auth_envelope(&token_id, &credential)).await;
        let _server_hello = recv_json(&mut ws).await;
        send_json(&mut ws, &client_hello_envelope(&token_id, &client_id)).await;
        send_json(
            &mut ws,
            &presence_envelope(
                &token_id,
                json!([{ "name": "alpha", "harness": "opencode" }]),
            ),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (write, read) = ws.split();
        spawn_prompt_withholding_interrupt_responder(write, read, token_id.clone())
    });

    // `holler say alpha "<long prompt>"`, a separate CLI process, starts
    // waiting on a reply this responder will never send.
    let say_child = env
        .cmd()
        .args(["say", "alpha", "this prompt takes a while"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `holler say`");

    // Give the server a moment to register the prompt as pending before
    // a *different* CLI invocation interrupts the very same session.
    std::thread::sleep(Duration::from_millis(300));
    let interrupt_out = env.cmd().args(["interrupt", "alpha"]).output().unwrap();
    assert!(interrupt_out.status.success(), "{interrupt_out:?}");

    // The interrupt is acked well inside `CLIENT_TIMEOUT` (5s): `say`
    // must come back promptly with the new, accurate message rather than
    // idling out to its own 5s control-socket read timeout and reporting
    // the misleading "no live server" (the pre-fix behavior this test
    // guards against — the generous bound below still catches that
    // regression, just more slowly).
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = say_child.wait_with_output().expect("wait on `holler say`");
        let _ = tx.send(out);
    });
    let say_out = rx
        .recv_timeout(Duration::from_secs(7))
        .expect("`holler say` must return once its session's interrupt is acked, not hang");

    assert!(!say_out.status.success(), "{say_out:?}");
    let say_stderr = String::from_utf8(say_out.stderr).unwrap();
    assert!(
        say_stderr.contains("interrupted before it completed"),
        "{say_stderr:?}"
    );
    assert!(
        !say_stderr.contains("no live"),
        "must not report the misleading \"no live server\" message: {say_stderr:?}"
    );

    rt.block_on(async {
        responder.abort();
        let _ = responder.await;
    });
}

// ---------------------------------------------------------------------
// Revoke force-closes the live connection (issue #78): `holler client
// detach` / `token delete` correctly flip the on-disk record to
// `revoked`, but before this fix left the live WebSocket connection (and
// `holler status`/`roster`'s view of it) untouched. These exercise the
// new control-channel `Revoke` request end to end against a real
// connection.
// ---------------------------------------------------------------------

#[test]
fn client_detach_force_closes_the_live_connection() {
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

        // `holler client detach <id>`, a separate CLI process, revokes
        // the token on disk AND reaches this live `holler serve` process
        // over the control channel to force-close the connection — not
        // just stop the client from reconnecting later.
        let detach_out = env
            .cmd()
            .args(["client", "detach", &token_id])
            .output()
            .unwrap();
        assert!(detach_out.status.success(), "{detach_out:?}");
        let detach_stdout = String::from_utf8(detach_out.stdout).unwrap();
        assert!(detach_stdout.contains("revoked"), "{detach_stdout:?}");

        // The live connection actually closes, from the client's own
        // point of view — not just that the CLI printed success and the
        // on-disk record flipped to `revoked`. The server drops the
        // socket outright (no close handshake), so this may surface as
        // an `Ok(None)` (clean EOF) or a websocket-level error depending
        // on OS timing — either is "closed".
        let closed = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("the connection closes within 5s");
        assert!(
            matches!(closed, None | Some(Err(_)) | Some(Ok(Message::Close(_)))),
            "expected the connection to close, got {closed:?}"
        );
    });

    // `holler status` no longer counts this client as connected.
    let status_out = env.cmd().args(["status", "--json"]).output().unwrap();
    assert!(status_out.status.success(), "{status_out:?}");
    let status_json: Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(status_json["clients"], 0);
}

#[test]
fn client_detach_with_no_live_server_is_not_an_error() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (_client_id, _credential) = redeem(&env, &token_id, &secret, "kiwi.local");

    // No `holler serve` process running at all — the control channel is
    // unreachable, but the on-disk revoke alone is sufficient (issue
    // #78's scope note: this must not become a hard error).
    let out = env
        .cmd()
        .args(["client", "detach", &token_id])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("revoked"), "{stdout:?}");
}

#[test]
fn client_detach_also_rejects_a_reconnect_attempt_with_the_old_credential() {
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

        let detach_out = env
            .cmd()
            .args(["client", "detach", &token_id])
            .output()
            .unwrap();
        assert!(detach_out.status.success(), "{detach_out:?}");

        // Drain the close on the first connection so it doesn't linger.
        let _ = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
    });

    // A brand-new connection presenting the now-revoked credential is
    // rejected, same as any other bad `auth` — confirming
    // `TokenStore::verify_credential` already fails closed on `Revoked`
    // (it does, per `token::tests::verify_credential_on_revoked_token_is_revoked`);
    // this issue's actual gap was the live force-close exercised above,
    // not this rejection.
    rt.block_on(async {
        let (mut ws2, _resp2) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws2, &auth_envelope(&token_id, &credential)).await;
        let err = recv_json(&mut ws2).await;
        assert_eq!(err["type"], "error");
        assert_eq!(err["body"]["code"], "unauthenticated");
    });
}
