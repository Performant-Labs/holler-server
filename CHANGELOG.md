# Changelog

All notable changes to this project are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and version
numbers follow the policy in [ADR 0014](docs/adr/ADR-0014.md) (standard SemVer per crate,
starting at 0.1.0). This file starts from ADR 0014's decision forward — it is not
backfilled with pre-decision history.

## [Unreleased]

## [0.1.0] - 2026-09-07

First tagged release. Covers everything since [ADR 0014](docs/adr/ADR-0014.md)'s versioning
decision — this file's own stated starting point, not the project's full history.

### Enhancements

- Real join flow: `join`/`join_ok` frames redeem a one-time secret over the wire (ADR 0015);
  `token mint` prints a ready-to-run `holler join` command.
- `holler interrupt <session>` control path — cancel a session's in-flight turn without
  tearing down the session itself (ADR 0005).
- Prompt/reply routed by session **name**, not connection/host (ADR 0007), so a client can
  host multiple independently-addressable sessions on one connection.
- Attach mode: a session can attach to an already-running harness process (e.g. inside a
  Herdr pane) over HTTP instead of being spawned — presence, `status`, and `support` all
  advertise attach sessions correctly (ADR 0017).
- `--version`/`-V` on both binaries.
- Debug logging: emission timestamps, `text`/`json` output formats, a severity axis so
  operational lines survive `--debug=none`, and (most recently) a `component` column so a
  `--debug noisy` line is attributable to the layer that emitted it (wire/registry/talklog/etc.)
  without prior codebase knowledge.
- Security hardening: per-IP throttling on repeated bad auth/join attempts; frame-size cap,
  pre-auth timeout, connection cap, and peer-IP tracking on every connection; the control
  socket and credential file are both restricted to owner-only permissions.
- `holler-server serve` now refuses to start a second instance against the same state dir
  rather than racing it.
- The test-case catalog (`#98`) and its `scripts/test-run.rb` runner, and `docs/releasing.md`'s
  release process — project-facing rather than binary-facing, but part of what this release
  actually ships as a maintained project.

### Breaking Changes

- The server binary is now named `holler-server`, not `holler` (avoids an install collision
  with the client's own `holler` binary). There is no prior tagged release this could break,
  but noted here since it's exactly the kind of change ADR 0014 classifies as breaking.

### Bug Fixes

- A superseded connection (same token reconnecting) is now actually closed, not leaked
  (`Registry::insert`).
- A server-initiated revoke force-closes the live connection immediately, instead of waiting
  for it to notice on its own.
- A roster row is marked `gone` immediately on an explicit close/revoke, instead of waiting
  out the staleness timer.
- `say`/`interrupt` distinguish "the turn was interrupted" and "the reply is just slow" from
  a genuine "no live server" failure, instead of collapsing all three into one ambiguous error.

### Known Issues

- Interrupting one session can disrupt an unrelated sibling session's connection on the same
  client, and the roster can show stale `reconnecting` state afterward even with active
  traffic — root cause documented on [#202](https://github.com/Performant-Labs/holler-server/issues/202)
  (also [#203](https://github.com/Performant-Labs/holler-server/issues/203),
  [#204](https://github.com/Performant-Labs/holler-server/issues/204)).
- A rejected one-shot presence (a session name already claimed by a different live client)
  can permanently hide a session from the roster until the client reconnects
  ([#243](https://github.com/Performant-Labs/holler-server/issues/243)).
- No `wss`/TLS support yet — non-loopback binding is refused
  ([#205](https://github.com/Performant-Labs/holler-server/issues/205)); loopback + an SSH
  tunnel is the supported remote pattern for now.
- No Windows binary in this release — Windows is off CI's test matrix (a real runner-timing
  issue, not just untested) — see [#302](https://github.com/Performant-Labs/holler-server/issues/302)
  and `docs/releasing.md`'s platform table.
