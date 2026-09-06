//! Proves the shared test-support harness (`tests/support/mod.rs`) actually
//! works against a real `holler-server` process -- this is not a test case
//! from the catalog itself, it's the foundation the catalog's Process
//! Lifecycle / Concurrency / Network groups will build on.

mod support;

use std::time::Duration;

#[test]
fn start_mint_roster_status_stop() {
    let server = support::ServerHandle::start(support::DEFAULT_TIMEOUT);
    assert!(
        server.addr.starts_with("127.0.0.1:"),
        "expected a loopback address, got {}",
        server.addr
    );

    let (token_id, secret) = support::mint_token(&server.state_dir, "harness-smoke");
    assert!(!token_id.is_empty());
    assert!(!secret.is_empty());

    let roster = support::roster_json(&server.state_dir);
    assert_eq!(
        roster.as_array().map(|a| a.len()),
        Some(0),
        "a freshly-started server with no joined clients should have an empty roster, got: {roster}"
    );

    let status = support::status_json(&server.state_dir);
    assert!(
        status.is_object(),
        "status --json should print a JSON object, got: {status}"
    );

    server.stop(Duration::from_secs(5));
}
