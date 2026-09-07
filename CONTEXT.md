# Holler — domain glossary

A pure glossary of terms and their meanings *in this project*. No implementation
details, no specs. If a term here conflicts with how you're using the word, the
glossary wins — flag the conflict and update this file.

## Test-case catalog (holler-server#98)

- **Test ID** — The stable identifier for a single test case: `hlrsvr-NNNN` for a
  `holler-server` case, `hlrclnt-NNNN` for a `holler-client` case. The prefix encodes
  *which binary owns the case*; the **second digit** of the four-digit number encodes
  *which group* it belongs to (see Group) — so a case in group *k* sits in the
  `1k00`–`1k99` range (1000–1099 = invocation, 1100–1199 = lifecycle, …, 1900–1999 =
  load). The last two digits are the case's slot within its group.
- **Test case** — A reusable, standing statement of one thing that must be true of the
  system. Tracked as one GitHub issue (labeled `test-case`) and identified by its Test ID.
  Distinct from a *test run* (a single execution of many cases).
- **Group** (`test-grp-*`) — *Where in the system* a case lives. Structurally **exactly
  one** per case. There are ten groups, in fixed order — invocation, lifecycle, logging,
  io, platform, concurrency, network, diagnostics, crypto, load — and group *k* occupies
  the `1k00`–`1k99` sub-range of a Test ID (invocation=1000, lifecycle=1100, …,
  load=1900). The group is the **hundreds digit** of the Test ID; the label descriptions
  spell it out in full four digits (`x1000` … `x1900`) precisely because a *tenth* group
  already exists (`load`, `1900`) — a bare `x000`-style "hundreds" shorthand would become
  ambiguous the moment a group wraps to `2000`, so the full number is always shown.
- **Category** (`test-cat-*`) — *Why/when you'd run* a case. Cross-cutting, **zero or more**
  per case, and a **closed** set: smoke (fast baseline, run before anything else),
  regression (guards an already-fixed bug), acceptance (full real end-to-end release
  gate), unit (an inline `#[cfg(test)]` case, run batched).
- **Tag** (`test-tag-*`) — An **open-ended, zero-or-more** per-case marker for a property
  that cuts across groups — e.g. `alters-db` (mutates the shared state/token store),
  `remote` (needs a second machine / tunnel), `needs-tunnel`, `slow`. The Playwright
  `@tag` analogue. Unlike `test-cat-*`, there is no reserved vocabulary — the common set
  is *suggested, not enforced*, and a tag's meaning is local to what it marks. The third
  label axis, added 2026-09-07 (holler-server#305); selection over it is #304's
  `--tag`/`--tag-invert`.
- **Applies to** — `server`, `client`, or `both`. A `both` case is a joint/interop case and
  therefore carries **two** Test IDs (one per binary) in the same group.
- **Automation** — The catalog field pointing at what an automated case actually runs, in a
  strict machine-parsed grammar (`<repo>: tests/<file>.rs (fn)`, `src/… (fn)` → `--lib`,
  `; `-joined segments, or `manual`). Git is the source of truth; this field is the index.
- **Test run** — One execution of a selected set of cases, recorded as a GitHub issue
  (labeled `test-run`) with one row per case (pass/fail + evidence). Release-gating: a
  green run against the *joint* state of both repos is required to cut a release.

## Test-selection axes

Selection over the catalog composes across the independent axes. In Playwright terms,
`test-grp-*` ≈ the test *describe-group* (where), `test-cat-*`/`test-tag-*` ≈ tags (why /
open-ended property), and `Applies to` ≈ `--project`. The negative form (`--tag-invert`,
Playwright `--grep-invert`) selects the *complement* of a tag — e.g. "everything except the
`alters-db` cases".
