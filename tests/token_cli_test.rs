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
    let list_json_out = env
        .cmd()
        .args(["token", "list", "--json"])
        .output()
        .unwrap();
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
    let ping_out = env
        .cmd()
        .args(["token", "ping", &token_id])
        .output()
        .unwrap();
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

fn mint(env: &Env, label: &str) -> (String, String) {
    let mint_out = env
        .cmd()
        .args(["token", "mint", "--label", label])
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
    (token_id, secret)
}

#[test]
fn redeem_binds_token_and_shows_up_via_client_list() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "laptop");

    let redeem_out = env
        .cmd()
        .args([
            "token",
            "redeem",
            &token_id,
            "--secret",
            &secret,
            "--machine",
            "kiwi.local",
        ])
        .output()
        .unwrap();
    assert!(redeem_out.status.success(), "{redeem_out:?}");
    let redeem_stdout = String::from_utf8(redeem_out.stdout).unwrap();
    let client_id = redeem_stdout
        .lines()
        .find_map(|l| l.strip_prefix("client_id:").map(|s| s.trim().to_string()))
        .expect("redeem prints a client_id line");
    let credential = redeem_stdout
        .lines()
        .find_map(|l| l.strip_prefix("credential:").map(|s| s.trim().to_string()))
        .expect("redeem prints a credential line");
    assert!(client_id.starts_with("cli_"));
    assert!(credential.starts_with("hlr_live_"));

    // `holler token list` shows the real hostname, replacing "-".
    let token_list_out = env.cmd().args(["token", "list"]).output().unwrap();
    let token_list_stdout = String::from_utf8(token_list_out.stdout).unwrap();
    assert!(token_list_stdout.contains("kiwi.local"));
    assert!(token_list_stdout.contains(&client_id));
    assert!(token_list_stdout.contains("bound"));
    assert!(!token_list_stdout.contains(&credential));

    // `holler client list` is the bound-token alias.
    let client_list_out = env.cmd().args(["client", "list"]).output().unwrap();
    assert!(client_list_out.status.success());
    let client_list_stdout = String::from_utf8(client_list_out.stdout).unwrap();
    assert!(client_list_stdout.contains(&client_id));
    assert!(client_list_stdout.contains("kiwi.local"));
    assert!(!client_list_stdout.contains(&credential));
}

#[test]
fn redeem_with_wrong_secret_fails_and_leaves_token_unused() {
    let env = Env::new();
    let (token_id, _secret) = mint(&env, "phone");

    let redeem_out = env
        .cmd()
        .args([
            "token",
            "redeem",
            &token_id,
            "--secret",
            "hlr_totally-wrong",
            "--machine",
            "kiwi.local",
        ])
        .output()
        .unwrap();
    assert!(!redeem_out.status.success());
    let stderr = String::from_utf8(redeem_out.stderr).unwrap();
    assert!(stderr.to_lowercase().contains("wrong secret"));

    let list_out = env.cmd().args(["token", "list"]).output().unwrap();
    let list_stdout = String::from_utf8(list_out.stdout).unwrap();
    assert!(list_stdout.contains("unused"));
}

#[test]
fn client_detach_revokes_a_bound_token() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "desktop");
    env.cmd()
        .args([
            "token",
            "redeem",
            &token_id,
            "--secret",
            &secret,
            "--machine",
            "kiwi.local",
        ])
        .output()
        .unwrap();

    let detach_out = env
        .cmd()
        .args(["client", "detach", &token_id])
        .output()
        .unwrap();
    assert!(detach_out.status.success(), "{detach_out:?}");
    let detach_stdout = String::from_utf8(detach_out.stdout).unwrap();
    assert!(detach_stdout.contains("revoked"));

    let list_out = env.cmd().args(["token", "list"]).output().unwrap();
    let list_stdout = String::from_utf8(list_out.stdout).unwrap();
    assert!(list_stdout.contains("revoked"));

    // A revoked token no longer shows up under `client list`.
    let client_list_out = env.cmd().args(["client", "list"]).output().unwrap();
    let client_list_stdout = String::from_utf8(client_list_out.stdout).unwrap();
    assert!(!client_list_stdout.contains(&token_id));
}

#[test]
fn redeem_already_bound_token_fails() {
    let env = Env::new();
    let (token_id, secret) = mint(&env, "tablet");
    env.cmd()
        .args([
            "token",
            "redeem",
            &token_id,
            "--secret",
            &secret,
            "--machine",
            "first.local",
        ])
        .output()
        .unwrap();

    let second = env
        .cmd()
        .args([
            "token",
            "redeem",
            &token_id,
            "--secret",
            &secret,
            "--machine",
            "second.local",
        ])
        .output()
        .unwrap();
    assert!(!second.status.success());
    let stderr = String::from_utf8(second.stderr).unwrap();
    assert!(stderr.to_lowercase().contains("bound"));
}
