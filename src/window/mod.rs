use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    entropy::mem::highbit32,
    error::{Error, Result},
    sequence::{
        RepeatOffsets, SequenceCommand, estimate_sequence_bit_cost, literal_length_code_unchecked,
        ll_bits, match_length_code_unchecked, ml_bits, offset_code_unchecked,
    },
};

mod binary_tree;
mod dict;
mod double_fast;
mod fast;
mod internal;
mod lazy;
mod ldm;
mod opt;
#[cfg(test)]
mod tests;

pub(crate) use self::binary_tree::*;
pub(crate) use self::dict::*;
pub(crate) use self::double_fast::*;
pub(crate) use self::fast::*;
pub(crate) use self::internal::*;
pub(crate) use self::lazy::*;
pub(crate) use self::ldm::*;
// Named again, and `pub`, because `ParameterOverrides` exposes it: a glob
// re-export cannot carry an item out past the private `window` module.
pub use self::ldm::LdmMode;
pub(crate) use self::opt::*;

#[derive(Debug, Clone)]
pub(crate) struct ContiguousBlockMatchState {
    inner: ContiguousBlockMatchStateInner,
    /// Where the fast and double-fast hash tables have been filled up to, for
    /// the long-distance path only.
    ///
    /// C keeps one `ms->nextToUpdate` per match state and every strategy
    /// shares it, but only the fast pair reads it outside their own parsers:
    /// they file positions as they parse and never consult it, so it moves
    /// only when something else fills their tables. Long-distance matching is
    /// the only thing that does (`ZSTD_ldm_fillFastTables`), because it hands
    /// the parser a segment that begins after a match it never saw. Every
    /// other strategy keeps its own cursor on its own finder.
    fast_table_next_to_update: usize,
    /// The parameters this state was built from.
    built_with: MatchFinderParameters,
    /// Whether a tagged table was built wide enough for positions past 16 MiB.
    wide_entries: bool,
}

#[derive(Debug, Clone)]
enum ContiguousBlockMatchStateInner {
    Chain(MatchFinder),
    Fast(FastFinder),
    WideFast(WideFastFinder),
    DoubleFast(DoubleFastFinder),
    WideDoubleFast(WideDoubleFastFinder),
    Row(RowHashFinder),
    BinaryTree(BinaryTreeFinder),
}

#[derive(Debug, Clone)]
pub(crate) struct PrefixedBlockMatchState {
    inner: PrefixedBlockMatchStateInner,
    /// Where [`fill_fast_tables`](Self::fill_fast_tables) should resume, in
    /// source coordinates.
    ///
    /// The fast pair keep no cursor of their own -- they file every position
    /// the parser walks past and never revisit one -- so the catch-up over a
    /// long-distance match needs somewhere to record how far it has got. The
    /// contiguous state holds the same field for the same reason.
    fast_table_next_to_update: usize,
}

#[derive(Debug, Clone)]
enum PrefixedBlockMatchStateInner {
    Chain {
        prefix_finder: Arc<MatchFinder>,
        src_finder: MatchFinder,
        mode: PrefixMatchMode,
        /// Where source position zero sits in the ext-dict virtual coordinate
        /// space, which is the length of the prefix and nothing else.
        ///
        /// Held here because the searches take it from `PrefixChain::len` while
        /// the inserts have only the finders to hand, and the two must agree:
        /// an insert that files a position under the hash of the wrong bytes
        /// leaves an entry no search can ever match.
        prefix_len: usize,
    },
    Fast {
        /// `None` under [`PrefixMatchMode::DictMatchState`], where the parse
        /// runs off `prepared` and never reads this. See the note where it is
        /// built.
        prefix_finder: Option<Arc<FastFinder>>,
        src_finder: FastFinder,
        mode: PrefixMatchMode,
        prepared: Option<Arc<PreparedFastDictionaryTables>>,
    },
    WideFast {
        prefix_finder: Option<Arc<WideFastFinder>>,
        src_finder: WideFastFinder,
        mode: PrefixMatchMode,
        prepared: Option<Arc<PreparedFastDictionaryTables>>,
    },
    DoubleFast {
        /// `None` under [`PrefixMatchMode::DictMatchState`], as for `Fast`.
        prefix_finder: Option<Arc<DoubleFastFinder>>,
        src_finder: DoubleFastFinder,
        mode: PrefixMatchMode,
        prepared: Option<Arc<PreparedDoubleFastDictionaryTables>>,
    },
    WideDoubleFast {
        prefix_finder: Option<Arc<WideDoubleFastFinder>>,
        src_finder: WideDoubleFastFinder,
        mode: PrefixMatchMode,
        prepared: Option<Arc<PreparedDoubleFastDictionaryTables>>,
    },
    Row {
        prefix_finder: Arc<RowHashFinder>,
        src_finder: RowHashFinder,
        mode: PrefixMatchMode,
    },
    BinaryTree {
        prefix_finder: Arc<BinaryTreeFinder>,
        src_finder: BinaryTreeFinder,
        mode: PrefixMatchMode,
    },
}

fn new_prefixed_fast_finders<E: TaggedEntry>(
    prefix: &[u8],
    params: MatchFinderParameters,
    mode: PrefixMatchMode,
) -> (Option<Arc<FastFinderImpl<E>>>, FastFinderImpl<E>) {
    let prefix_finder = (mode == PrefixMatchMode::ExtDict).then(|| {
        let mut finder = FastFinderImpl::new(params.dictionary_hash_bits(), params.min_match);
        // C's byCopyingCDict populates this from a CDict built with
        // `ZSTD_fillHashTableForCDict`: stride 3 with empty-slot extras.
        finder.insert_prefix_for_cdict(prefix);
        Arc::new(finder)
    });
    let src_finder = FastFinderImpl::new(params.hash_bits, params.min_match);
    (prefix_finder, src_finder)
}

fn new_prefixed_double_fast_finders<E: TaggedEntry>(
    prefix: &[u8],
    params: MatchFinderParameters,
    mode: PrefixMatchMode,
) -> (
    Option<Arc<DoubleFastFinderImpl<E>>>,
    DoubleFastFinderImpl<E>,
) {
    let prefix_finder = (mode == PrefixMatchMode::ExtDict).then(|| {
        let mut finder = DoubleFastFinderImpl::new(
            params.dictionary_hash_bits(),
            params.dictionary_chain_log(),
            params.min_match,
        );
        finder.insert_prefix_ext_dict(prefix);
        Arc::new(finder)
    });
    let src_finder = DoubleFastFinderImpl::new(
        params.hash_bits,
        params.secondary_hash_bits,
        params.min_match,
    );
    (prefix_finder, src_finder)
}

impl ContiguousBlockMatchState {
    pub(crate) fn new(src_len: usize, params: MatchFinderParameters) -> Self {
        Self::new_with_position_limit(src_len, src_len, params)
    }

    pub(crate) fn new_with_position_limit(
        src_len: usize,
        position_limit: usize,
        params: MatchFinderParameters,
    ) -> Self {
        let inner = match params.parser_strategy {
            ParserStrategy::Fast if position_limit > PACKED_TAGGED_POSITION_LIMIT => {
                ContiguousBlockMatchStateInner::WideFast(WideFastFinder::new(
                    params.hash_bits,
                    params.min_match,
                ))
            }
            ParserStrategy::Fast => ContiguousBlockMatchStateInner::Fast(FastFinder::new(
                params.hash_bits,
                params.min_match,
            )),
            ParserStrategy::DoubleFast if position_limit > PACKED_TAGGED_POSITION_LIMIT => {
                ContiguousBlockMatchStateInner::WideDoubleFast(WideDoubleFastFinder::new(
                    params.hash_bits,
                    params.secondary_hash_bits,
                    params.min_match,
                ))
            }
            ParserStrategy::DoubleFast => {
                ContiguousBlockMatchStateInner::DoubleFast(DoubleFastFinder::new(
                    params.hash_bits,
                    params.secondary_hash_bits,
                    params.min_match,
                ))
            }
            strategy if strategy.is_row_hash() => ContiguousBlockMatchStateInner::Row(
                RowHashFinder::new(params.hash_bits, params.search_log, params.min_match),
            ),
            strategy if strategy.is_hash_chain() => {
                ContiguousBlockMatchStateInner::Chain(MatchFinder::with_chain_log(
                    src_len,
                    params.hash_bits,
                    params.chain_log,
                    params.min_match,
                ))
            }
            strategy if strategy.is_binary_tree() => ContiguousBlockMatchStateInner::BinaryTree(
                BinaryTreeFinder::new(params.hash_bits, params.chain_log, params.min_match)
                    .with_window_log(params.window_log)
                    .with_search_depth(params.search_depth),
            ),
            _ => unreachable!(),
        };
        Self {
            inner,
            fast_table_next_to_update: 0,
            built_with: params,
            wide_entries: position_limit > PACKED_TAGGED_POSITION_LIMIT,
        }
    }

    /// Empty every table, returning the state to what [`Self::new`] would
    /// build for the parameters it already holds.
    pub(crate) fn reset(&mut self) {
        match &mut self.inner {
            ContiguousBlockMatchStateInner::Fast(finder) => finder.reset(),
            ContiguousBlockMatchStateInner::WideFast(finder) => finder.reset(),
            ContiguousBlockMatchStateInner::DoubleFast(finder) => finder.reset(),
            ContiguousBlockMatchStateInner::WideDoubleFast(finder) => finder.reset(),
            ContiguousBlockMatchStateInner::Row(finder) => finder.reset(),
            ContiguousBlockMatchStateInner::Chain(finder) => finder.reset(),
            ContiguousBlockMatchStateInner::BinaryTree(finder) => finder.reset(),
        }
        self.fast_table_next_to_update = 0;
    }

    /// [`Self::reset`], but only for a frame with the same parameters. Returns
    /// false, leaving the state untouched, when it was built for different
    /// ones and the caller must build a new one.
    ///
    /// The bar is equality of the whole parameter set rather than of the
    /// fields that decide table geometry, which is stricter than reuse
    /// strictly requires: two frames differing only in, say, `target_length`
    /// could share tables and now do not. That costs an allocation on a frame
    /// whose parameters changed -- no worse than the fresh state it replaces,
    /// and only where a caller varies parameters between frames -- and it buys
    /// a rule that stays true as parameters are added. The check this replaces
    /// listed the geometry fields alone, so four of the five finders took a
    /// state built at one `min_match` and parsed the next frame at another;
    /// the finders keep their own clamped copies of `min_match`, `chain_log`
    /// and `search_log`, and no `reset` restores them.
    pub(crate) fn reset_if_compatible(
        &mut self,
        params: MatchFinderParameters,
        position_limit: usize,
    ) -> bool {
        if self.built_with != params
            || self.wide_entries != (position_limit > PACKED_TAGGED_POSITION_LIMIT)
        {
            return false;
        }
        self.reset();
        true
    }

    /// The alignment [`shift_positions`](Self::shift_positions) requires of its
    /// `delta`, which the caller has to arrange when it decides how much of the
    /// buffer to drop.
    ///
    /// Every finder holds a hash table indexed by the bytes at each position,
    /// and those bytes do not move when the buffer in front of them is dropped.
    /// Chain and binary-tree finders hold a *second* table indexed by
    /// `position & mask`, and that one only survives a rebase when the low bits
    /// of every position come through unchanged. One is the alignment a finder
    /// with no such table imposes: any delta will do.
    pub(crate) fn rebase_period(&self) -> usize {
        match &self.inner {
            ContiguousBlockMatchStateInner::Fast(_)
            | ContiguousBlockMatchStateInner::WideFast(_)
            | ContiguousBlockMatchStateInner::DoubleFast(_)
            | ContiguousBlockMatchStateInner::WideDoubleFast(_)
            | ContiguousBlockMatchStateInner::Row(_) => 1,
            ContiguousBlockMatchStateInner::Chain(finder) => finder.rebase_period(),
            ContiguousBlockMatchStateInner::BinaryTree(finder) => finder.rebase_period(),
        }
    }

    /// Rebase every position this finder holds by `delta`, so the state stays
    /// usable after the first `delta` bytes are dropped from a buffer that
    /// currently ends at `live_end`. Returns false when neither route below
    /// applies and the caller must clear the state instead.
    ///
    /// Two routes, because a cycle-indexed table can be kept in either of two
    /// circumstances and the ones that arise are at opposite extremes. A cycle
    /// narrower than the drop is handled by aligning the drop to it, which costs
    /// nothing. A cycle wider than the whole buffer wraps nothing, so the table
    /// is a plain array and shifts bodily. Between them the caller widens the
    /// buffer until the first route applies, which is bounded because the band
    /// itself is.
    pub(crate) fn shift_positions(&mut self, delta: usize, live_end: usize) -> bool {
        let period = self.rebase_period();
        let by_slot = live_end <= period;
        if !by_slot && !delta.is_multiple_of(period) {
            return false;
        }
        // A cursor is a position like any other the finder holds, so it moves
        // with them. Leaving it behind would make the next long-distance fill
        // start from a position that now names different bytes.
        self.fast_table_next_to_update = self.fast_table_next_to_update.saturating_sub(delta);
        match &mut self.inner {
            // No cycle-indexed table, so both routes are the same one.
            ContiguousBlockMatchStateInner::Fast(finder) => finder.shift_positions(delta),
            ContiguousBlockMatchStateInner::WideFast(finder) => finder.shift_positions(delta),
            ContiguousBlockMatchStateInner::DoubleFast(finder) => finder.shift_positions(delta),
            ContiguousBlockMatchStateInner::WideDoubleFast(finder) => finder.shift_positions(delta),
            ContiguousBlockMatchStateInner::Row(finder) => finder.shift_positions(delta),
            ContiguousBlockMatchStateInner::Chain(finder) if by_slot => {
                finder.shift_positions_by_slot(delta, live_end);
            }
            ContiguousBlockMatchStateInner::Chain(finder) => finder.shift_positions(delta),
            ContiguousBlockMatchStateInner::BinaryTree(finder) if by_slot => {
                finder.shift_positions_by_slot(delta, live_end);
            }
            ContiguousBlockMatchStateInner::BinaryTree(finder) => finder.shift_positions(delta),
        }
        true
    }

    pub(crate) fn insert_range(&mut self, src: &[u8], start: usize, end: usize) {
        match &mut self.inner {
            ContiguousBlockMatchStateInner::Chain(finder) => finder.insert_range(src, start, end),
            ContiguousBlockMatchStateInner::Fast(finder) => finder.insert_range(src, start, end),
            ContiguousBlockMatchStateInner::WideFast(finder) => {
                finder.insert_range(src, start, end)
            }
            ContiguousBlockMatchStateInner::DoubleFast(finder) => {
                finder.insert_range(src, start, end)
            }
            ContiguousBlockMatchStateInner::WideDoubleFast(finder) => {
                finder.insert_range(src, start, end)
            }
            ContiguousBlockMatchStateInner::Row(finder) => finder.insert_range(src, start, end),
            ContiguousBlockMatchStateInner::BinaryTree(finder) => {
                let depth = finder.default_search_depth;
                finder.insert_range(src, start, end, depth)
            }
        }
    }

    /// Where a bypassed block's insert has to begin.
    ///
    /// The chain, row and tree finders catch up lazily from their own cursor,
    /// and a parser leaves that cursor wherever it stopped, short of the
    /// block's end. A block that goes out without the parser ever running has
    /// to file from there rather than from its own start: beginning at
    /// `block_start` would step the cursor over the span the previous parser
    /// left behind, and nothing would ever file it. That is the same shape as
    /// the block-tail insert deleted from
    /// `plan_sequences_for_contiguous_segment_into`, and it only became
    /// reachable once that insert stopped parking the cursor at the block end.
    ///
    /// Fast and DoubleFast hold no such cursor -- their parsers file as they
    /// go and nothing catches up afterwards -- so they file exactly the range
    /// they are handed.
    fn bypassed_insert_start(&self, block_start: usize) -> usize {
        match &self.inner {
            ContiguousBlockMatchStateInner::Chain(finder) => finder.next_to_update.min(block_start),
            ContiguousBlockMatchStateInner::Row(finder) => finder.next_to_update.min(block_start),
            ContiguousBlockMatchStateInner::BinaryTree(finder) => {
                finder.next_to_update.min(block_start)
            }
            ContiguousBlockMatchStateInner::Fast(_)
            | ContiguousBlockMatchStateInner::WideFast(_)
            | ContiguousBlockMatchStateInner::DoubleFast(_)
            | ContiguousBlockMatchStateInner::WideDoubleFast(_) => block_start,
        }
    }

    /// Insert a run-length block, which goes out without the parser ever
    /// running on it.
    ///
    /// C reaches the same table state by a different route: `ZSTD_isRLE` is
    /// tested *after* `ZSTD_buildSeqStore` has run (`zstd_compress.c:4430`),
    /// so the parser walks a run-length block like any other, and its first
    /// search catches up from `nextToUpdate` under the long-match clamp
    /// applied at the top of that function. Both steps happen here, in that
    /// order.
    pub(crate) fn insert_rle_block(&mut self, src: &[u8], start: usize, end: usize) {
        if end <= start {
            return;
        }
        self.limit_update_after_long_match(start);
        let start = self.bypassed_insert_start(start);
        self.insert_range(src, start, end);
    }

    /// Variant of `insert_range` used when a block went out raw without the
    /// parser ever running on it, so nothing else would file its positions.
    /// Without this the hash table would have zero entries from the bypassed
    /// block and later blocks could not match back into it.
    ///
    /// No long-match clamp here, unlike [`insert_rle_block`](Self::insert_rle_block):
    /// C returns `ZSTDbss_noCompress` from `ZSTD_buildSeqStore`
    /// (`zstd_compress.c:3279`) *before* reaching the clamp, so a block this
    /// small leaves the cursor alone and the next real block absorbs the whole
    /// catch-up.
    ///
    /// BinaryTree skips this because its next-block planner catches up lazily
    /// via `next_to_update`.
    pub(crate) fn insert_range_for_uncompressed_block(
        &mut self,
        src: &[u8],
        start: usize,
        end: usize,
    ) {
        if end <= start {
            return;
        }
        if matches!(&self.inner, ContiguousBlockMatchStateInner::BinaryTree(_)) {
            return;
        }
        let start = self.bypassed_insert_start(start);
        self.insert_range(src, start, end);
    }

    /// Abandon the oldest part of the catch-up when a finder has fallen a long
    /// way behind. See [`limited_update_after_long_match`].
    ///
    /// C keeps one `ms->nextToUpdate` and clamps it for every strategy at
    /// `zstd_compress.c:3297`, which is why this reaches each finder's cursor
    /// rather than only the tree's. It fires for any parser that ends a block
    /// more than 384 positions behind, which is routine: a parser stops on the
    /// match it just emitted, so a match longer than 384 bytes at the end of a
    /// block leaves the cursor that far back. A long-distance match is the
    /// extreme of the same thing, handing the parser a block it has seen none
    /// of.
    fn limit_update_after_long_match(&mut self, block_start: usize) {
        self.fast_table_next_to_update =
            limited_update_after_long_match(self.fast_table_next_to_update, block_start);
        match &mut self.inner {
            ContiguousBlockMatchStateInner::Chain(finder) => {
                finder.next_to_update =
                    limited_update_after_long_match(finder.next_to_update, block_start);
            }
            ContiguousBlockMatchStateInner::Row(finder) => {
                finder.next_to_update =
                    limited_update_after_long_match(finder.next_to_update, block_start);
            }
            ContiguousBlockMatchStateInner::BinaryTree(finder) => {
                finder.next_to_update =
                    limited_update_after_long_match(finder.next_to_update, block_start);
            }
            ContiguousBlockMatchStateInner::Fast(_)
            | ContiguousBlockMatchStateInner::WideFast(_)
            | ContiguousBlockMatchStateInner::DoubleFast(_)
            | ContiguousBlockMatchStateInner::WideDoubleFast(_) => {}
        }
    }

    /// C's `ZSTD_ldm_limitTableUpdate` (`zstd_ldm.c:331`), run before each
    /// segment of a long-distance block.
    ///
    /// A long-distance match hands the parser a segment starting well past
    /// where its table left off, and every strategy that fills lazily would
    /// otherwise catch up over the whole skipped match. This bounds that
    /// catch-up to 512 positions. It is a *second*, looser clamp on top of the
    /// per-block one in [`limited_update_after_long_match`]: 1024/512 here
    /// against 384/192 there, and C applies both.
    fn limit_update_after_ldm_match(&mut self, segment_start: usize) {
        let clamp = |cursor: usize| {
            if segment_start <= cursor + LDM_LIMITED_UPDATE_LAG {
                return cursor;
            }
            segment_start
                - LDM_LIMITED_UPDATE_SPAN.min(segment_start - cursor - LDM_LIMITED_UPDATE_LAG)
        };
        self.fast_table_next_to_update = clamp(self.fast_table_next_to_update);
        match &mut self.inner {
            ContiguousBlockMatchStateInner::Chain(finder) => {
                finder.next_to_update = clamp(finder.next_to_update);
            }
            ContiguousBlockMatchStateInner::Row(finder) => {
                finder.next_to_update = clamp(finder.next_to_update);
            }
            ContiguousBlockMatchStateInner::BinaryTree(finder) => {
                finder.next_to_update = clamp(finder.next_to_update);
            }
            // Neither files positions through a cursor while parsing; the
            // clamp above moved the one the fill below reads.
            ContiguousBlockMatchStateInner::Fast(_)
            | ContiguousBlockMatchStateInner::WideFast(_)
            | ContiguousBlockMatchStateInner::DoubleFast(_)
            | ContiguousBlockMatchStateInner::WideDoubleFast(_) => {}
        }
    }

    /// C's `ZSTD_ldm_fillFastTables` (`zstd_ldm.c:251`), run before each
    /// segment of a long-distance block.
    ///
    /// The fast pair never revisit a position once the parser has passed it,
    /// so the bytes a long-distance match covered would be missing from their
    /// tables entirely and no later match could reach them. Every other
    /// strategy fills inside its own parser, which is why C's switch has one
    /// arm per fast strategy and a `break` for the rest.
    ///
    /// The stride of three and the two-past-the-end bound are C's
    /// (`ZSTD_fillHashTableForCCtx` and `ZSTD_fillDoubleHashTableForCCtx`,
    /// both reached with `ZSTD_dtlm_fast`, which is why neither fills the two
    /// positions between the strides). The cursor deliberately does *not*
    /// advance to `end`: C leaves `nextToUpdate` where it is and lets the
    /// clamp above move it, which is what bounds the work this does.
    fn fill_fast_tables(&mut self, src: &[u8], end: usize) {
        if !matches!(
            &self.inner,
            ContiguousBlockMatchStateInner::Fast(_)
                | ContiguousBlockMatchStateInner::WideFast(_)
                | ContiguousBlockMatchStateInner::DoubleFast(_)
                | ContiguousBlockMatchStateInner::WideDoubleFast(_)
        ) {
            return;
        }
        // `ip + 3 < iend + 2` in C, with `iend = end - HASH_READ_SIZE`.
        let Some(limit) = end.checked_sub(FAST_FILL_HASH_READ_SIZE + FAST_FILL_STEP - 2) else {
            return;
        };
        let mut pos = self.fast_table_next_to_update;
        // One dispatch for the whole run, not one per position: with four
        // arms the compiler no longer lifts the match out of the loop, and
        // this loop is most of what a long match costs at level 1.
        macro_rules! fill {
            ($finder:expr) => {
                while pos < limit {
                    $finder.insert_src_position(src, pos);
                    pos += FAST_FILL_STEP;
                }
            };
        }
        match &mut self.inner {
            ContiguousBlockMatchStateInner::Fast(finder) => fill!(finder),
            ContiguousBlockMatchStateInner::WideFast(finder) => fill!(finder),
            ContiguousBlockMatchStateInner::DoubleFast(finder) => fill!(finder),
            ContiguousBlockMatchStateInner::WideDoubleFast(finder) => fill!(finder),
            _ => unreachable!("the strategy was checked above"),
        }
    }
}

impl PrefixedBlockMatchState {
    #[allow(dead_code)]
    pub(crate) fn new(prefix: &[u8], src_len: usize, params: MatchFinderParameters) -> Self {
        Self::new_with_mode(prefix, src_len, params, PrefixMatchMode::ExtDict)
    }

    pub(crate) fn new_with_mode(
        prefix: &[u8],
        src_len: usize,
        params: MatchFinderParameters,
        mode: PrefixMatchMode,
    ) -> Self {
        Self::new_with_prepared_match_state(prefix, src_len, params, mode, None)
    }

    pub(crate) fn new_with_prepared_match_state(
        prefix: &[u8],
        src_len: usize,
        params: MatchFinderParameters,
        mode: PrefixMatchMode,
        prepared_match_state: Option<&PreparedDictionaryMatchState>,
    ) -> Self {
        Self::new_with_prepared_match_state_and_position_limit(
            prefix,
            src_len,
            src_len,
            params,
            mode,
            prepared_match_state,
        )
    }

    pub(crate) fn new_with_prepared_match_state_and_position_limit(
        prefix: &[u8],
        src_len: usize,
        position_limit: usize,
        params: MatchFinderParameters,
        mode: PrefixMatchMode,
        prepared_match_state: Option<&PreparedDictionaryMatchState>,
    ) -> Self {
        let inner = match params.parser_strategy {
            strategy if strategy.is_row_hash() => {
                let prefix_finder = if let Some(PreparedDictionaryMatchState::Row(prefix_finder)) =
                    prepared_match_state
                {
                    prefix_finder.prefix_finder()
                } else {
                    // Every `prefix_finder` below is the dictionary's match
                    // state, so it takes the dictionary's table geometry; every
                    // `src_finder` stays on the applied parameters. C keeps the
                    // two apart the same way and reads whichever belongs to the
                    // table it is touching.
                    let mut prefix_finder = RowHashFinder::new(
                        params.dictionary_hash_bits(),
                        params.search_log,
                        params.min_match,
                    );
                    prefix_finder.insert_prefix(prefix);
                    Arc::new(prefix_finder)
                };
                let mut src_finder =
                    RowHashFinder::new(params.hash_bits, params.search_log, params.min_match);
                src_finder.hash_salt = prefix_finder.hash_salt;
                PrefixedBlockMatchStateInner::Row {
                    prefix_finder,
                    src_finder,
                    mode,
                }
            }
            strategy if strategy.is_hash_chain() => {
                let prefix_finder =
                    if let Some(PreparedDictionaryMatchState::Chain(prefix_finder)) =
                        prepared_match_state
                    {
                        prefix_finder.prefix_finder()
                    } else {
                        let mut prefix_finder = MatchFinder::with_chain_log(
                            prefix.len(),
                            params.dictionary_hash_bits(),
                            params.dictionary_chain_log(),
                            params.min_match,
                        );
                        let prefix_refs = [prefix];
                        let prefix_chain = PrefixChain::new(&prefix_refs)
                            .expect("single prefix must not overflow")
                            .expect("single non-empty prefix expected");
                        prefix_finder.insert_prefix_chain(prefix_chain, &[]);
                        Arc::new(prefix_finder)
                    };
                let src_finder = if mode == PrefixMatchMode::ExtDict {
                    prefix_finder.as_ref().clone()
                } else {
                    MatchFinder::with_chain_log(
                        src_len,
                        params.hash_bits,
                        params.chain_log,
                        params.min_match,
                    )
                };
                PrefixedBlockMatchStateInner::Chain {
                    prefix_finder,
                    src_finder,
                    mode,
                    prefix_len: prefix.len(),
                }
            }
            ParserStrategy::Fast => {
                let prepared = match prepared_match_state {
                    Some(PreparedDictionaryMatchState::Fast(prepared)) => Some(prepared.clone()),
                    _ if mode == PrefixMatchMode::DictMatchState => {
                        Some(Arc::new(PreparedFastDictionaryTables::build(
                            prefix,
                            params.dictionary_hash_bits(),
                            params.min_match,
                        )))
                    }
                    _ => None,
                };
                // Built only for the mode that reads it. Under
                // `DictMatchState` the parser returns into the prepared path
                // before touching this table, and `prepared` is `Some` there by
                // construction: the arm above builds it locally whenever none
                // was cached. Filling it anyway indexed the whole dictionary
                // once per frame and threw the result away, which is most of
                // what a prepared dictionary is supposed to save at these two
                // strategies.
                // Ext-dict parsing temporarily lifts both tables into the
                // prefix-plus-source coordinate space, so that sum is the
                // widest position either entry representation may carry.
                if prefix.len().saturating_add(position_limit) > PACKED_TAGGED_POSITION_LIMIT {
                    let (prefix_finder, src_finder) =
                        new_prefixed_fast_finders::<u64>(prefix, params, mode);
                    PrefixedBlockMatchStateInner::WideFast {
                        prefix_finder,
                        src_finder,
                        mode,
                        prepared,
                    }
                } else {
                    let (prefix_finder, src_finder) =
                        new_prefixed_fast_finders::<u32>(prefix, params, mode);
                    PrefixedBlockMatchStateInner::Fast {
                        prefix_finder,
                        src_finder,
                        mode,
                        prepared,
                    }
                }
            }
            ParserStrategy::DoubleFast => {
                let prepared = match prepared_match_state {
                    Some(PreparedDictionaryMatchState::DoubleFast(prepared)) => {
                        Some(prepared.clone())
                    }
                    _ if mode == PrefixMatchMode::DictMatchState => {
                        Some(Arc::new(PreparedDoubleFastDictionaryTables::build(
                            prefix,
                            params.dictionary_hash_bits(),
                            params.dictionary_chain_log(),
                            params.min_match,
                        )))
                    }
                    _ => None,
                };
                // Built only for the mode that reads it, as for `Fast` above.
                // The `insert_prefix` this used to run under `DictMatchState`
                // was the dead half.
                if prefix.len().saturating_add(position_limit) > PACKED_TAGGED_POSITION_LIMIT {
                    let (prefix_finder, src_finder) =
                        new_prefixed_double_fast_finders::<u64>(prefix, params, mode);
                    PrefixedBlockMatchStateInner::WideDoubleFast {
                        prefix_finder,
                        src_finder,
                        mode,
                        prepared,
                    }
                } else {
                    let (prefix_finder, src_finder) =
                        new_prefixed_double_fast_finders::<u32>(prefix, params, mode);
                    PrefixedBlockMatchStateInner::DoubleFast {
                        prefix_finder,
                        src_finder,
                        mode,
                        prepared,
                    }
                }
            }
            strategy if strategy.is_binary_tree() => {
                let prefix_finder =
                    if let Some(PreparedDictionaryMatchState::BinaryTree(prefix_finder)) =
                        prepared_match_state
                    {
                        prefix_finder.prefix_finder()
                    } else {
                        let mut prefix_finder = BinaryTreeFinder::new(
                            params.dictionary_hash_bits(),
                            params.dictionary_chain_log(),
                            params.min_match,
                        )
                        .with_window_log(params.dictionary_window_log());
                        let prefix_refs = [prefix];
                        let prefix_chain = PrefixChain::new(&prefix_refs)
                            .expect("single prefix must not overflow")
                            .expect("single non-empty prefix expected");
                        prefix_finder.insert_prefix_chain(prefix_chain, &[], params.search_depth);
                        Arc::new(prefix_finder)
                    };
                let mut src_finder =
                    BinaryTreeFinder::new(params.hash_bits, params.chain_log, params.min_match)
                        .with_window_log(params.window_log)
                        .with_search_depth(params.search_depth);

                // For ExtDict, pre-populate src_finder with dictionary entries
                // matching C's ZSTD_loadDictionaryContent → ZSTD_updateTree.
                if mode == PrefixMatchMode::ExtDict {
                    let prefix_refs = [prefix];
                    if let Some(prefix_chain) = PrefixChain::new(&prefix_refs).ok().flatten() {
                        src_finder.insert_prefix_into_unified(
                            prefix_chain,
                            params.search_depth,
                            params.window_log,
                        );
                        src_finder.prefix_len = prefix_chain.len();
                        src_finder.next_to_update = prefix_chain.len();
                    }
                }

                PrefixedBlockMatchStateInner::BinaryTree {
                    prefix_finder,
                    src_finder,
                    mode,
                }
            }
            _ => unreachable!(),
        };
        Self {
            inner,
            fast_table_next_to_update: 0,
        }
    }

    pub(crate) fn insert_range(&mut self, src: &[u8], start: usize, end: usize) {
        match &mut self.inner {
            PrefixedBlockMatchStateInner::Chain {
                src_finder,
                mode,
                prefix_len,
                ..
            } => match mode {
                PrefixMatchMode::DictMatchState => src_finder.insert_range(src, start, end),
                PrefixMatchMode::ExtDict => {
                    src_finder.insert_ext_dict_range(*prefix_len, src, start, end)
                }
            },
            PrefixedBlockMatchStateInner::Fast { src_finder, .. } => {
                src_finder.insert_range(src, start, end)
            }
            PrefixedBlockMatchStateInner::WideFast { src_finder, .. } => {
                src_finder.insert_range(src, start, end)
            }
            PrefixedBlockMatchStateInner::DoubleFast { src_finder, .. } => {
                src_finder.insert_range(src, start, end)
            }
            PrefixedBlockMatchStateInner::WideDoubleFast { src_finder, .. } => {
                src_finder.insert_range(src, start, end)
            }
            PrefixedBlockMatchStateInner::Row { src_finder, .. } => {
                src_finder.insert_range(src, start, end)
            }
            PrefixedBlockMatchStateInner::BinaryTree { src_finder, .. } => {
                if src_finder.prefix_len > 0 {
                    // When pre-populated with dictionary entries, use DUBT-style
                    // unsorted insertion at virtual coordinates. This avoids
                    // needing the prefix data for byte comparison (O(1) per position).
                    // The entries will be sorted during the next find_match_with_prefix.
                    src_finder.dubt_insert_range_virtual(
                        src,
                        src_finder.next_to_update,
                        src_finder.prefix_len + end,
                    );
                } else {
                    let depth = src_finder.default_search_depth;
                    src_finder.insert_range(src, start, end, depth)
                }
            }
        }
    }

    /// Abandon the oldest part of the catch-up when the tree has fallen a long
    /// way behind. See [`limited_update_after_long_match`].
    ///
    /// `next_to_update` is in the finder's own coordinates, which are offset by
    /// the dictionary prefix once one has been loaded, so the block start has to
    /// be lifted into the same space before the two are compared.
    fn limit_update_after_long_match(&mut self, block_start: usize) {
        if let PrefixedBlockMatchStateInner::BinaryTree { src_finder, .. } = &mut self.inner {
            src_finder.next_to_update = limited_update_after_long_match(
                src_finder.next_to_update,
                src_finder.prefix_len + block_start,
            );
        }
    }

    /// C's `ZSTD_ldm_limitTableUpdate` (`zstd_ldm.c:331`), run before each
    /// segment of a long-distance block.
    ///
    /// The contiguous twin explains what the clamp is for. What differs here is
    /// its reach: [`limit_update_after_long_match`](Self::limit_update_after_long_match)
    /// touches the tree alone, because between blocks
    /// [`insert_block_tail`](Self::insert_block_tail) files whole blocks and
    /// leaves no other cursor behind. A long-distance match is *inside* a block,
    /// where nothing has filed anything yet, so every lazily-filled finder can
    /// be a whole match behind and every one of them has to be clamped.
    ///
    /// Each cursor is clamped in its own coordinate space, which is not the same
    /// space for all of them: the chain in ext-dict mode and the binary tree
    /// count from the start of the prefix, while the row finder and the fast
    /// pair count from the start of the source. Comparing a cursor against a
    /// bound measured in the other space is the defect this crate has already
    /// paid for twice.
    fn limit_update_after_ldm_match(&mut self, segment_start: usize) {
        let clamp = |cursor: usize, segment_start: usize| {
            if segment_start <= cursor + LDM_LIMITED_UPDATE_LAG {
                return cursor;
            }
            segment_start
                - LDM_LIMITED_UPDATE_SPAN.min(segment_start - cursor - LDM_LIMITED_UPDATE_LAG)
        };
        self.fast_table_next_to_update = clamp(self.fast_table_next_to_update, segment_start);
        match &mut self.inner {
            PrefixedBlockMatchStateInner::Chain {
                src_finder,
                mode,
                prefix_len,
                ..
            } => {
                let segment_start = match mode {
                    PrefixMatchMode::DictMatchState => segment_start,
                    PrefixMatchMode::ExtDict => *prefix_len + segment_start,
                };
                src_finder.next_to_update = clamp(src_finder.next_to_update, segment_start);
            }
            PrefixedBlockMatchStateInner::Row { src_finder, .. } => {
                src_finder.next_to_update = clamp(src_finder.next_to_update, segment_start);
            }
            PrefixedBlockMatchStateInner::BinaryTree { src_finder, .. } => {
                src_finder.next_to_update = clamp(
                    src_finder.next_to_update,
                    src_finder.prefix_len + segment_start,
                );
            }
            // Neither files positions through a cursor while parsing; the clamp
            // above moved the one the fill below reads.
            PrefixedBlockMatchStateInner::Fast { .. }
            | PrefixedBlockMatchStateInner::WideFast { .. }
            | PrefixedBlockMatchStateInner::DoubleFast { .. }
            | PrefixedBlockMatchStateInner::WideDoubleFast { .. } => {}
        }
    }

    /// C's `ZSTD_ldm_fillFastTables` (`zstd_ldm.c:251`), run before each segment
    /// of a long-distance block. See the contiguous twin for why the fast pair
    /// need this and the others do not.
    fn fill_fast_tables(&mut self, src: &[u8], end: usize) {
        if !matches!(
            &self.inner,
            PrefixedBlockMatchStateInner::Fast { .. }
                | PrefixedBlockMatchStateInner::WideFast { .. }
                | PrefixedBlockMatchStateInner::DoubleFast { .. }
                | PrefixedBlockMatchStateInner::WideDoubleFast { .. }
        ) {
            return;
        }
        let Some(limit) = end.checked_sub(FAST_FILL_HASH_READ_SIZE + FAST_FILL_STEP - 2) else {
            return;
        };
        let mut pos = self.fast_table_next_to_update;
        // One dispatch for the whole run, not one per position: with four
        // arms the compiler no longer lifts the match out of the loop, and
        // this loop is most of what a long match costs at level 1.
        macro_rules! fill {
            ($finder:expr) => {
                while pos < limit {
                    $finder.insert_src_position(src, pos);
                    pos += FAST_FILL_STEP;
                }
            };
        }
        match &mut self.inner {
            PrefixedBlockMatchStateInner::Fast { src_finder, .. } => fill!(src_finder),
            PrefixedBlockMatchStateInner::WideFast { src_finder, .. } => fill!(src_finder),
            PrefixedBlockMatchStateInner::DoubleFast { src_finder, .. } => fill!(src_finder),
            PrefixedBlockMatchStateInner::WideDoubleFast { src_finder, .. } => fill!(src_finder),
            _ => unreachable!("the strategy was checked above"),
        }
    }

    /// Bring the finder up to the end of the block the parser just crossed.
    ///
    /// Despite the name this inserts the whole block, not a tail, and that is
    /// what makes it safe: `insert_range` starts at `max(start, cursor)`, so a
    /// range covering the block reaches every position between where the
    /// parser stopped and the block's end. Nothing is stepped over.
    ///
    /// C has no per-block insert at all: `nextToUpdate` only ever advances
    /// through positions actually inserted, and the next block's first search
    /// catches up over the whole gap. Doing it eagerly here reaches the same
    /// table contents, because nothing searches in between. The one thing it
    /// does not reproduce is the long-match clamp, which measures from where
    /// the parser really stopped and so cannot fire once the cursor has been
    /// parked at the block end. Here that is deliberate: this state's
    /// [`limit_update_after_long_match`](Self::limit_update_after_long_match)
    /// only reaches the tree, whose cursor this leaves alone.
    ///
    /// The contiguous state does none of this. It used to insert a 64-byte
    /// *window* at the block's end, which both stepped the cursor over the
    /// span the parser left behind and hid the gap from the clamp; it now
    /// files nothing and lets each finder catch up, as C does. See the comment
    /// in `plan_sequences_for_contiguous_segment_into`.
    fn insert_block_tail(&mut self, src: &[u8], block_start: usize, block_end: usize) {
        match &mut self.inner {
            PrefixedBlockMatchStateInner::Fast { .. }
            | PrefixedBlockMatchStateInner::WideFast { .. }
            | PrefixedBlockMatchStateInner::DoubleFast { .. }
            | PrefixedBlockMatchStateInner::WideDoubleFast { .. } => {}
            _ => self.insert_range(src, block_start, block_end),
        }
    }

    /// Variant of `insert_range` used when a block went out raw without the
    /// parser running. Same policy as the contiguous variant — see that doc.
    pub(crate) fn insert_range_for_uncompressed_block(
        &mut self,
        src: &[u8],
        start: usize,
        end: usize,
    ) {
        if end <= start {
            return;
        }
        if matches!(&self.inner, PrefixedBlockMatchStateInner::BinaryTree { .. }) {
            return;
        }
        self.insert_range(src, start, end);
    }
}

/// Where a lazily-filled match finder should resume inserting at the start of a
/// block, given how far behind it fell during the previous one.
///
/// C's "limited update after a very long match" in `ZSTD_buildSeqStore`
/// (`zstd_compress.c`), run once per block before the parser sees it:
///
/// ```c
/// if (curr > ms->nextToUpdate + 384)
///     ms->nextToUpdate = curr - MIN(192, (U32)(curr - ms->nextToUpdate - 384));
/// ```
///
/// The binary tree records how far it has been filled and lets the next block
/// bridge the gap. A block the parser crossed in one long match leaves that gap
/// a whole block wide, and bridging it is not free: each skipped position is
/// inserted against a buffer that now ends a block further away, so on input
/// with a long period every one of those insertions counts a match running to
/// the end of the block. Upstream gives up on the oldest part of the gap
/// instead, which costs the tree some entries nothing was going to ask for and
/// holds the catch-up to 192 positions.
fn limited_update_after_long_match(next_to_update: usize, block_start: usize) -> usize {
    if block_start <= next_to_update + LIMITED_UPDATE_LAG {
        return next_to_update;
    }
    block_start - LIMITED_UPDATE_SPAN.min(block_start - next_to_update - LIMITED_UPDATE_LAG)
}

#[allow(dead_code)]
pub(crate) fn plan_sequences(src: &[u8], repeat_offsets: RepeatOffsets) -> Result<SequencePlan> {
    plan_sequences_with_params(src, repeat_offsets, MatchFinderParameters::default())
}

#[allow(dead_code)]
pub(crate) fn plan_sequences_with_params(
    src: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_with_params_into(&mut plan, src, repeat_offsets, params)?;
    Ok(plan)
}

#[allow(dead_code)]
pub(crate) fn plan_sequences_with_params_and_prefix(
    src: &[u8],
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<SequencePlan> {
    let prefixes = [prefix];
    plan_sequences_with_params_and_prefixes(src, &prefixes, repeat_offsets, params)
}

fn plan_sequences_with_params_into(
    plan: &mut SequencePlan,
    src: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<()> {
    plan_sequences_without_prefix_into(plan, src, repeat_offsets, params)
}

pub(crate) fn plan_sequences_with_params_and_prefixes(
    src: &[u8],
    prefixes: &[&[u8]],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_with_params_and_prefixes_into(&mut plan, src, prefixes, repeat_offsets, params)?;
    Ok(plan)
}

fn plan_sequences_with_params_and_prefixes_into(
    plan: &mut SequencePlan,
    src: &[u8],
    prefixes: &[&[u8]],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<()> {
    let Some(prefix_chain) = PrefixChain::new(prefixes)? else {
        return plan_sequences_without_prefix_into(plan, src, repeat_offsets, params);
    };
    plan_sequences_with_prefix_chain_into(plan, src, prefix_chain, repeat_offsets, params)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn plan_sequences_for_block(
    src: &[u8],
    prefixes: &[&[u8]],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_for_block_into(&mut plan, src, prefixes, repeat_offsets, params)?;
    Ok(plan)
}

pub(crate) fn plan_sequences_for_block_into(
    plan: &mut SequencePlan,
    src: &[u8],
    prefixes: &[&[u8]],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<()> {
    match params.parser_strategy {
        strategy if strategy.is_row_hash() => {
            if prefixes.iter().all(|prefix| prefix.is_empty()) {
                plan_sequences_row_without_prefix_into(plan, src, repeat_offsets, params)
            } else {
                plan_sequences_row_with_prefixes_into(plan, src, prefixes, repeat_offsets, params)
            }
        }
        strategy if strategy.is_hash_chain() => {
            if prefixes.iter().all(|prefix| prefix.is_empty()) {
                plan_sequences_with_params_into(plan, src, repeat_offsets, params)
            } else {
                plan_sequences_with_params_and_prefixes_into(
                    plan,
                    src,
                    prefixes,
                    repeat_offsets,
                    params,
                )
            }
        }
        ParserStrategy::Fast => match single_non_empty_prefix(prefixes) {
            Ok(Some(prefix)) => {
                plan_sequences_fast_with_prefix_into(plan, src, prefix, repeat_offsets, params)
            }
            Ok(None) => plan_sequences_fast_without_prefix_into(plan, src, repeat_offsets, params),
            Err(()) => plan_sequences_with_params_and_prefixes_into(
                plan,
                src,
                prefixes,
                repeat_offsets,
                params,
            ),
        },
        ParserStrategy::DoubleFast => match single_non_empty_prefix(prefixes) {
            Ok(Some(prefix)) => plan_sequences_double_fast_with_prefix_into(
                plan,
                src,
                prefix,
                repeat_offsets,
                params,
            ),
            Ok(None) => {
                plan_sequences_double_fast_without_prefix_into(plan, src, repeat_offsets, params)
            }
            Err(()) => plan_sequences_with_params_and_prefixes_into(
                plan,
                src,
                prefixes,
                repeat_offsets,
                params,
            ),
        },
        strategy if strategy.is_binary_tree() => match single_non_empty_prefix(prefixes) {
            Ok(Some(prefix)) => plan_sequences_binary_tree_with_prefix_into(
                plan,
                src,
                prefix,
                repeat_offsets,
                params,
            ),
            Ok(None) => {
                plan_sequences_binary_tree_without_prefix_into(plan, src, repeat_offsets, params)
            }
            Err(()) => plan_sequences_binary_tree_with_prefixes_into(
                plan,
                src,
                prefixes,
                repeat_offsets,
                params,
            ),
        },
        _ => unreachable!(),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn plan_sequences_for_contiguous_block(
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    max_history_bytes: usize,
    state: &mut ContiguousBlockMatchState,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_for_contiguous_block_into(
        &mut plan,
        src,
        block_start,
        repeat_offsets,
        params,
        max_history_bytes,
        state,
    )?;
    Ok(plan)
}

pub(crate) fn plan_sequences_for_contiguous_block_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    max_history_bytes: usize,
    state: &mut ContiguousBlockMatchState,
) -> Result<()> {
    plan_sequences_for_contiguous_block_into_with_candidates(
        plan,
        src,
        block_start,
        repeat_offsets,
        params,
        max_history_bytes,
        state,
        None,
    )
}

/// One whole block through one parser, with the block's long-distance matches
/// offered to it as candidates if it is one of the parsers that price them.
#[allow(clippy::too_many_arguments)]
fn plan_sequences_for_contiguous_block_into_with_candidates(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    max_history_bytes: usize,
    state: &mut ContiguousBlockMatchState,
    ldm: Option<&[RawSequence]>,
) -> Result<()> {
    // C runs this once per block in `ZSTD_buildSeqStore`, before the parser
    // sees the block and for every strategy.
    state.limit_update_after_long_match(block_start);
    plan_sequences_for_contiguous_segment_into(
        plan,
        src,
        block_start,
        block_start.saturating_sub(max_history_bytes),
        repeat_offsets,
        params,
        max_history_bytes,
        state,
        ldm,
    )
}

/// One run of the parser over `[segment_start, src.len())`.
///
/// A block without long-distance matching is a single segment, which is what
/// [`plan_sequences_for_contiguous_block_into`] calls this as. With it, a block
/// is one segment per gap between long-distance matches, and `rep_window_low`
/// is the one thing that does *not* follow the segment: C sets
/// `window.lowLimit` once per block in `ZSTD_window_enforceMaxDist` and the
/// per-segment calls read it unchanged, so it stays measured from the block's
/// start however the block is cut up.
#[allow(clippy::too_many_arguments)]
fn plan_sequences_for_contiguous_segment_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    rep_window_low: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    max_history_bytes: usize,
    state: &mut ContiguousBlockMatchState,
    ldm: Option<&[RawSequence]>,
) -> Result<()> {
    debug_assert!(
        ldm.is_none() || matches!(state.inner, ContiguousBlockMatchStateInner::BinaryTree(_)),
        "only the binary tree carries a parser that prices long-distance matches"
    );
    // One floor for the whole block, measured from its *end*. That is what C's
    // fast and double-fast parsers use — `prefixStartIndex =
    // ZSTD_getLowestPrefixIndex(ms, endIndex, ..)` at `zstd_fast.c:204` and
    // `zstd_double_fast.c:119`. It bounds the widest offset those two can emit
    // by the window itself *except* under the clamp below, which is why the
    // declared `Window_Size` still carries a block on top of the history.
    //
    // Measuring from `block_start` instead let a match reach a whole block
    // further back. That is why frames had to declare `window + block_size`
    // for a conforming decoder to accept them, and the extra reach cost ratio
    // rather than buying any: on a body whose period sits just inside the
    // window, level 1 spent offset bits on matches upstream will not take and
    // never settled into the repeat-offset groove upstream rides for the rest
    // of the frame. On 4 MiB of JSON records that was 305581 bytes against
    // 242386.
    //
    // The `.min(block_start)` clamp holds the floor down to the block's start
    // for the case where the block is wider than its own window. C avoids that
    // case by capping a block at the window; neither encoder here does, so both
    // declare a window wide enough for the blocks they emit instead. Letting
    // the floor rise past `block_start` would underflow the branchless
    // in-window tests and admit a match index from outside the buffer.
    let window_low = src.len().saturating_sub(max_history_bytes).min(block_start);
    // Repeat offsets carried in from the previous block are a separate bound,
    // and C retires them against the block's *start*
    // (`ZSTD_compressBlock_fast_noDict_generic`, just above the parse loop).
    // Judging them by `window_low` would drop offsets that stay addressable
    // from every position in this block.
    //
    // Every other parser gets C's per-position floor instead. `rep_window_low`
    // is its base for the same reason it bounds the repeat offsets:
    // `ZSTD_window_enforceMaxDist` is called with the block's *start*
    // (`zstd_compress.c:4630`), so `window.lowLimit` is `blockStart - maxDist`,
    // and `ZSTD_getLowestMatchIndex` lifts it to `curr - maxDist` at every
    // position that looks.
    let match_floor = MatchFloor::reaching(rep_window_low, max_history_bytes);
    match &mut state.inner {
        ContiguousBlockMatchStateInner::Row(finder) => plan_sequences_row_without_prefix_from_into(
            plan,
            src,
            block_start,
            repeat_offsets,
            params,
            match_floor,
            finder,
        ),
        ContiguousBlockMatchStateInner::Chain(finder) => plan_sequences_without_prefix_from_into(
            plan,
            src,
            block_start,
            repeat_offsets,
            params,
            match_floor,
            finder,
        ),
        ContiguousBlockMatchStateInner::Fast(finder) => {
            plan_sequences_fast_without_prefix_from_into(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            )
        }
        ContiguousBlockMatchStateInner::WideFast(finder) => {
            plan_sequences_fast_without_prefix_from_into(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            )
        }
        ContiguousBlockMatchStateInner::DoubleFast(finder) => {
            plan_sequences_double_fast_without_prefix_from_into(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            )
        }
        ContiguousBlockMatchStateInner::WideDoubleFast(finder) => {
            plan_sequences_double_fast_without_prefix_from_into(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            )
        }
        ContiguousBlockMatchStateInner::BinaryTree(finder) => {
            plan_sequences_binary_tree_without_prefix_from_into(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                match_floor,
                finder,
                ldm,
            )
        }
    }?;
    // Nothing is filed at the block's end. Every finder here catches up from its
    // own `next_to_update` the next time it searches, which is C's model: a
    // parser stops short of `iend`, leaves the cursor where it stopped, and the
    // next block's first search inserts the gap -- clamped, if it is a long way
    // behind, by `limited_update_after_long_match` and, under long-distance
    // matching, by `limit_update_after_ldm_match`.
    //
    // This used to file the block's last 64 positions eagerly instead. That is
    // both too much and too little: it moved the cursor to the block's end,
    // hiding the gap from those clamps, and it *dropped* whatever sat between
    // where the parser stopped and those 64 bytes. On `json-records` at greedy
    // the parser stopped 108 bytes short of a block boundary, so positions 64
    // through 108 back were never filed at all, and the next block missed a
    // 79-byte match that upstream took. Removing it closed 23 of the 25
    // divergences in the long-distance parameter grid and cost one row three
    // bytes -- see `KNOWN_UPSTREAM_SIZE_GAPS`.
    Ok(())
}

/// Everything one frame needs to run the long-distance matcher: the table that
/// outlives its blocks, and the two scratch buffers a block borrows.
///
/// Bundled because the three have exactly the same lifetime and are useless
/// apart. A frame that does not run the matcher holds no [`LdmFrameState`] at
/// all, which is what keeps "is it on" from being a separate flag that could
/// disagree with whether the table exists.
#[derive(Debug, Clone)]
pub(crate) struct LdmFrameState {
    matcher: LdmState,
    /// How far back a match may reach: the frame's own history limit, which is
    /// what its header will declare. See [`LdmState::generate_sequences`].
    max_distance: usize,
    /// The current block's matches, in the matcher's own coordinates.
    sequences: Vec<RawSequence>,
    /// Which block [`Self::sequences`] holds the matches of, as the
    /// `[start, end)` it was generated for.
    generated_for: Option<(usize, usize)>,
    /// One gap between two long-distance matches, parsed on its own before
    /// being appended to the block's plan.
    segment: SequencePlan,
}

impl LdmFrameState {
    pub(crate) fn new(params: LdmParameters, max_distance: usize) -> Self {
        Self {
            matcher: LdmState::new(params),
            max_distance,
            sequences: Vec::new(),
            generated_for: None,
            segment: SequencePlan::default(),
        }
    }

    /// Re-key the frame's matches to a buffer whose first `dropped` bytes have
    /// been discarded, for the streaming encoder's compaction.
    ///
    /// The memo moves with the table. It describes a block that has already
    /// been encoded -- compaction happens between blocks -- so nothing will ask
    /// for it again, but shifting it rather than dropping it keeps
    /// [`sequences_for_block`](Self::sequences_for_block)'s monotonicity check
    /// meaningful across the boundary.
    pub(crate) fn shift_positions(&mut self, dropped: usize) {
        self.matcher.shift_positions(dropped);
        self.generated_for = self
            .generated_for
            .map(|(start, end)| (start.saturating_sub(dropped), end.saturating_sub(dropped)));
    }

    /// The long-distance matches of one block, running the matcher over it if
    /// that has not happened yet.
    ///
    /// Every block must reach the table, and each must reach it exactly once.
    /// Every block, because C reaches its raw and RLE decisions *after*
    /// building the sequence store (`zstd_compress.c:4136` and `:4463` both
    /// test `ZSTD_isRLE` on a store that is already built), so its table has
    /// seen every byte of the frame whatever each block turned into, and a
    /// block skipped here would leave a hole no later match could reach into.
    /// Exactly once, because the table's buckets rotate: a position filed
    /// twice evicts a real candidate.
    ///
    /// Exactly once is not the same as once per parse. `btultra2` parses the
    /// first block of a frame twice, and C generates the store before the
    /// block compressor that runs both passes, so both read the same matches.
    /// Asking for the same block twice here answers from what is already held
    /// rather than generating again, and asking for a different one generates.
    ///
    /// Takes no dictionary because no caller has one yet: the encoder refuses
    /// long-distance matching alongside a dictionary, and lifting that needs the
    /// *prefixed* block paths rather than anything here. [`LdmState`] itself
    /// searches a dictionary already; what is missing is a prefixed parser to
    /// hand the matches to.
    /// `dictionary` is the frame's dictionary content, empty when it has none.
    /// The memo and every caller speak in frame positions; the matcher speaks in
    /// the joint space that puts the dictionary before them. What comes back
    /// needs no conversion either way, since a literal run and an offset are
    /// both differences.
    pub(crate) fn sequences_for_block(
        &mut self,
        dictionary: &[u8],
        src: &[u8],
        block_start: usize,
    ) -> &[RawSequence] {
        let block = (block_start, src.len());
        if self.generated_for == Some(block) {
            return &self.sequences;
        }
        debug_assert!(
            match self.generated_for {
                Some((_, end)) => end == block_start,
                None => true,
            },
            "a block the long-distance table has already passed cannot be generated for again"
        );
        let dictionary_len = dictionary.len();
        self.sequences.clear();
        self.matcher.generate_sequences(
            LdmSource::with_dictionary(dictionary, src),
            block_start + dictionary_len,
            src.len() + dictionary_len,
            self.max_distance,
            &mut self.sequences,
        );
        self.generated_for = Some(block);
        &self.sequences
    }

    /// Hash a dictionary into the table, once, before the frame's first block.
    ///
    /// Separate from [`new`](Self::new) because the dictionary is borrowed for
    /// the call rather than held for the frame: the table keeps positions, not
    /// bytes. Separate from generation because it is a *different walk* over
    /// those bytes -- see [`LdmState::fill_from_dictionary`].
    pub(crate) fn load_dictionary(&mut self, dictionary: &[u8]) {
        self.matcher.fill_from_dictionary(dictionary);
    }
}

/// C's `ZSTD_ldm_blockCompress` (`zstd_ldm.c:681`): one block, with its
/// long-distance matches either laid down for the parser or offered to it.
///
/// Which of the two is C's `cParams->strategy >= ZSTD_btopt`. Below that the
/// matcher's output is *taken*: each match becomes a sequence, and the parser
/// only ever sees the bytes between two of them. At and above it the parser
/// searches every position as it always would, and each long-distance match is
/// one more candidate it prices against its own.
pub(crate) fn plan_sequences_for_contiguous_block_with_ldm_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    max_history_bytes: usize,
    state: &mut ContiguousBlockMatchState,
    ldm: &mut LdmFrameState,
) -> Result<()> {
    if params.parser_strategy.prices_long_distance_matches() {
        // C sets `ms->ldmSeqStore` and calls the ordinary block compressor,
        // so everything a block without long-distance matching does still
        // happens, in the same order, on the whole block at once. No segments,
        // and none of the table fills the other branch needs -- those exist to
        // catch a parser up over a gap it was never run on.
        let sequences = ldm.sequences_for_block(&[], src, block_start);
        return plan_sequences_for_contiguous_block_into_with_candidates(
            plan,
            src,
            block_start,
            repeat_offsets,
            params,
            max_history_bytes,
            state,
            Some(sequences),
        );
    }
    lay_down_long_distance_matches_into(
        plan,
        src,
        block_start,
        repeat_offsets,
        params,
        max_history_bytes,
        state,
        ldm,
    )
}

/// The `< btopt` half of [`plan_sequences_for_contiguous_block_with_ldm_into`]:
/// emit each long-distance match as a sequence, and run the ordinary parser on
/// the gaps between them.
///
/// [`LdmFrameState::segment`] is scratch: every parser resets the plan it
/// writes into, so each gap is parsed into that and appended. That is the one
/// structural difference from C, which appends into a single `seqStore`
/// because its block compressors take a `SeqStore_t*` and never clear it.
#[allow(clippy::too_many_arguments)]
fn lay_down_long_distance_matches_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    max_history_bytes: usize,
    state: &mut ContiguousBlockMatchState,
    ldm: &mut LdmFrameState,
) -> Result<()> {
    let block_end = src.len();
    ldm.sequences_for_block(&[], src, block_start);
    let LdmFrameState {
        sequences: ldm_sequences,
        segment,
        ..
    } = ldm;

    state.limit_update_after_long_match(block_start);
    let rep_window_low = block_start.saturating_sub(max_history_bytes);

    plan.reset_for_block(block_end - block_start);
    plan.repeat_offsets = repeat_offsets;
    // Literals the plan holds that some sequence has already claimed. The rest
    // are the run in front of whatever comes next, which is what a
    // long-distance sequence needs for its own literal length -- C's
    // `lastLLSize`, returned by each call to the block compressor.
    let mut literals_claimed = 0usize;
    let mut position = block_start;

    for index in 0..ldm_sequences.len() {
        let raw = ldm_sequences[index];
        let literal_length = raw.literal_length as usize;
        let match_length = raw.match_length as usize;
        // A store generated for this block cannot outrun it: the matcher stops
        // its forward extension at the range's end and drops the literals past
        // its last match. C carries a `maybeSplitSequence` for the case where
        // it can -- an external sequence producer's store, which this crate
        // has no way to supply -- and asserts this same bound when the store
        // is its own.
        debug_assert!(
            position + literal_length + match_length <= block_end,
            "a long-distance sequence ran past the block it was generated for"
        );

        state.limit_update_after_ldm_match(position);
        state.fill_fast_tables(src, position);
        plan_sequences_for_contiguous_segment_into(
            segment,
            &src[..position + literal_length],
            position,
            rep_window_low,
            plan.repeat_offsets,
            params,
            max_history_bytes,
            state,
            None,
        )?;
        literals_claimed += append_segment_plan(plan, segment);
        position += literal_length;

        let off_base = plan.repeat_offsets.update_explicit_offset(raw.offset);
        let trailing_literals = plan.literals.len() - literals_claimed;
        plan.push_stored_sequence(SequenceCommand {
            literal_length: trailing_literals as u32,
            offset_value: off_base,
            match_length: raw.match_length,
        });
        literals_claimed = plan.literals.len();
        position += match_length;
    }

    state.limit_update_after_ldm_match(position);
    state.fill_fast_tables(src, position);
    plan_sequences_for_contiguous_segment_into(
        segment,
        src,
        position,
        rep_window_low,
        plan.repeat_offsets,
        params,
        max_history_bytes,
        state,
        None,
    )?;
    append_segment_plan(plan, segment);
    Ok(())
}

/// Append one segment's parse to the block's plan, and report how many of the
/// appended literals its sequences claimed.
///
/// The remainder are the segment's trailing run, which belongs to whatever
/// sequence comes next rather than to this segment.
fn append_segment_plan(plan: &mut SequencePlan, segment: &SequencePlan) -> usize {
    plan.literals.extend_from_slice(&segment.literals);
    plan.sequences.extend_from_slice(&segment.sequences);
    plan.literal_codes.extend_from_slice(&segment.literal_codes);
    plan.offset_codes.extend_from_slice(&segment.offset_codes);
    plan.match_codes.extend_from_slice(&segment.match_codes);
    if plan.trace_enabled {
        plan.trace_match_sources
            .extend_from_slice(&segment.trace_match_sources);
        plan.trace_emissions
            .extend_from_slice(&segment.trace_emissions);
        plan.trace_row_searches
            .extend_from_slice(&segment.trace_row_searches);
        plan.trace_row_lazy_probes
            .extend_from_slice(&segment.trace_row_lazy_probes);
        #[cfg(test)]
        plan.trace_chain_searches
            .extend_from_slice(&segment.trace_chain_searches);
    }
    plan.repeat_offsets = segment.repeat_offsets;
    segment
        .sequences
        .iter()
        .map(|sequence| sequence.literal_length as usize)
        .sum()
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn plan_sequences_for_prefixed_contiguous_block(
    prefix: &[u8],
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    max_history_bytes: usize,
    state: &mut PrefixedBlockMatchState,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_for_prefixed_contiguous_block_into(
        &mut plan,
        prefix,
        src,
        block_start,
        repeat_offsets,
        params,
        max_history_bytes,
        state,
    )?;
    Ok(plan)
}

pub(crate) fn plan_sequences_for_prefixed_contiguous_block_into(
    plan: &mut SequencePlan,
    prefix: &[u8],
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    max_history_bytes: usize,
    state: &mut PrefixedBlockMatchState,
) -> Result<()> {
    // C runs this once per block in `ZSTD_buildSeqStore`, before the parser
    // sees the block and for every strategy.
    state.limit_update_after_long_match(block_start);
    let floors = prefixed_block_floors(
        prefix,
        block_start,
        src.len(),
        max_history_bytes,
        params.prefix_low_limit,
    );
    plan_sequences_for_prefixed_contiguous_segment_into(
        plan,
        prefix,
        src,
        block_start,
        floors,
        repeat_offsets,
        params,
        state,
        None,
    )?;
    state.insert_block_tail(src, block_start, src.len());
    Ok(())
}

/// The floors a prefixed block searches under, resolved once for the block.
///
/// One resolution per block, not per segment: C sets `window.lowLimit` once in
/// `ZSTD_window_enforceMaxDist` and every per-segment call reads it unchanged,
/// so a block cut up by long-distance matches still measures its floors from
/// where the block began. The contiguous side carries `rep_window_low` through
/// its segments for the same reason.
#[derive(Debug, Clone, Copy)]
struct PrefixedBlockFloors {
    match_floor: PrefixedMatchFloor,
    prefix_low: usize,
    source_low: usize,
    /// What bounds the repeat offsets carried in from the previous block, for
    /// the parsers that run without the prefix once it has retired.
    ///
    /// Measured from the block's *start* while `source_low` is measured from
    /// its end, which is not an inconsistency: C reads the same
    /// `window.dictLimit` for both and asks a different question of it. The
    /// match floor is an index, taken at the block's end
    /// (`ZSTD_getLowestPrefixIndex(ms, endIndex, ..)`); the repeat bound is a
    /// *distance*, `curr - windowLow` at the block's start. The contiguous side
    /// carries the same pair for the same reason.
    rep_window_low: usize,
    /// Whether no position of the prefix is reachable from anywhere in this
    /// block, which is C dropping the dictionary in
    /// [`prefixed_window_lows`]'s first branch.
    ///
    /// Set in the same branch that produces the floors so the two cannot
    /// disagree: a parser that skips the prefix must be the one whose floors
    /// already say the prefix is unreachable.
    ///
    /// C stops paying for the prefix here and this crate used not to. Both
    /// `ZSTD_checkDictValidity` and `ZSTD_window_enforceMaxDist` run once per
    /// block (`zstd_compress.c:4629`), the first clearing `dictMatchState` and
    /// the second catching `dictLimit` up to `lowLimit`, so
    /// `ZSTD_matchState_dictMode` returns `ZSTD_noDict` and every remaining
    /// block goes through the plain compressor. Staying on the prefixed parser
    /// searches a prefix the floors have already made unreachable, which on a
    /// 4 MiB body is 87% of the blocks at level 1 (`window_log` 19) and half at
    /// levels 3-8 (21).
    ///
    /// **Only `Fast` takes the switch, and the other four are not oversights.**
    ///
    /// `Fast` is free: its `src_finder` is keyed in source coordinates, so the
    /// plain parser reads the same table untouched, and its prefixed loop is
    /// the same four-position walk with the same step acceleration as the plain
    /// one. With the prefix unreachable the two agree position for position,
    /// which is why the frames come out byte-identical -- an explanation, not a
    /// coincidence, and [`retired_fast_prefix_matches_the_plain_parser`] pins
    /// it. Over 4 MiB at levels 1 and 2, same bytes out and **+20% to +35%**:
    /// `raw-dictionary` 2830 -> 3495 MiB/s and 2833 -> 3406,
    /// `trained-dictionary` 1042 -> 1403 and 968 -> 1220. Two measurement
    /// sessions under different load agreed to within 2%, the pairs
    /// interleaved and taken fastest-of-N; the byte counts are the control that
    /// says the two runs compressed the same thing.
    ///
    /// `DoubleFast` was measured and declined. Its prefixed and plain loops are
    /// genuinely different searches, so the switch moves the parse: at level 3
    /// over 4 MiB it buys 10-11% throughput and costs 5.2% of ratio on
    /// `raw-dictionary` (65872 -> 69310 bytes) and 0.72% on
    /// `trained-dictionary`. Both figures stay well under upstream's 78398, so
    /// this is not a parity gap being closed -- it is a divergence that was
    /// winning, and the ratio-first policy declines the trade. The blocks that
    /// move are 16-26 of 32, where the two parsers enter and leave the same
    /// repeating groove a block apart.
    ///
    /// `Row` was measured and declined, and it is **not** blocked on a table
    /// rebase -- an earlier version of this comment said it was, on the same
    /// reasoning that turned out to be wrong for `Fast`. `Row` files its source
    /// positions through the same `src_finder.insert_range` the contiguous
    /// state uses, so the plain parser reads the table it already filled and no
    /// rebase arises. What stops it is ratio. The two walks are not the same:
    /// the plain one is C's `ZSTD_compressBlock_lazy_row` for `ZSTD_noDict`,
    /// which retires a repeat offset wider than the window before parsing
    /// (`offsetSaved1`/`offsetSaved2`), while the prefixed one has no such
    /// clause because a dictionary makes those offsets reachable. Taking the
    /// switch moved 4 of 1690 baseline rows, every one `streamed-dict` at L5:
    /// three shrank (0.16-0.76%) and `wikipedia` grew **13.57%**, 10809 ->
    /// 12276, for **+0.707% overall**. Those rows already sit well under
    /// upstream, so this is the `DoubleFast` shape again -- a divergence that
    /// was winning -- and the ratio-first policy declines it.
    ///
    /// Those four rows are also the only place in the tree where `Row` meets a
    /// prefix *and* retires, which is worth knowing before pricing this again:
    /// a dictionary frame only reaches a row parser when the **CDict's own**
    /// cparams clear `ZSTD_resolveRowMatchFinderMode`'s `window_log > 14`, and
    /// both benchmark dictionaries land on exactly 14 -- 156 and 512 bytes both
    /// resolve through the `rSize <= 16 KB` row of C's table, checked against
    /// `ZSTD_getCParams` itself. They miss it by one bit, so levels 4-8 of both
    /// dictionary cases run `Chain`, not `Row`, in C as here, since C takes
    /// `useRowMatchFinder` from the CDict as well. A 16 KiB dictionary resolves
    /// to 17 and does reach `Row`, which is why the rows that moved above are
    /// the `streamed-dict` ones and not these.
    ///
    /// `Chain` and `BinaryTree` do key their tables in *virtual* coordinates
    /// (`prefix_len + position`), so for them the rebase objection is real: a
    /// position-masked table cannot be rebased by an arbitrary prefix length,
    /// and rebuilding one costs a full re-hash of the window. **But there is
    /// nothing on the other side of it.** Once retired, `low_limit` is
    /// `prefix_len + source_low`, which is at or above `prefix_len`, so the
    /// walk's `candidate >= low_limit` test can never admit a prefix position
    /// and the `is_prefix` arm is dead by construction. Counted over
    /// `raw-dictionary` at L6 over 4 MiB: **0 of 616311 candidate visits** in
    /// the 16 retired blocks took it, against 68 of 619364 in the 16 live ones.
    /// Every candidate already takes the source arm -- one predictable compare
    /// and a subtraction away from what the plain parser runs -- so switching
    /// parsers would save per-*position* work only, spread over the 6.3
    /// candidates each position visits.
    ///
    /// So the retirement dispatch is finished, not half-done, and the
    /// dictionary encode gap at levels 4-8 is somewhere else. `BENCHMARKS.md`
    /// records where it currently sits; it is the largest encode gap left.
    prefix_retired: bool,
}

fn prefixed_block_floors(
    prefix: &[u8],
    block_start: usize,
    block_end: usize,
    max_history_bytes: usize,
    prefix_low_limit: usize,
) -> PrefixedBlockFloors {
    let (prefix_low, source_low) = prefixed_window_lows(
        prefix.len(),
        block_start,
        block_end,
        max_history_bytes,
        prefix_low_limit,
    );
    // C picks between two floors per block, and `prefix_low` is already this
    // crate's answer to the question it picks on: while any of the prefix is
    // still inside the window, the pair above is C's `isDictionary` branch and
    // holds for every position. Once the prefix has aged out, the floor rises
    // to `curr - maxDist` at the position doing the looking, never below the
    // prefix it can no longer reach.
    //
    // Deciding this per block rather than per position is not a shortcut. C
    // decides it per block too, in `ZSTD_checkDictValidity`. Taking it from
    // `block_start` instead put it back inside the block, and the first block
    // of any buffer has `block_start == 0`, which makes the whole prefix
    // reachable however far past the window the frame has already run. Four
    // dictionary streaming tests said so at once.
    //
    // One known difference from C remains, in the value rather than the shape:
    // C also retires on `loadedDictEnd != window->dictLimit`, for
    // non-contiguous segments, which has no counterpart here.
    //
    // The boundary itself used to be off by one and no longer is; see
    // `prefixed_window_lows`. The comment that stood here gave as the reason
    // for leaving it that this crate's decoder bounds a dictionary match by the
    // window, so the boundary could not move without the live floor moving
    // with it. Both halves were false. The floor has since moved on its own,
    // and the decoder reads every one of C's frames that reference a dictionary
    // past the declared window — checked by running them, not by reading.
    //
    // The fast and double-fast parsers keep the block-constant pair either way,
    // as they do in the contiguous case.
    let prefix_retired = prefix_low >= prefix.len();
    let match_floor = if prefix_retired {
        PrefixedMatchFloor::reaching(
            prefix.len(),
            MatchFloor::reaching(
                prefix
                    .len()
                    .saturating_add(block_start.saturating_sub(max_history_bytes)),
                max_history_bytes,
            ),
        )
    } else {
        PrefixedMatchFloor::fixed(prefix_low, source_low)
    };
    PrefixedBlockFloors {
        match_floor,
        prefix_low,
        source_low,
        rep_window_low: block_start.saturating_sub(max_history_bytes),
        prefix_retired,
    }
}

/// One run of a prefixed parser over `[segment_start, src.len())`.
///
/// A block without long-distance matching is a single segment, which is what
/// [`plan_sequences_for_prefixed_contiguous_block_into`] calls this as. With it,
/// a block is one segment per gap between long-distance matches, and `floors`
/// is the one thing that does not follow the segment.
#[allow(clippy::too_many_arguments)]
fn plan_sequences_for_prefixed_contiguous_segment_into(
    plan: &mut SequencePlan,
    prefix: &[u8],
    src: &[u8],
    block_start: usize,
    floors: PrefixedBlockFloors,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    state: &mut PrefixedBlockMatchState,
    ldm: Option<&[RawSequence]>,
) -> Result<()> {
    debug_assert!(
        ldm.is_none() || matches!(state.inner, PrefixedBlockMatchStateInner::BinaryTree { .. }),
        "only the binary tree carries a parser that prices long-distance matches"
    );
    let PrefixedBlockFloors {
        match_floor,
        prefix_low,
        source_low,
        rep_window_low,
        prefix_retired,
    } = floors;
    // C retires the repeat offsets carried in from the previous block against
    // this block's window, in `ZSTD_compressBlock_lazy_generic` and its
    // siblings, under `if (dictMode == ZSTD_noDict)`. That guard reads as "only
    // when there is no dictionary" and means something narrower: `dictMode` is
    // recomputed per block from `ms->dictMatchState`, which
    // `ZSTD_checkDictValidity` has just set to NULL if the block ends past
    // `maxDist + loadedDictEnd`. A block whose prefix has retired *is* C's
    // no-dictionary case, and takes the clamp.
    //
    // Only `Fast` switches parsers on retirement here, so for every other one
    // the clamp has to happen on this side of the dispatch. Without it an
    // offset found while the prefix was live -- which may legally exceed
    // `maxDist`, since the decoder holds the dictionary outside the window --
    // survives in `offset_1` into a block that can no longer reach it, and the
    // parser repeats it. That is a frame this crate's own decoder rejects, and
    // `dictionary_encode_roundtrip` found one: a 45-byte dictionary at
    // `window_log` 10, offset 1025 stored at source 989 and repeated at 1025.
    let (repeat_offsets, saved1, saved2) = if prefix_retired {
        let (clamped, _, _, saved1, saved2) = invalidate_no_dict_repeat_offsets(
            repeat_offsets,
            block_start,
            match_floor.at(block_start).1,
        );
        (clamped, saved1, saved2)
    } else {
        (repeat_offsets, 0, 0)
    };
    let restore = |plan: &mut SequencePlan| {
        if prefix_retired {
            plan.repeat_offsets =
                restore_invalidated_repeat_offsets(plan.repeat_offsets, saved1, saved2);
        }
    };
    let result = match &mut state.inner {
        PrefixedBlockMatchStateInner::Row {
            prefix_finder,
            src_finder,
            mode,
        } => match mode {
            PrefixMatchMode::ExtDict => {
                let prefix_refs = [prefix];
                let prefix_chain = PrefixChain::new(&prefix_refs)?
                    .expect("prefixed contiguous planning requires a non-empty prefix");
                plan_sequences_row_ext_dict_from_into(
                    plan,
                    src,
                    block_start,
                    prefix_chain,
                    repeat_offsets,
                    params,
                    match_floor,
                    prefix_finder.as_ref(),
                    src_finder,
                )
            }
            PrefixMatchMode::DictMatchState => plan_sequences_row_dict_match_state_from_into(
                plan,
                src,
                block_start,
                prefix,
                repeat_offsets,
                params,
                match_floor,
                prefix_finder.as_ref(),
                src_finder,
            ),
        },
        PrefixedBlockMatchStateInner::Chain {
            prefix_finder,
            src_finder,
            mode,
            ..
        } => {
            let prefix_refs = [prefix];
            let prefix_chain = PrefixChain::new(&prefix_refs)?
                .expect("prefixed contiguous planning requires a non-empty prefix");
            if *mode == PrefixMatchMode::DictMatchState {
                plan_sequences_chain_dict_match_state_with_prefix_chain_from_into(
                    plan,
                    src,
                    block_start,
                    prefix_chain,
                    repeat_offsets,
                    params,
                    match_floor,
                    prefix_finder.as_ref(),
                    src_finder,
                )
            } else {
                plan_sequences_chain_ext_dict_with_prefix_chain_from_into(
                    plan,
                    src,
                    block_start,
                    prefix_chain,
                    repeat_offsets,
                    params,
                    match_floor,
                    src_finder,
                )
            }
        }
        // Once the prefix has retired, this is the plain no-dictionary parser
        // reading the same table. See [`prefix_retired`](PrefixedBlockFloors::prefix_retired)
        // for why only `Fast` takes the switch.
        PrefixedBlockMatchStateInner::Fast {
            prefix_finder,
            src_finder,
            mode,
            prepared,
        } => {
            if prefix_retired {
                plan_sequences_fast_without_prefix_from_into(
                    plan,
                    src,
                    block_start,
                    repeat_offsets,
                    params,
                    source_low,
                    rep_window_low,
                    src_finder,
                )
            } else {
                plan_sequences_fast_with_prefix_from_into(
                    plan,
                    src,
                    block_start,
                    prefix,
                    repeat_offsets,
                    params,
                    prefix_low,
                    source_low,
                    prefix_finder.as_deref(),
                    src_finder,
                    *mode,
                    prepared.as_deref(),
                )
            }
        }
        PrefixedBlockMatchStateInner::WideFast {
            prefix_finder,
            src_finder,
            mode,
            prepared,
        } => {
            if prefix_retired {
                plan_sequences_fast_without_prefix_from_into(
                    plan,
                    src,
                    block_start,
                    repeat_offsets,
                    params,
                    source_low,
                    rep_window_low,
                    src_finder,
                )
            } else {
                plan_sequences_fast_with_prefix_from_into(
                    plan,
                    src,
                    block_start,
                    prefix,
                    repeat_offsets,
                    params,
                    prefix_low,
                    source_low,
                    prefix_finder.as_deref(),
                    src_finder,
                    *mode,
                    prepared.as_deref(),
                )
            }
        }
        PrefixedBlockMatchStateInner::DoubleFast {
            prefix_finder,
            src_finder,
            mode,
            prepared,
        } => plan_sequences_double_fast_with_prefix_from_into(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            prefix_low,
            source_low,
            prefix_finder.as_deref(),
            src_finder,
            *mode,
            prepared.as_deref(),
        ),
        PrefixedBlockMatchStateInner::WideDoubleFast {
            prefix_finder,
            src_finder,
            mode,
            prepared,
        } => plan_sequences_double_fast_with_prefix_from_into(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            prefix_low,
            source_low,
            prefix_finder.as_deref(),
            src_finder,
            *mode,
            prepared.as_deref(),
        ),
        PrefixedBlockMatchStateInner::BinaryTree {
            prefix_finder,
            src_finder,
            mode,
        } => plan_sequences_binary_tree_with_prefix_from_into(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            match_floor,
            prefix_finder.as_ref(),
            src_finder,
            *mode,
            ldm,
        ),
    };
    restore(plan);
    result
}

/// C's `ZSTD_ldm_blockCompress` for a frame that carries a dictionary: the
/// prefixed twin of [`plan_sequences_for_contiguous_block_with_ldm_into`].
///
/// The split is the same one C makes on `cParams->strategy >= ZSTD_btopt`, and
/// for the same reason -- below it the matcher's output is taken and the parser
/// only ever sees the gaps, at and above it the parser searches everything and
/// prices each long-distance match as one more candidate.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_sequences_for_prefixed_contiguous_block_with_ldm_into(
    plan: &mut SequencePlan,
    prefix: &[u8],
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    max_history_bytes: usize,
    state: &mut PrefixedBlockMatchState,
    ldm: &mut LdmFrameState,
) -> Result<()> {
    if params.parser_strategy.prices_long_distance_matches() {
        state.limit_update_after_long_match(block_start);
        let floors = prefixed_block_floors(
            prefix,
            block_start,
            src.len(),
            max_history_bytes,
            params.prefix_low_limit,
        );
        let sequences = ldm.sequences_for_block(prefix, src, block_start);
        plan_sequences_for_prefixed_contiguous_segment_into(
            plan,
            prefix,
            src,
            block_start,
            floors,
            repeat_offsets,
            params,
            state,
            Some(sequences),
        )?;
        state.insert_block_tail(src, block_start, src.len());
        return Ok(());
    }
    lay_down_long_distance_matches_prefixed_into(
        plan,
        prefix,
        src,
        block_start,
        repeat_offsets,
        params,
        max_history_bytes,
        state,
        ldm,
    )
}

/// The `< btopt` half of the above: emit each long-distance match as a
/// sequence, and run the ordinary prefixed parser on the gaps between them.
///
/// The contiguous twin carries the commentary; the two differences here are that
/// the floors are resolved once for the block rather than a single
/// `rep_window_low`, and that the whole-block insert at the end is this state's
/// own [`insert_block_tail`](PrefixedBlockMatchState::insert_block_tail) rather
/// than nothing at all.
#[allow(clippy::too_many_arguments)]
fn lay_down_long_distance_matches_prefixed_into(
    plan: &mut SequencePlan,
    prefix: &[u8],
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    max_history_bytes: usize,
    state: &mut PrefixedBlockMatchState,
    ldm: &mut LdmFrameState,
) -> Result<()> {
    let block_end = src.len();
    state.limit_update_after_long_match(block_start);
    let floors = prefixed_block_floors(
        prefix,
        block_start,
        block_end,
        max_history_bytes,
        params.prefix_low_limit,
    );
    ldm.sequences_for_block(prefix, src, block_start);
    let LdmFrameState {
        sequences: ldm_sequences,
        segment,
        ..
    } = ldm;

    plan.reset_for_block(block_end - block_start);
    plan.repeat_offsets = repeat_offsets;
    let mut literals_claimed = 0usize;
    let mut position = block_start;

    for index in 0..ldm_sequences.len() {
        let raw = ldm_sequences[index];
        let literal_length = raw.literal_length as usize;
        let match_length = raw.match_length as usize;
        debug_assert!(
            position + literal_length + match_length <= block_end,
            "a long-distance sequence ran past the block it was generated for"
        );

        state.limit_update_after_ldm_match(position);
        state.fill_fast_tables(src, position);
        plan_sequences_for_prefixed_contiguous_segment_into(
            segment,
            prefix,
            &src[..position + literal_length],
            position,
            floors,
            plan.repeat_offsets,
            params,
            state,
            None,
        )?;
        literals_claimed += append_segment_plan(plan, segment);
        position += literal_length;

        let off_base = plan.repeat_offsets.update_explicit_offset(raw.offset);
        let trailing_literals = plan.literals.len() - literals_claimed;
        plan.push_stored_sequence(SequenceCommand {
            literal_length: trailing_literals as u32,
            offset_value: off_base,
            match_length: raw.match_length,
        });
        literals_claimed = plan.literals.len();
        position += match_length;
    }

    state.limit_update_after_ldm_match(position);
    state.fill_fast_tables(src, position);
    plan_sequences_for_prefixed_contiguous_segment_into(
        segment,
        prefix,
        src,
        position,
        floors,
        plan.repeat_offsets,
        params,
        state,
        None,
    )?;
    append_segment_plan(plan, segment);
    state.insert_block_tail(src, block_start, block_end);
    Ok(())
}

/// Where a match may start when part of the history is a separate prefix: one
/// floor inside the prefix, one inside the source.
///
/// Measured from `block_end`, which is what C's fast and double-fast parsers do
/// (`ZSTD_getLowestPrefixIndex(ms, endIndex, ..)`) and what those two still take
/// from here. Everything else now takes [`PrefixedMatchFloor`] instead, and
/// uses this only to decide which of C's two branches applies: the prefix is
/// still inside the window exactly when `prefix_low < prefix_len`, which is the
/// `else` arm below.
///
/// Measuring from the block's start let a block reach a further `block_size`
/// back, and since the frame declares only the window, the decoder rejected the
/// result. On a 4 MiB body against a trained dictionary at level 2 that was an
/// offset of 1142716 inside a 1048576-byte window — a frame this crate could not
/// read back.
///
/// `block_start` caps the source floor for the same reason it does there: a
/// block wider than its own window would otherwise push the floor past the
/// bytes being encoded and underflow the branchless in-window tests.
fn prefixed_window_lows(
    prefix_len: usize,
    block_start: usize,
    block_end: usize,
    history_limit: usize,
    prefix_low_limit: usize,
) -> (usize, usize) {
    let trim = prefix_len
        .saturating_add(block_end)
        .saturating_sub(history_limit);
    if trim > prefix_len {
        // The prefix has aged out of the window entirely, so the floor lands
        // inside the source.
        //
        // Strictly greater, matching C's `blockEndIdx > loadedDictEnd + maxDist`
        // (`zstd_compress_internal.h:1315`). `>=` retires one byte early, which
        // sounds like nothing and is not: the window is fitted to the source as
        // `highbit32(srcSize - 1) + 1`, so a source that is an exact power of
        // two gets a window of exactly itself and lands on this boundary at the
        // *first* block. It retired the dictionary before a single position had
        // been searched — 3.73x upstream at 4 KiB against 0.91x at 3 KiB.
        (prefix_len, (trim - prefix_len).min(block_start))
    } else {
        // While the prefix is live, *all* of it is reachable. C does the same
        // and it is the whole point of a dictionary match state: an offset into
        // the dictionary may exceed `Window_Size`, because the decoder holds
        // the dictionary separately from the window.
        //
        // Trimming to the window instead cost 3.5x. The window is fitted to the
        // source, so a small source got a window no larger than itself and
        // retired a dictionary many times its size before using it — traced as
        // zero dictionary match sources at every level from 3 to 19. C keeps
        // the dictionary until `blockEndIdx > loadedDictEnd + maxDist`, which
        // is the branch above.
        //
        // `prefix_low_limit` rather than zero: see its definition. The decoder
        // is handed the dictionary's content, and the prefix here starts two
        // bytes earlier than that.
        (prefix_low_limit.min(prefix_len), 0)
    }
}

fn single_non_empty_prefix<'a>(
    prefixes: &'a [&'a [u8]],
) -> core::result::Result<Option<&'a [u8]>, ()> {
    let mut single = None;
    for prefix in prefixes.iter().copied().filter(|prefix| !prefix.is_empty()) {
        if single.replace(prefix).is_some() {
            return Err(());
        }
    }
    Ok(single)
}
