#!/usr/bin/env ruby
# frozen_string_literal: true

# test_selection_test.rb -- unit tests for scripts/test_selection.rb
# (issue #304's selection core). Deliberately stdlib-only (minitest, no
# octokit) so it runs on a runner's preinstalled Ruby (2.6 on macOS) with no
# gem install -- see the CI step that runs it.

require 'minitest/autorun'
require 'tempfile'

require_relative 'test_selection'

class TestSelectionTest < Minitest::Test
  # A synthetic discover()-shaped catalog: mixed repos, out-of-group IDs to
  # prove ordering is the input order (filtering must not reorder), and
  # realistic label sets (a group label plus tags).
  CATALOG = [
    { id: 'hlrsvr-1120', applies: 'server', group: 'concurrency', labels: %w[test-grp-concurrency test-tag-alters-db], title: 'srv A' },
    { id: 'hlrsvr-1130', applies: 'client', group: 'concurrency', labels: %w[test-grp-concurrency test-tag-remote], title: 'srv B' },
    { id: 'hlrclnt-1140', applies: 'server', group: 'concurrency', labels: %w[test-grp-concurrency test-tag-alters-db test-tag-slow], title: 'clnt C' },
    { id: 'hlrsvr-1000', applies: 'server', group: 'invocation', labels: %w[test-grp-invoc], title: 'invocation, no group label' },
    { id: 'hlrsvr-1010', applies: 'client', group: 'invocation', labels: %w[test-grp-invoc test-tag-alters-db], title: 'invocation, alters-db' }
  ].freeze

  def ids(result)
    result.map { |c| c[:id] }
  end

  # (a) No conjuncts -> all entries, in order.
  def test_no_conjuncts_selects_all
    sel = TestSelection.new
    assert_empty(sel.tags.to_a)
    # TestSelection.new returns an object that IS-A TestSelection (via an
    # anonymous including class, since a bare Module has no Module#new), so
    # assert_kind_of (not assert_instance_of, which is too strict on .class).
    assert_operator(sel, :kind_of?, TestSelection)
    assert_equal(CATALOG, sel.call(CATALOG))
  end

  # (b) --group by full name, label stem, and raw label -> same subset;
  #     an unknown group aborts listing the valid groups.
  def test_group_by_full_name_stem_and_label
    sel_full = TestSelection.new(group: 'concurrency')
    sel_stem = TestSelection.new(group: 'concurrency') # 'concurrency' is its own stem
    sel_label = TestSelection.new(group: 'test-grp-concurrency')
    # The invocation case proves full-name and stem are NOT identical
    # ('invocation' -> 'invoc'), i.e. full-name input goes through the map.
    sel_invoc_stem = TestSelection.new(group: 'invoc')
    sel_invoc_full = TestSelection.new(group: 'invocation')

    expected = %w[hlrsvr-1120 hlrsvr-1130 hlrclnt-1140]
    assert_equal(expected, ids(sel_full.call(CATALOG)))
    assert_equal(expected, ids(sel_stem.call(CATALOG)))
    assert_equal(expected, ids(sel_label.call(CATALOG)))
    assert_equal(%w[hlrsvr-1000 hlrsvr-1010], ids(sel_invoc_full.call(CATALOG)))
    assert_equal(%w[hlrsvr-1000 hlrsvr-1010], ids(sel_invoc_stem.call(CATALOG)))
  end

  def test_unknown_group_aborts_listing_valid_groups
    # $stderr is a write-only IO in Ruby, so capture abort's message by
    # redirecting it to a StringIO (a real read/write stream) during the call.
    require 'stringio'
    cap = StringIO.new
    original = $stderr
    $stderr = cap
    assert_raises(SystemExit) do
      TestSelection.new(group: 'bogus').call(CATALOG)
    end
  ensure
    $stderr = original
  end

  # The aborted stderr output must name the bad value and list every valid group.
  def test_unknown_group_error_names_value_and_lists_valid_groups
    require 'stringio'
    cap = StringIO.new
    original = $stderr
    $stderr = cap
    assert_raises(SystemExit) { TestSelection.new(group: 'bogus').call(CATALOG) }
  ensure
    $stderr = original
    msg = cap.string
    assert_match(/unknown group 'bogus'/, msg)
    TestSelection.valid_groups.each { |g| assert_match(g, msg) }
  end

  # (c) --applies filters; nil is no filter; 'all' keeps everything.
  def test_applies
    sel = TestSelection.new(applies: 'server')
    assert_equal(%w[hlrsvr-1120 hlrclnt-1140 hlrsvr-1000], ids(sel.call(CATALOG)))

    sel_all = TestSelection.new(applies: 'all')
    assert_equal(CATALOG, sel_all.call(CATALOG))

    sel_nil = TestSelection.new
    assert_equal(CATALOG, sel_nil.call(CATALOG))
  end

  # (d) --tag single, --tag a b OR-union, unknown tag -> empty,
  #      --tag-invert -> complement, unknown tag-invert -> everything.
  def test_tag_single_and_or_union
    sel_one = TestSelection.new(tags: %w[alters-db])
    assert_equal(%w[hlrsvr-1120 hlrclnt-1140 hlrsvr-1010], ids(sel_one.call(CATALOG)))

    sel_union = TestSelection.new(tags: %w[alters-db remote])
    assert_equal(%w[hlrsvr-1120 hlrsvr-1130 hlrclnt-1140 hlrsvr-1010], ids(sel_union.call(CATALOG)))
  end

  def test_unknown_tag_selects_nothing
    sel = TestSelection.new(tags: %w[never-applied])
    assert_equal([], ids(sel.call(CATALOG)))
  end

  def test_tag_invert_selects_complement
    sel = TestSelection.new(tag_inverts: %w[alters-db])
    assert_equal(%w[hlrsvr-1130 hlrsvr-1000], ids(sel.call(CATALOG)))
  end

  def test_unknown_tag_invert_selects_everything
    sel = TestSelection.new(tag_inverts: %w[never-applied])
    assert_equal(CATALOG, sel.call(CATALOG))
  end

  # (e) --list file subset / --list-invert file (with a # comment + blank line).
  def test_list_and_list_invert_files
    t = Tempfile.new('list304')
    t.write("hlrsvr-1120\n\n# a comment line\nhlrsvr-1130\n")
    t.close

    sel = TestSelection.new(list_ids: TestSelection.read_list_file(t.path))
    assert_equal(%w[hlrsvr-1120 hlrsvr-1130], ids(sel.call(CATALOG)))

    sel_inv = TestSelection.new(list_invert_ids: TestSelection.read_list_file(t.path))
    assert_equal(%w[hlrclnt-1140 hlrsvr-1000 hlrsvr-1010], ids(sel_inv.call(CATALOG)))
  ensure
    t&.close!
  end

  # (f) Composition: group AND tag AND tag-invert AND list-invert -> exact intersection.
  def test_composition_is_exact_intersection
    t = Tempfile.new('list304')
    t.write("hlrsvr-1130\n")
    t.close

    sel = TestSelection.new(
      group: 'concurrency',
      tags: %w[alters-db remote],
      tag_inverts: %w[slow],
      list_invert_ids: TestSelection.read_list_file(t.path)
    )
    # concurrency + (alters-db OR remote) - slow - {1130} = only hlrsvr-1120
    assert_equal(%w[hlrsvr-1120], ids(sel.call(CATALOG)))
  ensure
    t&.close!
  end

  # (g) Ordering: call() must NOT reorder what discover() gives it. Feed it
  # the three concurrency cases in hlrsvr-first / ascending order and confirm
  # the output is byte-identical to the input order (a reordering bug would
  # move hlrclnt-1140 ahead of the hlrsvr-* entries).
  def test_result_order_preserved
    ordered = [CATALOG[0], CATALOG[1], CATALOG[2]] # hlrsvr-1120, hlrsvr-1130, hlrclnt-1140
    sel = TestSelection.new(group: 'concurrency')
    assert_equal(%w[hlrsvr-1120 hlrsvr-1130 hlrclnt-1140], ids(sel.call(ordered)))
    # A shuffled-but-valid input must come back in ITS order, not re-sorted.
    shuffled = [CATALOG[2], CATALOG[0], CATALOG[1]]
    assert_equal(%w[hlrclnt-1140 hlrsvr-1120 hlrsvr-1130], ids(sel.call(shuffled)))
  end

  # (i) No conjuncts => the whole catalog ("all pending" is the no-selection
  #     default). Distinguishable from (a) only via the explicit any? gate the
  #     dispatch uses to decide "nothing given -> usage error". (Plain
  #     assert on the boolean, not assert_operator, so we don't rely on
  #     Object#any? being arity-0 in the runner's Ruby.)
  def test_no_selection_means_all_pending
    assert_equal(false, TestSelection.new.any?)
    assert_equal(CATALOG, TestSelection.new.call(CATALOG))
    assert_equal(true, TestSelection.new(group: 'concurrency').any?)
  end

  # (h) read_list_file missing file -> SystemExit.
  def test_read_list_file_missing_aborts
    assert_raises(SystemExit) { TestSelection.read_list_file('/nonexistent/test-selection-304') }
  end

  # valid_groups is the 10 authoritative full names.
  def test_valid_groups_is_ten_full_names
    assert_equal(10, TestSelection.valid_groups.size)
    assert_includes(TestSelection.valid_groups, 'concurrency')
    assert_includes(TestSelection.valid_groups, 'invocation')
    assert_includes(TestSelection.valid_groups, 'load')
  end
end
