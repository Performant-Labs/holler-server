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

That is the e2e analogue: **subprocess + JSON assertions**, in the repo’s normal test runner (Go `testing`, pytest, bats, … — pick whatever the implementation language is). Optional bats for extra shell cases. Not Playwright, not Cypress, not a browser WebSocket client.

## Same machine, same method as the wire

```
runner
  ├─ holler-server   ws://127.0.0.1:41807
  └─ holler-client   --server ws://127.0.0.1:41807 --token …
         └─ stub-acp (ACP stdio)   sessions alpha, beta
```

TLS is not required: loopback `ws` is allowed. Remote/prod still uses `wss`.

## When it comes on

- **Design + stub + runner scaffold:** early (after protocol types exist).
- **First talk:** mint → join → `hello` → `holler status` / `holler token ping` (server story “WebSocket listener”, client story “WebSocket session”).
- Every later story **adds cases** to this harness (query, roster, say, interrupt). Do not invent a second test stack.
- Acceptance gate still uses **real OpenCode** on a Herdr-free box. The stub does not replace that gate.

## Layout (v1)

- holler-server: `tests/wire/` — runner, fixtures, assertions
- holler-client: `tests/stub-acp/` — fake ACP agent the runner execs as `command`

Secrets never appear in logs (`--debug=quiet` or `none`; `--json` output must not include join secrets after mint’s one-time print).
