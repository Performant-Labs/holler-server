# Research memo — hijacking and denial of service: can the circuit be taken over or knocked over?

**Status:** research / discussion — not a decision record. This is not an ADR; ADR slots
[#14–26](https://github.com/Performant-Labs/holler-server/issues) stay reserved for future
decisions, and any recommendation below the team adopts should become its own ADR.

**Question asked (verbatim):** "we need to discuss how to ensure the communication stream
isn't hijacked by a malicious actor or slammed with requests that cause a DOS outage. What is
best practice? What do other tools do?"

Two distinct concerns, kept separate throughout: **(1) hijacking** — can an attacker take over,
impersonate, or inject into an established, authenticated connection; **(2) denial of service**
— can an attacker, or just a buggy client, overwhelm the server with connections, auth
attempts, or messages.

**Scope note — sibling research.** Two related memos were commissioned in parallel:
`research/message-integrity` (pushed, [`docs/research-message-integrity.md`](https://github.com/Performant-Labs/holler-server/blob/research/message-integrity/docs/research-message-integrity.md))
covers whether bytes arrive *intact* — a different question from whether they arrive from the
*right party* or arrive *too fast to handle*. It already did the legwork on what TLS 1.3's AEAD
ciphers guarantee and why loopback `ws` doesn't meaningfully expose TCP's weak checksum to a
network adversary (there's no wire to flip bits on). This memo leans on that finding rather than
re-deriving it (§1 below) and does not re-litigate it. `research/dropped-connections` was
scoped to reconnection/presence liveness; at the time of writing it exists as a local worktree
(`ada4435`, same tip as this branch) with no `docs/research-*.md` file committed yet — its
research deliverable hadn't landed when this memo was written, so there is nothing to
cross-reference yet. The one place the two topics clearly touch: reconnect-with-backoff is a
natural DoS-adjacent control (it's what stops a flaky client from becoming an unintentional
flood), noted where relevant in §3.

All code claims below were checked directly against `origin/main` (commit `ada4435`, "WebSocket
listener + holler status — first talk (server) (issue #31)"), not guessed or taken from docs.

---

## 1. Hijacking: where the real boundary is, and what's already solved

Holler's auth model is deliberately **not** network-trust-based — [ADR-0010](adr/ADR-0010.md) is
explicit that Tailscale/SSH/LAN are underlay, not identity, and the credential presented in the
`auth` frame is the actual security boundary. That framing is the right one to evaluate hijacking
against: the question isn't "is the network trusted," it's "does possessing the credential (or
sitting on the path) let an attacker act as the legitimate peer."

### 1.1 `wss` (non-loopback, TLS 1.3) — solved by TLS itself

[ADR-0004](adr/ADR-0004.md)/[ADR-0010](adr/ADR-0010.md) already require `wss` off loopback, with
AES-256-GCM or ChaCha20-Poly1305 records. An AEAD-protected TLS 1.3 connection is
authenticated *and* encrypted for its whole lifetime — an on-path attacker can't inject frames
into it, replay a captured ciphertext record into a live session, or splice their own traffic in
without the connection failing closed (a forged/altered record fails the AEAD tag check and the
connection tears down). This is the same mechanism the message-integrity memo already
establishes gives integrity "as a side effect of confidentiality." **Hijacking a `wss` session
requires breaking TLS 1.3 itself** — out of scope for anything short of a nation-state adversary,
which ADR-0010 already disclaims as the threat model ("resist casual/automated attack," X25519MLKEM768
offered specifically for the *harvest-now-decrypt-later* class, not real-time injection). No
gap here.

One classic WebSocket-specific hijack vector is worth naming explicitly *because it's the first
thing "WebSocket hijacking" means to most security references*: **Cross-Site WebSocket
Hijacking (CSWSH)** — a malicious webpage causes a victim's browser to open a WebSocket to a
vulnerable server, and if the server trusts ambient browser credentials (cookies) without
validating the `Origin` header, the attacker's page talks to the server *as* the victim. The
[OWASP WebSocket Security Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/WebSocket_Security_Cheat_Sheet.html)
leads with this exact attack, citing the 2023 Gitpod account-takeover as the canonical real-world
instance, and recommends an explicit `Origin` allowlist plus not relying on cookies alone.
**This structurally does not apply to Holler.** holler-client is a CLI process, not a browser;
there is no ambient/cookie-based credential and no page that could silently ride a victim's
session. Auth is an explicit `auth` frame with a credential the client process itself holds —
the CSWSH precondition (browser auto-attaches credentials to any page's cross-origin request)
doesn't exist here. Confirmed by reading `connection.rs`: the server never inspects an `Origin`
header at all (`tokio_tungstenite::accept_async` doesn't check one), which would be a real gap
*if* a browser-facing surface existed — it doesn't today, but note it as a design constraint if a
future web UI is ever added as a body: **that surface would need Origin validation from day one**,
this analysis doesn't transfer to it for free.

### 1.2 Plain `ws` (loopback-only, v1 default) — no TLS, so what's the actual residual risk?

`ws` carries no confidentiality or per-record authentication at all. The question is what stops
another party from reading or injecting into that TCP stream, given it's loopback-restricted by
[ADR-0004](adr/ADR-0004.md) and the listener code (`src/wire/mod.rs`) fails closed
(`NonLoopbackWithoutTls`) on any non-loopback bind.

On a real OS (Linux/BSD/macOS/Windows), loopback traffic between two processes is not
observable by an arbitrary third process the way traffic on a shared LAN segment is. Capturing
it requires either: (a) **root / `CAP_NET_RAW`** — packet capture on `lo` needs elevated
privilege on every mainstream OS, same as any other interface; or (b) **being the same OS user**
as one of the two endpoints, at which point that "attacker" already has full code-execution
equivalence with the account whose session they'd be hijacking — reading holler-client's
credential file directly would be strictly easier than a TCP capture; there's no meaningful
privilege boundary being crossed. A different unprivileged user on a shared multi-user box, or a
process in a different container/namespace, cannot sniff or inject into another user's/
container's loopback connection without first escalating — a scenario no amount of
Holler-application-layer hardening addresses, because at that point the attacker already
controls a peer OS security boundary Holler explicitly sits *above*, not *instead of*.

**Conclusion: for the stated threat model (single operator, homelab-or-workstation box,
"resist casual/automated attack"), loopback `ws`'s residual hijack risk is bounded by OS process
isolation, which is already doing real work and isn't a gap this project needs to close in v1.**
This is consistent with — not a stretch beyond — what [ADR-0009](adr/ADR-0009.md)'s fail-closed
philosophy already assumes about the trust boundary. If Holler's threat model ever needs to
include "an untrusted, unprivileged co-tenant process on the same box" (e.g. a genuinely
multi-tenant shared server), that's a materially different problem than what v1 is built for and
would deserve its own ADR at that time — flagging it as a real future boundary, not a present gap.

### 1.3 Replay of the `auth` credential

Per [ADR-0003](adr/ADR-0003.md), reconnect intentionally reuses the same long-lived client
credential — "Reconnect uses the credential again. A new join token is only for a new pairing."
Reading `token/mod.rs::verify_credential`, this is confirmed exactly as documented: it's a
constant-time HMAC comparison against a stored hash, it does **not** consume or rotate anything
on success, and there is no nonce, timestamp, or freshness proof anywhere in the `auth` frame
(`docs/protocol/v1.md` §4–5 confirms the envelope's `ts` field is never validated against a
window — the message-integrity memo notes it's "opaque, never-parsed" for codec purposes, and
nothing in `connection.rs` treats it as a freshness check either).

Is this a defect? **No — it's the standard shape of a bearer credential** (an API key, a GitHub
PAT, a Stripe secret key), and the join-token half of the design already applies the *other*
standard mitigation where it actually matters: the short-lived **join secret** is genuinely
single-use and TTL-bound (`redeem` fails closed on `AlreadyBound`/`Stale`/`Revoked` — verified in
`token/mod.rs`), which is exactly the device-pairing pattern
[ADR-0003](adr/ADR-0003.md) cites Cloudflare's agent-WS guidance for. A long-lived credential's
replay resistance, industry-wide, doesn't come from adding a nonce to the bearer secret itself
(that just moves the problem to "now the nonce store needs to be replay-proof") — it comes from
(a) **channel confidentiality** while the secret is in transit (TLS for `wss` — already solved,
§1.1) or **OS-boundary confidentiality** (loopback `ws` — already argued adequate, §1.2), and
(b) **revocability**: `holler token delete <id>` already revokes a bound credential outright
(`TokenRecord::state → Revoked`, `credential_hash` cleared so it "can never validate a credential
again even by accident" — direct quote from the code comment). That is the correct, standard
mitigation path for a leaked bearer credential, and it already exists. **No design change needed
here either** — the honest caveat is that nothing *detects* a leak or an anomalous reuse for the
operator (no "this credential was just used from somewhere it's never connected from before"
signal); at Holler's stated scale (one operator, a handful of tokens) that's reasonably left as
"the operator notices something's wrong and revokes," not a feature v1 needs to build.

### 1.4 A genuinely open, concrete finding: silent supersede on re-auth

This one **is** a real gap, found by reading `wire/registry.rs::insert` and
`wire/connection.rs` together, not by generic best-practice pattern-matching:

`Registry::insert` is documented as "Replaces any prior entry for the same `token_id` (a
reconnect from the same client supersedes its old, presumably now-dead, socket)" — and the code
does exactly that: a `HashMap::insert` on the same key silently drops the old `Entry`, including
its `out_tx` sender. But **nothing closes the old connection's actual WebSocket** when this
happens. The old connection's `handle_connection` task keeps running its `tokio::select!` loop
untouched; it just silently stops being reachable via the registry (future `ping`/`status`
traffic goes to the new connection only). If a second party — having obtained the same credential
by any of the means in §1.3 — opens a new authenticated connection while the legitimate client's
original connection is still alive, **both sockets are live simultaneously**, only one is
tracked, and the legitimate client gets no signal that anything happened (no `error` frame, no
close). This is a real "session-identity confusion" primitive sitting directly on top of the
already-acceptable bearer-replay design in §1.3 — it's not new risk from network exposure, but it
*is* a needless amplifier of whatever a stolen credential would do in the first place, since it
lets an attacker's connection and the legitimate one coexist and race, instead of the server
enforcing "one live connection per token" the way the registry's own doc comment already assumes.

**Recommended fix (cheap, correct, not a design change):** when a new `auth` succeeds for a
`token_id` that already has a live registry entry, send that old connection an `error`
(a new code, e.g. `session_superseded`) and close its socket before/while installing the new
entry — matching what the doc comment already claims happens ("supersedes its old... socket")
but the code doesn't yet do.

---

## 2. Denial of service: what's actually in the code today (verified, not assumed)

Read directly: `src/wire/mod.rs` (`accept_loop`, `serve`), `src/wire/connection.rs`
(`handle_connection`, `next_text_frame`), `src/token/mod.rs` (`verify_credential`), `Cargo.toml`
(dependency versions).

**Confirmed absent, as of `ada4435`:**

- **No connection cap.** `accept_loop` calls `listener.accept()` in an unconditional loop and
  `tokio::spawn`s a handler for every accepted socket — no ceiling, no back-pressure, no
  rejection path at all.
- **No pre-auth timeout.** `next_text_frame` (the function that waits for the first, mandatory
  `auth` frame) loops on `read.next()` with no `tokio::time::timeout` wrapper. A connection that
  opens and sends nothing — or only WebSocket `Ping`s, which the loop explicitly `continue`s
  past — occupies a spawned task and an open socket **indefinitely**. This is a textbook
  slow-loris-shaped exposure: cheap for an attacker (or a hung client) to hold open many such
  connections, unbounded by anything in this code path.
- **No failed-auth throttling or lockout**, per-token or per-source. `verify_credential` fails
  closed correctly (constant-time compare, no state mutation on failure — already good) but nothing
  counts failures or slows down a repeat offender.
- **Peer address is discarded.** `accept_loop` destructures `Ok((stream, _peer))` — the remote
  address is never captured, logged, or usable as a throttling key even if a per-IP control were
  added later. This is the one-line prerequisite any per-IP mitigation in §3 would need first.
- **No app-chosen frame/message size limit.** `handle_connection` calls
  `tokio_tungstenite::accept_async(stream)` with no `WebSocketConfig` — this takes the library
  default, not an intentional Holler choice. [`tungstenite`'s own docs](https://docs.rs/tungstenite/latest/tungstenite/protocol/struct.WebSocketConfig.html)
  give that default as **64 MiB max message size / 16 MiB max frame size** — generous headroom
  for a protocol whose entire v1 vocabulary (`docs/protocol/v1.md`) is small JSON control
  envelopes with no file-transfer or binary payload at all (binary frames are explicitly rejected,
  confirmed in `connection.rs`'s `Message::Binary` arm). Per-connection, this is a 64 MiB
  memory-amplification factor sitting on a code path with no connection cap either — the two
  absences compound.
- **Blocking file I/O inside the async accept/auth path.** `TokenStore::load`/`save` use
  synchronous `std::fs::read_to_string`/`fs::write` (confirmed in `token/mod.rs`), and
  `verify_credential` calls `load()` on *every* `auth` attempt (successful or not — it's how it
  finds the record to check against). This runs on whatever tokio worker thread is servicing that
  connection's task, blocking it for the syscall's duration. This isn't itself a security
  vulnerability, but it's the concrete mechanism by which "slammed with connections" becomes
  worse than a bare connection-count problem: every one of those connections' auth attempts
  contends for the same small worker-thread pool doing blocking disk I/O, not just memory.
  Standard fix if this becomes a priority: `tokio::task::spawn_blocking` around the store call, or
  make the store's I/O genuinely async — small, targeted, independent of the DoS-specific
  recommendations below.

**Confirmed present and already correct** (worth naming so the gaps above read as targeted, not
as "the whole thing is unhardened"): constant-time credential/secret comparison via
`Mac::verify_slice` ([issue #30](https://github.com/Performant-Labs/holler-server/issues/30)),
fail-closed error responses (not silent drops) on bad auth/unknown type/unsupported version per
[ADR-0009](adr/ADR-0009.md), no state mutation on any failed auth/redeem path, and binary frames
rejected outright rather than silently buffered.

## 3. What comparable tools and the closest real-world analog (SSH) actually do

### 3.1 SSH — the closest fit for Holler's actual threat model

Holler's own [ADR-0001](adr/ADR-0001.md) explicitly distinguishes itself from SSH ("Do not
become... SSH as the app protocol"), but as a **hardening precedent** SSH is the much closer
analog than any multi-tenant public API: a listening daemon on a box, authenticated by a
credential, meant to resist casual/automated attack rather than a nation-state adversary — which
is exactly [ADR-0010](adr/ADR-0010.md)'s own framing. `sshd`'s documented defaults
([`sshd_config(5)`](https://man7.org/linux/man-pages/man5/sshd_config.5.html)):

| Control | Default | What it bounds |
| --- | --- | --- |
| `LoginGraceTime` | 120s | Max time an unauthenticated connection may stay open before the server disconnects it — directly analogous to the missing pre-auth timeout in §2. |
| `MaxAuthTries` | 6 | Max auth attempts *within one connection* before it's dropped. |
| `MaxStartups` | `10:30:100` | Caps **concurrent unauthenticated connections**; past the first number (10), a rising percentage (30%) are randomly dropped, rising to 100% at the ceiling (100) — connection-level backpressure specifically on the pre-auth population, leaving already-authenticated sessions untouched. |

The operationally common *second layer* on top of stock `sshd` is [fail2ban](https://wiki.archlinux.org/title/Fail2ban):
typical `sshd` jail defaults are `findtime` 10 minutes / `maxretry` 5 / `bantime` 10 minutes (many
guides recommend tightening `bantime` to 1 hour) — i.e., "5 failures from one source in 10
minutes → temporary ban." Notably fail2ban is a **separate, general-purpose tool** watching log
output, not code baked into `sshd` itself — itself a data point for §4's "app vs.
proxy/firewall layer" question.

### 3.2 The ADR-0001 competitive set, checked specifically for this angle

| Project | What was found |
| --- | --- |
| [c2c](https://github.com/clankercode/c2c) | No documentation of rate limiting, connection throttling, or DoS protection found for the relay/broker. Docs describe the wire envelope and `--token` auth, nothing about abuse resistance. |
| [ai-crew-sync](https://github.com/joaquinbejar/ai-crew-sync) | No documentation of rate limiting or DoS protection found for the Postgres-backed agent bus itself (the earlier message-integrity memo separately found an at-least-once **webhook delivery** guarantee, a different concern). |
| **[Buzz](https://github.com/block/buzz)** | **The most directly useful precedent found.** Buzz's own `ARCHITECTURE.md` documents a `RateLimiter` trait in `buzz-auth` with a `RateLimitConfig` defining **four tiers** (human, agent-standard, agent-elevated, agent-platform) — i.e., the team explicitly designed for tiered rate limiting. But the doc is equally explicit that it isn't enforced: *"Does NOT: implement the `RateLimiter` beyond a test stub (`AlwaysAllowRateLimiter`...). No Redis-backed rate limiter exists anywhere in the codebase — rate limiting is not currently enforced."* This is real signal: the architecturally closest sibling in the whole survey recognized the need, designed an interface for it, and still shipped without enforcing it — supporting the read (§4) that this is a legitimate, common scope/sequencing call, not something Holler is uniquely behind on. |
| [Herdr](https://herdr.dev/docs/socket-api/) | Documents a `rate_limited` possible-reason code, but only on the `notification.show` method (desktop-notification delivery), not on the socket API's connection/auth path generally. Not a security control in the sense this memo is asking about — it's a UX throttle for one feature. |
| Harness Remote, opencode-orchestrator, Claude Code cross-session messaging, ai-crew-sync (auth angle) | No accessible documentation found addressing rate limiting, connection-auth throttling, or DoS protection specifically. Silence noted plainly rather than inferred either way. |

### 3.3 OWASP's general WebSocket guidance

The [OWASP WebSocket Security Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/WebSocket_Security_Cheat_Sheet.html)
converges on the same shape as SSH's, translated to WebSocket specifics: message size caps
("typically 64KB or less" — note this is far below tungstenite's 64 MiB *library* default, §2),
a message-rate limit ("100 messages per minute is a common starting point"), connection caps
preferring per-user over per-IP where identity is known (Holler's `token_id` is exactly such an
identity once authenticated), idle timeouts, and heartbeat-based dead-peer cleanup (which Holler
already has via its own `ping`/`pong`, per the message-integrity memo). None of these numbers are
Holler-specific — they're generic starting points, cited as calibration, not a mandate.

---

## 4. Recommendations, right-sized to a single-operator, self-hosted control plane

The epic's own [non-goals](https://github.com/Performant-Labs/holler-server/issues/27) list
(PTY mux, Buzz clone, LLM-answered `support`, browser e2e, killing remote processes) says
nothing about hardening or DoS protection either way — this is genuinely **undecided scope**,
not something already ruled in or out. The recommendations below are sorted by how confidently
each one is "just do it" versus "this is a real team call."

### Do now — cheap, unambiguous, no design trade-off

1. **Set an explicit, small `WebSocketConfig`** on `accept_async_with_config` instead of the
   64 MiB/16 MiB library default — something in the range OWASP suggests (tens of KB to low
   single-digit MB is still generous for Holler's small JSON control envelopes) closes a real
   memory-amplification factor for a one-line change.
2. **Wrap the pre-auth read in a timeout** (`tokio::time::timeout` around `next_text_frame`).
   SSH's `LoginGraceTime` default (120s) is more generous than Holler needs for a loopback/LAN
   client; something in the 10–30s range is plenty and directly closes the slow-loris-shaped gap
   in §2.
3. **Cap concurrent connections**, at minimum the *unauthenticated* population (mirroring SSH's
   `MaxStartups`) — a semaphore or simple counter around `accept_loop`'s spawn point. A number
   like 50–100 is generous headroom for a personal tool while eliminating the "open N sockets and
   never send anything" resource-exhaustion path.
4. **Capture the peer address** (`_peer` is currently discarded) — a one-line change, and a
   prerequisite for any per-source control, including the ones below and independently useful for
   `noisy`-level debug logging.
5. **Close the old socket when a new `auth` supersedes it** (§1.4) — makes the registry's own
   documented behavior ("supersedes its old... socket") actually true, and removes a real,
   narrow hijack-adjacent amplifier at effectively no cost.

None of these require a design decision or trade off correctness against availability — they're
the kind of gap that exists because issue #31 was explicitly scoped as "first talk" (per its own
module doc: TLS and hardening were named as deliberate follow-on cuts, not oversights), not
because anyone decided against them.

### A genuine team call — worth doing, less obviously free

6. **Failed-auth throttling/lockout**, in SSH/fail2ban's shape (something like "N failures from
   one source within T minutes → refuse new connections from that source for M minutes").
   Grounded starting numbers from what was actually found in practice: fail2ban's own defaults
   (5 attempts / 10 minutes / 10-minute ban, commonly tightened to a 1-hour ban) or SSH's
   `MaxAuthTries` (6 per connection). The important caveat, specific to Holler and not generic:
   the credential space here is a 256-bit random secret behind a constant-time HMAC compare —
   brute-forcing `WrongCredential` is not a practical threat the way password-guessing is for
   SSH, so a dedicated lockout mostly defends against *connection churn/noise* (which item 3's
   connection cap and item 4's throttling infrastructure already substantially cover), not
   credential guessing per se. Worth doing if Holler ever runs somewhere genuinely reachable by
   strangers; a smaller incremental win than items 1–5 if it stays loopback-or-trusted-LAN as
   [ADR-0004](adr/ADR-0004.md) currently defaults it.

### Explicitly out of scope for v1 — belongs to the operator's network layer, not this app

7. **IP-level rate limiting at scale, geo/ASN blocking, adaptive throttling** — none of this
   matches Holler's stated threat model (single operator, not a multi-tenant public API), and the
   SSH-plus-fail2ban precedent itself argues for keeping this *out* of the app: fail2ban is a
   separate tool watching logs, not code inside `sshd`. If Holler is ever bound non-loopback on a
   box with real internet exposure, the standard-practice answer is a firewall allowlist,
   WireGuard, or a reverse proxy doing `nginx limit_conn`/`limit_req`-style throttling in front of
   it — not reimplementing that inside holler-server. Worth a short "operator hardening" note in
   the docs saying this explicitly, so it's a stated position rather than a silent gap.
8. **Volumetric/SYN-flood DDoS mitigation** — outside any single process's ability to defend
   against regardless of language or framework; that's OS/network/upstream-provider territory for
   any tool, Holler included.

---

## Summary of what's genuinely open vs. already solved

| Question | Verdict |
| --- | --- |
| Is `wss`/TLS-only-for-non-loopback ([ADR-0004](adr/ADR-0004.md)) sufficient for hijacking? | **Yes** — hijacking a `wss` session requires breaking TLS 1.3 itself, out of scope for the stated threat model. |
| Is loopback `ws`'s lack of TLS a hijacking gap? | **No** for the stated single-operator threat model — the residual risk is bounded by OS process/user isolation, which the design already sits above rather than instead of. |
| Is credential replay a design flaw? | **No** — it's the standard bearer-credential shape; the correct mitigations (channel confidentiality, revocability) already exist. The join-secret half (single-use + TTL) already goes further than the credential half needs to. |
| Is there a concrete, currently-real hijack-adjacent gap? | **Yes, one:** §1.4's silent-supersede-without-closing-the-old-socket. Small, cheap fix. |
| Does the code have DoS protection today? | **No** — confirmed no connection cap, no pre-auth timeout, no failed-auth throttling, no app-chosen frame-size limit, peer address discarded. All independently verified against `ada4435`, not assumed. |
| Is that a defect, given the epic's scope? | **No** — issue #31 was explicitly "first talk" scope; hardening was a named follow-on, not an oversight. It is real, currently-open scope, not yet decided either way. |
| What should happen now? | Items 1–5 above: cheap, unambiguous, no trade-off. Item 6 (failed-auth lockout) is a real prioritization call given the credential's brute-force-infeasible size. Items 7–8 belong to the operator's network layer by design precedent (SSH + fail2ban), not this app. |
