#!/usr/bin/env ruby
# frozen_string_literal: true

# test-run.rb -- the loop described in holler-server#98 "How to perform a
# test run": select test cases from the catalog, run the automated ones for
# real, write results back into the SAME test-run issue.
#
# Rewrite of the abandoned scripts/test-run.sh (bash) -- that version hit a
# real BSD-vs-GNU `sed` portability bug (an alternation with an empty branch,
# invalid on macOS's BSD sed, fine on Linux CI) while doing string-surgery on
# the results table. This version uses octokit.rb (one of GitHub's own
# officially-maintained Octokit ports) and Ruby's real String/Array/Hash
# handling instead of sed/awk/jq pipelines for that surgery.
#
# Requires Ruby >= 3.2 (the `octokit` gem's `public_suffix` dependency does)
# and the `octokit` gem: `gem install octokit --user-install`. On a machine
# whose system Ruby is older (macOS ships 2.6.x), use a Homebrew Ruby:
#   /opt/homebrew/opt/ruby/bin/gem install octokit --user-install
#   /opt/homebrew/opt/ruby/bin/ruby scripts/test-run.rb ...
#
# Auth: uses `gh`'s stored token via `gh auth token`, so no separate
# credential setup is needed on a machine that already has `gh` logged in.
#
# Catalog source: holler-server issues labeled `test-case` whose body
# contains a "| Test ID |" header row (the "Test case slot -- reserved"
# placeholders are skipped). Each such issue's header table supplies:
#   Test ID     -- e.g. TC-001, or hlrsvr-1000/hlrclnt-1000 once remapped
#   Applies to  -- server / client / both
#   Group       -- invocation / lifecycle / logging / io / platform /
#                 concurrency / network / diagnostics / crypto / load
#                 (should match the case's test-grp-* label; the group is the
#                 hundreds digit of the Test ID -- invocation=1000 ..
#                 load=1900. nil on the pre-existing TC-NNN cases that predate
#                 this field. exec's --group matches on the test-grp-* LABEL,
#                 treating this field only as a fallback, because a few
#                 catalog issues carry a stale body value that disagrees with
#                 the label -- e.g. the io-group cases #177/#178 are labeled
#                 test-grp-logging. A third, open-ended tag axis --
#                 test-tag-*, added in issue #305 -- also exists in the
#                 catalog; exec consumes it as a selection filter via
#                 --tag/--tag-invert (issue #304).)
#   Automation  -- free-text pointer(s) to the automated assertion(s), or a
#                 string starting with "manual" for manual-only cases
#
# Automation-field convention this script actually executes (exact match
# for anything written this way; best-effort for the pre-existing free-text
# entries):
#   "<repo>: tests/<file>.rs"        -> cargo test --test <file>
#   "<repo>: tests/<file>.rs (<fn>)" -> cargo test --test <file> <fn>
#   "<repo>: src/<path>.rs (<fn>)"   -> cargo test --lib <fn> -- an inline
#     #[cfg(test)] unit test living in the crate's own src/ tree (not a
#     separate tests/*.rs integration binary). `--lib` scopes the run to
#     the crate's own unit-test binary so a same-named test elsewhere
#     (an integration file, the sibling repo) can't get matched instead --
#     verified live: `cargo test --lib <fn>` runs exactly one test, not the
#     whole crate, for every case this form was introduced for.
#   segments separated by "; "       -> run each; ALL must pass
#   starts with "manual"             -> not run; stays pending for `record`
#   unparseable (e.g. a bare src/ pointer with no `(<fn>)`) -> runs that
#     repo's whole `cargo test` as a conservative fallback; evidence says
#     a fallback ran
#
# `run` batches unit tests (issue #248): a case labeled `test-cat-unit`
# whose Automation is a single, unqualified "<repo>: src/<path>.rs (<fn>)"
# segment gets grouped with every other such case in the same repo and run
# together as ONE `cargo test --lib` invocation instead of one subprocess
# per case -- `--lib` already runs a repo's whole unit-test binary
# regardless of case count, so per-case subprocesses were pure overhead.
# Per-case results are parsed back out of that one run's `test <path> ...
# ok|FAILED` lines (matched by bare function name, same uniqueness
# assumption the single-case `--lib <fn>` form already relies on) and
# recorded individually, same as an unbatched case. Anything not matching
# that exact shape (integration cases, manual cases, multi-segment
# automations, non-`test-cat-unit` cases) runs one at a time as before --
# batching only ever changes *how* a result is produced, never what gets
# recorded. `exec TEST_ID` (single-case local convenience) is unaffected;
# batching only applies to `run`'s full pass over many cases at once.
#
# Usage:
#   ruby scripts/test-run.rb discover
#   ruby scripts/test-run.rb start [--applies server|client|both|all] [--type auto|manual|all]
#   ruby scripts/test-run.rb run ISSUE --server-dir DIR --client-dir DIR
#   ruby scripts/test-run.rb record ISSUE TEST_ID pass|fail [note]
#   ruby scripts/test-run.rb exec [TEST_ID] [--applies X] [--group G] [--tag S...] \
#                                    [--tag-invert S...] [--list F] [--list-invert F] \
#                                    [--list] [--server-dir DIR] [--client-dir DIR]
#     Runs the selected test case(s)' Automation fields locally, right now,
#     streaming real `cargo test` output live -- no GitHub write of any
#     kind (no test-run issue, no comment). A positional TEST_ID still works
#     as before (just the ID, e.g. `exec hlrsvr-1000`) and composes (ANDs)
#     with the selection flags. The five flags --group/--applies/--tag/
#     --tag-invert/--list are each an independent conjunct over the catalog
#     (Playwright-style; --applies also ANDs, and --tag is OR-within,
#     Playwright's --grep ∧ --project pattern). --list (bare, no file)
#     PREVIEWs the resolved Test IDs + their Automation commands one per
#     line and exits 0 without running anything or writing to GitHub.
#     --grep over case TITLES is deliberately NOT ported -- the test-tag-*
#     axis (issue #305) fills that role; see docs/running-tests.md. Exits
#     with the same status the underlying `cargo test` exits with
#     (exit 0 only if every selected case passed). --server-dir and
#     --client-dir default to ~/Projects/holler-server and
#     ~/Projects/holler-client (this machine's layout) if omitted.

require 'octokit'
require 'time'
require 'open3'
require 'optparse'
require 'set'
require_relative 'test_selection'

REPO = 'Performant-Labs/holler-server'
MARKER_START = '<!-- test-run-fields:start -->'
MARKER_END = '<!-- test-run-fields:end -->'

def client
  token = ENV['GITHUB_TOKEN']
  if token.nil? || token.empty?
    token, status = Open3.capture2('gh', 'auth', 'token')
    token = token.strip
    abort('error: no GITHUB_TOKEN and `gh auth token` failed -- run `gh auth login` first') if token.empty?
  end
  Octokit::Client.new(access_token: token, auto_paginate: true)
end

# ---------------------------------------------------------------------------
# discover: pull the filled test-case catalog as an Array of Hashes.
# ---------------------------------------------------------------------------
def discover(gh)
  issues = gh.list_issues(REPO, labels: 'test-case', state: 'open', per_page: 100)
  issues.flat_map do |issue|
    body = issue.body || ''
    next [] unless body.include?('| Test ID |')
    next [] if issue.title.start_with?('Test case slot')

    # An `Applies to: both` case carries TWO "| Test ID |" rows in one issue
    # (one hlrsvr-*, one hlrclnt-*, per holler-server#98's contract) -- every
    # row becomes its own catalog entry, sharing this issue's other fields.
    # (Restored 2026-09-07: a concurrent, unrelated PR (#250, cut before this
    # fix originally landed in #249) squash-merged over it and silently
    # reverted discover/field back to a single-Test-ID-row assumption --
    # classic stale-branch-reverts-an-intervening-fix. #250's own batching
    # feature below is untouched; it only reads discover's output.)
    field_all(body, 'Test ID').map do |id|
      {
        issue: issue.number,
        title: issue.title,
        labels: issue.labels.map(&:name),
        id: id,
        applies: field(body, 'Applies to'),
        group: field(body, 'Group'),
        automation: field(body, 'Automation')
      }
    end
  end
end

def field(body, name)
  field_all(body, name).first
end

def field_all(body, name)
  body.lines.select { |l| l.strip.start_with?("| #{name} |") }.map do |line|
    # "| Field | Value |" -> "Value" (trim whitespace, keep everything between
    # the second and (last) closing pipe so a Value containing "|" inside code
    # spans isn't accidentally truncated at the wrong pipe).
    cells = line.strip.split('|').map(&:strip).reject(&:empty?)
    cells[1..].join(' | ')
  end
end

# ---------------------------------------------------------------------------
# start: create a new test-run issue from a filtered slice of the catalog.
# ---------------------------------------------------------------------------
def start(gh, applies_filter: 'all', type_filter: 'all')
  catalog = discover(gh)
  selected = catalog.select do |c|
    applies_ok = applies_filter == 'all' || c[:applies] == applies_filter
    type_ok =
      case type_filter
      when 'all' then true
      when 'auto' then c[:labels].include?('test-auto')
      when 'manual' then c[:labels].include?('test-manual')
      else false
      end
    applies_ok && type_ok
  end

  abort("error: start: no catalog entries matched --applies=#{applies_filter} --type=#{type_filter}") if selected.empty?

  now = Time.now.utc.strftime('%Y-%m-%d %H:%M UTC')
  rows = selected.map { |c| Row.new(id: c[:id], type: row_type(c[:labels]), status: '⏳ pending', evidence: '') }
  body = render_body(
    fields: {
      'Triggered by' => 'manual test-run.rb invocation',
      'Server commit' => `git rev-parse --short HEAD 2>/dev/null`.strip.then { |s| s.empty? ? 'unknown' : s },
      'Client commit' => 'unknown (fill in if a client checkout is involved)'
    },
    rows: rows,
    catalog: catalog
  )

  issue = gh.create_issue(REPO, "Test run: #{now}", body, labels: 'test-run')
  puts issue.html_url
end

def row_type(labels)
  auto = labels.include?('test-auto')
  manual = labels.include?('test-manual')
  return 'auto+manual' if auto && manual
  return 'auto' if auto

  'manual'
end

Row = Struct.new(:id, :type, :status, :evidence, keyword_init: true)

def render_body(fields:, rows:, catalog:)
  passed = rows.count { |r| r.status.include?('✅') }
  pending = rows.count { |r| r.status.include?('pending') }
  total = rows.size
  overall = "#{passed}/#{total} passed (#{pending} pending)"

  lines = []
  lines << MARKER_START
  lines << '| Field | Value |'
  lines << '|---|---|'
  fields.each { |k, v| lines << "| #{k} | #{v} |" }
  lines << "| Overall | #{overall} |"
  lines << ''
  lines << '| Test Case | Type | Status | Evidence |'
  lines << '|---|---|---|---|'
  rows.each do |r|
    cat = catalog.find { |c| c[:id] == r.id }
    link = cat ? "https://github.com/#{REPO}/issues/#{cat[:issue]}" : ''
    lines << "| [#{r.id}](#{link}) | #{r.type} | #{r.status} | #{r.evidence} |"
  end
  lines << MARKER_END
  lines.join("\n")
end

# ---------------------------------------------------------------------------
# Shared: parse the current results table out of a test-run issue body.
# ---------------------------------------------------------------------------
def extract_rows(body)
  block = body[/#{Regexp.escape(MARKER_START)}(.*)#{Regexp.escape(MARKER_END)}/m, 1] || ''
  block.lines.filter_map do |line|
    next nil unless line.strip.start_with?('| [')

    m = line.match(/^\|\s*\[([^\]]+)\][^|]*\|\s*([^|]*?)\s*\|\s*([^|]*?)\s*\|\s*(.*?)\s*\|\s*$/)
    next nil unless m

    Row.new(id: m[1], type: m[2], status: m[3], evidence: m[4])
  end
end

def fields_before_table(body)
  block = body[/#{Regexp.escape(MARKER_START)}(.*?)\n\n/m, 1] || ''
  fields = {}
  block.lines.each do |line|
    next unless line.strip.start_with?('|') && !line.include?('---') && !line.include?('| Field |')

    cells = line.strip.split('|').map(&:strip).reject(&:empty?)
    fields[cells[0]] = cells[1] if cells.size >= 2
  end
  fields.reject { |k, _| k == 'Overall' }
end

def splice_body(body, rows, catalog)
  fields = fields_before_table(body)
  new_block = render_body(fields: fields, rows: rows, catalog: catalog)
  body.sub(/#{Regexp.escape(MARKER_START)}.*#{Regexp.escape(MARKER_END)}/m, new_block)
end

# ---------------------------------------------------------------------------
# run: execute pending automated cases in a test-run issue for real.
# ---------------------------------------------------------------------------

# Cases carrying this label AND a single, unqualified `src/<path>.rs (<fn>)`
# Automation field (issue #248) get batched: every such pending case for one
# repo runs together as one `cargo test --lib` invocation instead of one
# `cargo test --lib <fn>` subprocess per case. `--lib` already runs a
# repo's entire unit-test binary in one process regardless of how many
# individual tests it contains, so running N of them as N separate
# subprocesses was pure per-process overhead multiplied by N, not N times
# the actual test work. Batching changes only *how* a result is produced --
# each case in the batch still gets recorded with its own status/evidence,
# exactly as an individually-run case would.
UNIT_BATCH_LABEL = 'test-cat-unit'

Segment = Struct.new(:repo, :file, :fn, :lib_test, :fallback_used, :dir, :cmd, :error, keyword_init: true)

# Parses one ';'-separated Automation segment into a Segment describing what
# to run, using exactly the grammar documented at the top of this file.
# Shared by run_cases (captures output) and exec_test (streams it live) so
# the two can't silently drift onto different grammars over time.
def parse_segment(seg, server_dir:, client_dir:)
  seg = seg.strip
  repo = file = fn = nil
  lib_test = false
  fallback_used = false

  if (m = seg.match(%r{\A([a-zA-Z0-9_-]+):\s*tests/([A-Za-z0-9_]+)\.rs(?:\s*\(([a-zA-Z0-9_]+)\))?\z}))
    repo, file, fn = m[1], m[2], m[3]
  elsif (m = seg.match(%r{\A([a-zA-Z0-9_-]+):\s*src/[A-Za-z0-9_/]+\.rs\s*\(([a-zA-Z0-9_]+)\)\z}))
    repo, fn = m[1], m[2]
    lib_test = true
  elsif (m = seg.match(/\A([a-zA-Z0-9_-]+):/))
    repo = m[1]
    fallback_used = true
  else
    return Segment.new(error: "[unparseable automation segment: #{seg}]\n")
  end

  dir = { 'holler-server' => server_dir, 'holler-client' => client_dir }[repo]
  return Segment.new(repo: repo, error: "[unknown repo in automation: #{repo}]\n") unless dir
  return Segment.new(repo: repo, error: "[no checkout at #{dir} for #{repo}]\n") unless Dir.exist?(dir)

  cmd = if lib_test
          "cargo test --lib #{fn}"
        elsif file
          fn ? "cargo test --test #{file} #{fn}" : "cargo test --test #{file}"
        else
          'cargo test'
        end

  Segment.new(repo: repo, file: file, fn: fn, lib_test: lib_test,
              fallback_used: fallback_used, dir: dir, cmd: cmd)
end

# Runs a fully-resolved Segment's command for real via bash -lc (so
# ~/.cargo/env gets sourced), returning [ok, output, exitstatus]. A filter
# matching zero tests still exits 0 -- cargo has no way to say "your filter
# named nothing real" -- so a named-fn segment with 0 passed/0 failed is
# treated as a failure rather than silently recorded as a pass.
def exec_segment_captured(seg)
  full_cmd = "source \"$HOME/.cargo/env\" 2>/dev/null; #{seg.cmd}"
  out, status = Open3.capture2e('bash', '-lc', full_cmd, chdir: seg.dir)
  ok = status.success?
  if ok && seg.fn && (m = out.match(/^test result: \w+\. (\d+) passed; (\d+) failed;.*?(\d+) filtered out/))
    ok = false if m[1].to_i.zero? && m[2].to_i.zero?
  end
  [ok, out, status.exitstatus]
end

# Runs `cargo test --lib` once in `dir` (no `<fn>` filter -- the whole
# unit-test binary) and parses cargo's own `test <path> ... ok|FAILED`
# lines back into a { bare_fn_name => passed? } map, keyed on the last
# `::`-segment of each test's full path -- the same bare-name matching
# `cargo test --lib <fn>` already relies on elsewhere in this file (see the
# module doc comment's note that bare names have been verified unique per
# repo for every case introduced this way).
def run_unit_batch(dir)
  full_cmd = "source \"$HOME/.cargo/env\" 2>/dev/null; cargo test --lib"
  out, = Open3.capture2e('bash', '-lc', full_cmd, chdir: dir)
  results = {}
  out.each_line do |line|
    m = line.match(/^test (\S+) \.\.\. (ok|FAILED)/)
    next unless m

    results[m[1].split('::').last] = (m[2] == 'ok')
  end
  { out: out, results: results }
end

# Partitions `rows` into { row.id => [repo, fn] } for every pending, auto,
# single-segment, `test-cat-unit`-labeled case whose Automation field
# resolves to a plain lib-test Segment -- everything else (integration
# cases, manual cases, multi-segment `; `-joined automations, anything
# `parse_segment` can't cleanly resolve) is deliberately left out and
# continues to run one case at a time exactly as before.
def partition_unit_batchable(rows, catalog, server_dir:, client_dir:)
  batchable = {}
  rows.each do |row|
    next unless row.status.include?('pending') && row.type.include?('auto')

    cat = catalog.find { |c| c[:id] == row.id }
    next unless cat && cat[:labels].include?(UNIT_BATCH_LABEL)

    automation = cat[:automation]
    next if automation.nil? || automation.empty? || automation =~ /\Amanual/i || automation.include?(';')

    seg = parse_segment(automation, server_dir: server_dir, client_dir: client_dir)
    next if seg.error || !seg.lib_test

    batchable[row.id] = [seg.repo, seg.fn]
  end
  batchable
end

def run_cases(gh, issue_number, server_dir:, client_dir:)
  issue = gh.issue(REPO, issue_number)
  catalog = discover(gh)
  rows = extract_rows(issue.body)
  dirs = { 'holler-server' => server_dir, 'holler-client' => client_dir }

  batchable = partition_unit_batchable(rows, catalog, server_dir: server_dir, client_dir: client_dir)
  batch_runs = {}
  batchable.values.map(&:first).uniq.each do |repo|
    puts "==> batched unit run: #{repo} (cargo test --lib)"
    batch_runs[repo] = run_unit_batch(dirs[repo])
  end

  rows.each do |row|
    unless row.status.include?('pending')
      next # already resolved by a prior run/record
    end

    unless row.type.include?('auto')
      row.evidence = row.evidence.to_s.empty? ? '' : row.evidence
      next # manual-only, stays pending for `record`
    end

    cat = catalog.find { |c| c[:id] == row.id }
    automation = cat && cat[:automation]

    if automation.nil? || automation.empty? || automation =~ /\Amanual/i
      row.status = '⏳ pending — manual, use `record`'
      next
    end

    if (repo_fn = batchable[row.id])
      repo, fn = repo_fn
      batch = batch_runs[repo]
      found = batch[:results].key?(fn)
      passed = found && batch[:results][fn]
      ts = Time.now.utc.strftime('%Y-%m-%dT%H:%MZ')

      if passed
        row.status = '✅ pass'
        row.evidence = "batched unit run #{ts}"
      else
        row.status = '❌ fail'
        note = found ? '' : " -- test '#{fn}' not found in --lib output (renamed or removed?)"
        row.evidence = "batched unit run #{ts} — see comment"
        comment_body = "### Result for `#{row.id}`: #{row.status}\n\n" \
                       "Part of a batched `cargo test --lib` run in #{repo} (issue #248)#{note}.\n\n" \
                       "```\n#{batch[:out].lines.last(25).join}\n```"
        comment = gh.add_comment(REPO, issue_number, comment_body)
        row.evidence = "[#{row.evidence}](#{comment.html_url})"
      end
      puts "==> #{row.id}: #{row.status} (batched, #{repo})"
      next
    end

    puts "==> #{row.id}: #{automation}"
    all_ok = true
    fallback_used = false
    log = +''

    automation.split(';').each do |raw_seg|
      seg = parse_segment(raw_seg, server_dir: server_dir, client_dir: client_dir)
      if seg.error
        all_ok = false
        log << seg.error
        next
      end
      fallback_used ||= seg.fallback_used

      seg_ok, out, exitstatus = exec_segment_captured(seg)
      if !seg_ok && seg.fn
        m = out.match(/^test result: \w+\. (\d+) passed; (\d+) failed;.*?(\d+) filtered out/)
        if m && m[1].to_i.zero? && m[2].to_i.zero?
          log << "[named test '#{seg.fn}' did not run -- filtered out or does not exist#{seg.file ? " in #{seg.file}.rs" : ''}]\n"
        end
      end

      all_ok &&= seg_ok
      target = if seg.lib_test then '--lib'
               elsif seg.file then seg.file
               else 'whole crate'
               end
      log << "\n--- #{seg.repo} (#{target}#{seg.fn ? " / #{seg.fn}" : ''}), exit #{exitstatus} ---\n"
      log << out.lines.last(25).join
    end

    ts = Time.now.utc.strftime('%Y-%m-%dT%H:%MZ')
    if all_ok
      row.status = '✅ pass'
      row.evidence = "local run #{ts}"
    else
      row.status = '❌ fail'
      row.evidence = "local run #{ts} — see comment"
    end
    row.evidence += ' (fallback: whole-crate run, automation field not precisely parseable)' if fallback_used

    if row.status == '❌ fail' || fallback_used
      comment_body = "### Result for `#{row.id}`: #{row.status}\n\n```\n#{log}\n```"
      comment = gh.add_comment(REPO, issue_number, comment_body)
      row.evidence = "[#{row.evidence}](#{comment.html_url})"
    end
  end

  new_body = splice_body(issue.body, rows, catalog)
  gh.update_issue(REPO, issue_number, body: new_body)
  passed = rows.count { |r| r.status.include?('✅') }
  puts "Updated https://github.com/#{REPO}/issues/#{issue_number} -- #{passed}/#{rows.size} passed"
end

# ---------------------------------------------------------------------------
# record: manually record one result (typically a manual-labeled case).
# ---------------------------------------------------------------------------
def record(gh, issue_number, target_id, result, note)
  abort("error: record: result must be 'pass' or 'fail'") unless %w[pass fail].include?(result)

  issue = gh.issue(REPO, issue_number)
  catalog = discover(gh)
  rows = extract_rows(issue.body)

  target = rows.find { |r| r.id == target_id }
  abort("error: record: test id '#{target_id}' not found in the results table of issue ##{issue_number}") unless target

  ts = Time.now.utc.strftime('%Y-%m-%dT%H:%MZ')
  target.status = result == 'pass' ? '✅ pass' : '❌ fail'
  target.evidence = "manual, recorded #{ts}#{note && !note.empty? ? " — #{note}" : ''}"

  new_body = splice_body(issue.body, rows, catalog)
  gh.update_issue(REPO, issue_number, body: new_body)
  puts "Updated https://github.com/#{REPO}/issues/#{issue_number} -- #{target_id} -> #{target.status}"
end

# ---------------------------------------------------------------------------
# exec: run ONE test case's Automation field locally, live, for a human at
# a terminal -- no GitHub write of any kind (no test-run issue, no comment).
# Streams real `cargo test` output as it happens instead of capturing it,
# and exits with the same status the underlying command(s) exit with, so
# it composes with shell scripting ("test-run.rb exec hlrsvr-1000 || ...").
# ---------------------------------------------------------------------------
# entries: the pre-resolved Array of discover() catalog Hashes to run, in
# order (exactly one for a single positional Test ID). Runs each entry's
# Automation field for real, live, and exits 0 only if ALL passed.
def exec_test(gh, entries, server_dir:, client_dir:)
  overall_ok = true
  entries.each do |cat|
    test_id = cat[:id]
    automation = cat[:automation]
    if automation.nil? || automation.empty? || automation =~ /\Amanual/i
      warn "error: exec: #{test_id} is a manual case with no automated command to run (Automation: #{automation.inspect}) -- skipping"
      overall_ok = false
      next
    end

    puts "==> #{test_id}: #{automation}"
    automation.split(';').each do |raw_seg|
      seg = parse_segment(raw_seg, server_dir: server_dir, client_dir: client_dir)
      if seg.error
        warn seg.error
        overall_ok = false
        next
      end

      puts "--- #{seg.repo} $ #{seg.cmd} (in #{seg.dir}) ---"
      full_cmd = "source \"$HOME/.cargo/env\" 2>/dev/null; #{seg.cmd}"
      ok = system('bash', '-lc', full_cmd, chdir: seg.dir)
      overall_ok &&= ok
    end
  end

  exit(overall_ok ? 0 : 1)
end

# ---------------------------------------------------------------------------
def main
  cmd = ARGV.shift
  gh = client

  case cmd
  when 'discover'
    require 'json'
    puts JSON.pretty_generate(discover(gh))
  when 'start'
    opts = { applies: 'all', type: 'all' }
    OptionParser.new do |o|
      o.on('--applies X') { |v| opts[:applies] = v }
      o.on('--type X') { |v| opts[:type] = v }
    end.parse!(ARGV)
    start(gh, applies_filter: opts[:applies], type_filter: opts[:type])
  when 'run'
    issue = ARGV.shift or abort('usage: run ISSUE --server-dir DIR --client-dir DIR')
    opts = {}
    OptionParser.new do |o|
      o.on('--server-dir DIR') { |v| opts[:server] = v }
      o.on('--client-dir DIR') { |v| opts[:client] = v }
    end.parse!(ARGV)
    abort('error: run: --server-dir is required') unless opts[:server]
    abort('error: run: --client-dir is required') unless opts[:client]
    run_cases(gh, issue.to_i, server_dir: opts[:server], client_dir: opts[:client])
  when 'record'
    issue = ARGV.shift or abort('usage: record ISSUE TEST_ID pass|fail [note]')
    test_id = ARGV.shift or abort('usage: record ISSUE TEST_ID pass|fail [note]')
    result = ARGV.shift or abort('usage: record ISSUE TEST_ID pass|fail [note]')
    note = ARGV.shift || ''
    record(gh, issue.to_i, test_id, result, note)
  when 'exec'
    # Positional TEST_ID is OPTIONAL (selection flags may stand in for it).
    # Only shift it when the first remaining arg is NOT an option -- a bare
    # `ARGV.shift` would otherwise grab `--group` (the first flag) as the ID
    # on a flag-only invocation, which is exactly the mis-parse we must avoid.
    # The pack's "no `or abort`" intent is preserved: a bare `exec` (no
    # position, no flags) still reaches the has_selection guard below and
    # prints usage, so it exits non-zero.
    test_id = (ARGV.first && !ARGV.first.start_with?('-')) ? ARGV.shift : nil
    opts = {
      server: File.expand_path('~/Projects/holler-server'),
      client: File.expand_path('~/Projects/holler-client')
    }
    # Hoisted (not block-local) so the has_selection abort below can reuse it
    # after the OptionParser block scope has ended.
    exec_banner = "usage: exec [TEST_ID] [--applies X] [--group G] [--tag S...] " \
                  "[--tag-invert S...] [--list F] [--list-invert F] [--list] " \
                  "[--server-dir DIR] [--client-dir DIR]"
    OptionParser.new do |o|
      o.banner = exec_banner
      o.on('--server-dir DIR') { |v| opts[:server] = v }
      o.on('--client-dir DIR') { |v| opts[:client] = v }
      o.on('--group G') { |v| opts[:group] = v }
      o.on('--applies X') { |v| opts[:applies] = v }
      o.on('--tag S', 'test-tag-<S> to select (OR-within, multiple allowed)') { |v| (opts[:tags] ||= []) << v }
      o.on('--tag-invert S', 'exclude cases carrying test-tag-<S> (multiple allowed)') { |v| (opts[:tag_inverts] ||= []) << v }
      o.on('--list [FILE]', 'bare: preview resolved IDs without running; FILE: keep only those IDs') { |v| opts[:list] = v; opts[:preview] = (v.nil?) }
      o.on('--list-invert FILE', 'exclude the Test IDs listed in FILE') { |v| opts[:list_invert] = v }
      o.on('-h', '--help') { puts o.banner; exit 0 }
    end.parse!(ARGV)

    has_selection = !test_id.nil? || !opts[:group].nil? || !opts[:applies].nil? ||
                    !opts[:tags].nil? || !opts[:tag_inverts].nil? ||
                    !opts[:list].nil? || !opts[:list_invert].nil?
    abort("#{exec_banner}") unless has_selection

    catalog = discover(gh)
    # A positional TEST_ID narrows the catalog to that one entry (the legacy
    # single-case path). It then ANDs with any selection flags below, so
    # `exec <ID> --group G` runs <ID> only if <ID> also matches the flags.
    selected = test_id ? catalog.select { |c| c[:id] == test_id } : catalog

    # Which flags were given (used both to decide whether to AND them and to
    # name them in the "no match" error). `--list` is a FILE here (its bare
    # preview form set opts[:preview] and left opts[:list] nil).
    active =
      (opts[:group] ? ['group: ' + opts[:group]] : []) +
      (opts[:applies] ? ['applies: ' + opts[:applies]] : []) +
      (opts[:tags] ? ['tag: ' + opts[:tags].join(',')] : []) +
      (opts[:tag_inverts] ? ['tag-invert: ' + opts[:tag_inverts].join(',')] : []) +
      (opts[:list_invert] ? ['list-invert: ' + opts[:list_invert]] : [])
    active = ['list: ' + opts[:list]] unless opts[:list].nil? || opts[:list] == ''

    # No flags at all -> the legacy positional path: preserve today's exact
    # message for an unknown ID, and an empty selection is impossible here.
    if active.empty?
      abort("error: exec: test id '#{test_id}' not found in the catalog") if test_id && selected.empty?
    else
      # Flags given (with or without a positional ID): apply them as a
      # conjunction over the (possibly positional-narrowed) set.
      sel = TestSelection.new(group: opts[:group], applies: opts[:applies], tags: opts[:tags],
                               tag_inverts: opts[:tag_inverts],
                               list_ids: (opts[:list] && !opts[:preview]) ? TestSelection.read_list_file(opts[:list]) : nil,
                               list_invert_ids: opts[:list_invert] ? TestSelection.read_list_file(opts[:list_invert]) : nil)
      selected = sel.call(selected)
      if opts[:preview]
        selected.each { |c| puts "#{c[:id]}  #{c[:automation] || '(none)'}" }
        puts "#{selected.size} case(s) matched"
        exit(0)
      end
      # Flags narrowed to nothing (also covers "positional ID given but the
      # selection flags exclude it" -- e.g. `exec hlrsvr-1000 --group concurrency`).
      abort("error: exec: no catalog cases matched the selection (#{active.join(', ')})") if selected.empty?
    end

    exec_test(gh, selected, server_dir: opts[:server], client_dir: opts[:client])
  else
    abort("usage: #{$PROGRAM_NAME} {discover|start|run|record|exec} ...")
  end
end

main if __FILE__ == $PROGRAM_NAME
