# Prior art: self-hosted heterogeneous "talk circuit" for coding agents

> Research memo — not a decision record. This is Grok's follow-up survey to
> [ADR-0001](adr/ADR-0001.md), going one layer further into other AI-coding
> vendors (competitors to OpenAI/Anthropic) to check whether any of them have
> built, or explicitly recommend installing, something in Holler's exact
> space. No decision is made here. Issues #14–26 in this repo are pre-reserved
> future ADR slots — this memo does not claim one; if the team wants to fold
> this into ADR-0001 or a new ADR later, that's a separate step.

**Claim that survives review:** vendors built account-routed remote controls for *their own* agents; Warp bolted a cloud publish button onto third-party CLIs; open-source rooms bolted MCP chat onto mixed agents; nobody shipped a self-hosted token control plane for live heterogeneous sessions.

**Failure modes used below**
- **A** — same-vendor single-identity across devices (Claude Dispatch's shape)
- **B** — no first-party cross-machine *session join/drive* story (SSH-into-a-project or resume-history does not count)

Axes for each vendor:
1. Control/message a coding-agent session on a *different* machine than the GUI — join, drive, interrupt a turn without killing the session, roster/status?
2. Same-vendor identity only, or heterogeneous harnesses?
3. Join/auth: minted revocable token (mint/list/delete/ping), account-based cloud routing, or other?
4. If the product does not do this, do they document/recommend a third-party tool?
5. Any repo/post that is an attempt at exactly this circuit?

---

## Google — Gemini CLI / Antigravity

1. **Yes, for Antigravity desktop sessions.** Remote Control (announced ~21 Aug 2026): browser, pick a nicknamed machine, view conversations, start tasks, review plans/artifacts, push notifications when the agent needs input.
   - Docs: https://antigravity.google/docs/remote-control/
   - Writeup: https://medium.com/google-cloud/take-your-ai-harness-with-you-with-antigravity-remote-control-25b1d1208bab
2. **No.** Drives Antigravity 2.0 sessions on the same Google account. Gemini CLI "remote subagents" are A2A *delegation*, not joining an independent Codex/Claude session.
   - https://geminicli.com/docs/core/remote-agents/
3. **Account-based cloud routing** (same Google account + dashboard). QR / copy-link. Machine nickname. Not mint/list/delete/ping join tokens.
4. Community only (`agys`, `antigravity-gemini-bridge`, unofficial coworker MCP). Not Google's recommended heterogeneous bus.
5. Nearby but different: Gemini Workspaces PR (GCP workers + tmux) = offload compute, not a join bus.
   - https://github.com/google-gemini/gemini-cli/pull/22714

**Failure mode: A**

---

## Cursor

1. **Yes, for Cursor Cloud Agents on "My Machines."** Worker on the box, outbound to Cursor. Steer from cursor.com, iOS, Slack/GitHub/Linear. `/remote-control` hands a *local* Cursor agent to the phone; tools stay on-machine, agent loop moves to Cursor cloud.
   - https://cursor.com/docs/cloud-agent/my-machines
   - https://www.productcompass.pm/p/cursor-remote-control
2. **No.** Cursor's own agent only. Third-party writeups: other agents (e.g. Grok Build) are out of reach.
3. Cursor account + worker registration. Minted *worker* artifacts exist: API keys, `POST /v1/sub-tokens`, `--auth-token-file`. Those auth **workers to Cursor's cloud**, not a hub that lists/joins arbitrary harness sessions.
4. Phone companions / session-sync extensions exist. Cursor does not point users at a self-hosted heterogeneous bus.
5. —

**Failure mode: A** (tokenized worker plane, still Cursor-identity)

---

## Windsurf (Codeium) / Cascade

1. **Not first-party.** Multiple local Cascades + Windsurf 2.0 Agent Command Center (local Cascade + cloud Devin). IDE fleet UI, not joining a headless Cascade on another box.
   - https://www.wwt.com/article/partner-overview-windsurf-20-introducing-the-agent-command-center-and-devin-in-windsurf
2. Cascade + Devin only, inside Windsurf's product.
3. Account / IDE session. No minted join tokens for remote Cascade.
4. Unofficial `wsc` CLI; CodeAgent Mobile pairs a phone to local Cascade via 6-char code/QR. Not vendor-recommended as a talk circuit.
   - https://github.com/staroneLabs/windsurf-cli
   - https://www.codeagent-mobile.com/control-windsurf-from-phone
5. —

**Failure mode: B** for first-party session-join (Devin handoff = cloud delegation)

---

## Cognition — Devin

1. **Yes, of Devin sessions.** Outposts: `devin worker start --outpost=…` claims queued Cloud sessions; tools run on your machine; message/watch/take over from Devin Cloud (shell, IDE, browser, stop mid-answer). "Devin can manage Devins" = same-product swarm.
   - https://devin.ai/blog/introducing-devin-outposts
   - https://docs.devin.ai/cloud/outposts/overview
2. **No.** Outpost workers serve Devin only. OSS "Devin Handoff" *starts* a Devin cloud session from Claude/Codex/Cursor — one-way dump, not a shared bus.
   - https://docs.devin.ai/api-reference/common-flows
3. Outpost token (`DEVIN_OUTPOSTS_TOKEN`) or CLI login; claim returns `connect_token` + gateway URL for `devin-remote`. Account-cloud + worker token, Devin-only.
   - https://docs.devin.ai/cloud/outposts/reference
4. `npx devin-remote` = local ACP web console for Devin CLI — still Devin.
5. —

**Failure mode: A**

---

## Replit Agent

1. Agent lives in Replit's cloud workspace. SSH into a Repl is remote shell/editor access, not joining a headless third-party agent on your hardware.
   - https://replit.com/blog/ssh
2–3. Same Replit account / workspace. No heterogeneous token bus.
4. —

**Failure mode: B**

---

## GitHub Copilot / VS Code Agent Host (Microsoft)

Two stacked stories.

**Copilot CLI remote control.** `/remote on` (or `copilot --remote`) mirrors a running Copilot CLI session to github.com / GitHub Mobile. Monitor, follow-ups, approve/deny, stop. Session stays on the original machine. GitHub auth; typically a GitHub-backed repo. Same GitHub identity.
- https://github.blog/news-insights/product-news/take-your-local-github-sessions-anywhere/
- https://docs.github.com/copilot/concepts/agents/copilot-cli/about-remote-control
- https://docs.github.com/copilot/how-tos/copilot-cli/steer-remotely

**VS Code Agent Host + AHP.** Sessions owned by a host process; Agents window attaches over SSH or a dev tunnel (host auto-installs VS Code CLI). Browser client: `insiders.vscode.dev/agents`. VS Code can *discover* local Copilot CLI, Claude Code, and Codex sessions; they stay "external" until you send a message, then the Agent Host adopts them. Handoff between harnesses inside VS Code is first-class.
- https://code.visualstudio.com/docs/agents/remote-agent-sessions
- https://code.visualstudio.com/docs/agents/concepts/sessions
- https://code.visualstudio.com/blogs/2026/08/26/agent-host-architecture

1. Yes for Copilot sessions (account remote control) and for whatever the remote Agent Host will run.
2. Partial heterogeneity **inside VS Code's Agent Host** (Copilot / Claude / Codex as targets), not a vendor-neutral bus you point OpenCode or Amazon Q at with a minted token. Copilot `/remote` itself is Copilot-only.
3. GitHub account + SSH/dev-tunnel. Not self-hosted mint/list/delete/ping.
4. No official "install this third-party talk circuit."

**Failure mode: A** for Copilot remote control.
**AHP is the most serious first-party protocol prior art** for "host owns sessions, clients attach," but the host is Microsoft's, not your token server.

---

## Amazon Q Developer

1. **No session-join product.** SSH integration so Q CLI/completions work *on* a remote Linux box (`q integrations install ssh` + `AcceptEnv Q_SET_PARENT`). Remote MCP servers (HTTP + OAuth) are tool backends, not a roster of coding sessions.
   - https://docs.aws.amazon.com/amazonq/latest/qdeveloper-ug/command-line-remote-machines.html
   - https://aws.amazon.com/about-aws/whats-new/2025/09/amazon-q-developer-remote-mcp-servers/
2–4. Q-only. Custom agents are local config profiles. No recommended third-party join bus.

**Failure mode: B**

---

## Zed

1. Remote Development = SSH: local UI, remote LSPs/tasks/terminals; Agent Panel works in that remote project. Remoting the editor, not attaching to an already-running foreign agent with a token. ACP embeds Gemini CLI / other ACP agents *in Zed*. GitHub discussion: Claude Code `/remote-control` inside Zed's panel is missing.
   - https://zed.dev/docs/remote-development
   - https://zed.dev/blog/bring-your-own-agent-to-zed
   - https://github.com/zed-industries/zed/discussions/59569
2. ACP = multi-agent-in-editor, same machine/project.
3. SSH credentials.
4. Community: Zedra (GPUI + Iroh P2P, QR pair). Not Zed-official.
   - https://github.com/tanlethanh/zedra

**Failure mode: B** (SSH remoting ≠ token-joined session)

---

## Warp

Closest **shipping vendor product** to "drive a session that isn't Warp's own agent."

1. **Yes.** `/remote-control` publishes a running third-party CLI session; from phone/browser/another computer: monitor, send input, approve commands, redirect. Separate "Agent Session Sharing" = live multi-viewer collab.
   - https://docs.warp.dev/agents/cli-agents/remote-control/
   - https://docs.warp.dev/agents/local-agents/session-sharing/
2. **Heterogeneous at the CLI layer:** Claude Code, Codex, OpenCode, etc. Control surface is still Warp.
   - https://x.com/warpdotdev/status/2045253339055595649
3. Warp uploads session state and mints a **shareable cloud link**. Anyone with the link can view; edit/steer is granted. Publisher revokes / stops publishing. **Not** a self-hosted mint/list/delete/ping API.
4. Warp *is* the third-party remote-control layer other vendors didn't build. They do not tell you to self-host an alternative.

**Failure mode: neither A nor B — heterogeneous, but cloud-routed and link-based.**
This is the product you will be asked "how are you different from Warp?" about.

---

## Continue.dev

1. `cn remote` launches a **Continue** agent on Continue's remote/devbox infra and returns a URL. `cn serve` = HTTP server mode. Cloud agents on Mission Control. Local list/resume: `cn ls`, `cn --resume`.
   - https://github.com/continuedev/continue/tree/main/extensions/cli
2. Continue's own agent (models pluggable; harness is Continue).
3. Continue account / `CONTINUE_API_KEY`.
4. No official pointer to a heterogeneous talk circuit.

**Failure mode: A** (Continue-identity remote instances), plus **B** for joining someone else's Claude/Codex process.

---

## Amp (Sourcegraph)

Amp Neo CLI streams a local Amp thread to Amp's web UI: follow-ups, queue, interrupt, cancel. Later: remotely launch Amp agents on any machine that can run `amp`. Still Amp-only, Amp-account / Amp-cloud.
- https://thenewstack.io/amp-neo-cli-agents/
- https://x.com/sqs/status/2074835614625919119

**Failure mode: A**

---

## OpenCode (open-source harness)

Native `opencode serve` / `opencode web` / `opencode attach` — TUI or `run --attach` talks to a remote OpenCode backend with basic-auth user/password. **OpenCode-to-OpenCode** attach, not a multi-harness token bus.
- https://www.opencode.asia/cli/

Community `@lesquel/opencode-pilot`: dashboard, rotating token, QR, tunnel, Telegram — OpenCode + Codex.
- https://github.com/lesquel/open-remote-control

---

## Q5 — Projects that read like the talk circuit

None of these is a major vendor's GA product.

| Project | What it is | Token / join | Heterogeneous? | Self-hosted? |
|---|---|---|---|---|
| [agents-party](https://github.com/1gr14/agents-party) | Skill + CLI; Claude/Cursor/Codex share a channel; local file or `party:` server ref | Invite **ref** is the credential (`party:server/id#k=key`); guests need no account | Yes | Yes (`agents-party web`) + hosted |
| [agent-room](https://github.com/agent-room-alkl/agent-room) | MCP room: create → 9-char code → join/send/listen/admin | Short room code | Claude, Cursor, Windsurf, Codex, Gemini, Antigravity, OpenClaw | Yes |
| [harness-remote](https://github.com/giuliastro/harness-remote) | Control plane over *native* sessions; resume/handoff across machines and agents | HTTP basic user/password on your launcher | OpenCode, Claude Code, Codex, OMP, PI | Yes (on the machine that has the CLIs) |
| [remote-agent](https://github.com/d-kimuson/remote-agent) | Self-hosted web UI that owns ACP sessions | Bind + optional IP allowlist | Codex, Claude, Copilot CLI, Cursor CLI, OpenCode, custom ACP | Yes |
| [moltnet](https://github.com/noopolis/moltnet) | Agent chat network; join via `install.md` URL / invite | Invite / join link | Claude, Codex, OpenClaw | Yes |
| [join.cloud](https://github.com/kushneryk/join.cloud) | Rooms + git; MCP/A2A/HTTP | `agentToken` from `createRoom` | Any MCP/A2A agent | Yes |
| [caucus-mcp](https://github.com/obeone/caucus-mcp) | Supervised hub; human pause/stop/kick | MCP session join | Any MCP client | Local hub |
| [ninjamcp](https://github.com/steveseguin/ninjamcp) | P2P rooms via VDO.Ninja | Optional `join_token` | Any MCP client | P2P + optional token |
| [cross-agent-teams-mcp](https://github.com/jtianling/cross-agent-teams-mcp) | Teams across Claude/Codex/OpenCode; later cross-host | Daemon + named agents | Yes | Daemon; multi-host via bind/Tailscale |
| [AgentsRoom](https://agentsroom.dev/) | Desktop app that *spawns* many CLIs + SSH | App-local / SSH creds | ~14 providers | App on your machine, not a token server |
| [agentreach](https://github.com/bojieli/agentreach) | Point a local agent at an SSH/docker target | SSH | Claude/Codex/etc. as the *client* | N/A (redirects I/O) |
| [open-remote-control / Pilot](https://github.com/lesquel/open-remote-control) | Dashboard + QR for OpenCode/Codex | `/pilot-token` rotate | OpenCode + Codex | Local plugin |
| [Zedra](https://github.com/tanlethanh/zedra) | Mobile editor/agent remote over Iroh | QR pair | Agent-agnostic desktop daemon | P2P |
| Tactic Remote / CodeAgent Mobile | Phone companions | Pairing codes | Multi-agent (vendor-specific apps) | Companion server on the desktop |

**Adjacent, not the thing**
- Speakeasy session portability — same-machine handoff via on-disk transcripts, no network
  https://www.speakeasy.com/blog/release-agent-session-portability/
- VS Code Agent Host Protocol
- Gemini A2A remote subagents
- OpenAI Codex swarm-protocol (already excluded: Codex-to-Codex)

No vendor engineer found saying "install agents-party / agent-room / harness-remote instead of our Remote Control." The OSS cluster grew *because* vendors only shipped A.

---

## Is the gap real?

**Yes, across the paid field**, with one commercial near-miss.

- Almost every serious vendor independently reinvented **A**: same account, their cloud relays steering *their* agent (Antigravity Remote Control, Cursor Remote Control / My Machines, Copilot `/remote`, Amp Neo, Claude Dispatch, Devin Outposts, Continue `cn remote`).
- **Warp** is the exception on axis 2 (publishes CC/Codex/OpenCode) but fails axes 3–5: Warp cloud, share link, not a self-hosted mint/list/delete/ping server, not a roster of independent machines you own.
- **Microsoft Agent Host** is the exception on "sessions live in a host process clients attach to," but the host is VS Code's, auth is GitHub/SSH, and adoption of foreign harnesses is "discover then swallow," not a minted join token those harnesses present to *your* circuit.
- The exact tuple — **self-hosted server + revocable join tokens with mint/list/delete/ping + join an already-running session on a headless box + prompt + cooperative interrupt without kill + roster + any harness** — is not a GA feature of Google, Cursor, Windsurf, Devin, Replit, GitHub, Amazon, Zed, Warp, Continue, or Amp.
- Prior art to distinguish in a writeup: **Warp Remote Control**, **VS Code Agent Host / AHP**, **agents-party / agent-room / harness-remote / remote-agent**, **OpenCode serve+attach**. Pieces of the design space; none is the whole circuit.
