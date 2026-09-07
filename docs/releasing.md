# Releasing

How to cut a release of **this repo** (holler-server). holler-client releases independently — see
[its own `docs/releasing.md`](https://github.com/Performant-Labs/holler-client/blob/main/docs/releasing.md),
which points back here for the shared mechanics and documents only what differs.

Versioning policy is [ADR 0014](adr/ADR-0014.md); this doc is the mechanics ADR 0014 deliberately
left open ("Not decided here"). See [issue #90](https://github.com/Performant-Labs/holler-server/issues/90)
for the decision record behind the choices below.

## Where the version number lives

**`Cargo.toml`'s `[package] version` field is the single source of truth.** Nothing else derives
it independently:

- `holler-server --version` / `-V` reads it at compile time via clap's `#[command(version)]`
  (`main.rs`), which pulls Cargo's own `CARGO_PKG_VERSION` build-time env var — this is why
  bumping `Cargo.toml` is the *only* step needed for the binary itself to report the new version.
- The git tag (`vX.Y.Z`), the `CHANGELOG.md` heading, and a GitHub Release's title must all match
  what's in `Cargo.toml` at the commit being released — they're representations of the same fact,
  not independent decisions.

Format: standard SemVer, starting at `0.1.0`, independent from holler-client's own version — see
ADR 0014 for exactly what counts as PATCH/MINOR/MAJOR at this project's pre-1.0 stage.

## Were the tests run? What proves it?

**The release commit must be `main` at a commit where CI's `test` job is green on every matrix
entry** — check the [Actions tab](https://github.com/Performant-Labs/holler-server/actions/workflows/ci.yml)
for that exact commit's run, or `gh run list --branch main --workflow ci.yml --limit 1` /
`gh pr checks <the merging PR>`. "Green" means both required checks passed:

| Check | What it proves |
| --- | --- |
| `test (ubuntu-latest)` | Canary + wire harness + full `cargo test` suite + `cargo clippy --all-targets -- -D warnings`, all clean on Linux |
| `test (macos-latest)` | Same, on macOS |

These are already the two checks `main`'s branch protection requires before any PR can merge — so
in practice, if the release commit is `main` itself (not a stray unmerged branch), CI has already
gated it once at merge time. Re-checking the Actions run for that specific commit before tagging
is the confirmation step, not a re-run — don't tag off a commit whose CI run you haven't actually
looked at.

## Which platforms does a release target?

**Don't ship a binary for a platform CI never ran the suite on.** Only `ubuntu-latest` and
`macos-latest` qualify today:

| Platform | CI-tested | Released | Status |
| --- | --- | --- | --- |
| `ubuntu-latest` | Yes | Yes | Full support |
| `macos-latest` | Yes | Yes | Full support |
| Windows | No | No | Excluded from CI (real runner-timing issue) — tracked in [#302](https://github.com/Performant-Labs/holler-server/issues/302); add a row here (and to CI's matrix) once that's resolved |

This table is the one place platform status is recorded — the checklist and any other doc
mentioning target platforms should link here rather than repeat/restate it.

## What a release actually produces

Two tiers — the first is required, the second is a deliberate extra:

1. **A git tag + `CHANGELOG.md` entry.** Always. `vX.Y.Z`, annotated (`git tag -a`), signed
   (this org's git config already has `commit.gpgsign`/`tag.gpgsign` on — sign release tags the
   same way). The `CHANGELOG.md`'s `## [Unreleased]` section becomes `## [X.Y.Z] - YYYY-MM-DD`,
   summarizing everything merged since the last tag (Keep a Changelog format, hand-written per
   release — not generated from commit messages).
2. **A GitHub Release**, with prebuilt binaries attached for `ubuntu-latest` and `macos-latest`
   (built via `cargo build --release` on each platform, from the exact tagged commit). This is a
   **public, outward-facing artifact** — confirm with whoever's driving the release before
   publishing it, every time; it's not something to automate past without a look.

## CHANGELOG entry structure

A release's `CHANGELOG.md` entry (`## [X.Y.Z] - YYYY-MM-DD`) is organized into fixed
subsections, in this order, each omitted entirely if empty (don't print an empty
`### Bug Fixes` with nothing under it):

```markdown
## [X.Y.Z] - YYYY-MM-DD

### Enhancements
- New capability or additive change, one line each.

### Breaking Changes
- Anything matching ADR 0014's definition of "breaking" for this project (CLI surface,
  `--json` output shape, on-disk file formats). Omit this subsection entirely if there
  are none — don't print "None" either, just leave it out.

### Bug Fixes
- Real fixes, one line each, linking the issue.

### Known Issues
- Real, still-open gaps worth a user knowing about before they hit them — see this
  doc's own "Known issues" section below for the policy. Link the issue.
```

This mirrors the real shape large projects converge on for the same reason (Mattermost's
own changelog, e.g. its Desktop App changelog, separates Improvements / Bug Fixes /
Known Issues; its server changelog adds an "Upgrade Impact" section for breaking changes
— same four ideas, different names). Keep a Changelog's own vocabulary (Added / Changed /
Deprecated / Removed / Fixed / Security) is a fine reference but not literally used here —
this project's four buckets (Enhancements / Breaking Changes / Bug Fixes / Known Issues)
map onto it well enough (Enhancements ⊇ Added+Changed, Bug Fixes = Fixed, Breaking
Changes ⊇ Removed+some Changed) without needing all six of its headings.

## Known issues

**A known, documented issue is not by itself a release blocker** (team decision,
2026-09-07) — this project ships betas with real gaps rather than holding a release
hostage to a fix. Before finalizing release notes:

- Skim open `bug`-labeled issues (`gh issue list --repo Performant-Labs/holler-server
  --label bug --state open`) and open issues in holler-client too — a real defect doesn't
  need the `bug` label to be worth mentioning (e.g. a plainly-titled defect report with no
  label yet).
- For anything real and still open, add one line to the release notes: what it is, roughly
  when it bites, and a link to the issue. Silence is the failure mode here, not the bug
  itself — a user who hits a documented known issue is annoyed for a minute; a user who
  hits an undocumented one loses trust in the release notes.
- A `bug` you've already root-caused (even if not yet fixed) is worth linking that
  diagnosis in the release note too, not just the symptom — see holler-server#202's own
  comment thread for the pattern.
- The per-release checklist issue (see below) is where the *specific, current* list for
  that release actually lives — this section is the durable policy, not a live list, since
  a specific issue number here would go stale the moment it's fixed.

## Who cuts a release, and when

Manual, on-demand — a person decides "time to tag." No cadence, no automated trigger. This may
change once there's real release volume to justify automating it (see ADR 0014's "Not decided
here" — an automated tool like `cargo-release`/`release-plz` was explicitly left for later, not
ruled out).

## Step by step

1. Confirm the release commit (usually `main`'s tip) has a green CI run on both platforms — see
   above.
2. Decide the version bump (PATCH/MINOR/MAJOR) per ADR 0014's rules, from everything accumulated
   in `CHANGELOG.md`'s `## [Unreleased]` section since the last tag.
3. `cargo set-version <X.Y.Z>` (or edit `Cargo.toml`'s `version` by hand) — this is the only
   source-of-truth edit; nothing else needs independent updating.
4. Move `CHANGELOG.md`'s `## [Unreleased]` content under a new `## [X.Y.Z] - YYYY-MM-DD` heading,
   organized per "CHANGELOG entry structure" above; leave a fresh empty `## [Unreleased]` above it.
5. **Update `README.md`** with anything a user landing on the repo needs to know about this
   release: notable new features, fixes, or changes to CLI invocation (new/changed flags,
   subcommands, output shape). Not every CHANGELOG line belongs here — only what changes how
   someone actually uses the tool, the same bar as a man page going stale.
6. Commit those file changes (`Bump version to X.Y.Z`), push, confirm CI is green on the bump
   commit itself too.
7. `git tag -s vX.Y.Z -m "vX.Y.Z"` (signed, annotated), `git push origin vX.Y.Z`.
8. Check open `bug`-labeled (and otherwise plainly real) issues for anything worth a known-issue
   line in the release notes — see "Known issues" above.
9. `cargo build --release` on each target platform (see the platform table above); verify
   `./target/release/holler-server --version` actually reports `X.Y.Z` before attaching anything.
10. Create the GitHub Release from the tag (`gh release create vX.Y.Z <binaries...> --notes-file
    <changelog excerpt>`) — the confirm-before-publish step from above.
11. **Verify the published artifact, not just the local build.** Download the binary actually
    attached to the GitHub Release (`gh release download vX.Y.Z`), from a clean directory, and
    run `./holler-server --version` against *that* file — confirms the upload isn't corrupted,
    is the right architecture, has its executable bit set, and actually reports `X.Y.Z`. A local
    build passing step 9 is not evidence the uploaded artifact works; only downloading and
    running the real thing is.

For the actual checklist to run through each time (not just the narrative above), see this
repo's pinned release-checklist issue — [#301](https://github.com/Performant-Labs/holler-server/issues/301)
for the first release; a fresh one gets filed per release going forward.
