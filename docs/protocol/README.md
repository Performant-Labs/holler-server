# Protocols Holler uses

**This page is the single index.** Specs live here or at the linked external document. ADRs decide *why*; this page lists *what is on which wire*.

GitHub copy: [#39](https://github.com/Performant-Labs/holler-server/issues/39).

Holler does **not** invent a cipher, does **not** speak MCP on the circuit, and does **not** use IETF RFC 8994 (a different “ACP”).

## Index

| Layer | Protocol | Version we pin | Canonical spec | ADR |
| --- | --- | --- | --- | --- |
| **Talk circuit** (server ↔ client) | **Holler** | **v1** | [v1.md](v1.md) | [0009](../adr/ADR-0009.md) |
| Circuit transport | WebSocket text frames | default TCP **41807** (user-settable) | [v1.md](v1.md) §1 | [0004](../adr/ADR-0004.md) |
| Circuit confidentiality | TLS 1.3 on `wss` (AES-256-GCM or ChaCha20-Poly1305). Plain `ws` = loopback only | TLS 1.3 | [ADR 0010](../adr/ADR-0010.md) | [0010](../adr/ADR-0010.md) |
| Pairing / auth | Join token → client credential; secret at rest is HMAC-SHA-256 + pepper | — | [ADR 0003](../adr/ADR-0003.md), [0010](../adr/ADR-0010.md) | [0003](../adr/ADR-0003.md) |
| **Body** (client → local coding agent) | **Agent Client Protocol (ACP)** | **v1** (stable) | [agentclientprotocol.com](https://agentclientprotocol.com/protocol/v1/overview) · [schema repo](https://github.com/agentclientprotocol/agent-client-protocol) | [0012](../adr/ADR-0012.md), [0008](../adr/ADR-0008.md) |
| ACP encoding | JSON-RPC 2.0 over stdio (newline-delimited JSON) | 2.0 | [jsonrpc.org](https://www.jsonrpc.org/specification) | [0012](../adr/ADR-0012.md) |
| Interrupt fallback | OpenCode HTTP `POST /api/session/{id}/interrupt` | vendor HTTP | [ADR 0005](../adr/ADR-0005.md) | [0005](../adr/ADR-0005.md) |

There is no Holler v0. ACP v2 is a **draft**; do not pin it (ADR 0012).

## What each protocol is for

```
meta-O --CLI--> holler-server  <== Holler v1 / wss ==>  holler-client
                                                              │
                                                              │ ACP v1 / stdio
                                                              ▼
                                                         opencode acp  (or other command)
```

- **Holler v1** — machines talk: `say`, `interrupt`, `query`, roster, tokens.
- **ACP v1** — this box drives a coding agent: `session/new`, `session/prompt`, `session/cancel`, `session/update`.
- **TLS 1.3** — encrypts the WebSocket. Not Tailscale, not a Holler cipher.
- **JSON-RPC 2.0** — how ACP messages are framed. Holler frames are *not* JSON-RPC; they are the envelope in [v1.md](v1.md).

## ACP notes (so this index stands alone)

ACP is *not* an IETF RFC. Changes go through **RFDs** (Requests for Dialog) on the ACP project — their RFC-shaped process, not Internet Standards.

Do not confuse with **RFC 8994** Autonomic Control Plane (also “ACP”). Unrelated.

ACP is not **MCP** (tools) and not Holler (the circuit).

## Not on the Holler wire

| Name | Why it is listed here |
| --- | --- |
| Tailscale / WireGuard / SSH | Optional *underlay* for TCP. Not auth, not addressing, not encryption of Holler (ADR 0004, 0010). |
| MCP, Google A2A | Different jobs. |
| IETF RFC 8994 | Different “ACP.” |
| ACP v2 draft | Out of scope for Holler v1. |
