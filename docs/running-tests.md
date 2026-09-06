# Running tests

This page is the practical "how do I run the tests" guide. For the *design* of the
process-level wire harness (why it exists, what it proves, the ACP stub), see
[testing.md](testing.md) instead — this page is about invoking things, that one is about
the test architecture.

## Plain `cargo test`

The baseline, no catalog involved:

| Command | Runs |
| --- | --- |
| `cargo test` | Everything |
| `cargo test --test <file>` | One integration test file (e.g. `cli_invocation_test`) |
| `cargo test --test <file> <fn>` | One test in that file (substring match) |
| `cargo test --test <file> <fn> -- --exact` | Same, but requires an exact name match |
| `cargo test <fn>` | Substring match across the **whole crate**, including inline `#[cfg(test)]` unit tests inside `src/` (no `--test <file>` makes sense for those — they aren't a separate integration binary) |
| `cargo test ... -- --nocapture` | Shows `println!`/stdout even for a passing test |

A `#[rstest]`-parameterized test expands into one test per `#[case(...)]`, named
`<fn>::case_1`, `<fn>::case_2`, etc. Running the bare `<fn>` runs all its cases; add
`::case_1` to run just one.

## The test-case catalog + `scripts/test-run.rb`

Every test case in this project is a GitHub issue — in `holler-server`, even for
client-only cases (see [holler-server#98](https://github.com/Performant-Labs/holler-server/issues/98),
the master catalog, for the full contract). Each carries a `Test ID` (`hlrsvr-NNNN` /
`hlrclnt-NNNN`), a `Group`, and — for automated cases — an `Automation` field pointing at
the real committed test instead of a prose description (git is the source of truth for
what an automated case actually does).

### Running one ticket's test — the easy way

You don't need to know the file or function name, just the Test ID from the issue:

    ruby scripts/test-run.rb exec hlrsvr-1000

This looks the ticket up in the catalog, runs its real `cargo test` command, and streams
the output live to your terminal as it runs. It exits with the same pass/fail status the
underlying test exits with, so it composes with shell scripting
(`ruby scripts/test-run.rb exec hlrsvr-1000 || echo failed`). It makes **no GitHub writes at
all** — no test-run issue, no comment, nothing — it's a pure local convenience. If the ID
doesn't exist in the catalog, or it's a manual-only case with nothing automated to run, it
says so clearly and exits non-zero rather than silently doing nothing.

### Running a full test run (the release-gating process)

    ruby scripts/test-run.rb discover
    ruby scripts/test-run.rb start [--applies server|client|both|all] [--type auto|manual|all]
    ruby scripts/test-run.rb run <issue-number> --server-dir DIR --client-dir DIR
    ruby scripts/test-run.rb record <issue-number> <test-id> pass|fail [note]

- `discover` dumps the parsed catalog as JSON — useful for checking the script actually
  sees a ticket the way you expect.
- `start` creates a new test-run issue (labeled `test-run`) from a filtered slice of the
  catalog, all rows starting `⏳ pending`.
- `run` executes every pending automated case in that issue for real and writes the
  results back into the same issue (pass/fail, evidence, a comment with the full log on
  failure or fallback).
- `record` manually sets one row's result — for a manual-labeled case, or to override an
  automated one.

### The `Automation` field's exact grammar

This is parsed by the script, so it isn't free-form prose — the accepted forms are:

| Form | Runs |
| --- | --- |
| `<repo>: tests/<file>.rs` | `cargo test --test <file>` (the whole integration test file) |
| `<repo>: tests/<file>.rs (<fn>)` | `cargo test --test <file> <fn>` — **parentheses, not `::`** |
| `<repo>: src/<path>.rs (<fn>)` | `cargo test --lib <fn>` — for an inline `#[cfg(test)]` unit test living in `src/`, not a separate `tests/*.rs` file |
| segments separated by `; ` | run each; **all** must pass |
| starts with `manual` | not run at all — no automated command exists for this case |
| anything else (e.g. a bare `src/...` pointer with no `(<fn>)`) | falls back to that repo's whole `cargo test` — works, but tells you far less; fix the field to one of the precise forms above if you hit this |

`<repo>` is `holler-server` or `holler-client`.

## Environment gotchas on this machine

**Ruby version.** The `octokit` gem (used to talk to GitHub) needs Ruby ≥ 3.2. This
machine's system Ruby (`/usr/bin/ruby`) is 2.6.10 — too old to even install the gem. Use a
Homebrew Ruby instead:

    brew install ruby          # if not already installed
    ruby -v                    # should report 4.x, not 2.6.x

If `ruby scripts/test-run.rb ...` fails with `cannot load such file -- octokit`, that's
this exact problem. Either put `/opt/homebrew/opt/ruby/bin` ahead of the system Ruby in
your `PATH` (so plain `ruby` resolves correctly — add
`export PATH="/opt/homebrew/opt/ruby/bin:$PATH"` to your shell rc), or invoke it
explicitly: `/opt/homebrew/opt/ruby/bin/ruby scripts/test-run.rb ...`.

**Auth.** No manual token setup needed if you're already logged in via `gh auth login` —
the script falls back to `gh auth token` automatically when `GITHUB_TOKEN` isn't set in
the environment.

## Cross-platform

CI runs on `ubuntu-latest` and `macos-latest` via GitHub Actions (see each repo's
`.github/workflows/ci.yml`). Windows is currently off both repos' CI matrices entirely —
a deliberate, dated, temporary decision (see the comment in each `ci.yml`), not an
oversight — pending real work on the known issues that made it permanently red there.
