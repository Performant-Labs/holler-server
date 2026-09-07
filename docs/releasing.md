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

**The same two platforms CI verifies: `ubuntu-latest`, `macos-latest`.** Windows is deliberately
off CI's matrix (a real runner-timing issue, tracked separately — see `ci.yml`'s own comment and
the matrix note), so it is not released either until that's fixed. Don't ship a binary for a
platform CI never ran the suite on.

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
4. Move `CHANGELOG.md`'s `## [Unreleased]` content under a new `## [X.Y.Z] - YYYY-MM-DD` heading;
   leave a fresh empty `## [Unreleased]` above it.
5. Commit those two file changes (`Bump version to X.Y.Z`), push, confirm CI is green on the bump
   commit itself too.
6. `git tag -s vX.Y.Z -m "vX.Y.Z"` (signed, annotated), `git push origin vX.Y.Z`.
7. `cargo build --release` on each target platform; verify `./target/release/holler-server
   --version` actually reports `X.Y.Z` before attaching anything.
8. Create the GitHub Release from the tag (`gh release create vX.Y.Z <binaries...> --notes-file
   <changelog excerpt>`) — the confirm-before-publish step from above.

For the actual checklist to run through each time (not just the narrative above), see the pinned
release-checklist issue template referenced from [issue #90](https://github.com/Performant-Labs/holler-server/issues/90).
