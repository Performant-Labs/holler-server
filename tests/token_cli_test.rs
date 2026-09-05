//! Integration test for `holler token mint/list/delete/ping` (issue
//! #29), driven through the actual built binary — not the library API
//! — so it exercises argument parsing, process exit codes, and stdout
//! formatting the way an operator actually sees them.

use std::process::Command;

fn holler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_holler"))
}

/// A fresh, isolated state dir + pepper per test so parallel tests
/// never share a token store or race on process env.
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
            .env("HOLLER_SERVER_PEPPER", "integration-test-pepper");
        cmd
    }
}

#[test]
fn mint_list_delete_ping_flow() {
    let env = Env::new();

    // mint
    let mint_out = env
        .cmd()
        .args(["token", "mint", "--label", "ci-box"])
        .output()
        .unwrap();
    assert!(mint_out.status.success(), "{mint_out:?}");
    let mint_stdout = String::from_utf8(mint_out.stdout).unwrap();
    let secret = mint_stdout
        .lines()
        .find_map(|l| l.strip_prefix("secret:").map(|s| s.trim().to_string()))
        .expect("mint prints a secret line");
    let token_id = mint_stdout
        .lines()
        .find_map(|l| l.strip_prefix("token_id:").map(|s| s.trim().to_string()))
        .expect("mint prints a token_id line");
    assert!(secret.starts_with("hlr_"));
    assert!(token_id.starts_with("tok_"));

    // list (table) never contains the secret
    let list_out = env.cmd().args(["token", "list"]).output().unwrap();
    assert!(list_out.status.success());
    let list_stdout = String::from_utf8(list_out.stdout).unwrap();
    assert!(list_stdout.contains(&token_id));
    assert!(list_stdout.contains("ci-box"));
    assert!(list_stdout.contains("unused"));
    assert!(!list_stdout.contains(&secret));

    // list --json never contains the secret either
    let list_json_out = env.cmd().args(["token", "list", "--json"]).output().unwrap();
    assert!(list_json_out.status.success());
    let list_json_stdout = String::from_utf8(list_json_out.stdout).unwrap();
    assert!(list_json_stdout.contains(&token_id));
    assert!(!list_json_stdout.contains(&secret));
    assert!(!list_json_stdout.contains("secret_hash"));

    // delete
    let delete_out = env
        .cmd()
        .args(["token", "delete", &token_id])
        .output()
        .unwrap();
    assert!(delete_out.status.success(), "{delete_out:?}");
    let delete_stdout = String::from_utf8(delete_out.stdout).unwrap();
    assert!(delete_stdout.contains("invalidated"));

    // ping on the now-deleted (invalidated) token: error contract.
    let ping_out = env.cmd().args(["token", "ping", &token_id]).output().unwrap();
    assert!(!ping_out.status.success());
    let ping_stderr = String::from_utf8(ping_out.stderr).unwrap();
    assert!(ping_stderr.to_lowercase().contains("error"));
}

#[test]
fn ping_unknown_token_errors() {
    let env = Env::new();
    let ping_out = env
        .cmd()
        .args(["token", "ping", "tok_doesnotexist"])
        .output()
        .unwrap();
    assert!(!ping_out.status.success());
}

#[test]
fn mint_fails_closed_without_pepper() {
    let env = Env::new();
    let mut cmd = holler();
    cmd.env("HOLLER_STATE_DIR", env.dir.path())
        .env_remove("HOLLER_SERVER_PEPPER");
    let out = cmd.args(["token", "mint"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.to_lowercase().contains("pepper"));
}

#[test]
fn delete_unknown_token_errors() {
    let env = Env::new();
    let out = env
        .cmd()
        .args(["token", "delete", "tok_doesnotexist"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn mint_alias_create_works() {
    let env = Env::new();
    let out = env.cmd().args(["token", "create"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn delete_alias_rm_works() {
    let env = Env::new();
    let mint_out = env.cmd().args(["token", "mint"]).output().unwrap();
    let mint_stdout = String::from_utf8(mint_out.stdout).unwrap();
    let token_id = mint_stdout
        .lines()
        .find_map(|l| l.strip_prefix("token_id:").map(|s| s.trim().to_string()))
        .unwrap();

    let out = env.cmd().args(["token", "rm", &token_id]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
}
