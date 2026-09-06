# holler-server

Talk circuit for the herd. The home: mint / list / ping / revoke tokens, route hollers, roster, interrupt, query capability. **Rust.**

See [issue #1](https://github.com/Performant-Labs/holler-server/issues/1), [dev environment](docs/dev-env.md), [how they talk](docs/protocol/talk.md), the [protocol index](docs/protocol/README.md), and [docs/adr](docs/adr/README.md).

## Prior art

Nobody has shipped this exact cut. Several projects overlap a piece of it — Holler steals **ideas** from them, not code.

| Project | What it is | vs Holler |
| --- | --- | --- |
| **Heterogeneous agent frameworks** | Most buses assume one connector (MCP on every agent, ACP on every agent, or “everyone is Claude”). | **First requirement.** OpenCode today; Claude and others unchecked on #1. A new harness is an adapter, not a new plane. |
| **[c2c](https://c2c.im/)** ([clankercode/c2c](https://github.com/clankercode/c2c)) | Heterogeneous CLIs (Claude, Codex, OpenCode, Grok, Pi, Kimi, agy, Hermes). Local file broker + optional HTTP/WS relay with `--token`. `c2c send` / `monitor` / `status`. **Alpha.** | Strongest product match. Cross-machine is inbox **sync/poll**, not a live WS circuit. Wake is uneven (OpenCode/Pi guaranteed; Claude/Grok often idle-deaf). **License: GitHub `null`; npm wrapper MIT — do not vendor.** |
| **[ai-crew-sync](https://github.com/joaquinbejar/ai-crew-sync)** | Postgres bus, `token issue` / revoke, presence, tasks, MCP HTTP. Cross-user and cross-machine. | Token identity is real. Still MCP-shaped; far box must be an MCP client. No ACP cancel as the product. |
| **[swarmcode](https://github.com/spranab/swarmcode)** | Redis channel so **Claude Code on different machines** can message. | Cross-machine yes. Claude-only. Shared Redis, not a minted join token. |
| **[Harness Remote](https://github.com/giuliastro/opencode-remote-android)** | Control plane over native OpenCode / Claude / Codex / Pi sessions across devices. | Remote **observe/resume/handoff**, not a talk circuit. You attach to *their* session. |
| **[opencode-orchestrator](https://github.com/autonomica-xyz/opencode-orchestrator)** | CLI against `opencode serve` HTTP on several hosts. | You configure URLs (SSH-shaped). No join token, no other harnesses. |
| **Claude Code [cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging)** | `SendMessage` across machines via Anthropic Remote Control + per-session token. | Official, works. **Claude-only**; other-machine traffic through Anthropic. |
| **[Buzz](https://github.com/block/buzz)** | Relay as home; ACP body; presence and `!shutdown` are messages. | Same *layer*. Not a clone (no Nostr/nsec). Apache-2.0. |
| **[Herdr](https://herdr.dev)** | PTY runtime; local socket; multi-remote ([#515](https://github.com/herdrdev/herdr/discussions/515)) is one TUI over many Herdr servers. | Closest *sibling*. Hub body, not the bus. Far box need not run it. Apache-2.0. |

### Closest commercial near-miss: Warp

A later survey (competitors to OpenAI/Anthropic — Google, Cursor, Windsurf, Devin, Replit, GitHub/VS Code, Amazon, Zed, Continue.dev, Amp, and an open-source-project cluster) found the same pattern everywhere: every serious vendor reinvented the same-account, single-identity remote control shown above. One is closer than the rest:

**Warp is heterogeneous but cloud-routed and link-based; Holler is heterogeneous and self-hosted with real token identity and a persistent roster, not a one-off share link.**

| | Warp | Holler |
| --- | --- | --- |
| Heterogeneous? | Yes — publishes Claude Code, Codex, OpenCode sessions | Yes — any harness that speaks ACP |
| Hosting | Warp's cloud; you publish, Warp routes | Self-hosted; you run holler-server yourself |
| Join mechanism | A shareable link; steering granted per-link, publisher revokes | Minted, revocable token: `mint`/`list`/`delete`/`ping`, hashed at rest |
| What you're joining | A single published session via its link | A roster of named sessions across joined machines, with presence/TTL |
| Trust model | Warp is in the middle of every session | Nothing in the middle — a direct WebSocket you control end to end |
| Interrupt | Send input / approve / redirect via Warp's UI | A first-class control frame (ACP `session/cancel` + HTTP fallback); the session survives |

Full competitive survey: [`docs/research-competitive-landscape.md`](https://github.com/Performant-Labs/holler-server/blob/research/grok-competitive-landscape/docs/research-competitive-landscape.md) (research memo, unmerged branch — discussion material, not yet folded into an ADR).

Gap that remains — why Holler still exists: a **small server that mints a join token**, a **client on a box with no Herdr**, **OpenCode (then Claude) as adapters**, **interrupt as ACP cancel with the session still alive**, and **meta-O talking over CLI without SSH**.

Full survey, including the same-machine tools, an X sampling, and the recommendations behind this decision: [ADR 0001](docs/adr/ADR-0001.md).

Companion: [holler-client](https://github.com/Performant-Labs/holler-client).
