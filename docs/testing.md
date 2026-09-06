# Testing Holler (CLI, not a browser)

Playwright is the wrong tool here. It drives **web pages**. Holler is two processes and a CLI. There is no DOM.

## What we use instead

A **process-level wire harness** that uses the **same WebSocket path as production** (loopback `ws://127.0.0.1:41807`, ADR 0004 / 0010). No mock of the circuit. No SSH.

| Piece | Job |
| --- | --- |
| holler-server | Real binary, `--listen 127.0.0.1:41807 --debug=quiet` |
| holler-client | Real binary, `join` with a minted token |
| **ACP stub** | Tiny fake agent on **stdio** (ACP v1): `session/prompt` → canned `session/update`; `session/cancel` → turn ends. Not OpenCode. |
| Runner | Spawns the three, drives `holler … --json`, asserts stdout |

That is the e2e analogue: **`cargo test` + subprocess + JSON assertions**. Not Playwright, not Cypress, not a browser WebSocket client.

**Language is Rust.** Spawn with `std::process::Command` / `tokio::process`. On Windows the stub is `stub-acp.exe`. Kill the tree without relying on `SIGTERM` alone.

**Must pass on Linux, macOS, and Windows.** The required harness is TCP **`127.0.0.1:41807`** + process stdio — not bash, not Unix sockets, not `#!/bin/sh` as the only runner. Bind **`127.0.0.1`**, not `localhost` (`localhost` is often `::1` on Windows while the default listen is IPv4). A Unix-only bats extra is optional and **not** the required e2e.

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
- Linux, macOS, Windows. Not Playwright.

Lives in `tests/wire/selftest/` and is run with **`cargo test`**. Does **not** require a working `holler` binary yet.

## When it comes on

- **Test of tests:** first. CI must be able to go red.
- **Design + stub + runner scaffold:** early (after protocol types exist).
- **First talk:** mint → join → `hello` → `holler status` / `holler-server token ping` (server story “WebSocket listener”, client story “WebSocket session”).
- Every later story **adds cases** to this harness (query, roster, say, interrupt). Do not invent a second test stack.
- Acceptance gate still uses **real OpenCode** on a Herdr-free box. The stub does not replace that gate.

## Layout (v1)

- holler-server: `tests/wire/` — runner, fixtures, assertions
- holler-client: `tests/stub-acp/` — fake ACP agent the runner execs as `command`

Secrets never appear in logs (`--debug=quiet` or `none`; `--json` output must not include join secrets after mint’s one-time print).

CI: run `tests/wire/` on Linux, macOS, and Windows (GitHub Actions matrix is enough).
