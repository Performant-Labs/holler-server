//! hlrsvr-1503 (Concurrency & Rapid Requests): many CLI processes hitting
//! one server's control socket at the same instant must all get a real,
//! valid answer -- none dropped, none corrupted, none serialized behind a
//! lock that silently loses a request. Mirrors `tests/wire_instance_guard_test.rs`'s
//! `Env`/`ServerProcess` harness rather than sharing it (that file is not a
//! library).

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::Value;

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
            .env("HOLLER_SERVER_PEPPER", "rapid-control-socket-test-pepper");
        cmd
    }
}

struct ServerProcess {
    child: Child,
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
        rx.recv_timeout(Duration::from_secs(5))
            .expect("`holler-server serve` printed its listening line within 5s");

        ServerProcess { child }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 20 real `holler-server status --json` processes launched at once
/// (not sequentially awaited one at a time), each its own OS process
/// dialing the same Unix-domain control socket -- the actual shape a
/// human hammering `status`/`roster` from several terminals, or a script
/// polling status in a tight loop, produces. Every single one must
/// return a valid, self-consistent status document; none may see a
/// connection refused/reset, a truncated response, or a hang.
#[test]
fn many_simultaneous_control_socket_calls_all_get_a_real_answer() {
    let env = Env::new();
    let _server = ServerProcess::spawn(&env);

    const N: usize = 20;
    let handles: Vec<_> = (0..N)
        .map(|_| {
            let mut cmd = env.cmd();
            std::thread::spawn(move || {
                cmd.args(["status", "--json"])
                    .output()
                    .expect("run `holler-server status --json`")
            })
        })
        .collect();

    let outputs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    for (i, out) in outputs.iter().enumerate() {
        assert!(
            out.status.success(),
            "call {i} of {N} must succeed: {out:?}"
        );
        let json: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("call {i} of {N} produced invalid JSON: {e}\n{out:?}"));
        assert_eq!(
            json["role"], "server",
            "call {i} of {N} returned a malformed/wrong document: {json:?}"
        );
    }
}
