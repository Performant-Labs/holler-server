# holler-server

Your agents are just a holler away.

[![CI](https://github.com/Performant-Labs/holler-server/actions/workflows/ci.yml/badge.svg)](https://github.com/Performant-Labs/holler-server/actions/workflows/ci.yml)

Holler is the missing talk circuit between coding-agent sessions on different machines. A meta-orchestrator (a human, or another agent) mints a token here; a `holler-client` on the far machine joins with it; after that, the two sides prompt, reply, and interrupt each other directly — no SSH, no shared Herdr socket, and no third party's cloud in the middle. It's self-hosted, harness-agnostic (anything that speaks ACP works), and built around a persistent, revocable roster of joined machines rather than one-off shareable links.

## Table of contents

- [Quickstart](#quickstart)
- [Why this is different](#why-this-is-different)
- [Architecture](#architecture)
- [Documentation](#documentation)
- [Status and license](#status-and-license)
- [Companion repo](#companion-repo)

## Quickstart

This repo is the server half of the circuit. A full round-trip also needs [holler-client](https://github.com/Performant-Labs/holler-client) running on the machine you're joining — this repo alone gets you through minting a token, not a live prompt/reply exchange.

```
# 1. Build
cargo build --release

# 2. Start the server (defaults to 127.0.0.1:41807; --advertise records the
#    address others should use so `token mint` can print a working join command)
holler serve --listen 0.0.0.0:41807 --advertise myhost.example.com:41807

# 3. Mint a join token — prints the token_id/secret once, plus a ready-to-paste
#    `holler join` command
holler token mint --label laptop
#   token_id: ...
#   secret:   ...
#   expires:  ...
#
#   Run on the joining machine:
#     holler join --server ws://myhost.example.com:41807 --token <token_id>:<secret>

# 4. On the joining machine, run that printed `holler join` command — the `join`
#    subcommand ships in holler-client, not this repo

# 5. Still on the joining machine: `holler run` has no default sessions — every
#    session is explicit, in a `--config` TOML file (see holler-client's README
#    for the exact format). Without one, `run` connects but drives nothing, and
#    every `say`/`interrupt` from here will fail `unknown_session`.
holler run --config sessions.toml

# 6. Back here, see who's joined and talk to a session
holler roster
holler say <session> "hello"
```

`holler status`, `holler caps`, and `holler token list` are the other day-to-day operator commands; run any of them with `--help` for the full flag list.

## Why this is different

| Difference | Benefit |
| --- | --- |
| Self-hosted, not routed through a vendor's cloud (vs. Warp publishing sessions through Warp's relay) | You control your own data path end to end — no third party's infrastructure ever sees your session traffic, and nothing stops working because another company's cloud has an outage or the company itself stops operating it |
| Minted, revocable token identity — mint / list / ping / delete (vs. Warp's shareable link) | Durable, auditable, centrally-managed identity per machine: you can see exactly what was minted and kill it on demand, instead of "whoever has the link has access until someone remembers to revoke it" |
| A persistent roster of joined machines (vs. republishing a one-off session each time) | Standing infrastructure you can see at a glance — what's joined, what's alive — rather than re-publishing and re-sharing a link every time you want someone else in |
| Direct connection with nothing in the middle — loopback `ws` or your own `wss`/TLS (vs. Warp's Remote Control publishing every session to Warp's cloud, **even when the agent inside it is a third-party one like Claude Code or Codex** — Warp's own docs confirm the cloud relay is required for Remote Control regardless of which agent you chose) | No third party has access to session content or commands, and the connection keeps working even if a third-party cloud has an outage. Picking a different agent doesn't get you out of the intermediary with Warp — it's tied to the *feature*, not the agent |
| Works with any harness that speaks ACP — a new one is a config row, not a vendor decision (vs. Warp's Remote Control being limited to a fixed, vendor-curated agent list: Claude Code, Codex, OpenCode as of this writing) | Not gated by which agents a third party has chosen to support — a harness Warp hasn't added yet, or never will, works with Holler the same day |
| Cooperative interrupt as a protocol-level primitive — ACP `session/cancel`, session survives (vs. UI-level approve/redirect controls) | The guarantee is structural, part of the wire protocol both sides implement, not dependent on a specific client app's UI behavior |
| Self-hosted with no cloud intermediary at all, regardless of either party's license (Warp open-sourced its terminal client in April 2026 under AGPL-3.0/MIT, but the cloud relay and agent-orchestration backend that actually routes sessions — Oz — remains closed; Holler's own source is not currently public, license `Proprietary` in `Cargo.toml`) | Even Warp's own open-source move doesn't let you audit the piece that sees your traffic — the client being open doesn't open the relay. Holler sidesteps the question entirely: because there's no third-party intermediary in the first place, there's nothing closed-source standing between you and your own session data to worry about |

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

## Architecture

Two hops, two protocols, and the client is the hinge. Meta-O talks to `holler-server` over the CLI; `holler-server` and `holler-client` talk **Holler protocol v1** (JSON over WebSocket, default port `41807`); `holler-client` talks **ACP v1** (JSON-RPC over stdio) to the actual coding-agent subprocess on that box. `holler-server` never speaks a harness's native API directly — that's `holler-client`'s job, on the other side of the wire.

See [how server and client talk](docs/protocol/talk.md) for the interrupt and prompt/reply sequence diagrams, and the [Holler v1 spec](docs/protocol/v1.md) for the wire format itself.

## Documentation

- [Project intent (issue #1)](https://github.com/Performant-Labs/holler-server/issues/1) — the brief and the success test this project is building toward
- [Development environment](docs/dev-env.md)
- [Protocol index](docs/protocol/README.md) — every protocol Holler uses
- [How server and client talk](docs/protocol/talk.md)
- [Holler v1 spec](docs/protocol/v1.md)
- [Architecture decision records](docs/adr/README.md)

## Status and license

The [v1 epic](https://github.com/Performant-Labs/holler-server/issues/27) is complete: all 13 builder-order stories are closed, and the shared acceptance gate — mint, join, roster, independent prompts to two live sessions, cooperative interrupt with sibling-session isolation, and clean detach — has passed end-to-end against real OpenCode sessions, not just a fixture.

A handful of small, non-blocking follow-ups from that gate run remain open: roster staleness on an explicit disconnect, an unwired `sessions` count in the status document, and an error-message wording issue on an interrupted `say`. None of them affect correctness of routing, interrupt, or isolation.

License is `Proprietary` (see `Cargo.toml`) — this isn't an open-contribution project at this stage.

## Companion repo

[holler-client](https://github.com/Performant-Labs/holler-client) — the client half of the circuit: joins with a server-minted token and drives the ACP subprocess on the far machine.
