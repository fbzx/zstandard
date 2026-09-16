use super::*;

fn naive_count_match_length(src: &[u8], left: usize, right: usize) -> usize {
    let max_len = src
        .len()
        .saturating_sub(right)
        .min(src.len().saturating_sub(left));
    let mut matched = 0usize;
    while matched < max_len && src[left + matched] == src[right + matched] {
        matched += 1;
    }
    matched
}

#[test]
fn word_at_a_time_match_count_matches_naive_for_overlapping_positions() {
    let mut src = Vec::with_capacity(96);
    while src.len() < 96 {
        src.extend_from_slice(b"abc123abcXYZabc123abcxy");
    }
    src.truncate(96);
    src[31] = b'Q';
    src[63] = b'R';
    src[95] = b'S';

    for left in 0..src.len() {
        for right in 0..src.len() {
            assert_eq!(
                count_match_length(&src, left, right),
                naive_count_match_length(&src, left, right),
                "left={left} right={right}",
            );
        }
    }
}

#[test]
fn word_at_a_time_match_count_handles_tail_mismatches_on_word_boundaries() {
    let src = b"01234567012345670123456001234567";

    assert_eq!(count_match_length(src, 0, 8), 15);
    assert_eq!(count_match_length(src, 0, 16), 7);
    assert_eq!(count_match_length(src, 8, 24), 8);
}

#[test]
fn plans_matches_from_an_external_prefix() {
    let prefix =
        b"GET /v2/customers/10000/orders?status=open&limit=50\n{\"customer_id\":10000,\"status\":\"open\"}\n";
    let src = b"GET /v2/customers/10000/orders?status=open&limit=50\n{\"customer_id\":10000,\"status\":\"open\"}\n";

    let plan = plan_sequences_with_params_and_prefix(
        src,
        prefix,
        RepeatOffsets::default(),
        MatchFinderParameters::default(),
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    assert_eq!(plan.literals.len(), 0);
}

#[test]
fn external_prefix_matcher_supports_cross_boundary_candidates() {
    let prefix = b"xxab";
    let src = b"ababababzz";
    let plan = plan_sequences_with_params_and_prefix(
        src,
        prefix,
        RepeatOffsets::default(),
        MatchFinderParameters::default(),
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, 2);
    assert!(plan.sequences[0].match_length >= 8);
}

#[test]
fn uses_the_segmented_matcher_for_a_later_non_empty_prefix_slot() {
    let prefix = b"xxab";
    let src = b"ababababzz";
    let plan = plan_sequences_with_params_and_prefixes(
        src,
        &[b"", prefix],
        RepeatOffsets::default(),
        MatchFinderParameters::default(),
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, 2);
}

#[test]
fn segmented_prefix_chain_supports_cross_segment_candidates() {
    let src = b"ababababzz";
    let plan = plan_sequences_with_params_and_prefixes(
        src,
        &[b"xx", b"ab"],
        RepeatOffsets::default(),
        MatchFinderParameters::default(),
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, 2);
    assert!(plan.sequences[0].match_length >= 8);
}

#[test]
fn binary_tree_segmented_prefix_planner_finds_dictionary_matches() {
    let src = b"ababababzz";
    let plan = plan_sequences_for_block(
        src,
        &[b"xx", b"ab"],
        RepeatOffsets::default(),
        MatchFinderParameters {
            parser_strategy: ParserStrategy::BinaryTreeLazy2,
            search_depth: 8,
            dictionary_search_depth: 8,
            ..MatchFinderParameters::default()
        },
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, 2);
    assert!(plan.sequences[0].match_length >= 8);
}

#[test]
fn btopt_segmented_prefix_planner_finds_dictionary_matches() {
    let src = b"customer=0012|region=us-east|tier=gold|invoice=42";
    let prefixes: [&[u8]; 2] = [b"customer=0012|", b"region=us-east|tier=gold|"];
    let plan = plan_sequences_for_block(
        src,
        &prefixes,
        RepeatOffsets::default(),
        MatchFinderParameters {
            parser_strategy: ParserStrategy::BinaryTreeOpt,
            search_depth: 12,
            dictionary_search_depth: 12,
            good_enough_match_length: 32,
            ..MatchFinderParameters::default()
        },
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert!(raw_offset >= prefixes.iter().map(|prefix| prefix.len()).sum::<usize>() as u32);
}

#[test]
fn no_prefix_planner_keeps_finding_repeated_sequences() {
    let src = b"pattern-0123456789-pattern-0123456789-pattern-0123456789";
    let plan = plan_sequences_with_params(
        src,
        RepeatOffsets::default(),
        MatchFinderParameters::default(),
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
}

#[test]
fn reusable_sequence_plan_keeps_allocated_buffers() {
    let large = b"pattern-0123456789-pattern-0123456789-pattern-0123456789-pattern-0123456789";
    let small = b"pattern-0123456789-pattern-0123456789";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::Fast,
        fast_search_step: 2,
        ..MatchFinderParameters::default()
    };
    let mut plan = SequencePlan::default();

    plan_sequences_for_block_into(&mut plan, large, &[&[]], RepeatOffsets::default(), params)
        .unwrap();
    assert!(!plan.sequences.is_empty());
    let literals_ptr = plan.literals.as_ptr();
    let sequences_ptr = plan.sequences.as_ptr();
    let literals_capacity = plan.literals.capacity();
    let sequences_capacity = plan.sequences.capacity();

    plan_sequences_for_block_into(&mut plan, small, &[&[]], RepeatOffsets::default(), params)
        .unwrap();

    assert_eq!(plan.literals.as_ptr(), literals_ptr);
    assert_eq!(plan.sequences.as_ptr(), sequences_ptr);
    assert_eq!(plan.literals.capacity(), literals_capacity);
    assert_eq!(plan.sequences.capacity(), sequences_capacity);
    assert!(!plan.sequences.is_empty());
}

#[test]
fn contiguous_block_state_reuses_prior_block_history() {
    let block = b"pattern-0123456789-pattern-0123456789";
    let mut src = Vec::new();
    src.extend_from_slice(block);
    src.extend_from_slice(block);

    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::Fast,
        fast_search_step: 2,
        ..MatchFinderParameters::default()
    };
    let mut state = ContiguousBlockMatchState::new(src.len(), params);
    let first = plan_sequences_for_contiguous_block(
        &src[..block.len()],
        0,
        RepeatOffsets::default(),
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    let second = plan_sequences_for_contiguous_block(
        &src,
        block.len(),
        first.repeat_offsets,
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    assert!(!second.sequences.is_empty());
    assert!(second.literals.len() < block.len());
}

#[test]
fn greedy_row_contiguous_block_keeps_rep1_before_searching_regular_candidates() {
    let src = b"abcde12345abcde12345TAILTAILTAIL";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::GreedyRow,
        search_log: 5,
        min_match: 5,
        lazy_search_depth: 0,
        ..MatchFinderParameters::default()
    };
    let mut state = ContiguousBlockMatchState::new(src.len(), params);
    state.insert_range(src, 0, 10);

    let plan = plan_sequences_for_contiguous_block(
        src,
        10,
        RepeatOffsets::from_values([10, 64, 8]),
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    assert_eq!(plan.sequences[0].literal_length, 1);
    assert_eq!(plan.sequences[0].match_length, 9);
    assert!(
        !plan
            .trace_row_searches
            .iter()
            .any(|search| search.pos == 10),
        "GreedyRow should store the baseline rep1 without a regular row search"
    );

    let mut repeat_offsets = RepeatOffsets::from_values([10, 64, 8]);
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, 10);
}

/// A regular match whose distance is still live in rep2 is coded as rep2.
///
/// The fixture is built for exactly that collision: rep2 is 11 and the only
/// match the row finder can find is at distance 11, with a literal before it so
/// the `literal_length == 0` remapping is not in play. Under C's lazy family
/// this stores `explicit_offbase(11)`, which is 14 -- offset code 3 with three
/// extra bits -- because `ZSTD_compressBlock_lazy_generic` never compares the
/// distance its search produced against `offset_1/2/3`. We store 2, which is
/// offset code 1 with none.
///
/// This test asserted the C behaviour until 2026-08-06 and was named for it;
/// see "The repcode substitution" in `docs/PARITY_PLAN.md` for why that is now
/// deliberate. What did not change is underneath: `resolve` still recovers a
/// distance of 11, and rep1 still ends up 11. The substitution is a cheaper
/// spelling of the same sequence, not a different one, and those two
/// assertions are what say so.
#[test]
fn greedy_row_contiguous_block_codes_regular_store_as_rep2_when_raw_offset_matches_rep2() {
    let src = b"Zabcde123450abcde12345TAILTAILTAIL";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::GreedyRow,
        search_log: 5,
        min_match: 5,
        lazy_search_depth: 0,
        ..MatchFinderParameters::default()
    };
    let mut state = ContiguousBlockMatchState::new(src.len(), params);
    state.insert_range(src, 0, 11);

    let plan = plan_sequences_for_contiguous_block(
        src,
        11,
        RepeatOffsets::from_values([20, 11, 8]),
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    assert_eq!(plan.sequences[0].literal_length, 1);
    assert_eq!(plan.sequences[0].match_length, 10);
    assert_eq!(plan.sequences[0].offset_value, 2);
    assert_ne!(plan.sequences[0].offset_value, explicit_offbase(11));

    let mut repeat_offsets = RepeatOffsets::from_values([20, 11, 8]);
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, 11);
    assert_eq!(repeat_offsets.values()[0], 11);
}

#[test]
fn row_collect_match_indices_group_width_one_keeps_rotated_order_and_low_limit_break() {
    let context = RowHashContext {
        total_hash_bits: 0,
        row_entries: 16,
        row_mask: 15,
        group_width: 1,
    };
    let location = RowHashLocation {
        rel_row: 0,
        tag: 0xAB,
        head_index: 7,
        head_grouped: 7,
        insert_index: row_next_insert_index(7, context.row_mask),
    };
    let mut row = [0u32; 16];
    let mut tag_row = [0u8; 16];
    tag_row[0] = location.head_index as u8;
    for (slot, index) in [(7usize, 100u32), (9, 90), (15, 80), (1, 70)] {
        tag_row[slot] = location.tag;
        row[slot] = index;
    }
    let mut match_positions = [0usize; ROW_HASH_MAX_ENTRIES];
    let mut match_buffer = [NO_POS; ROW_HASH_MAX_ENTRIES];

    // `search_pos` past every entry, so only the `low_limit` break applies:
    // 100 and 90 clear it, 80 does not and ends the walk.
    let (num_matches, attempts_left) = row_collect_match_indices(
        &row,
        &tag_row,
        location,
        context,
        85,
        200,
        8,
        Some(&mut match_positions),
        &mut match_buffer,
        core::ptr::null(),
    );

    assert_eq!(num_matches, 2);
    assert_eq!(attempts_left, 6);
    assert_eq!(&match_positions[..num_matches], &[7, 9]);
    assert_eq!(&match_buffer[..num_matches], &[100u32, 90]);

    // The upper bound, which is what keeps a stale entry from a previous frame
    // out of the unchecked match-length count. At `search_pos` 95 the entry
    // holding 100 is ahead of the byte being searched and must be skipped
    // rather than ending the walk, so 90 is still found behind it.
    let mut match_positions = [0usize; ROW_HASH_MAX_ENTRIES];
    let mut match_buffer = [NO_POS; ROW_HASH_MAX_ENTRIES];
    let (num_matches, _) = row_collect_match_indices(
        &row,
        &tag_row,
        location,
        context,
        85,
        95,
        8,
        Some(&mut match_positions),
        &mut match_buffer,
        core::ptr::null(),
    );

    assert_eq!(
        num_matches, 1,
        "an entry at or beyond search_pos is not a match"
    );
    assert_eq!(&match_buffer[..num_matches], &[90u32]);
}

#[test]
fn prefixed_block_state_reuses_prior_source_history() {
    let prefix = b"dictionary-only-prefix";
    let block = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ012345";
    let mut src = Vec::new();
    src.extend_from_slice(block);
    src.extend_from_slice(block);

    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::Fast,
        fast_search_step: 2,
        ..MatchFinderParameters::default()
    };
    let mut state = PrefixedBlockMatchState::new(prefix, src.len(), params);
    let first = plan_sequences_for_prefixed_contiguous_block(
        prefix,
        &src[..block.len()],
        0,
        RepeatOffsets::default(),
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    let second = plan_sequences_for_prefixed_contiguous_block(
        prefix,
        &src,
        block.len(),
        first.repeat_offsets,
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    assert!(!second.sequences.is_empty());
    let mut repeat_offsets = first.repeat_offsets;
    let raw_offset = repeat_offsets.resolve(&second.sequences[0]).unwrap();
    assert_eq!(raw_offset, block.len() as u32);
}

#[test]
fn generic_planner_extends_source_matches_back_to_the_anchor() {
    let src = b"x!abcdefghijklmnopx!abcdefghijklmnop";
    let plan = plan_sequences_with_params(
        src,
        RepeatOffsets::default(),
        MatchFinderParameters::default(),
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    assert_eq!(plan.literals, b"x!abcdefghijklmnop");
    assert_eq!(plan.sequences[0].literal_length, 18);
    assert_eq!(plan.sequences[0].match_length, 18);
}

#[test]
fn generic_prefix_planner_extends_dictionary_matches_back_to_the_anchor() {
    let prefix = b"abcXYZ";
    let src = b"abcXYZrest";
    let plan = plan_sequences_with_params_and_prefix(
        src,
        prefix,
        RepeatOffsets::default(),
        MatchFinderParameters::default(),
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    assert_eq!(plan.literals, b"rest");
    assert_eq!(plan.sequences[0].literal_length, 0);
    assert_eq!(plan.sequences[0].match_length, 6);
}

#[test]
fn lazy_family_chains_immediate_rep2_after_storing_a_match() {
    let src = b"abcdWXYZabcdWXYZWXYZWXYZWXYZWXYZ";

    for (strategy, lazy_search_depth) in [
        (ParserStrategy::Greedy, 0),
        (ParserStrategy::Lazy, 1),
        (ParserStrategy::Lazy2, 2),
        (ParserStrategy::BinaryTreeLazy2, 2),
    ] {
        let plan = plan_sequences_for_block(
            src,
            &[&[]],
            RepeatOffsets::from_values([4, 64, 8]),
            MatchFinderParameters {
                parser_strategy: strategy,
                lazy_search_depth,
                ..MatchFinderParameters::default()
            },
        )
        .unwrap();

        assert!(
            plan.sequences.len() >= 2,
            "expected an immediate rep2 chain for {strategy:?}"
        );

        let mut repeat_offsets = RepeatOffsets::from_values([4, 64, 8]);
        let first_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
        let second_offset = repeat_offsets.resolve(&plan.sequences[1]).unwrap();

        assert_eq!(first_offset, 8, "unexpected first offset for {strategy:?}");
        assert_eq!(second_offset, 4, "expected rep2 chain for {strategy:?}");
        assert_eq!(plan.sequences[1].literal_length, 0);
    }
}

#[test]
fn lazy_family_row_contiguous_blocks_chain_immediate_rep2_after_storing_a_match() {
    let src = b"abcdWXYZabcdWXYZWXYZWXYZWXYZWXYZ";

    for (strategy, lazy_search_depth) in [
        (ParserStrategy::GreedyRow, 0),
        (ParserStrategy::LazyRow, 1),
        (ParserStrategy::Lazy2Row, 2),
    ] {
        let params = MatchFinderParameters {
            parser_strategy: strategy,
            lazy_search_depth,
            ..MatchFinderParameters::default()
        };
        let mut state = ContiguousBlockMatchState::new(src.len(), params);
        state.insert_range(src, 0, 8);

        let plan = plan_sequences_for_contiguous_block(
            src,
            8,
            RepeatOffsets::from_values([4, 64, 8]),
            params,
            128 * 1024,
            &mut state,
        )
        .unwrap();

        assert!(
            plan.sequences.len() >= 2,
            "expected an immediate rep2 chain for {strategy:?}"
        );

        let mut repeat_offsets = RepeatOffsets::from_values([4, 64, 8]);
        let first_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
        let second_offset = repeat_offsets.resolve(&plan.sequences[1]).unwrap();

        assert_eq!(first_offset, 8, "unexpected first offset for {strategy:?}");
        assert_eq!(second_offset, 4, "expected rep2 chain for {strategy:?}");
        assert_eq!(plan.sequences[1].literal_length, 0);
    }
}

#[test]
fn lazy_family_prefix_path_chains_immediate_rep2_after_storing_a_match() {
    let prefix = b"abcdWXYZ";
    let src = b"abcdWXYZWXYZWXYZWXYZWXYZ";

    for (strategy, lazy_search_depth) in [
        (ParserStrategy::Greedy, 0),
        (ParserStrategy::Lazy, 1),
        (ParserStrategy::Lazy2, 2),
        (ParserStrategy::GreedyRow, 0),
        (ParserStrategy::LazyRow, 1),
        (ParserStrategy::Lazy2Row, 2),
        (ParserStrategy::BinaryTreeLazy2, 2),
    ] {
        let plan = plan_sequences_for_block(
            src,
            &[prefix],
            RepeatOffsets::from_values([4, 64, 8]),
            MatchFinderParameters {
                parser_strategy: strategy,
                lazy_search_depth,
                ..MatchFinderParameters::default()
            },
        )
        .unwrap();

        assert!(
            plan.sequences.len() >= 2,
            "expected an immediate rep2 chain with prefix history for {strategy:?}"
        );

        let mut repeat_offsets = RepeatOffsets::from_values([4, 64, 8]);
        let first_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
        let second_offset = repeat_offsets.resolve(&plan.sequences[1]).unwrap();

        assert_eq!(first_offset, prefix.len() as u32);
        assert_eq!(second_offset, 4, "expected rep2 chain for {strategy:?}");
        assert_eq!(plan.sequences[0].literal_length, 0);
        assert_eq!(plan.sequences[1].literal_length, 0);
    }
}

#[test]
fn prepared_fast_and_double_fast_dict_tables_preserve_zero_index_matches() {
    let prefix = b"abcdWXYZabcdWXYZ";

    for strategy in [ParserStrategy::Fast, ParserStrategy::DoubleFast] {
        let params = MatchFinderParameters {
            parser_strategy: strategy,
            fast_search_step: 2,
            secondary_hash_bits: 16,
            ..MatchFinderParameters::default()
        };
        let prepared = build_prepared_dictionary_match_state(prefix, params)
            .expect("prepared dictionary state must build");
        match prepared {
            PreparedDictionaryMatchState::Fast(ref prepared) => {
                assert_eq!(prepared.candidate_at(prefix, 0), Some(0));
            }
            PreparedDictionaryMatchState::DoubleFast(ref prepared) => {
                assert_eq!(prepared.long_candidate_at(prefix, 0), Some(0));
                assert_eq!(prepared.short_candidate_at(prefix, 0), Some(0));
            }
            _ => panic!("unexpected prepared dictionary state for {strategy:?}"),
        }
    }
}

#[test]
fn prepared_row_dict_state_uses_dictionary_boundary_offsets() {
    let prefix = b"customer=0012|region=us-east|tier=gold|";
    let src = b"customer=0012|region=us-east|tier=gold|invoice=42";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::GreedyRow,
        search_log: 5,
        min_match: 5,
        ..MatchFinderParameters::default()
    };
    let prepared = build_prepared_dictionary_match_state(prefix, params)
        .expect("prepared row dictionary state must build");
    let mut state = PrefixedBlockMatchState::new_with_prepared_match_state(
        prefix,
        src.len(),
        params,
        PrefixMatchMode::DictMatchState,
        Some(&prepared),
    );
    let plan = plan_sequences_for_prefixed_contiguous_block(
        prefix,
        src,
        0,
        RepeatOffsets::default(),
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, prefix.len() as u32);
}

#[test]
fn prepared_chain_dict_state_uses_dictionary_boundary_offsets() {
    let prefix = b"customer=0012|region=us-east|tier=gold|";
    let src = b"customer=0012|region=us-east|tier=gold|invoice=42";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::Greedy,
        search_log: 5,
        min_match: 5,
        ..MatchFinderParameters::default()
    };
    let prepared = build_prepared_dictionary_match_state(prefix, params)
        .expect("prepared chain dictionary state must build");
    let mut state = PrefixedBlockMatchState::new_with_prepared_match_state(
        prefix,
        src.len(),
        params,
        PrefixMatchMode::DictMatchState,
        Some(&prepared),
    );
    let plan = plan_sequences_for_prefixed_contiguous_block(
        prefix,
        src,
        0,
        RepeatOffsets::default(),
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, prefix.len() as u32);
}

#[test]
fn row_dict_match_state_prefers_longer_dict_match_over_shorter_source_match() {
    let prefix = b"ABCDEFGHIJ";
    let src = b"ABCDE12345ABCDEFGHIJabcdefgh";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::LazyRow,
        search_log: 5,
        min_match: 5,
        ..MatchFinderParameters::default()
    };
    let mut prefix_finder =
        RowHashFinder::new(params.hash_bits, params.search_log, params.min_match);
    prefix_finder.hash_salt = 0;
    prefix_finder.insert_prefix(prefix);
    let mut src_finder = RowHashFinder::new(params.hash_bits, params.search_log, params.min_match);
    src_finder.insert_range(src, 0, 10);
    let mut trace_first_row_contest = None;
    let mut trace_row_searches = Vec::new();

    let (candidate, source) = best_row_dict_match_state_regular_match_core::<true, false>(
        prefix,
        src,
        10,
        params,
        0,
        0,
        &prefix_finder,
        &mut src_finder,
        false,
        &mut trace_first_row_contest,
        &mut trace_row_searches,
        None,
    )
    .unwrap();

    assert_eq!(source, SequenceTraceMatchSource::Dict);
    assert_eq!(candidate.length, prefix.len());
    assert_eq!(candidate.offset, prefix.len() + 10);
    if let Some(contest) = trace_first_row_contest {
        assert_eq!(contest.winner, SequenceTraceMatchSource::Dict);
        assert!(contest.dict_length > contest.source_length);
    }
}

#[test]
fn row_dict_match_state_keeps_source_match_when_source_candidate_is_valid() {
    let prefix = b"mnopabcdWXYZ";
    let src = b"abcdWXYZuvabcdWXYZtailtailtail";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::GreedyRow,
        search_log: 5,
        min_match: 5,
        ..MatchFinderParameters::default()
    };
    let mut state = PrefixedBlockMatchState::new_with_mode(
        prefix,
        src.len(),
        params,
        PrefixMatchMode::DictMatchState,
    );
    state.insert_range(src, 0, 10);

    let plan = plan_sequences_for_prefixed_contiguous_block(
        prefix,
        src,
        10,
        RepeatOffsets::default(),
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, 10);
    assert_eq!(
        plan.trace_match_sources.first().copied(),
        Some(SequenceTraceMatchSource::Source)
    );
    if let Some(contest) = plan.trace_first_row_contest {
        assert_eq!(contest.winner, SequenceTraceMatchSource::Source);
        assert!(contest.source_length >= contest.dict_length);
    }
    assert!(
        !plan
            .trace_match_sources
            .contains(&SequenceTraceMatchSource::Prefix)
    );
}

#[test]
fn row_dict_match_state_tied_lengths_keep_source_match() {
    let prefix = b"ABCDEFGHIJ";
    let src = b"ABCDEFGHIJABCDEFGHIJtailtailtail";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::GreedyRow,
        search_log: 5,
        min_match: 5,
        ..MatchFinderParameters::default()
    };
    let mut state = PrefixedBlockMatchState::new_with_mode(
        prefix,
        src.len(),
        params,
        PrefixMatchMode::DictMatchState,
    );
    state.insert_range(src, 0, 10);

    let plan = plan_sequences_for_prefixed_contiguous_block(
        prefix,
        src,
        10,
        RepeatOffsets::default(),
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, 10);
    assert_eq!(
        plan.trace_match_sources.first().copied(),
        Some(SequenceTraceMatchSource::Source)
    );
    if let Some(contest) = plan.trace_first_row_contest {
        assert_eq!(contest.winner, SequenceTraceMatchSource::Source);
        assert_eq!(contest.source_length, contest.dict_length);
    }
}

/// The dictionary is reachable while the window still covers it, and the window
/// is measured from the *end* of the block being encoded — so whether the parser
/// may look into the prefix at all depends on the whole block, not on where it
/// starts.
///
/// The two halves are that decision, either side of its boundary. With the
/// 36-byte window — the 8 the second half passes, plus the 28-byte block — the
/// prefix is live, and *all* of it is in reach: the match begins at `ABCD`, the
/// first byte of the dictionary, for the full 12 bytes. It is not trimmed to
/// the part of the prefix the window happens to span, because the decoder holds
/// the dictionary outside the window and an offset into it may exceed
/// `Window_Size`. This asserted a four-byte trim until that cost 3.5x on an
/// attached dictionary, by retiring one many times the window's size before it
/// was ever searched.
///
/// Passing 8 retires the prefix instead, and then the parser must decline the
/// match rather than emit an offset the frame will not declare.
#[test]
fn row_dict_match_state_catch_up_respects_prefix_low() {
    let prefix = b"ABCDWXYZQRST";
    let src = b"ABCDWXYZQRSTabcdefghijklmnop";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::GreedyRow,
        search_log: 5,
        min_match: 4,
        ..MatchFinderParameters::default()
    };

    let plan_within_window = {
        let mut state = PrefixedBlockMatchState::new_with_mode(
            prefix,
            src.len(),
            params,
            PrefixMatchMode::DictMatchState,
        );
        plan_sequences_for_prefixed_contiguous_block(
            prefix,
            src,
            0,
            RepeatOffsets::default(),
            params,
            8 + src.len(),
            &mut state,
        )
        .unwrap()
    };

    assert!(!plan_within_window.sequences.is_empty());
    assert_eq!(plan_within_window.sequences[0].literal_length, 0);
    assert_eq!(plan_within_window.sequences[0].match_length, 12);
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets
        .resolve(&plan_within_window.sequences[0])
        .unwrap();
    assert_eq!(raw_offset, 12);
    assert_eq!(
        plan_within_window.trace_match_sources.first().copied(),
        Some(SequenceTraceMatchSource::Dict)
    );

    let plan_past_window = {
        let mut state = PrefixedBlockMatchState::new_with_mode(
            prefix,
            src.len(),
            params,
            PrefixMatchMode::DictMatchState,
        );
        plan_sequences_for_prefixed_contiguous_block(
            prefix,
            src,
            0,
            RepeatOffsets::default(),
            params,
            8,
            &mut state,
        )
        .unwrap()
    };

    assert!(
        plan_past_window.sequences.is_empty(),
        "an 8-byte window cannot reach a match 12 bytes back, but took {:?}",
        plan_past_window.sequences,
    );
}

#[test]
fn prepared_row_dict_state_use_rep1_across_dictionary_boundary() {
    let prefix = b"abcdefghij";
    let src = b"ZabcdefghijTAILTAIL";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::GreedyRow,
        search_log: 5,
        min_match: 5,
        ..MatchFinderParameters::default()
    };
    let prepared = build_prepared_dictionary_match_state(prefix, params)
        .expect("prepared row dictionary state must build");
    let mut state = PrefixedBlockMatchState::new_with_prepared_match_state(
        prefix,
        src.len(),
        params,
        PrefixMatchMode::DictMatchState,
        Some(&prepared),
    );
    let plan = plan_sequences_for_prefixed_contiguous_block(
        prefix,
        src,
        0,
        RepeatOffsets::from_values([prefix.len() as u32 + 1, 4, 8]),
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    assert_eq!(plan.sequences[0].literal_length, 1);
    let mut repeat_offsets = RepeatOffsets::from_values([prefix.len() as u32 + 1, 4, 8]);
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, prefix.len() as u32 + 1);
    assert_eq!(
        plan.trace_match_sources.first().copied(),
        Some(SequenceTraceMatchSource::Rep)
    );
    assert!(
        !plan
            .trace_match_sources
            .contains(&SequenceTraceMatchSource::Prefix)
    );
}

#[test]
fn prepared_row_dict_state_chain_rep2_across_dictionary_boundary() {
    let prefix = b"abcdefghABCDEFGH";
    let src = b"abcdefghABCDEFGHEFGHEFGHTAILTAIL";
    for (strategy, lazy_search_depth) in [
        (ParserStrategy::GreedyRow, 0),
        (ParserStrategy::LazyRow, 1),
        (ParserStrategy::Lazy2Row, 2),
    ] {
        let params = MatchFinderParameters {
            parser_strategy: strategy,
            search_log: 5,
            min_match: 5,
            lazy_search_depth,
            ..MatchFinderParameters::default()
        };
        let prepared = build_prepared_dictionary_match_state(prefix, params)
            .expect("prepared row dictionary state must build");
        let mut state = PrefixedBlockMatchState::new_with_prepared_match_state(
            prefix,
            src.len(),
            params,
            PrefixMatchMode::DictMatchState,
            Some(&prepared),
        );
        let plan = plan_sequences_for_prefixed_contiguous_block(
            prefix,
            src,
            0,
            RepeatOffsets::from_values([4, 64, 8]),
            params,
            128 * 1024,
            &mut state,
        )
        .unwrap();

        assert!(
            plan.sequences.len() >= 2,
            "expected a regular dict match followed by a rep2 chain for {strategy:?}"
        );
        assert_eq!(
            plan.sequences[0].offset_value,
            prefix.len() as u32 + 3,
            "expected the first regular match to stay explicit for {strategy:?}"
        );
        assert_eq!(
            plan.sequences[1].offset_value, 1,
            "expected the immediate rep2 chain to encode as rep1 for {strategy:?}"
        );
        let mut repeat_offsets = RepeatOffsets::from_values([4, 64, 8]);
        let first_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
        let second_offset = repeat_offsets.resolve(&plan.sequences[1]).unwrap();
        assert_eq!(first_offset, prefix.len() as u32, "for {strategy:?}");
        assert_eq!(second_offset, 4, "for {strategy:?}");
        assert_eq!(
            plan.trace_match_sources.first().copied(),
            Some(SequenceTraceMatchSource::Dict),
            "for {strategy:?}"
        );
        assert_eq!(
            plan.trace_match_sources.get(1).copied(),
            Some(SequenceTraceMatchSource::Rep),
            "for {strategy:?}"
        );
        assert!(
            !plan
                .trace_match_sources
                .contains(&SequenceTraceMatchSource::Prefix)
        );
    }
}

#[test]
fn exact_offset_store_keeps_zero_literal_regular_matches_explicit() {
    let src = b"abcdabcd";
    let mut plan = SequencePlan::default();
    let mut anchor = 0usize;
    let mut repeat_offsets = RepeatOffsets::from_values([8, 4, 16]);

    let raw_offset = store_lazy_sequence_with_offset_value_and_source(
        &mut plan,
        src,
        &mut anchor,
        &mut repeat_offsets,
        0,
        explicit_offbase(4),
        4,
        SequenceTraceMatchSource::Dict,
    )
    .unwrap();

    assert_eq!(raw_offset, 4);
    assert_eq!(plan.sequences.len(), 1);
    assert_eq!(plan.sequences[0].literal_length, 0);
    assert_eq!(plan.sequences[0].offset_value, 7);
    assert_eq!(repeat_offsets.values(), [4, 8, 4]);
}

#[test]
fn chain_dict_match_state_prefers_longer_dict_match_over_shorter_source_match() {
    let prefix = b"ABCDEFGHIJ";
    let src = b"ABCDE12345ABCDEFGHIJabcdefgh";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::Lazy,
        search_log: 5,
        min_match: 5,
        ..MatchFinderParameters::default()
    };
    let prefix_refs = [prefix.as_slice()];
    let prefix_chain = PrefixChain::new(&prefix_refs).unwrap().unwrap();
    let mut prefix_finder = MatchFinder::with_chain_log(
        prefix.len(),
        params.hash_bits,
        params.chain_log,
        params.min_match,
    );
    prefix_finder.insert_prefix_chain(prefix_chain, &[]);
    let mut src_finder = MatchFinder::with_chain_log(
        src.len(),
        params.hash_bits,
        params.chain_log,
        params.min_match,
    );
    src_finder.insert_range(src, 0, 10);

    let search = best_chain_dict_match_state_regular_match(
        prefix_chain,
        src,
        10,
        params,
        0,
        0,
        &prefix_finder,
        &mut src_finder,
        false,
        None,
    );
    let candidate = search.candidate.unwrap();
    let source = search.source;

    assert_eq!(source, SequenceTraceMatchSource::Dict);
    assert_eq!(candidate.length, prefix.len());
    assert_eq!(candidate.offset, prefix.len() + 10);
}

#[test]
fn chain_ext_dict_regular_search_uses_unified_prefix_chain_finder() {
    let prefix = b"ABCDEFGHIJ";
    let src = b"ABCDE12345ABCDEFGHIJabcdefgh";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::Lazy,
        search_log: 5,
        min_match: 5,
        ..MatchFinderParameters::default()
    };
    let prefix_refs = [prefix.as_slice()];
    let prefix_chain = PrefixChain::new(&prefix_refs).unwrap().unwrap();
    let mut finder = MatchFinder::with_chain_log(
        prefix.len(),
        params.hash_bits,
        params.chain_log,
        params.min_match,
    );
    finder.insert_prefix_chain(prefix_chain, &[]);

    let search = best_chain_ext_dict_regular_match(
        prefix_chain,
        src,
        10,
        params,
        0,
        0,
        &mut finder,
        false,
        None,
    );
    let candidate = search.candidate.unwrap();

    assert_eq!(search.source, SequenceTraceMatchSource::Prefix);
    assert_eq!(candidate.length, prefix.len());
    assert_eq!(candidate.offset, prefix.len() + 10);
}

#[test]
fn prepared_chain_ext_dict_state_reuses_prepared_prefix_tables() {
    let prefix = vec![b'a'; 16];
    let src = vec![b'a'; 8];
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::Lazy,
        ..MatchFinderParameters::default()
    };
    let prepared = build_prepared_dictionary_match_state(&prefix, params)
        .expect("prepared chain dictionary state should build");
    let state = PrefixedBlockMatchState::new_with_prepared_match_state(
        &prefix,
        src.len(),
        params,
        PrefixMatchMode::ExtDict,
        Some(&prepared),
    );
    let PrefixedBlockMatchStateInner::Chain {
        prefix_finder,
        mut src_finder,
        mode,
        ..
    } = state.inner
    else {
        panic!("expected chain extdict state");
    };

    assert_eq!(mode, PrefixMatchMode::ExtDict);
    assert_eq!(src_finder.next_to_update, prefix_finder.next_to_update);
    assert_eq!(
        src_finder.insert_and_find_first_index_ext_dict(prefix.len(), &src, 0, false),
        Some(7)
    );
}

#[test]
fn chain_extdict_lazy_family_reserves_last_8_bytes_as_literals() {
    let prefix = b"abcdefghi";
    let src = b"abcdefghifghiXYZ";

    for (strategy, lazy_search_depth) in [
        (ParserStrategy::Greedy, 0),
        (ParserStrategy::Lazy, 1),
        (ParserStrategy::Lazy2, 2),
    ] {
        let params = MatchFinderParameters {
            parser_strategy: strategy,
            lazy_search_depth,
            ..MatchFinderParameters::default()
        };
        let mut state = PrefixedBlockMatchState::new_with_mode(
            prefix,
            src.len(),
            params,
            PrefixMatchMode::ExtDict,
        );
        let plan = plan_sequences_for_prefixed_contiguous_block(
            prefix,
            src,
            0,
            RepeatOffsets::default(),
            params,
            128 * 1024,
            &mut state,
        )
        .unwrap();

        assert_eq!(
            plan.sequences.len(),
            1,
            "expected the final 7 bytes to remain literals for {strategy:?}"
        );
        assert_eq!(plan.literals, b"fghiXYZ");
        assert_eq!(plan.sequences[0].literal_length, 0);
        assert_eq!(plan.sequences[0].match_length, prefix.len() as u32);

        let mut repeat_offsets = RepeatOffsets::default();
        let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
        assert_eq!(raw_offset, prefix.len() as u32);
        assert_eq!(
            plan.trace_match_sources.first().copied(),
            Some(SequenceTraceMatchSource::Prefix)
        );
    }
}

#[test]
fn prepared_fast_and_double_fast_dict_state_use_rep1_across_dictionary_boundary() {
    let prefix = b"abcdefghij";
    let src = b"ZabcdefghijTAIL";

    for strategy in [ParserStrategy::Fast, ParserStrategy::DoubleFast] {
        let params = MatchFinderParameters {
            parser_strategy: strategy,
            fast_search_step: 2,
            secondary_hash_bits: 16,
            ..MatchFinderParameters::default()
        };
        let prepared = build_prepared_dictionary_match_state(prefix, params)
            .expect("prepared dictionary state must build");
        let mut state = PrefixedBlockMatchState::new_with_prepared_match_state(
            prefix,
            src.len(),
            params,
            PrefixMatchMode::DictMatchState,
            Some(&prepared),
        );
        let plan = plan_sequences_for_prefixed_contiguous_block(
            prefix,
            src,
            0,
            RepeatOffsets::from_values([prefix.len() as u32 + 1, 4, 8]),
            params,
            128 * 1024,
            &mut state,
        )
        .unwrap();

        assert!(!plan.sequences.is_empty());
        assert_eq!(plan.sequences[0].literal_length, 1);

        let mut repeat_offsets = RepeatOffsets::from_values([prefix.len() as u32 + 1, 4, 8]);
        let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
        assert_eq!(raw_offset, prefix.len() as u32 + 1);
        assert_eq!(
            plan.trace_match_sources.first().copied(),
            Some(SequenceTraceMatchSource::Rep)
        );
        assert!(
            !plan
                .trace_match_sources
                .contains(&SequenceTraceMatchSource::Prefix),
            "dictMatchState should not trace prefix-origin matches for {strategy:?}"
        );
    }
}

#[test]
fn prepared_fast_and_double_fast_dict_state_chain_rep2_across_dictionary_boundary() {
    let prefix = b"abcdWXYZabcdWXYZ";
    let src = b"abcdWXYZabcdWXYZWXYZWXYZ";

    for strategy in [ParserStrategy::Fast, ParserStrategy::DoubleFast] {
        let params = MatchFinderParameters {
            parser_strategy: strategy,
            fast_search_step: 2,
            secondary_hash_bits: 16,
            ..MatchFinderParameters::default()
        };
        let prepared = build_prepared_dictionary_match_state(prefix, params)
            .expect("prepared dictionary state must build");
        let mut state = PrefixedBlockMatchState::new_with_prepared_match_state(
            prefix,
            src.len(),
            params,
            PrefixMatchMode::DictMatchState,
            Some(&prepared),
        );
        let plan = plan_sequences_for_prefixed_contiguous_block(
            prefix,
            src,
            0,
            RepeatOffsets::from_values([4, 64, 8]),
            params,
            128 * 1024,
            &mut state,
        )
        .unwrap();

        assert!(
            plan.sequences.len() >= 2,
            "expected an immediate rep2 chain for {strategy:?}"
        );

        let mut repeat_offsets = RepeatOffsets::from_values([4, 64, 8]);
        let first_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
        let second_offset = repeat_offsets.resolve(&plan.sequences[1]).unwrap();

        assert_eq!(first_offset, prefix.len() as u32);
        assert_eq!(second_offset, 4, "expected rep2 chain for {strategy:?}");
        assert!(
            !plan
                .trace_match_sources
                .contains(&SequenceTraceMatchSource::Prefix),
            "dictMatchState should not trace prefix-origin matches for {strategy:?}"
        );
    }
}

#[test]
fn fast_dict_match_state_keeps_source_match_when_source_candidate_is_valid() {
    let prefix = b"ZZZZabcdWXYZ";
    let src = b"abcdqrstabcdWXYZtail";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::Fast,
        fast_search_step: 2,
        ..MatchFinderParameters::default()
    };
    let mut state = PrefixedBlockMatchState::new_with_mode(
        prefix,
        src.len(),
        params,
        PrefixMatchMode::DictMatchState,
    );
    state.insert_range(src, 0, 8);

    let plan = plan_sequences_for_prefixed_contiguous_block(
        prefix,
        src,
        8,
        RepeatOffsets::default(),
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, 8);
    assert_eq!(
        plan.trace_match_sources.first().copied(),
        Some(SequenceTraceMatchSource::Source)
    );
    assert!(
        !plan
            .trace_match_sources
            .contains(&SequenceTraceMatchSource::Prefix)
    );
}

#[test]
fn double_fast_dict_match_state_keeps_source_long_match_when_source_candidate_is_valid() {
    let prefix = b"mnopabcdWXYZ";
    let src = b"abcdWXYZuvabcdWXYZtail";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::DoubleFast,
        fast_search_step: 2,
        secondary_hash_bits: 16,
        ..MatchFinderParameters::default()
    };
    let mut state = PrefixedBlockMatchState::new_with_mode(
        prefix,
        src.len(),
        params,
        PrefixMatchMode::DictMatchState,
    );
    state.insert_range(src, 0, 10);

    let plan = plan_sequences_for_prefixed_contiguous_block(
        prefix,
        src,
        10,
        RepeatOffsets::default(),
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, 10);
    assert_eq!(
        plan.trace_match_sources.first().copied(),
        Some(SequenceTraceMatchSource::Source)
    );
    assert!(
        !plan
            .trace_match_sources
            .contains(&SequenceTraceMatchSource::Prefix)
    );
}

#[test]
fn double_fast_dict_match_state_prefers_ip_plus_one_dict_long_probe() {
    let prefix = b"abcdefghij";
    let src = b"0abcqrstuv0abcdefghijTAIL";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::DoubleFast,
        fast_search_step: 2,
        secondary_hash_bits: 16,
        ..MatchFinderParameters::default()
    };
    let mut state = PrefixedBlockMatchState::new_with_mode(
        prefix,
        src.len(),
        params,
        PrefixMatchMode::DictMatchState,
    );
    state.insert_range(src, 0, 10);

    let plan = plan_sequences_for_prefixed_contiguous_block(
        prefix,
        src,
        10,
        RepeatOffsets::default(),
        params,
        128 * 1024,
        &mut state,
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    assert_eq!(plan.sequences[0].literal_length, 1);
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert_eq!(raw_offset, prefix.len() as u32 + 11);
    assert_eq!(
        plan.trace_match_sources.first().copied(),
        Some(SequenceTraceMatchSource::Dict)
    );
    assert!(
        !plan
            .trace_match_sources
            .contains(&SequenceTraceMatchSource::Prefix)
    );
}

#[test]
fn double_fast_planner_keeps_finding_repeated_sequences() {
    let src = b"pattern-0123456789-pattern-0123456789-pattern-0123456789";
    let plan = plan_sequences_for_block(
        src,
        &[&[]],
        RepeatOffsets::default(),
        MatchFinderParameters {
            parser_strategy: ParserStrategy::DoubleFast,
            secondary_hash_bits: 16,
            ..MatchFinderParameters::default()
        },
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
}

#[test]
fn fast_planner_keeps_finding_repeated_sequences() {
    let src = b"pattern-0123456789-pattern-0123456789-pattern-0123456789";
    let plan = plan_sequences_for_block(
        src,
        &[&[]],
        RepeatOffsets::default(),
        MatchFinderParameters {
            parser_strategy: ParserStrategy::Fast,
            fast_search_step: 2,
            ..MatchFinderParameters::default()
        },
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
}

#[test]
fn double_fast_prefix_planner_finds_dictionary_matches() {
    let prefix = b"customer=0012|region=us-east|tier=gold|";
    let src = b"customer=0012|region=us-east|tier=gold|invoice=42";
    let plan = plan_sequences_for_block(
        src,
        &[prefix],
        RepeatOffsets::default(),
        MatchFinderParameters {
            parser_strategy: ParserStrategy::DoubleFast,
            secondary_hash_bits: 16,
            ..MatchFinderParameters::default()
        },
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert!(raw_offset >= prefix.len() as u32);
}

#[test]
fn fast_prefix_planner_finds_dictionary_matches() {
    let prefix = b"customer=0012|region=us-east|tier=gold|";
    let src = b"customer=0012|region=us-east|tier=gold|invoice=42";
    let plan = plan_sequences_for_block(
        src,
        &[prefix],
        RepeatOffsets::default(),
        MatchFinderParameters {
            parser_strategy: ParserStrategy::Fast,
            fast_search_step: 2,
            ..MatchFinderParameters::default()
        },
    )
    .unwrap();

    assert!(!plan.sequences.is_empty());
    let mut repeat_offsets = RepeatOffsets::default();
    let raw_offset = repeat_offsets.resolve(&plan.sequences[0]).unwrap();
    assert!(raw_offset >= prefix.len() as u32);
}

#[test]
fn fast_prefix_matcher_rejects_tail_positions_shorter_than_its_threshold() {
    let prefix = b"prefix-wxyz";
    let src = b"tailwxyz";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::Fast,
        min_match_length_after_literals: 6,
        ..MatchFinderParameters::default()
    };
    let mut prefix_finder = FastFinder::new(params.hash_bits, params.min_match);
    prefix_finder.insert_prefix(prefix);
    let mut src_finder = FastFinder::new(params.hash_bits, params.min_match);

    let found = fast_best_match_with_prefix(
        prefix,
        src,
        4,
        0,
        &prefix_finder,
        &mut src_finder,
        params,
        0,
        0,
    );

    assert!(found.is_none());
}

#[test]
fn lazy_match_scoring_accounts_for_skipped_literals() {
    assert!(
        should_take_lazy_match_with_skip(
            MatchCandidate {
                offset: 64,
                length: 8,
            },
            0,
            MatchCandidate {
                offset: 8,
                length: 10,
            },
            1,
            RepeatOffsets::default().values(),
            0,
        ),
        "a one-byte skip should win when it gains enough match length"
    );
    assert!(
        !should_take_lazy_match_with_skip(
            MatchCandidate {
                offset: 8,
                length: 10,
            },
            0,
            MatchCandidate {
                offset: 4,
                length: 11,
            },
            2,
            RepeatOffsets::default().values(),
            0,
        ),
        "a two-byte skip should not win when it only buys one extra byte"
    );
}

#[test]
fn source_penalty_can_keep_a_better_prefix_match() {
    let repeat_offsets = RepeatOffsets::default().values();
    let mut chosen = None;
    for prefix_length in 8..24 {
        for source_length in 4..24 {
            let prefix = MatchCandidate {
                offset: 96,
                length: prefix_length,
            };
            let source = MatchCandidate {
                offset: 4,
                length: source_length,
            };
            let prefix_score = estimated_match_score_bits(prefix, repeat_offsets, 1);
            let source_score = estimated_match_score_bits(source, repeat_offsets, 1);
            if source_score > prefix_score && source_score - 16 < prefix_score {
                chosen = Some((prefix, source));
                break;
            }
        }
        if chosen.is_some() {
            break;
        }
    }

    let (prefix, source) = chosen.expect("expected a score pair that the source penalty flips");
    assert_eq!(
        choose_better_match_with_adjustment(Some(prefix), Some(source), repeat_offsets, 1, -16,),
        Some(prefix)
    );
}

#[test]
fn lazy_matching_checks_repeat_offset_one_ahead_before_stopping() {
    let src = vec![b'a'; 80];
    let params = MatchFinderParameters {
        lazy_search_depth: 2,
        good_enough_match_length: 32,
        ..MatchFinderParameters::default()
    };
    let mut finder = MatchFinder::new(src.len(), params.hash_bits, params.min_match);

    let decision = find_lazy_match_skip_without_prefix(
        &src,
        0,
        0,
        RepeatOffsets::default().values(),
        MatchCandidate {
            offset: 64,
            length: 32,
        },
        params,
        &mut finder,
        MatchFloor::fixed(0),
    );

    assert_eq!(decision.skip, 1);
    assert_eq!(decision.inserted, 1);
}

#[test]
fn prefix_lazy_matching_checks_repeat_offset_one_ahead_before_stopping() {
    let prefix = vec![b'a'; 64];
    let src = vec![b'a'; 80];
    let params = MatchFinderParameters {
        lazy_search_depth: 2,
        good_enough_match_length: 32,
        ..MatchFinderParameters::default()
    };
    let prefix_refs = [prefix.as_slice()];
    let prefix_chain = PrefixChain::new(&prefix_refs).unwrap().unwrap();
    let prefix_finder = MatchFinder::new(prefix_chain.len(), params.hash_bits, params.min_match);
    let mut src_finder = MatchFinder::new(src.len(), params.hash_bits, params.min_match);

    let decision = find_lazy_match_skip_with_prefix_chain(
        prefix_chain,
        &src,
        0,
        0,
        RepeatOffsets::default().values(),
        MatchCandidate {
            offset: 64,
            length: 32,
        },
        params,
        PrefixedMatchFloor::fixed(0, 0),
        PrefixMatchMode::ExtDict,
        &prefix_finder,
        &mut src_finder,
    );

    assert_eq!(decision.skip, 1);
    assert_eq!(decision.inserted, 1);
}

#[test]
fn prefix_match_search_keeps_looking_past_good_enough_candidates() {
    let src = {
        let mut src = b"ABCD".to_vec();
        src.extend(vec![b'x'; 60]);
        src
    };
    let mut prefix = src.clone();
    prefix.push(b'!');
    prefix.extend_from_slice(&src[..48]);
    prefix.push(b'?');
    prefix.extend_from_slice(&src[..32]);

    let params = MatchFinderParameters {
        dictionary_search_depth: 8,
        good_enough_match_length: 32,
        source_score_penalty_with_prefix: 32,
        ..MatchFinderParameters::default()
    };
    let prefix_refs = [prefix.as_slice()];
    let prefix_chain = PrefixChain::new(&prefix_refs).unwrap().unwrap();
    let mut prefix_finder =
        MatchFinder::new(prefix_chain.len(), params.hash_bits, params.min_match);
    let slot = hash_at(&src, 0, params.hash_bits);
    prefix_finder.heads[slot] = (src.len() + 50) as u32;
    prefix_finder.previous[src.len() + 50] = (src.len() + 1) as u32;
    prefix_finder.previous[src.len() + 1] = 0u32;

    let candidate = prefix_finder
        .find_prefix_chain_match(prefix_chain, &src, 0, params, 0, 0)
        .expect("expected a dictionary candidate");

    assert_eq!(candidate.length, src.len());
}

#[test]
fn incompressible_skip_matches_upstream_shape() {
    let params = MatchFinderParameters::default();

    assert_eq!(skip_after_no_match(0, 0, params), 1);
    assert_eq!(skip_after_no_match(0, 255, params), 1);
    assert_eq!(skip_after_no_match(0, 256, params), 2);
    assert_eq!(skip_after_no_match(0, 1024, params), 5);
}

#[test]
fn chain_match_finder_lazy_skipping_only_inserts_one_position_per_search() {
    let src = vec![b'a'; 32];

    let mut eager = MatchFinder::with_chain_log(src.len(), 10, 10, 4);
    assert_eq!(eager.insert_and_find_first_index(&src, 8, false), Some(7));
    assert_eq!(eager.next_to_update, 8);

    let mut lazy = MatchFinder::with_chain_log(src.len(), 10, 10, 4);
    assert_eq!(lazy.insert_and_find_first_index(&src, 8, true), Some(0));
    assert_eq!(lazy.next_to_update, 8);
    assert_eq!(lazy.insert_and_find_first_index(&src, 9, true), Some(8));
    assert_eq!(lazy.next_to_update, 9);
}

#[test]
fn chain_extdict_match_finder_lazy_skipping_only_inserts_one_position_per_search() {
    let prefix = vec![b'a'; 32];
    let src = vec![b'a'; 32];
    let prefix_refs = [prefix.as_slice()];
    let prefix_chain = PrefixChain::new(&prefix_refs).unwrap().unwrap();

    let mut eager = MatchFinder::with_chain_log(prefix.len(), 10, 10, 4);
    eager.insert_prefix_chain(prefix_chain, &[]);
    assert!(
        eager
            .insert_and_find_first_index_ext_dict(prefix.len(), &src, 8, false)
            .is_some()
    );
    assert_eq!(eager.next_to_update, prefix.len() + 8);

    let mut lazy = MatchFinder::with_chain_log(prefix.len(), 10, 10, 4);
    lazy.insert_prefix_chain(prefix_chain, &[]);
    assert!(
        lazy.insert_and_find_first_index_ext_dict(prefix.len(), &src, 8, true)
            .is_some()
    );
    assert_eq!(lazy.next_to_update, prefix.len() + 8);
    assert!(
        lazy.insert_and_find_first_index_ext_dict(prefix.len(), &src, 9, true)
            .is_some()
    );
    assert_eq!(lazy.next_to_update, prefix.len() + 9);
}

#[test]
fn row_hash_finder_lazy_skipping_only_inserts_current_position_on_search() {
    let src = vec![b'a'; 32];
    let limit = row_search_limit(src.len());

    let mut eager = RowHashFinder::new(10, 6, 4);
    eager.refill_hash_cache::<4>(&src, limit);
    let eager_attempts = eager.search_attempt_budget(6);
    let (eager_candidate, _, _) =
        eager.find_source_match_with_budget(&src, 8, 0, false, eager_attempts, false, None);
    assert!(eager_candidate.is_some());
    assert_eq!(eager.next_to_update, 9);

    let mut lazy = RowHashFinder::new(10, 6, 4);
    lazy.refill_hash_cache::<4>(&src, limit);
    let lazy_attempts = lazy.search_attempt_budget(6);
    let (first_candidate, _, _) =
        lazy.find_source_match_with_budget(&src, 8, 0, true, lazy_attempts, false, None);
    assert!(first_candidate.is_none());
    assert_eq!(lazy.next_to_update, 9);

    let (second_candidate, _, _) =
        lazy.find_source_match_with_budget(&src, 9, 0, true, lazy_attempts, false, None);
    assert_eq!(
        second_candidate,
        Some(MatchCandidate {
            offset: 1,
            length: 23
        })
    );
    assert_eq!(lazy.next_to_update, 10);
}

#[test]
fn row_hash_finder_large_gap_update_matches_upstream_head_and_tail_ranges() {
    let mut src = Vec::with_capacity(640);
    while src.len() < 640 {
        src.extend_from_slice(b"abcde12345vwxyz67890ABCDE54321");
    }
    src.truncate(640);

    let mut actual = RowHashFinder::new(16, 6, 4);
    let context = actual.row_context();
    let limit = row_search_limit(src.len());
    let target = 480;
    assert!(target < limit);

    actual.refill_hash_cache::<4>(&src, limit);
    actual.update_internal_with_context::<4>(&src, target, true, context);

    let mut expected = RowHashFinder::new(16, 6, 4);
    expected.insert_range(&src, 0, 96);
    expected.insert_range(&src, target - 32, target);

    assert_eq!(actual.next_to_update, target);
    assert_eq!(actual.hash_table, expected.hash_table);
    assert_eq!(actual.tag_table, expected.tag_table);
}

#[test]
fn row_hash_finder_untraced_search_matches_traced_result_and_state() {
    let src = b"abcde12345abcde12345abcde12345abcde12345uvwxyz";
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::LazyRow,
        search_log: 5,
        min_match: 5,
        ..MatchFinderParameters::default()
    };
    let pos = 20;
    let limit = row_search_limit(src.len());
    assert!(pos < limit);

    let mut traced = RowHashFinder::new(params.hash_bits, params.search_log, params.min_match);
    traced.insert_range(src, 0, pos);
    traced.refill_hash_cache::<5>(src, limit);
    let mut untraced = traced.clone();
    let attempts = traced.search_attempt_budget(params.search_log);

    let (traced_candidate, traced_attempts_left, traced_trace) =
        traced.find_source_match_with_budget(src, pos, 0, false, attempts, true, None);
    let (untraced_candidate, untraced_attempts_left, untraced_trace) =
        untraced.find_source_match_with_budget(src, pos, 0, false, attempts, false, None);

    assert_eq!(untraced_candidate, traced_candidate);
    assert_eq!(untraced_attempts_left, traced_attempts_left);
    assert_eq!(untraced_trace, RowMatchBufferTrace::default());
    assert!(traced_trace.num_matches > 0);
    assert_eq!(untraced.next_to_update, traced.next_to_update);
    assert_eq!(untraced.hash_table, traced.hash_table);
    assert_eq!(untraced.tag_table, traced.tag_table);
}

#[test]
fn chain_match_finder_hashes_with_min_match_length() {
    let src = b"ABCDx123ABCDy123ABCDx123".to_vec();

    let mut min4 = MatchFinder::with_chain_log(src.len(), 20, 10, 4);
    min4.insert_range(&src, 0, 8);
    assert_eq!(min4.insert_and_find_first_index(&src, 8, false), Some(0));

    let mut min5 = MatchFinder::with_chain_log(src.len(), 20, 10, 5);
    min5.insert_range(&src, 0, 8);
    assert_eq!(min5.insert_and_find_first_index(&src, 8, false), None);
}

#[test]
fn prepared_chain_dictionary_tables_stop_at_hash_read_tail() {
    let prefix = vec![b'a'; 16];
    let src = vec![b'a'; 8];
    let prefix_refs = [prefix.as_slice()];
    let prefix_chain = PrefixChain::new(&prefix_refs).unwrap().unwrap();

    let mut generic = MatchFinder::with_chain_log(prefix.len(), 10, 10, 4);
    generic.insert_prefix_chain(prefix_chain, &[]);

    let mut cdict = MatchFinder::with_chain_log(prefix.len(), 10, 10, 4);
    cdict.insert_prefix_chain_for_cdict(prefix_chain);

    assert_eq!(generic.lookup_prefix_chain(prefix_chain, &src, 0), Some(12));
    assert_eq!(cdict.lookup_prefix_chain(prefix_chain, &src, 0), Some(7));
    assert_eq!(cdict.next_to_update, prefix.len());
}

#[test]
fn match_finders_preserve_configured_hash_bits_up_to_twenty() {
    let fast = FastFinder::new(20, MIN_MATCH as u32);
    assert_eq!(fast.hash_bits, 20);
    assert_eq!(fast.heads.len(), 1usize << 20);

    let double_fast = DoubleFastFinder::new(20, 19, MIN_MATCH as u32);
    assert_eq!(double_fast.long_hash_bits, 20);
    assert_eq!(double_fast.short_hash_bits, 19);
    assert_eq!(double_fast.long_entries.len(), 1usize << 20);
    assert_eq!(double_fast.short_heads.len(), 1usize << 19);

    let chain = MatchFinder::new(128 * 1024, 19, 4);
    assert_eq!(chain.hash_bits, 19);
    assert_eq!(chain.heads.len(), 1usize << 19);

    let binary_tree = BinaryTreeFinder::new(20, 16, 4);
    assert_eq!(binary_tree.hash_bits, 20);
    assert_eq!(binary_tree.heads.len(), 1usize << 20);
}

/// The bound on which positions may be hashed, at the sizes where the obvious
/// arithmetic goes wrong.
///
/// A buffer shorter than the key has no hashable position at all. Writing this
/// as `len.saturating_sub(MIN_MATCH) + 1` floors the subtraction at zero and
/// then admits position 0, whose key runs past the end of the buffer — a read
/// off the end of the caller's slice, reachable from `encode_all_with_dict`
/// with a one-byte body.
#[test]
fn only_positions_with_a_whole_hash_key_are_insertable() {
    for len in 0..MIN_MATCH {
        assert_eq!(hash_insert_end(len), 0, "a {len}-byte buffer has no key");
    }
    assert_eq!(hash_insert_end(MIN_MATCH), 1);
    for len in MIN_MATCH..64 {
        assert_eq!(
            hash_insert_end(len),
            len - MIN_MATCH + 1,
            "last insertable position in {len} bytes is {}",
            len - MIN_MATCH,
        );
    }
}

/// The fixed-width literal copy reads 16 bytes whatever the run length, so it
/// needs room at *both* ends. The destination has headroom by construction; the
/// source is the caller's buffer and has none, and checking only the
/// destination read up to 16 bytes past the end of it.
#[test]
fn the_fixed_width_literal_copy_needs_room_in_the_source_too() {
    let src = vec![0u8; 64];
    let roomy_destination = 1 << 20;

    // A short run far from the end: fine.
    assert!(wildcopy_literals_fits(&src, 8, 8, 8, roomy_destination));

    // The same run ending within the overshoot of the buffer's end: not fine,
    // however much room the destination has.
    for literals_end in (src.len() - WILDCOPY_OVERLENGTH + 1)..=src.len() {
        assert!(
            !wildcopy_literals_fits(&src, literals_end, 4, 4, roomy_destination),
            "a run ending at {literals_end} of {} is inside the overshoot",
            src.len(),
        );
    }

    // A destination without the headroom still refuses, as it always did.
    assert!(!wildcopy_literals_fits(&src, 8, 8, 8, 8));

    // Runs longer than the fixed width take the exact-length path.
    assert!(!wildcopy_literals_fits(&src, 17, 17, 17, roomy_destination));
}

/// The traced and untraced lazy planners are two copies of one loop, and
/// nothing but this keeps them in step.
///
/// They had already drifted: the untraced copy carried a depth-probe shortcut
/// the traced copy did not, so from level 13 to 15 the block trace described a
/// parse the encoder never produced — a diagnostic that lies about the thing it
/// exists to explain. The shortcut only fired once the first match reached
/// `good_enough_match_length`, so the body has to be long enough and repetitive
/// enough to produce matches past that. Its periodicity is deliberately
/// imperfect: a body that repeats verbatim is matched in one stride and never
/// reaches the depth probes at all, which would make this test pass without
/// exercising anything.
#[test]
fn traced_and_untraced_lazy_planners_agree_on_the_same_block() {
    // A synthetic record stream with real per-record variation: shared long
    // structure so the finder sees matches past 64 bytes, but enough churn that
    // the match one position later is often the better one. A body that repeats
    // near-verbatim is matched in a single stride, the depth probes never change
    // the answer, and this test passes without testing anything.
    const HOSTS: [&str; 6] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];
    const STATUSES: [&str; 4] = ["ok", "warn", "error", "timeout"];
    let mut src = Vec::with_capacity(48 * 1024);
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut id = 1000u64;
    while src.len() < 48 * 1024 {
        let value = next();
        id += value % 7;
        src.extend_from_slice(
            format!(
                "{{\"id\":{id},\"host\":\"{}-node-{:02}\",\"status\":\"{}\",\"latency_ms\":{},\"region\":\"us-{}-{}\"}}\n",
                HOSTS[(value >> 3) as usize % HOSTS.len()],
                (value >> 11) % 32,
                STATUSES[(value >> 17) as usize % STATUSES.len()],
                (value >> 23) % 500,
                if value & 1 == 0 { "east" } else { "west" },
                (value >> 29) % 4,
            )
            .as_bytes(),
        );
    }

    // Every lazy-family strategy, because each selects a different pair of loop
    // copies and nothing couples one pair to another. Covering only the
    // binary-tree family, as this test first did, left the row family's fast
    // copy unguarded, and it had drifted too: its depth-2 probe used the
    // depth-1 acceptance bias. `Lazy2Row` is what levels 8 through 12 run.
    const STRATEGIES: [(ParserStrategy, usize); 7] = [
        (ParserStrategy::Greedy, 0),
        (ParserStrategy::Lazy, 1),
        (ParserStrategy::Lazy2, 2),
        (ParserStrategy::GreedyRow, 0),
        (ParserStrategy::LazyRow, 1),
        (ParserStrategy::Lazy2Row, 2),
        (ParserStrategy::BinaryTreeLazy2, 2),
    ];

    for (parser_strategy, lazy_search_depth) in STRATEGIES {
        let params = MatchFinderParameters {
            parser_strategy,
            hash_bits: 17,
            chain_log: 17,
            search_log: 4,
            min_match: 5,
            search_depth: 16,
            lazy_search_depth,
            // Deliberately low: the shortcut that made these two loops disagree
            // only fires once the depth-0 match clears this, so a small value
            // exercises the divergence on a body a unit test can afford.
            good_enough_match_length: 8,
            window_log: 20,
            ..MatchFinderParameters::default()
        };

        // Both plans are built through the same entry point; the only difference
        // is which copy of the loop the dispatcher picks. Tracing has to be
        // turned off explicitly because it defaults to on under `cfg(test)`,
        // which is the reason this drift survived: every unit test was
        // exercising the traced copy and none was exercising the one that ships.
        let mut untraced = SequencePlan::default();
        untraced.disable_tracing();
        plan_sequences_for_block_into(&mut untraced, &src, &[], RepeatOffsets::default(), params)
            .unwrap();
        assert!(!untraced.tracing_enabled());

        let mut traced = SequencePlan::default();
        traced.enable_tracing();
        plan_sequences_for_block_into(&mut traced, &src, &[], RepeatOffsets::default(), params)
            .unwrap();
        assert!(traced.tracing_enabled());

        assert_eq!(
            traced.sequences, untraced.sequences,
            "{parser_strategy:?}: traced and untraced planners disagree on sequences",
        );
        assert_eq!(
            traced.literals, untraced.literals,
            "{parser_strategy:?}: traced and untraced planners disagree on literals",
        );
        assert_eq!(
            traced.repeat_offsets.values(),
            untraced.repeat_offsets.values(),
            "{parser_strategy:?}: traced and untraced planners disagree on repeat offsets",
        );
    }
}

/// A match older than the children roll buffer is still compared.
///
/// `ZSTD_insertBtAndGetAllMatches` bounds its descent by `windowLow` and uses
/// `btLow` — the low end of the `1 << (chainLog - 1)` children roll buffer —
/// only for a break *inside* the loop, taken after the candidate it lands on
/// has been compared and recorded. Folding `btLow` into the loop-entry floor
/// instead skips that candidate, so a hash head pointing further back than the
/// roll buffer reaches yields no match at all rather than the one match C
/// still gets from it.
///
/// This is invisible on small bodies, because `btLow` stays at zero until the
/// body outgrows the buffer, and every level except 16 gives the binary tree a
/// buffer at least as large as the benchmark corpus. Rather than encode the
/// megabytes that would take at a real `chainLog`, this shrinks the buffer:
/// `chain_log` 6 leaves 32 children slots, so a match 200 bytes back is well
/// outside it.
#[test]
fn binary_tree_compares_a_match_older_than_the_children_roll_buffer() {
    const CHAIN_LOG: u32 = 6;
    // The leading four bytes are the hash key, and they must not recur inside
    // the run: a second occurrence would take over the hash head and give the
    // search a candidate inside the roll buffer, which is the case that
    // already worked.
    const REPEAT: &[u8] = b"Zq7-mixed run of bytes with no inner echo 9876543210";
    const GAP: usize = 200;

    // Filler that never repeats, so the only match available at the second
    // copy is the first copy. Periodic filler would let the search succeed
    // through a near match and prove nothing.
    let mut src = Vec::from(REPEAT);
    let mut state = 0x9e37_79b9u32;
    while src.len() < REPEAT.len() + GAP {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        src.extend_from_slice(&state.to_le_bytes());
    }
    let second_copy = src.len();
    src.extend_from_slice(REPEAT);

    let mut finder = BinaryTreeFinder::new(16, CHAIN_LOG, 4);
    assert!(
        second_copy > finder.bt_mask,
        "the match at offset {second_copy} has to sit outside the {}-slot roll buffer \
         or the bug cannot show",
        finder.bt_mask
    );

    let mut matches = Vec::new();
    for pos in 0..=second_copy {
        finder.insert_and_collect_matches(&mut matches, &src, pos, 32, 0, MIN_MATCH);
    }

    let found = matches
        .iter()
        .copied()
        .find(|candidate| candidate.offset == second_copy)
        .unwrap_or_else(|| panic!("no candidate at offset {second_copy}; collected {matches:?}"));
    assert_eq!(
        found.length,
        REPEAT.len(),
        "the whole repeated run should match, not a prefix of it"
    );
}

/// C's clause in `ZSTD_buildSeqStore`:
///
/// ```c
/// if (curr > ms->nextToUpdate + 384)
///     ms->nextToUpdate = curr - MIN(192, (U32)(curr - ms->nextToUpdate - 384));
/// ```
///
/// Both sides of the threshold matter. Below it the tree keeps every position
/// it was going to insert; above it the catch-up must be bounded, because the
/// cost of a bridged position is not constant — it is the length of the match
/// starting there, which on periodic input runs to the end of the block.
#[test]
fn limited_update_bounds_the_catch_up_only_once_the_lag_is_long() {
    // Within the threshold: nothing moves, whatever the gap.
    assert_eq!(limited_update_after_long_match(1_000, 1_000), 1_000);
    assert_eq!(limited_update_after_long_match(1_000, 1_384), 1_000);

    // One past it: the catch-up shortens by exactly the overshoot, so the
    // transition is continuous rather than a jump to the full 192.
    assert_eq!(limited_update_after_long_match(1_000, 1_385), 1_384);
    assert_eq!(limited_update_after_long_match(1_000, 1_500), 1_384);

    // Far past it: bridge 192 positions and abandon the rest.
    assert_eq!(limited_update_after_long_match(1_000, 1_576), 1_384);
    assert_eq!(limited_update_after_long_match(1_000, 200_000), 199_808);

    // A tree already ahead of the block is left alone; the parser skips
    // forward to meet it rather than re-inserting what it already covered.
    assert_eq!(limited_update_after_long_match(200_000, 1_000), 200_000);
}

/// A buffer with imperfect periodicity, so hash slots are contended and the
/// tables under test hold a mix of live entries, overwritten entries, and slots
/// that were never filled. A verbatim repeat would leave most slots empty and
/// let a broken rebase pass.
fn shift_fixture(len: usize) -> Vec<u8> {
    let mut src = Vec::with_capacity(len);
    let mut counter = 0u32;
    while src.len() < len {
        src.extend_from_slice(b"id=");
        src.extend_from_slice(counter.to_string().as_bytes());
        src.extend_from_slice(b",name=widget,qty=");
        src.extend_from_slice((counter % 97).to_string().as_bytes());
        src.push(b'\n');
        counter += 1;
    }
    src.truncate(len);
    src
}

/// Rebasing a densely filled table by `delta` must leave exactly the table a
/// dense fill of the retained bytes alone would produce.
///
/// The two agree because a dense fill leaves each slot holding the last
/// position that hashed there: if that position survives the shift both tables
/// name it, and if it does not, neither table has anything else to put in the
/// slot. That equality is what makes this a real check on the shift rather than
/// a restatement of it -- it catches a lost tag, an off-by-one in the position,
/// and an entry that should have been emptied but was not.
#[test]
fn shifting_the_fast_table_matches_a_fill_of_the_retained_bytes() {
    let src = shift_fixture(64 * 1024);
    for delta in [1usize, 7, 4096, 32 * 1024, 63 * 1024] {
        let mut shifted = FastFinder::new(14, 4);
        shifted.insert_range(&src, 0, src.len());
        shifted.shift_positions(delta);

        let mut rebuilt = FastFinder::new(14, 4);
        rebuilt.insert_range(&src[delta..], 0, src.len() - delta);

        assert_eq!(shifted.heads, rebuilt.heads, "delta={delta}");
    }
}

#[test]
fn shifting_the_double_fast_tables_matches_a_fill_of_the_retained_bytes() {
    let src = shift_fixture(64 * 1024);
    for delta in [1usize, 7, 4096, 32 * 1024, 63 * 1024] {
        let mut shifted = DoubleFastFinder::new(14, 13, 4);
        shifted.insert_range(&src, 0, src.len());
        shifted.shift_positions(delta);

        let mut rebuilt = DoubleFastFinder::new(14, 13, 4);
        rebuilt.insert_range(&src[delta..], 0, src.len() - delta);

        assert_eq!(shifted.long_entries, rebuilt.long_entries, "delta={delta}");
        assert_eq!(shifted.short_heads, rebuilt.short_heads, "delta={delta}");
    }
}

/// The long table files raw source positions, and the one-shot encoder hands
/// the parser the whole input, so those positions run to the length of the
/// frame. Its sibling short table packs a position into 24 bits beside an
/// 8-bit tag; if the long table's entry ever narrows to a `u32` and borrows
/// that packing, every position past 16 MiB wraps to an index far below the
/// window, `check_long_match_branchless` rejects it, and the long match arm
/// silently stops finding anything. That is a compression-ratio collapse with
/// no panic and no test failure anywhere else: the upstream parity sweep tops
/// out at 4 MiB inputs, so nothing in the suite reaches this size. It is
/// asserted here, at the one place it is cheap.
#[test]
fn the_long_table_files_source_positions_past_sixteen_mib() {
    const PAST_CEILING: usize = (1 << 24) + 4096;
    let mut src = vec![0u8; PAST_CEILING + 8];
    src[PAST_CEILING..PAST_CEILING + 8].copy_from_slice(b"long tag");

    let mut finder = DoubleFastFinder::new(12, 12, 4);
    finder.insert_src_long_position(&src, PAST_CEILING);

    let hash = hash_long_at(&src, PAST_CEILING, finder.long_hash_bits);
    let entry = finder.long_entries[tagged_index(hash)];
    assert_eq!(
        long_entry_pos(entry),
        PAST_CEILING as u32,
        "the filed position must survive at full width"
    );
    assert!(
        long_entry_tag_matches(entry, hash),
        "and the tag must sit below it without taking any of its range"
    );
}

/// The row-hash table cannot be compared against a rebuild the way the two
/// above can: each row keeps an insert cursor, so a fill of the retained bytes
/// lands surviving positions at different offsets within the row. What is
/// asserted instead is the property the caller depends on -- every entry still
/// names the byte it named before, or names nothing.
#[test]
fn shifting_the_row_table_rebases_every_live_entry() {
    let src = shift_fixture(64 * 1024);
    for delta in [1usize, 7, 4096, 32 * 1024] {
        let mut finder = RowHashFinder::new(14, 4, 4);
        finder.insert_range(&src, 0, src.len());
        let before = finder.hash_table.clone();
        let next_to_update_before = finder.next_to_update;
        finder.shift_positions(delta);

        assert_eq!(
            finder.next_to_update,
            next_to_update_before - delta,
            "delta={delta}"
        );
        assert!(next_to_update_before > delta, "fixture must span the shift");

        let mut live = 0usize;
        for (slot, (&old, &new)) in before.iter().zip(finder.hash_table.iter()).enumerate() {
            if (old as usize) < delta {
                assert_eq!(new, 0, "slot {slot} should have emptied, delta={delta}");
            } else {
                assert_eq!(
                    new as usize,
                    old as usize - delta,
                    "slot {slot}, delta={delta}"
                );
                live += 1;
            }
        }
        // Without this the assertions above are satisfied by a table that
        // emptied everything, which is the rebuild this replaces.
        assert!(
            live > before.len() / 4,
            "only {live} of {} entries survived a {delta}-byte shift",
            before.len()
        );
    }
}

/// [`MatchFloor::at`] is C's `ZSTD_getLowestMatchIndex`: never below the
/// block-constant base, never further than one window behind the position doing
/// the looking.
#[test]
fn a_reaching_floor_rises_with_the_position() {
    let floor = MatchFloor::reaching(1_000, 4_096);

    // Below `base + reach` the base wins, which is C's `lowestValid` arm.
    assert_eq!(floor.at(0), 1_000);
    assert_eq!(floor.at(5_000), 1_000);
    // At exactly `base + reach` the two agree, so the boundary cannot be a
    // discontinuity in either direction.
    assert_eq!(floor.at(5_096), 1_000);
    // Above it the position wins, which is C's `curr - maxDistance` arm.
    assert_eq!(floor.at(5_097), 1_001);
    assert_eq!(floor.at(10_000), 5_904);
    // The floor never passes the position that reads it, so no caller can
    // underflow computing `pos - floor.at(pos)`.
    for pos in [0usize, 1, 999, 1_000, 5_096, 5_097, 100_000] {
        assert!(floor.at(pos) <= pos.max(1_000));
    }
}

/// A fixed floor is C's `isDictionary` arm: the same value at every position.
#[test]
fn a_fixed_floor_does_not_move() {
    let floor = MatchFloor::fixed(700);
    for pos in [0usize, 1, 700, 701, usize::MAX] {
        assert_eq!(floor.at(pos), 700, "fixed floor moved at {pos}");
    }
    // `reach` is saturating rather than a special case, so the widest possible
    // position still lands on the base.
    assert_eq!(MatchFloor::fixed(0).at(usize::MAX), 0);
}

/// The prefixed floor lives in virtual coordinates and splits back into the
/// pair the prefixed parsers take. The split is where an off-by-one would hide,
/// so this pins the position each side of `prefix_len`.
#[test]
fn a_prefixed_floor_splits_at_the_prefix_boundary() {
    const PREFIX_LEN: usize = 1_125;
    let floor = PrefixedMatchFloor::reaching(PREFIX_LEN, MatchFloor::reaching(PREFIX_LEN, 2_048));

    // The floor sits exactly on the boundary: the prefix is wholly out of
    // reach and nothing of the source is yet.
    assert_eq!(floor.at(0), (PREFIX_LEN, 0));

    // One byte of source past the point where the reach clears the prefix, the
    // source floor starts moving and the prefix stays out of reach. Virtual
    // position is `PREFIX_LEN + pos`, so this is the first `pos` for which
    // `PREFIX_LEN + pos - 2048 > PREFIX_LEN`.
    assert_eq!(floor.at(2_048), (PREFIX_LEN, 0));
    assert_eq!(floor.at(2_049), (PREFIX_LEN, 1));
    assert_eq!(floor.at(4_105), (PREFIX_LEN, 2_057));

    // A base low enough to leave part of the prefix reachable puts the floor
    // inside the prefix and the source floor at zero, never a negative split.
    let live = PrefixedMatchFloor::reaching(PREFIX_LEN, MatchFloor::reaching(0, 2_048));
    assert_eq!(live.at(0), (0, 0));
    assert_eq!(live.at(900), (0, 0));
    // Virtual 1125 + 924 - 2048 = 1, still inside the prefix.
    assert_eq!(live.at(924), (1, 0));
    // Virtual 1125 + 2048 - 2048 = 1125, exactly the boundary: the whole prefix
    // is out of reach and the source floor is zero, not one.
    assert_eq!(live.at(2_048), (PREFIX_LEN, 0));

    // A fixed prefixed floor is the same pair everywhere.
    let fixed = PrefixedMatchFloor::fixed(400, 12);
    for pos in [0usize, 1, 4_105, usize::MAX] {
        assert_eq!(
            fixed.at(pos),
            (400, 12),
            "fixed prefixed floor moved at {pos}"
        );
    }
}

/// A retired block's floor *is* the contiguous block's floor, once the prefix
/// coordinate is taken back off it.
///
/// This is the fact any future retirement dispatch rests on, and the reason it
/// is worth pinning on its own is that it is the cheap half of such a switch.
/// The floor can always be handed to a parser that has never heard of the
/// prefix; what cannot, for `Chain` in ext-dict mode and for the tree, is the
/// *table*, whose entries are keyed at `prefix_len + position`. Read
/// [`PrefixedBlockFloors::prefix_retired`] before taking this as permission to
/// switch a parser: two of the three strategies that could take it were
/// measured and declined for ratio.
#[test]
fn retired_floors_agree_in_both_coordinate_spaces() {
    const PREFIX_LEN: usize = 1_125;
    const MAX_HISTORY: usize = 2_048;
    let prefix = vec![0u8; PREFIX_LEN];

    let mut retired_blocks = 0usize;
    for block_start in [0usize, 1_024, 2_048, 4_096, 16_384] {
        let block_end = block_start + 1_024;
        let floors = prefixed_block_floors(&prefix, block_start, block_end, MAX_HISTORY, 0);
        if !floors.prefix_retired {
            continue;
        }
        retired_blocks += 1;
        // What `plan_sequences_for_contiguous_block_into` builds for a block
        // with no prefix at all, at the same block start and window.
        let contiguous = MatchFloor::reaching(block_start.saturating_sub(MAX_HISTORY), MAX_HISTORY);
        for pos in block_start..block_end {
            let (prefix_low, source_low) = floors.match_floor.at(pos);
            assert_eq!(
                prefix_low, PREFIX_LEN,
                "a retired block left part of the prefix reachable at {pos}",
            );
            assert_eq!(
                source_low,
                contiguous.at(pos),
                "the two coordinate spaces disagreed at {pos} of the block at {block_start}",
            );
        }
    }

    // Anti-vacuity: the loop above skips live blocks, so a quiet pass would
    // otherwise be indistinguishable from one that never entered the body.
    assert!(
        retired_blocks >= 2,
        "only {retired_blocks} of the block starts retired the prefix",
    );
}

/// `prefixed_window_lows` decides which of C's two branches applies, so the
/// point where it declares the prefix out of reach is load-bearing.
///
/// One byte of it, specifically. `ZSTD_adjustCParams_internal` fits the window
/// to `highbit32(srcSize - 1) + 1`, so a source that is an exact power of two
/// gets a window of exactly itself and sits on this boundary from its very
/// first block. Retiring at `>=` rather than `>` therefore threw the whole
/// dictionary away before a single position had been searched, on precisely
/// the sizes a benchmark is most likely to pick: 1.05x to 3.73x upstream at
/// 4 KiB, 8 KiB and 16 KiB, against parity at 3 KiB, 6 KiB and 12 KiB.
#[test]
fn the_prefix_leaves_the_window_only_once_the_block_passes_it() {
    const PREFIX_LEN: usize = 1_125;
    const HISTORY: usize = 2_048;

    // Below the window the *whole* prefix is reachable, down to its low limit.
    // Not a trimmed slice of it: the dictionary lives outside the window and an
    // offset into it may exceed `Window_Size`. Trimming here retired a
    // dictionary larger than the window before it was ever searched.
    let (prefix_low, source_low) = prefixed_window_lows(PREFIX_LEN, 0, HISTORY - 1, HISTORY, 0);
    assert_eq!(prefix_low, 0);
    assert_eq!(source_low, 0);

    // A formatted dictionary's prefix starts two bytes before the content the
    // decoder is given, and no match may begin in them. Reaching position 0
    // produces an offset two larger than the decoder can resolve, which is a
    // frame this crate rejects on its own output.
    let (prefix_low, _) = prefixed_window_lows(PREFIX_LEN, 0, HISTORY - 1, HISTORY, 2);
    assert_eq!(prefix_low, 2);

    // *At* the window the prefix is still live. C's test is strictly greater --
    // `blockEndIdx > loadedDictEnd + maxDist`, `zstd_compress_internal.h:1315`
    // -- and this is the byte the whole boundary turns on.
    let (prefix_low, source_low) = prefixed_window_lows(PREFIX_LEN, 0, HISTORY, HISTORY, 0);
    assert_eq!(prefix_low, 0);
    assert_eq!(source_low, 0);

    // One byte past it, the prefix has gone.
    let (prefix_low, source_low) = prefixed_window_lows(PREFIX_LEN, 0, HISTORY + 1, HISTORY, 0);
    assert_eq!(prefix_low, PREFIX_LEN);
    // Zero rather than one: the source floor never passes the block's start,
    // and this block starts at zero. The clamp is the next assertion.
    assert_eq!(source_low, 0);

    // With the block starting later, that same byte lands in the source floor.
    let (prefix_low, source_low) = prefixed_window_lows(PREFIX_LEN, 64, HISTORY + 1, HISTORY, 0);
    assert_eq!(prefix_low, PREFIX_LEN);
    assert_eq!(source_low, 1);

    // The source floor never passes the block's start, which is the clamp that
    // keeps a block wider than its own window from flooring past its own bytes.
    let (_, source_low) = prefixed_window_lows(PREFIX_LEN, 4_096, 131_072, HISTORY, 0);
    assert_eq!(source_low, 4_096);
}

/// A body whose records repeat in shape but not in content, which is what makes
/// a match-finder test non-vacuous: a verbatim-repeating body is matched by any
/// parser that can find one offset, so it cannot tell two parsers apart.
fn imperfectly_periodic_records(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 128);
    let mut counter = 1u32;
    while out.len() < len {
        counter = counter.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.extend_from_slice(b"{\"id\":");
        out.extend_from_slice((counter % 100_000).to_string().as_bytes());
        out.extend_from_slice(b",\"region\":\"us-east-1\",\"tier\":\"gold\",\"amount\":");
        out.extend_from_slice((counter % 997).to_string().as_bytes());
        out.extend_from_slice(b"}\n");
    }
    out.truncate(len);
    out
}

/// Once the prefix has retired, a `Fast` block goes to the plain no-dictionary
/// parser, and this is the equivalence that makes that free rather than a trade.
///
/// The two are the same four-position walk under the same step acceleration, so
/// with every prefix position out of reach they agree position for position.
/// That is why the switch left 4 MiB of both dictionary cases byte-identical at
/// levels 1 and 2 while running 22-34% faster. `DoubleFast` has no such
/// equivalence and does not take the switch — see
/// `PrefixedBlockFloors::prefix_retired`.
///
/// Driven over a whole frame with the two states advancing independently,
/// because the interesting failure is a table falling out of step: that shows
/// up as a divergence in some *later* block, which comparing a single block in
/// isolation cannot see. The finders are compared at the end for the same
/// reason.
#[test]
fn retired_fast_prefix_matches_the_plain_parser() {
    const MAX_HISTORY: usize = 16 * 1024;
    const BLOCK_SIZE: usize = 4 * 1024;

    let prefix = imperfectly_periodic_records(1_024);
    let src = imperfectly_periodic_records(32 * 1024);
    let params = MatchFinderParameters {
        parser_strategy: ParserStrategy::Fast,
        ..MatchFinderParameters::default()
    };

    // One state takes the dispatch under test; the other is forced through the
    // prefixed parser for every block, retired or not.
    let mut dispatched = PrefixedBlockMatchState::new_with_mode(
        &prefix,
        src.len(),
        params,
        PrefixMatchMode::ExtDict,
    );
    let mut forced = dispatched.clone();

    let mut dispatched_offsets = RepeatOffsets::default();
    let mut forced_offsets = RepeatOffsets::default();
    let (mut retired_blocks, mut live_blocks, mut sequences) = (0usize, 0usize, 0usize);
    let mut block_start = 0usize;

    while block_start < src.len() {
        let block_end = (block_start + BLOCK_SIZE).min(src.len());
        let floors = prefixed_block_floors(
            &prefix,
            block_start,
            block_end,
            MAX_HISTORY,
            params.prefix_low_limit,
        );
        if floors.prefix_retired {
            retired_blocks += 1;
        } else {
            live_blocks += 1;
        }

        let mut dispatched_plan = SequencePlan::default();
        plan_sequences_for_prefixed_contiguous_block_into(
            &mut dispatched_plan,
            &prefix,
            &src[..block_end],
            block_start,
            dispatched_offsets,
            params,
            MAX_HISTORY,
            &mut dispatched,
        )
        .unwrap();

        let mut forced_plan = SequencePlan::default();
        let PrefixedBlockMatchStateInner::Fast {
            prefix_finder,
            src_finder,
            mode,
            prepared,
        } = &mut forced.inner
        else {
            panic!("the Fast strategy must build a Fast match state");
        };
        plan_sequences_fast_with_prefix_from_into(
            &mut forced_plan,
            &src[..block_end],
            block_start,
            &prefix,
            forced_offsets,
            params,
            floors.prefix_low,
            floors.source_low,
            prefix_finder.as_deref(),
            src_finder,
            *mode,
            prepared.as_deref(),
        )
        .unwrap();

        // Reported as the first differing sequence rather than as two whole
        // vectors: a block holds hundreds of them, and the pair either side of
        // the divergence is the part that says what went wrong.
        let divergence = dispatched_plan
            .sequences
            .iter()
            .zip(forced_plan.sequences.iter())
            .position(|(dispatched, forced)| dispatched != forced);
        assert!(
            divergence.is_none() && dispatched_plan.sequences.len() == forced_plan.sequences.len(),
            "sequences diverged in the block at {block_start} (retired={}) at index {divergence:?} \
             of {}/{}: {:?} against {:?}",
            floors.prefix_retired,
            dispatched_plan.sequences.len(),
            forced_plan.sequences.len(),
            divergence.map(|at| dispatched_plan.sequences[at]),
            divergence.map(|at| forced_plan.sequences[at]),
        );
        assert!(
            dispatched_plan.literals == forced_plan.literals,
            "literals diverged in the block at {block_start}: {} bytes against {}",
            dispatched_plan.literals.len(),
            forced_plan.literals.len(),
        );
        assert_eq!(
            dispatched_plan.repeat_offsets, forced_plan.repeat_offsets,
            "repeat offsets diverged in the block at {block_start}",
        );

        sequences += dispatched_plan.sequences.len();
        dispatched_offsets = dispatched_plan.repeat_offsets;
        forced_offsets = forced_plan.repeat_offsets;
        block_start = block_end;
    }

    // Anti-vacuity: the frame has to cross the boundary, not sit on one side of
    // it, and the parser has to have found something to disagree about.
    assert!(
        retired_blocks >= 2,
        "the body never passed the window: {retired_blocks} retired blocks",
    );
    assert!(
        live_blocks >= 2,
        "the prefix retired immediately: {live_blocks} live blocks",
    );
    assert!(
        sequences > 100,
        "the parse found almost nothing: {sequences}"
    );

    let (
        PrefixedBlockMatchStateInner::Fast {
            src_finder: dispatched_finder,
            ..
        },
        PrefixedBlockMatchStateInner::Fast {
            src_finder: forced_finder,
            ..
        },
    ) = (&dispatched.inner, &forced.inner)
    else {
        panic!("the Fast strategy must build a Fast match state");
    };
    let differing = dispatched_finder
        .heads
        .iter()
        .zip(forced_finder.heads.iter())
        .filter(|(dispatched, forced)| dispatched != forced)
        .count();
    assert_eq!(
        differing,
        0,
        "the two routes filed different positions in {differing} of {} hash slots",
        dispatched_finder.heads.len(),
    );
}

/// The prefix table exists exactly when a parse will read it.
///
/// `Fast` and `DoubleFast` build the dictionary's prefix table only under
/// [`PrefixMatchMode::ExtDict`], because under `DictMatchState` both parsers
/// return into the prepared path without touching it. That is safe only while
/// `prepared` is `Some` on every `DictMatchState` construction — otherwise
/// there is no parse left to take, and both parsers say so with an `expect`
/// rather than proceeding. Both halves are pinned here, since the saving and
/// the panic rest on the same invariant;
/// `a_dict_match_state_parse_without_prepared_tables_says_so` pins the panic
/// itself.
///
/// Filling it regardless cost a full index of the dictionary per frame, which
/// at a 110 KiB dictionary and a 1 KiB body was 173µs of a 177µs level-1
/// encode.
#[test]
fn the_prefix_table_is_built_exactly_when_a_parse_reads_it() {
    let prefix = b"abcdefghABCDEFGHabcdefghABCDEFGH";
    let src = b"abcdefghABCDEFGHEFGHEFGHTAILTAIL";

    for strategy in [ParserStrategy::Fast, ParserStrategy::DoubleFast] {
        let params = MatchFinderParameters {
            parser_strategy: strategy,
            min_match: 4,
            ..MatchFinderParameters::default()
        };

        for mode in [PrefixMatchMode::ExtDict, PrefixMatchMode::DictMatchState] {
            // Both with and without cached tables: the constructor is expected
            // to build them itself for `DictMatchState` when none was handed in,
            // and that locally built pair is what makes the table droppable.
            for cached in [false, true] {
                let prepared =
                    cached.then(|| build_prepared_dictionary_match_state(prefix, params).unwrap());
                let state = PrefixedBlockMatchState::new_with_prepared_match_state(
                    prefix,
                    src.len(),
                    params,
                    mode,
                    prepared.as_ref(),
                );

                let (prefix_finder_built, prepared_present) = match &state.inner {
                    PrefixedBlockMatchStateInner::Fast {
                        prefix_finder,
                        prepared,
                        ..
                    } => (prefix_finder.is_some(), prepared.is_some()),
                    PrefixedBlockMatchStateInner::DoubleFast {
                        prefix_finder,
                        prepared,
                        ..
                    } => (prefix_finder.is_some(), prepared.is_some()),
                    _ => panic!("{strategy:?} must build its own kind of match state"),
                };

                let ext_dict = mode == PrefixMatchMode::ExtDict;
                assert_eq!(
                    prefix_finder_built, ext_dict,
                    "{strategy:?}/{mode:?}/cached={cached}: the prefix table should be \
                     built for ext-dict mode and only for ext-dict mode",
                );
                assert!(
                    ext_dict || prepared_present,
                    "{strategy:?}/{mode:?}/cached={cached}: dropping the prefix table is \
                     only safe while dict-match-state always carries prepared tables",
                );
            }
        }
    }
}

/// A dict-match-state parse with no prepared tables panics rather than parsing.
///
/// Both prefixed fast parsers used to carry a third walk for this case, over
/// the dictionary's prefix table. It was dead -- the same mode that leaves
/// `prepared` unset is the one that builds no prefix table either, so the walk
/// unwrapped a `None` on its first line -- but it read as live code and a
/// comment above it said it was. What replaced it is the unwrap alone, and this
/// is what says the unwrap is real: a caller that breaks the invariant
/// [`the_prefix_table_is_built_exactly_when_a_parse_reads_it`] holds gets a
/// panic, not a frame parsed against tables nobody filled.
#[test]
// `catch_unwind` needs unwinding, which wasm targets do not have.
#[cfg_attr(target_family = "wasm", ignore = "panic = abort on wasm")]
fn a_dict_match_state_parse_without_prepared_tables_says_so() {
    let prefix = b"abcdefghABCDEFGHabcdefghABCDEFGH";
    let src = b"abcdefghABCDEFGHEFGHEFGHTAILTAIL";

    for strategy in [ParserStrategy::Fast, ParserStrategy::DoubleFast] {
        let params = MatchFinderParameters {
            parser_strategy: strategy,
            min_match: 4,
            ..MatchFinderParameters::default()
        };
        let mut plan = SequencePlan::default();
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match strategy {
                ParserStrategy::Fast => {
                    let mut src_finder = FastFinder::new(params.hash_bits, params.min_match);
                    plan_sequences_fast_with_prefix_from_into(
                        &mut plan,
                        src,
                        0,
                        prefix,
                        RepeatOffsets::default(),
                        params,
                        0,
                        0,
                        None,
                        &mut src_finder,
                        PrefixMatchMode::DictMatchState,
                        None,
                    )
                }
                _ => {
                    let mut src_finder = DoubleFastFinder::new(
                        params.hash_bits,
                        params.secondary_hash_bits,
                        params.min_match,
                    );
                    plan_sequences_double_fast_with_prefix_from_into(
                        &mut plan,
                        src,
                        0,
                        prefix,
                        RepeatOffsets::default(),
                        params,
                        0,
                        0,
                        None,
                        &mut src_finder,
                        PrefixMatchMode::DictMatchState,
                        None,
                    )
                }
            }
            .unwrap();
        }))
        .expect_err("{strategy:?} parsed a dict-match-state block with no prepared tables");
        // The message, not merely the panic: an early return on a short block
        // would leave this test passing for the wrong reason.
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(
            message.contains("must have been given prepared tables"),
            "{strategy:?} panicked, but not on the missing tables: {message}",
        );
    }
}

/// A cached state is refused for reuse when the entry width it was built at no
/// longer fits the next frame, and only then. The four strategies that file no
/// tagged table cannot change width, so the 16 MiB boundary must not reach
/// them: a reused `Encoder` that crosses it at level 10 would otherwise
/// rebuild its row table for nothing.
#[test]
fn only_the_tagged_strategies_gate_reuse_on_the_entry_width() {
    const BELOW: usize = PACKED_TAGGED_POSITION_LIMIT;
    const ABOVE: usize = PACKED_TAGGED_POSITION_LIMIT + 1;

    for strategy in [
        ParserStrategy::Fast,
        ParserStrategy::DoubleFast,
        ParserStrategy::Greedy,
        ParserStrategy::Lazy,
        ParserStrategy::Lazy2,
        ParserStrategy::GreedyRow,
        ParserStrategy::LazyRow,
        ParserStrategy::Lazy2Row,
        ParserStrategy::BinaryTreeLazy2,
        ParserStrategy::BinaryTreeOpt,
        ParserStrategy::BinaryTreeUltra,
    ] {
        let params = MatchFinderParameters {
            parser_strategy: strategy,
            ..MatchFinderParameters::default()
        };
        let tagged = matches!(strategy, ParserStrategy::Fast | ParserStrategy::DoubleFast);

        // Same width class on both sides: always reusable.
        for limit in [BELOW, ABOVE] {
            let mut state =
                ContiguousBlockMatchState::new_with_position_limit(limit, limit, params);
            assert!(
                state.reset_if_compatible(params, limit),
                "{strategy:?} refused reuse at an unchanged position limit of {limit}",
            );
        }

        // Across the boundary: only a tagged table has a layout to change.
        let mut state = ContiguousBlockMatchState::new_with_position_limit(BELOW, BELOW, params);
        assert_eq!(
            state.reset_if_compatible(params, ABOVE),
            !tagged,
            "{strategy:?} reuse across the 16 MiB boundary",
        );
        let mut state = ContiguousBlockMatchState::new_with_position_limit(ABOVE, ABOVE, params);
        assert_eq!(
            state.reset_if_compatible(params, BELOW),
            !tagged,
            "{strategy:?} reuse back across the 16 MiB boundary",
        );
    }
}
