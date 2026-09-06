# How server and client talk

Two hops. Two protocols. The client is the hinge.

Holler **never** speaks OpenCode (or Claude, or anyone else’s native API) on the wire. The server only talks **Holler v1**. The client talks **ACP v1** to the target on this box.

Specs: [Holler v1](v1.md) · [protocol index](README.md) · interrupt [ADR 0005](../adr/ADR-0005.md) · body [ADR 0008](../adr/ADR-0008.md) / [ADR 0012](../adr/ADR-0012.md)

## Interrupt

```
you:  holler-server interrupt alpha
          │  Holler v1  (WebSocket, default :41807)
          ▼
     holler-server  --interrupt frame-->  holler-client
                                                │  ACP v1  (stdio JSON-RPC)
                                                ▼
                                          opencode acp   ← the target
                                          session/cancel
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

## Prompt (the other direction)

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

## What lives where

| Hop | Protocol | Carries |
| --- | --- | --- |
| Meta-O → holler-server | CLI | `say`, `interrupt`, `query`, `roster`, tokens |
| holler-server ↔ holler-client | **Holler v1** on `wss` (plain `ws` = loopback) | `prompt`, `reply`, `interrupt`, `query`, `presence`, `ping` |
| holler-client → agent | **ACP v1** stdio | `session/new`, `session/prompt`, `session/cancel`, `session/update` |

The client does **not** listen for Holler TCP. It dials the server (default port **41807** if the URL omits one). ACP is a subprocess, not a port.
