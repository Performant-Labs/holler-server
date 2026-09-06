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
    reconnect_ms: u64,
    gone_ms: u64,
}

impl Env {
    fn new() -> Self {
        Env {
            dir: tempfile::tempdir().unwrap(),
            // Short TTLs so this test doesn't wait out the real 45s /
            // 180s production window: 500ms to `reconnecting`, 3s
            // total (from last presence) to `gone`. Generous enough
            // above CLI subprocess-spawn overhead and CI scheduler
            // jitter (`holler roster` runs as its own process per
            // check) that polling this roster doesn't race the
            // transition it's trying to observe — the in-process unit
            // tests in `src/wire/roster.rs` hit exactly this kind of
            // flake on a loaded CI runner with tighter margins.
            reconnect_ms: 500,
            gone_ms: 3000,
        }
    }

    /// Wide TTLs (issue #80): for tests proving a row reaches `gone`
    /// via the certain-close path, not the TTL decay. A row that
    /// reached `gone` fast under these windows could not have gotten
    /// there any other way.
    fn with_wide_ttls() -> Self {
        Env {
            dir: tempfile::tempdir().unwrap(),
            reconnect_ms: 30_000,
            gone_ms: 60_000,
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = holler();
        cmd.env("HOLLER_STATE_DIR", self.dir.path())
            .env("HOLLER_SERVER_PEPPER", "roster-test-pepper")
            .env("HOLLER_ROSTER_RECONNECT_MS", self.reconnect_ms.to_string())
            .env("HOLLER_ROSTER_GONE_MS", self.gone_ms.to_string())
            .env("HOLLER_ROSTER_SWEEP_MS", "100");
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

/// `holler token delete <token_id>` (issue #78): revokes on disk, then
/// (if a live server is reachable, which it is here) force-closes the
/// live connection over the control channel.
fn revoke(env: &Env, token_id: &str) {
    let out = env
        .cmd()
        .args(["token", "delete", token_id])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
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

fn status_json(env: &Env) -> Value {
    let out = env.cmd().args(["status", "--json"]).output().unwrap();
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
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
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

/// Like [`wait_for_roster_row`], but with a short (2s) budget — for
/// asserting a row reaches `gone` *fast*, not just eventually. Used
/// alongside [`Env::with_wide_ttls`] (issue #80): under those wide
/// windows, a row that only decayed via the TTL could not possibly
/// reach `gone` within this budget, so a pass here is only possible via
/// the certain-close/revoke path, not the TTL.
fn wait_for_roster_state_soon(env: &Env, name: &str, state: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let rows = roster_json(env);
        let row = find_row(&rows, name).cloned();
        if row.as_ref().is_some_and(|r| r["state"] == state) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("row {name:?} did not reach state {state:?} within 2s; last snapshot: {rows}");
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

/// Issue #81: `holler status --json`'s `sessions` field used to be
/// hardcoded to `0` even with real sessions advertised and showing
/// correctly in `holler roster`. Two sibling sessions on one connection
/// (mirroring the issue's own `alpha`/`beta` repro) must show up in
/// `status`'s live count.
#[test]
fn holler_status_reports_the_real_session_count() {
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
                json!([
                    {"name": "alpha", "harness": "opencode"},
                    {"name": "beta", "harness": "opencode"}
                ]),
            ),
        )
        .await;

        // Confirm the roster sees both first (this path is already
        // known-correct), then check `status --json` reports the same
        // live count instead of the old hardcoded `0`.
        wait_for_roster_row(&env, "alpha", |r| r.is_some());
        wait_for_roster_row(&env, "beta", |r| r.is_some());

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let status = status_json(&env);
            if status["sessions"] == 2 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for status.sessions == 2; last status: {status}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
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

/// Issue #80: an explicit, graceful WebSocket close is a certain signal
/// the connection is over — the roster must reflect `gone` right away,
/// not decay through the same `connected -> reconnecting -> gone` TTL a
/// silent network drop would use. `Env::with_wide_ttls` makes those TTLs
/// wide enough that reaching `gone` within this test's 2s budget is only
/// possible via the certain-close path, not the TTL.
#[test]
fn holler_roster_shows_gone_immediately_after_explicit_close() {
    let env = Env::with_wide_ttls();
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
                json!([{"name": "closer", "harness": "opencode"}]),
            ),
        )
        .await;
        let row = wait_for_roster_row(&env, "closer", |r| r.is_some());
        assert_eq!(row["state"], "connected");

        // A real, graceful WebSocket close handshake — not a network
        // drop or a killed process.
        ws.close(None)
            .await
            .expect("client sends a real Close frame");
        drop(ws);

        wait_for_roster_state_soon(&env, "closer", "gone");
    });
}

/// Issue #80/#78: a server-initiated revoke (`holler token delete` /
/// `client detach`) force-closes the live connection with the same
/// certainty as an explicit client close — the roster must reflect
/// `gone` immediately here too, not via the TTL.
#[test]
fn holler_roster_shows_gone_immediately_after_revoke() {
    let env = Env::with_wide_ttls();
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
                json!([{"name": "revoked", "harness": "opencode"}]),
            ),
        )
        .await;
        let row = wait_for_roster_row(&env, "revoked", |r| r.is_some());
        assert_eq!(row["state"], "connected");

        // Revoke while the connection is still open, from a separate
        // `holler token delete` process — mirrors the real CLI flow.
        revoke(&env, &token_id);

        wait_for_roster_state_soon(&env, "revoked", "gone");

        // The connection was actually force-closed server-side, not
        // just marked gone with the socket still open. `Registry::remove`
        // (issue #78) drops the outbound channel rather than sending a
        // clean `Close` handshake, so the client may see a graceful
        // close, a protocol-level reset, or EOF — any of those confirms
        // the socket is gone; only a live, still-open connection would
        // fail this.
        let closed = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("the revoked connection closes within 5s");
        assert!(
            !matches!(closed, Some(Ok(_))),
            "expected the server to close the revoked connection, got {closed:?}"
        );
    });
}
