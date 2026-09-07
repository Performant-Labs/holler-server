# How server and client talk

Two hops. Two protocols. The client is the hinge.

Holler **never** speaks OpenCode (or Claude, or anyone else’s native API) on the wire. The server only talks **Holler v1**. The client talks **ACP v1** to the target on this box.

Specs: [Holler v1](v1.md) · [protocol index](README.md) · interrupt [ADR 0005](../adr/ADR-0005.md) · body [ADR 0008](../adr/ADR-0008.md) / [ADR 0012](../adr/ADR-0012.md) · attach [ADR 0017](../adr/ADR-0017.md)

The second hop (holler-client → agent) has two shapes as of ADR 0017. **Spawn is the default** — this is it, unchanged:

## Interrupt (spawn — default)

```
you:  holler-server interrupt alpha
          │  Holler v1  (WebSocket, default :41807)
          ▼
     holler-server  --interrupt frame-->  holler-client
                                                │  ACP v1  (stdio JSON-RPC)
                                                ▼
                                          opencode acp   ← the target,
                                          session/cancel   spawned and owned
                                                           by holler-client
```

`holler-server interrupt alpha` is **not** a prompt and not SIGINT. It is a Holler control frame:

```json
{ "v": 1, "type": "interrupt", "body": { "session": "alpha" } }
```

holler-client already has an ACP subprocess for that session (v1 default: `opencode acp`). It maps the frame to ACP **`session/cancel`**.

- The **turn** dies.
- The **ACP session stays** and must accept a later `holler-server say`.
- Nested tool/permission work should cancel with the turn (ACP cascading cancel).
- Interrupting `alpha` must not touch `beta`.

If that agent does not honor `session/cancel`, v1 falls back to OpenCode HTTP `POST /api/session/{id}/interrupt` — same idea, different door. That fallback is **config**, not a second plugin system. The HTTP port is OpenCode’s (often 4096), not Holler’s.

## Prompt (spawn — default, the other direction)

Same hinge:

```
you:  holler-server say alpha "…"
          │  Holler v1  prompt
          ▼
     holler-server  -------------------->  holler-client
                                                │  ACP session/prompt
                                                ▼
                                          opencode acp
                                                │  ACP session/update
                                                ▼
     holler-server  <----- reply ---------  holler-client
```

`holler-server say` is a Holler `prompt` frame. The client turns it into ACP `session/prompt`. Agent output comes back as ACP `session/update` and is published on the circuit as Holler `reply`.

Extra prompts while a turn runs are **queued**. Interrupt cancels the current turn; queued prompts stay (ADR 0005).

## Attach mode (ADR 0017 / holler-client ADR 0005) — second hop shape, not the default

A far box that already runs Herdr with a live OpenCode TUI in a pane does not need a second, spawned `opencode acp` — holler-client attaches to that *existing* session over its own HTTP control surface instead:

```
remote:  [Herdr pane] --PTY--> OpenCode TUI  <--HTTP attach-- holler-client (sidecar)
                                                          |
hub:     holler-server say / interrupt  <-----------------WS-+
```

The first hop (Meta-O → holler-server → holler-client, plain Holler v1 over WS/wss) is **identical** to spawn mode — `say`/`interrupt`/`prompt`/`reply` frames are unchanged, no new frame types, no protocol version bump. Only the second hop differs: holler-client is not the parent of OpenCode, so it drives it over HTTP (`GET` to confirm the session exists, an async prompt endpoint, an HTTP interrupt endpoint) instead of ACP stdio `session/new`/`session/prompt`/`session/cancel`. `holler-server`'s own view of the circuit — the session *name* (`alpha`) it routes `say`/`interrupt` to — does not change; the OpenCode `ses_…` id is a body-side locator only, optionally shown on presence/roster (`mode`, `harness_session_id` — both optional keys, ignored by any decoder that predates them).

`holler run` is **not** the foreground command of the OpenCode pane in this shape — it runs as a sidecar (another pane, or a background process) next to Herdr's pane, which stays the TUI's real parent throughout. Detach / client exit / WS drop never kills or replaces the attached session — see ADR 0017 for the full rationale.

## What lives where

| Hop | Protocol | Carries |
| --- | --- | --- |
| Meta-O → holler-server | CLI | `say`, `interrupt`, `query`, `roster`, tokens |
| holler-server ↔ holler-client | **Holler v1** on `wss` (plain `ws` = loopback) | `prompt`, `reply`, `interrupt`, `query`, `presence`, `ping` — identical for spawn and attach |
| holler-client → agent (**spawn**, default) | **ACP v1** stdio | `session/new`, `session/prompt`, `session/cancel`, `session/update` |
| holler-client → agent (**attach**) | OpenCode **HTTP** on the existing session | session-exists check, async prompt, interrupt — never `session/new` |

The client does **not** listen for Holler TCP. It dials the server (default port **41807** if the URL omits one). In spawn mode ACP is a subprocess, not a port; in attach mode the "port" is OpenCode's own HTTP endpoint on the same box (typically `http://127.0.0.1:4096`), not Holler's.
