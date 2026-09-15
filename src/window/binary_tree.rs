use super::*;

/// Sentinel marking an unsorted entry in the DUBT chain (C's ZSTD_DUBT_UNSORTED_MARK).
const DUBT_UNSORTED_MARK: u32 = NO_POS - 1;

/// Sentinel index for child slots that should not be written to (equivalent to C's dummy32 pointer).
const DUMMY_IDX: usize = usize::MAX;

/// Offset applied to every prefix position stored in a tree, C's
/// `ZSTD_WINDOW_START_INDEX`: dictionary positions there start at 2, not 0, and
/// biasing ours to match makes `bt_low` and `match_low` behave identically to
/// `ZSTD_insertBt1` during CDict construction. Source positions carry no bias,
/// so the two regions of a tree are indexed differently and any bound crossing
/// between them has to be converted — see
/// [`BinaryTreeFinder::stored_index_floor`].
const DICT_POS_BIAS: usize = 2;

#[derive(Debug, Clone)]
pub(crate) struct PreparedBinaryTreeDictionaryTables {
    pub(crate) prefix_finder: Arc<BinaryTreeFinder>,
}

impl PreparedBinaryTreeDictionaryTables {
    pub(crate) fn prefix_finder(&self) -> Arc<BinaryTreeFinder> {
        self.prefix_finder.clone()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BinaryTreeFinder {
    pub(crate) heads: Vec<u32>,
    /// Interleaved children array matching C's `bt[2*i]` layout:
    /// `children[2*i]` = smaller child, `children[2*i+1]` = larger child.
    pub(crate) children: Vec<u32>,
    pub(crate) hash_bits: u32,
    pub(crate) min_match: u32,
    pub(crate) bt_mask: usize,
    pub(crate) default_search_depth: usize,
    /// Matches C's `ms->nextToUpdate`: positions below this have already been
    /// covered by a long match found during BST search and can be skipped.
    pub(crate) next_to_update: usize,
    /// When > 0, the DUBT operates in virtual coordinate space where
    /// positions 0..prefix_len are dictionary entries and positions
    /// >= prefix_len are source entries. Matches C's model where
    /// > ZSTD_loadDictionaryContent inserts dictionary positions into
    /// > the source tree before compression begins.
    pub(crate) prefix_len: usize,
    /// The window this tree's *inserts* bound their traversal by, as C's
    /// `ZSTD_insertBt1` does with
    /// `ZSTD_getLowestMatchIndex(ms, target, cParams->windowLog)`.
    ///
    /// 31 means unbounded, which is what the callers that build a tree over a
    /// buffer no larger than one window want: there, C's own floor never rises
    /// off `lowLimit` either.
    pub(crate) window_log: u32,
}

impl BinaryTreeFinder {
    pub(crate) fn new(hash_bits: u32, chain_log: u32, min_match: u32) -> Self {
        let hash_bits = hash_bits.clamp(10, MAX_MATCH_HASH_BITS);
        let bt_log = binary_tree_cycle_log(chain_log);
        let bt_size = 1usize << bt_log;
        let bt_mask = bt_size - 1;
        Self {
            // C memsets all tables to 0 (ZSTD_cwksp_clean_tables). Hash heads
            // pointing to position 0 create a "phantom root" that unfilled BST
            // children can traverse through during insertion, building richer
            // tree structures that enable finding matches across hash collisions.
            heads: vec![0; 1usize << hash_bits],
            children: vec![0; bt_size * 2],
            hash_bits,
            min_match: min_match.clamp(3, 6),
            bt_mask,
            default_search_depth: 32,
            // C starts data at ZSTD_WINDOW_START_INDEX (2), so positions 0-1
            // are phantom padding never inserted into the BST. Match this by
            // starting insertion at position 2.
            next_to_update: 2,
            prefix_len: 0,
            window_log: 31,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.heads.fill(0);
        self.children.fill(0);
        self.next_to_update = 2;
        self.prefix_len = 0;
    }

    /// The alignment [`shift_positions`](Self::shift_positions) requires, which
    /// is the length of the cycle `children` is indexed by.
    pub(crate) fn rebase_period(&self) -> usize {
        self.bt_mask + 1
    }

    /// Rebase every position by `delta`, which must be a multiple of
    /// [`rebase_period`](Self::rebase_period).
    ///
    /// The tree is the one structure here that cannot be rebuilt from the bytes
    /// it describes. Its shape depends on the order positions were inserted in,
    /// and `ZSTD_updateDUBT` files them unsorted: only a search sorts them, and
    /// only as deep as `search_depth` reaches. Re-inserting a whole retained
    /// window in one go therefore leaves most of each bucket's chain unsorted
    /// and unreachable, which is measurably worse than the tree the parser had.
    ///
    /// `children` is indexed by `position & bt_mask`, so an aligned `delta` is
    /// what keeps each node in the slot its new position names. C arranges the
    /// same alignment deliberately: `ZSTD_window_correctOverflow` builds its
    /// correction out of `curr & cycleMask` plus whole cycles, so that "the
    /// least significant cycleLog bits of the indices must remain the same"
    /// (`zstd_compress_internal.h:1154`).
    ///
    /// Empty is `0` rather than [`NO_POS`], matching the zeroed tables
    /// [`reset`](Self::reset) leaves behind: a position rebased to zero is one
    /// the phantom-root traversal already terminates on.
    pub(crate) fn shift_positions(&mut self, delta: usize) {
        debug_assert!(
            delta.is_multiple_of(self.rebase_period()),
            "rebasing a binary tree by an unaligned delta moves nodes out of their slots"
        );
        debug_assert_eq!(
            self.prefix_len, 0,
            "a dictionary-prefixed tree has no buffer of its own to rebase against"
        );
        shift_raw_positions(&mut self.heads, delta, 0);
        shift_raw_positions_preserving(&mut self.children, delta, 0, DUBT_UNSORTED_MARK);
        // Never below the phantom padding the tree starts at, or the catch-up
        // would try to insert positions `new` deliberately never inserts.
        self.next_to_update = self.next_to_update.saturating_sub(delta).max(2);
    }

    /// Rebase every position by any `delta`, valid only where no live position
    /// reaches the cycle: `live_end` must be at most
    /// [`rebase_period`](Self::rebase_period).
    ///
    /// Under that condition `position & bt_mask` is the position itself, so the
    /// table is a plain array indexed by position and moving a node to the slot
    /// its new position names is a shift of the array. Alignment stops being a
    /// constraint because there is no wrapping left for it to protect.
    ///
    /// This is the case a wider buffer cannot buy its way out of: a cycle that
    /// exceeds the whole buffer would need the buffer to grow past the cycle,
    /// and the cycle is what is already large.
    pub(crate) fn shift_positions_by_slot(&mut self, delta: usize, live_end: usize) {
        debug_assert!(
            live_end <= self.rebase_period(),
            "a position past the cycle does not sit in the slot it names"
        );
        debug_assert_eq!(
            self.prefix_len, 0,
            "a dictionary-prefixed tree has no buffer of its own to rebase against"
        );
        // Two entries per position, so the slot shift is twice the byte shift.
        let len = self.children.len();
        let kept = len - (2 * delta).min(len);
        self.children.copy_within(len - kept.., 0);
        self.children[kept..].fill(0);
        shift_raw_positions(&mut self.heads, delta, 0);
        shift_raw_positions_preserving(&mut self.children[..kept], delta, 0, DUBT_UNSORTED_MARK);
        self.next_to_update = self.next_to_update.saturating_sub(delta).max(2);
    }

    /// Zero all hash and children tables, matching C's `ZSTD_cwksp_clean_tables`
    /// which memsets all table memory to 0 before compression.
    ///
    /// C's zeroed tables cause two behaviours that NO_POS initialization misses:
    /// "Phantom position 0" — every unfilled hash bucket points to position 0,
    /// so the first insertion at each bucket threads position 0 into the tree
    /// (terminated by the btLow break).
    pub(crate) fn zero_tables(&mut self) {
        self.heads.fill(0);
        self.children.fill(0);
    }

    /// Hash a source position using the mls-appropriate hash function,
    /// matching C's `ZSTD_hashPtr(ip, hashLog, mls)`.
    #[inline(always)]
    fn hash_src_at(&self, src: &[u8], pos: usize) -> usize {
        if pos + HASH_READ_SIZE <= src.len() {
            // C's ZSTD_hashPtr has no case for mls=3 — it falls through to
            // case 4 (ZSTD_hash4Ptr). Use max(min_match, 4) to match.
            hash_at_mls(src, pos, self.hash_bits, self.min_match.max(4))
        } else {
            hash_at(src, pos, self.hash_bits)
        }
    }

    /// Bound this tree's inserts by `window_log`, matching C's `ZSTD_insertBt1`.
    ///
    /// Without it the traversal runs to the bottom of the buffer and threads
    /// the tree through positions the window has already dropped, which cuts
    /// later searches short at exactly the distances that are still legal. See
    /// the note on [`Self::insert_range`].
    pub(crate) fn with_window_log(mut self, window_log: u32) -> Self {
        self.window_log = window_log;
        self
    }

    pub(crate) fn with_search_depth(mut self, depth: usize) -> Self {
        self.default_search_depth = depth;
        self
    }

    /// Insert a position into the binary tree with proper BST threading.
    /// Implements C's `ZSTD_insertBt1` algorithm with modular indexing.
    /// NOTE: C's ZSTD_C_PREDICT is disabled upstream (#ifdef never
    /// defined) because it "can create issues when hlog small <= 11".
    ///
    /// `window_low` is the biased index below which the traversal stops, and it
    /// belongs to the caller because C takes it from the *end* of the range
    /// being inserted rather than from `pos` — see [`Self::insert_range`], which
    /// computes it once and is the only caller that has the range to compute it
    /// from. It is not a constant: passing the `ZSTD_WINDOW_START_INDEX` bias
    /// unconditionally is only right while the history still fits in the window,
    /// and doing so threaded the tree through dropped positions at a cost of
    /// 10.93%.
    #[inline(always)]
    #[allow(unsafe_code)]
    pub(crate) fn insert_with_floor(
        &mut self,
        src: &[u8],
        pos: usize,
        search_depth: usize,
        window_low: usize,
    ) -> usize {
        // Bias positions by 2 to match C's ZSTD_WINDOW_START_INDEX. In C,
        // source data starts at window position 2; positions 0-1 are phantom.
        // All BST-stored positions are biased; data access uses pos directly.
        const BIAS: usize = 2;
        if pos + MIN_MATCH > src.len() {
            return 1;
        }
        let biased_pos = pos + BIAS;
        let slot = self.hash_src_at(src, pos);
        debug_assert!(slot < self.heads.len());
        let mut match_index = unsafe { *self.heads.get_unchecked(slot) };
        unsafe {
            *self.heads.get_unchecked_mut(slot) = biased_pos as u32;
        }

        let bt_mask = self.bt_mask;
        let bt_low = biased_pos.saturating_sub(bt_mask);
        // C bounds the traversal below by `windowLow`, not `btLow`:
        //   `for (; nbCompares && (matchIndex >= windowLow); --nbCompares)`
        // and asserts `windowLow > 0` — it never drops below
        // `ZSTD_WINDOW_START_INDEX`. `btLow` is a *different* bound, used only
        // for the in-loop break below, and it is 0 while the tree is still
        // smaller than the chain table. Bounding the loop by `btLow` alone let
        // the phantom entries at biased positions 0 and 1 — the zero-filled
        // hash heads and children slots — enter the loop body, where
        // `mi - BIAS` underflows: a panic under `debug_assertions` and a
        // wrapped index otherwise. `count_match_length_from` saturates such an
        // index to a zero-length match, so release builds already exited here
        // via the `actual_mi + match_len >= src_end` break; raising the floor
        // to `BIAS` reaches the same exit through the loop condition, without
        // the underflow.
        // `bt_low` is not part of the floor. Including it cut the traversal
        // short once the body outgrew the roll buffer, leaving the tree
        // shallower than C's from that point on.
        //
        // The floor is `window_low`, which the caller derives from
        // `ZSTD_getLowestMatchIndex(ms, target, windowLog)`. An earlier version
        // of this comment claimed C's `windowLow` "stays at
        // `ZSTD_WINDOW_START_INDEX`" on the contiguous no-dictionary path and
        // used `BIAS` alone. That is not what C does: `isDictionary` is false
        // here, so the function returns `withinWindow`, which becomes
        // `curr - maxDistance` as soon as the history outgrows the window
        // (`zstd_compress_internal.h:1395`). It only stays at `lowLimit` while
        // everything still fits, which is why the two agreed on short inputs
        // and diverged on long ones.
        //
        // Reading no matches is not a reason to skip the floor. This inserter
        // writes the tree's links, and running past `window_low` threads them
        // through positions the window has dropped; a later search follows
        // those links, hits its own floor, and stops before candidates that are
        // still in range. Measured on a megabyte of `wikipedia` at level 16
        // with a window of 17, that cost 10.93% and 541 extra sequences.
        let match_low = window_low.max(BIAS);
        let pos_masked = biased_pos & bt_mask;
        let pos_base = pos_masked << 1;

        // NOTE: Do NOT zero children[pos_base] here. C doesn't pre-zero;
        // it lets smallerPtr/largerPtr start pointing at the slot and the
        // final `*smallerPtr = *largerPtr = 0` handles cleanup. Pre-zeroing
        // destroys children of aliased positions when bt_mask is small,
        // breaking BST chains.

        let mut smaller_idx = pos_base;
        let mut larger_idx = pos_base | 1;

        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares = search_depth;
        let src_end = src.len();
        let mut best_length = 8usize;
        let mut match_end_idx = biased_pos + 8 + 1;

        while nb_compares > 0
            && (match_index as usize) < biased_pos
            && (match_index as usize) >= match_low
        {
            nb_compares -= 1;
            let mi = match_index as usize;
            let mi_masked = mi & bt_mask;
            let next_base = mi_masked << 1;
            debug_assert!(mi >= BIAS, "phantom position {mi} reached the loop body");
            let actual_mi = mi - BIAS;

            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len = count_match_length_from(src, actual_mi, pos, known_equal);

            if match_len > best_length {
                best_length = match_len;
                if mi + match_len > match_end_idx {
                    match_end_idx = mi + match_len;
                }
            }

            if pos + match_len >= src_end || actual_mi + match_len >= src_end {
                break;
            }

            debug_assert!(actual_mi + match_len < src_end && pos + match_len < src_end);
            if unsafe {
                *src.get_unchecked(actual_mi + match_len) < *src.get_unchecked(pos + match_len)
            } {
                debug_assert!(smaller_idx < self.children.len());
                unsafe {
                    *self.children.get_unchecked_mut(smaller_idx) = match_index;
                }
                common_len_smaller = match_len;
                if mi <= bt_low {
                    smaller_idx = DUMMY_IDX;
                    break;
                }
                debug_assert!((next_base | 1) < self.children.len());
                match_index = unsafe { *self.children.get_unchecked(next_base | 1) };
                smaller_idx = next_base | 1;
            } else {
                debug_assert!(larger_idx < self.children.len());
                unsafe {
                    *self.children.get_unchecked_mut(larger_idx) = match_index;
                }
                common_len_larger = match_len;
                if mi <= bt_low {
                    larger_idx = DUMMY_IDX;
                    break;
                }
                debug_assert!(next_base < self.children.len());
                match_index = unsafe { *self.children.get_unchecked(next_base) };
                larger_idx = next_base;
            }
        }

        if smaller_idx != DUMMY_IDX {
            debug_assert!(smaller_idx < self.children.len());
            unsafe {
                *self.children.get_unchecked_mut(smaller_idx) = 0;
            }
        }
        if larger_idx != DUMMY_IDX {
            debug_assert!(larger_idx < self.children.len());
            unsafe {
                *self.children.get_unchecked_mut(larger_idx) = 0;
            }
        }

        // Return skip distance matching C's ZSTD_insertBt1:
        // skip positions covered by long matches.
        let mut positions = 0usize;
        if best_length > 384 {
            positions = (best_length - 384).min(192);
        }
        positions.max(match_end_idx - (biased_pos + 8))
    }

    pub(crate) fn insert_range(
        &mut self,
        src: &[u8],
        start: usize,
        end: usize,
        search_depth: usize,
    ) {
        let end = end.min(hash_insert_end(src.len()));
        // One floor for the whole range, taken from its end. C does the same,
        // and says why: `ZSTD_insertBt1` computes
        // `windowLow = ZSTD_getLowestMatchIndex(ms, target, cParams->windowLog)`
        // because "we only need positions that will be in the window at the end
        // of the tree update". `target` is this range's `end`, not the position
        // being inserted, so the bound is deliberately the strict one.
        //
        // This is the opposite of a *search* floor, which has to be resolved at
        // every probe against that probe's own position. Both exist, they are
        // not interchangeable, and this one is the tree's shape rather than any
        // one match's legality.
        const BIAS: usize = 2;
        let target = end + BIAS;
        let max_distance = 1usize << self.window_log;
        let window_low = if target - BIAS > max_distance {
            target - max_distance
        } else {
            BIAS
        };
        let mut pos = start;
        while pos < end {
            let forward = self.insert_with_floor(src, pos, search_depth, window_low);
            pos += forward;
        }
    }

    /// Insert dictionary prefix positions into the unified tree, matching
    /// C's `ZSTD_updateTree(ms, iend-HASH_READ_SIZE, iend)` during
    /// `ZSTD_loadDictionaryContent`.  Key constraints:
    ///
    /// 1. Skip the last HASH_READ_SIZE (8) positions — can't compute a
    ///    full hash without reading into source bytes.
    /// 2. Limit match counting to the prefix boundary — don't read source
    ///    bytes during prefix insertion (source isn't "loaded" yet in C).
    /// 3. Use mls-aware hashing so prefix and source entries that start
    ///    with the same bytes share hash buckets.
    pub(crate) fn insert_prefix_into_unified(
        &mut self,
        prefix_chain: PrefixChain<'_>,
        search_depth: usize,
        window_log: u32,
    ) {
        let prefix_len = prefix_chain.len();
        if prefix_len <= HASH_READ_SIZE {
            return;
        }
        let target = prefix_len - HASH_READ_SIZE;
        let mut pos = 0usize;
        while pos < target {
            if pos + MIN_MATCH > prefix_len {
                pos += 1;
                continue;
            }
            // C's ZSTD_hashPtr has no case for mls=3 — it falls through to
            // case 4 (ZSTD_hash4Ptr). Use max(min_match, 4) to match.
            let hash_mls = self.min_match.max(4);
            let slot = hash_prefix_chain_at_mls(prefix_chain, &[], pos, self.hash_bits, hash_mls);
            let forward =
                self.insert_prefix_bt1(prefix_chain, slot, pos, target, window_log, search_depth);
            pos += forward;
        }
    }

    /// BST insertion for prefix positions matching C's `ZSTD_insertBt1`.
    /// Returns the number of positions to advance (>= 1), matching C's
    /// skip logic: `MAX(positions, matchEndIdx - (curr + 8))` where
    /// `positions` is a speed heuristic for very long matches.
    ///
    /// Match counting is bounded by `prefix_len` — never reads source
    /// bytes — matching C's `ZSTD_noDict` mode used during dictionary
    /// loading.
    fn insert_prefix_bt1(
        &mut self,
        prefix_chain: PrefixChain<'_>,
        slot: usize,
        pos: usize,
        _target: usize,
        window_log: u32,
        search_depth: usize,
    ) -> usize {
        let prefix_len = prefix_chain.len();
        let bt_mask = self.bt_mask;
        let biased_pos = pos + DICT_POS_BIAS;
        let bt_low = biased_pos.saturating_sub(bt_mask);
        let pos_masked = biased_pos & bt_mask;
        let pos_base = pos_masked << 1;
        // C: matchLow = ZSTD_getLowestPrefixIndex(ms, curr, windowLog)
        //    lowestValid = ZSTD_WINDOW_START_INDEX = 2.
        //    withinWindow = max(0, curr - (1<<windowLog)).
        //    matchLowest = max(withinWindow, lowestValid).
        let window_size = 1usize << window_log;
        let match_low = if biased_pos > window_size {
            biased_pos - window_size
        } else {
            DICT_POS_BIAS
        };

        let mut match_index = self.heads[slot];
        self.heads[slot] = biased_pos as u32;

        // C: children start zeroed; explicit init matches C's `*smallBase = *smallBase+1 = 0` pattern.
        self.children[pos_base] = 0;
        self.children[pos_base | 1] = 0;

        let mut smaller_idx = pos_base;
        let mut larger_idx = pos_base | 1;

        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares = search_depth;
        // C: bestLength = 8, matchEndIdx = curr+8+1
        let mut best_length = 8usize;
        let mut match_end_idx = biased_pos + 8 + 1;

        // NOTE: C's ZSTD_C_PREDICT is disabled upstream (#ifdef never
        // defined) — it "can create issues when hlog small <= 11".

        // C: for (; nbCompares && (matchIndex >= windowLow); --nbCompares)
        while nb_compares > 0
            && (match_index as usize) < biased_pos
            && (match_index as usize) >= match_low
        {
            nb_compares -= 1;
            let mi = match_index as usize;
            let mi_masked = mi & bt_mask;
            let next_base = mi_masked << 1;

            let known_equal = common_len_smaller.min(common_len_larger);
            let actual_mi = mi - DICT_POS_BIAS;

            // Count match length bounded by prefix_len — never read source bytes.
            let mut match_len = known_equal;
            while pos + match_len < prefix_len
                && actual_mi + match_len < prefix_len
                && prefix_chain.byte(pos + match_len) == prefix_chain.byte(actual_mi + match_len)
            {
                match_len += 1;
            }

            // C: track bestLength and matchEndIdx for skip logic
            if match_len > best_length {
                best_length = match_len;
                if match_len > match_end_idx.wrapping_sub(mi) {
                    match_end_idx = mi + match_len;
                }
            }

            if pos + match_len >= prefix_len || actual_mi + match_len >= prefix_len {
                break;
            }

            let pos_byte = prefix_chain.byte(pos + match_len);
            let match_byte = prefix_chain.byte(actual_mi + match_len);

            if match_byte < pos_byte {
                self.children[smaller_idx] = match_index;
                common_len_smaller = match_len;
                // C: if (matchIndex <= btLow) { smallerPtr=&dummy32; break; }
                if mi <= bt_low {
                    smaller_idx = DUMMY_IDX;
                    break;
                }
                match_index = self.children[next_base | 1];
                smaller_idx = next_base | 1;
            } else {
                self.children[larger_idx] = match_index;
                common_len_larger = match_len;
                if mi <= bt_low {
                    larger_idx = DUMMY_IDX;
                    break;
                }
                match_index = self.children[next_base];
                larger_idx = next_base;
            }
        }

        // C: `*smallerPtr = *largerPtr = 0;` — terminal children are 0.
        if smaller_idx != DUMMY_IDX {
            self.children[smaller_idx] = 0;
        }
        if larger_idx != DUMMY_IDX {
            self.children[larger_idx] = 0;
        }

        // C: return MAX(positions, matchEndIdx - (curr + 8))
        // where `positions` is a speed heuristic for very long matches.
        let mut positions = 0usize;
        if best_length > 384 {
            positions = 192.min(best_length - 384);
        }
        debug_assert!(match_end_idx > biased_pos + 8);
        positions.max(match_end_idx - (biased_pos + 8))
    }

    /// Pre-insert the current position into the hash table and clear its BST
    /// children. Matching C's `ZSTD_insertBtAndGetAllMatches` which does the
    /// hash insert + children clear BEFORE repcode/HC3 checks, ensuring early
    /// returns still leave the position registered in the hash table.
    ///
    /// Returns the old `match_index` that was in the hash table slot. The
    /// caller must pass this to `insert_and_collect_matches_unified` so the
    /// BST search can start from the correct chain entry.
    #[inline(always)]
    #[allow(unsafe_code)]
    pub(crate) fn pre_insert_hash_unified(
        &mut self,
        src: &[u8],
        prefix_len: usize,
        pos: usize,
    ) -> u32 {
        if pos + MIN_MATCH > src.len() {
            return NO_POS;
        }

        let current = prefix_len + pos;
        let slot = self.hash_src_at(src, pos);
        debug_assert!(slot < self.heads.len());
        let old_match_index = unsafe { *self.heads.get_unchecked(slot) };
        unsafe {
            *self.heads.get_unchecked_mut(slot) = current as u32;
        }

        // NOTE: Do NOT zero children[pos_base] here. C doesn't pre-zero
        // either; it lets smallerPtr/largerPtr start pointing at the slot
        // and the final `*smallerPtr = *largerPtr = 0` handles cleanup.
        // Pre-zeroing destroys children of aliased positions when bt_mask
        // is small (e.g., 1023), breaking BST chains.

        old_match_index
    }

    /// Combined insert + search for the unified tree. Inserts a source position
    /// into the tree (using virtual coordinates) and searches for matches across
    /// both dictionary and source ranges. Returns `(next_to_update, remaining_compares)`
    /// so the caller can pass the remaining budget to a dictionary search phase.
    ///
    /// The caller MUST call `pre_insert_hash_unified` first and pass the returned
    /// `prev_match_index` here. The hash insert + children clear are already done.
    #[inline(always)]
    #[allow(unsafe_code)]
    pub(crate) fn insert_and_collect_matches_unified(
        &mut self,
        dst: &mut Vec<MatchCandidate>,
        virtual_buf: &[u8],
        prefix_len: usize,
        pos: usize,
        search_depth: usize,
        prefix_low: usize,
        source_low: usize,
        min_match_length: usize,
        prev_match_index: u32,
    ) -> (usize, usize) {
        dst.clear();
        let src_len = virtual_buf.len() - prefix_len;
        if pos + MIN_MATCH > src_len {
            return (pos + 1, search_depth);
        }

        let virtual_len = virtual_buf.len();
        let current = prefix_len + pos;

        // Hash insert + children clear already done by pre_insert_hash_unified.
        let mut match_index = prev_match_index;

        let bt_mask = self.bt_mask;
        let bt_low = current.saturating_sub(bt_mask);
        // C: matchLow = windowLow ? windowLow : 1
        // windowLow = ZSTD_getLowestMatchIndex(ms, curr, windowLog)
        // For dictionary mode: windowLow = lowLimit = ZSTD_WINDOW_START_INDEX = prefix_low.
        // The loop condition uses windowLow (allows traversal to old dict entries),
        // while btLow is used for the BT buffer break INSIDE the loop.
        let window_low = if prefix_low > 0 { prefix_low } else { 1 };
        let pos_masked = current & bt_mask;
        let pos_base = pos_masked << 1;

        let mut smaller_idx = pos_base;
        let mut larger_idx = pos_base | 1;

        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares = search_depth;
        let mut best_length = min_match_length.saturating_sub(1);
        let mut match_end_idx = current + 8 + 1;

        while nb_compares > 0
            && (match_index as usize) < current
            && (match_index as usize) >= window_low
        {
            nb_compares -= 1;
            let mi = match_index as usize;

            // Per-region validation: prefix positions must be >= prefix_low,
            // source positions must be >= prefix_len + source_low.
            // Invalid nodes are still traversed (bytes are comparable in
            // virtual_buf) — we just don't record them as matches.
            let valid = if mi < prefix_len {
                mi >= prefix_low
            } else {
                mi >= prefix_len + source_low
            };

            let mi_masked = mi & bt_mask;
            let next_base = mi_masked << 1;
            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len = count_match_length_from(virtual_buf, mi, current, known_equal);

            if valid {
                let offset = current - mi;
                if match_len > best_length {
                    best_length = match_len;
                    // C: unconditionally update matchEndIdx (zstd_opt.c:528).
                    // Even dictionary-only matches can advance the skip
                    // position, matching C's PREDICT optimization.
                    let match_end = mi + match_len;
                    if match_end > match_end_idx {
                        match_end_idx = match_end;
                    }
                    dst.push(MatchCandidate {
                        offset,
                        length: match_len,
                    });
                    if match_len > OPT_NUM || current + match_len >= virtual_len {
                        // C: `*smallerPtr = *largerPtr = 0;`
                        if smaller_idx != DUMMY_IDX {
                            debug_assert!(smaller_idx < self.children.len());
                            unsafe {
                                *self.children.get_unchecked_mut(smaller_idx) = 0;
                            }
                        }
                        if larger_idx != DUMMY_IDX {
                            debug_assert!(larger_idx < self.children.len());
                            unsafe {
                                *self.children.get_unchecked_mut(larger_idx) = 0;
                            }
                        }
                        let ntu = match_end_idx
                            .saturating_sub(prefix_len)
                            .saturating_sub(8)
                            .max(pos + 1);
                        if ntu > self.next_to_update {
                            self.next_to_update = ntu;
                        }
                        // Long match found — set remaining to 0 to skip dict phase
                        return (ntu, 0);
                    }
                }
            }

            if current + match_len >= virtual_len || mi + match_len >= virtual_len {
                break;
            }

            let match_byte = virtual_buf[mi + match_len];
            let pos_byte = virtual_buf[current + match_len];

            if match_byte < pos_byte {
                debug_assert!(smaller_idx < self.children.len());
                unsafe {
                    *self.children.get_unchecked_mut(smaller_idx) = match_index;
                }
                common_len_smaller = match_len;
                if mi <= bt_low {
                    smaller_idx = DUMMY_IDX;
                    break;
                }
                debug_assert!((next_base | 1) < self.children.len());
                match_index = unsafe { *self.children.get_unchecked(next_base | 1) };
                smaller_idx = next_base | 1;
            } else {
                debug_assert!(larger_idx < self.children.len());
                unsafe {
                    *self.children.get_unchecked_mut(larger_idx) = match_index;
                }
                common_len_larger = match_len;
                if mi <= bt_low {
                    larger_idx = DUMMY_IDX;
                    break;
                }
                debug_assert!(next_base < self.children.len());
                match_index = unsafe { *self.children.get_unchecked(next_base) };
                larger_idx = next_base;
            }
        }

        // C: `*smallerPtr = *largerPtr = 0;`
        if smaller_idx != DUMMY_IDX {
            debug_assert!(smaller_idx < self.children.len());
            unsafe {
                *self.children.get_unchecked_mut(smaller_idx) = 0;
            }
        }
        if larger_idx != DUMMY_IDX {
            debug_assert!(larger_idx < self.children.len());
            unsafe {
                *self.children.get_unchecked_mut(larger_idx) = 0;
            }
        }
        let ntu = match_end_idx
            .saturating_sub(prefix_len)
            .saturating_sub(8)
            .max(pos + 1);
        if ntu > self.next_to_update {
            self.next_to_update = ntu;
        }
        (ntu, nb_compares)
    }

    /// Read-only dictionary BST search matching C's Phase 2 (zstd_opt.c:777-813).
    /// Searches the pre-built dictionary tree without modifying it. Returns
    /// the number of compares used, so the caller can track budget.
    ///
    /// C uses two boundary levels:
    /// - Outer loop: `dictMatchIndex > dmsLowLimit` — broad (typically 0/1)
    /// - Inner break before child read: `dictMatchIndex <= dmsBtLow` — chain table boundary
    ///
    /// `dms_low_limit` corresponds to C's `dmsLowLimit` (DMS window.lowLimit).
    /// `dms_bt_low` corresponds to C's `dmsBtLow` (chain table circular boundary).
    /// Matches at positions between dms_low_limit and dms_bt_low are compared
    /// but their children are NOT traversed (they may be overwritten).
    #[inline(always)]
    #[allow(unsafe_code)]
    pub(crate) fn search_dict_bst(
        dict_finder: &BinaryTreeFinder,
        dst: &mut Vec<MatchCandidate>,
        virtual_buf: &[u8],
        prefix_len: usize,
        pos: usize,
        mut remaining_compares: usize,
        dms_low_limit: usize,
        dms_bt_low: usize,
        mut best_length: usize,
    ) -> usize {
        dst.clear();
        if prefix_len == 0 || remaining_compares == 0 {
            return 0;
        }
        let src = &virtual_buf[prefix_len..];
        let src_len = virtual_buf.len() - prefix_len;
        if pos + MIN_MATCH > src_len {
            return 0;
        }

        let current = prefix_len + pos;
        let virtual_len = virtual_buf.len();

        // Hash using the source bytes at `pos` with dict_finder's hash params
        let slot = dict_finder.hash_src_at(src, pos);
        debug_assert!(slot < dict_finder.heads.len());
        let mut match_index = unsafe { *dict_finder.heads.get_unchecked(slot) };

        let bt_mask = dict_finder.bt_mask;
        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let compares_at_start = remaining_compares;

        // C: for (; nbCompares && (dictMatchIndex > dmsLowLimit); --nbCompares)
        // Dict BST stores biased positions (pos + DICT_POS_BIAS), so the upper
        // bound is prefix_len + DICT_POS_BIAS and dms_low_limit is biased.
        while remaining_compares > 0
            && (match_index as usize) < prefix_len + DICT_POS_BIAS
            && (match_index as usize) > dms_low_limit
        {
            remaining_compares -= 1;
            let mi = match_index as usize;
            let actual_mi = mi - DICT_POS_BIAS;
            let mi_masked = mi & bt_mask;
            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len = count_match_length_from(virtual_buf, actual_mi, current, known_equal);

            if match_len > best_length {
                best_length = match_len;
                let offset = current - actual_mi;
                dst.push(MatchCandidate {
                    offset,
                    length: match_len,
                });
                if match_len > OPT_NUM || current + match_len >= virtual_len {
                    break;
                }
            }

            if current + match_len >= virtual_len || actual_mi + match_len >= virtual_len {
                break;
            }

            // C: if (dictMatchIndex <= dmsBtLow) { break; } — stop before
            // reading children whose slots may be overwritten in the circular
            // chain table.
            if mi <= dms_bt_low {
                break;
            }

            let match_byte = virtual_buf[actual_mi + match_len];
            let pos_byte = virtual_buf[current + match_len];

            if match_byte < pos_byte {
                common_len_smaller = match_len;
                match_index = unsafe { *dict_finder.children.get_unchecked((mi_masked << 1) | 1) };
            } else {
                common_len_larger = match_len;
                match_index = unsafe { *dict_finder.children.get_unchecked(mi_masked << 1) };
            }
        }

        compares_at_start - remaining_compares
    }

    /// Encapsulates the "catch up" insertion pattern matching C's
    /// `ZSTD_updateTree_internal`: insert all positions from
    /// `self.next_to_update` up to `target`, then set `next_to_update = target`.
    pub(crate) fn update_tree_unified(
        &mut self,
        virtual_buf: &[u8],
        prefix_len: usize,
        target: usize,
        search_depth: usize,
    ) {
        // C: windowLow = ZSTD_getLowestMatchIndex(ms, target, windowLog), whose
        // `isDictionary` arm is `ms->loadedDictEnd != 0`. With a prefix loaded
        // that is true and the function returns `lowLimit`, which is
        // `ZSTD_WINDOW_START_INDEX` here — so the constant below is C's answer
        // and not a shortcut past it.
        //
        // The `prefix_len == 0` arm is the attached-CDict path, and it is a
        // checked equivalence too. An earlier version of this comment called it
        // a lead, on the reasoning that the source match state's own
        // `loadedDictEnd` is zero there, so C would take the *other* arm and
        // bound this by `target - (1 << windowLog)`. C's invariant is the
        // opposite: `ZSTD_checkDictValidity` clears `loadedDictEnd` and
        // `dictMatchState` in the same branch (`zstd_compress_internal.h:1315`)
        // and says so — "loadedDictEnd may be 0, if forceWindow is true, but in
        // that case we never use dictMatchState" — with `zstd_compress.c:3289`
        // asserting the pairing. So while a dictionary is attached the
        // `isDictionary` arm always wins and the floor is `window.lowLimit`,
        // which equals `dictLimit`: the index of the first source byte, and
        // therefore *zero* in the source-relative coordinates used here. The `1`
        // excludes only the phantom position that the zero-filled hash heads use
        // to mean "empty".
        //
        // Driven rather than read: an oracle printing both candidate floors per
        // block took the `isDictionary` arm on every attached row, at a floor of
        // exactly 0 relative to the first source byte. The single row that took
        // `withinWindow` had already dropped the dictionary, which is the
        // never-expiring extDict problem and not this arm.
        //
        // C can afford "no bound" here because it guarantees it has left
        // dictMatchState before the buffer can outgrow the window --
        // `ZSTD_window_enforceMaxDist` drops the dictionary the moment
        // `blockEndIdx > maxDist + loadedDictEnd`. We stay on this path past
        // that point, so the guarantee the constant rests on is one this crate
        // does not yet hold. Bounding the insert here anyway moves 12 of 46
        // streamed-dictionary rows -- nine smaller by up to 0.31%, three larger
        // by a byte -- but the honest fix is to drop the dictionary when C does,
        // not to diverge from C's floor.
        let window_low = if prefix_len > 0 { 2usize } else { 1 };
        if self.next_to_update < target {
            self.insert_range_unified(
                virtual_buf,
                prefix_len,
                self.next_to_update,
                target,
                search_depth,
                window_low,
            );
            self.next_to_update = target;
        }
        // If next_to_update >= target, do nothing — matching C's guard in
        // ZSTD_btGetAllMatches_internal (zstd_opt.c:846) which returns 0
        // without modifying nextToUpdate.
    }

    /// Insert a range of source positions into the unified tree.
    /// Each position is inserted using virtual coordinates.
    /// C's ZSTD_updateTree_internal always increments by 1, ignoring
    /// insertBt1's skip return value. Match that here — inserting every
    /// position keeps the BST fully populated.
    pub(crate) fn insert_range_unified(
        &mut self,
        virtual_buf: &[u8],
        prefix_len: usize,
        start: usize,
        end: usize,
        search_depth: usize,
        window_low: usize,
    ) {
        let src_len = virtual_buf.len() - prefix_len;
        let end = end.min(hash_insert_end(src_len));
        let mut pos = start;
        while pos < end {
            let forward =
                self.insert_unified(virtual_buf, prefix_len, pos, search_depth, window_low);
            pos += forward;
        }
    }

    /// Insert a single source position into the unified tree using virtual coordinates.
    /// Similar to `insert` but operates in the virtual address space (prefix + source).
    /// Returns the number of subsequent positions that can be skipped.
    #[inline(always)]
    #[allow(unsafe_code)]
    fn insert_unified(
        &mut self,
        virtual_buf: &[u8],
        prefix_len: usize,
        pos: usize,
        search_depth: usize,
        window_low: usize,
    ) -> usize {
        let src_len = virtual_buf.len() - prefix_len;
        if pos + MIN_MATCH > src_len {
            return 1;
        }

        let virtual_len = virtual_buf.len();
        let current = prefix_len + pos;

        let slot = self.hash_src_at(&virtual_buf[prefix_len..], pos);
        debug_assert!(slot < self.heads.len());
        let mut match_index = unsafe { *self.heads.get_unchecked(slot) };
        unsafe {
            *self.heads.get_unchecked_mut(slot) = current as u32;
        }

        let bt_mask = self.bt_mask;
        let bt_low = current.saturating_sub(bt_mask);
        let pos_masked = current & bt_mask;
        let pos_base = pos_masked << 1;

        // NOTE: Do NOT zero children[pos_base] here. C doesn't pre-zero;
        // it lets smallerPtr/largerPtr start pointing at the slot and the
        // final `*smallerPtr = *largerPtr = 0` handles cleanup. Pre-zeroing
        // destroys children of aliased positions when bt_mask is small
        // (e.g., 1023), breaking BST chains.

        let mut smaller_idx = pos_base;
        let mut larger_idx = pos_base | 1;

        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares = search_depth;
        let mut best_length = 8usize;
        let mut match_end_idx = current + 8 + 1;

        // NOTE: C's ZSTD_C_PREDICT heuristic is DISABLED in upstream
        // (#ifdef ZSTD_C_PREDICT is never defined). C's source notes it
        // "can create issues when hlog small <= 11". Do NOT use it here.

        // C: while (nbCompares-- && (matchIndex >= windowLow))
        // windowLow is the lowest valid match position (not btLow).
        // btLow is used for the BT buffer break INSIDE the loop.
        while nb_compares > 0
            && (match_index as usize) < current
            && (match_index as usize) >= window_low
        {
            nb_compares -= 1;
            let mi = match_index as usize;
            let mi_masked = mi & bt_mask;
            let next_base = mi_masked << 1;

            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len = count_match_length_from(virtual_buf, mi, current, known_equal);

            if match_len > best_length {
                best_length = match_len;
                // C: unconditionally update matchEndIdx (zstd_opt.c:528).
                let match_end = mi + match_len;
                if match_end > match_end_idx {
                    match_end_idx = match_end;
                }
            }

            if current + match_len >= virtual_len || mi + match_len >= virtual_len {
                break;
            }

            let match_byte = virtual_buf[mi + match_len];
            let pos_byte = virtual_buf[current + match_len];

            if match_byte < pos_byte {
                debug_assert!(smaller_idx < self.children.len());
                unsafe {
                    *self.children.get_unchecked_mut(smaller_idx) = match_index;
                }
                common_len_smaller = match_len;
                if mi <= bt_low {
                    smaller_idx = DUMMY_IDX;
                    break;
                }
                debug_assert!((next_base | 1) < self.children.len());
                match_index = unsafe { *self.children.get_unchecked(next_base | 1) };
                smaller_idx = next_base | 1;
            } else {
                debug_assert!(larger_idx < self.children.len());
                unsafe {
                    *self.children.get_unchecked_mut(larger_idx) = match_index;
                }
                common_len_larger = match_len;
                if mi <= bt_low {
                    larger_idx = DUMMY_IDX;
                    break;
                }
                debug_assert!(next_base < self.children.len());
                match_index = unsafe { *self.children.get_unchecked(next_base) };
                larger_idx = next_base;
            }
        }

        // C: `*smallerPtr = *largerPtr = 0;`
        if smaller_idx != DUMMY_IDX {
            debug_assert!(smaller_idx < self.children.len());
            unsafe {
                *self.children.get_unchecked_mut(smaller_idx) = 0;
            }
        }
        if larger_idx != DUMMY_IDX {
            debug_assert!(larger_idx < self.children.len());
            unsafe {
                *self.children.get_unchecked_mut(larger_idx) = 0;
            }
        }

        // Return skip distance matching C's ZSTD_insertBt1:
        // skip positions covered by long matches.
        let mut positions = 0usize;
        if best_length > 384 {
            positions = (best_length - 384).min(192);
        }
        // Convert match_end_idx from virtual to source coordinates for skip distance.
        let match_end_src = match_end_idx.saturating_sub(prefix_len);
        positions.max(match_end_src - (pos + 8))
    }

    /// Insert a prefix chain into the tree. Uses simplified insertion since
    /// this is called once during dictionary setup, not in the hot path.
    pub(crate) fn insert_prefix_chain(
        &mut self,
        prefix_chain: PrefixChain<'_>,
        src: &[u8],
        search_depth: usize,
    ) {
        let virtual_len = prefix_chain.len() + src.len();
        let end = prefix_chain.len().min(hash_insert_end(virtual_len));
        // One floor for the whole range, taken from its end, exactly as
        // `insert_range` does and for the same reason C gives in
        // `ZSTD_insertBt1`. Filling a CDict runs before
        // `ZSTD_loadDictionaryContent` sets `loadedDictEnd`, so
        // `ZSTD_getLowestMatchIndex` takes its `withinWindow` arm and the
        // floor is the window measured back from the dictionary's end.
        //
        // In practice this is zero on every row the level table can produce,
        // because a CDict's own `window_log` is chosen to cover its
        // dictionary. It is derived rather than hardcoded so that an override
        // that does narrow the window cannot silently build a tree whose shape
        // disagrees with the bound the search applies to it.
        let max_distance = 1usize << self.window_log.min(usize::BITS - 1);
        let window_low = end.saturating_sub(max_distance);
        for pos in 0..end {
            if pos + MIN_MATCH > virtual_len || pos >= prefix_chain.len() {
                continue;
            }
            let slot = hash_prefix_chain_at(prefix_chain, src, pos, self.hash_bits);
            self.insert_prefix_chain_bt(prefix_chain, src, slot, pos, search_depth, window_low);
        }
    }

    /// BST insertion for prefix chain positions.
    ///
    /// `window_low` is the traversal floor and `bt_low` below is the roll
    /// buffer's break, which is C's split in `ZSTD_insertBt1`: the loop runs
    /// `for (; nbCompares && (matchIndex >= windowLow); ...)` and each branch
    /// breaks separately on `matchIndex <= btLow`. Using `bt_low` for both —
    /// as this did — truncates the tree it is building to the last `bt_mask`
    /// positions of the dictionary, which is a shape the search then cannot
    /// see past however well bounded it is.
    fn insert_prefix_chain_bt(
        &mut self,
        prefix_chain: PrefixChain<'_>,
        src: &[u8],
        slot: usize,
        pos: usize,
        search_depth: usize,
        window_low: usize,
    ) {
        let bt_mask = self.bt_mask;
        let bt_low = pos.saturating_sub(bt_mask);
        let pos_masked = pos & bt_mask;
        let pos_base = pos_masked << 1;
        let virtual_len = prefix_chain.len() + src.len();

        let mut match_index = self.heads[slot];
        self.heads[slot] = pos as u32;

        self.children[pos_base] = 0;
        self.children[pos_base | 1] = 0;

        let mut smaller_idx = pos_base;
        let mut larger_idx = pos_base | 1;

        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares = search_depth;

        while nb_compares > 0
            && (match_index as usize) < pos
            && (match_index as usize) >= window_low
        {
            nb_compares -= 1;
            let mi = match_index as usize;
            let mi_masked = mi & bt_mask;
            let next_base = mi_masked << 1;
            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len =
                count_match_length_virtual_from(prefix_chain, src, mi, pos, known_equal);

            if pos + match_len >= virtual_len || mi + match_len >= virtual_len {
                break;
            }

            let match_byte = virtual_byte(prefix_chain, src, mi + match_len);
            let pos_byte = virtual_byte(prefix_chain, src, pos + match_len);

            if match_byte < pos_byte {
                self.children[smaller_idx] = match_index;
                common_len_smaller = match_len;
                if mi <= bt_low {
                    smaller_idx = DUMMY_IDX;
                    break;
                }
                match_index = self.children[next_base | 1];
                smaller_idx = next_base | 1;
            } else {
                self.children[larger_idx] = match_index;
                common_len_larger = match_len;
                if mi <= bt_low {
                    larger_idx = DUMMY_IDX;
                    break;
                }
                match_index = self.children[next_base];
                larger_idx = next_base;
            }
        }

        if smaller_idx != DUMMY_IDX {
            self.children[smaller_idx] = 0;
        }
        if larger_idx != DUMMY_IDX {
            self.children[larger_idx] = 0;
        }
    }

    /// Combined insert and collect_matches: insert the current position and
    /// collect all matches (for the optimal parser) in one descent.
    ///
    /// Returns `(next_to_update, remaining_compares)` — the position up to
    /// which the BT has been updated (matching C's `ms->nextToUpdate =
    /// matchEndIdx - 8`), and the remaining compare budget so the caller can
    /// pass it to a secondary dictionary search phase.
    #[inline(always)]
    #[allow(unsafe_code)]
    pub(crate) fn insert_and_collect_matches(
        &mut self,
        dst: &mut Vec<MatchCandidate>,
        src: &[u8],
        pos: usize,
        search_depth: usize,
        window_low: usize,
        min_match_length: usize,
    ) -> (usize, usize) {
        // Bias positions by 2 to match C's ZSTD_WINDOW_START_INDEX.
        const BIAS: usize = 2;
        dst.clear();
        if pos + MIN_MATCH > src.len() {
            return (pos + 1, search_depth);
        }

        let biased_pos = pos + BIAS;
        let biased_window_low = window_low + BIAS;
        let slot = self.hash_src_at(src, pos);
        debug_assert!(slot < self.heads.len());
        let mut match_index = unsafe { *self.heads.get_unchecked(slot) };
        unsafe {
            *self.heads.get_unchecked_mut(slot) = biased_pos as u32;
        }

        let bt_mask = self.bt_mask;
        let bt_low = biased_pos.saturating_sub(bt_mask);
        // C: matchLow = windowLow ? windowLow : 1 (`zstd_opt.c`, in
        // `ZSTD_insertBtAndGetAllMatches`).  windowLow is already biased
        // (>= ZSTD_WINDOW_START_INDEX = 2), so the fallback to 1 only matters
        // for phantom positions.
        //
        // `bt_low` deliberately does *not* enter this floor. It bounds the
        // children array, which is a roll buffer of `1 << (chainLog - 1)`
        // entries, and C applies it only through the in-loop break below —
        // after the candidate it lands on has been compared and recorded.
        // Folding it in here instead drops that candidate, and with it every
        // match older than the roll buffer, which is silent until the body
        // outgrows the buffer: `bt_low` is 0 up to that point.
        let match_low = if biased_window_low > 0 {
            biased_window_low
        } else {
            1
        };
        let pos_masked = biased_pos & bt_mask;
        let pos_base = pos_masked << 1;

        let mut smaller_idx = pos_base;
        let mut larger_idx = pos_base | 1;

        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares = search_depth;
        let src_end = src.len();
        let mut best_length = min_match_length.saturating_sub(1);
        let mut match_end_idx = biased_pos + 8 + 1;

        while nb_compares > 0
            && (match_index as usize) < biased_pos
            && (match_index as usize) >= match_low
        {
            nb_compares -= 1;
            let mi = match_index as usize;
            let mi_masked = mi & bt_mask;
            let next_base = mi_masked << 1;
            let actual_mi = mi - BIAS;
            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len = count_match_length_from(src, actual_mi, pos, known_equal);

            let offset = pos - actual_mi;
            if match_len > best_length {
                best_length = match_len;
                if mi + match_len > match_end_idx {
                    match_end_idx = mi + match_len;
                }
                dst.push(MatchCandidate {
                    offset,
                    length: match_len,
                });
                if match_len > OPT_NUM || pos + match_len >= src_end {
                    if smaller_idx != DUMMY_IDX {
                        debug_assert!(smaller_idx < self.children.len());
                        unsafe {
                            *self.children.get_unchecked_mut(smaller_idx) = 0;
                        }
                    }
                    if larger_idx != DUMMY_IDX {
                        debug_assert!(larger_idx < self.children.len());
                        unsafe {
                            *self.children.get_unchecked_mut(larger_idx) = 0;
                        }
                    }
                    return (match_end_idx.saturating_sub(BIAS + 8), 0);
                }
            }

            if pos + match_len >= src_end || actual_mi + match_len >= src_end {
                break;
            }

            debug_assert!(actual_mi + match_len < src_end && pos + match_len < src_end);
            if unsafe {
                *src.get_unchecked(actual_mi + match_len) < *src.get_unchecked(pos + match_len)
            } {
                debug_assert!(smaller_idx < self.children.len());
                unsafe {
                    *self.children.get_unchecked_mut(smaller_idx) = match_index;
                }
                common_len_smaller = match_len;
                if mi <= bt_low {
                    smaller_idx = DUMMY_IDX;
                    break;
                }
                debug_assert!((next_base | 1) < self.children.len());
                match_index = unsafe { *self.children.get_unchecked(next_base | 1) };
                smaller_idx = next_base | 1;
            } else {
                debug_assert!(larger_idx < self.children.len());
                unsafe {
                    *self.children.get_unchecked_mut(larger_idx) = match_index;
                }
                common_len_larger = match_len;
                if mi <= bt_low {
                    larger_idx = DUMMY_IDX;
                    break;
                }
                debug_assert!(next_base < self.children.len());
                match_index = unsafe { *self.children.get_unchecked(next_base) };
                larger_idx = next_base;
            }
        }

        if smaller_idx != DUMMY_IDX {
            debug_assert!(smaller_idx < self.children.len());
            unsafe {
                *self.children.get_unchecked_mut(smaller_idx) = 0;
            }
        }
        if larger_idx != DUMMY_IDX {
            debug_assert!(larger_idx < self.children.len());
            unsafe {
                *self.children.get_unchecked_mut(larger_idx) = 0;
            }
        }
        (match_end_idx.saturating_sub(BIAS + 8), nb_compares)
    }

    /// Read-only search: collect all matches at `pos` without modifying the tree.
    pub(crate) fn collect_matches(
        &self,
        dst: &mut Vec<MatchCandidate>,
        src: &[u8],
        pos: usize,
        params: MatchFinderParameters,
        literal_length: usize,
        window_low: usize,
    ) {
        dst.clear();
        if pos + MIN_MATCH > src.len() {
            return;
        }

        let bt_mask = self.bt_mask;
        let bt_low = pos.saturating_sub(bt_mask);
        // C's `matchLow` is `windowLow` alone; `bt_low` bounds the children
        // roll buffer and belongs in the in-loop break below, which stops the
        // descent only *after* comparing the candidate it lands on. See
        // `insert_and_collect_matches`, the inserting twin of this search.
        let match_low = if window_low > 0 { window_low } else { 1 };
        let src_end = src.len();
        let min_length = regular_match_length_threshold(literal_length, params);

        let mut match_index = self.heads[self.hash_src_at(src, pos)];
        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares = params.search_depth;
        let mut best_length = min_length.saturating_sub(1);

        while nb_compares > 0 && (match_index as usize) < pos && (match_index as usize) >= match_low
        {
            nb_compares -= 1;
            let mi = match_index as usize;
            let mi_masked = mi & bt_mask;
            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len = count_match_length_from(src, mi, pos, known_equal);

            let offset = pos - mi;
            if match_len > best_length {
                best_length = match_len;
                dst.push(MatchCandidate {
                    offset,
                    length: match_len,
                });
                if match_len > OPT_NUM {
                    return;
                }
            }

            if pos + match_len >= src_end || mi + match_len >= src_end {
                break;
            }

            // C: `if (matchIndex <= btLow) { ... break; }`. Below `bt_low` the
            // children slots have been overwritten by newer positions, so the
            // link is stale and the descent has to stop here — but only after
            // the candidate above was compared and recorded.
            if mi <= bt_low {
                break;
            }

            if src[mi + match_len] < src[pos + match_len] {
                common_len_smaller = match_len;
                match_index = self.children[(mi_masked << 1) | 1];
            } else {
                common_len_larger = match_len;
                match_index = self.children[mi_masked << 1];
            }
        }
    }

    /// Search-only for the prefix chain finder (read-only, no insertion).
    /// Search the pre-built dictionary binary tree for the best match.
    /// Uses C's DUBT cost-based acceptance criterion (4x length weighting)
    /// to match `ZSTD_DUBT_findBetterDictMatch`.
    pub(crate) fn find_prefix_chain_match(
        &self,
        prefix_chain: PrefixChain<'_>,
        src: &[u8],
        pos: usize,
        params: MatchFinderParameters,
        _literal_length: usize,
        prefix_low: usize,
    ) -> Option<MatchCandidate> {
        if pos + MIN_MATCH > src.len() {
            return None;
        }
        let current = prefix_chain.len() + pos;
        let bt_mask = self.bt_mask;
        let dict_high_limit = prefix_chain.len();
        let bt_low = dictionary_bt_low(bt_mask, dict_high_limit, prefix_low);
        let virtual_len = prefix_chain.len() + src.len();

        let mut match_index = self.heads[self.hash_src_at(src, pos)];
        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares = params.dictionary_search_depth;
        // C's DUBT criterion: track best_length and best_offbase separately.
        // Matches ZSTD_DUBT_findBetterDictMatch's acceptance logic.
        let mut best_length = 0usize;
        let mut best_offbase = 999_999_999u32;

        while nb_compares > 0
            && (match_index as usize) < dict_high_limit
            && (match_index as usize) >= prefix_low
        {
            nb_compares -= 1;
            let mi = match_index as usize;
            let mi_masked = mi & bt_mask;
            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len =
                count_match_length_virtual_from(prefix_chain, src, mi, current, known_equal);

            let offset = current - mi;
            if match_len > best_length && match_len >= params.min_match_length_zero_literals {
                let new_offbase = (offset as u32).saturating_add(3);
                let gain = 4 * (match_len as i32 - best_length as i32);
                let cost = highbit32((offset as u32) + 1) as i32 - highbit32(best_offbase) as i32;
                if gain > cost {
                    best_length = match_len;
                    best_offbase = new_offbase;
                }
                if match_len >= params.good_enough_match_length {
                    break;
                }
                if pos + match_len == src.len() {
                    break;
                }
            }

            if current + match_len >= virtual_len || mi + match_len >= virtual_len {
                break;
            }

            // C breaks here, in both branches of the byte comparison and after
            // the candidate has been compared and recorded
            // (`zstd_lazy.c:220,225`). The children of a position below
            // `bt_low` have been overwritten by the roll buffer, so it is the
            // last position that can be descended *from* — not a bound on
            // which positions may be reached.
            if mi <= bt_low {
                break;
            }

            let match_byte = virtual_byte(prefix_chain, src, mi + match_len);
            let pos_byte = virtual_byte(prefix_chain, src, current + match_len);

            if match_byte < pos_byte {
                common_len_smaller = match_len;
                match_index = self.children[(mi_masked << 1) | 1];
            } else {
                common_len_larger = match_len;
                match_index = self.children[mi_masked << 1];
            }
        }

        if best_length >= MIN_MATCH {
            Some(MatchCandidate {
                offset: best_offbase.saturating_sub(3) as usize,
                length: best_length,
            })
        } else {
            None
        }
    }

    /// DUBT-style insert: chain the position unsorted (O(1)).
    /// C's ZSTD_updateDUBT equivalent.
    fn dubt_insert(&mut self, src: &[u8], pos: usize) {
        if pos + MIN_MATCH > src.len() {
            return;
        }
        let slot = self.hash_src_at(src, pos);
        let old_head = self.heads[slot];
        self.heads[slot] = pos as u32;
        let pos_base = (pos & self.bt_mask) << 1;
        self.children[pos_base] = old_head; // chain link to previous head
        self.children[pos_base | 1] = DUBT_UNSORTED_MARK;
    }

    fn dubt_insert_range(&mut self, src: &[u8], start: usize, end: usize) {
        let end = end.min(hash_insert_end(src.len()));
        for pos in start..end {
            self.dubt_insert(src, pos);
        }
    }

    /// DUBT-style find: sort unsorted entries in the hash bucket, then
    /// do a combined insert+search at the current position.
    /// C's ZSTD_DUBT_findBestMatch equivalent.
    fn dubt_find_match(
        &mut self,
        src: &[u8],
        pos: usize,
        params: MatchFinderParameters,
        _literal_length: usize,
        window_low: usize,
    ) -> Option<MatchCandidate> {
        if pos + MIN_MATCH > src.len() {
            return None;
        }

        let slot = self.hash_src_at(src, pos);
        let mut match_index = self.heads[slot];

        let bt_mask = self.bt_mask;
        let bt_low = pos.saturating_sub(bt_mask);
        let unsort_limit = bt_low.max(window_low);
        let nb_compares = params.search_depth;
        let mut nb_candidates = nb_compares;

        // Phase 1: Walk the unsorted chain and build a reversed stack
        let mut previous_candidate: u32 = 0; // 0 is the sentinel (like C uses 0)
        while (match_index as usize) > unsort_limit
            && match_index != NO_POS
            && self.children[((match_index as usize) & bt_mask) << 1 | 1] == DUBT_UNSORTED_MARK
            && nb_candidates > 1
        {
            let mi_base = ((match_index as usize) & bt_mask) << 1;
            // The unsorted mark slot becomes a reversed chain link
            self.children[mi_base | 1] = previous_candidate;
            previous_candidate = match_index;
            match_index = self.children[mi_base];
            nb_candidates -= 1;
        }

        // Nullify the last candidate if still unsorted (C's simplification)
        if (match_index as usize) > unsort_limit
            && match_index != NO_POS
            && self.children[((match_index as usize) & bt_mask) << 1 | 1] == DUBT_UNSORTED_MARK
        {
            let mi_base = ((match_index as usize) & bt_mask) << 1;
            self.children[mi_base] = 0;
            self.children[mi_base | 1] = 0;
        }

        // Phase 2: Sort the stacked unsorted entries into the BST (oldest first)
        let mut sort_index = previous_candidate;
        while sort_index != 0 {
            let si = sort_index as usize;
            let si_base = (si & bt_mask) << 1;
            let next_candidate = self.children[si_base | 1]; // reversed chain link
            self.dubt_insert_one(src, si, nb_candidates, unsort_limit, window_low);
            sort_index = next_candidate;
            nb_candidates += 1;
        }

        // Phase 3: Combined insert + search at current position
        // Matches C's ZSTD_DUBT_findBestMatch Phase 3 exactly.
        match_index = self.heads[slot];
        self.heads[slot] = pos as u32;

        let pos_masked = pos & bt_mask;
        let pos_base = pos_masked << 1;
        self.children[pos_base] = 0;
        self.children[pos_base | 1] = 0;

        let mut smaller_idx = pos_base;
        let mut larger_idx = pos_base | 1;
        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares_left = nb_compares;
        let src_end = src.len();
        // C starts with bestLength = 0 and offBase = caller's initial (999999999).
        // offBase in C = offset + 3 (OFFSET_TO_OFFBASE).
        let mut best_length = 0usize;
        let mut best_offbase = 999_999_999u32;
        let mut match_end_idx = pos + 8 + 1;

        // C uses: for (; nbCompares && (matchIndex > windowLow); --nbCompares)
        // Note: strict > to match C's boundary semantics.
        while nb_compares_left > 0 && (match_index as usize) > window_low {
            nb_compares_left -= 1;
            let mi = match_index as usize;
            if mi >= pos {
                break;
            }
            let mi_masked = mi & bt_mask;
            let next_base = mi_masked << 1;
            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len = count_match_length_from(src, mi, pos, known_equal);

            // C: if (matchLength > bestLength) { ... accept with cost check ... }
            if match_len > best_length {
                if match_len > match_end_idx.wrapping_sub(mi) {
                    match_end_idx = mi + match_len;
                }
                let offset = pos - mi;
                // C's cost-based acceptance (ZSTD_DUBT_findBestMatch line 342):
                // if (4*(matchLength-bestLength)) > (highbit32(curr-matchIndex+1) - highbit32(offBase))
                let new_offbase = (offset as u32).saturating_add(3); // OFFSET_TO_OFFBASE (for storage)
                let gain = 4 * (match_len as i32 - best_length as i32);
                // C uses (offset + 1) for the new candidate cost, not offbase (offset + 3)
                let cost = highbit32((offset as u32) + 1) as i32 - highbit32(best_offbase) as i32;
                if gain > cost {
                    best_length = match_len;
                    best_offbase = new_offbase;
                }
                // C: if (ip+matchLength == iend) break
                if pos + match_len == src_end {
                    break;
                }
            }

            if pos + match_len >= src_end || mi + match_len >= src_end {
                break;
            }

            if src[mi + match_len] < src[pos + match_len] {
                self.children[smaller_idx] = match_index;
                common_len_smaller = match_len;
                if mi <= bt_low {
                    smaller_idx = DUMMY_IDX;
                    break;
                }
                match_index = self.children[next_base | 1];
                smaller_idx = next_base | 1;
            } else {
                self.children[larger_idx] = match_index;
                common_len_larger = match_len;
                if mi <= bt_low {
                    larger_idx = DUMMY_IDX;
                    break;
                }
                match_index = self.children[next_base];
                larger_idx = next_base;
            }
        }

        if smaller_idx != DUMMY_IDX {
            self.children[smaller_idx] = 0;
        }
        if larger_idx != DUMMY_IDX {
            self.children[larger_idx] = 0;
        }

        // C: ms->nextToUpdate = matchEndIdx - 8; (skip repetitive patterns)
        debug_assert!(match_end_idx > pos + 8);
        self.next_to_update = match_end_idx - 8;

        if best_length >= MIN_MATCH {
            // Convert offbase back to offset: offset = offbase - 3 (OFFBASE_TO_OFFSET)
            let offset = best_offbase.saturating_sub(3) as usize;
            Some(MatchCandidate {
                offset,
                length: best_length,
            })
        } else {
            None
        }
    }

    /// Sort one unsorted entry into the BST. C's ZSTD_insertDUBT1 equivalent.
    /// `bt_low_param` corresponds to C's `btLow` parameter (which receives `unsortLimit`).
    fn dubt_insert_one(
        &mut self,
        src: &[u8],
        curr: usize,
        nb_compares: usize,
        bt_low_param: usize,
        window_low: usize,
    ) {
        let bt_mask = self.bt_mask;
        // No local `curr - bt_mask` floor here, deliberately. That value is the
        // roll buffer's wrap point, and using it to bound the traversal instead
        // of `window_low` is what once cost 7.56% of ratio at level 16: it stops
        // the walk at the buffer break rather than at the edge of the window,
        // discarding candidates that are still perfectly valid. The loop below
        // bounds on `window_low`; `bt_low_param` appears only in the `<=` break
        // checks, which is what C does with `btLow`.
        let curr_masked = curr & bt_mask;
        let curr_base = curr_masked << 1;
        let src_end = src.len();

        // The match index comes from the chain link stored in children[smaller]
        let mut match_index = self.children[curr_base];

        self.children[curr_base] = 0;
        self.children[curr_base | 1] = 0;

        let mut smaller_idx = curr_base;
        let mut larger_idx = curr_base | 1;
        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares = nb_compares;

        // C uses `matchIndex > windowLow` for the loop condition (strict >).
        // The bt_low_param is only used for the <= break checks inside.
        while nb_compares > 0
            && (match_index as usize) < curr
            && (match_index as usize) > window_low
        {
            nb_compares -= 1;
            let mi = match_index as usize;
            let mi_masked = mi & bt_mask;
            let next_base = mi_masked << 1;
            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len = count_match_length_from(src, mi, curr, known_equal);

            if curr + match_len >= src_end || mi + match_len >= src_end {
                break;
            }

            if src[mi + match_len] < src[curr + match_len] {
                self.children[smaller_idx] = match_index;
                common_len_smaller = match_len;
                if mi <= bt_low_param {
                    smaller_idx = DUMMY_IDX;
                    break;
                }
                match_index = self.children[next_base | 1];
                smaller_idx = next_base | 1;
            } else {
                self.children[larger_idx] = match_index;
                common_len_larger = match_len;
                if mi <= bt_low_param {
                    larger_idx = DUMMY_IDX;
                    break;
                }
                match_index = self.children[next_base];
                larger_idx = next_base;
            }
        }

        if smaller_idx != DUMMY_IDX {
            self.children[smaller_idx] = 0;
        }
        if larger_idx != DUMMY_IDX {
            self.children[larger_idx] = 0;
        }
    }

    /// DUBT-style insert in virtual coordinates: chain the position unsorted (O(1)).
    /// Source position `frame_pos` maps to virtual position `prefix_len + frame_pos`.
    fn dubt_insert_virtual(&mut self, src: &[u8], frame_pos: usize) {
        let virtual_pos = self.prefix_len + frame_pos;
        if frame_pos + MIN_MATCH > src.len() {
            return;
        }
        let slot = self.hash_src_at(src, frame_pos);
        let old_head = self.heads[slot];
        self.heads[slot] = virtual_pos as u32;
        let pos_base = (virtual_pos & self.bt_mask) << 1;
        self.children[pos_base] = old_head;
        self.children[pos_base | 1] = DUBT_UNSORTED_MARK;
    }

    pub(crate) fn dubt_insert_range_virtual(
        &mut self,
        src: &[u8],
        start_virtual: usize,
        end_virtual: usize,
    ) {
        let prefix_len = self.prefix_len;
        // Convert virtual positions to frame positions, clamping to valid range.
        let start_frame = start_virtual.saturating_sub(prefix_len);
        let end_frame = end_virtual.saturating_sub(prefix_len);
        let end_frame = end_frame.min(hash_insert_end(src.len()));
        for frame_pos in start_frame..end_frame {
            self.dubt_insert_virtual(src, frame_pos);
        }
    }

    /// Sort one unsorted entry into the BST using virtual coordinates.
    /// C's ZSTD_insertDUBT1 equivalent for the prefix-aware case.
    fn dubt_insert_one_virtual(
        &mut self,
        prefix_chain: PrefixChain<'_>,
        src: &[u8],
        curr: usize,
        nb_compares: usize,
        bt_low_param: usize,
        window_low: usize,
    ) {
        let bt_mask = self.bt_mask;
        let bt_low = curr.saturating_sub(bt_mask);
        let _ = bt_low; // bt_low_param is used instead
        let curr_masked = curr & bt_mask;
        let curr_base = curr_masked << 1;
        let virtual_len = prefix_chain.len() + src.len();

        let mut match_index = self.children[curr_base];

        self.children[curr_base] = 0;
        self.children[curr_base | 1] = 0;

        let mut smaller_idx = curr_base;
        let mut larger_idx = curr_base | 1;
        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares = nb_compares;

        // See `dubt_find_match_virtual`: un-bias prefix indices when
        // reading bytes / counting match length.
        let prefix_len = prefix_chain.len();
        while nb_compares > 0
            && (match_index as usize) < curr
            && (match_index as usize) > window_low
        {
            nb_compares -= 1;
            let mi = match_index as usize;
            let mi_masked = mi & bt_mask;
            let next_base = mi_masked << 1;
            let mi_virtual = if mi < prefix_len {
                mi.saturating_sub(DICT_POS_BIAS)
            } else {
                mi
            };
            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len =
                count_match_length_virtual_from(prefix_chain, src, mi_virtual, curr, known_equal);

            if curr + match_len >= virtual_len || mi_virtual + match_len >= virtual_len {
                break;
            }

            let match_byte = virtual_byte(prefix_chain, src, mi_virtual + match_len);
            let curr_byte = virtual_byte(prefix_chain, src, curr + match_len);

            if match_byte < curr_byte {
                self.children[smaller_idx] = match_index;
                common_len_smaller = match_len;
                if mi <= bt_low_param {
                    smaller_idx = DUMMY_IDX;
                    break;
                }
                match_index = self.children[next_base | 1];
                smaller_idx = next_base | 1;
            } else {
                self.children[larger_idx] = match_index;
                common_len_larger = match_len;
                if mi <= bt_low_param {
                    larger_idx = DUMMY_IDX;
                    break;
                }
                match_index = self.children[next_base];
                larger_idx = next_base;
            }
        }

        if smaller_idx != DUMMY_IDX {
            self.children[smaller_idx] = 0;
        }
        if larger_idx != DUMMY_IDX {
            self.children[larger_idx] = 0;
        }
    }

    /// The match floor as a bound on an index stored in this tree.
    ///
    /// C compares one number here, because its dictionary and its source share
    /// a single index space. This tree has one too, but an uneven one: prefix
    /// position `p` is stored at `p + DICT_POS_BIAS` and source position `p` at
    /// `prefix_len + p`. The floor arrives split into the pair the parsers use,
    /// which address the prefix and the source as separate buffers, so it has
    /// to be folded back into this space before it can bound anything.
    ///
    /// `source_low` on its own understates the bound by `prefix_len`, which let
    /// btlazy2 reach a whole dictionary further back than the window and emit
    /// offsets the decoder rejects. Only this finder is affected: every other
    /// one searches the source in source coordinates.
    ///
    /// While any of the prefix is still inside the window, the floor lands in
    /// the prefix and every source position is reachable; once it has aged out,
    /// no prefix position is, and the floor lands in the source.
    fn stored_index_floor(&self, prefix_low: usize, source_low: usize) -> usize {
        if prefix_low < self.prefix_len {
            prefix_low + DICT_POS_BIAS
        } else {
            self.prefix_len + source_low
        }
    }

    /// DUBT-style find with virtual coordinates: sort unsorted entries, then
    /// combined insert+search. Matches C's ZSTD_DUBT_findBestMatch but
    /// operates in virtual coordinate space (prefix + source).
    ///
    /// `index_low` bounds a *stored* index, so callers go through
    /// [`Self::stored_index_floor`] rather than passing a source-space value.
    fn dubt_find_match_virtual(
        &mut self,
        prefix_chain: PrefixChain<'_>,
        src: &[u8],
        frame_pos: usize,
        params: MatchFinderParameters,
        _literal_length: usize,
        index_low: usize,
    ) -> Option<MatchCandidate> {
        if frame_pos + MIN_MATCH > src.len() {
            return None;
        }

        let prefix_len = self.prefix_len;
        let virtual_pos = prefix_len + frame_pos;
        let virtual_len = prefix_len + src.len();

        let slot = self.hash_src_at(src, frame_pos);
        let mut match_index = self.heads[slot];

        let bt_mask = self.bt_mask;
        let bt_low = virtual_pos.saturating_sub(bt_mask);
        let unsort_limit = bt_low.max(index_low);
        let nb_compares = params.search_depth;
        let mut nb_candidates = nb_compares;

        // Phase 1: Walk the unsorted chain and build a reversed stack
        let mut previous_candidate: u32 = 0;
        while (match_index as usize) > unsort_limit
            && match_index != NO_POS
            && self.children[((match_index as usize) & bt_mask) << 1 | 1] == DUBT_UNSORTED_MARK
            && nb_candidates > 1
        {
            let mi_base = ((match_index as usize) & bt_mask) << 1;
            self.children[mi_base | 1] = previous_candidate;
            previous_candidate = match_index;
            match_index = self.children[mi_base];
            nb_candidates -= 1;
        }

        // Nullify the last candidate if still unsorted
        if (match_index as usize) > unsort_limit
            && match_index != NO_POS
            && self.children[((match_index as usize) & bt_mask) << 1 | 1] == DUBT_UNSORTED_MARK
        {
            let mi_base = ((match_index as usize) & bt_mask) << 1;
            self.children[mi_base] = 0;
            self.children[mi_base | 1] = 0;
        }

        // Phase 2: Sort the stacked unsorted entries into the BST (oldest first)
        let mut sort_index = previous_candidate;
        while sort_index != 0 {
            let si = sort_index as usize;
            let si_base = (si & bt_mask) << 1;
            let next_candidate = self.children[si_base | 1];
            self.dubt_insert_one_virtual(
                prefix_chain,
                src,
                si,
                nb_candidates,
                unsort_limit,
                index_low,
            );
            sort_index = next_candidate;
            nb_candidates += 1;
        }

        // Phase 3: Combined insert + search at current virtual position
        match_index = self.heads[slot];
        self.heads[slot] = virtual_pos as u32;

        let pos_masked = virtual_pos & bt_mask;
        let pos_base = pos_masked << 1;
        self.children[pos_base] = 0;
        self.children[pos_base | 1] = 0;

        let mut smaller_idx = pos_base;
        let mut larger_idx = pos_base | 1;
        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares_left = nb_compares;
        let mut best_length = 0usize;
        let mut best_offbase = 999_999_999u32;
        let mut match_end_idx = virtual_pos + 8 + 1;

        // Prefix entries were inserted by `insert_prefix_bt1` with
        // DICT_POS_BIAS=2 (C's ZSTD_WINDOW_START_INDEX). Source entries go
        // through `dubt_insert_virtual`, which stores true virtual coords.
        // The search must un-bias prefix indices when reading bytes and
        // computing offsets, otherwise it reads `prefix[2..]` where it
        // should read `prefix[0..]`.
        while nb_compares_left > 0 && (match_index as usize) > index_low {
            nb_compares_left -= 1;
            let mi = match_index as usize;
            if mi >= virtual_pos {
                break;
            }
            let mi_masked = mi & bt_mask;
            let next_base = mi_masked << 1;
            let mi_virtual = if mi < prefix_len {
                mi.saturating_sub(DICT_POS_BIAS)
            } else {
                mi
            };
            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len = count_match_length_virtual_from(
                prefix_chain,
                src,
                mi_virtual,
                virtual_pos,
                known_equal,
            );

            if match_len > best_length {
                if match_len > match_end_idx.wrapping_sub(mi_virtual) {
                    match_end_idx = mi_virtual + match_len;
                }
                let offset = virtual_pos - mi_virtual;
                let new_offbase = (offset as u32).saturating_add(3);
                let gain = 4 * (match_len as i32 - best_length as i32);
                let cost = highbit32((offset as u32) + 1) as i32 - highbit32(best_offbase) as i32;
                if gain > cost {
                    best_length = match_len;
                    best_offbase = new_offbase;
                }
                if frame_pos + match_len == src.len() {
                    break;
                }
            }

            if virtual_pos + match_len >= virtual_len || mi_virtual + match_len >= virtual_len {
                break;
            }

            let match_byte = virtual_byte(prefix_chain, src, mi_virtual + match_len);
            let pos_byte = virtual_byte(prefix_chain, src, virtual_pos + match_len);

            if match_byte < pos_byte {
                self.children[smaller_idx] = match_index;
                common_len_smaller = match_len;
                if mi <= bt_low {
                    smaller_idx = DUMMY_IDX;
                    break;
                }
                match_index = self.children[next_base | 1];
                smaller_idx = next_base | 1;
            } else {
                self.children[larger_idx] = match_index;
                common_len_larger = match_len;
                if mi <= bt_low {
                    larger_idx = DUMMY_IDX;
                    break;
                }
                match_index = self.children[next_base];
                larger_idx = next_base;
            }
        }

        if smaller_idx != DUMMY_IDX {
            self.children[smaller_idx] = 0;
        }
        if larger_idx != DUMMY_IDX {
            self.children[larger_idx] = 0;
        }

        // C: ms->nextToUpdate = matchEndIdx - 8
        debug_assert!(match_end_idx > virtual_pos + 8);
        self.next_to_update = match_end_idx - 8;

        if best_length >= MIN_MATCH {
            let offset = best_offbase.saturating_sub(3) as usize;
            Some(MatchCandidate {
                offset,
                length: best_length,
            })
        } else {
            None
        }
    }

    /// Search-only collect for prefix chain (read-only, no insertion).
    pub(crate) fn collect_prefix_chain_matches(
        &self,
        dst: &mut Vec<MatchCandidate>,
        prefix_chain: PrefixChain<'_>,
        src: &[u8],
        pos: usize,
        search_depth: usize,
        prefix_low: usize,
        min_match_length: usize,
    ) {
        dst.clear();
        if pos + MIN_MATCH > src.len() {
            return;
        }
        let current = prefix_chain.len() + pos;
        let bt_mask = self.bt_mask;
        let dict_high_limit = prefix_chain.len();
        let bt_low = dictionary_bt_low(bt_mask, dict_high_limit, prefix_low);
        let virtual_len = prefix_chain.len() + src.len();

        let mut match_index = self.heads[self.hash_src_at(src, pos)];
        let mut common_len_smaller = 0usize;
        let mut common_len_larger = 0usize;
        let mut nb_compares = search_depth;
        let mut best_length = min_match_length.saturating_sub(1);

        while nb_compares > 0
            && (match_index as usize) < dict_high_limit
            && (match_index as usize) >= prefix_low
        {
            nb_compares -= 1;
            let mi = match_index as usize;
            let mi_masked = mi & bt_mask;
            let known_equal = common_len_smaller.min(common_len_larger);
            let match_len =
                count_match_length_virtual_from(prefix_chain, src, mi, current, known_equal);

            let offset = current - mi;
            if match_len > best_length {
                best_length = match_len;
                dst.push(MatchCandidate {
                    offset,
                    length: match_len,
                });
                if match_len > OPT_NUM {
                    return;
                }
            }

            if current + match_len >= virtual_len || mi + match_len >= virtual_len {
                break;
            }

            // The roll-buffer break, same as in `find_prefix_chain_match`.
            if mi <= bt_low {
                break;
            }

            let match_byte = virtual_byte(prefix_chain, src, mi + match_len);
            let pos_byte = virtual_byte(prefix_chain, src, current + match_len);

            if match_byte < pos_byte {
                common_len_smaller = match_len;
                match_index = self.children[(mi_masked << 1) | 1];
            } else {
                common_len_larger = match_len;
                match_index = self.children[mi_masked << 1];
            }
        }
    }
}

/// The last dictionary position whose children survive in the tree's roll
/// buffer, which is C's `btLow` in `ZSTD_DUBT_findBetterDictMatch`
/// (`zstd_lazy.c:192`):
///
/// ```c
/// U32 const btLow = (btMask >= dictHighLimit - dictLowLimit) ? dictLowLimit
///                                                            : dictHighLimit - btMask;
/// ```
///
/// **Both ends come from the dictionary, not from where the source has
/// reached.** Deriving it from the current source position instead — which
/// this port did at both call sites — measures the roll buffer from a point
/// past the whole dictionary and slides the result forward with every byte
/// encoded, so any dictionary longer than `btMask` loses everything below its
/// last `btMask` bytes. With the dictionary's tables sized from
/// `appliedParams` that was a 255-position window onto a 16 KiB dictionary.
///
/// It is a *break*, taken after a candidate has been compared and only to stop
/// the descent into its children; the traversal floor is the dictionary's own
/// low limit. Conflating the two is the same mistake, one layer down, that
/// `insert`, `insert_and_collect_matches` and `collect_matches` were fixed for.
fn dictionary_bt_low(bt_mask: usize, dict_high_limit: usize, dict_low_limit: usize) -> usize {
    if bt_mask >= dict_high_limit - dict_low_limit {
        dict_low_limit
    } else {
        dict_high_limit - bt_mask
    }
}

impl LazySearchFinder for BinaryTreeFinder {
    fn insert(&mut self, src: &[u8], pos: usize) {
        // For BinaryTreeFinder, defer actual insertion to find_match.
        // Just advance next_to_update so find_match knows where to start.
        // This matches C where ZSTD_updateDUBT is called inside
        // ZSTD_BtFindBestMatch, not by the lazy parser.
        let _ = (src, pos);
    }

    fn insert_range(&mut self, src: &[u8], _start: usize, _end: usize) {
        // For BinaryTreeFinder, defer actual insertion to find_match.
        // The lazy parser calls insert_range before find_match, but in C
        // the equivalent ZSTD_updateDUBT is called INSIDE BtFindBestMatch.
        // We defer to match C's behavior: when next_to_update causes a skip,
        // the insertion is also skipped.
        let _ = src;
    }

    fn find_match(
        &mut self,
        src: &[u8],
        pos: usize,
        params: MatchFinderParameters,
        literal_length: usize,
        window_low: usize,
    ) -> Option<MatchCandidate> {
        // C: if (ip < ms->window.base + ms->nextToUpdate) return 0;
        // When skipped, NEITHER insertion NOR search happens (matching C).
        if pos < self.next_to_update {
            return None;
        }
        // C: ZSTD_updateDUBT(ms, ip, iLimit, mls);
        // Insert all positions from next_to_update up to pos.
        self.dubt_insert_range(src, self.next_to_update, pos);
        self.next_to_update = pos;
        self.dubt_find_match(src, pos, params, literal_length, window_low)
    }

    fn would_skip(&self, pos: usize) -> bool {
        if self.prefix_len > 0 {
            // In virtual coordinates, gate both source and dictionary searches.
            self.prefix_len + pos < self.next_to_update
        } else {
            // When not pre-populated, don't gate the prefix search here;
            // find_match handles the skip internally for source-only search.
            false
        }
    }

    fn skips_prefix_regular_search(&self) -> bool {
        // When prefix_len > 0, the source DUBT already includes dictionary
        // entries via pre-population, so no separate prefix search is needed.
        // When prefix_len == 0, the prefix search must be done separately
        // (the DUBT only has source entries).
        self.prefix_len > 0
    }

    fn find_match_with_prefix(
        &mut self,
        prefix_chain: PrefixChain<'_>,
        src: &[u8],
        pos: usize,
        params: MatchFinderParameters,
        _literal_length: usize,
        prefix_low: usize,
        source_low: usize,
    ) -> Option<MatchCandidate> {
        if self.prefix_len == 0 {
            return self.find_match(src, pos, params, _literal_length, source_low);
        }
        let virtual_pos = self.prefix_len + pos;
        if virtual_pos < self.next_to_update {
            return None;
        }
        // Insert source positions from next_to_update to virtual_pos using DUBT unsorted insert
        self.dubt_insert_range_virtual(src, self.next_to_update, virtual_pos);
        self.next_to_update = virtual_pos;
        self.dubt_find_match_virtual(
            prefix_chain,
            src,
            pos,
            params,
            _literal_length,
            self.stored_index_floor(prefix_low, source_low),
        )
    }

    fn find_prefix_chain_match(
        &self,
        prefix_chain: PrefixChain<'_>,
        src: &[u8],
        pos: usize,
        params: MatchFinderParameters,
        literal_length: usize,
        prefix_low: usize,
    ) -> Option<MatchCandidate> {
        Self::find_prefix_chain_match(
            self,
            prefix_chain,
            src,
            pos,
            params,
            literal_length,
            prefix_low,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `children` past `isize::MAX` bytes is a `capacity overflow` panic
    /// rather than a refused allocation, and `panic = "abort"` makes that
    /// unrecoverable. `chain_log` 29 and 30 reach it on a 32-bit target.
    #[test]
    fn no_chain_log_in_range_asks_for_a_table_past_the_address_space() {
        // `ParameterOverrides::CHAIN_LOG`, which `validate` accepts in full on
        // every target.
        for chain_log in 6..=30u32 {
            let slots = 1usize << binary_tree_cycle_log(chain_log);
            let bytes = slots.checked_mul(2 * size_of::<u32>());
            assert!(
                bytes.is_some_and(|bytes| bytes <= isize::MAX as usize),
                "chain_log {chain_log} sizes children past isize::MAX"
            );
        }
    }

    /// The streaming encoder aligns compaction to
    /// [`MatchFinderParameters::rebase_period`], so a tree narrowed to fit the
    /// target has to leave that reading the narrowed cycle too -- otherwise
    /// every rebase moves nodes out of the slots their positions name.
    ///
    /// 32-bit only, and not merely as an optimization: the clamp in
    /// [`binary_tree_cycle_log`] is what the two readings could disagree about,
    /// and [`MAX_BINARY_TREE_CYCLE_LOG`] is 59 where `usize` is 64 bits, so no
    /// `chain_log` the parameters admit reaches it. The assertions would hold
    /// on a 64-bit host whatever the narrowing did, while the tree they build
    /// costs two gibibytes of `children`. Keep the gate: running this
    /// everywhere buys the suite an allocation, not a check.
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn a_narrowed_tree_keeps_the_parameters_rebase_period() {
        const CHAIN_LOG: u32 = 29;
        let params = MatchFinderParameters {
            parser_strategy: ParserStrategy::BinaryTreeLazy2,
            chain_log: CHAIN_LOG,
            ..MatchFinderParameters::default()
        };
        let finder = BinaryTreeFinder::new(20, CHAIN_LOG, 4);
        assert_eq!(finder.rebase_period(), params.rebase_period());
        assert_eq!(finder.children.len(), 2 * params.rebase_period());
    }

    /// `chain_log` 29 with a binary-tree strategy used to abort the process on
    /// a 32-bit target. One-shot never reached it -- `adjust_upstream_cparams`
    /// narrows the cycle log against the window once the source size is known
    /// -- so the streaming encoder, which has no such hint, is the path that
    /// has to hold.
    ///
    /// 32-bit only, because that is where the abort was: on a 64-bit host
    /// `chain_log` 29 is an ordinary parameter that no clamp touches, so the
    /// encode proves nothing about the narrowing and still pays two gibibytes
    /// for the tree. The `wasm32-wasip1` leg of CI runs the suite on a genuine
    /// 32-bit `usize`, which is where this one has to pass.
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn a_wide_chain_log_streams_instead_of_aborting() {
        let options = crate::EncoderOptions {
            compression_level: crate::CompressionLevel::try_new(3).unwrap(),
            parameters: crate::ParameterOverrides {
                strategy: Some(crate::Strategy::BinaryTreeLazy2),
                chain_log: Some(29),
                ..Default::default()
            },
            ..Default::default()
        };
        let input = b"a wide chain log streams a wide chain log streams";
        let mut encoder = crate::StreamingEncoder::new(options).unwrap();
        encoder.push(input).unwrap();
        encoder.finish().unwrap();
        assert_eq!(crate::decode_all(&encoder.take_output()).unwrap(), input);
    }
}
