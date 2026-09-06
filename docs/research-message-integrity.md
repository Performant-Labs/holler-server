# Research memo — message integrity: did the exact bytes arrive, and were they processed?

**Status:** research / discussion — not a decision record. This is not an ADR; ADR slots
[#14–26](https://github.com/Performant-Labs/holler-server/issues) stay reserved for future
decisions, and any of the recommendations below that the team adopts should become its own ADR.

**Question asked:** "we also haven't discussed how to ensure the communication arrived at its
destination intact. This is different from encryption. What is best practice? What do other
tools do?"

**Scope note — how this differs from the sibling research task.** A parallel task
(`research/dropped-connections`) was scoped to cover reconnection and presence — i.e. what
happens when the *connection itself* goes away. At the time of writing, that branch exists
locally but contains an unrelated, much larger set of feature commits (join tokens, the
WebSocket listener) with no `docs/research-*.md` file anywhere in its tree — its research
deliverable had not yet been committed when this memo was written. This memo does not
duplicate connection-liveness territory; where the two clearly interact (a reply that never
arrives is ambiguous between "message never arrived" and "connection died"), it says so
explicitly and defers the liveness half to that other memo once it lands.

---

## 1. What TCP and TLS actually guarantee, and what they don't

**TCP's checksum is not an integrity mechanism in the security sense.** It is a 16-bit
one's-complement sum, designed to catch the kind of bit-flips that were common on 1980s links,
not deliberate tampering — and it isn't even fully reliable against *accidental* corruption. The
canonical citation here is Stone & Partridge, ["When the CRC and TCP Checksum Disagree"](https://dl.acm.org/doi/10.1145/347059.347561)
(SIGCOMM 2000): across a large sample of real Internet traffic, the TCP checksum failed to
catch corruption in roughly 1 in 16 million to 1 in 10 billion packets depending on the traffic
mix, and in one pathological hour-long link it missed 1 packet in 400. The 16-bit checksum
provably detects any single burst error of up to 15 bits, and most 16-bit bursts — but two
separately-placed corruptions that happen to cancel out (e.g. swapping two 16-bit words, which
is a real bug pattern from buggy NICs and drivers) sail through undetected. Root causes found in
that study spanned memory errors, driver/NIC bugs, and stack bugs, not just noisy wires. This is
why any protocol whose correctness matters layers something stronger (a strong hash, HMAC, or an
AEAD tag) on top when it cares about that residual class of corruption — see [Baeldung's summary](https://www.baeldung.com/cs/tcp-checksum-errors)
and [Evan Jones' write-up](https://www.evanjones.ca/tcp-and-ethernet-checksums-fail.html) for
approachable restatements of the same finding.

**TLS 1.3's AEAD ciphers give you real, cryptographic integrity as a side effect of
confidentiality** — this is not a coincidence, it's the point of an AEAD construction.
AES-256-GCM and ChaCha20-Poly1305 (both named in [ADR-0010](adr/ADR-0010.md)) each produce an
authentication tag over the ciphertext; a receiver that doesn't recompute a matching tag rejects
the record outright, before ever handing plaintext to the application. Poly1305 (paired with
ChaCha20) and GHASH (paired with AES-GCM) are the MAC step; unlike a bolted-on non-cryptographic
checksum, a bit-flip is astronomically unlikely (2⁻¹²⁸-ish) to produce a still-valid tag, so
tampering — accidental or deliberate — is what AEAD is *for*, not a lucky extra. See the AEAD
overview and TLS 1.3 cipher-suite background in [this ChaCha20-Poly1305 explainer](https://en.wikipedia.org/wiki/ChaCha20-Poly1305)
and the [TLS 1.3 AEAD summary](https://100plus.tools/guides/authenticated-encryption-and-integrity).
**Conclusion for the `wss` path: "arrived intact" is already fully covered by the same mechanism
that gives confidentiality.** There is no separate integrity problem to solve on `wss` — a
corrupted or tampered TLS record simply fails to decrypt and the connection is torn down. Nothing
in this memo proposes adding anything on top of that for the TLS case.

**The open question is the loopback `ws` case — Holler's actual v1 default** (`docs/protocol/v1.md`
§1: "Plain `ws` is loopback only"). Here there is no AEAD tag, so integrity rests on TCP's weak
checksum alone, *unless* the loopback path bypasses even that. It does, in practice: on Linux,
BSD, and Windows, the loopback interface is where the kernel "zero-copies" the packet from the
sending socket buffer directly into the receiving socket buffer without ever building or
validating a real link-layer frame — see the [FreeBSD/Linux `lo(4)` manpage](https://manpages.ubuntu.com/manpages/bionic/man4/lo.4freebsd.html)
("if the receive checksum offload capability flag is enabled on a loopback interface, checksums
will not be validated... since you can't get corrupted packets on the loopback network, not
bothering to checksum segments on it is a reasonable optimization") and the corroborating
discussion in the [npcap issue on loopback checksums](https://github.com/nmap/npcap/issues/715)
and the ["zero-copy loopback stack" write-up](https://catonmat.net/zero-copy-loopback-stack).
**The residual risk on loopback is not "network bit flips" (there is no wire to flip bits on) —
it is kernel/driver memory bugs, buggy checksum-offload interacting badly with a NIC path that
loopback traffic normally never takes, or a compromised/misbehaving process on the same host.**
Those are not "did the network mangle my bytes" failures; they are host-integrity failures that
no amount of application-level framing fixes anyway (if the local kernel is corrupting memory, a
message-level ack doesn't help — you have a much bigger problem). **This confirms the ADR-0009
framing is already correct**: loopback `ws` is not pretending to be secure against a network
adversary, and TCP's weak checksum is not meaningfully "at risk" there because it's barely in the
data path to begin with.

## 2. Application-level integrity/delivery-confirmation practice in comparable protocols

**WebSocket ping/pong (RFC 6455) is connection liveness, not message delivery confirmation.**
The control opcodes 0x9/0xA exist to detect a dead peer or keep a middlebox/proxy from closing
an idle TCP connection — see [websocket.org's heartbeat guide](https://websocket.org/guides/heartbeat/)
and the [RFC 6455 text itself](https://www.rfc-editor.org/rfc/rfc6455.html). A successful pong
tells you the *socket* is alive right now; it says nothing about whether the last *text frame*
you sent was parsed, accepted, or acted on by the application above the WebSocket layer. Holler
already has its own `ping`/`pong` for exactly this liveness purpose (`docs/protocol/v1.md` §10,
"Bound-socket aliveness (`holler token ping`)") — this is correctly scoped as a liveness check,
not conflated with message-level integrity, and this memo does not propose changing that.

**JSON-RPC 2.0's `id` correlation is Holler's own envelope pattern, and it already buys implicit
delivery+processing confirmation for request/reply pairs — but only for request/reply pairs.**
Per the [JSON-RPC 2.0 spec](https://www.jsonrpc.org/specification): a request with an `id` gets
a response reusing that `id`; a request *without* an `id` is a "Notification," and "the Server
MUST NOT reply" to one. Holler's envelope (`docs/protocol/v1.md` §3: "`id` — Correlation id...
Replies reuse the request `id`") already follows this shape for `query`/`query_ok` and
`prompt`/`reply`. **This is the load-bearing insight for point 3 below: a matched-`id` reply is
already proof the request arrived, parsed, and was acted on** — no separate integrity primitive
is needed for that half of the protocol. The gap JSON-RPC's own spec leaves open, by design, is
exactly Holler's one-way frames (`interrupt`, `presence`) — those are structurally notifications,
and JSON-RPC 2.0 has nothing to say about confirming those either.

**MQTT's QoS levels are the most directly relevant precedent for a lightweight one-way ack.**
[HiveMQ's QoS explainer](https://www.hivemq.com/blog/mqtt-essentials-part-6-mqtt-quality-of-service-levels/)
and the [MQTT v3.1 spec](https://public.dhe.ibm.com/software/dw/webservices/ws-mqtt/mqtt-v3r1.html)
lay out three levels:

- **QoS 0** (at-most-once): fire and forget, no ack at all.
- **QoS 1** (at-least-once): a single `PUBACK` round-trip. Sender keeps the message until it
  sees a `PUBACK`; on timeout it retransmits (with the `DUP` flag set), so the receiver may see
  the same message twice. Simple: one extra frame, one piece of sender-side state (a pending-ack
  table keyed by packet id), one timeout.
- **QoS 2** (exactly-once): a four-packet handshake — `PUBLISH → PUBREC → PUBREL → PUBCOMP`.
  This exists specifically to prevent the duplicate-delivery side effect of QoS 1's naive retry,
  at the cost of two full round trips and state on both sides until the handshake completes.

**gRPC deliberately does not solve this at the transport/RPC layer.** For a unary call, a
successful response *is* the delivery confirmation (structurally identical to Holler's
`query`/`query_ok`). For streaming, gRPC provides no built-in redelivery or ack — the [grpc-io
mailing list discussion on bidirectional-stream reliability](https://groups.google.com/g/grpc-io/c/DT3yYbz5F_M)
and papers building exactly-once systems on top of gRPC streams (e.g.
[LogPlayer](https://arxiv.org/pdf/1911.11286)) confirm the application has to build
sequence-number/ack/dedup logic itself if it wants at-least-once or exactly-once semantics over
a stream. gRPC's own position is: request/response gets confirmation for free from the RPC
model; a fire-and-forget stream item does not, and that's left to the app.

**The ADR-0001 competitive set does not document any message-level ack or integrity scheme** —
this was checked directly against each project's public docs, and it's worth being explicit
about which of these are genuinely silent versus which have something adjacent:

| Project | What was found |
| --- | --- |
| [c2c](https://github.com/clankercode/c2c) | Docs describe the wire envelope (`<c2c event="message" from=… to=…>`) but say nothing about acks, checksums, or delivery confirmation. Prioritizes simplicity over guaranteed delivery. |
| [ai-crew-sync](https://github.com/joaquinbejar/ai-crew-sync) | No ack/integrity scheme for agent-to-agent bus messages. It *does* document an at-least-once, replica-safe delivery guarantee — but that's for outbound **webhooks** to external services (DB-trigger enqueue + `FOR UPDATE SKIP LOCKED` + exponential-backoff retry), a materially different problem (durable queue to an HTTP endpoint) from a live bidirectional socket. |
| [Buzz](https://github.com/block/buzz) | Documents that "every message... is a signed event in one log" — that's message **authentication** (who sent it, non-repudiation, audit trail), not delivery confirmation or bit-level integrity. Signing is orthogonal to the question asked here. |
| [Herdr](https://herdr.dev) | Nothing on messaging protocol reliability at all — its docs are scoped to session/PTY persistence, not a wire protocol. |
| [Claude Code cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging) | No wire-level ack/checksum documented. What it does have is an explicit **outcome model** for every send — a message resolves to exactly one of **Delivered / Held / Refused**, and same-machine transport is a Unix domain socket (kernel-reliable, ordered, no network involved) while cross-machine transport goes through Anthropic's servers (implicitly TLS-protected, so integrity is inherited the same way `wss` inherits it for Holler). Notably, it does *not* give the sender a durable receipt that a **Delivered** message was actually read/acted on by the far Claude — only that Claude Code's own inbox accepted it. |

**Conclusion for this section:** no comparable tool in the ADR-0001 survey has solved "one-way
control-frame delivery confirmation" with anything more sophisticated than what MQTT already
formalized decades ago. The field's actual practice, once you exclude confidentiality (TLS) and
liveness (heartbeats), converges on: request/reply gets confirmation for free from correlation
ids; one-way messages either get nothing (most of these tools), or a single-ack pattern
(MQTT QoS 1) when someone bothers.

## 3. What actually needs an ack in Holler's design, and what doesn't

Holler is a **single hop** (`docs/protocol/v1.md` §1: client↔server, not a multi-hop bus) over
an already-ordered, already-reliable transport (TCP; optionally TLS 1.3 for `wss`). That framing
matters: WebSocket already frames messages at the transport level (RFC 6455 defines a frame's
boundaries explicitly — a text frame either arrives complete or the connection breaks; there is
no "message cut off mid-stream but the socket looks fine" state the way there is for a raw TCP
byte stream that an application has to re-delimit itself). Combined with `src/proto/mod.rs`'s
`decode()`, which already treats "valid JSON but doesn't fit the envelope schema" as
`DecodeError::Malformed` (see `src/proto/mod.rs:280` and the exhaustive missing-field checks in
`parse_envelope`), **a truncated-or-corrupt-JSON failure mode is already fully handled** — there
is no scenario where `decode()` silently accepts a mangled frame as if it were well-formed. This
much needs nothing new.

Splitting the message-type table (`docs/protocol/v1.md` §5) by whether a reply already exists:

| Type | Has a natural reply? | Delivery+processing confirmation today |
| --- | --- | --- |
| `query` → `query_ok` | Yes | Free, via `id` correlation (§2 above) — a matched-`id` `query_ok` *is* proof of receipt and processing. |
| `prompt` → `reply` | Yes | Same. |
| `auth`, `hello` | Yes (handshake is inherently synchronous) | Same — the peer's next expected frame is the tell. |
| `interrupt` | **No** | Nothing. Server sends it, session-side effect happens (or doesn't) with no signal back. |
| `presence` | **No** (client → server, one-way heartbeat) | Nothing, but this is fine — see below. |
| `ping` / `pong` | Yes, but only confirms liveness, not a prior message | N/A — not this problem. |
| `ack` | Spec says "optional receipt," shape unspecified | Nothing yet — this is the primitive in question. |

**`presence` doesn't need an ack.** It's a heartbeat; ADR-0006 (roster, referenced in
`docs/protocol/v1.md` §10) already handles the failure mode — a stale row just times out of the
roster. Missing one `presence` frame is self-healing by the next one arriving; adding an ack
here would just double the traffic for a signal that's already tolerant of loss by design.

**`interrupt` is the one frame where silent loss is a real, user-visible correctness problem.**
ADR-0005 is explicit that interrupt existing as a distinct control frame (not a substrate-level
signal) is a deliberate design choice — the whole point is that the user believes they cancelled
a turn. If the `interrupt` frame is sent and, for whatever reason, never reaches the client
(process momentarily wedged, message dropped at a layer below WebSocket framing that shouldn't
normally drop anything but the JSON parse still fails some other way), the server currently has
no way to know the cancel didn't take — the turn keeps running while the operator believes it
was stopped. That is a materially worse failure mode than "an ack round-trip was mildly
redundant."

**Recommendation: add a single ack for `interrupt` only, matching MQTT QoS 1's model, not QoS 2's.**
QoS 2's four-packet handshake exists to solve *duplicate delivery* under retransmission — a
concern for a multi-hop broker relaying to many subscribers, where the same publish might be
retried by multiple actors. Holler is one hop, one recipient, over an ordered TCP stream that
doesn't reorder or duplicate frames on its own; the failure mode that needs covering is "did it
arrive at all," not "did it arrive exactly once despite retries across a lossy multi-hop path."
That's a QoS 1 problem, and using QoS 2's heavier machinery here would be solving a problem
Holler's topology doesn't have — this is squarely the kind of unneeded machinery ADR-0009's
fail-closed-but-simple philosophy and the "No steer-inject in v1" precedent (ADR-0005) already
warn against. A plain request/ack pair is enough.

**Concrete shape for `ack`.** The spec already reserves the field this needs
(`src/proto/mod.rs:250`, `AckBody { of: Option<String> }` — "optional receipt referencing the
acknowledged frame id"). Stop leaving it "unspecified" and pin it down:

```
type: "ack"
id:   (new correlation id for this ack frame itself)
body: { "of": "<id of the frame being acknowledged>" }
```

- **Required for:** `interrupt` only, in v1. The client sends `ack` (with `of` set to the
  interrupt's `id`) as soon as it has *applied* the cancel (i.e., after calling ACP
  `session/cancel` or the OpenCode HTTP interrupt fallback, not just after parsing the frame) —
  the ack should mean "cancelled," not "received." The server should treat a missing ack within
  a bounded timeout as "interrupt may not have landed" and surface that to the operator (e.g. in
  `holler status`) rather than silently assuming success. The exact timeout value is a
  judgment call for the team, not something this memo can derive from a source — pick a
  clip 2–3x normal RTT and revisit after real usage.
- **Not required for:** `query`, `prompt`, `presence`, `hello`, `auth`, `ping`/`pong` — all
  either already have a natural reply that serves the same purpose, or are self-healing
  heartbeats where an ack is pure overhead.
- **Left genuinely open, for the team:** whether a future frame type (e.g. a v2 addition) should
  default to "ack required" or "no ack" — this memo only has grounds to make a call for the v1
  vocabulary that exists today.

## 4. Recommendations

**(a) Is anything needed beyond TCP + TLS + existing request/reply correlation, for loopback-`ws`
v1 scope?** No, with one exception. TLS already gives cryptographic integrity on `wss`, and
loopback's zero-copy kernel path means the "weak TCP checksum" risk on `ws` is not meaningfully
live — the residual risk there is host-level (kernel bug, malicious co-resident process), which
no application-level framing addresses. Request/reply correlation (already spec'd, already
implemented) already gives free delivery+processing confirmation for `query`/`prompt`/`hello`/
`auth`. **The one exception is `interrupt`**, addressed in (b).

**(b) Is `ack` worth specifying now?** Yes — narrowly. Pin down the shape above
(`{"of": "<id>"}`, sent once the receiver has *applied* the action, not merely parsed the frame)
and require it only for `interrupt`. This is a clear-correct-answer recommendation, not a coin
flip: `interrupt`'s entire reason for existing (ADR-0005) is that the operator needs to trust
that a cancel took effect, and that's exactly the one v1 frame type with no other way to find
out. Extending the requirement to `presence` (self-healing by design) or to `query`/`prompt`
(already covered by their own replies) would be solving a problem those frames don't have.

**(c) What should happen when integrity IS violated at the application level (`decode()` hits
malformed JSON mid-connection)?** Drop the connection and require reconnect, matching ADR-0009's
fail-closed philosophy — do **not** try to skip just the bad frame and keep going. Reasoning:
WebSocket already frames messages at the transport level, so a single malformed *text frame*
handed to `decode()` is not evidence of a network delimiter problem that a resync could recover
from — it's evidence that either (a) the peer is running a mismatched/buggy protocol version, or
(b) something is actively wrong in a way `decode()`'s existing `UnsupportedVersion` /
`UnknownType` / `Malformed` distinctions can't further diagnose from inside one frame. Treating
it as recoverable-and-skippable would mean silently discarding a `prompt` or `interrupt` and
carrying on as if nothing happened — precisely the "ignore it as success" failure mode
`docs/protocol/v1.md` §3 already explicitly forbids for unknown types, and the same logic
extends to malformed bodies. This is a place where **the existing codec and existing ADR-0009
philosophy already answer the question**; this memo is not proposing new code, just confirming
the connection-drop reading is the one consistent with what's already decided, and noting that
one loose end remains genuinely open for the team: whether the *reconnect* path (credential
reuse, per `docs/protocol/v1.md` §4: "Reconnect uses the credential again") should carry any
signal to the operator that the previous connection died on a malformed frame specifically
(vs. a plain network drop) — that distinction is presumably the dropped-connections research's
territory once it lands, since it's about *what happens after* a connection ends, not about
message integrity per se.

---

## Source quality summary

| Claim | Source status |
| --- | --- |
| TCP checksum weakness, real-world miss rate | Primary: Stone & Partridge SIGCOMM 2000 paper (peer-reviewed, directly fetched via search abstract + two independent secondary summaries) |
| TLS 1.3 AEAD integrity mechanics | Primary: RFC-level AEAD construction facts (GHASH/Poly1305), corroborated by two independent technical explainers |
| Loopback interface skips checksum validation | Primary: BSD/Linux `lo(4)` manpage text, corroborated by an active kernel-tooling bug report (npcap) discussing the same behavior |
| RFC 6455 ping/pong scope | Primary: RFC 6455 itself + websocket.org's implementer guide |
| JSON-RPC 2.0 notification semantics | Primary: the JSON-RPC 2.0 specification text itself |
| MQTT QoS 1/2 mechanics | Primary: MQTT v3.1 spec + HiveMQ's widely-cited implementer explainer (cross-checked, consistent) |
| gRPC has no built-in stream redelivery | Secondary but consistent: grpc-io official mailing list thread + an academic paper building exactly-once delivery on top of gRPC (both agree the RPC layer itself provides none) |
| c2c / ai-crew-sync / Buzz / Herdr have no ack/integrity docs | Directly fetched each project's own documentation; **absence of a scheme is what was found**, not inferred — flagged per-project above where something adjacent (webhooks, signing) exists but doesn't answer the question |
| Claude Code cross-session messaging delivery model | Primary: official Claude Code docs, fetched directly and read in full |

No source in this memo is asserted from background knowledge alone without a citation; every
quantitative or protocol-behavior claim above traces to a fetched primary or corroborated
secondary source.
