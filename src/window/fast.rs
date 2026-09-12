use super::*;

#[derive(Debug, Clone)]
pub(crate) struct FastFinder {
    pub(crate) heads: Vec<u64>,
    pub(crate) hash_bits: u32,
    pub(crate) min_match: u32,
}

impl FastFinder {
    pub(crate) fn new(hash_bits: u32, min_match: u32) -> Self {
        let hash_bits = tagged_match_hash_bits(hash_bits);
        Self {
            heads: vec![NO_TAGGED_ENTRY; 1usize << hash_bits],
            hash_bits,
            min_match: min_match.clamp(4, 7),
        }
    }

    /// Read a hash table entry without bounds checking.
    ///
    /// # Safety
    ///
    /// `hash` must have been produced by one of the `hash_at_mls*` functions
    /// with `self.hash_bits`, which is what guarantees it is less than
    /// `self.heads.len()`.
    #[allow(unsafe_code)]
    #[inline(always)]
    pub(crate) unsafe fn get_head(&self, hash: usize) -> u64 {
        debug_assert!(hash < self.heads.len());
        // SAFETY: `hash` is in bounds by contract.
        unsafe { *self.heads.get_unchecked(hash) }
    }

    /// Write a hash table entry without bounds checking.
    ///
    /// # Safety
    ///
    /// Same requirement as [`Self::get_head`].
    #[allow(unsafe_code)]
    #[inline(always)]
    pub(crate) unsafe fn set_head(&mut self, hash: usize, entry: u64) {
        debug_assert!(hash < self.heads.len());
        // SAFETY: `hash` is in bounds by contract.
        unsafe { *self.heads.get_unchecked_mut(hash) = entry };
    }

    #[inline(always)]
    pub(crate) fn insert_src_position(&mut self, src: &[u8], pos: usize) {
        if pos + 8 <= src.len() {
            let ht = hash_short_cache_src_at_mls(src, pos, self.hash_bits, self.min_match);
            self.heads[tagged_index(ht)] = tagged_entry(pos, ht);
        }
    }

    pub(crate) fn insert_prefix(&mut self, prefix: &[u8]) {
        if prefix.len() >= 8 {
            for pos in 0..=prefix.len() - 8 {
                let ht =
                    hash_short_cache_prefix_at_mls(prefix, pos, self.hash_bits, self.min_match);
                self.heads[tagged_index(ht)] = tagged_entry(pos, ht);
            }
        }
    }

    /// Stride-3 fill matching C's `ZSTD_fillHashTableForCDict` with
    /// `ZSTD_dtlm_full`: the main positions (every 3rd) overwrite their hash
    /// slot unconditionally; the two positions in between each main position
    /// only fill slots that are still empty. Dense `insert_prefix` produces a
    /// different hash-table state than C's CDict, which causes Rust's Fast
    /// ext-dict parser to miss longer matches that C finds.
    pub(crate) fn insert_prefix_for_cdict(&mut self, prefix: &[u8]) {
        if prefix.len() < 8 {
            return;
        }
        let iend_pos = prefix.len() - 8;
        let step = 3usize;
        let mut pos = 0usize;
        while pos + step < iend_pos + 2 {
            let ht = hash_short_cache_prefix_at_mls(prefix, pos, self.hash_bits, self.min_match);
            self.heads[tagged_index(ht)] = tagged_entry(pos, ht);
            for extra in 1..step {
                let epos = pos + extra;
                if epos + 8 > prefix.len() {
                    break;
                }
                let eht =
                    hash_short_cache_prefix_at_mls(prefix, epos, self.hash_bits, self.min_match);
                let slot = tagged_index(eht);
                if self.heads[slot] == NO_TAGGED_ENTRY {
                    self.heads[slot] = tagged_entry(epos, eht);
                }
            }
            pos += step;
        }
    }

    /// Reset the hash table for a new frame without re-allocating.
    pub(crate) fn reset(&mut self) {
        self.heads.fill(NO_TAGGED_ENTRY);
    }

    /// Rebase every filed position by `delta`. See [`shift_tagged_positions`].
    pub(crate) fn shift_positions(&mut self, delta: usize) {
        shift_tagged_positions(&mut self.heads, delta);
    }

    pub(crate) fn insert_range(&mut self, src: &[u8], start: usize, end: usize) {
        let end = end.min(hash_insert_end(src.len()));
        for pos in start..end {
            self.insert_src_position(src, pos);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn insert_post_match(&mut self, src: &[u8], match_start: usize, match_end: usize) {
        for pos in [
            match_start.saturating_add(1),
            match_start.saturating_add(2),
            match_end.saturating_sub(2),
        ] {
            self.insert_src_position(src, pos);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedFastDictionaryTables {
    pub(crate) hash_table: Vec<u32>,
    pub(crate) hash_bits: u32,
    pub(crate) min_match: u32,
}

impl PreparedFastDictionaryTables {
    pub(crate) fn build(prefix: &[u8], hash_bits: u32, min_match: u32) -> Self {
        let hash_bits = hash_bits.clamp(10, MAX_MATCH_HASH_BITS);
        let min_match = min_match.clamp(4, 7);
        let mut hash_table = vec![0u32; 1usize << hash_bits];
        if prefix.len() >= 8 {
            let end = prefix.len().saturating_sub(8);
            let mut pos = 0usize;
            while pos + 3 < end + 2 {
                let hash_and_tag =
                    hash_short_cache_prefix_at_mls(prefix, pos, hash_bits, min_match);
                write_tagged_dict_index(&mut hash_table, hash_and_tag, pos);
                for extra in 1..3 {
                    let extra_pos = pos + extra;
                    if extra_pos + 8 > prefix.len() {
                        break;
                    }
                    let extra_hash_and_tag =
                        hash_short_cache_prefix_at_mls(prefix, extra_pos, hash_bits, min_match);
                    let slot = extra_hash_and_tag >> SHORT_CACHE_TAG_BITS;
                    if hash_table[slot] == 0 {
                        write_tagged_dict_index(&mut hash_table, extra_hash_and_tag, extra_pos);
                    }
                }
                pos += 3;
            }
        }
        Self {
            hash_table,
            hash_bits,
            min_match,
        }
    }

    /// Runtime-`min_match` candidate lookup. Only `window::tests` calls this;
    /// the hot loops all go through the monomorphized `candidate_at_mls`
    /// below, so this stays out of non-test builds.
    #[cfg(test)]
    pub(crate) fn candidate_at(&self, src: &[u8], pos: usize) -> Option<usize> {
        if pos + 8 > src.len() {
            return None;
        }
        let hash_and_tag = hash_short_cache_src_at_mls(src, pos, self.hash_bits, self.min_match);
        let entry = self.hash_table[hash_and_tag >> SHORT_CACHE_TAG_BITS];
        tagged_dict_candidate(entry, hash_and_tag)
    }

    /// Monomorphized dict candidate lookup used inside MLS-specialized hot loops.
    /// Returns `u32::MAX` on miss. The caller must guarantee
    /// `pos + 8 <= src.len()` and that the prepared dict's `min_match` equals `MLS`.
    #[inline(always)]
    #[allow(unsafe_code)]
    pub(crate) fn candidate_at_mls<const MLS: u32>(&self, src: &[u8], pos: usize) -> u32 {
        debug_assert!(pos + 8 <= src.len());
        debug_assert_eq!(self.min_match, MLS);
        let hash_and_tag = hash_at_mls_const_tagged::<MLS>(src, pos, self.hash_bits);
        // SAFETY: hash_at_mls_const_tagged yields a value in
        // [0, 1 << (hash_bits + SHORT_CACHE_TAG_BITS)); shifting right by
        // SHORT_CACHE_TAG_BITS gives an index in [0, 1 << hash_bits) which
        // equals hash_table.len().
        let entry = unsafe {
            *self
                .hash_table
                .get_unchecked(hash_and_tag >> SHORT_CACHE_TAG_BITS)
        };
        if entry == 0
            || (entry & SHORT_CACHE_TAG_MASK) != (hash_and_tag as u32 & SHORT_CACHE_TAG_MASK)
        {
            return u32::MAX;
        }
        (entry >> SHORT_CACHE_TAG_BITS).wrapping_sub(1)
    }
}

#[inline(always)]
pub(crate) fn fast_step_size(params: MatchFinderParameters) -> usize {
    params.fast_search_step.max(2)
}

pub(crate) fn fast_dict_match_step_size(params: MatchFinderParameters) -> usize {
    params.fast_search_step.saturating_sub(1).max(1)
}

pub(crate) fn repeat_offsets12(repeat_offsets: RepeatOffsets) -> (usize, usize) {
    let [rep1, rep2, _] = repeat_offsets.values();
    (rep1 as usize, rep2 as usize)
}

pub(crate) fn invalidate_no_dict_repeat_offsets(
    repeat_offsets: RepeatOffsets,
    current: usize,
    window_low: usize,
) -> (RepeatOffsets, usize, usize, usize, usize) {
    let [mut rep1, mut rep2, rep3] = repeat_offsets.values();
    let mut saved1 = 0usize;
    let mut saved2 = 0usize;
    let max_rep = current.saturating_sub(window_low);
    if rep2 as usize > max_rep {
        saved2 = rep2 as usize;
        rep2 = 0;
    }
    if rep1 as usize > max_rep {
        saved1 = rep1 as usize;
        rep1 = 0;
    }
    (
        RepeatOffsets::from_values([rep1, rep2, rep3]),
        rep1 as usize,
        rep2 as usize,
        saved1,
        saved2,
    )
}

pub(crate) fn restore_invalidated_repeat_offsets(
    repeat_offsets: RepeatOffsets,
    saved1: usize,
    mut saved2: usize,
) -> RepeatOffsets {
    let mut values = repeat_offsets.values();
    if saved1 != 0 && values[0] != 0 {
        saved2 = saved1;
    }
    if values[0] == 0 {
        values[0] = saved1.min(u32::MAX as usize) as u32;
    }
    if values[1] == 0 {
        values[1] = saved2.min(u32::MAX as usize) as u32;
    }
    RepeatOffsets::from_values(values)
}

pub(crate) fn fast_candidate_index_with_prefix(
    prefix_len: usize,
    prefix_candidate: usize,
    src_candidate: usize,
    prefix_low: usize,
    source_low: usize,
    current_pos: usize,
) -> Option<usize> {
    if src_candidate != NO_POS as usize
        && src_candidate >= source_low
        && src_candidate < current_pos
    {
        Some(prefix_len + src_candidate)
    } else if prefix_candidate != NO_POS as usize
        && prefix_candidate >= prefix_low
        && prefix_candidate < prefix_len
    {
        Some(prefix_candidate)
    } else {
        None
    }
}

pub(crate) fn prefixed_offset_match_start(
    prefix_len: usize,
    current_pos: usize,
    raw_offset: usize,
    prefix_low: usize,
    source_low: usize,
) -> Option<usize> {
    if raw_offset == 0 {
        return None;
    }
    let current = prefix_len + current_pos;
    if raw_offset > current {
        return None;
    }
    let match_start = current - raw_offset;
    logical_match_start_is_valid(prefix_len, match_start, prefix_low, source_low)
        .then_some(match_start)
}

pub(crate) fn prefixed_match_length_at(
    prefix: &[u8],
    src: &[u8],
    match_start: usize,
    current_pos: usize,
    initial_length: usize,
) -> usize {
    initial_length
        + count_match_length_with_prefix(
            prefix,
            src,
            match_start + initial_length,
            prefix.len() + current_pos + initial_length,
        )
}

#[inline(always)]
pub(crate) fn insert_fast_match_positions(
    finder: &mut FastFinder,
    src: &[u8],
    match_pos: usize,
    ip: usize,
) {
    finder.insert_src_position(src, match_pos.saturating_add(2));
    finder.insert_src_position(src, ip.saturating_sub(2));
}

#[inline(always)]
pub(crate) fn insert_double_fast_match_positions(
    finder: &mut DoubleFastFinder,
    src: &[u8],
    match_pos: usize,
    ip: usize,
) {
    let index_to_insert = match_pos.saturating_add(2);
    finder.insert_src_long_position(src, index_to_insert);
    finder.insert_src_long_position(src, ip.saturating_sub(2));
    finder.insert_src_short_position(src, index_to_insert);
    finder.insert_src_short_position(src, ip.saturating_sub(1));
}

#[allow(dead_code)]
pub(crate) fn plan_sequences_fast_without_prefix(
    src: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_fast_without_prefix_into(&mut plan, src, repeat_offsets, params)?;
    Ok(plan)
}

pub(crate) fn plan_sequences_fast_without_prefix_into(
    plan: &mut SequencePlan,
    src: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<()> {
    let mut finder = FastFinder::new(params.hash_bits, params.min_match);
    plan_sequences_fast_without_prefix_from_into(
        plan,
        src,
        0,
        repeat_offsets,
        params,
        0,
        0,
        &mut finder,
    )
}
#[allow(dead_code)]
pub(crate) fn plan_sequences_fast_without_prefix_from(
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    window_low: usize,
    rep_window_low: usize,
    finder: &mut FastFinder,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_fast_without_prefix_from_into(
        &mut plan,
        src,
        block_start,
        repeat_offsets,
        params,
        window_low,
        rep_window_low,
        finder,
    )?;
    Ok(plan)
}

pub(crate) fn plan_sequences_fast_without_prefix_from_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    window_low: usize,
    rep_window_low: usize,
    finder: &mut FastFinder,
) -> Result<()> {
    if plan.tracing_enabled() {
        match finder.min_match {
            4 => plan_sequences_fast_without_prefix_inner_tracing::<4>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
            5 => plan_sequences_fast_without_prefix_inner_tracing::<5>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
            6 => plan_sequences_fast_without_prefix_inner_tracing::<6>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
            _ => plan_sequences_fast_without_prefix_inner_tracing::<7>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
        }
    } else {
        match finder.min_match {
            4 => plan_sequences_fast_without_prefix_inner_no_trace::<4>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
            5 => plan_sequences_fast_without_prefix_inner_no_trace::<5>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
            6 => plan_sequences_fast_without_prefix_inner_no_trace::<6>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
            _ => plan_sequences_fast_without_prefix_inner_no_trace::<7>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
        };
        Ok(())
    }
}

// ---- Tracing path (preserves existing behavior for tests) ----

#[inline(always)]
fn chain_rep2_fast(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    repeat_offsets: &mut RepeatOffsets,
    rep_offsets: &mut (usize, usize),
    ip: &mut usize,
    finder: &mut FastFinder,
    current: usize,
    search_limit: usize,
    rep_window_low: usize,
) -> Result<()> {
    *ip = *anchor;
    *rep_offsets = repeat_offsets12(*repeat_offsets);
    if *ip <= search_limit {
        insert_fast_match_positions(finder, src, current, *ip);
        while *ip <= search_limit {
            let rep_length = count_rep_match_length(src, *ip, rep_offsets.1, rep_window_low);
            if rep_length < MIN_MATCH {
                break;
            }
            let rep_ip = *ip;
            store_lazy_sequence(
                plan,
                src,
                anchor,
                repeat_offsets,
                rep_ip,
                rep_offsets.1,
                rep_length,
            )?;
            finder.insert_src_position(src, rep_ip);
            *ip = *anchor;
            *rep_offsets = repeat_offsets12(*repeat_offsets);
        }
    }
    Ok(())
}

#[inline(always)]
fn plan_sequences_fast_without_prefix_inner_tracing<const MLS: u32>(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    window_low: usize,
    rep_window_low: usize,
    finder: &mut FastFinder,
) -> Result<()> {
    let block_len = src.len().saturating_sub(block_start);
    plan.reset_for_block(block_len);
    if block_len < MIN_MATCH {
        plan.literals.extend_from_slice(&src[block_start..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    let search_limit = src.len().saturating_sub(8);
    let mut anchor = block_start;
    let mut ip0 = block_start + usize::from(block_start == window_low);
    if ip0 >= search_limit {
        plan.literals.extend_from_slice(&src[anchor..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    let (mut repeat_offsets, _, _, saved1, saved2) =
        invalidate_no_dict_repeat_offsets(repeat_offsets, ip0, rep_window_low);
    let mut rep_offsets = repeat_offsets12(repeat_offsets);
    let step_incr = fast_step_increment(params);

    'start: loop {
        let mut step = fast_step_size(params);
        let mut next_step = ip0 + step_incr;
        let mut ip1 = ip0 + 1;
        let mut ip2 = ip0 + step;
        let mut ip3 = ip2 + 1;
        if ip3 >= search_limit {
            break;
        }

        let tagged_bits = finder.hash_bits + SHORT_CACHE_TAG_BITS;
        let mut ht0 = hash_at_mls_const::<MLS>(src, ip0, tagged_bits);
        let mut ht1 = hash_at_mls_const::<MLS>(src, ip1, tagged_bits);
        #[allow(unsafe_code)]
        let mut match_entry = unsafe { finder.get_head(tagged_index(ht0)) };

        #[allow(unsafe_code)]
        loop {
            if ip0 + 256 < src.len() {
                unsafe { crate::entropy::mem::prefetch_l1(src.as_ptr().add(ip0 + 256)) };
            }

            // Search-before-insert: defer hash table update until after
            // match checks to avoid store-load hazards.
            let current0 = ip0;

            {
                let rep_length = count_rep_match_length(src, ip2, rep_offsets.0, rep_window_low);
                if rep_length >= MIN_MATCH {
                    let mut start = ip2;
                    let mut length = rep_length;
                    if start > anchor
                        && ip2 - rep_offsets.0 > rep_window_low
                        && src[start - 1] == src[ip2 - rep_offsets.0 - 1]
                    {
                        start -= 1;
                        length += 1;
                    }
                    unsafe { finder.set_head(tagged_index(ht0), tagged_entry(current0, ht0)) };
                    unsafe { finder.set_head(tagged_index(ht1), tagged_entry(ip1, ht1)) };
                    store_lazy_sequence(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        start,
                        rep_offsets.0,
                        length,
                    )?;
                    chain_rep2_fast(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        &mut rep_offsets,
                        &mut ip0,
                        finder,
                        current0,
                        search_limit,
                        rep_window_low,
                    )?;
                    continue 'start;
                }
            }

            let match_idx = tagged_pos(match_entry);
            if match_idx.wrapping_sub(window_low) < ip0.wrapping_sub(window_low) {
                if unsafe { match_prefix_4bytes(src, match_idx, ip0) } {
                    let length = MIN_MATCH
                        + unsafe {
                            count_match_length_unchecked(
                                src,
                                match_idx + MIN_MATCH,
                                ip0 + MIN_MATCH,
                            )
                        };
                    unsafe { finder.set_head(tagged_index(ht0), tagged_entry(current0, ht0)) };
                    unsafe { finder.set_head(tagged_index(ht1), tagged_entry(ip1, ht1)) };
                    let found = extend_back_source_match(
                        src,
                        anchor,
                        DoubleFastMatch {
                            start: ip0,
                            offset: ip0 - match_idx,
                            length,
                        },
                    );
                    store_lazy_regular_sequence_with_source(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        found.start,
                        found.offset,
                        found.length,
                        SequenceTraceMatchSource::Unknown,
                    )?;
                    chain_rep2_fast(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        &mut rep_offsets,
                        &mut ip0,
                        finder,
                        current0,
                        search_limit,
                        rep_window_low,
                    )?;
                    continue 'start;
                }
            }

            // No match at ip0: insert and advance to second check position.
            unsafe { finder.set_head(tagged_index(ht0), tagged_entry(current0, ht0)) };

            match_entry = unsafe { finder.get_head(tagged_index(ht1)) };
            ht0 = ht1;
            ht1 = hash_at_mls_const::<MLS>(src, ip2, tagged_bits);
            ip0 = ip1;
            ip1 = ip2;
            ip2 = ip3;

            let current0 = ip0;
            let match_idx = tagged_pos(match_entry);
            if match_idx.wrapping_sub(window_low) < ip0.wrapping_sub(window_low) {
                if unsafe { match_prefix_4bytes(src, match_idx, ip0) } {
                    let length = MIN_MATCH
                        + unsafe {
                            count_match_length_unchecked(
                                src,
                                match_idx + MIN_MATCH,
                                ip0 + MIN_MATCH,
                            )
                        };
                    unsafe { finder.set_head(tagged_index(ht0), tagged_entry(current0, ht0)) };
                    if step <= MIN_MATCH {
                        unsafe { finder.set_head(tagged_index(ht1), tagged_entry(ip1, ht1)) };
                    }
                    let found = extend_back_source_match(
                        src,
                        anchor,
                        DoubleFastMatch {
                            start: ip0,
                            offset: ip0 - match_idx,
                            length,
                        },
                    );
                    store_lazy_regular_sequence_with_source(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        found.start,
                        found.offset,
                        found.length,
                        SequenceTraceMatchSource::Unknown,
                    )?;
                    chain_rep2_fast(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        &mut rep_offsets,
                        &mut ip0,
                        finder,
                        current0,
                        search_limit,
                        rep_window_low,
                    )?;
                    continue 'start;
                }
            }

            // No match at second position: insert and advance.
            unsafe { finder.set_head(tagged_index(ht0), tagged_entry(current0, ht0)) };

            match_entry = unsafe { finder.get_head(tagged_index(ht1)) };
            ht0 = ht1;
            ht1 = hash_at_mls_const::<MLS>(src, ip2, tagged_bits);
            ip0 = ip1;
            ip1 = ip2;
            ip2 = ip0 + step;
            ip3 = ip1 + step;
            if ip2 >= next_step {
                step += 1;
                next_step += step_incr;
                #[allow(unsafe_code)]
                unsafe {
                    crate::entropy::mem::prefetch_l1(src.as_ptr().add(ip1 + 64));
                    crate::entropy::mem::prefetch_l1(src.as_ptr().add(ip1 + 128));
                }
            }
            if ip3 >= search_limit {
                break;
            }
        }
    }

    plan.literals.extend_from_slice(&src[anchor..]);
    plan.repeat_offsets = restore_invalidated_repeat_offsets(repeat_offsets, saved1, saved2);
    Ok(())
}

// ---- No-trace fast path: no Result, local rep offset variables ----

/// No-trace rep2 chain for Fast. Uses local rep variables, returns `()`.
#[inline(always)]
fn chain_rep2_fast_no_trace(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    rep1: &mut usize,
    rep2: &mut usize,
    ip: &mut usize,
    finder: &mut FastFinder,
    current: usize,
    search_limit: usize,
    rep_window_low: usize,
) {
    *ip = *anchor;
    if *ip <= search_limit {
        insert_fast_match_positions(finder, src, current, *ip);
        // Collect positions for deferred batch insertion to reduce
        // store-buffer stalls in the inner rep2 chain loop.
        let mut deferred_positions: [usize; 32] = [0; 32];
        let mut deferred_count = 0usize;
        while *ip <= search_limit {
            let rep_length = count_rep_match_length(src, *ip, *rep2, rep_window_low);
            if rep_length < MIN_MATCH {
                break;
            }
            let rep_ip = *ip;
            std::mem::swap(&mut *rep2, &mut *rep1);
            let literal_length = rep_ip - *anchor;
            let offset_value = if literal_length == 0 { 1u32 } else { 2u32 };
            let sequence = SequenceCommand {
                literal_length: literal_length.min(u32::MAX as usize) as u32,
                offset_value,
                match_length: rep_length.min(u32::MAX as usize) as u32,
            };
            push_lazy_sequence_no_trace(plan, src, anchor, sequence);
            if deferred_count < 32 {
                deferred_positions[deferred_count] = rep_ip;
                deferred_count += 1;
            }
            *ip = *anchor;
        }
        // Batch-insert deferred positions after the chain completes.
        for i in 0..deferred_count {
            finder.insert_src_position(src, deferred_positions[i]);
        }
    }
}

/// Store a rep-match sequence using local rep offset variables for Fast.
#[inline(always)]
fn store_rep_sequence_fast_no_trace(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    start: usize,
    rep_offset: usize,
    match_length: usize,
    rep1: &mut usize,
    rep2: &mut usize,
) {
    let literal_length = start - *anchor;
    let offset_value = if literal_length > 0 && rep_offset == *rep1 {
        1u32
    } else if rep_offset == *rep2 {
        std::mem::swap(&mut *rep2, &mut *rep1);
        if literal_length == 0 { 1u32 } else { 2u32 }
    } else {
        debug_assert!(rep_offset == *rep1);
        1u32
    };
    let sequence = SequenceCommand {
        literal_length: literal_length.min(u32::MAX as usize) as u32,
        offset_value,
        match_length: match_length.min(u32::MAX as usize) as u32,
    };
    push_lazy_sequence_no_trace(plan, src, anchor, sequence);
}

/// Store an explicit-offset sequence using local rep offset variables for Fast.
#[inline(always)]
fn store_explicit_sequence_fast_no_trace(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    start: usize,
    raw_offset: usize,
    match_length: usize,
    rep1: &mut usize,
    rep2: &mut usize,
) {
    let literal_length = start - *anchor;
    let raw_offset_u32 = raw_offset.min(u32::MAX as usize) as u32;
    let offset_value = raw_offset_u32 + 3;
    *rep2 = *rep1;
    *rep1 = raw_offset;
    let sequence = SequenceCommand {
        literal_length: literal_length.min(u32::MAX as usize) as u32,
        offset_value,
        match_length: match_length.min(u32::MAX as usize) as u32,
    };
    push_lazy_sequence_no_trace(plan, src, anchor, sequence);
}

#[inline(always)]
fn plan_sequences_fast_without_prefix_inner_no_trace<const MLS: u32>(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    window_low: usize,
    rep_window_low: usize,
    finder: &mut FastFinder,
) {
    let block_len = src.len().saturating_sub(block_start);
    plan.reset_for_block(block_len);
    if block_len < MIN_MATCH {
        plan.literals.extend_from_slice(&src[block_start..]);
        plan.repeat_offsets = repeat_offsets;
        return;
    }

    let search_limit = src.len().saturating_sub(8);
    let mut anchor = block_start;
    let mut ip0 = block_start + usize::from(block_start == window_low);
    if ip0 >= search_limit {
        plan.literals.extend_from_slice(&src[anchor..]);
        plan.repeat_offsets = repeat_offsets;
        return;
    }

    let (invalidated, _, _, saved1, saved2) =
        invalidate_no_dict_repeat_offsets(repeat_offsets, ip0, rep_window_low);
    let [r1, r2, _] = invalidated.values();
    let mut rep1 = r1 as usize;
    let mut rep2 = r2 as usize;
    let step_incr = fast_step_increment(params);

    'start: loop {
        let mut step = fast_step_size(params);
        let mut next_step = ip0 + step_incr;
        let mut ip1 = ip0 + 1;
        let mut ip2 = ip0 + step;
        let mut ip3 = ip2 + 1;
        if ip3 >= search_limit {
            break;
        }

        let tagged_bits = finder.hash_bits + SHORT_CACHE_TAG_BITS;
        let mut ht0 = hash_at_mls_const::<MLS>(src, ip0, tagged_bits);
        let mut ht1 = hash_at_mls_const::<MLS>(src, ip1, tagged_bits);
        #[allow(unsafe_code)]
        let mut match_entry = unsafe { finder.get_head(tagged_index(ht0)) };

        #[allow(unsafe_code)]
        loop {
            // Search-before-insert: defer hash table update until after
            // match checks to avoid store-load hazards.
            let current0 = ip0;

            // Inlined rep match check at ip2: after offset invalidation,
            // rep1 <= ip0 - rep_window_low, so ip2 - rep1 >= rep_window_low
            // and ip2 >= rep1 are guaranteed. ip3 < search_limit guarantees
            // ip2 + 4 <= src.len(). Only check rep1 != 0 + 4-byte compare.
            // C applies no window test here either, for the same reason.
            {
                let rep_match_pos = ip2.wrapping_sub(rep1);
                if rep1 != 0 && unsafe { match_prefix_4bytes(src, rep_match_pos, ip2) } {
                    let base_length = 4 + unsafe {
                        count_match_length_unchecked(src, rep_match_pos + 4, ip2 + 4)
                    };
                    let mut start = ip2;
                    let mut length = base_length;
                    if start > anchor && src[start - 1] == src[rep_match_pos - 1] {
                        start -= 1;
                        length += 1;
                    }
                    unsafe { finder.set_head(tagged_index(ht0), tagged_entry(current0, ht0)) };
                    unsafe { finder.set_head(tagged_index(ht1), tagged_entry(ip1, ht1)) };
                    store_rep_sequence_fast_no_trace(
                        plan,
                        src,
                        &mut anchor,
                        start,
                        rep1,
                        length,
                        &mut rep1,
                        &mut rep2,
                    );
                    chain_rep2_fast_no_trace(
                        plan,
                        src,
                        &mut anchor,
                        &mut rep1,
                        &mut rep2,
                        &mut ip0,
                        finder,
                        current0,
                        search_limit,
                        rep_window_low,
                    );
                    continue 'start;
                }
            }

            // Tag check filters ~255/256 of false hash collisions before
            // the expensive 4-byte memory compare in check_short_match_branchless.
            if tag_matches(match_entry, ht0)
                && unsafe {
                    check_short_match_branchless(src, tagged_pos(match_entry), ip0, window_low)
                }
            {
                let m = tagged_pos(match_entry);
                let length = MIN_MATCH
                    + unsafe { count_match_length_unchecked(src, m + MIN_MATCH, ip0 + MIN_MATCH) };
                unsafe { finder.set_head(tagged_index(ht0), tagged_entry(current0, ht0)) };
                unsafe { finder.set_head(tagged_index(ht1), tagged_entry(ip1, ht1)) };
                let found = extend_back_source_match(
                    src,
                    anchor,
                    DoubleFastMatch {
                        start: ip0,
                        offset: ip0 - m,
                        length,
                    },
                );
                store_explicit_sequence_fast_no_trace(
                    plan,
                    src,
                    &mut anchor,
                    found.start,
                    found.offset,
                    found.length,
                    &mut rep1,
                    &mut rep2,
                );
                chain_rep2_fast_no_trace(
                    plan,
                    src,
                    &mut anchor,
                    &mut rep1,
                    &mut rep2,
                    &mut ip0,
                    finder,
                    current0,
                    search_limit,
                    rep_window_low,
                );
                continue 'start;
            }

            // No match at ip0: insert and advance to second check position.
            unsafe { finder.set_head(tagged_index(ht0), tagged_entry(current0, ht0)) };

            match_entry = unsafe { finder.get_head(tagged_index(ht1)) };
            ht0 = ht1;
            ht1 = hash_at_mls_const::<MLS>(src, ip2, tagged_bits);
            ip0 = ip1;
            ip1 = ip2;
            ip2 = ip3;

            let current0 = ip0;
            if tag_matches(match_entry, ht0)
                && unsafe {
                    check_short_match_branchless(src, tagged_pos(match_entry), ip0, window_low)
                }
            {
                let m = tagged_pos(match_entry);
                let length = MIN_MATCH
                    + unsafe { count_match_length_unchecked(src, m + MIN_MATCH, ip0 + MIN_MATCH) };
                unsafe { finder.set_head(tagged_index(ht0), tagged_entry(current0, ht0)) };
                if step <= MIN_MATCH {
                    unsafe { finder.set_head(tagged_index(ht1), tagged_entry(ip1, ht1)) };
                }
                let found = extend_back_source_match(
                    src,
                    anchor,
                    DoubleFastMatch {
                        start: ip0,
                        offset: ip0 - m,
                        length,
                    },
                );
                store_explicit_sequence_fast_no_trace(
                    plan,
                    src,
                    &mut anchor,
                    found.start,
                    found.offset,
                    found.length,
                    &mut rep1,
                    &mut rep2,
                );
                chain_rep2_fast_no_trace(
                    plan,
                    src,
                    &mut anchor,
                    &mut rep1,
                    &mut rep2,
                    &mut ip0,
                    finder,
                    current0,
                    search_limit,
                    rep_window_low,
                );
                continue 'start;
            }

            // No match at second position: insert and advance.
            unsafe { finder.set_head(tagged_index(ht0), tagged_entry(current0, ht0)) };

            match_entry = unsafe { finder.get_head(tagged_index(ht1)) };
            ht0 = ht1;
            ht1 = hash_at_mls_const::<MLS>(src, ip2, tagged_bits);
            ip0 = ip1;
            ip1 = ip2;
            ip2 = ip0 + step;
            ip3 = ip1 + step;
            if ip2 >= next_step {
                step += 1;
                next_step += step_incr;
                #[allow(unsafe_code)]
                unsafe {
                    crate::entropy::mem::prefetch_l1(src.as_ptr().add(ip1 + 64));
                    crate::entropy::mem::prefetch_l1(src.as_ptr().add(ip1 + 128));
                }
            }
            if ip3 >= search_limit {
                break;
            }
        }
    }

    plan.literals.extend_from_slice(&src[anchor..]);
    let final_offsets = RepeatOffsets::from_values([
        rep1.min(u32::MAX as usize) as u32,
        rep2.min(u32::MAX as usize) as u32,
        invalidated.values()[2],
    ]);
    plan.repeat_offsets = restore_invalidated_repeat_offsets(final_offsets, saved1, saved2);
}
#[allow(dead_code)]
pub(crate) fn plan_sequences_fast_with_prefix(
    src: &[u8],
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_fast_with_prefix_into(&mut plan, src, prefix, repeat_offsets, params)?;
    Ok(plan)
}

pub(crate) fn plan_sequences_fast_with_prefix_into(
    plan: &mut SequencePlan,
    src: &[u8],
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<()> {
    let mut prefix_finder = FastFinder::new(params.hash_bits, params.min_match);
    prefix_finder.insert_prefix(prefix);
    let mut src_finder = FastFinder::new(params.hash_bits, params.min_match);
    plan_sequences_fast_with_prefix_from_into(
        plan,
        src,
        0,
        prefix,
        repeat_offsets,
        params,
        0,
        0,
        Some(&prefix_finder),
        &mut src_finder,
        PrefixMatchMode::ExtDict,
        None,
    )
}
#[allow(dead_code)]
pub(crate) fn plan_sequences_fast_with_prefix_from(
    src: &[u8],
    block_start: usize,
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prefix_finder: &FastFinder,
    src_finder: &mut FastFinder,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_fast_with_prefix_from_into(
        &mut plan,
        src,
        block_start,
        prefix,
        repeat_offsets,
        params,
        prefix_low,
        source_low,
        Some(prefix_finder),
        src_finder,
        PrefixMatchMode::ExtDict,
        None,
    )?;
    Ok(plan)
}

pub(crate) fn plan_sequences_fast_with_prefix_from_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prefix_finder: Option<&FastFinder>,
    src_finder: &mut FastFinder,
    mode: PrefixMatchMode,
    prepared: Option<&PreparedFastDictionaryTables>,
) -> Result<()> {
    match src_finder.min_match {
        4 => plan_sequences_fast_with_prefix_inner::<4>(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            prefix_low,
            source_low,
            prefix_finder,
            src_finder,
            mode,
            prepared,
        ),
        5 => plan_sequences_fast_with_prefix_inner::<5>(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            prefix_low,
            source_low,
            prefix_finder,
            src_finder,
            mode,
            prepared,
        ),
        6 => plan_sequences_fast_with_prefix_inner::<6>(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            prefix_low,
            source_low,
            prefix_finder,
            src_finder,
            mode,
            prepared,
        ),
        _ => plan_sequences_fast_with_prefix_inner::<7>(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            prefix_low,
            source_low,
            prefix_finder,
            src_finder,
            mode,
            prepared,
        ),
    }
}

fn plan_sequences_fast_with_prefix_inner<const MLS: u32>(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prefix_finder: Option<&FastFinder>,
    src_finder: &mut FastFinder,
    mode: PrefixMatchMode,
    prepared: Option<&PreparedFastDictionaryTables>,
) -> Result<()> {
    let block_len = src.len().saturating_sub(block_start);
    plan.reset_for_block(block_len);
    if block_len < MIN_MATCH {
        plan.literals.extend_from_slice(&src[block_start..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    let prefix_len = prefix.len();
    let search_limit = src.len().saturating_sub(8);
    let mut anchor = block_start;
    let mut ip0 = block_start;
    if ip0 >= search_limit {
        plan.literals.extend_from_slice(&src[anchor..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    let lowest_match_index = if source_low == 0 {
        prefix_low
    } else {
        prefix_len + source_low
    };
    let (mut repeat_offsets, saved1, saved2) = if mode == PrefixMatchMode::ExtDict {
        let current = prefix_len + ip0;
        let (repeat_offsets, _, _, saved1, saved2) =
            invalidate_no_dict_repeat_offsets(repeat_offsets, current, lowest_match_index);
        (repeat_offsets, saved1, saved2)
    } else {
        (repeat_offsets, 0, 0)
    };
    let mut rep_offsets = repeat_offsets12(repeat_offsets);
    let step_incr = fast_step_increment(params);

    if mode == PrefixMatchMode::DictMatchState {
        // Always prepared, and never with a prefix table:
        // `PrefixedBlockMatchState` builds the prepared tables itself in this
        // mode when the dictionary had none cached, and skips the prefix table
        // entirely. A prefix walk once stood here for the case where neither
        // was true, and it could not have run -- its first act was to unwrap
        // the prefix table that this same mode declines to build, so reaching
        // it was a panic and never a parse.
        // `the_prefix_table_is_built_exactly_when_a_parse_reads_it` pins both
        // halves, for this parser and for `DoubleFast`, which had the same
        // walk behind the same wrong comment.
        let prepared =
            prepared.expect("a dict-match-state fast parse must have been given prepared tables");
        return plan_sequences_fast_with_prepared_dict_from_into(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            prefix_low,
            source_low,
            prepared,
            src_finder,
        );
    } else {
        let prefix_finder = prefix_finder
            .expect("a prefixed fast parse reaching the prefix table must have been given one");
        'start: loop {
            let mut step = fast_step_size(params);
            let mut next_step = ip0 + step_incr;
            let mut ip1 = ip0 + 1;
            let mut ip2 = ip0 + step;
            let mut ip3 = ip2 + 1;
            if ip3 >= search_limit {
                break;
            }

            let tagged_bits = src_finder.hash_bits + SHORT_CACHE_TAG_BITS;
            let mut ht0 = hash_at_mls_const::<MLS>(src, ip0, tagged_bits);
            let mut ht1 = hash_at_mls_const::<MLS>(src, ip1, tagged_bits);
            // SAFETY: hash values are produced by hash_at_mls_const with tagged_bits, guaranteeing tagged_index is in range.
            #[allow(unsafe_code)]
            let mut src_candidate = unsafe { src_finder.get_head(tagged_index(ht0)) };
            #[allow(unsafe_code)]
            let mut prefix_candidate = unsafe { prefix_finder.get_head(tagged_index(ht0)) };

            #[allow(unsafe_code)]
            loop {
                let current0 = ip0;
                unsafe { src_finder.set_head(tagged_index(ht0), tagged_entry(current0, ht0)) };

                if let Some(mut match_start) = prefixed_offset_match_start(
                    prefix_len,
                    ip2,
                    rep_offsets.0,
                    prefix_low,
                    source_low,
                )
                .filter(|match_start| {
                    logical_match_has_length(prefix, src, *match_start, prefix_len + ip2, MIN_MATCH)
                }) {
                    let mut start = ip2;
                    let mut length = MIN_MATCH;
                    if start > anchor
                        && match_start > 0
                        && logical_match_start_is_valid(
                            prefix_len,
                            match_start - 1,
                            prefix_low,
                            source_low,
                        )
                        && src[start - 1] == logical_byte(prefix, src, match_start - 1)
                    {
                        start -= 1;
                        match_start -= 1;
                        length += 1;
                    }
                    length += count_match_length_with_prefix(
                        prefix,
                        src,
                        match_start + length,
                        prefix_len + start + length,
                    );
                    unsafe { src_finder.set_head(tagged_index(ht1), tagged_entry(ip1, ht1)) };
                    store_lazy_sequence(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        start,
                        rep_offsets.0,
                        length,
                    )?;
                    ip0 = anchor;
                    rep_offsets = repeat_offsets12(repeat_offsets);
                    if ip0 <= search_limit {
                        insert_fast_match_positions(src_finder, src, current0, ip0);
                        while let Some(rep_match_start) = prefixed_offset_match_start(
                            prefix_len,
                            ip0,
                            rep_offsets.1,
                            prefix_low,
                            source_low,
                        ) {
                            if !logical_match_has_length(
                                prefix,
                                src,
                                rep_match_start,
                                prefix_len + ip0,
                                MIN_MATCH,
                            ) {
                                break;
                            }
                            let rep_ip = ip0;
                            let rep_length = prefixed_match_length_at(
                                prefix,
                                src,
                                rep_match_start,
                                rep_ip,
                                MIN_MATCH,
                            );
                            store_lazy_sequence(
                                plan,
                                src,
                                &mut anchor,
                                &mut repeat_offsets,
                                rep_ip,
                                rep_offsets.1,
                                rep_length,
                            )?;
                            src_finder.insert_src_position(src, rep_ip);
                            ip0 = anchor;
                            rep_offsets = repeat_offsets12(repeat_offsets);
                            if ip0 > search_limit {
                                break;
                            }
                        }
                    }
                    continue 'start;
                }

                let candidate_index = fast_candidate_index_with_prefix(
                    prefix_len,
                    tagged_pos(prefix_candidate),
                    tagged_pos(src_candidate),
                    prefix_low,
                    source_low,
                    ip0,
                );
                if let Some(candidate_index) = candidate_index.filter(|candidate_index| {
                    match_prefix_4bytes_logical(prefix, src, *candidate_index, prefix_len + ip0)
                }) {
                    unsafe { src_finder.set_head(tagged_index(ht1), tagged_entry(ip1, ht1)) };
                    let offset = prefix_len + ip0 - candidate_index;
                    let match_min_start = if candidate_index < prefix_len {
                        prefix_low
                    } else {
                        prefix_len + source_low
                    };
                    let found = extend_back_logical_match_with_min_start(
                        prefix,
                        src,
                        anchor,
                        DoubleFastMatch {
                            start: ip0,
                            offset,
                            length: MIN_MATCH
                                + count_match_length_with_prefix(
                                    prefix,
                                    src,
                                    candidate_index + MIN_MATCH,
                                    prefix_len + ip0 + MIN_MATCH,
                                ),
                        },
                        match_min_start,
                    );
                    store_lazy_regular_sequence_with_source(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        found.start,
                        found.offset,
                        found.length,
                        SequenceTraceMatchSource::Unknown,
                    )?;
                    ip0 = anchor;
                    rep_offsets = repeat_offsets12(repeat_offsets);
                    if ip0 <= search_limit {
                        insert_fast_match_positions(src_finder, src, current0, ip0);
                        while let Some(rep_match_start) = prefixed_offset_match_start(
                            prefix_len,
                            ip0,
                            rep_offsets.1,
                            prefix_low,
                            source_low,
                        ) {
                            if !logical_match_has_length(
                                prefix,
                                src,
                                rep_match_start,
                                prefix_len + ip0,
                                MIN_MATCH,
                            ) {
                                break;
                            }
                            let rep_ip = ip0;
                            let rep_length = prefixed_match_length_at(
                                prefix,
                                src,
                                rep_match_start,
                                rep_ip,
                                MIN_MATCH,
                            );
                            store_lazy_sequence(
                                plan,
                                src,
                                &mut anchor,
                                &mut repeat_offsets,
                                rep_ip,
                                rep_offsets.1,
                                rep_length,
                            )?;
                            src_finder.insert_src_position(src, rep_ip);
                            ip0 = anchor;
                            rep_offsets = repeat_offsets12(repeat_offsets);
                            if ip0 > search_limit {
                                break;
                            }
                        }
                    }
                    continue 'start;
                }

                src_candidate = unsafe { src_finder.get_head(tagged_index(ht1)) };
                prefix_candidate = unsafe { prefix_finder.get_head(tagged_index(ht1)) };
                ht0 = ht1;
                ht1 = hash_at_mls_const::<MLS>(src, ip2, tagged_bits);
                ip0 = ip1;
                ip1 = ip2;
                ip2 = ip3;

                let current0 = ip0;
                unsafe { src_finder.set_head(tagged_index(ht0), tagged_entry(current0, ht0)) };
                let candidate_index = fast_candidate_index_with_prefix(
                    prefix_len,
                    tagged_pos(prefix_candidate),
                    tagged_pos(src_candidate),
                    prefix_low,
                    source_low,
                    ip0,
                );
                if let Some(candidate_index) = candidate_index.filter(|candidate_index| {
                    match_prefix_4bytes_logical(prefix, src, *candidate_index, prefix_len + ip0)
                }) {
                    if step <= MIN_MATCH {
                        unsafe { src_finder.set_head(tagged_index(ht1), tagged_entry(ip1, ht1)) };
                    }
                    let offset = prefix_len + ip0 - candidate_index;
                    let match_min_start = if candidate_index < prefix_len {
                        prefix_low
                    } else {
                        prefix_len + source_low
                    };
                    let found = extend_back_logical_match_with_min_start(
                        prefix,
                        src,
                        anchor,
                        DoubleFastMatch {
                            start: ip0,
                            offset,
                            length: MIN_MATCH
                                + count_match_length_with_prefix(
                                    prefix,
                                    src,
                                    candidate_index + MIN_MATCH,
                                    prefix_len + ip0 + MIN_MATCH,
                                ),
                        },
                        match_min_start,
                    );
                    store_lazy_regular_sequence_with_source(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        found.start,
                        found.offset,
                        found.length,
                        SequenceTraceMatchSource::Unknown,
                    )?;
                    ip0 = anchor;
                    rep_offsets = repeat_offsets12(repeat_offsets);
                    if ip0 <= search_limit {
                        insert_fast_match_positions(src_finder, src, current0, ip0);
                        while let Some(rep_match_start) = prefixed_offset_match_start(
                            prefix_len,
                            ip0,
                            rep_offsets.1,
                            prefix_low,
                            source_low,
                        ) {
                            if !logical_match_has_length(
                                prefix,
                                src,
                                rep_match_start,
                                prefix_len + ip0,
                                MIN_MATCH,
                            ) {
                                break;
                            }
                            let rep_ip = ip0;
                            let rep_length = prefixed_match_length_at(
                                prefix,
                                src,
                                rep_match_start,
                                rep_ip,
                                MIN_MATCH,
                            );
                            store_lazy_sequence(
                                plan,
                                src,
                                &mut anchor,
                                &mut repeat_offsets,
                                rep_ip,
                                rep_offsets.1,
                                rep_length,
                            )?;
                            src_finder.insert_src_position(src, rep_ip);
                            ip0 = anchor;
                            rep_offsets = repeat_offsets12(repeat_offsets);
                            if ip0 > search_limit {
                                break;
                            }
                        }
                    }
                    continue 'start;
                }

                src_candidate = unsafe { src_finder.get_head(tagged_index(ht1)) };
                prefix_candidate = unsafe { prefix_finder.get_head(tagged_index(ht1)) };
                ht0 = ht1;
                ht1 = hash_at_mls_const::<MLS>(src, ip2, tagged_bits);
                ip0 = ip1;
                ip1 = ip2;
                ip2 = ip0 + step;
                ip3 = ip1 + step;
                if ip2 >= next_step {
                    step += 1;
                    next_step += step_incr;
                }
                if ip3 >= search_limit {
                    break;
                }
            }
        }
    }

    plan.literals.extend_from_slice(&src[anchor..]);
    plan.repeat_offsets = if mode == PrefixMatchMode::ExtDict {
        restore_invalidated_repeat_offsets(repeat_offsets, saved1, saved2)
    } else {
        repeat_offsets
    };
    Ok(())
}

pub(crate) fn plan_sequences_fast_with_prepared_dict_from_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prepared: &PreparedFastDictionaryTables,
    src_finder: &mut FastFinder,
) -> Result<()> {
    match src_finder.min_match {
        4 => plan_sequences_fast_with_prepared_dict_inner::<4>(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            prefix_low,
            source_low,
            prepared,
            src_finder,
        ),
        5 => plan_sequences_fast_with_prepared_dict_inner::<5>(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            prefix_low,
            source_low,
            prepared,
            src_finder,
        ),
        6 => plan_sequences_fast_with_prepared_dict_inner::<6>(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            prefix_low,
            source_low,
            prepared,
            src_finder,
        ),
        _ => plan_sequences_fast_with_prepared_dict_inner::<7>(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            prefix_low,
            source_low,
            prepared,
            src_finder,
        ),
    }
}

fn plan_sequences_fast_with_prepared_dict_inner<const MLS: u32>(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    prefix: &[u8],
    mut repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prepared: &PreparedFastDictionaryTables,
    src_finder: &mut FastFinder,
) -> Result<()> {
    let prefix_len = prefix.len();
    let search_limit = src.len().saturating_sub(8);
    let mut anchor = block_start;
    let mut ip0 = block_start;
    if ip0 >= search_limit {
        plan.literals.extend_from_slice(&src[anchor..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    let mut rep_offsets = repeat_offsets12(repeat_offsets);
    let step_size = fast_dict_match_step_size(params);
    let step_incr = fast_step_increment(params);
    let tagged_bits = src_finder.hash_bits + SHORT_CACHE_TAG_BITS;
    let mut ip1 = ip0 + step_size;
    while ip1 <= search_limit {
        let mut ht0 = hash_at_mls_const::<MLS>(src, ip0, tagged_bits);
        // SAFETY: hash values are produced by hash_at_mls_const with tagged_bits, guaranteeing tagged_index is in range.
        #[allow(unsafe_code)]
        let mut src_candidate = unsafe { src_finder.get_head(tagged_index(ht0)) };
        let mut dict_candidate_raw = prepared.candidate_at_mls::<MLS>(src, ip0);
        let mut current = ip0;
        let mut step = step_size;
        let mut next_step = ip0.saturating_add(step_incr << 1);

        #[allow(unsafe_code)]
        loop {
            let current_ht = ht0;
            let ht1 = hash_at_mls_const::<MLS>(src, ip1, tagged_bits);
            unsafe {
                src_finder.set_head(tagged_index(current_ht), tagged_entry(current, current_ht))
            };

            if let Some(rep_match_start) = prefixed_offset_match_start(
                prefix_len,
                ip0 + 1,
                rep_offsets.0,
                prefix_low,
                source_low,
            )
            .filter(|match_start| {
                logical_match_has_length(prefix, src, *match_start, prefix_len + ip0 + 1, MIN_MATCH)
            }) {
                let rep_length =
                    prefixed_match_length_at(prefix, src, rep_match_start, ip0 + 1, MIN_MATCH);
                store_lazy_sequence_with_source(
                    plan,
                    src,
                    &mut anchor,
                    &mut repeat_offsets,
                    ip0 + 1,
                    rep_offsets.0,
                    rep_length,
                    SequenceTraceMatchSource::Rep,
                )?;
                ip0 = anchor;
                rep_offsets = repeat_offsets12(repeat_offsets);
                if ip0 <= search_limit {
                    insert_fast_match_positions(src_finder, src, current, ip0);
                    while let Some(rep_match_start) = prefixed_offset_match_start(
                        prefix_len,
                        ip0,
                        rep_offsets.1,
                        prefix_low,
                        source_low,
                    ) {
                        if !logical_match_has_length(
                            prefix,
                            src,
                            rep_match_start,
                            prefix_len + ip0,
                            MIN_MATCH,
                        ) {
                            break;
                        }
                        let rep_ip = ip0;
                        let rep_length = prefixed_match_length_at(
                            prefix,
                            src,
                            rep_match_start,
                            rep_ip,
                            MIN_MATCH,
                        );
                        store_lazy_sequence_with_source(
                            plan,
                            src,
                            &mut anchor,
                            &mut repeat_offsets,
                            rep_ip,
                            rep_offsets.1,
                            rep_length,
                            SequenceTraceMatchSource::Rep,
                        )?;
                        src_finder.insert_src_position(src, rep_ip);
                        ip0 = anchor;
                        rep_offsets = repeat_offsets12(repeat_offsets);
                        if ip0 > search_limit {
                            break;
                        }
                    }
                }
                ip1 = ip0 + step_size;
                break;
            }

            let src_candidate_pos = tagged_pos(src_candidate);
            let source_valid = src_candidate != NO_TAGGED_ENTRY
                && src_candidate_pos >= source_low
                && src_candidate_pos < ip0;
            let dict_valid =
                dict_candidate_raw != u32::MAX && (dict_candidate_raw as usize) >= prefix_low;
            let (candidate_index, candidate_source, have_candidate) = if source_valid {
                (
                    prefix_len + src_candidate_pos,
                    SequenceTraceMatchSource::Source,
                    true,
                )
            } else if dict_valid {
                (
                    dict_candidate_raw as usize,
                    SequenceTraceMatchSource::Dict,
                    true,
                )
            } else {
                (0, SequenceTraceMatchSource::Source, false)
            };
            if have_candidate
                && match_prefix_4bytes_logical(prefix, src, candidate_index, prefix_len + ip0)
            {
                let offset = prefix_len + ip0 - candidate_index;
                let match_min_start = if candidate_index < prefix_len {
                    prefix_low
                } else {
                    prefix_len + source_low
                };
                let found = extend_back_logical_match_with_min_start(
                    prefix,
                    src,
                    anchor,
                    DoubleFastMatch {
                        start: ip0,
                        offset,
                        length: MIN_MATCH
                            + count_match_length_with_prefix(
                                prefix,
                                src,
                                candidate_index + MIN_MATCH,
                                prefix_len + ip0 + MIN_MATCH,
                            ),
                    },
                    match_min_start,
                );
                store_lazy_regular_sequence_with_source(
                    plan,
                    src,
                    &mut anchor,
                    &mut repeat_offsets,
                    found.start,
                    found.offset,
                    found.length,
                    candidate_source,
                )?;
                ip0 = anchor;
                rep_offsets = repeat_offsets12(repeat_offsets);
                if ip0 <= search_limit {
                    insert_fast_match_positions(src_finder, src, current, ip0);
                    while let Some(rep_match_start) = prefixed_offset_match_start(
                        prefix_len,
                        ip0,
                        rep_offsets.1,
                        prefix_low,
                        source_low,
                    ) {
                        if !logical_match_has_length(
                            prefix,
                            src,
                            rep_match_start,
                            prefix_len + ip0,
                            MIN_MATCH,
                        ) {
                            break;
                        }
                        let rep_ip = ip0;
                        let rep_length = prefixed_match_length_at(
                            prefix,
                            src,
                            rep_match_start,
                            rep_ip,
                            MIN_MATCH,
                        );
                        store_lazy_sequence_with_source(
                            plan,
                            src,
                            &mut anchor,
                            &mut repeat_offsets,
                            rep_ip,
                            rep_offsets.1,
                            rep_length,
                            SequenceTraceMatchSource::Rep,
                        )?;
                        src_finder.insert_src_position(src, rep_ip);
                        ip0 = anchor;
                        rep_offsets = repeat_offsets12(repeat_offsets);
                        if ip0 > search_limit {
                            break;
                        }
                    }
                }
                ip1 = ip0 + step_size;
                break;
            }

            src_candidate = unsafe { src_finder.get_head(tagged_index(ht1)) };
            dict_candidate_raw = prepared.candidate_at_mls::<MLS>(src, ip1);
            if ip1 >= next_step {
                step += 1;
                next_step = next_step.saturating_add(step_incr << 1);
            }
            ip0 = ip1;
            ip1 = ip1.saturating_add(step);
            if ip1 > search_limit {
                break;
            }
            current = ip0;
            ht0 = ht1;
        }

        if ip1 > search_limit {
            break;
        }
    }

    plan.literals.extend_from_slice(&src[anchor..]);
    plan.repeat_offsets = repeat_offsets;
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn search_fast_without_prefix(
    src: &[u8],
    pos: usize,
    anchor: usize,
    repeat_offsets: [u32; 3],
    finder: &mut FastFinder,
    params: MatchFinderParameters,
    window_low: usize,
) -> Option<DoubleFastMatch> {
    if pos + MIN_MATCH > src.len() {
        return None;
    }

    let step_incr = fast_step_increment(params);
    let mut step = params.fast_search_step.max(1);
    let mut current = pos;
    let mut next_step = current.saturating_add(step_incr);

    while current + MIN_MATCH <= src.len() {
        if let Some(length) =
            repeat_match_length_without_prefix(src, current, repeat_offsets[0] as usize, window_low)
        {
            return Some(DoubleFastMatch {
                start: current + 1,
                offset: repeat_offsets[0] as usize,
                length,
            });
        }

        if let Some(found) =
            fast_source_match_without_prefix(src, current, anchor, finder, params, window_low)
        {
            return Some(found);
        }

        let next = current + 1;
        if let Some(found) =
            fast_source_match_without_prefix(src, next, anchor, finder, params, window_low)
        {
            return Some(found);
        }

        current = current.saturating_add(step);
        if current >= next_step {
            step = step.saturating_add(1);
            next_step += step_incr;
            #[allow(unsafe_code)]
            if current + 64 < src.len() {
                unsafe { crate::entropy::mem::prefetch_l1(src.as_ptr().add(current + 64)) };
            }
        }
    }

    None
}
#[allow(dead_code)]
pub(crate) fn search_fast_with_prefix(
    prefix: &[u8],
    src: &[u8],
    pos: usize,
    anchor: usize,
    repeat_offsets: [u32; 3],
    prefix_finder: &FastFinder,
    src_finder: &mut FastFinder,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
) -> Option<DoubleFastMatch> {
    if pos + MIN_MATCH > src.len() {
        return None;
    }

    let step_incr = fast_step_increment(params);
    let mut step = params.fast_search_step.max(1);
    let mut current = pos;
    let mut next_step = current.saturating_add(step_incr);

    while current + MIN_MATCH <= src.len() {
        if let Some(length) = repeat_match_length_with_prefix(
            prefix,
            src,
            current,
            repeat_offsets[0] as usize,
            prefix_low,
            source_low,
        ) {
            return Some(DoubleFastMatch {
                start: current + 1,
                offset: repeat_offsets[0] as usize,
                length,
            });
        }

        if let Some(found) = fast_best_match_with_prefix(
            prefix,
            src,
            current,
            anchor,
            prefix_finder,
            src_finder,
            params,
            prefix_low,
            source_low,
        ) {
            return Some(found);
        }

        let next = current + 1;
        if let Some(found) = fast_best_match_with_prefix(
            prefix,
            src,
            next,
            anchor,
            prefix_finder,
            src_finder,
            params,
            prefix_low,
            source_low,
        ) {
            return Some(found);
        }

        current = current.saturating_add(step);
        if current >= next_step {
            step = step.saturating_add(1);
            next_step += step_incr;
        }
    }

    None
}

#[allow(dead_code)]
pub(crate) fn fast_source_match_without_prefix(
    src: &[u8],
    pos: usize,
    anchor: usize,
    finder: &mut FastFinder,
    params: MatchFinderParameters,
    window_low: usize,
) -> Option<DoubleFastMatch> {
    if pos + 8 > src.len() {
        return None;
    }

    let required_length = regular_match_length_threshold(pos.saturating_sub(anchor), params);
    let ht = hash_short_cache_src_at_mls(src, pos, finder.hash_bits, finder.min_match);
    let idx = tagged_index(ht);
    let entry = finder.heads[idx];
    finder.heads[idx] = tagged_entry(pos, ht);
    if entry == NO_TAGGED_ENTRY {
        return None;
    }
    let candidate = tagged_pos(entry);
    if candidate < window_low || candidate >= pos {
        return None;
    }
    #[allow(unsafe_code)]
    let length = unsafe { count_match_length_unchecked(src, candidate, pos) };
    if length < required_length {
        return None;
    }
    Some(extend_back_source_match(
        src,
        anchor,
        DoubleFastMatch {
            start: pos,
            offset: pos - candidate,
            length,
        },
    ))
}

#[allow(dead_code)]
pub(crate) fn fast_best_match_with_prefix(
    prefix: &[u8],
    src: &[u8],
    pos: usize,
    anchor: usize,
    prefix_finder: &FastFinder,
    src_finder: &mut FastFinder,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
) -> Option<DoubleFastMatch> {
    if pos + 8 > src.len() {
        return None;
    }

    let literal_length = pos.saturating_sub(anchor);
    let required_length = regular_match_length_threshold(literal_length, params);
    let ht = hash_short_cache_src_at_mls(src, pos, src_finder.hash_bits, src_finder.min_match);
    let idx = tagged_index(ht);
    let src_entry = src_finder.heads[idx];
    let prefix_entry = prefix_finder.heads[idx];
    src_finder.heads[idx] = tagged_entry(pos, ht);

    let src_match = if src_entry != NO_TAGGED_ENTRY {
        let src_cand = tagged_pos(src_entry);
        if src_cand >= source_low && src_cand < pos {
            #[allow(unsafe_code)]
            let length = unsafe { count_match_length_unchecked(src, src_cand, pos) };
            (length >= required_length).then(|| MatchCandidate {
                offset: pos - src_cand,
                length,
            })
        } else {
            None
        }
    } else {
        None
    };
    let pc = if prefix_entry != NO_TAGGED_ENTRY {
        tagged_pos(prefix_entry)
    } else {
        usize::MAX
    };
    let prefix_match = (prefix_entry != NO_TAGGED_ENTRY
        && pc >= prefix_low
        && pos + required_length <= src.len()
        && pc + required_length <= prefix.len()
        && logical_match_has_length(prefix, src, pc, prefix.len() + pos, required_length))
    .then(|| MatchCandidate {
        offset: prefix.len() + pos - pc,
        length: count_match_length_with_prefix(prefix, src, pc, prefix.len() + pos),
    });

    choose_better_regular_match_with_adjustment(
        prefix_match,
        src_match,
        literal_length,
        -params.source_score_penalty_with_prefix,
    )
    .map(|best| {
        if src_match == Some(best) {
            extend_back_source_match(
                src,
                anchor,
                DoubleFastMatch {
                    start: pos,
                    offset: best.offset,
                    length: best.length,
                },
            )
        } else {
            extend_back_logical_match(
                prefix,
                src,
                anchor,
                DoubleFastMatch {
                    start: pos,
                    offset: best.offset,
                    length: best.length,
                },
            )
        }
    })
}
