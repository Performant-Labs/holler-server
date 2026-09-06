//! End-to-end integration test for issue #32 ("roster + presence
//! TTL"): a real `holler serve` listener, driven by a real
//! `tokio-tungstenite` WebSocket client sending `presence` frames, and
//! `holler roster` (run as a separate CLI process) reaching the live
//! server's in-memory roster over the local control channel.
//!
//! Deliberately its own file with its own copies of the `Env` /
//! `ServerProcess` / `mint` / `redeem` / `auth_envelope` /
//! `send_json` / `recv_json` helpers, mirroring
//! `tests/wire_first_talk_test.rs` rather than sharing code with it —
//! this repo's own convention (see that file's and
//! `tests/wire/selftest.rs`'s doc comments) is that each top-level
//! integration test stays standalone.
//!
//! `HOLLER_ROSTER_RECONNECT_MS` / `HOLLER_ROSTER_GONE_MS` (see
//! `src/wire/roster.rs::RosterConfig::from_env`) let this test exercise
//! the `reconnecting`/`gone` transitions in milliseconds instead of the
//! real 45s/180s production window.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

fn holler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_holler"))
}

/// A fresh, isolated state dir + pepper + short roster TTLs per test.
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
            .env("HOLLER_SERVER_PEPPER", "roster-test-pepper")
            // Short TTLs so this test doesn't wait out the real 45s /
            // 180s production window: 300ms to `reconnecting`, 900ms
            // total (from last presence) to `gone`. Generous enough
            // above the CLI subprocess-spawn overhead (`holler roster`
            // runs as its own process per check) that polling this
            // roster doesn't race the transition it's trying to observe.
            .env("HOLLER_ROSTER_RECONNECT_MS", "300")
            .env("HOLLER_ROSTER_GONE_MS", "900")
            .env("HOLLER_ROSTER_SWEEP_MS", "50");
        cmd
    }
}

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

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
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

fn presence_envelope(token_id: &str, sessions: Value) -> Value {
    json!({
        "v": 1, "type": "presence", "id": "id-presence", "ts": "2026-09-05T00:00:00Z",
        "from": token_id, "body": { "sessions": sessions }
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

fn roster_json(env: &Env) -> Value {
    let out = env.cmd().args(["roster", "--json"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

fn find_row<'a>(rows: &'a Value, name: &str) -> Option<&'a Value> {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
}

/// Polls `holler roster --json` until `pred` matches the named row (or
/// the row is absent and `pred` accepts `None`), or the budget expires.
fn wait_for_roster_row(env: &Env, name: &str, pred: impl Fn(Option<&Value>) -> bool) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let rows = roster_json(env);
        let row = find_row(&rows, name).cloned();
        if pred(row.as_ref()) {
            return row.unwrap_or(Value::Null);
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for roster row {name:?}; last snapshot: {rows}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn holler_roster_reflects_an_advertised_session() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (_client_id, credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .expect("client connects to the real listener");
        send_json(&mut ws, &auth_envelope(&token_id, &credential)).await;
        let _server_hello = recv_json(&mut ws).await;

        send_json(
            &mut ws,
            &presence_envelope(
                &token_id,
                json!([{"name": "alpha", "harness": "opencode"}]),
            ),
        )
        .await;

        // No ack for `presence` (research memo: it's a self-healing
        // heartbeat) — poll the roster directly rather than waiting on
        // a reply that will never come.
        let row = wait_for_roster_row(&env, "alpha", |r| r.is_some());
        assert_eq!(row["harness"], "opencode");
        assert_eq!(row["state"], "connected");
    });
}

#[test]
fn holler_roster_owning_client_matches_the_redeemed_client_id() {
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

        send_json(
            &mut ws,
            &presence_envelope(
                &token_id,
                json!([{"name": "solo", "harness": "opencode"}]),
            ),
        )
        .await;

        let row = wait_for_roster_row(&env, "solo", |r| r.is_some());
        assert_eq!(row["client_id"], client_id);
    });
}

#[test]
fn holler_roster_shows_gone_after_ttl_elapses() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (_client_id, credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws, &auth_envelope(&token_id, &credential)).await;
        let _server_hello = recv_json(&mut ws).await;

        send_json(
            &mut ws,
            &presence_envelope(
                &token_id,
                json!([{"name": "fading", "harness": "opencode"}]),
            ),
        )
        .await;

        // Connected right after advertise.
        let row = wait_for_roster_row(&env, "fading", |r| r.is_some());
        assert_eq!(row["state"], "connected");

        // No further heartbeat sent: `HOLLER_ROSTER_RECONNECT_MS=200`
        // means it should flip to `reconnecting` first...
        let row = wait_for_roster_row(&env, "fading", |r| {
            r.is_some_and(|r| r["state"] == "reconnecting")
        });
        assert_eq!(row["state"], "reconnecting");

        // ...then, past `HOLLER_ROSTER_GONE_MS=500` (measured from the
        // same last-presence timestamp), `gone`.
        let row = wait_for_roster_row(&env, "fading", |r| {
            r.is_some_and(|r| r["state"] == "gone")
        });
        assert_eq!(row["state"], "gone");
    });
}

#[test]
fn holler_roster_tracks_two_sibling_sessions_independently() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "kiwi");
    let (_client_id, credential) = redeem(&env, &token_id, &secret, "kiwi.local");
    let server = ServerProcess::spawn(&env);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(server.url())
            .await
            .unwrap();
        send_json(&mut ws, &auth_envelope(&token_id, &credential)).await;
        let _server_hello = recv_json(&mut ws).await;

        // Advertise both sessions in one `presence` frame.
        send_json(
            &mut ws,
            &presence_envelope(
                &token_id,
                json!([
                    {"name": "keep-alive", "harness": "opencode"},
                    {"name": "left-behind", "harness": "opencode"}
                ]),
            ),
        )
        .await;

        let rows = roster_json(&env);
        assert!(find_row(&rows, "keep-alive").is_some());
        assert!(find_row(&rows, "left-behind").is_some());

        // Wait until both are `reconnecting`, then re-advertise only
        // `keep-alive`.
        wait_for_roster_row(&env, "keep-alive", |r| {
            r.is_some_and(|r| r["state"] == "reconnecting")
        });
        send_json(
            &mut ws,
            &presence_envelope(
                &token_id,
                json!([{"name": "keep-alive", "harness": "opencode"}]),
            ),
        )
        .await;

        // `keep-alive` is connected again; `left-behind`, never
        // re-advertised, is still aging on its own in the very same
        // snapshot — this is the independence claim: one sibling's
        // heartbeat does not touch the other's state.
        let row = wait_for_roster_row(&env, "keep-alive", |r| {
            r.is_some_and(|r| r["state"] == "connected")
        });
        assert_eq!(row["state"], "connected");
        let rows = roster_json(&env);
        assert_eq!(find_row(&rows, "left-behind").unwrap()["state"], "reconnecting");

        // Left entirely alone from here, `left-behind` still reaches
        // `gone` on its own TTL (already covered end-to-end by
        // `holler_roster_shows_gone_after_ttl_elapses`; this just
        // confirms the same clock keeps ticking for an unattended
        // sibling row).
        let row = wait_for_roster_row(&env, "left-behind", |r| {
            r.is_some_and(|r| r["state"] == "gone")
        });
        assert_eq!(row["state"], "gone");
    });
}
