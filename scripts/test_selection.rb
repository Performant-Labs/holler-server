# frozen_string_literal: true

# test_selection.rb -- the selection core for `exec` (issue #304): a
# conjunction over the discover() catalog of independent, composable axes.
#
# The axes, each nil/empty == "no constraint on this axis":
#   group       -- one test-grp-* group (full name, label stem, or the raw
#                  test-grp-<stem> label); the "where" axis.
#   applies     -- Applies to: server/client/both, or 'all' == no constraint.
#   tags        -- an OPEN-ENDED set of test-tag-* tags; a case matches if
#                  it carries ANY of them (OR within the list). Unknown tags
#                  are not errors -- they simply select nothing.
#   tag_inverts -- test-tag-* tags to EXCLUDE; a case is dropped if it
#                  carries ANY of them. Unknown tags simply exclude nothing.
#   list_ids    -- explicit Test IDs to keep (from --list FILE).
#   list_invert_ids -- explicit Test IDs to drop (from --list-invert FILE).
#
# Every axis applies to the survivors of the previous axis (conjunction),
# matching Playwright's --grep ∧ --project model. Result order is whatever
# discover() produced (hlrsvr-* before hlrclnt-*, ascending Test ID within
# each repo) -- filtering must not reorder.
#
# Deliberately dependency-free (stdlib only, Ruby 2.6-compatible): it must run
# on a runner with nothing installed, unlike test-run.rb's octokit use. It
# takes the catalog as an argument so it is testable without a GitHub client.
# `set` is required here (not in test-run.rb) because call() builds a Set from
# the --list ID arrays and, being dependency-free, must not assume the Set
# class is already loaded by whatever loaded this file.
require 'set'

module TestSelection
  # Full group name -> label stem. Authoritative list lives in #98's Test ID
  # group tables; this is the script-side mirror of it.
  GROUPS = {
    'invocation' => 'invoc',
    'lifecycle' => 'lifecycle',
    'logging' => 'logging',
    'io' => 'io',
    'platform' => 'platform',
    'concurrency' => 'concurrency',
    'network' => 'network',
    'diagnostics' => 'diagnostics',
    'crypto' => 'crypto',
    'load' => 'load'
  }.freeze

  def self.valid_groups
    GROUPS.keys
  end

  # A bare module has no `Module#new` (only `Class` does), so define one here.
  # It builds a fresh anonymous class that `include`s this module and returns
  # an instance of it -- so the returned object's *instance* methods are
  # exactly TestSelection's (initialize/any?/call/attr_reader). `Class#new`
  # then invokes our `initialize`. (test-run.rb's call site is
  # `sel = TestSelection.new(...); sel.call(catalog)`; the tests do the same.)
  def self.new(group: nil, applies: nil, tags: nil, tag_inverts: nil, list_ids: nil, list_invert_ids: nil)
    Class.new { include TestSelection }.new(group: group, applies: applies, tags: tags, tag_inverts: tag_inverts, list_ids: list_ids, list_invert_ids: list_invert_ids)
  end

  attr_reader :group, :applies, :tags, :tag_inverts, :list_ids, :list_invert_ids

  def initialize(group: nil, applies: nil, tags: nil, tag_inverts: nil, list_ids: nil, list_invert_ids: nil)
    @group = group
    @applies = applies
    @tags = tags
    @tag_inverts = tag_inverts
    @list_ids = list_ids
    @list_invert_ids = list_invert_ids
  end

  # No args: "is any axis constrained?" (the dispatch uses this to decide
  # whether a bare `exec` should just print usage).
  def any?
    !group.nil? || !applies.nil? || !tags.nil? || !tag_inverts.nil? || !list_ids.nil? || !list_invert_ids.nil?
  end

  # Reads a --list FILE into an Array of Test IDs: one per line, whitespace
  # trimmed, blank lines and `#` comment lines skipped. abort()s if missing.
  def self.read_list_file(path)
    abort("error: exec: list file '#{path}' not found") unless File.exist?(path)

    File.readlines(path).map { |l| l.strip }.reject { |l| l.empty? || l.start_with?('#') }
  end

  # Resolves a user-supplied group value to its label stem. Accepts the full
  # name ('concurrency'), the label stem ('invoc'), or the raw label
  # ('test-grp-concurrency'). abort()s (listing the valid groups) if the
  # resolved stem is not a known group.
  def resolve_group(v)
    gv = v.to_s
    stem = if GROUPS.key?(gv)
             GROUPS[gv]
           else
             gv.sub(/^test-grp-/, '')
           end
    abort("error: exec: unknown group '#{gv}' -- valid groups: #{GROUPS.keys.join(', ')}") unless GROUPS.values.include?(stem)

    stem
  end

  # Applies every non-nil axis to `catalog` (an Array of discover() Hashes),
  # in order, returning the surviving entries in catalog order.
  def call(catalog)
    out = catalog

    out = out.select { |c| group_matches?(c, resolve_group(@group)) } if @group
    out = out.select { |c| @applies == 'all' || c[:applies] == @applies } if @applies
    unless @tags.nil? || @tags.empty?
      wanted = @tags.map { |s| "test-tag-#{s}" }
      out = out.select { |c| wanted.any? { |l| c[:labels].include?(l) } }
    end
    unless @tag_inverts.nil? || @tag_inverts.empty?
      banned = @tag_inverts.map { |s| "test-tag-#{s}" }
      out = out.reject { |c| banned.any? { |l| c[:labels].include?(l) } }
    end
    unless @list_ids.nil?
      set = @list_ids.to_set
      out = out.select { |c| set.include?(c[:id]) }
    end
    unless @list_invert_ids.nil?
      set = @list_invert_ids.to_set
      out = out.reject { |c| set.include?(c[:id]) }
    end
    out
  end

  private

  def group_matches?(c, stem)
    c[:labels].include?("test-grp-#{stem}") || c[:group] == stem
  end
end
