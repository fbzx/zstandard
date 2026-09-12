use super::*;

/// C's MINMATCH for the optimal parser repcode check is 3, which is lower
/// than the regular `MIN_MATCH = 4`. This allows the optimal parser to
/// capture short repcode matches that compress very cheaply.
const OPT_REPCODE_MIN_MATCH: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OptimalPriceType {
    Predefined,
    Dynamic,
}

#[derive(Debug, Clone)]
pub(crate) struct OptimalPriceModel<const IS_ULTRA: bool> {
    pub(crate) price_type: OptimalPriceType,
    pub(crate) ll_freq: [u32; 36],
    pub(crate) ml_freq: [u32; 53],
    pub(crate) of_freq: [u32; 32],
    pub(crate) ll_sum: u32,
    pub(crate) ml_sum: u32,
    pub(crate) of_sum: u32,
    pub(crate) lit_freq: [u32; 256],
    pub(crate) lit_sum: u32,
    pub(crate) ll_sum_base_price: u32,
    pub(crate) ml_sum_base_price: u32,
    pub(crate) of_sum_base_price: u32,
    pub(crate) lit_sum_base_price: u32,
    /// [`match_length_price`](Self::match_length_price) for every match
    /// length code, refreshed by [`set_base_prices`](Self::set_base_prices).
    ///
    /// The optimal parser prices each candidate at every length it can take,
    /// which is where most of a level-19 encode goes, and the length's share
    /// of the price depends on nothing but the code and the current
    /// statistics. The statistics change only through `set_base_prices`, so
    /// the 53 values are recomputed there once per sequence rather than once
    /// per length per candidate. Read only under `Dynamic` pricing.
    ml_price_by_code: [u32; 53],
    /// Penalize long offsets (off_code >= 20). True for btopt/btultra,
    /// false for btultra2 (matching C's optLevel < 2 check).
    pub(crate) long_offset_penalty: bool,
    /// C's `ZSTD_compressedLiterals`. False stores literals verbatim, so the
    /// model prices one at the eight bits it will occupy and stops keeping
    /// statistics it would never read.
    pub(crate) compressed_literals: bool,
}

/// C's ZSTD_MAX_PRICE sentinel — large enough to lose every comparison
/// but small enough that `MAX_PRICE + step_cost` cannot overflow u32.
const MAX_PRICE: u32 = 1 << 30;

/// Compact DP node matching C zstd's `ZSTD_optimal_t` layout (28 bytes).
/// All position/length fields use `u32` since values never exceed the DP
/// horizon (4096).
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub(crate) struct OptimalPathNode {
    pub(crate) price: u32,
    pub(crate) off: u32,
    pub(crate) mlen: u32,
    pub(crate) litlen: u32,
    pub(crate) reps: [u32; 3],
}

impl Default for OptimalPathNode {
    #[inline(always)]
    fn default() -> Self {
        Self {
            price: MAX_PRICE,
            off: 0,
            mlen: 0,
            litlen: 0,
            reps: [0; 3],
        }
    }
}

impl OptimalPathNode {
    pub(crate) const UNSET: Self = Self {
        price: MAX_PRICE,
        off: 0,
        mlen: 0,
        litlen: 0,
        reps: [0; 3],
    };
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OptimalMatchCandidate {
    pub(crate) offset_value: u32,
    pub(crate) length: u32,
}

/// Where in the block the long-distance match the cursor last passed sits,
/// and how far it reaches.
#[derive(Debug, Clone, Copy)]
struct LdmCandidateSpan {
    start: u32,
    end: u32,
    offset: u32,
}

/// One block's long-distance matches, offered to the optimal parser as
/// candidates it may take or leave.
///
/// C's `ZSTD_optLdm_t` (`zstd_opt.c:912`). This is the other half of long-
/// distance matching: below `btopt` the matcher's output is laid down and the
/// parser only sees the gaps, while here the parser searches every position as
/// it always would and a long-distance match is one more candidate competing
/// on price. It wins only where it is longer than anything the window-sized
/// tables can reach, which is the case it exists for.
///
/// The cursor is over a borrowed slice rather than the matcher's own, because
/// advancing it consumes nothing: C copies the whole `RawSeqStore_t` into
/// `optLdm` for the same reason, and `btultra2` relies on it by parsing the
/// first block twice from the same matches.
struct LdmCandidates<'a> {
    cursor: RawSequenceCursor<'a>,
    /// `None` is C's pair of `UINT_MAX` sentinels: no match is on offer, and
    /// no position can be past the end of one.
    span: Option<LdmCandidateSpan>,
}

impl<'a> LdmCandidates<'a> {
    /// C's three lines of `optLdm` setup at `zstd_opt.c:1107`, including the
    /// first `ZSTD_opt_getNextMatchAndUpdateSeqStore` that arms the first
    /// match before the parse loop starts.
    fn new(sequences: &'a [RawSequence], block_len: u32) -> Self {
        let mut candidates = Self {
            cursor: RawSequenceCursor::new(sequences),
            span: None,
        };
        candidates.arm_next_match(0, block_len);
        candidates
    }

    /// C's `ZSTD_opt_getNextMatchAndUpdateSeqStore` (`zstd_opt.c:940`): work
    /// out where the cursor's match sits in the block, then step the cursor
    /// over it.
    ///
    /// Stepping over it here, before it has been offered anywhere, is why a
    /// block's *last* match is never offered: the cursor lands past the end of
    /// the store, and [`Self::offer`] declines to look at a span once that has
    /// happened. The exception is a match the block cut short, where the
    /// cursor stops inside it and it is still on offer -- which a store
    /// generated for this block alone never produces. That is upstream's
    /// behaviour rather than an economy taken here.
    fn arm_next_match(&mut self, position_in_block: u32, block_bytes_remaining: u32) {
        let Some(current) = self.cursor.current() else {
            self.span = None;
            return;
        };
        let block_end = position_in_block + block_bytes_remaining;
        let consumed = self.cursor.offset_in_sequence();
        debug_assert!(consumed <= current.literal_length + current.match_length);
        let literals_remaining = current.literal_length.saturating_sub(consumed);
        let match_remaining = if literals_remaining == 0 {
            current.match_length - (consumed - current.literal_length)
        } else {
            current.match_length
        };

        // More literals than the block has left: nothing of this match lands
        // inside it.
        if literals_remaining >= block_bytes_remaining {
            self.span = None;
            self.cursor.skip_bytes(block_bytes_remaining as usize);
            return;
        }

        let start = position_in_block + literals_remaining;
        let end = start + match_remaining;
        if end > block_end {
            // The match runs past the block; only the part inside it is on
            // offer, and the cursor stops at the block's end so the rest is
            // still there for the next block.
            self.span = Some(LdmCandidateSpan {
                start,
                end: block_end,
                offset: current.offset,
            });
            self.cursor
                .skip_bytes((block_end - position_in_block) as usize);
        } else {
            self.span = Some(LdmCandidateSpan {
                start,
                end,
                offset: current.offset,
            });
            self.cursor
                .skip_bytes((literals_remaining + match_remaining) as usize);
        }
    }

    /// C's `ZSTD_optLdm_processMatchCandidate` (`zstd_opt.c:1024`): offer the
    /// long-distance match covering `position_in_block`, if there is one, to a
    /// list of candidates the parser has just searched for itself.
    fn offer(
        &mut self,
        candidates: &mut Vec<OptimalMatchCandidate>,
        position_in_block: u32,
        block_bytes_remaining: u32,
        min_match: u32,
    ) {
        if self.cursor.current().is_none() {
            return;
        }
        if let Some(span) = self.span {
            if position_in_block >= span.end {
                // The parser does not stop on match boundaries, so it usually
                // arrives some way past the end of the match it just left.
                let overshoot = position_in_block - span.end;
                if overshoot > 0 {
                    self.cursor.skip_bytes(overshoot as usize);
                }
                self.arm_next_match(position_in_block, block_bytes_remaining);
            }
        }
        self.add_match(candidates, position_in_block, min_match);
    }

    /// C's `ZSTD_optLdm_maybeAddMatch` (`zstd_opt.c:996`).
    ///
    /// The candidate list is ordered by increasing length and the parser reads
    /// its last entry as the longest, so a long-distance match that is not
    /// longer than what the tables already found is dropped rather than
    /// inserted.
    fn add_match(
        &self,
        candidates: &mut Vec<OptimalMatchCandidate>,
        position_in_block: u32,
        min_match: u32,
    ) {
        let Some(span) = self.span else {
            return;
        };
        if position_in_block < span.start || position_in_block >= span.end {
            return;
        }
        // What is left of the match from here on. A match the parser entered
        // partway through is shorter than the matcher found it, and may now be
        // too short to be worth a sequence at all.
        let length = span.end - position_in_block;
        if length < min_match {
            return;
        }
        if candidates.len() >= OPT_NUM {
            return;
        }
        if let Some(last) = candidates.last() {
            if length <= last.length {
                return;
            }
        }
        candidates.push(OptimalMatchCandidate {
            // C: `OFFSET_TO_OFFBASE`, an explicit offset. Never classified
            // against the repeat offsets, even where it would match one.
            offset_value: span.offset + 3,
            length,
        });
    }
}

pub(crate) fn plan_sequences_binary_tree_without_prefix_into(
    plan: &mut SequencePlan,
    src: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<()> {
    let mut finder = BinaryTreeFinder::new(params.hash_bits, params.chain_log, params.min_match)
        .with_window_log(params.window_log);
    plan_sequences_binary_tree_without_prefix_from_into(
        plan,
        src,
        0,
        repeat_offsets,
        params,
        // One block covering the whole input, so the whole input is reachable.
        MatchFloor::fixed(0),
        &mut finder,
        None,
    )
}

/// `ldm` is the block's long-distance matches, for the optimal parsers to
/// price against their own. Only they can take it: `btlazy2` is below C's
/// `ZSTD_btopt` threshold and reaches this through the same binary tree, but
/// its long-distance matches are laid down before it ever runs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_sequences_binary_tree_without_prefix_from_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    match_floor: MatchFloor,
    finder: &mut BinaryTreeFinder,
    ldm: Option<&[RawSequence]>,
) -> Result<()> {
    match params.parser_strategy {
        ParserStrategy::BinaryTreeOpt => plan_sequences_optimal_without_prefix_into::<false>(
            plan,
            src,
            block_start,
            repeat_offsets,
            params,
            match_floor,
            finder,
            ldm,
        ),
        ParserStrategy::BinaryTreeUltra => plan_sequences_optimal_without_prefix_into::<true>(
            plan,
            src,
            block_start,
            repeat_offsets,
            params,
            match_floor,
            finder,
            ldm,
        ),
        _ => {
            debug_assert!(
                ldm.is_none(),
                "only the optimal parsers price long-distance matches"
            );
            plan_sequences_without_prefix_from_into(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                match_floor,
                finder,
            )
        }
    }
}

pub(crate) fn plan_sequences_binary_tree_with_prefix_into(
    plan: &mut SequencePlan,
    src: &[u8],
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<()> {
    let prefix_refs = [prefix];
    plan_sequences_binary_tree_with_prefixes_into(plan, src, &prefix_refs, repeat_offsets, params)
}

pub(crate) fn plan_sequences_binary_tree_with_prefixes_into(
    plan: &mut SequencePlan,
    src: &[u8],
    prefixes: &[&[u8]],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<()> {
    let prefix_chain = PrefixChain::new(prefixes)?
        .expect("binary-tree prefix planning requires a non-empty prefix");
    let mut prefix_finder =
        BinaryTreeFinder::new(params.hash_bits, params.chain_log, params.min_match)
            .with_window_log(params.window_log);
    prefix_finder.insert_prefix_chain(prefix_chain, src, params.search_depth);
    let mut src_finder =
        BinaryTreeFinder::new(params.hash_bits, params.chain_log, params.min_match)
            .with_window_log(params.window_log);
    plan_sequences_binary_tree_with_prefix_chain_from_into(
        plan,
        src,
        0,
        prefix_chain,
        repeat_offsets,
        params,
        PrefixedMatchFloor::fixed(0, 0),
        &prefix_finder,
        &mut src_finder,
        PrefixMatchMode::ExtDict,
        None,
    )
}

pub(crate) fn plan_sequences_binary_tree_with_prefix_from_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    match_floor: PrefixedMatchFloor,
    prefix_finder: &BinaryTreeFinder,
    src_finder: &mut BinaryTreeFinder,
    mode: PrefixMatchMode,
    ldm: Option<&[RawSequence]>,
) -> Result<()> {
    let prefix_refs = [prefix];
    let prefix_chain = PrefixChain::new(&prefix_refs)?
        .expect("binary-tree prefix planning requires a non-empty prefix");
    plan_sequences_binary_tree_with_prefix_chain_from_into(
        plan,
        src,
        block_start,
        prefix_chain,
        repeat_offsets,
        params,
        match_floor,
        prefix_finder,
        src_finder,
        mode,
        ldm,
    )
}

pub(crate) fn plan_sequences_binary_tree_with_prefix_chain_from_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    prefix_chain: PrefixChain<'_>,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    match_floor: PrefixedMatchFloor,
    prefix_finder: &BinaryTreeFinder,
    src_finder: &mut BinaryTreeFinder,
    mode: PrefixMatchMode,
    ldm: Option<&[RawSequence]>,
) -> Result<()> {
    match params.parser_strategy {
        ParserStrategy::BinaryTreeOpt | ParserStrategy::BinaryTreeUltra => {
            // C's byCopyingCDict path: dictMode = ZSTD_extDict, NO separate
            // dict match state (ms->dictMatchState = NULL), Phase 2 dict BST
            // search is SKIPPED. One unified BST with CDict params, dict
            // entries pre-loaded from ZSTD_loadDictionaryContent, source
            // entries inserted during compression. Match bytes in dict region
            // accessed via dictBase + matchIndex (here: contiguous virtual_buf).
            //
            // When source overrides are set (DictMatchState / byAttachingCDict
            // path), use separate source-dimensioned BST + dict BST (Phase 2).
            let has_source_overrides = params.dictionary_attaches;
            // Both trees below embed the prefix. They may only be carried over
            // from an earlier block while that prefix is unchanged — true for a
            // dictionary, false for streaming history, which grows per block.
            if plan.opt_prefix_len != Some(prefix_chain.len()) {
                plan.opt_source_bt = None;
                plan.opt_dict_bt = None;
                plan.opt_prefix_len = Some(prefix_chain.len());
            }
            // No `with_window_log` here, unlike the dict tree below, and that is
            // load-bearing rather than an oversight: this tree is only ever
            // driven through `update_tree_unified`, whose floor is a constant,
            // so `window_log` would be dead on it. It is worth knowing anyway,
            // because the field stays on its `31` "unbounded" sentinel — anyone
            // probing whether a window-derived floor binds on this finder will
            // measure `target - (1 << 31)`, get zero every time, and read an
            // inert experiment as evidence that the floor does not matter. It
            // cost two A/B sweeps before the sentinel turned up in a panic
            // message. Set it in the same change that gives this tree a floor
            // that reads it, and not before.
            let mut source_bt = plan.opt_source_bt.take().unwrap_or_else(|| {
                let mut bt =
                    BinaryTreeFinder::new(params.hash_bits, params.chain_log, params.min_match);
                bt.zero_tables();
                // extDict path: pre-populate BST with dict entries matching
                // C's byCopyingCDict which copies CDict tables into ms.
                if !has_source_overrides {
                    let dict_search_depth = params.search_depth.max(64);
                    bt.insert_prefix_into_unified(
                        prefix_chain,
                        dict_search_depth,
                        params.window_log,
                    );
                }
                bt
            });
            // Dict BST: only needed for the DictMatchState (Phase 2) path.
            if plan.opt_dict_bt.is_none() {
                // The dictionary's own geometry, not the applied one: this is
                // C's `dms`, and `ZSTD_insertBtAndGetAllMatches` bounds its
                // dictionary phase with `dmsCParams->chainLog`.
                let mut bt = BinaryTreeFinder::new(
                    params.dictionary_hash_bits(),
                    params.dictionary_chain_log(),
                    params.min_match,
                )
                .with_window_log(params.dictionary_window_log());
                bt.zero_tables();
                let dms_search_depth = params.search_depth.max(64);
                bt.insert_prefix_into_unified(
                    prefix_chain,
                    dms_search_depth,
                    params.dictionary_window_log(),
                );
                plan.opt_dict_bt = Some(Arc::new(bt));
            }
            let dict_bt = plan.opt_dict_bt.as_ref().unwrap().clone();
            let result = if matches!(params.parser_strategy, ParserStrategy::BinaryTreeUltra) {
                plan_sequences_optimal_with_prefix_two_phase_into::<true>(
                    plan,
                    src,
                    block_start,
                    prefix_chain,
                    repeat_offsets,
                    params,
                    match_floor,
                    &mut source_bt,
                    &dict_bt,
                    ldm,
                )
            } else {
                plan_sequences_optimal_with_prefix_two_phase_into::<false>(
                    plan,
                    src,
                    block_start,
                    prefix_chain,
                    repeat_offsets,
                    params,
                    match_floor,
                    &mut source_bt,
                    &dict_bt,
                    ldm,
                )
            };
            plan.opt_source_bt = Some(source_bt);
            result
        }
        _ => {
            debug_assert!(
                ldm.is_none(),
                "only the optimal parsers price long-distance matches"
            );
            plan_sequences_with_prefix_chain_from_into(
                plan,
                src,
                block_start,
                prefix_chain,
                repeat_offsets,
                params,
                match_floor,
                prefix_finder,
                src_finder,
                mode,
            )
        }
    }
}

impl<const IS_ULTRA: bool> OptimalPriceModel<IS_ULTRA> {
    pub(crate) fn new(
        src: &[u8],
        dict_seed: Option<&DictionaryPriceSeed>,
        long_offset_penalty: bool,
        compressed_literals: bool,
    ) -> Self {
        // When a dictionary seed is available, use dictionary-derived
        // frequencies for ALL symbol types (matching C's ZSTD_rescaleFreqs).
        // The dictionary path always uses dynamic pricing.
        if let Some(seed) = dict_seed {
            let mut model = Self {
                price_type: OptimalPriceType::Dynamic,
                ll_freq: seed.ll_freq,
                ml_freq: seed.ml_freq,
                of_freq: seed.of_freq,
                ll_sum: seed.ll_sum,
                ml_sum: seed.ml_sum,
                of_sum: seed.of_sum,
                // C derives these from the dictionary's Huffman table under
                // `if (compressedLiterals)` and leaves them zeroed otherwise
                // (`zstd_opt.c:163`). Nothing reads them in that case, but a
                // model carrying statistics for a symbol class it prices at a
                // flat rate is a trap for the next reader.
                lit_freq: if compressed_literals {
                    seed.lit_freq
                } else {
                    [0; 256]
                },
                lit_sum: if compressed_literals { seed.lit_sum } else { 0 },
                ll_sum_base_price: 0,
                ml_sum_base_price: 0,
                of_sum_base_price: 0,
                lit_sum_base_price: 0,
                ml_price_by_code: [0; 53],
                long_offset_penalty,
                compressed_literals,
            };
            model.set_base_prices();
            return model;
        }

        let mut model = Self {
            price_type: if src.len() <= OPT_PREDEFINED_THRESHOLD {
                OptimalPriceType::Predefined
            } else {
                OptimalPriceType::Dynamic
            },
            ll_freq: optimal_base_ll_frequencies(),
            ml_freq: [1; 53],
            of_freq: optimal_base_of_frequencies(),
            ll_sum: 0,
            ml_sum: 53,
            of_sum: 0,
            lit_freq: [0; 256],
            lit_sum: 0,
            ll_sum_base_price: 0,
            ml_sum_base_price: 0,
            of_sum_base_price: 0,
            lit_sum_base_price: 0,
            ml_price_by_code: [0; 53],
            long_offset_penalty,
            compressed_literals,
        };

        model.ll_sum = model.ll_freq.iter().sum();
        model.of_sum = model.of_freq.iter().sum();

        // Independent of `price_type`, as in C: `ZSTD_rescaleFreqs` runs
        // `HIST_count_simple` over the block and downscales it whether or not
        // this block will be priced from those counts. A block short enough to
        // be priced from the predefined tables still seeds the next block, and
        // skipping this left `lit_sum` at zero for the block that followed.
        //
        // It is gated on `compressedLiterals` and on nothing else
        // (`zstd_opt.c:216`), which is a different question: uncoded literals
        // have a fixed price, so there is no statistic to keep.
        if model.compressed_literals {
            for &byte in src {
                model.lit_freq[byte as usize] += 1;
            }
            for freq in &mut model.lit_freq {
                // Bytes the block never used stay at zero, matching C's
                // `base_0possible`.
                if *freq != 0 {
                    *freq = 1 + (*freq >> 8);
                    model.lit_sum += *freq;
                }
            }
        }

        model.set_base_prices();
        model
    }

    #[inline(always)]
    fn weight(&self, stat: u32) -> u32 {
        if IS_ULTRA {
            optimal_fractional_weight(stat)
        } else {
            optimal_bit_weight(stat)
        }
    }

    pub(crate) fn set_base_prices(&mut self) {
        self.ll_sum_base_price = self.weight(self.ll_sum);
        self.ml_sum_base_price = self.weight(self.ml_sum);
        self.of_sum_base_price = self.weight(self.of_sum);
        // C's `ZSTD_setBasePrices` leaves the literal base alone when literals
        // are not coded, and `ZSTD_rawLiteralsCost` returns before reading it.
        // Weighting a `lit_sum` held at zero would put a value there that only
        // looks meaningful.
        if self.compressed_literals {
            self.lit_sum_base_price = self.weight(self.lit_sum);
        }
        for code in 0..self.ml_price_by_code.len() {
            self.ml_price_by_code[code] = ml_bits(code as u8) * OPT_PRICE_UNIT
                + self.ml_sum_base_price
                - self.weight(self.ml_freq[code]);
        }
    }

    /// Create a price model from rescaled cross-block state.
    fn from_persisted_state(
        state: &OptimalPriceState,
        long_offset_penalty: bool,
        compressed_literals: bool,
    ) -> Self {
        let mut model = Self {
            price_type: OptimalPriceType::Dynamic,
            ll_freq: state.ll_freq,
            ml_freq: state.ml_freq,
            of_freq: state.of_freq,
            ll_sum: state.ll_sum,
            ml_sum: state.ml_sum,
            of_sum: state.of_sum,
            lit_freq: state.lit_freq,
            lit_sum: state.lit_sum,
            ll_sum_base_price: 0,
            ml_sum_base_price: 0,
            of_sum_base_price: 0,
            lit_sum_base_price: 0,
            ml_price_by_code: [0; 53],
            long_offset_penalty,
            compressed_literals,
        };
        model.set_base_prices();
        model
    }

    /// Snapshot current frequencies for cross-block persistence.
    fn to_persisted_state(&self) -> OptimalPriceState {
        OptimalPriceState {
            lit_freq: self.lit_freq,
            lit_sum: self.lit_sum,
            ll_freq: self.ll_freq,
            ll_sum: self.ll_sum,
            ml_freq: self.ml_freq,
            ml_sum: self.ml_sum,
            of_freq: self.of_freq,
            of_sum: self.of_sum,
        }
    }

    #[inline(always)]
    pub(crate) fn literal_byte_price(&self, byte: u8) -> u32 {
        // C: `if (!ZSTD_compressedLiterals(optPtr)) return (litLength << 3) *
        // BITCOST_MULTIPLIER`, ahead of the `zop_predef` test. A byte stored
        // verbatim costs the eight bits it occupies, whatever the block's
        // statistics say, and this is the only place the flag changes a number.
        if !self.compressed_literals {
            return 8 * OPT_PRICE_UNIT;
        }
        match self.price_type {
            OptimalPriceType::Predefined => 6 * OPT_PRICE_UNIT,
            // C: `litPrice = WEIGHT(litFreq[byte]); if (litPrice > litPriceMax) litPrice = litPriceMax;`
            // where litPriceMax = litSumBasePrice - BITCOST_MULTIPLIER.
            // This ensures each byte costs at least OPT_PRICE_UNIT.
            OptimalPriceType::Dynamic => {
                // C asserts the same thing here. It holds because every dynamic
                // model is built from at least one observed literal, so
                // `lit_sum >= 1` and its weight is at least one price unit.
                debug_assert!(
                    self.lit_sum_base_price >= OPT_PRICE_UNIT,
                    "dynamic literal pricing with lit_sum={} gives a {}-unit base",
                    self.lit_sum,
                    self.lit_sum_base_price,
                );
                let discount = self
                    .weight(self.lit_freq[byte as usize])
                    .min(self.lit_sum_base_price - OPT_PRICE_UNIT);
                self.lit_sum_base_price - discount
            }
        }
    }

    #[inline(always)]
    pub(crate) fn ll_price(&self, literal_length: u32) -> u32 {
        if matches!(self.price_type, OptimalPriceType::Predefined) {
            return self.weight(literal_length);
        }

        let ll_code = literal_length_code_unchecked(literal_length) as usize;
        ll_bits(ll_code as u8) * OPT_PRICE_UNIT + self.ll_sum_base_price
            - self.weight(self.ll_freq[ll_code])
    }

    /// C's `ZSTD_getMatchPrice`, as the sum of [`offset_price`](Self::offset_price)
    /// and [`match_length_price`](Self::match_length_price).
    ///
    /// Split so that a loop pricing one candidate at every length can pay
    /// for the offset once. The terms are the same ones C adds, in a
    /// different order, which changes nothing: every intermediate is a small
    /// unsigned sum that neither wraps nor goes below zero.
    #[inline(always)]
    pub(crate) fn match_price(&self, offset_value: u32, match_length: u32) -> u32 {
        self.offset_price(offset_value) + self.match_length_price(match_length)
    }

    /// The part of a match's price that depends only on its offset.
    #[inline(always)]
    pub(crate) fn offset_price(&self, offset_value: u32) -> u32 {
        let off_code = offset_code_unchecked(offset_value) as usize;

        if matches!(self.price_type, OptimalPriceType::Predefined) {
            return (16 + off_code as u32) * OPT_PRICE_UNIT;
        }

        let mut price = off_code as u32 * OPT_PRICE_UNIT + self.of_sum_base_price
            - self.weight(self.of_freq[off_code]);
        if self.long_offset_penalty && off_code >= 20 {
            price += (off_code as u32 - 19) * 2 * OPT_PRICE_UNIT;
        }
        price + OPT_PRICE_UNIT / 5
    }

    /// The part of a match's price that depends only on its length.
    #[inline(always)]
    pub(crate) fn match_length_price(&self, match_length: u32) -> u32 {
        if matches!(self.price_type, OptimalPriceType::Predefined) {
            return self.weight(match_length.wrapping_sub(OPT_REPCODE_MIN_MATCH as u32));
        }

        let ml_code = match_length_code_unchecked(match_length) as usize;
        debug_assert_eq!(
            self.ml_price_by_code[ml_code],
            ml_bits(ml_code as u8) * OPT_PRICE_UNIT + self.ml_sum_base_price
                - self.weight(self.ml_freq[ml_code])
        );
        self.ml_price_by_code[ml_code]
    }

    pub(crate) fn update_stats(
        &mut self,
        literals: &[u8],
        literal_length: u32,
        offset_value: u32,
        match_length: u32,
    ) -> Result<()> {
        // Gated on `compressed_literals` and nothing else, as in C.
        // `ZSTD_updateStats` asks whether literals are compressed at all — not
        // which prices this block was parsed with. A block priced from the
        // predefined tables still has to seed the next block's statistics, and
        // gating on `Dynamic` meant it did not: a first block of
        // `OPT_PREDEFINED_THRESHOLD` bytes or fewer left `lit_sum` at zero, the
        // next block built a dynamic model on top of it, and
        // `literal_byte_price` subtracted a full price unit from a zero base.
        // Only reachable through `StreamingEncoder::flush`, which is the one way
        // a caller can end a block that small.
        if self.compressed_literals {
            for &byte in literals {
                self.lit_freq[byte as usize] += OPT_LITERAL_FREQ_ADD;
                self.lit_sum += OPT_LITERAL_FREQ_ADD;
            }
        }

        let ll_code = literal_length_code_unchecked(literal_length) as usize;
        let ml_code = match_length_code_unchecked(match_length) as usize;
        let of_code = offset_code_unchecked(offset_value) as usize;
        self.ll_freq[ll_code] += 1;
        self.ll_sum += 1;
        self.ml_freq[ml_code] += 1;
        self.ml_sum += 1;
        self.of_freq[of_code] += 1;
        self.of_sum += 1;
        self.set_base_prices();
        Ok(())
    }
}

pub(crate) fn optimal_base_ll_frequencies() -> [u32; 36] {
    [
        4, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1,
    ]
}

pub(crate) fn optimal_base_of_frequencies() -> [u32; 32] {
    [
        6, 2, 1, 1, 2, 3, 4, 4, 4, 3, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1,
    ]
}

pub(crate) fn optimal_bit_weight(stat: u32) -> u32 {
    highbit32(stat.saturating_add(1)) * OPT_PRICE_UNIT
}

pub(crate) fn optimal_fractional_weight(raw_stat: u32) -> u32 {
    let stat = raw_stat.saturating_add(1);
    let high_bit = highbit32(stat);
    let base = high_bit * OPT_PRICE_UNIT;
    let fraction = (stat << OPT_PRICE_ACCURACY_LOG) >> high_bit;
    base + fraction
}

#[inline(always)]
pub(crate) fn push_optimal_candidate(
    candidates: &mut Vec<OptimalMatchCandidate>,
    raw_offset: usize,
    length: usize,
    current_reps: [u32; 3],
    literal_length: usize,
) {
    if raw_offset == 0 || length < OPT_REPCODE_MIN_MATCH {
        return;
    }

    let repeat_offsets = RepeatOffsets::from_values(current_reps);
    let offset_value = repeat_offsets.classify_offset(
        raw_offset.min(u32::MAX as usize) as u32,
        literal_length.min(u32::MAX as usize) as u32,
    );
    candidates.push(OptimalMatchCandidate {
        offset_value,
        length: length as u32,
    });
}

/// Collect optimal match candidates at `pos`. Returns `(next_to_update)` —
/// the position up to which the BT has been updated, so the caller can skip
/// redundant `insert_range` calls (matching C's `nextToUpdate` advancement).
/// Dispatch to MLS-specialized inner function. Matching C's compile-time
/// specialization via `GEN_ZSTD_BT_GET_ALL_MATCHES_(dictMode, mls)` which
/// generates separate functions for mls=3,4,5,6 so the hash function,
/// repcode quick-check, and HC3 path are all compile-time constants.
pub(crate) fn collect_optimal_matches_without_prefix(
    candidates: &mut Vec<OptimalMatchCandidate>,
    tree_matches: &mut Vec<MatchCandidate>,
    src: &[u8],
    pos: usize,
    current_reps: [u32; 3],
    literal_length: usize,
    params: MatchFinderParameters,
    finder: &mut BinaryTreeFinder,
    window_low: usize,
    hash3: Option<&mut Hash3Table>,
    sufficient_len: u32,
) -> usize {
    match params.min_match {
        3 => collect_optimal_matches_without_prefix_inner::<3>(
            candidates,
            tree_matches,
            src,
            pos,
            current_reps,
            literal_length,
            params,
            finder,
            window_low,
            hash3,
            sufficient_len,
        ),
        5 => collect_optimal_matches_without_prefix_inner::<5>(
            candidates,
            tree_matches,
            src,
            pos,
            current_reps,
            literal_length,
            params,
            finder,
            window_low,
            hash3,
            sufficient_len,
        ),
        6 => collect_optimal_matches_without_prefix_inner::<6>(
            candidates,
            tree_matches,
            src,
            pos,
            current_reps,
            literal_length,
            params,
            finder,
            window_low,
            hash3,
            sufficient_len,
        ),
        _ => collect_optimal_matches_without_prefix_inner::<4>(
            candidates,
            tree_matches,
            src,
            pos,
            current_reps,
            literal_length,
            params,
            finder,
            window_low,
            hash3,
            sufficient_len,
        ),
    }
}

#[inline(always)]
fn collect_optimal_matches_without_prefix_inner<const MLS: u32>(
    candidates: &mut Vec<OptimalMatchCandidate>,
    tree_matches: &mut Vec<MatchCandidate>,
    src: &[u8],
    pos: usize,
    current_reps: [u32; 3],
    literal_length: usize,
    params: MatchFinderParameters,
    finder: &mut BinaryTreeFinder,
    window_low: usize,
    hash3: Option<&mut Hash3Table>,
    sufficient_len: u32,
) -> usize {
    candidates.clear();
    let [rep1, rep2, rep3] = current_reps;
    // C: `U32 const minMatch = (cParams->minMatch == 3) ? 3 : 4`
    // With const MLS, this is fully resolved at compile time.
    const fn effective_min(mls: u32) -> usize {
        if mls == 3 { 3 } else { 4 }
    }
    let mut best_length = effective_min(MLS).saturating_sub(1);

    // Check repeat offsets first (matches C's repcode checking order).
    let rep_offsets: [Option<u32>; 3] = if literal_length == 0 {
        [Some(rep2), Some(rep3), Some(rep1.saturating_sub(1))]
    } else {
        [Some(rep1), Some(rep2), Some(rep3)]
    };
    for raw_offset in rep_offsets.into_iter().flatten() {
        let raw_offset = raw_offset as usize;
        if raw_offset == 0 || raw_offset > pos {
            continue;
        }
        let match_start = pos - raw_offset;
        if match_start < window_low {
            continue;
        }
        // C: ZSTD_readMINMATCH quick reject — compare first minMatch bytes
        // before calling the full count. With const MLS, the 3-byte vs 4-byte
        // branch is eliminated at compile time.
        #[allow(unsafe_code)]
        unsafe {
            let base = src.as_ptr();
            let l = core::ptr::read_unaligned(base.add(match_start) as *const u32);
            let r = core::ptr::read_unaligned(base.add(pos) as *const u32);
            if MLS == 3 {
                if cfg!(target_endian = "little") {
                    if (l ^ r) & 0x00FF_FFFF != 0 {
                        continue;
                    }
                } else if (l ^ r) & 0xFFFF_FF00 != 0 {
                    continue;
                }
            } else if l != r {
                continue;
            }
        }
        // Start counting from effective_min_match (already verified above).
        let length = count_match_length_from(src, match_start, pos, effective_min(MLS));
        if length > best_length {
            push_optimal_candidate(candidates, raw_offset, length, current_reps, literal_length);
            best_length = length;
            if length > sufficient_len as usize || pos + length >= src.len() {
                return pos;
            }
        }
    }

    // HC3: only when MLS == 3. With const MLS, this entire block is
    // dead-code-eliminated for MLS >= 4.
    if MLS == 3 {
        if let Some(hash3) = hash3 {
            if best_length < 3 {
                let match_index = hash3.insert_and_find(src, pos) as usize;
                let match_low = window_low;
                if match_index < pos
                    && match_index >= match_low
                    && pos - match_index < HASH3_MAX_DISTANCE
                {
                    let length = count_match_length(src, match_index, pos);
                    if length >= 3 {
                        let raw_offset = pos - match_index;
                        push_optimal_candidate(
                            candidates,
                            raw_offset,
                            length,
                            current_reps,
                            literal_length,
                        );
                        best_length = length;
                        if length > sufficient_len as usize || pos + length >= src.len() {
                            return pos + 1;
                        }
                    }
                }
            }
        }
    }

    // Combined insert + search (matches C's ZSTD_insertBtAndGetAllMatches).
    let (next_to_update, _remaining) = finder.insert_and_collect_matches(
        tree_matches,
        src,
        pos,
        params.search_depth,
        window_low,
        best_length + 1,
    );
    for candidate in tree_matches.iter().copied() {
        push_optimal_candidate(
            candidates,
            candidate.offset,
            candidate.length,
            current_reps,
            literal_length,
        );
        best_length = best_length.max(candidate.length);
    }

    next_to_update
}

#[allow(dead_code)]
pub(crate) fn collect_optimal_matches_with_prefix(
    candidates: &mut Vec<OptimalMatchCandidate>,
    tree_matches: &mut Vec<MatchCandidate>,
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    pos: usize,
    current_reps: [u32; 3],
    literal_length: usize,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prefix_finder: &BinaryTreeFinder,
    src_finder: &mut BinaryTreeFinder,
    hash3: Option<&mut Hash3Table>,
    sufficient_len: u32,
) -> usize {
    match params.min_match {
        3 => collect_optimal_matches_with_prefix_inner::<3>(
            candidates,
            tree_matches,
            prefix_chain,
            src,
            pos,
            current_reps,
            literal_length,
            params,
            prefix_low,
            source_low,
            prefix_finder,
            src_finder,
            hash3,
            sufficient_len,
        ),
        5 => collect_optimal_matches_with_prefix_inner::<5>(
            candidates,
            tree_matches,
            prefix_chain,
            src,
            pos,
            current_reps,
            literal_length,
            params,
            prefix_low,
            source_low,
            prefix_finder,
            src_finder,
            hash3,
            sufficient_len,
        ),
        6 => collect_optimal_matches_with_prefix_inner::<6>(
            candidates,
            tree_matches,
            prefix_chain,
            src,
            pos,
            current_reps,
            literal_length,
            params,
            prefix_low,
            source_low,
            prefix_finder,
            src_finder,
            hash3,
            sufficient_len,
        ),
        _ => collect_optimal_matches_with_prefix_inner::<4>(
            candidates,
            tree_matches,
            prefix_chain,
            src,
            pos,
            current_reps,
            literal_length,
            params,
            prefix_low,
            source_low,
            prefix_finder,
            src_finder,
            hash3,
            sufficient_len,
        ),
    }
}

#[inline(always)]
fn collect_optimal_matches_with_prefix_inner<const MLS: u32>(
    candidates: &mut Vec<OptimalMatchCandidate>,
    tree_matches: &mut Vec<MatchCandidate>,
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    pos: usize,
    current_reps: [u32; 3],
    literal_length: usize,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prefix_finder: &BinaryTreeFinder,
    src_finder: &mut BinaryTreeFinder,
    hash3: Option<&mut Hash3Table>,
    sufficient_len: u32,
) -> usize {
    candidates.clear();
    let [rep1, rep2, rep3] = current_reps;
    let current = prefix_chain.len() + pos;
    const fn effective_min(mls: u32) -> usize {
        if mls == 3 { 3 } else { 4 }
    }
    let mut best_length = effective_min(MLS).saturating_sub(1);

    // Check repeat offsets first (matches C's repcode checking order).
    let rep_offsets: [Option<u32>; 3] = if literal_length == 0 {
        [Some(rep2), Some(rep3), Some(rep1.saturating_sub(1))]
    } else {
        [Some(rep1), Some(rep2), Some(rep3)]
    };
    for raw_offset in rep_offsets.into_iter().flatten() {
        let raw_offset = raw_offset as usize;
        if raw_offset == 0 || raw_offset > current {
            continue;
        }
        let match_start = current - raw_offset;
        if !logical_match_start_is_valid(prefix_chain.len(), match_start, prefix_low, source_low) {
            continue;
        }
        let length = count_match_length_virtual(prefix_chain, src, match_start, current);
        if length > best_length {
            push_optimal_candidate(candidates, raw_offset, length, current_reps, literal_length);
            best_length = length;
            if length > sufficient_len as usize || pos + length >= src.len() {
                return pos;
            }
        }
    }

    // HC3: only when MLS == 3. Dead-code-eliminated for MLS >= 4.
    if MLS == 3 {
        if let Some(hash3) = hash3 {
            if best_length < 3 {
                let match_index = hash3.insert_and_find(src, pos) as usize;
                let match_low = source_low;
                if match_index < pos
                    && match_index >= match_low
                    && pos - match_index < HASH3_MAX_DISTANCE
                {
                    let length = count_match_length(src, match_index, pos);
                    if length >= 3 {
                        let raw_offset = pos - match_index;
                        push_optimal_candidate(
                            candidates,
                            raw_offset,
                            length,
                            current_reps,
                            literal_length,
                        );
                        best_length = length;
                        // C: early return when HC3 match exceeds sufficient_len
                        // (zstd_opt.c:714-717). Skip BST insert+search.
                        // Return pos+1 matching C's `ms->nextToUpdate = curr+1`.
                        if length > sufficient_len as usize || pos + length >= src.len() {
                            return pos + 1;
                        }
                    }
                }
            }
        }
    } // if MLS == 3

    // Raise threshold for tree matches to the configured minimum.
    let tree_min = regular_match_length_threshold(literal_length, params);
    if best_length < tree_min.saturating_sub(1) {
        best_length = tree_min.saturating_sub(1);
    }

    prefix_finder.collect_prefix_chain_matches(
        tree_matches,
        prefix_chain,
        src,
        pos,
        params.dictionary_search_depth,
        prefix_low,
        best_length + 1,
    );
    for candidate in tree_matches.iter().copied() {
        push_optimal_candidate(
            candidates,
            candidate.offset,
            candidate.length,
            current_reps,
            literal_length,
        );
        best_length = best_length.max(candidate.length);
    }

    // Combined insert + search for source finder
    let (next_to_update, _remaining) = src_finder.insert_and_collect_matches(
        tree_matches,
        src,
        pos,
        params.search_depth,
        source_low,
        best_length + 1,
    );
    for candidate in tree_matches.iter().copied() {
        push_optimal_candidate(
            candidates,
            candidate.offset,
            candidate.length,
            current_reps,
            literal_length,
        );
        best_length = best_length.max(candidate.length);
    }

    next_to_update
}

/// Two-phase optimal match collection matching C's ZSTD_insertBtAndGetAllMatches:
/// Phase 1: Insert current position + search source-only BST.
/// Phase 2: Search pre-built dictionary BST read-only.
/// Both phases share an `nbCompares` budget.
/// Dispatch to MLS-specialized inner function for compile-time dead-code
/// elimination of HC3 (MLS>=4) and const-folded repcode quick reject.
pub(crate) fn collect_optimal_matches_two_phase(
    candidates: &mut Vec<OptimalMatchCandidate>,
    tree_matches: &mut Vec<MatchCandidate>,
    dict_matches: &mut Vec<MatchCandidate>,
    virtual_buf: &[u8],
    prefix_len: usize,
    pos: usize,
    current_reps: [u32; 3],
    literal_length: usize,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    source_finder: &mut BinaryTreeFinder,
    dict_finder: &BinaryTreeFinder,
    hash3: Option<&mut Hash3Table>,
    ext_dict_mode: bool,
    sufficient_len: u32,
) {
    match params.min_match {
        3 => collect_optimal_matches_two_phase_inner::<3>(
            candidates,
            tree_matches,
            dict_matches,
            virtual_buf,
            prefix_len,
            pos,
            current_reps,
            literal_length,
            params,
            prefix_low,
            source_low,
            source_finder,
            dict_finder,
            hash3,
            ext_dict_mode,
            sufficient_len,
        ),
        5 => collect_optimal_matches_two_phase_inner::<5>(
            candidates,
            tree_matches,
            dict_matches,
            virtual_buf,
            prefix_len,
            pos,
            current_reps,
            literal_length,
            params,
            prefix_low,
            source_low,
            source_finder,
            dict_finder,
            hash3,
            ext_dict_mode,
            sufficient_len,
        ),
        6 => collect_optimal_matches_two_phase_inner::<6>(
            candidates,
            tree_matches,
            dict_matches,
            virtual_buf,
            prefix_len,
            pos,
            current_reps,
            literal_length,
            params,
            prefix_low,
            source_low,
            source_finder,
            dict_finder,
            hash3,
            ext_dict_mode,
            sufficient_len,
        ),
        _ => collect_optimal_matches_two_phase_inner::<4>(
            candidates,
            tree_matches,
            dict_matches,
            virtual_buf,
            prefix_len,
            pos,
            current_reps,
            literal_length,
            params,
            prefix_low,
            source_low,
            source_finder,
            dict_finder,
            hash3,
            ext_dict_mode,
            sufficient_len,
        ),
    }
}

#[inline(always)]
fn collect_optimal_matches_two_phase_inner<const MLS: u32>(
    candidates: &mut Vec<OptimalMatchCandidate>,
    tree_matches: &mut Vec<MatchCandidate>,
    dict_matches: &mut Vec<MatchCandidate>,
    virtual_buf: &[u8],
    prefix_len: usize,
    pos: usize,
    current_reps: [u32; 3],
    literal_length: usize,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    source_finder: &mut BinaryTreeFinder,
    dict_finder: &BinaryTreeFinder,
    hash3: Option<&mut Hash3Table>,
    ext_dict_mode: bool,
    sufficient_len: u32,
) {
    candidates.clear();
    let src = &virtual_buf[prefix_len..];
    let [rep1, rep2, rep3] = current_reps;
    let current = prefix_len + pos;
    const fn effective_min(mls: u32) -> usize {
        if mls == 3 { 3 } else { 4 }
    }
    let mut best_length = effective_min(MLS).saturating_sub(1);

    let sufficient_len = sufficient_len as usize;

    // C's ZSTD_insertBtAndGetAllMatches checks repcodes and HC3 BEFORE
    // updating the hash table (line 722). If either causes an early return,
    // the hash table is NOT updated — preserving the BST chain from the
    // previous position with this hash. Match C by deferring the hash
    // table update until after the repcode/HC3 checks.
    let bst_prefix_len = if ext_dict_mode { prefix_len } else { 0 };

    // Check repeat offsets first (matches C's repcode checking order).
    // Hash table and BST are NOT yet modified at this point.
    let rep_offsets: [Option<u32>; 3] = if literal_length == 0 {
        [Some(rep2), Some(rep3), Some(rep1.saturating_sub(1))]
    } else {
        [Some(rep1), Some(rep2), Some(rep3)]
    };
    for raw_offset in rep_offsets.into_iter().flatten() {
        let raw_offset = raw_offset as usize;
        if raw_offset == 0 || raw_offset > current {
            continue;
        }
        let match_start = current - raw_offset;
        if !logical_match_start_is_valid(prefix_len, match_start, prefix_low, source_low) {
            continue;
        }
        // C's ZSTD_index_overlap_check(dictLimit, repIndex) — reject
        // repcodes starting within 3 positions of the dict/source boundary.
        if match_start < prefix_len && prefix_len.wrapping_sub(1).wrapping_sub(match_start) < 3 {
            continue;
        }
        // ZSTD_readMINMATCH quick reject: compare first MLS bytes before
        // full match counting. virtual_buf is contiguous so pointer reads
        // are safe for both prefix and source positions.
        #[allow(unsafe_code)]
        unsafe {
            let base = virtual_buf.as_ptr();
            let l = core::ptr::read_unaligned(base.add(match_start) as *const u32);
            let r = core::ptr::read_unaligned(base.add(current) as *const u32);
            if MLS == 3 {
                if cfg!(target_endian = "little") {
                    if (l ^ r) & 0x00FF_FFFF != 0 {
                        continue;
                    }
                } else if (l ^ r) & 0xFFFF_FF00 != 0 {
                    continue;
                }
            } else if l != r {
                continue;
            }
        }
        let length = count_match_length_from(virtual_buf, match_start, current, effective_min(MLS));
        if length > best_length {
            push_optimal_candidate(candidates, raw_offset, length, current_reps, literal_length);
            best_length = length;
            if best_length > sufficient_len || pos + best_length >= src.len() {
                return;
            }
        }
    }

    // HC3: only when MLS == 3. Dead-code-eliminated for MLS >= 4.
    if MLS == 3 {
        if let Some(hash3) = hash3 {
            if best_length < 3 {
                let match_index = hash3.insert_and_find(src, pos) as usize;
                let match_low = source_low;
                if match_index < pos
                    && match_index >= match_low
                    && pos - match_index < HASH3_MAX_DISTANCE
                {
                    let length = count_match_length(src, match_index, pos);
                    if length >= 3 {
                        let raw_offset = pos - match_index;
                        push_optimal_candidate(
                            candidates,
                            raw_offset,
                            length,
                            current_reps,
                            literal_length,
                        );
                        best_length = length;
                        // C returns WITHOUT BT insertion when HC3 is sufficient.
                        // C: ms->nextToUpdate = curr + 1 (skip BST insertion
                        // for this position on the HC3 early-return path).
                        if best_length > sufficient_len || pos + best_length >= src.len() {
                            source_finder.next_to_update = pos + 1;
                            return;
                        }
                    }
                }
            }
        }
    } // if MLS == 3

    // Hash table update: C does this at line 722, AFTER repcode/HC3 checks
    // but BEFORE the BST search. This is the correct position — early returns
    // from repcode/HC3 must NOT update the hash table.
    let prev_match_index = source_finder.pre_insert_hash_unified(src, bst_prefix_len, pos);

    if ext_dict_mode {
        // extDict path: unified BST with dict entries pre-loaded.
        // Single-phase search finds both dict and source matches.
        // Matches C's byCopyingCDict + ZSTD_extDict where dms=NULL and
        // Phase 2 (dms tree search) is completely skipped.
        let (_ntu, _remaining_compares) = source_finder.insert_and_collect_matches_unified(
            tree_matches,
            virtual_buf,
            bst_prefix_len,
            pos,
            params.search_depth,
            prefix_low,
            source_low,
            best_length + 1,
            prev_match_index,
        );
        for candidate in tree_matches.iter().copied() {
            push_optimal_candidate(
                candidates,
                candidate.offset,
                candidate.length,
                current_reps,
                literal_length,
            );
            best_length = best_length.max(candidate.length);
        }
    } else {
        // dictMatchState path: separate source BST (Phase 1) + dict BST
        // (Phase 2). Source BST uses source-relative coordinates.
        let (_ntu, _remaining_compares) = source_finder.insert_and_collect_matches_unified(
            tree_matches,
            src,
            0, // prefix_len=0: source-relative coordinates
            pos,
            params.search_depth,
            0, // prefix_low irrelevant with prefix_len=0
            source_low,
            best_length + 1,
            prev_match_index,
        );
        for candidate in tree_matches.iter().copied() {
            push_optimal_candidate(
                candidates,
                candidate.offset,
                candidate.length,
                current_reps,
                literal_length,
            );
            best_length = best_length.max(candidate.length);
        }

        // Phase 2 — Dictionary BST: read-only search of pre-built dict tree.
        if prefix_len > 0 {
            const DICT_POS_BIAS: usize = 2;
            let dms_high_limit = prefix_len + DICT_POS_BIAS;
            let dms_low_limit = prefix_low + DICT_POS_BIAS;
            let dms_bt_low = if dict_finder.bt_mask >= dms_high_limit - dms_low_limit {
                dms_low_limit
            } else {
                dms_high_limit - dict_finder.bt_mask
            };
            BinaryTreeFinder::search_dict_bst(
                dict_finder,
                dict_matches,
                virtual_buf,
                prefix_len,
                pos,
                params.search_depth,
                dms_low_limit,
                dms_bt_low,
                best_length,
            );
            for candidate in dict_matches.iter().copied() {
                push_optimal_candidate(
                    candidates,
                    candidate.offset,
                    candidate.length,
                    current_reps,
                    literal_length,
                );
                best_length = best_length.max(candidate.length);
                let mi = (prefix_len + pos) - candidate.offset;
                let match_end_virtual = mi + candidate.length;
                if match_end_virtual >= prefix_len {
                    let match_end_src = match_end_virtual - prefix_len;
                    let ntu_candidate = match_end_src.saturating_sub(8).max(pos + 1);
                    if ntu_candidate > source_finder.next_to_update {
                        source_finder.next_to_update = ntu_candidate;
                    }
                }
            }
        }
    }
}

/// Collect matches at a DP position where the source BST has already
/// been inserted past this point (inr < inserted_until).  C's optimal
/// parser always does a full BT search at every DP position (insert +
/// search), even for positions already covered by `nextToUpdate`.
/// We match this by doing a read-only BT search at already-inserted
/// positions, ensuring the DP sees source matches it would otherwise
/// miss.
#[allow(dead_code)]
fn collect_optimal_rep_and_prefix_matches(
    candidates: &mut Vec<OptimalMatchCandidate>,
    tree_matches: &mut Vec<MatchCandidate>,
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    pos: usize,
    current_reps: [u32; 3],
    literal_length: usize,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prefix_finder: &BinaryTreeFinder,
    src_finder: &BinaryTreeFinder,
) {
    candidates.clear();
    let [rep1, rep2, rep3] = current_reps;
    let current = prefix_chain.len() + pos;
    let mut best_length = OPT_REPCODE_MIN_MATCH.saturating_sub(1);

    let rep_offsets: [Option<u32>; 3] = if literal_length == 0 {
        [Some(rep2), Some(rep3), Some(rep1.saturating_sub(1))]
    } else {
        [Some(rep1), Some(rep2), Some(rep3)]
    };
    for raw_offset in rep_offsets.into_iter().flatten() {
        let raw_offset = raw_offset as usize;
        if raw_offset == 0 || raw_offset > current {
            continue;
        }
        let match_start = current - raw_offset;
        if !logical_match_start_is_valid(prefix_chain.len(), match_start, prefix_low, source_low) {
            continue;
        }
        let length = count_match_length_virtual(prefix_chain, src, match_start, current);
        if length > best_length {
            push_optimal_candidate(candidates, raw_offset, length, current_reps, literal_length);
            best_length = length;
        }
    }

    prefix_finder.collect_prefix_chain_matches(
        tree_matches,
        prefix_chain,
        src,
        pos,
        params.dictionary_search_depth,
        prefix_low,
        best_length + 1,
    );
    for candidate in tree_matches.iter().copied() {
        push_optimal_candidate(
            candidates,
            candidate.offset,
            candidate.length,
            current_reps,
            literal_length,
        );
        best_length = best_length.max(candidate.length);
    }

    // Read-only source BT search (no insertion).  C's unified BT always
    // finds source matches at every DP position because the BT search is
    // unconditional.  With separate trees, we must explicitly search the
    // source BT even at positions already past inserted_until.
    src_finder.collect_matches(tree_matches, src, pos, params, literal_length, source_low);
    for candidate in tree_matches.iter().copied() {
        push_optimal_candidate(
            candidates,
            candidate.offset,
            candidate.length,
            current_reps,
            literal_length,
        );
    }
}

/// Returns the replaced match node if a literal extension overwrote a match
/// (needed for the btultra match+1literal optimization).
#[inline(always)]
#[allow(unsafe_code)]
pub(crate) fn update_optimal_literal_node<const IS_ULTRA: bool>(
    nodes: &mut [OptimalPathNode],
    pos: usize,
    byte: u8,
    price_model: &OptimalPriceModel<IS_ULTRA>,
) -> Option<OptimalPathNode> {
    debug_assert!(pos > 0 && pos < nodes.len());
    // SAFETY: pos is in 1..=last_pos where last_pos < nodes.len(), checked by caller.
    let previous = unsafe { *nodes.get_unchecked(pos - 1) };

    // No overflow guard needed: MAX_PRICE (1<<30) + any step cost stays
    // within u32, so unreachable predecessors naturally produce huge prices
    // that lose every comparison — matching C's ZSTD_MAX_PRICE arithmetic.
    let new_litlen = previous.litlen + 1;
    let price =
        previous.price + price_model.literal_byte_price(byte) + price_model.ll_price(new_litlen)
            - price_model.ll_price(previous.litlen);
    let current = unsafe { nodes.get_unchecked(pos) };
    if price <= current.price {
        let prev_node = *current;
        unsafe {
            *nodes.get_unchecked_mut(pos) = OptimalPathNode {
                price,
                off: 0,
                mlen: 0,
                litlen: new_litlen,
                reps: previous.reps,
            };
        }
        // Return the replaced node for btultra match+1literal optimization
        // when the replaced node was a match end (litlen==0 with a real price).
        if prev_node.litlen == 0 && prev_node.price < MAX_PRICE {
            return Some(prev_node);
        }
    }
    None
}

#[inline(always)]
/// Initial match node setup: scan upward, unconditionally fill every position.
/// Matches C's ZSTD_compressBlock_opt_generic lines 1173-1196.
#[allow(unsafe_code)]
pub(crate) fn init_optimal_match_nodes<const IS_ULTRA: bool>(
    nodes: &mut [OptimalPathNode],
    candidates: &[OptimalMatchCandidate],
    price_model: &OptimalPriceModel<IS_ULTRA>,
    horizon: usize,
    min_match: usize,
) -> usize {
    debug_assert!(horizon < nodes.len());
    // SAFETY: horizon < nodes.len() is maintained by the caller.
    let current = unsafe { *nodes.get_unchecked(0) };
    if current.price >= MAX_PRICE {
        return 0;
    }

    // C lines 1175-1179: set positions below min_match to unreachable.
    // Fully initialize price/mlen/litlen (no bulk fill to rely on).
    for p in 1..min_match {
        if p > horizon {
            break;
        }
        unsafe {
            let node = nodes.get_unchecked_mut(p);
            node.price = MAX_PRICE;
            node.mlen = 0;
            node.litlen = current.litlen + p as u32;
        }
    }

    let base_price = current.price + price_model.ll_price(0);
    let max_reach = horizon as u32;
    let mut pos = min_match as u32;
    for candidate in candidates {
        let end = candidate.length.min(max_reach);
        while pos <= end {
            let idx = pos as usize;
            debug_assert!(idx <= horizon && idx < nodes.len());
            let price = base_price + price_model.match_price(candidate.offset_value, pos);
            unsafe {
                let node = nodes.get_unchecked_mut(idx);
                node.price = price;
                node.off = candidate.offset_value;
                node.mlen = pos;
                node.litlen = 0;
                // reps: computed lazily in the DP inner loop (C lines 1256-1261)
            }
            pos += 1;
        }
    }
    let last_pos = pos.saturating_sub(1) as usize;
    // Sentinel at last_pos+1 (C line 1195): marks end of initialized region.
    if pos as usize <= horizon {
        unsafe {
            nodes.get_unchecked_mut(pos as usize).price = MAX_PRICE;
        }
    }
    last_pos
}

#[inline(always)]
#[allow(unsafe_code)]
pub(crate) fn update_optimal_match_nodes<const IS_ULTRA: bool>(
    nodes: &mut [OptimalPathNode],
    cur: usize,
    last_pos: &mut usize,
    candidates: &[OptimalMatchCandidate],
    price_model: &OptimalPriceModel<IS_ULTRA>,
    horizon: usize,
    min_match: usize,
) {
    debug_assert!(cur <= horizon && horizon < nodes.len());
    // SAFETY: cur <= horizon < nodes.len() is maintained by the caller.
    let current = unsafe { *nodes.get_unchecked(cur) };
    if current.price >= MAX_PRICE {
        return;
    }

    let base_price = current.price + price_model.ll_price(0);
    let max_reach = (horizon - cur) as u32;
    // C: `startML = minMatch` for the first candidate.
    let mut previous_length = min_match as u32 - 1;
    for candidate in candidates {
        let max_length = candidate.length.min(max_reach);
        if max_length <= previous_length {
            continue;
        }

        // C scans downward with early break at optLevel==0.
        // Scan downward: from max_length down to previous_length+1.
        let start_ml = previous_length + 1;
        let mut ml = max_length;
        // The offset does not change over the scan, so its share of the price
        // is paid once here rather than once per length.
        let candidate_price = base_price + price_model.offset_price(candidate.offset_value);
        while ml >= start_ml {
            let pos = cur + ml as usize;
            debug_assert!(pos <= horizon && pos < nodes.len());
            let price = candidate_price + price_model.match_length_price(ml);
            // C lines 1318-1326: when pos extends beyond the frontier,
            // gap-fill intermediate positions with MAX_PRICE sentinel.
            // SAFETY: pos = cur + ml where ml <= max_reach = horizon - cur,
            // so pos <= horizon < nodes.len().
            let beyond = pos > *last_pos;
            if beyond || price < unsafe { nodes.get_unchecked(pos) }.price {
                while *last_pos < pos {
                    *last_pos += 1;
                    unsafe {
                        let gap = nodes.get_unchecked_mut(*last_pos);
                        gap.price = MAX_PRICE;
                        gap.litlen = u32::MAX; // C: !0, "not an end of match"
                    }
                }
                unsafe {
                    let node = nodes.get_unchecked_mut(pos);
                    node.price = price;
                    node.off = candidate.offset_value;
                    node.mlen = ml;
                    node.litlen = 0;
                    // reps: computed lazily in the DP inner loop (C lines 1256-1261)
                }
            } else if !IS_ULTRA {
                break; // C: if (optLevel==0) break; early abort
            }
            ml -= 1;
        }

        previous_length = max_length;
        if previous_length == max_reach {
            break;
        }
    }
}

pub(crate) fn backtrace_optimal_path(
    nodes: &[OptimalPathNode],
    last_pos: usize,
    sequences: &mut Vec<(u32, u32, u32)>,
) -> u32 {
    sequences.clear();
    let tail_literals = nodes[last_pos].litlen;
    let mut pos = last_pos;
    // Walk back through literals (mlen == 0 means literal or start)
    while pos > 0 && nodes[pos].mlen == 0 {
        pos -= 1;
    }

    while pos > 0 {
        let node = nodes[pos];
        if node.mlen == 0 {
            break;
        }
        // In C's stretch model, a node with mlen=M and litlen=L spans M+L
        // positions.  Normal match nodes have litlen=0; the btultra
        // match+1literal optimization creates nodes with litlen=1.
        let prev = pos - node.mlen as usize - node.litlen as usize;
        sequences.push((nodes[prev].litlen, node.off, node.mlen));
        pos = prev;
        while pos > 0 && nodes[pos].mlen == 0 {
            pos -= 1;
        }
    }

    sequences.reverse();
    tail_literals
}

pub(crate) fn emit_optimal_sequence<const IS_ULTRA: bool>(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    repeat_offsets: &mut RepeatOffsets,
    price_model: &mut OptimalPriceModel<IS_ULTRA>,
    literal_length: u32,
    offset_value: u32,
    match_length: u32,
) -> Result<()> {
    let literals_end = *anchor + literal_length as usize;
    let literals = &src[*anchor..literals_end];
    plan.literals.extend_from_slice(literals);
    let command = SequenceCommand {
        literal_length,
        offset_value,
        match_length,
    };
    plan.sequences.push(command);
    price_model.update_stats(literals, literal_length, offset_value, match_length)?;
    repeat_offsets.resolve(&command)?;
    *anchor = literals_end + match_length as usize;
    Ok(())
}

/// A [`MatchFloor`] rather than a precomputed floor, because C recomputes this
/// parser's floor at every position it searches:
/// `windowLow = ZSTD_getLowestMatchIndex(ms, curr, cParams->windowLog)` at
/// `zstd_opt.c:619`, in `ZSTD_insertBtAndGetAllMatches` (the name
/// `ZSTD_BtGetAllMatches` survives only in a `DEBUGLOG` string at `:845`; the
/// function is `ZSTD_btGetAllMatches_internal`). A single per-block value
/// cannot express that. Taking
/// it from the block's end is safe but throws away the oldest slice of the
/// window at every position before the last, which cost 0.26% on a 16 MiB log
/// body at level 19.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_sequences_optimal_without_prefix_into<const IS_ULTRA: bool>(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    match_floor: MatchFloor,
    finder: &mut BinaryTreeFinder,
    ldm: Option<&[RawSequence]>,
) -> Result<()> {
    let block_len = src.len().saturating_sub(block_start);
    plan.reset_for_block(block_len);
    if block_len < MIN_MATCH {
        plan.literals.extend_from_slice(&src[block_start..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    let mut price_model = if let Some(mut persisted) = plan.opt_price_state.take() {
        persisted.rescale();
        OptimalPriceModel::<IS_ULTRA>::from_persisted_state(
            &persisted,
            params.long_offset_penalty,
            params.compressed_literals,
        )
    } else {
        OptimalPriceModel::<IS_ULTRA>::new(
            &src[block_start..],
            None,
            params.long_offset_penalty,
            params.compressed_literals,
        )
    };
    let mut repeat_offsets = repeat_offsets;
    let mut anchor = block_start;
    let mut ip = block_start;
    let limit = src.len().saturating_sub(8);
    // Carry forward from the previous block's end position, matching C's
    // ms->nextToUpdate persistence across blocks. If the previous block
    // didn't insert all positions up to block_start, the catch-up insertion
    // at the first match search will fill the gap.
    let mut inserted_until = finder.next_to_update.min(block_start);
    // Reuse cached scratch buffers from the plan to avoid per-block allocation.
    let mut nodes = core::mem::take(&mut plan.opt_nodes);
    // Kept only for its allocation: every entry has to start unset. Growing the
    // buffer without resetting what was already in it left the previous parse's
    // prices in place, and a position this parse never reaches but does read
    // then compares against a real price instead of an unreachable one. The
    // symptom is a valid frame that differs from what the same input produces on
    // a fresh encoder, so a reused `Encoder` quietly compressed worse on every
    // other call. C writes `ZSTD_MAX_PRICE` into these positions rather than
    // relying on what was there.
    nodes.clear();
    nodes.resize(OPT_NUM + 1, OptimalPathNode::UNSET);
    let mut tree_matches = core::mem::take(&mut plan.opt_tree_matches);
    let mut candidates = core::mem::take(&mut plan.opt_candidates);
    let mut backtrace_buf = core::mem::take(&mut plan.opt_backtrace);
    // HC3: take or create the 3-byte hash table when min_match == 3.
    // C: `nextToUpdate3 = ms->nextToUpdate` — HC3 starts from the BST's
    // carry-forward position, not block_start.  This lets HC3 cover the
    // gap between the previous block's last inserted position and the
    // new block's start, matching C's behavior.
    let mut hash3 = if params.min_match == 3 {
        let mut h3 = plan
            .opt_hash3
            .take()
            .unwrap_or_else(|| Hash3Table::new(params.window_log));
        h3.next_to_update = finder.next_to_update;
        Some(h3)
    } else {
        None
    };

    // C: `sufficient_len = MIN(cParams->targetLength, ZSTD_OPT_NUM - 1)`
    // (`zstd_opt.c:602`), and nothing else. This used to substitute
    // `good_enough_match_length` when `target_length` was zero, which no level
    // can produce -- every optimal row carries at least 12 -- but a
    // `ParameterOverrides { strategy: Some(BinaryTreeOpt), .. }` on a level
    // whose own row is `Fast` does, and there the substitution silently parsed
    // something upstream does not. A zero here really does mean "no match is
    // ever sufficient", and upstream pays the ratio for it.
    let sufficient_len = params.target_length.min(OPT_NUM - 1) as u32;
    // C: `minMatch = (cParams->minMatch == 3) ? 3 : 4` (`zstd_opt.c:1097`),
    // once for the whole block because the long-distance candidates are
    // measured against it before the first search.
    let min_match: usize = if params.min_match == 3 { 3 } else { 4 };
    let mut ldm_candidates = ldm.map(|sequences| LdmCandidates::new(sequences, block_len as u32));

    while ip < limit {
        // C: ZSTD_BtGetAllMatches returns 0 when ip < base + nextToUpdate
        // (the position was already inserted during a previous DP extension).
        // Without this guard, re-inserting zeroes the BST children, destroying
        // the tree structure built during the inner loop.
        //
        // C returns from *inside* the search and carries on: the position is
        // still one where a long-distance match can be offered, and one that
        // is long enough carries the parse on its own. So this leaves an empty
        // candidate list rather than skipping the position outright, and it is
        // the emptiness below that decides whether there is anything to do.
        if inserted_until > ip {
            candidates.clear();
        } else {
            if inserted_until < ip {
                finder.insert_range(src, inserted_until, ip, params.search_depth);
            }

            // Combined insert of ip + search (matches C's insertBtAndGetAllMatches)
            inserted_until = collect_optimal_matches_without_prefix(
                &mut candidates,
                &mut tree_matches,
                src,
                ip,
                repeat_offsets.values(),
                ip - anchor,
                params,
                finder,
                match_floor.at(ip),
                hash3.as_mut(),
                sufficient_len,
            );
        }
        if let Some(ldm_candidates) = ldm_candidates.as_mut() {
            ldm_candidates.offer(
                &mut candidates,
                (ip - block_start) as u32,
                (src.len() - ip) as u32,
                min_match as u32,
            );
        }
        if candidates.is_empty() {
            ip += 1;
            continue;
        }

        let horizon = OPT_NUM.min(src.len() - ip);
        let best = *candidates.last().expect("candidate list must be non-empty");
        if best.length > sufficient_len {
            let literal_length = (ip - anchor) as u32;
            emit_optimal_sequence(
                plan,
                src,
                &mut anchor,
                &mut repeat_offsets,
                &mut price_model,
                literal_length,
                best.offset_value,
                best.length,
            )?;
            ip = anchor;
            continue;
        }

        let litlen0 = (ip - anchor) as u32;
        // No bulk fill: init_optimal_match_nodes initializes positions
        // 0..last_pos and sets sentinel at last_pos+1, matching C which
        // never resets the opt[] array between DP iterations.
        nodes[0] = OptimalPathNode {
            price: price_model.ll_price(litlen0),
            off: 0,
            mlen: 0,
            litlen: litlen0,
            reps: repeat_offsets.values(),
        };

        let mut last_pos = init_optimal_match_nodes(
            &mut nodes[..=horizon],
            &candidates,
            &price_model,
            horizon,
            min_match,
        );

        // C's lastStretch: when the DP forward pass breaks due to a long
        // match (> sufficient_len, >= OPT_NUM, or >= iend), C saves the
        // UNCAPPED match as lastStretch and uses it directly for the final
        // sequence, bypassing the horizon-capped version in the opt table.
        let mut forced_last_stretch: Option<(u32, u32)> = None; // (offset_value, full_length)
        let mut forced_last_cur = 0usize;

        let mut cur = 1usize;
        while cur <= last_pos {
            let replaced_match = update_optimal_literal_node::<IS_ULTRA>(
                &mut nodes[..=horizon],
                cur,
                src[ip + cur - 1],
                &price_model,
            );

            // Lazy rep computation (C zstd_opt.c:1256-1261): compute rep
            // offsets only for the selected node at `cur`, not per-candidate.
            // If cur is a match end (litlen==0), derive reps from predecessor.
            // If cur is a literal, update_optimal_literal_node already copied
            // reps from cur-1.
            if nodes[cur].litlen == 0 {
                let prev = cur - nodes[cur].mlen as usize;
                let mut reps = RepeatOffsets::from_values(nodes[prev].reps);
                let _ = reps.resolve_values(nodes[prev].litlen, nodes[cur].off);
                nodes[cur].reps = reps.values();
            }

            let inr = ip + cur;

            // btultra match+1literal optimization (C zstd_opt.c:1219-1244):
            // When a literal overwrites a match, check if keeping the match
            // plus 1 trailing literal is cheaper than the literal run.
            if IS_ULTRA {
                if let Some(prev_match) = replaced_match {
                    let ll1_inc = price_model.ll_price(1) as i32 - price_model.ll_price(0) as i32;
                    if ll1_inc < 0 && cur + 1 <= horizon && inr < src.len() {
                        let with1literal = prev_match.price as i32
                            + price_model.literal_byte_price(src[inr]) as i32
                            + ll1_inc;
                        let new_litlen = nodes[cur].litlen + 1;
                        let with_more_literals = nodes[cur].price as i32
                            + price_model.literal_byte_price(src[inr]) as i32
                            + price_model.ll_price(new_litlen) as i32
                            - price_model.ll_price(nodes[cur].litlen) as i32;
                        if with1literal < with_more_literals
                            && (with1literal as u32) < nodes[cur + 1].price
                        {
                            let prev = cur - prev_match.mlen as usize;
                            let mut rep_offsets = RepeatOffsets::from_values(nodes[prev].reps);
                            // resolve_values matches C's ZSTD_newRep: takes
                            // the stored offset_value and the predecessor's
                            // literal_length (for the ll0 flag).
                            let _ = rep_offsets.resolve_values(nodes[prev].litlen, prev_match.off);
                            nodes[cur + 1] = OptimalPathNode {
                                price: with1literal as u32,
                                off: prev_match.off,
                                mlen: prev_match.mlen,
                                litlen: 1,
                                reps: rep_offsets.values(),
                            };
                            if last_pos < cur + 1 {
                                last_pos = cur + 1;
                            }
                        }
                    }
                }
            }

            if inr > limit {
                cur += 1;
                continue;
            }
            if cur == last_pos {
                break;
            }

            // Skip unpromising positions: matches C's optLevel==0 fast-skip.
            // If the next position already has a near-optimal price, match
            // collection here is unlikely to improve it. ~+6% speed, -0.01 ratio.
            if !IS_ULTRA
                && cur + 1 <= horizon
                && nodes[cur + 1].price <= nodes[cur].price + OPT_PRICE_UNIT / 2
            {
                cur += 1;
                continue;
            }

            // C: ZSTD_BtGetAllMatches guard — skip if already inserted. As in
            // the outer loop, an already-inserted position still gets its
            // long-distance candidate offered, so this empties the list rather
            // than skipping the position.
            if inserted_until > inr {
                candidates.clear();
            } else {
                if inserted_until < inr {
                    finder.insert_range(src, inserted_until, inr, params.search_depth);
                }
                inserted_until = collect_optimal_matches_without_prefix(
                    &mut candidates,
                    &mut tree_matches,
                    src,
                    inr,
                    nodes[cur].reps,
                    nodes[cur].litlen as usize,
                    params,
                    finder,
                    match_floor.at(inr),
                    hash3.as_mut(),
                    sufficient_len,
                );
            }
            if let Some(ldm_candidates) = ldm_candidates.as_mut() {
                ldm_candidates.offer(
                    &mut candidates,
                    (inr - block_start) as u32,
                    (src.len() - inr) as u32,
                    min_match as u32,
                );
            }
            {
                if !candidates.is_empty() {
                    // C: check break condition BEFORE update_optimal_match_nodes
                    // using the UNCAPPED longest match (matching C zstd_opt.c:1290-1301).
                    let best_candidate = candidates.last().expect("non-empty");
                    let uncapped_longest = best_candidate.length;
                    if uncapped_longest > sufficient_len
                        || cur + uncapped_longest as usize >= horizon
                        || inr + uncapped_longest as usize >= src.len()
                    {
                        // C: save lastStretch with FULL match length and break.
                        // C does NOT call the match pricing loop when breaking.
                        forced_last_stretch = Some((best_candidate.offset_value, uncapped_longest));
                        forced_last_cur = cur;
                        break;
                    }

                    update_optimal_match_nodes::<IS_ULTRA>(
                        &mut nodes[..=horizon],
                        cur,
                        &mut last_pos,
                        &candidates,
                        &price_model,
                        horizon,
                        min_match,
                    );
                }
            }
            cur += 1;
        }

        // C line 1337: sentinel after the inner loop for next iteration.
        if last_pos + 1 <= horizon {
            nodes[last_pos + 1].price = MAX_PRICE;
        }

        // C's lastStretch handling (zstd_opt.c:1296-1301, 1340-1365):
        // When the forward pass broke due to a long match, the final
        // sequence uses the UNCAPPED match from forced_last_stretch.
        // Run the normal backtrace up to forced_last_cur to emit all
        // preceding sequences, then emit the forced match. The
        // tail_literals from the backtrace become the literal length
        // for the forced match.
        if let Some((off, mlen)) = forced_last_stretch {
            let tail_literals = backtrace_optimal_path(
                &nodes[..=forced_last_cur],
                forced_last_cur,
                &mut backtrace_buf,
            );
            // No `ip` bookkeeping here: `ip = anchor` runs unconditionally
            // before the `continue` below, so both arms' writes were dead. That
            // leaves the empty case with nothing to do, since iterating an
            // empty backtrace emits nothing.
            for &(literal_length, offset_value, match_length) in &backtrace_buf {
                emit_optimal_sequence(
                    plan,
                    src,
                    &mut anchor,
                    &mut repeat_offsets,
                    &mut price_model,
                    literal_length,
                    offset_value,
                    match_length,
                )?;
            }
            // Emit the forced last stretch: tail_literals + mlen match.
            emit_optimal_sequence(
                plan,
                src,
                &mut anchor,
                &mut repeat_offsets,
                &mut price_model,
                tail_literals,
                off,
                mlen,
            )?;
            ip = anchor;
            continue;
        }

        let tail_literals =
            backtrace_optimal_path(&nodes[..=last_pos], last_pos, &mut backtrace_buf);
        if backtrace_buf.is_empty() {
            ip = anchor + tail_literals as usize;
            continue;
        }

        for &(literal_length, offset_value, match_length) in &backtrace_buf {
            emit_optimal_sequence(
                plan,
                src,
                &mut anchor,
                &mut repeat_offsets,
                &mut price_model,
                literal_length,
                offset_value,
                match_length,
            )?;
        }
        // C: ip = anchor after the last emitted sequence. Trailing literal
        // positions (where the DP chose a literal over a match at the window
        // boundary) are re-evaluated by the next parse round, which may find
        // matches there from a fresh starting position.
        ip = anchor;
    }

    plan.literals.extend_from_slice(&src[anchor..]);
    plan.repeat_offsets = repeat_offsets;
    // Persist accumulated pricing statistics for cross-block reuse,
    // matching C's optState_t frequency persistence.
    plan.opt_price_state = Some(price_model.to_persisted_state());
    // Carry forward the BST insertion frontier for the next block,
    // matching C's ms->nextToUpdate persistence.
    finder.next_to_update = inserted_until;
    // Return scratch buffers for reuse.
    plan.opt_nodes = nodes;
    plan.opt_tree_matches = tree_matches;
    plan.opt_candidates = candidates;
    plan.opt_backtrace = backtrace_buf;
    plan.opt_hash3 = hash3;
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn plan_sequences_optimal_with_prefix_into<const IS_ULTRA: bool>(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    prefix_chain: PrefixChain<'_>,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prefix_finder: &BinaryTreeFinder,
    src_finder: &mut BinaryTreeFinder,
) -> Result<()> {
    let block_len = src.len().saturating_sub(block_start);
    plan.reset_for_block(block_len);
    if block_len < MIN_MATCH {
        plan.literals.extend_from_slice(&src[block_start..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    // Cross-block frequency persistence (matching C's optState_t):
    // If accumulated pricing state exists from a previous block, rescale
    // and reuse it.  Otherwise initialize from dictionary seed (trained)
    // or source scan (raw / no dict).  C persists accumulated stats
    // unconditionally across blocks within a frame.
    let mut price_model = if let Some(mut persisted) = plan.opt_price_state.take() {
        persisted.rescale();
        OptimalPriceModel::<IS_ULTRA>::from_persisted_state(
            &persisted,
            params.long_offset_penalty,
            params.compressed_literals,
        )
    } else {
        let dict_seed = plan.opt_dict_price_seed.as_ref();
        OptimalPriceModel::<IS_ULTRA>::new(
            &src[block_start..],
            dict_seed,
            params.long_offset_penalty,
            params.compressed_literals,
        )
    };
    let mut repeat_offsets = repeat_offsets;
    let mut anchor = block_start;
    let mut ip = block_start;
    let limit = src.len().saturating_sub(8);
    let mut inserted_until = block_start;
    let mut nodes = core::mem::take(&mut plan.opt_nodes);
    // Kept only for its allocation: every entry has to start unset. Growing the
    // buffer without resetting what was already in it left the previous parse's
    // prices in place, and a position this parse never reaches but does read
    // then compares against a real price instead of an unreachable one. The
    // symptom is a valid frame that differs from what the same input produces on
    // a fresh encoder, so a reused `Encoder` quietly compressed worse on every
    // other call. C writes `ZSTD_MAX_PRICE` into these positions rather than
    // relying on what was there.
    nodes.clear();
    nodes.resize(OPT_NUM + 1, OptimalPathNode::UNSET);
    let mut tree_matches = core::mem::take(&mut plan.opt_tree_matches);
    let mut candidates = core::mem::take(&mut plan.opt_candidates);
    let mut backtrace_buf = core::mem::take(&mut plan.opt_backtrace);
    // HC3: take or create the 3-byte hash table when min_match == 3.
    let mut hash3 = if params.min_match == 3 {
        let mut h3 = plan
            .opt_hash3
            .take()
            .unwrap_or_else(|| Hash3Table::new(params.window_log));
        h3.next_to_update = block_start;
        Some(h3)
    } else {
        None
    };

    // C: `sufficient_len = MIN(cParams->targetLength, ZSTD_OPT_NUM - 1)`
    // (`zstd_opt.c:602`), and nothing else. This used to substitute
    // `good_enough_match_length` when `target_length` was zero, which no level
    // can produce -- every optimal row carries at least 12 -- but a
    // `ParameterOverrides { strategy: Some(BinaryTreeOpt), .. }` on a level
    // whose own row is `Fast` does, and there the substitution silently parsed
    // something upstream does not. A zero here really does mean "no match is
    // ever sufficient", and upstream pays the ratio for it.
    let sufficient_len = params.target_length.min(OPT_NUM - 1) as u32;

    while ip < limit {
        // C: ZSTD_BtGetAllMatches guard — skip if already inserted.
        if inserted_until > ip {
            ip += 1;
            continue;
        }

        if inserted_until < ip {
            src_finder.insert_range(src, inserted_until, ip, params.search_depth);
        }

        inserted_until = collect_optimal_matches_with_prefix(
            &mut candidates,
            &mut tree_matches,
            prefix_chain,
            src,
            ip,
            repeat_offsets.values(),
            ip - anchor,
            params,
            prefix_low,
            source_low,
            prefix_finder,
            src_finder,
            hash3.as_mut(),
            sufficient_len,
        );
        if candidates.is_empty() {
            ip += 1;
            continue;
        }

        let horizon = OPT_NUM.min(src.len() - ip);
        let best = *candidates.last().expect("candidate list must be non-empty");

        if best.length > sufficient_len {
            let literal_length = (ip - anchor) as u32;
            emit_optimal_sequence(
                plan,
                src,
                &mut anchor,
                &mut repeat_offsets,
                &mut price_model,
                literal_length,
                best.offset_value,
                best.length,
            )?;
            ip = anchor;
            continue;
        }

        let litlen0 = (ip - anchor) as u32;
        nodes[0] = OptimalPathNode {
            price: price_model.ll_price(litlen0),
            off: 0,
            mlen: 0,
            litlen: litlen0,
            reps: repeat_offsets.values(),
        };

        // C: initial match setup scans upward, unconditionally.
        let min_match = if params.min_match == 3 { 3 } else { 4 };
        let mut last_pos = init_optimal_match_nodes(
            &mut nodes[..=horizon],
            &candidates,
            &price_model,
            horizon,
            min_match,
        );

        let mut forced_last_stretch: Option<(u32, u32)> = None;
        let mut forced_last_cur = 0usize;

        let mut cur = 1usize;
        while cur <= last_pos {
            let replaced_match = update_optimal_literal_node::<IS_ULTRA>(
                &mut nodes[..=horizon],
                cur,
                src[ip + cur - 1],
                &price_model,
            );

            // Lazy rep computation (C zstd_opt.c:1256-1261).
            if nodes[cur].litlen == 0 {
                let prev = cur - nodes[cur].mlen as usize;
                let mut reps = RepeatOffsets::from_values(nodes[prev].reps);
                let _ = reps.resolve_values(nodes[prev].litlen, nodes[cur].off);
                nodes[cur].reps = reps.values();
            }

            let inr = ip + cur;

            // btultra match+1literal optimization (C zstd_opt.c:1219-1244).
            if IS_ULTRA {
                if let Some(prev_match) = replaced_match {
                    let ll1_inc = price_model.ll_price(1) as i32 - price_model.ll_price(0) as i32;
                    if ll1_inc < 0 && cur + 1 <= horizon && inr < src.len() {
                        let with1literal = prev_match.price as i32
                            + price_model.literal_byte_price(src[inr]) as i32
                            + ll1_inc;
                        let new_litlen = nodes[cur].litlen + 1;
                        let with_more_literals = nodes[cur].price as i32
                            + price_model.literal_byte_price(src[inr]) as i32
                            + price_model.ll_price(new_litlen) as i32
                            - price_model.ll_price(nodes[cur].litlen) as i32;
                        if with1literal < with_more_literals
                            && (with1literal as u32) < nodes[cur + 1].price
                        {
                            let prev = cur - prev_match.mlen as usize;
                            let mut rep_offsets = RepeatOffsets::from_values(nodes[prev].reps);
                            let _ = rep_offsets.resolve_values(nodes[prev].litlen, prev_match.off);
                            nodes[cur + 1] = OptimalPathNode {
                                price: with1literal as u32,
                                off: prev_match.off,
                                mlen: prev_match.mlen,
                                litlen: 1,
                                reps: rep_offsets.values(),
                            };
                            if last_pos < cur + 1 {
                                last_pos = cur + 1;
                            }
                        }
                    }
                }
            }

            if inr > limit {
                cur += 1;
                continue;
            }
            if cur == last_pos {
                break;
            }

            // Skip unpromising positions (optLevel==0 only).
            if !IS_ULTRA
                && cur + 1 <= horizon
                && nodes[cur + 1].price <= nodes[cur].price + OPT_PRICE_UNIT / 2
            {
                cur += 1;
                continue;
            }

            // C: ZSTD_BtGetAllMatches guard — skip if already inserted.
            if inserted_until > inr {
                cur += 1;
                continue;
            }
            if inserted_until < inr {
                src_finder.insert_range(src, inserted_until, inr, params.search_depth);
            }
            inserted_until = collect_optimal_matches_with_prefix(
                &mut candidates,
                &mut tree_matches,
                prefix_chain,
                src,
                inr,
                nodes[cur].reps,
                nodes[cur].litlen as usize,
                params,
                prefix_low,
                source_low,
                prefix_finder,
                src_finder,
                hash3.as_mut(),
                sufficient_len,
            );
            {
                if !candidates.is_empty() {
                    let best_candidate = candidates.last().expect("non-empty");
                    let uncapped_longest = best_candidate.length;
                    if uncapped_longest > sufficient_len
                        || cur + uncapped_longest as usize >= horizon
                        || inr + uncapped_longest as usize >= src.len()
                    {
                        forced_last_stretch = Some((best_candidate.offset_value, uncapped_longest));
                        forced_last_cur = cur;
                        break;
                    }

                    update_optimal_match_nodes::<IS_ULTRA>(
                        &mut nodes[..=horizon],
                        cur,
                        &mut last_pos,
                        &candidates,
                        &price_model,
                        horizon,
                        min_match,
                    );
                }
            }
            cur += 1;
        }

        if last_pos + 1 <= horizon {
            nodes[last_pos + 1].price = MAX_PRICE;
        }

        if let Some((off, mlen)) = forced_last_stretch {
            let tail_literals = backtrace_optimal_path(
                &nodes[..=forced_last_cur],
                forced_last_cur,
                &mut backtrace_buf,
            );
            // No `ip` bookkeeping here: `ip = anchor` runs unconditionally
            // before the `continue` below, so both arms' writes were dead. That
            // leaves the empty case with nothing to do, since iterating an
            // empty backtrace emits nothing.
            for &(literal_length, offset_value, match_length) in &backtrace_buf {
                emit_optimal_sequence(
                    plan,
                    src,
                    &mut anchor,
                    &mut repeat_offsets,
                    &mut price_model,
                    literal_length,
                    offset_value,
                    match_length,
                )?;
            }
            emit_optimal_sequence(
                plan,
                src,
                &mut anchor,
                &mut repeat_offsets,
                &mut price_model,
                tail_literals,
                off,
                mlen,
            )?;
            ip = anchor;
            continue;
        }

        let tail_literals =
            backtrace_optimal_path(&nodes[..=last_pos], last_pos, &mut backtrace_buf);

        if backtrace_buf.is_empty() {
            ip = anchor + tail_literals as usize;
            continue;
        }

        for &(literal_length, offset_value, match_length) in &backtrace_buf {
            emit_optimal_sequence(
                plan,
                src,
                &mut anchor,
                &mut repeat_offsets,
                &mut price_model,
                literal_length,
                offset_value,
                match_length,
            )?;
        }
        ip = anchor;
    }

    plan.literals.extend_from_slice(&src[anchor..]);
    plan.repeat_offsets = repeat_offsets;
    plan.opt_price_state = Some(price_model.to_persisted_state());
    plan.opt_nodes = nodes;
    plan.opt_tree_matches = tree_matches;
    plan.opt_candidates = candidates;
    plan.opt_backtrace = backtrace_buf;
    plan.opt_hash3 = hash3;
    Ok(())
}

/// Two-phase optimal parser: separate source BST (mutable, inserted
/// incrementally) and dictionary BST (pre-built, read-only). Matches C's
/// architecture where `ms->chainTable/hashTable` is source-only and
/// `dms->chainTable/hashTable` is the pre-built dictionary.
pub(crate) fn plan_sequences_optimal_with_prefix_two_phase_into<const IS_ULTRA: bool>(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    prefix_chain: PrefixChain<'_>,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    match_floor: PrefixedMatchFloor,
    source_finder: &mut BinaryTreeFinder,
    dict_finder: &BinaryTreeFinder,
    ldm: Option<&[RawSequence]>,
) -> Result<()> {
    let block_len = src.len().saturating_sub(block_start);
    plan.reset_for_block(block_len);
    if block_len < MIN_MATCH {
        plan.literals.extend_from_slice(&src[block_start..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    // extDict mode: when the dictionary is copied rather than attached,
    // the source BST was pre-populated with dict entries at biased positions
    // (2..dict_len+1) matching C's ZSTD_WINDOW_START_INDEX. Build virtual_buf
    // with 2-byte padding so positions align, and use unified coordinates for
    // all BST operations. This matches C's byCopyingCDict + extDict path.
    //
    // dictMatchState mode: when the dictionary is attached, keep the existing
    // two-phase approach with source-relative coordinates (prefix_len=0 for
    // source BST, prefix_len=dict_len for virtual_buf byte access).
    let ext_dict_mode = !params.dictionary_attaches;
    const DICT_POS_BIAS: usize = 2;
    let dict_len = prefix_chain.len();
    let prefix_len = if ext_dict_mode {
        dict_len + DICT_POS_BIAS
    } else {
        dict_len
    };
    let mut virtual_buf = Vec::with_capacity(prefix_len + src.len());
    if ext_dict_mode {
        virtual_buf.extend_from_slice(&[0u8; DICT_POS_BIAS]);
    }
    for segment in prefix_chain.segments {
        virtual_buf.extend_from_slice(segment);
    }
    virtual_buf.extend_from_slice(src);
    // The floor as this parser sees it, resolved at the position doing the
    // looking and shifted by the padding `virtual_buf` carries in extDict mode.
    let floor_at = |pos: usize| {
        let (prefix_low, source_low) = match_floor.at(pos);
        let prefix_low = if ext_dict_mode {
            prefix_low + DICT_POS_BIAS
        } else {
            prefix_low
        };
        (prefix_low, source_low)
    };

    let mut price_model = if let Some(mut persisted) = plan.opt_price_state.take() {
        persisted.rescale();
        OptimalPriceModel::<IS_ULTRA>::from_persisted_state(
            &persisted,
            params.long_offset_penalty,
            params.compressed_literals,
        )
    } else {
        let dict_seed = plan.opt_dict_price_seed.as_ref();
        OptimalPriceModel::<IS_ULTRA>::new(
            &src[block_start..],
            dict_seed,
            params.long_offset_penalty,
            params.compressed_literals,
        )
    };
    let mut repeat_offsets = repeat_offsets;
    let mut anchor = block_start;
    let mut ip = block_start;
    let limit = src.len().saturating_sub(8);
    // C carries next_to_update across blocks — do NOT reset to block_start.
    // Positions between the previous block's next_to_update and block_start
    // get inserted by the next update_tree_unified call, matching C's
    // ZSTD_updateTree_internal which starts from ms->nextToUpdate.
    let mut nodes = core::mem::take(&mut plan.opt_nodes);
    // Kept only for its allocation: every entry has to start unset. Growing the
    // buffer without resetting what was already in it left the previous parse's
    // prices in place, and a position this parse never reaches but does read
    // then compares against a real price instead of an unreachable one. The
    // symptom is a valid frame that differs from what the same input produces on
    // a fresh encoder, so a reused `Encoder` quietly compressed worse on every
    // other call. C writes `ZSTD_MAX_PRICE` into these positions rather than
    // relying on what was there.
    nodes.clear();
    nodes.resize(OPT_NUM + 1, OptimalPathNode::UNSET);
    let mut tree_matches = core::mem::take(&mut plan.opt_tree_matches);
    let mut dict_match_buf = core::mem::take(&mut plan.opt_dict_matches);
    let mut ldm_candidates = ldm.map(|sequences| LdmCandidates::new(sequences, block_len as u32));
    let mut candidates = core::mem::take(&mut plan.opt_candidates);
    let mut backtrace_buf = core::mem::take(&mut plan.opt_backtrace);
    let mut hash3 = if params.min_match == 3 {
        let mut h3 = plan
            .opt_hash3
            .take()
            .unwrap_or_else(|| Hash3Table::new(params.window_log));
        h3.next_to_update = block_start;
        Some(h3)
    } else {
        None
    };

    // C: `sufficient_len = MIN(cParams->targetLength, ZSTD_OPT_NUM - 1)`
    // (`zstd_opt.c:602`), and nothing else. This used to substitute
    // `good_enough_match_length` when `target_length` was zero, which no level
    // can produce -- every optimal row carries at least 12 -- but a
    // `ParameterOverrides { strategy: Some(BinaryTreeOpt), .. }` on a level
    // whose own row is `Fast` does, and there the substitution silently parsed
    // something upstream does not. A zero here really does mean "no match is
    // ever sufficient", and upstream pays the ratio for it.
    let sufficient_len = params.target_length.min(OPT_NUM - 1) as u32;

    // C: `ip += (ip==prefixStart)` — skip the first source byte in dict
    // mode.  In C, prefixStart marks the first source byte after the
    // dictionary.  This is block_start == 0 for the first block.
    if ip == 0 {
        ip = 1;
    }

    while ip < limit {
        let (prefix_low, source_low) = floor_at(ip);
        // C: ZSTD_btGetAllMatches_internal returns 0 when ip < nextToUpdate
        // (zstd_opt.c:846). Skip positions in the "already covered" zone.
        if ip < source_finder.next_to_update {
            ip += 1;
            continue;
        }

        // extDict: unified coordinates (prefix_len > 0 for BST).
        // dictMatchState: source-relative coordinates (prefix_len=0 for BST).
        if ext_dict_mode {
            source_finder.update_tree_unified(&virtual_buf, prefix_len, ip, params.search_depth);
        } else {
            source_finder.update_tree_unified(src, 0, ip, params.search_depth);
        }

        collect_optimal_matches_two_phase(
            &mut candidates,
            &mut tree_matches,
            &mut dict_match_buf,
            &virtual_buf,
            prefix_len,
            ip,
            repeat_offsets.values(),
            ip - anchor,
            params,
            prefix_low,
            source_low,
            source_finder,
            dict_finder,
            hash3.as_mut(),
            ext_dict_mode,
            sufficient_len,
        );
        if let Some(ldm_candidates) = ldm_candidates.as_mut() {
            ldm_candidates.offer(
                &mut candidates,
                (ip - block_start) as u32,
                (src.len() - ip) as u32,
                params.min_match,
            );
        }
        if candidates.is_empty() {
            ip += 1;
            continue;
        }

        let horizon = OPT_NUM.min(src.len() - ip);
        let best = *candidates.last().expect("candidate list must be non-empty");

        if best.length > sufficient_len {
            let literal_length = (ip - anchor) as u32;
            emit_optimal_sequence(
                plan,
                src,
                &mut anchor,
                &mut repeat_offsets,
                &mut price_model,
                literal_length,
                best.offset_value,
                best.length,
            )?;
            ip = anchor;
            continue;
        }

        let litlen0 = (ip - anchor) as u32;
        nodes[0] = OptimalPathNode {
            price: price_model.ll_price(litlen0),
            off: 0,
            mlen: 0,
            litlen: litlen0,
            reps: repeat_offsets.values(),
        };

        let min_match = if params.min_match == 3 { 3 } else { 4 };
        let mut last_pos = init_optimal_match_nodes(
            &mut nodes[..=horizon],
            &candidates,
            &price_model,
            horizon,
            min_match,
        );

        // C's lastStretch: when the DP forward pass breaks due to a long
        // match (> sufficient_len, >= OPT_NUM, or >= iend), C saves the
        // UNCAPPED match as lastStretch and uses it directly for the final
        // sequence, bypassing the horizon-capped version in the opt table.
        let mut forced_last_stretch: Option<(u32, u32)> = None; // (offset_value, full_length)
        let mut forced_last_cur = 0usize;

        let mut cur = 1usize;
        while cur <= last_pos {
            let replaced_match = update_optimal_literal_node::<IS_ULTRA>(
                &mut nodes[..=horizon],
                cur,
                src[ip + cur - 1],
                &price_model,
            );

            // Lazy rep computation (C zstd_opt.c:1256-1261).
            if nodes[cur].litlen == 0 {
                let prev = cur - nodes[cur].mlen as usize;
                let mut reps = RepeatOffsets::from_values(nodes[prev].reps);
                let _ = reps.resolve_values(nodes[prev].litlen, nodes[cur].off);
                nodes[cur].reps = reps.values();
            }

            let inr = ip + cur;
            let (prefix_low, source_low) = floor_at(inr);

            // btultra match+1literal optimization
            if IS_ULTRA {
                if let Some(prev_match) = replaced_match {
                    let ll1_inc = price_model.ll_price(1) as i32 - price_model.ll_price(0) as i32;
                    if ll1_inc < 0 && cur + 1 <= horizon && inr < src.len() {
                        let with1literal = prev_match.price as i32
                            + price_model.literal_byte_price(src[inr]) as i32
                            + ll1_inc;
                        let new_litlen = nodes[cur].litlen + 1;
                        let with_more_literals = nodes[cur].price as i32
                            + price_model.literal_byte_price(src[inr]) as i32
                            + price_model.ll_price(new_litlen) as i32
                            - price_model.ll_price(nodes[cur].litlen) as i32;
                        if with1literal < with_more_literals
                            && (with1literal as u32) < nodes[cur + 1].price
                        {
                            let prev = cur - prev_match.mlen as usize;
                            let mut rep_offsets = RepeatOffsets::from_values(nodes[prev].reps);
                            let _ = rep_offsets.resolve_values(nodes[prev].litlen, prev_match.off);
                            nodes[cur + 1] = OptimalPathNode {
                                price: with1literal as u32,
                                off: prev_match.off,
                                mlen: prev_match.mlen,
                                litlen: 1,
                                reps: rep_offsets.values(),
                            };
                            if last_pos < cur + 1 {
                                last_pos = cur + 1;
                            }
                        }
                    }
                }
            }

            if inr > limit {
                cur += 1;
                continue;
            }
            if cur == last_pos {
                break;
            }

            // Skip unpromising positions (optLevel==0 only).
            if !IS_ULTRA
                && cur + 1 <= horizon
                && nodes[cur + 1].price <= nodes[cur].price + OPT_PRICE_UNIT / 2
            {
                cur += 1;
                continue;
            }

            // C: ZSTD_btGetAllMatches_internal returns 0 when ip < nextToUpdate.
            // When a long match pushed next_to_update past inr, skip this position.
            if inr < source_finder.next_to_update {
                candidates.clear();
            } else {
                if ext_dict_mode {
                    source_finder.update_tree_unified(
                        &virtual_buf,
                        prefix_len,
                        inr,
                        params.search_depth,
                    );
                } else {
                    source_finder.update_tree_unified(src, 0, inr, params.search_depth);
                }

                collect_optimal_matches_two_phase(
                    &mut candidates,
                    &mut tree_matches,
                    &mut dict_match_buf,
                    &virtual_buf,
                    prefix_len,
                    inr,
                    nodes[cur].reps,
                    nodes[cur].litlen as usize,
                    params,
                    prefix_low,
                    source_low,
                    source_finder,
                    dict_finder,
                    hash3.as_mut(),
                    ext_dict_mode,
                    sufficient_len,
                );
                if let Some(ldm_candidates) = ldm_candidates.as_mut() {
                    ldm_candidates.offer(
                        &mut candidates,
                        (inr - block_start) as u32,
                        (src.len() - inr) as u32,
                        params.min_match,
                    );
                }
            }
            {
                if !candidates.is_empty() {
                    // C: check break condition BEFORE update_optimal_match_nodes
                    // using the UNCAPPED longest match (matching C zstd_opt.c:1290-1301).
                    //
                    let best_candidate = candidates.last().expect("non-empty");
                    let uncapped_longest = best_candidate.length;
                    if uncapped_longest > sufficient_len
                        || cur + uncapped_longest as usize >= horizon
                        || inr + uncapped_longest as usize >= src.len()
                    {
                        // C: save lastStretch with FULL match length and break.
                        // C does NOT call the match pricing loop when breaking.
                        forced_last_stretch = Some((best_candidate.offset_value, uncapped_longest));
                        forced_last_cur = cur;
                        break;
                    }

                    update_optimal_match_nodes::<IS_ULTRA>(
                        &mut nodes[..=horizon],
                        cur,
                        &mut last_pos,
                        &candidates,
                        &price_model,
                        horizon,
                        min_match,
                    );
                }
            }
            cur += 1;
        }

        if last_pos + 1 <= horizon {
            nodes[last_pos + 1].price = MAX_PRICE;
        }

        if let Some((off, mlen)) = forced_last_stretch {
            let tail_literals = backtrace_optimal_path(
                &nodes[..=forced_last_cur],
                forced_last_cur,
                &mut backtrace_buf,
            );
            // No `ip` bookkeeping here: `ip = anchor` runs unconditionally
            // before the `continue` below, so both arms' writes were dead. That
            // leaves the empty case with nothing to do, since iterating an
            // empty backtrace emits nothing.
            for &(literal_length, offset_value, match_length) in &backtrace_buf {
                emit_optimal_sequence(
                    plan,
                    src,
                    &mut anchor,
                    &mut repeat_offsets,
                    &mut price_model,
                    literal_length,
                    offset_value,
                    match_length,
                )?;
            }
            // Emit the forced last stretch: tail_literals + mlen match.
            emit_optimal_sequence(
                plan,
                src,
                &mut anchor,
                &mut repeat_offsets,
                &mut price_model,
                tail_literals,
                off,
                mlen,
            )?;
            ip = anchor;
            continue;
        }

        let tail_literals =
            backtrace_optimal_path(&nodes[..=last_pos], last_pos, &mut backtrace_buf);

        if backtrace_buf.is_empty() {
            ip = anchor + tail_literals as usize;
            continue;
        }

        for &(literal_length, offset_value, match_length) in &backtrace_buf {
            emit_optimal_sequence(
                plan,
                src,
                &mut anchor,
                &mut repeat_offsets,
                &mut price_model,
                literal_length,
                offset_value,
                match_length,
            )?;
        }
        // C: ip = anchor after the last emitted sequence. Trailing literal
        // positions are re-evaluated by the next parse round.
        ip = anchor;
    }

    plan.literals.extend_from_slice(&src[anchor..]);
    plan.repeat_offsets = repeat_offsets;
    plan.opt_price_state = Some(price_model.to_persisted_state());
    plan.opt_nodes = nodes;
    plan.opt_tree_matches = tree_matches;
    plan.opt_dict_matches = dict_match_buf;
    plan.opt_candidates = candidates;
    plan.opt_backtrace = backtrace_buf;
    plan.opt_hash3 = hash3;
    Ok(())
}
