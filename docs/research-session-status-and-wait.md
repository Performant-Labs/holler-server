# Research memo — notifying an idle/blocked/failed session without a webhook or a "more disciplined" operator

**Status:** research / discussion memo — **not an ADR, not a decision**. Written to inform a future numbered ADR once the team has actually discussed and decided.
**Date:** 2026-09-08
**Related:** [ADR-0005](adr/ADR-0005.md) (interrupt is control, session survives), [ADR-0006](adr/ADR-0006.md) (presence is status), [ADR-0007](adr/ADR-0007.md) (session addressing), [protocol v1](protocol/v1.md), [research-dropped-connections.md](research-dropped-connections.md) (heartbeat/reconnect numbers this memo builds on), issue [#139](https://github.com/Performant-Labs/holler-client/issues/139) (the `session_blocked` push this memo generalizes, shipped in [holler-client#141](https://github.com/Performant-Labs/holler-client/pull/141)/[holler-server#388](https://github.com/Performant-Labs/holler-server/pull/388)).

## 1. Scope

Real-world observation running Holler live: a meta-orchestrator agent ("MO") — itself an LLM (Claude Code or OpenCode), using `holler-server roster`/`say`/`interrupt`/`answer` as its own tools to dispatch work to two other agent sessions — periodically **stops tracking its agents and wanders off onto an unrelated tangent**. Meanwhile a session it dispatched finishes a task and goes idle, unattended, until MO (or a human) happens to notice. Nothing today actively notifies anyone when this happens — it depends entirely on MO's own discipline to poll `roster`, which is exactly the thing failing.

This memo researches: whether to build a live wire push + blocking CLI wait, whether to borrow A2A's webhook pattern instead, what the useful session-state set actually is, and whether the real fix is a technical one at all or a prompting/architecture one for the operator agent itself.

## 2. Should this be a wire push + CLI wait, or an A2A-style webhook?

**Recommendation: wire push + CLI wait. Do not build webhooks for this.**

Two agent-interop protocols were checked directly for prior art (not assumed from memory):

- **ACP (Agent Client Protocol)** has no equivalent, and it's not really a gap — it's out of scope by design. ACP is a local, 1:1, stdio JSON-RPC session between whatever spawned the agent and the agent subprocess itself. The "task done" signal (`stopReason`) is delivered as the resolution of the *same* `session/prompt` call that started the turn — whoever made that call sees it the instant it happens, inherently. There is no concept of a third party subscribing to be told later; ACP has no notion of "someone else" at all.
- **A2A (Agent2Agent Protocol)** has a real, mature, first-class feature for exactly this: `tasks/pushNotificationConfig/set`. A client registers an HTTPS webhook URL (+ optional auth token); the A2A server `POST`s to it whenever a task hits a "significant state change" — explicitly including terminal states (completed, failed) *and* `input-required`/`auth-required` (roughly Holler's existing `Blocked`). It requires the server to advertise a `pushNotifications` capability, and the spec includes real production concerns: retry with exponential backoff, SSRF protection on the registered URL, config persisting until task completion or explicit deletion.

A2A's feature validates the *event* — "push on completion/significant-state-change" is a legitimate, previously-solved protocol concern, not something invented here. But its *transport* (a webhook — a new inbound HTTP listener Holler would need to stand up to receive its own callbacks) exists to solve a **disconnected listener** problem: a client that isn't holding a live stream open. Holler's hub and bodies already share a persistent, bidirectional WebSocket. A webhook here would mean SSRF checks, retries, and a second listener just so the hub could learn what the body already told it over the connection both sides already hold open. That's solving a problem Holler doesn't have.

The right move is to reuse the mechanism already shipped for exactly this shape of problem — `session_blocked` (client → hub push, sent live on a real transition, not just at connect/reconnect the way `presence` is; see [protocol v1](protocol/v1.md) and the merged PRs above) — generalized into a full state-transition frame, plus a blocking CLI verb on the hub side so any caller (not just something watching the roster) can wait on it directly without polling:

```
holler-server wait alpha,beta --until idle,done,blocked,failed --timeout 600000
```

## 3. Wire shape

One frame, not a new sibling of `session_blocked`:

```
session_status { session, from, to, stop_reason, turn_id, blocked?, ts }
```

`session_blocked` becomes `to=blocked` / `to=working` on this same frame — one mechanism, not two independently-evolving ones. `roster` stays the snapshot (current state per session, as today); this frame is the delta that keeps it live.

**Two design constraints, both borrowed from prior art that already paid for learning them the hard way:**

- **Edge-triggered, not level-triggered.** `wait` should return immediately if the target session already matches one of the requested states (this is [Herdr](https://herdr.dev)'s own `agent wait` behavior). After it returns, a caller must not immediately block again on the *same* transition or it spins — carry a `turn_id`/generation/sequence number on the frame specifically so a caller can distinguish "still the same settled turn" from "a genuinely new one."
- **`idle` ≠ `done`.** A session is `idle` at join, after an interrupt, after a refused prompt, *and* after a genuinely completed turn — treating bare `idle` as "finished the job I assigned" conflates all four. The useful event is `working -> settled`, carrying `stop_reason` (`end_turn|cancelled|error|blocked`). Herdr's own model draws this distinction directly: `idle` and `done` both mean "ready for input," but `done` is idle-that-nobody-has-focused-yet; a per-watcher "seen" watermark (pane focus, or here an explicit `holler-server ack <session>`) is what actually clears `done -> idle`. This is precisely "alpha finished and MO/human never looked" — the case this memo exists to close.

## 4. State set

| Holler state | Means | A2A analogue | Waited on by default? |
| --- | --- | --- | --- |
| `working` | Mid-turn | `WORKING` | no |
| `blocked` | Structured question/permission outstanding | `INPUT_REQUIRED` / `AUTH_REQUIRED` | yes |
| `done` | Settled after a turn, not yet acknowledged | `COMPLETED` + an unseen bit | **yes — this is the actual gap** |
| `idle` | Ready for input, and acknowledged/seen | — | optional |
| `failed` | Body/harness error, crash, HTTP 5xx, ACP error | `FAILED` | yes |
| `disconnected` | Client dropped; last known status is stale | — | yes |
| `unknown` | Present, cannot classify | — | opt-in only |

Default `--until` set for an unattended watchdog: `done,blocked,failed,disconnected` — deliberately **not** bare `idle`, for the reason in §3. Do not lump a crash into `idle` either: an attach-mode client that loses its OpenCode HTTP connection is `disconnected`/`failed`, not "quietly idle" — a watchdog that only waits on `idle` will miss exactly the case that matters most.

`holler-server roster` should grow a `STATUS` + `BLOCKED` + unseen-`DONE` shape from this (mechanically similar to how it already grew a `BLOCKED` column from `session_blocked`). `holler-server wait --until done` fires on `working -> settled` with `stop_reason=end_turn`; a later `say` to that session, or an explicit `holler-server ack <session>`, clears `done -> idle`.

## 5. Can this be solved by making the operator agent (MO) more disciplined instead?

**Not reliably — "an LLM that remembers to poll" is the actual bug, not a symptom a better prompt fixes.** Instructing MO to "check roster every N turns" fails the first time it finds something more interesting mid-loop; this is the exact failure already observed in production. The pattern that actually holds up across real systems: **don't keep the orchestrator sitting in a live wait/turn** — dispatch, then let it exit the turn; a deterministic, non-LLM waiter wakes it with a single message only when something requires judgment. Event in, LLM only for judgment — not the reverse.

This mirrors a general pattern, not a Holler-specific invention: a supervisor that blocks on a worker's completion signal and only then re-engages judgment (rather than an LLM "supervisor" polling on its own initiative) shows up repeatedly in adjacent systems — CI job-completion notifiers (`systemd OnSuccess=`/`OnFailure=`, GitHub Actions `workflow_run`) are exactly "job state machine + edge notify," durable-workflow systems (Temporal signals, Cloudflare Workers' `schedule()`) keep a waiter independent of the worker's own event loop, and none of them ask the *worker* to remember to check in — the waiter is external and deterministic by construction.

Concretely for Holler: keep MO as the judgment layer; put the attentiveness in a small, boring, non-LLM process instead (a shell loop or a systemd/launchd unit running `holler-server wait`, not an agent). A Holler session literally named `watch` — not an LLM, just the waiter script bound as an ordinary client — is one way to keep it inside the same system rather than a bespoke side script; either is fine for a first cut.

## 6. Prior art consulted

- [Herdr's own agent-automation docs](https://herdr.dev/docs/agent-automation/) — direct source for `wait`/`--until`/idle-vs-done/"already matches, return now" and the "submitting a prompt requires observing `working` first" gate. Herdr already ships `herdr agent wait <target> --until idle,done,blocked` (a real blocking primitive, no polling) and `herdr agent prompt <target> <text>` (inject a prompt into a pane directly) — a viable *alternative* implementation of this whole idea for anyone already running Herdr, but one that doesn't help a Holler deployment with no Herdr in the picture, which is why this memo scopes the fix to Holler itself rather than Herdr.
- [`barnuri/herdr-telegram-notifications`](https://github.com/barnuri/herdr-telegram-notifications) — notifies a human on exactly the same three transitions (idle, blocked, done). Evidence that this state set is sufficient in practice for a real notification tool, not just a theoretical framing.
- A2A's push-notification spec (§2 above) — confirms the *event* design is sound prior art; its *transport* doesn't fit Holler's already-connected topology.
- `systemd OnSuccess=`/`OnFailure=`, CI `workflow_run`-style notifiers — the "job state machine plus edge notify" framing, deliberately not modeled as an agent at all.
- Temporal signals / Cloudflare Workers `schedule()` — durable waiter independent of the worker's own loop, the same shape as "MO doesn't hold the wait itself."

## 7. What to build (narrow scope)

1. **Body**: on an ACP/OpenCode turn settling, emit `session_status` with `from`/`to`/`stop_reason`/`turn_id`. `Blocked` already exists (`session_blocked`) — fold it into this same frame rather than keeping two mechanisms.
2. **Hub**: `roster` gains `STATUS` / `BLOCKED` / unseen-`DONE` columns; persist last status per session (mirroring how `blocked: bool` was added to `Roster`/`RosterEntry` for `#139`).
3. **CLI**: `holler-server wait <sessions> --until <states> [--timeout <ms>]`. One line of stdout per match: session, state, reason, turn_id. Exit `0` on match, `2` on timeout, `1` on disconnect/error.
4. **Example unit, not a new daemon protocol**: `wait` → `holler-server say <session> "..."` → optionally a system notification (`notify-send`/ntfy/Telegram) to a human. A `systemd`/`launchd` unit or a Herdr pane running this script is enough for a first cut.
5. **Explicitly do not build, for this scope**: webhooks (an *optional* future hub egress — e.g. `holler-server hook add` — only if something genuinely off-circuit needs notifying later, not the body→hub path this memo covers); a PTY-based approach; an "MO heartbeat every 60s" prompt-engineering fix; defaulting `wait` to bare `idle`.

## 8. Summary — what's a fairly clear call vs. a genuine judgment call

| Question | Answer type |
| --- | --- |
| Wire push + CLI wait vs. A2A-style webhook | Fairly clear call — webhooks solve a problem (disconnected listener) Holler doesn't have on this already-connected circuit |
| Fold `session_blocked` into one `session_status` frame rather than keeping two mechanisms | Fairly clear call — avoids two independently-evolving state signals for the same underlying concept |
| Edge-triggered wait, `turn_id`/generation to avoid re-spinning | Fairly clear call — directly reuses a pattern Herdr already shipped and got right |
| `idle` ≠ `done`, with a "seen" watermark | Fairly clear call — this is the one piece most likely to get reinvented wrong if skipped; Herdr's own model is the reference |
| Exact default `--until` set for a watchdog (`done,blocked,failed,disconnected`) | Judgment call, grounded in the state table in §4 |
| Fix MO's attention via a better prompt vs. an external waiter | Fairly clear call, grounded in the general "event in, LLM only for judgment" pattern seen across CI/workflow/durable-execution systems in §6 |
| Whether the watchdog itself should be a plain script vs. a `watch` Holler session | Genuine open judgment call — either is a reasonable first cut |
| Whether webhooks are ever worth adding later | Left open — only if something off-circuit (not sharing the WebSocket) needs notifying, which is a different problem than the one this memo scopes |
