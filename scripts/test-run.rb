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
#                 concurrency / network / diagnostics / crypto (matches the
#                 test-grp-* label; nil on the pre-existing TC-NNN cases
#                 that predate this field, harmless -- nothing reads it yet)
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
# Usage:
#   ruby scripts/test-run.rb discover
#   ruby scripts/test-run.rb start [--applies server|client|both|all] [--type auto|manual|all]
#   ruby scripts/test-run.rb run ISSUE --server-dir DIR --client-dir DIR
#   ruby scripts/test-run.rb record ISSUE TEST_ID pass|fail [note]
#   ruby scripts/test-run.rb exec TEST_ID [--server-dir DIR] [--client-dir DIR]
#     Runs ONE test case's Automation field locally, right now, streaming
#     real `cargo test` output live -- no GitHub write of any kind (no
#     test-run issue, no comment). For "I have a Test ID from an issue and
#     just want to run that one test" -- you don't need to know its
#     file/function, just the ID (e.g. `exec hlrsvr-1000`). Exits with the
#     same status the underlying `cargo test` exits with. --server-dir and
#     --client-dir default to ~/Projects/holler-server and
#     ~/Projects/holler-client (this machine's layout) if omitted.

require 'octokit'
require 'time'
require 'open3'
require 'optparse'

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
  issues.filter_map do |issue|
    body = issue.body || ''
    next nil unless body.include?('| Test ID |')
    next nil if issue.title.start_with?('Test case slot')

    {
      issue: issue.number,
      title: issue.title,
      labels: issue.labels.map(&:name),
      id: field(body, 'Test ID'),
      applies: field(body, 'Applies to'),
      group: field(body, 'Group'),
      automation: field(body, 'Automation')
    }
  end
end

def field(body, name)
  line = body.lines.find { |l| l.strip.start_with?("| #{name} |") }
  return nil unless line

  # "| Field | Value |" -> "Value" (trim whitespace, keep everything between
  # the second and (last) closing pipe so a Value containing "|" inside code
  # spans isn't accidentally truncated at the wrong pipe).
  cells = line.strip.split('|').map(&:strip).reject(&:empty?)
  cells[1..].join(' | ')
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

def run_cases(gh, issue_number, server_dir:, client_dir:)
  issue = gh.issue(REPO, issue_number)
  catalog = discover(gh)
  rows = extract_rows(issue.body)

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
def exec_test(gh, test_id, server_dir:, client_dir:)
  catalog = discover(gh)
  cat = catalog.find { |c| c[:id] == test_id }
  abort("error: exec: test id '#{test_id}' not found in the catalog") unless cat

  automation = cat[:automation]
  if automation.nil? || automation.empty? || automation =~ /\Amanual/i
    abort("error: exec: #{test_id} is a manual case with no automated command to run " \
          "(Automation: #{automation.inspect})")
  end

  puts "==> #{test_id}: #{automation}"
  overall_ok = true

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
    test_id = ARGV.shift or abort('usage: exec TEST_ID [--server-dir DIR] [--client-dir DIR]')
    opts = {
      server: File.expand_path('~/Projects/holler-server'),
      client: File.expand_path('~/Projects/holler-client')
    }
    OptionParser.new do |o|
      o.on('--server-dir DIR') { |v| opts[:server] = v }
      o.on('--client-dir DIR') { |v| opts[:client] = v }
    end.parse!(ARGV)
    exec_test(gh, test_id, server_dir: opts[:server], client_dir: opts[:client])
  else
    abort("usage: #{$PROGRAM_NAME} {discover|start|run|record|exec} ...")
  end
end

main if __FILE__ == $PROGRAM_NAME
