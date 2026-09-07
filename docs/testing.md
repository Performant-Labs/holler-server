# Testing Holler (CLI, not a browser)

This page is about the *design* of the wire harness below. For practical "how do I run a
test / a single ticket / the release-gating catalog" instructions, see
[running-tests.md](running-tests.md) instead.

Playwright is the wrong tool here. It drives **web pages**. Holler is two processes and a CLI. There is no DOM.

## What we use instead

A **process-level wire harness** that uses the **same WebSocket path as production** (loopback `ws://127.0.0.1:41807`, ADR 0004 / 0010). No mock of the circuit. No SSH.

| Piece | Job |
| --- | --- |
| `holler-server` | Real binary (renamed from a bare `holler` — see [holler-server#232](https://github.com/Performant-Labs/holler-server/pull/232) — specifically so it can't collide with `holler-client`'s own binary, which is still named `holler`), `--listen 127.0.0.1:41807 --debug=quiet` |
| `holler-client`'s `holler` | Real binary, `join` with a minted token |
| **ACP stub** | Tiny fake agent on **stdio** (ACP v1): `session/prompt` → canned `session/update`; `session/cancel` → turn ends. Not OpenCode. Each repo keeps its own local copy (`holler-server`'s at `tests/wire/stub_acp.rs`, `holler-client`'s at `tests/stub-acp/src/main.rs`) so each crate's own `cargo test` stays self-contained — see [running-tests.md](running-tests.md). |
| Runner | Spawns the pieces it needs, drives the real binaries' CLI, asserts stdout/stderr/exit code |

That is the e2e analogue: **`cargo test` + subprocess + JSON assertions**. Not Playwright, not Cypress, not a browser WebSocket client.

**Language is Rust.** Spawn with `std::process::Command` / `tokio::process`. On Windows the stub is `stub-acp.exe`. Kill the tree without relying on `SIGTERM` alone.

**Currently required on Linux and macOS.** Windows is deliberately off both repos' CI matrices for now (not soft-failed — actually removed; see the dated comment in each repo's `.github/workflows/ci.yml`), pending a flaky timing canary on the server side and a real Windows compile break on the client side ([holler-client#60](https://github.com/Performant-Labs/holler-client/issues/60)). The design below still targets all three; re-add `windows-latest` to the CI matrices once those are fixed rather than treating "two OSes" as the permanent target. The required harness is TCP **`127.0.0.1:41807`** + process stdio — not bash, not Unix sockets, not `#!/bin/sh` as the only runner. Bind **`127.0.0.1`**, not `localhost` (`localhost` is often `::1` on Windows while the default listen is IPv4). A Unix-only bats extra is optional and **not** the required e2e.

IPv6: optional extra case `--listen [::1]:41807` and `ws://[::1]:41807`. **Skip** if `::1` is not available. Do not fail the required matrix when IPv6 is off.

## Same machine, same method as the wire

```
runner
  ├─ holler-server   ws://127.0.0.1:41807
  └─ holler-client   --server ws://127.0.0.1:41807 --token …
         └─ stub-acp (ACP stdio)   sessions alpha, beta
```

TLS is not required: loopback `ws` is allowed. Remote/prod still uses `wss`.

## Test of tests (fail right away)

Own story, **first in the epic**: a canary that proves the runner can **fail**.

If this is green while broken, later e2e is theater.

- A nested case that is **designed to fail** (spawn `exit 1` / `cmd /c exit 1`, or dial a closed port). The outer test **fails immediately** (<2s, no long timeout) if that case was green or if the runner swallowed the error.
- Dial `127.0.0.1:41807` with **no server**: must fail fast, not hang.
- Linux and macOS today, Windows once it's back on the CI matrix (see above). Not Playwright.

Lives in `tests/wire/selftest.rs` and is run with **`cargo test`**. Does **not** require a working `holler-server` or `holler-client` binary yet.

## When it comes on

- **Test of tests:** first. CI must be able to go red.
- **Design + stub + runner scaffold:** early (after protocol types exist).
- **First talk:** mint → join → `hello` → `holler status` / `holler-server token ping` (server story “WebSocket listener”, client story “WebSocket session”).
- Every later story **adds cases** to this harness (query, roster, say, interrupt). Do not invent a second test stack — this held: the test-catalog pilot (below) added a second **shared helper module**, not a second harness design.
- Acceptance gate still uses **real OpenCode** on a Herdr-free box. The stub does not replace that gate — see `hlrsvr-1101`/`hlrclnt-1102` (formerly `TC-020`), which stays manual by design for exactly this reason.

## The test-case catalog (built on top of this harness)

Every test case — including everything this page describes — is tracked as a GitHub issue with a `hlrsvr-*`/`hlrclnt-*` Test ID, living in [holler-server#98](https://github.com/Performant-Labs/holler-server/issues/98) (the master catalog, covering both repos). Git is the source of truth for what an automated case actually does; the issue is an index + run log, not a second spec. See [running-tests.md](running-tests.md) for how to actually run a case (`ruby scripts/test-run.rb exec <TEST_ID>`, or plain `cargo test`) — this page stays about the harness's *design*, that one is the practical how-to.

The pilot that built this catalog also added a **shared test-support module** per repo (`tests/support/mod.rs` in each) — real subprocess orchestration helpers (ephemeral state dir, `127.0.0.1:0` binding, `cargo_bin` resolution, mint/join/run/say/interrupt/roster/status, wait-for-predicate, graceful stop + kill unifying Unix signals and Windows `TerminateProcess` in one place) that new automated tests compose from, hand-written in Rust — deliberately **not** Cucumber/Gherkin and **not** a declarative YAML step registry, both considered and rejected. `assert_cmd`/`predicates`/`rstest` are used for CLI-invocation-shaped cases (the Invocation group).

## Beyond loopback: the real cross-platform interop harness

Everything above tests one machine talking to itself over loopback `ws://`. [Issue #94](https://github.com/Performant-Labs/holler-server/issues/94) built the real next layer: `.github/workflows/interop.yml`, which runs a real `holler-server` on Linux, exposes it through a genuine Cloudflare tunnel (real `wss://`, real TLS termination — this is also what proved `holler-client` needed a TLS backend at all, fixed in [holler-client#65/#66](https://github.com/Performant-Labs/holler-client/issues/65)), and drives a real `holler-client` on a GitHub-hosted macOS runner across that tunnel. It's `workflow_dispatch`-only (not automatic, to conserve Actions minutes), and its own `windows-client` job is present but commented out pending holler-client#60, the same Windows compile break noted above.

## Layout (v1)

- holler-server: `tests/wire/` — runner, fixtures, assertions; `tests/support/mod.rs` — shared harness helpers for the catalog pilot's tests
- holler-client: `tests/stub-acp/` — fake ACP agent the runner execs as `command`; `tests/support/mod.rs` — its own mirror of the shared harness, plus `tests/interop_smoke_test.rs` (a real cross-repo mint→join→run→status→roster→stop smoke test, `#[ignore]`d by default, opt-in via `HOLLER_SERVER_BIN` since the two repos have no workspace/path dependency between them)

Secrets never appear in logs (`--debug=quiet` or `none`; `--json` output must not include join secrets after mint’s one-time print).

CI: run `tests/wire/` on Linux and macOS (GitHub Actions matrix; Windows currently excluded, see above).
