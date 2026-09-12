use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) struct DoubleFastMatch {
    pub(crate) start: usize,
    pub(crate) offset: usize,
    pub(crate) length: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct DoubleFastFinder {
    /// Filed positions with their tags, one [`long_entry`] per slot.
    ///
    /// The tag comes from the same hash that chose the slot, and the parser
    /// tests it before reading the eight source bytes the filed position
    /// names. That read is a scattered one, and it is the single most
    /// expensive thing in the search loop once this table stops fitting in
    /// cache. The filter is exact: equal eight bytes hash equally and so tag
    /// equally, and unlike the short table -- whose hash spans `min_match`
    /// bytes while its comparison spans four -- nothing here can reject a
    /// match the comparison would have taken.
    ///
    /// Sixty-four bits per slot rather than a `u32` beside a `u8`. The tag
    /// has to travel with the position because they are read together every
    /// iteration, and holding them in two arrays means two cache lines and
    /// two stores per iteration against upstream's one of each. It cannot
    /// share a `u32` with the position because the one-shot path files raw
    /// source indices beyond 16 MiB. Both tables keep the position and tag
    /// together in a `u64`.
    pub(crate) long_entries: Vec<u64>,
    pub(crate) short_heads: Vec<u64>,
    pub(crate) long_hash_bits: u32,
    pub(crate) short_hash_bits: u32,
    pub(crate) min_match: u32,
}

impl DoubleFastFinder {
    pub(crate) fn new(long_hash_bits: u32, short_hash_bits: u32, min_match: u32) -> Self {
        let long_hash_bits = long_hash_bits.clamp(10, MAX_MATCH_HASH_BITS);
        // Only the short table is hashed through the 32-bit tagged path; the
        // long one tags a 64-bit hash and has room to spare. See
        // `MAX_TAGGED_MATCH_HASH_BITS`.
        let short_hash_bits = tagged_match_hash_bits(short_hash_bits);
        Self {
            long_entries: vec![LONG_ENTRY_EMPTY; 1usize << long_hash_bits],
            short_heads: vec![NO_TAGGED_ENTRY; 1usize << short_hash_bits],
            long_hash_bits,
            short_hash_bits,
            min_match: min_match.clamp(4, 7),
        }
    }

    /// Read a long hash table slot without bounds checking: filed position and
    /// tag together, since the parser wants both and they share an entry.
    ///
    /// # Safety
    ///
    /// `tagged_index(hash_and_tag) < self.long_entries.len()`.
    #[allow(unsafe_code)]
    #[inline(always)]
    pub(crate) unsafe fn get_long_entry(&self, hash_and_tag: usize) -> u64 {
        let slot = tagged_index(hash_and_tag);
        debug_assert!(slot < self.long_entries.len());
        // SAFETY: `slot` is in bounds by contract.
        unsafe { *self.long_entries.get_unchecked(slot) }
    }

    /// File a position and its tag in the long table, without bounds checking.
    ///
    /// # Safety
    ///
    /// `tagged_index(hash_and_tag) < self.long_entries.len()`.
    #[allow(unsafe_code)]
    #[inline(always)]
    pub(crate) unsafe fn set_long_head(&mut self, hash_and_tag: usize, pos: u32) {
        let slot = tagged_index(hash_and_tag);
        debug_assert!(slot < self.long_entries.len());
        // SAFETY: `slot` is in bounds by contract.
        unsafe {
            *self.long_entries.get_unchecked_mut(slot) = long_entry(pos, hash_and_tag);
        }
    }

    /// File a position and its tag in the long table.
    ///
    /// The bounds-checked counterpart of [`Self::set_long_head`], for the
    /// insert paths that run outside the search loop.
    #[inline(always)]
    pub(crate) fn file_long_entry(&mut self, hash_and_tag: usize, pos: u32) {
        self.long_entries[tagged_index(hash_and_tag)] = long_entry(pos, hash_and_tag);
    }

    /// Read a short hash table entry without bounds checking.
    ///
    /// # Safety
    ///
    /// `hash < self.short_heads.len()`, i.e. the caller masked it to
    /// `short_hash_bits`.
    #[allow(unsafe_code)]
    #[inline(always)]
    pub(crate) unsafe fn get_short_head(&self, hash: usize) -> u64 {
        debug_assert!(hash < self.short_heads.len());
        // SAFETY: `hash` is in bounds by contract.
        unsafe { *self.short_heads.get_unchecked(hash) }
    }

    /// Write a short hash table entry without bounds checking.
    ///
    /// # Safety
    ///
    /// `hash < self.short_heads.len()`.
    #[allow(unsafe_code)]
    #[inline(always)]
    pub(crate) unsafe fn set_short_head(&mut self, hash: usize, entry: u64) {
        debug_assert!(hash < self.short_heads.len());
        // SAFETY: `hash` is in bounds by contract.
        unsafe { *self.short_heads.get_unchecked_mut(hash) = entry };
    }

    #[inline(always)]
    pub(crate) fn insert_src_position(&mut self, src: &[u8], pos: usize) {
        self.insert_src_short_position(src, pos);
        self.insert_src_long_position(src, pos);
    }

    #[inline(always)]
    pub(crate) fn insert_src_short_position(&mut self, src: &[u8], pos: usize) {
        if pos + 8 <= src.len() {
            let ht = hash_short_cache_src_at_mls(src, pos, self.short_hash_bits, self.min_match);
            self.short_heads[tagged_index(ht)] = tagged_entry(pos, ht);
        }
    }

    #[inline(always)]
    pub(crate) fn insert_src_long_position(&mut self, src: &[u8], pos: usize) {
        if pos + 8 <= src.len() {
            let long_hash = hash_long_at(src, pos, self.long_hash_bits);
            self.file_long_entry(long_hash, pos as u32);
        }
    }

    pub(crate) fn reset(&mut self) {
        self.long_entries.fill(LONG_ENTRY_EMPTY);
        self.short_heads.fill(NO_TAGGED_ENTRY);
    }

    /// Rebase every filed position by `delta`.
    ///
    /// The two tables are not encoded the same way: only the short table tags
    /// its entries, because only it is searched by tag. The long table holds
    /// raw positions and is confirmed by comparing eight bytes.
    pub(crate) fn shift_positions(&mut self, delta: usize) {
        // Only the positions move. A slot still describes the same eight
        // bytes after the rebase, so its tag is still the right one.
        shift_long_entries(&mut self.long_entries, delta);
        shift_tagged_positions(&mut self.short_heads, delta);
    }

    pub(crate) fn insert_prefix(&mut self, prefix: &[u8]) {
        if prefix.len() >= 8 {
            for pos in 0..=prefix.len() - 8 {
                let ht = hash_short_cache_prefix_at_mls(
                    prefix,
                    pos,
                    self.short_hash_bits,
                    self.min_match,
                );
                self.short_heads[tagged_index(ht)] = tagged_entry(pos, ht);
                if pos + 8 <= prefix.len() {
                    let long_hash = hash_long_prefix_at(prefix, pos, self.long_hash_bits);
                    self.file_long_entry(long_hash, pos as u32);
                }
            }
        }
    }

    pub(crate) fn insert_prefix_ext_dict(&mut self, prefix: &[u8]) {
        if prefix.len() < 8 {
            return;
        }

        let end = prefix.len().saturating_sub(8);
        let mut pos = 0usize;
        while pos + 2 <= end {
            let ht =
                hash_short_cache_prefix_at_mls(prefix, pos, self.short_hash_bits, self.min_match);
            self.short_heads[tagged_index(ht)] = tagged_entry(pos, ht);

            let long_hash = hash_long_prefix_at(prefix, pos, self.long_hash_bits);
            self.file_long_entry(long_hash, pos as u32);
            pos += 3;
        }
    }

    pub(crate) fn insert_range(&mut self, src: &[u8], start: usize, end: usize) {
        let end = end.min(hash_insert_end(src.len()));
        for pos in start..end {
            self.insert_src_position(src, pos);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn insert_post_match(&mut self, src: &[u8], search_pos: usize, match_end: usize) {
        for pos in [
            search_pos.saturating_add(2),
            match_end.saturating_sub(2),
            match_end.saturating_sub(1),
        ] {
            self.insert_src_position(src, pos);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedDoubleFastDictionaryTables {
    pub(crate) long_hash_table: Vec<u32>,
    pub(crate) short_hash_table: Vec<u32>,
    pub(crate) long_hash_bits: u32,
    pub(crate) short_hash_bits: u32,
    pub(crate) min_match: u32,
}

impl PreparedDoubleFastDictionaryTables {
    pub(crate) fn build(
        prefix: &[u8],
        long_hash_bits: u32,
        short_hash_bits: u32,
        min_match: u32,
    ) -> Self {
        let long_hash_bits = long_hash_bits.clamp(10, MAX_MATCH_HASH_BITS);
        // Only the short table is hashed through the 32-bit tagged path; the
        // long one tags a 64-bit hash and has room to spare. See
        // `MAX_TAGGED_MATCH_HASH_BITS`.
        let short_hash_bits = tagged_match_hash_bits(short_hash_bits);
        let min_match = min_match.clamp(4, 7);
        let mut long_hash_table = vec![0u32; 1usize << long_hash_bits];
        let mut short_hash_table = vec![0u32; 1usize << short_hash_bits];
        if prefix.len() >= 8 {
            let end = prefix.len().saturating_sub(8);
            let mut pos = 0usize;
            while pos + 2 <= end {
                for extra in 0..3 {
                    let extra_pos = pos + extra;
                    if extra_pos + 8 > prefix.len() {
                        break;
                    }
                    let short_hash_and_tag = hash_short_cache_prefix_at_mls(
                        prefix,
                        extra_pos,
                        short_hash_bits,
                        min_match,
                    );
                    let long_hash_and_tag =
                        hash_short_cache_long_prefix_at(prefix, extra_pos, long_hash_bits);
                    if extra == 0 {
                        write_tagged_dict_index(
                            &mut short_hash_table,
                            short_hash_and_tag,
                            extra_pos,
                        );
                    }
                    let long_slot = long_hash_and_tag >> SHORT_CACHE_TAG_BITS;
                    if extra == 0 || long_hash_table[long_slot] == 0 {
                        write_tagged_dict_index(&mut long_hash_table, long_hash_and_tag, extra_pos);
                    }
                }
                pos += 3;
            }
        }
        Self {
            long_hash_table,
            short_hash_table,
            long_hash_bits,
            short_hash_bits,
            min_match,
        }
    }

    /// Test-only probe: `window::tests` and `encode::tests` use these to
    /// assert a prepared dictionary's tables resolve a known position. The
    /// hot loops use the `*_at_mls` / `*_at_fast` forms below.
    #[cfg(test)]
    pub(crate) fn long_candidate_at(&self, src: &[u8], pos: usize) -> Option<usize> {
        if pos + 8 > src.len() {
            return None;
        }
        let hash_and_tag = hash_short_cache_long_src_at(src, pos, self.long_hash_bits);
        let entry = self.long_hash_table[hash_and_tag >> SHORT_CACHE_TAG_BITS];
        tagged_dict_candidate(entry, hash_and_tag)
    }

    #[cfg(test)]
    pub(crate) fn short_candidate_at(&self, src: &[u8], pos: usize) -> Option<usize> {
        if pos + 8 > src.len() {
            return None;
        }
        let hash_and_tag =
            hash_short_cache_src_at_mls(src, pos, self.short_hash_bits, self.min_match);
        let entry = self.short_hash_table[hash_and_tag >> SHORT_CACHE_TAG_BITS];
        tagged_dict_candidate(entry, hash_and_tag)
    }

    /// Fast long candidate lookup. Caller must guarantee `pos + 8 <= src.len()`.
    /// Returns `u32::MAX` on miss.
    #[inline(always)]
    #[allow(unsafe_code)]
    pub(crate) fn long_candidate_at_fast(&self, src: &[u8], pos: usize) -> u32 {
        debug_assert!(pos + 8 <= src.len());
        let tagged_bits = self.long_hash_bits + SHORT_CACHE_TAG_BITS;
        let value = unsafe { crate::entropy::mem::read_u64_unchecked(src, pos) };
        // Matches hash_short_cache_long_src_at: hash_bytes_long_short_cache path.
        // That path hashes u64 with 0xCF1BBCDCB7A56463 and shifts by 64 - tagged_bits.
        let hash_and_tag =
            (value.wrapping_mul(0xCF1B_BCDC_B7A5_6463) >> (64 - tagged_bits)) as usize;
        let entry = unsafe {
            *self
                .long_hash_table
                .get_unchecked(hash_and_tag >> SHORT_CACHE_TAG_BITS)
        };
        if entry == 0
            || (entry & SHORT_CACHE_TAG_MASK) != (hash_and_tag as u32 & SHORT_CACHE_TAG_MASK)
        {
            return u32::MAX;
        }
        (entry >> SHORT_CACHE_TAG_BITS).wrapping_sub(1)
    }

    /// Fast short candidate lookup, MLS-monomorphized. Caller must guarantee
    /// `pos + 8 <= src.len()` and `self.min_match == MLS`.
    #[inline(always)]
    #[allow(unsafe_code)]
    pub(crate) fn short_candidate_at_mls<const MLS: u32>(&self, src: &[u8], pos: usize) -> u32 {
        debug_assert!(pos + 8 <= src.len());
        debug_assert_eq!(self.min_match, MLS);
        let hash_and_tag = hash_at_mls_const_tagged::<MLS>(src, pos, self.short_hash_bits);
        let entry = unsafe {
            *self
                .short_hash_table
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

#[allow(dead_code)]
pub(crate) fn plan_sequences_double_fast_without_prefix(
    src: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_double_fast_without_prefix_into(&mut plan, src, repeat_offsets, params)?;
    Ok(plan)
}

pub(crate) fn plan_sequences_double_fast_without_prefix_into(
    plan: &mut SequencePlan,
    src: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<()> {
    let mut finder = DoubleFastFinder::new(
        params.hash_bits,
        params.secondary_hash_bits,
        params.min_match,
    );
    plan_sequences_double_fast_without_prefix_from_into(
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
pub(crate) fn plan_sequences_double_fast_without_prefix_from(
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    window_low: usize,
    rep_window_low: usize,
    finder: &mut DoubleFastFinder,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_double_fast_without_prefix_from_into(
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

pub(crate) fn plan_sequences_double_fast_without_prefix_from_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    window_low: usize,
    rep_window_low: usize,
    finder: &mut DoubleFastFinder,
) -> Result<()> {
    if plan.tracing_enabled() {
        match finder.min_match {
            4 => plan_sequences_double_fast_without_prefix_inner_tracing::<4>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
            5 => plan_sequences_double_fast_without_prefix_inner_tracing::<5>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
            6 => plan_sequences_double_fast_without_prefix_inner_tracing::<6>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
            _ => plan_sequences_double_fast_without_prefix_inner_tracing::<7>(
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
            4 => plan_sequences_double_fast_without_prefix_inner_no_trace::<4>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
            5 => plan_sequences_double_fast_without_prefix_inner_no_trace::<5>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
            6 => plan_sequences_double_fast_without_prefix_inner_no_trace::<6>(
                plan,
                src,
                block_start,
                repeat_offsets,
                params,
                window_low,
                rep_window_low,
                finder,
            ),
            _ => plan_sequences_double_fast_without_prefix_inner_no_trace::<7>(
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
fn chain_rep2_double_fast(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    repeat_offsets: &mut RepeatOffsets,
    rep_offsets: &mut (usize, usize),
    ip: &mut usize,
    finder: &mut DoubleFastFinder,
    current: usize,
    search_limit: usize,
    rep_window_low: usize,
) -> Result<()> {
    *ip = *anchor;
    *rep_offsets = repeat_offsets12(*repeat_offsets);
    if *ip <= search_limit {
        insert_double_fast_match_positions(finder, src, current, *ip);
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
fn plan_sequences_double_fast_without_prefix_inner_tracing<const MLS: u32>(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    window_low: usize,
    rep_window_low: usize,
    finder: &mut DoubleFastFinder,
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
    let mut ip = block_start + usize::from(block_start == window_low);
    if ip >= search_limit {
        plan.literals.extend_from_slice(&src[anchor..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    let (mut repeat_offsets, _, _, saved1, saved2) =
        invalidate_no_dict_repeat_offsets(repeat_offsets, ip, rep_window_low);
    let mut rep_offsets = repeat_offsets12(repeat_offsets);
    let step_incr = fast_step_increment(params) << 1;

    loop {
        let mut step = 1usize;
        let mut next_step = ip + step_incr;
        let mut ip1 = ip + 1;
        if ip1 > search_limit {
            break;
        }

        let mut long_hash0 = hash_long_at(src, ip, finder.long_hash_bits);
        let mut sht0 = hash_at_mls_const_tagged::<MLS>(src, ip, finder.short_hash_bits);
        #[allow(unsafe_code)]
        let mut long_entry0 = unsafe { finder.get_long_entry(long_hash0) };

        #[allow(unsafe_code)]
        loop {
            if ip + 256 < src.len() {
                unsafe { crate::entropy::mem::prefetch_l1(src.as_ptr().add(ip + 256)) };
            }

            // Search-before-insert: load candidates first, defer hash table
            // updates until after match checks to avoid store-load hazards.
            let short_entry = unsafe { finder.get_short_head(tagged_index(sht0)) };
            let short_candidate = tagged_pos(short_entry);
            let current = ip;

            {
                let rep_length = count_rep_match_length(src, ip + 1, rep_offsets.0, rep_window_low);
                if rep_length >= MIN_MATCH {
                    unsafe { finder.set_long_head(long_hash0, current as u32) };
                    unsafe {
                        finder.set_short_head(tagged_index(sht0), tagged_entry(current, sht0))
                    };
                    store_lazy_sequence(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        ip + 1,
                        rep_offsets.0,
                        rep_length,
                    )?;
                    chain_rep2_double_fast(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        &mut rep_offsets,
                        &mut ip,
                        finder,
                        current,
                        search_limit,
                        rep_window_low,
                    )?;
                    break;
                }
            }

            let long_hash1 = hash_long_at(src, ip1, finder.long_hash_bits);
            let sht1 = hash_at_mls_const_tagged::<MLS>(src, ip1, finder.short_hash_bits);
            let long_candidate0 = long_entry_pos(long_entry0) as usize;
            if long_entry_tag_matches(long_entry0, long_hash0)
                && long_candidate0.wrapping_sub(window_low) < ip.wrapping_sub(window_low)
            {
                #[allow(unsafe_code)]
                if unsafe { match_prefix_8bytes(src, long_candidate0, ip) } {
                    let length = 8 + unsafe {
                        count_match_length_unchecked(src, long_candidate0 + 8, ip + 8)
                    };
                    let found = extend_back_source_match(
                        src,
                        anchor,
                        DoubleFastMatch {
                            start: ip,
                            offset: ip - long_candidate0,
                            length,
                        },
                    );
                    unsafe { finder.set_long_head(long_hash0, current as u32) };
                    unsafe {
                        finder.set_short_head(tagged_index(sht0), tagged_entry(current, sht0))
                    };
                    if step < MIN_MATCH {
                        unsafe { finder.set_long_head(long_hash1, ip1 as u32) };
                    }
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
                    chain_rep2_double_fast(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        &mut rep_offsets,
                        &mut ip,
                        finder,
                        current,
                        search_limit,
                        rep_window_low,
                    )?;
                    break;
                }
            }

            let long_entry1 = unsafe { finder.get_long_entry(long_hash1) };
            let long_candidate1 = long_entry_pos(long_entry1) as usize;
            if short_candidate.wrapping_sub(window_low) < ip.wrapping_sub(window_low) {
                #[allow(unsafe_code)]
                if unsafe { match_prefix_4bytes(src, short_candidate, ip) } {
                    let short_length = MIN_MATCH
                        + unsafe {
                            count_match_length_unchecked(
                                src,
                                short_candidate + MIN_MATCH,
                                ip + MIN_MATCH,
                            )
                        };
                    let mut start = ip;
                    let mut offset = ip - short_candidate;
                    let mut length = short_length;
                    if long_entry_tag_matches(long_entry1, long_hash1)
                        && long_candidate1.wrapping_sub(window_low) < ip1.wrapping_sub(window_low)
                    {
                        #[allow(unsafe_code)]
                        if unsafe { match_prefix_8bytes(src, long_candidate1, ip1) } {
                            let long_length = 8 + unsafe {
                                count_match_length_unchecked(src, long_candidate1 + 8, ip1 + 8)
                            };
                            if long_length > length {
                                start = ip1;
                                offset = ip1 - long_candidate1;
                                length = long_length;
                            }
                        }
                    }
                    let found = extend_back_source_match(
                        src,
                        anchor,
                        DoubleFastMatch {
                            start,
                            offset,
                            length,
                        },
                    );
                    unsafe { finder.set_long_head(long_hash0, current as u32) };
                    unsafe {
                        finder.set_short_head(tagged_index(sht0), tagged_entry(current, sht0))
                    };
                    if step < MIN_MATCH {
                        unsafe { finder.set_long_head(long_hash1, ip1 as u32) };
                    }
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
                    chain_rep2_double_fast(
                        plan,
                        src,
                        &mut anchor,
                        &mut repeat_offsets,
                        &mut rep_offsets,
                        &mut ip,
                        finder,
                        current,
                        search_limit,
                        rep_window_low,
                    )?;
                    break;
                }
            }

            // No match: insert current position and advance.
            unsafe { finder.set_long_head(long_hash0, current as u32) };
            unsafe { finder.set_short_head(tagged_index(sht0), tagged_entry(current, sht0)) };

            if ip1 >= next_step {
                step += 1;
                next_step += step_incr;
                #[allow(unsafe_code)]
                unsafe {
                    crate::entropy::mem::prefetch_l1(src.as_ptr().add(ip1 + 64));
                    crate::entropy::mem::prefetch_l1(src.as_ptr().add(ip1 + 128));
                }
            }

            ip = ip1;
            ip1 += step;
            long_hash0 = long_hash1;
            long_entry0 = long_entry1;
            sht0 = sht1;

            if ip1 > search_limit {
                break;
            }
        }

        if ip1 > search_limit {
            break;
        }
    }

    plan.literals.extend_from_slice(&src[anchor..]);
    plan.repeat_offsets = restore_invalidated_repeat_offsets(repeat_offsets, saved1, saved2);
    Ok(())
}

// ---- No-trace fast path: no Result, local rep offset variables ----

/// Store a rep-match sequence using local rep offset variables.
/// Matches C's inline offset encoding: rep match → offset_value=1, swap rep1/rep2.
#[inline(always)]
fn store_rep_sequence_no_trace(
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
    // Rep match with literal_length>0 is offset_value=1 (rep1 unchanged).
    // Rep match with literal_length==0 uses rep2 → offset_value=1 after swap.
    // In both DoubleFast rep paths, the rep offset IS rep1 (or rep2 after
    // swap in chain_rep2). The caller ensures the correct rep is passed.
    // We encode: offset_value = 1 for rep1 match, or compute via
    // encode_offset_value logic.
    let offset_value = if literal_length > 0 && rep_offset == *rep1 {
        // Rep1 match with ll>0: offset_value=1, no state change
        1u32
    } else if rep_offset == *rep2 {
        // Rep2 match: swap rep2 to front
        std::mem::swap(&mut *rep2, &mut *rep1);
        if literal_length == 0 { 1u32 } else { 2u32 }
    } else {
        // Rep1 match with ll==0 (already at front), offset_value=1
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

/// Store an explicit-offset sequence using local rep offset variables.
/// Matches C's inline `ZSTD_storeSeq` + `OFFSET_TO_OFFBASE` pattern.
#[inline(always)]
fn store_explicit_sequence_no_trace(
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

/// No-trace rep2 chain for DoubleFast. Uses local rep variables, returns `()`.
#[inline(always)]
fn chain_rep2_double_fast_no_trace(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    rep1: &mut usize,
    rep2: &mut usize,
    ip: &mut usize,
    finder: &mut DoubleFastFinder,
    current: usize,
    search_limit: usize,
    rep_window_low: usize,
) {
    *ip = *anchor;
    if *ip <= search_limit {
        insert_double_fast_match_positions(finder, src, current, *ip);
        while *ip <= search_limit {
            let rep_length = count_rep_match_length(src, *ip, *rep2, rep_window_low);
            if rep_length < MIN_MATCH {
                break;
            }
            let rep_ip = *ip;
            // Rep2 chain: store as rep match, swap rep1/rep2
            std::mem::swap(&mut *rep2, &mut *rep1);
            let literal_length = rep_ip - *anchor;
            let offset_value = if literal_length == 0 { 1u32 } else { 2u32 };
            let sequence = SequenceCommand {
                literal_length: literal_length.min(u32::MAX as usize) as u32,
                offset_value,
                match_length: rep_length.min(u32::MAX as usize) as u32,
            };
            push_lazy_sequence_no_trace(plan, src, anchor, sequence);
            finder.insert_src_position(src, rep_ip);
            *ip = *anchor;
        }
    }
}

#[inline(always)]
fn plan_sequences_double_fast_without_prefix_inner_no_trace<const MLS: u32>(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    window_low: usize,
    rep_window_low: usize,
    finder: &mut DoubleFastFinder,
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
    let mut ip = block_start + usize::from(block_start == window_low);
    if ip >= search_limit {
        plan.literals.extend_from_slice(&src[anchor..]);
        plan.repeat_offsets = repeat_offsets;
        return;
    }

    let (invalidated, _, _, saved1, saved2) =
        invalidate_no_dict_repeat_offsets(repeat_offsets, ip, rep_window_low);
    let [r1, r2, _] = invalidated.values();
    let mut rep1 = r1 as usize;
    let mut rep2 = r2 as usize;
    let step_incr = fast_step_increment(params) << 1;

    // Copy hash parameters to locals to help LLVM keep them in registers.
    let long_hash_bits = finder.long_hash_bits;
    let short_hash_bits = finder.short_hash_bits;
    let src_len = src.len();

    loop {
        let mut step = 1usize;
        let mut next_step = ip + step_incr;
        let mut ip1 = ip + 1;
        if ip1 > search_limit {
            break;
        }

        let mut long_hash0 = hash_long_at(src, ip, long_hash_bits);
        let mut sht0 = hash_at_mls_const_tagged::<MLS>(src, ip, short_hash_bits);
        #[allow(unsafe_code)]
        let mut long_entry0 = unsafe { finder.get_long_entry(long_hash0) };

        #[allow(unsafe_code)]
        loop {
            if ip + 256 < src_len {
                unsafe { crate::entropy::mem::prefetch_l1(src.as_ptr().add(ip + 256)) };
            }

            // Search-before-insert: load candidates first, defer hash table
            // updates until after match checks to avoid store-load hazards.
            let short_entry = unsafe { finder.get_short_head(tagged_index(sht0)) };
            let short_candidate = tagged_pos(short_entry);
            let current = ip;

            // Rep match check at ip+1.
            {
                let rep_pos = ip + 1;
                let rep_match_pos = rep_pos.wrapping_sub(rep1);
                if rep1 != 0 && unsafe { match_prefix_4bytes(src, rep_match_pos, rep_pos) } {
                    let rep_length = 4 + unsafe {
                        count_match_length_unchecked(src, rep_match_pos + 4, rep_pos + 4)
                    };
                    unsafe { finder.set_long_head(long_hash0, current as u32) };
                    unsafe {
                        finder.set_short_head(tagged_index(sht0), tagged_entry(current, sht0))
                    };
                    store_rep_sequence_no_trace(
                        plan,
                        src,
                        &mut anchor,
                        rep_pos,
                        rep1,
                        rep_length,
                        &mut rep1,
                        &mut rep2,
                    );
                    chain_rep2_double_fast_no_trace(
                        plan,
                        src,
                        &mut anchor,
                        &mut rep1,
                        &mut rep2,
                        &mut ip,
                        finder,
                        current,
                        search_limit,
                        rep_window_low,
                    );
                    break;
                }
            }

            let long_hash1 = hash_long_at(src, ip1, long_hash_bits);
            let sht1 = hash_at_mls_const_tagged::<MLS>(src, ip1, short_hash_bits);
            let long_candidate0 = long_entry_pos(long_entry0) as usize;
            // Tag first. It rules out ~255/256 of the hash collisions without
            // reading the eight source bytes the candidate names, and that
            // scattered read is what costs once the long table stops fitting in
            // cache. See `DoubleFastFinder::long_entries` for why the filter
            // cannot change which matches are found.
            if long_entry_tag_matches(long_entry0, long_hash0)
                && unsafe { check_long_match_branchless(src, long_candidate0, ip, window_low) }
            {
                let length =
                    8 + unsafe { count_match_length_unchecked(src, long_candidate0 + 8, ip + 8) };
                let found = extend_back_source_match(
                    src,
                    anchor,
                    DoubleFastMatch {
                        start: ip,
                        offset: ip - long_candidate0,
                        length,
                    },
                );
                unsafe { finder.set_long_head(long_hash0, current as u32) };
                unsafe { finder.set_short_head(tagged_index(sht0), tagged_entry(current, sht0)) };
                if step < MIN_MATCH {
                    unsafe { finder.set_long_head(long_hash1, ip1 as u32) };
                }
                store_explicit_sequence_no_trace(
                    plan,
                    src,
                    &mut anchor,
                    found.start,
                    found.offset,
                    found.length,
                    &mut rep1,
                    &mut rep2,
                );
                chain_rep2_double_fast_no_trace(
                    plan,
                    src,
                    &mut anchor,
                    &mut rep1,
                    &mut rep2,
                    &mut ip,
                    finder,
                    current,
                    search_limit,
                    rep_window_low,
                );
                break;
            }

            let long_entry1 = unsafe { finder.get_long_entry(long_hash1) };
            let long_candidate1 = long_entry_pos(long_entry1) as usize;
            // Tag check filters ~255/256 of false hash collisions before
            // the expensive 4-byte memory compare in check_short_match_branchless.
            if tag_matches(short_entry, sht0)
                && unsafe { check_short_match_branchless(src, short_candidate, ip, window_low) }
            {
                let short_length = MIN_MATCH
                    + unsafe {
                        count_match_length_unchecked(
                            src,
                            short_candidate + MIN_MATCH,
                            ip + MIN_MATCH,
                        )
                    };
                let mut start = ip;
                let mut offset = ip - short_candidate;
                let mut length = short_length;
                // Nested long candidate check, tag-filtered the same way.
                if long_entry_tag_matches(long_entry1, long_hash1)
                    && unsafe { check_long_match_branchless(src, long_candidate1, ip1, window_low) }
                {
                    let long_length = 8 + unsafe {
                        count_match_length_unchecked(src, long_candidate1 + 8, ip1 + 8)
                    };
                    if long_length > length {
                        start = ip1;
                        offset = ip1 - long_candidate1;
                        length = long_length;
                    }
                }
                let found = extend_back_source_match(
                    src,
                    anchor,
                    DoubleFastMatch {
                        start,
                        offset,
                        length,
                    },
                );
                unsafe { finder.set_long_head(long_hash0, current as u32) };
                unsafe { finder.set_short_head(tagged_index(sht0), tagged_entry(current, sht0)) };
                if step < MIN_MATCH {
                    unsafe { finder.set_long_head(long_hash1, ip1 as u32) };
                }
                store_explicit_sequence_no_trace(
                    plan,
                    src,
                    &mut anchor,
                    found.start,
                    found.offset,
                    found.length,
                    &mut rep1,
                    &mut rep2,
                );
                chain_rep2_double_fast_no_trace(
                    plan,
                    src,
                    &mut anchor,
                    &mut rep1,
                    &mut rep2,
                    &mut ip,
                    finder,
                    current,
                    search_limit,
                    rep_window_low,
                );
                break;
            }

            // No match: insert current position and advance.
            unsafe { finder.set_long_head(long_hash0, current as u32) };
            unsafe { finder.set_short_head(tagged_index(sht0), tagged_entry(current, sht0)) };

            // No match: advance position.
            if ip1 >= next_step {
                step += 1;
                next_step += step_incr;
                #[allow(unsafe_code)]
                unsafe {
                    crate::entropy::mem::prefetch_l1(src.as_ptr().add(ip1 + 64));
                    crate::entropy::mem::prefetch_l1(src.as_ptr().add(ip1 + 128));
                }
            }

            ip = ip1;
            ip1 += step;
            long_hash0 = long_hash1;
            long_entry0 = long_entry1;
            sht0 = sht1;

            if ip1 > search_limit {
                break;
            }
        }

        if ip1 > search_limit {
            break;
        }
    }

    plan.literals.extend_from_slice(&src[anchor..]);
    // Write back local rep offsets to RepeatOffsets for the final state.
    let final_offsets = RepeatOffsets::from_values([
        rep1.min(u32::MAX as usize) as u32,
        rep2.min(u32::MAX as usize) as u32,
        invalidated.values()[2],
    ]);
    plan.repeat_offsets = restore_invalidated_repeat_offsets(final_offsets, saved1, saved2);
}

#[allow(dead_code)]
pub(crate) fn plan_sequences_double_fast_with_prefix(
    src: &[u8],
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_double_fast_with_prefix_into(&mut plan, src, prefix, repeat_offsets, params)?;
    Ok(plan)
}

pub(crate) fn plan_sequences_double_fast_with_prefix_into(
    plan: &mut SequencePlan,
    src: &[u8],
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
) -> Result<()> {
    let mut prefix_finder = DoubleFastFinder::new(
        params.hash_bits,
        params.secondary_hash_bits,
        params.min_match,
    );
    prefix_finder.insert_prefix(prefix);
    let mut src_finder = DoubleFastFinder::new(
        params.hash_bits,
        params.secondary_hash_bits,
        params.min_match,
    );
    plan_sequences_double_fast_with_prefix_from_into(
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
pub(crate) fn plan_sequences_double_fast_with_prefix_from(
    src: &[u8],
    block_start: usize,
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prefix_finder: &DoubleFastFinder,
    src_finder: &mut DoubleFastFinder,
) -> Result<SequencePlan> {
    let mut plan = SequencePlan::default();
    plan_sequences_double_fast_with_prefix_from_into(
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

/// The prefixed double-fast parse, which is two parses behind one door.
///
/// Both of them dispatch on `min_match` themselves, so this does not: it does
/// the per-block bookkeeping they share and hands off. It was monomorphised on
/// `min_match` for as long as a third parse lived here inline.
pub(crate) fn plan_sequences_double_fast_with_prefix_from_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prefix_finder: Option<&DoubleFastFinder>,
    src_finder: &mut DoubleFastFinder,
    mode: PrefixMatchMode,
    prepared: Option<&PreparedDoubleFastDictionaryTables>,
) -> Result<()> {
    let block_len = src.len().saturating_sub(block_start);
    plan.reset_for_block(block_len);
    if block_len < MIN_MATCH {
        plan.literals.extend_from_slice(&src[block_start..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    let search_limit = src.len().saturating_sub(8);
    if block_start >= search_limit {
        plan.literals.extend_from_slice(&src[block_start..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    if mode == PrefixMatchMode::ExtDict {
        return plan_sequences_double_fast_with_ext_dict_from_into(
            plan,
            src,
            block_start,
            prefix,
            repeat_offsets,
            params,
            prefix_low,
            source_low,
            prefix_finder.expect(
                "a prefixed double-fast parse reaching the prefix table must have been given one",
            ),
            src_finder,
        );
    }

    // `DictMatchState` is all that is left, and it always arrives with prepared
    // tables: `PrefixedBlockMatchState` builds them itself in that mode when
    // the dictionary had none cached, and builds no prefix table at all. A
    // prefix walk once stood here for the case where neither was true, and it
    // could not have run -- its first act was to unwrap the prefix table that
    // that same mode declines to build, so reaching it was a panic and never a
    // parse. `the_prefix_table_is_built_exactly_when_a_parse_reads_it` pins
    // both halves.
    let prepared = prepared
        .expect("a dict-match-state double-fast parse must have been given prepared tables");
    plan_sequences_double_fast_with_prepared_dict_from_into(
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
    )
}

fn extdict_insert_src_short_position(
    finder: &mut DoubleFastFinder,
    src: &[u8],
    source_base: usize,
    pos: usize,
) {
    if pos + 8 <= src.len() {
        let sht = hash_short_cache_src_at_mls(src, pos, finder.short_hash_bits, finder.min_match);
        finder.short_heads[tagged_index(sht)] = tagged_entry(source_base + pos, sht);
    }
}

fn extdict_insert_src_long_position(
    finder: &mut DoubleFastFinder,
    src: &[u8],
    source_base: usize,
    pos: usize,
) {
    if pos + 8 <= src.len() {
        let long_hash = hash_long_at(src, pos, finder.long_hash_bits);
        finder.file_long_entry(long_hash, (source_base + pos) as u32);
    }
}

fn extdict_insert_double_fast_match_positions(
    finder: &mut DoubleFastFinder,
    src: &[u8],
    source_base: usize,
    current: usize,
    ip: usize,
) {
    let index_to_insert = current.saturating_add(2);
    extdict_insert_src_long_position(finder, src, source_base, index_to_insert);
    extdict_insert_src_long_position(finder, src, source_base, ip.saturating_sub(2));
    extdict_insert_src_short_position(finder, src, source_base, index_to_insert);
    extdict_insert_src_short_position(finder, src, source_base, ip.saturating_sub(1));
}

fn write_back_extdict_source_tables(
    source_base: usize,
    combined_finder: &DoubleFastFinder,
    src_finder: &mut DoubleFastFinder,
) {
    let source_base_u32 = source_base as u32;
    for (dst, entry) in src_finder
        .short_heads
        .iter_mut()
        .zip(combined_finder.short_heads.iter().copied())
    {
        let pos = tagged_pos(entry);
        *dst = if entry != NO_TAGGED_ENTRY && pos >= source_base {
            // Preserve the tag bits, adjust the position
            tagged_entry(pos - source_base, entry as usize)
        } else {
            NO_TAGGED_ENTRY
        };
    }
    // A slot names the same eight bytes on both sides, so its tag carries over
    // untouched and only the position is rebased.
    for (dst, entry) in src_finder
        .long_entries
        .iter_mut()
        .zip(combined_finder.long_entries.iter().copied())
    {
        let pos = long_entry_pos(entry);
        *dst = if pos != NO_POS && pos >= source_base_u32 {
            long_entry_with_pos(entry, pos - source_base_u32)
        } else {
            LONG_ENTRY_EMPTY
        };
    }
}

#[inline(always)]
fn extdict_logical_byte(prefix: &[u8], src: &[u8], prefix_base: usize, pos: usize) -> Option<u8> {
    let source_base = prefix_base + prefix.len();
    if pos < prefix_base {
        None
    } else if pos < source_base {
        prefix.get(pos - prefix_base).copied()
    } else {
        src.get(pos - source_base).copied()
    }
}

fn extdict_logical_match_start_is_valid(
    prefix_len: usize,
    prefix_base: usize,
    match_start: usize,
    prefix_low: usize,
    source_low: usize,
) -> bool {
    let source_base = prefix_base + prefix_len;
    if match_start < source_base {
        match_start >= prefix_base + prefix_low && match_start < source_base
    } else {
        match_start - source_base >= source_low
    }
}

#[inline(always)]
fn extdict_match_u32(
    prefix: &[u8],
    src: &[u8],
    prefix_base: usize,
    match_index: usize,
    current_index: usize,
) -> bool {
    let source_base = prefix_base + prefix.len();
    if current_index < source_base {
        return false;
    }
    let current_pos = current_index - source_base;
    if current_pos + 4 > src.len() || match_index < prefix_base {
        return false;
    }
    if match_index < source_base {
        let match_pos = match_index - prefix_base;
        match_pos + 4 <= prefix.len()
            && crate::entropy::mem::read_u32(prefix, match_pos)
                == crate::entropy::mem::read_u32(src, current_pos)
    } else {
        let match_pos = match_index - source_base;
        match_pos + 4 <= src.len()
            && crate::entropy::mem::read_u32(src, match_pos)
                == crate::entropy::mem::read_u32(src, current_pos)
    }
}

#[inline(always)]
fn extdict_match_u64(
    prefix: &[u8],
    src: &[u8],
    prefix_base: usize,
    match_index: usize,
    current_index: usize,
) -> bool {
    let source_base = prefix_base + prefix.len();
    if current_index < source_base {
        return false;
    }
    let current_pos = current_index - source_base;
    if current_pos + 8 > src.len() || match_index < prefix_base {
        return false;
    }
    if match_index < source_base {
        let match_pos = match_index - prefix_base;
        match_pos + 8 <= prefix.len()
            && crate::entropy::mem::read_u64(prefix, match_pos)
                == crate::entropy::mem::read_u64(src, current_pos)
    } else {
        let match_pos = match_index - source_base;
        match_pos + 8 <= src.len()
            && crate::entropy::mem::read_u64(src, match_pos)
                == crate::entropy::mem::read_u64(src, current_pos)
    }
}

#[inline(always)]
fn extdict_count_match_length_with_prefix(
    prefix: &[u8],
    src: &[u8],
    prefix_base: usize,
    left: usize,
    right: usize,
) -> usize {
    let source_base = prefix_base + prefix.len();
    debug_assert!(right >= source_base);
    let right_pos = right - source_base;
    if left < source_base {
        let left_pos = left - prefix_base;
        let prefix_match = count_match_length_slices(&prefix[left_pos..], &src[right_pos..]);
        if left_pos + prefix_match != prefix.len() {
            prefix_match
        } else {
            prefix_match + count_match_length_slices(src, &src[right_pos + prefix_match..])
        }
    } else {
        count_match_length(src, left - source_base, right_pos)
    }
}

fn extdict_prefixed_offset_match_start(
    prefix_len: usize,
    prefix_base: usize,
    current_pos: usize,
    raw_offset: usize,
    prefix_low: usize,
    source_low: usize,
) -> Option<usize> {
    if raw_offset == 0 {
        return None;
    }
    let current = prefix_base + prefix_len + current_pos;
    if raw_offset > current {
        return None;
    }
    let match_start = current - raw_offset;
    extdict_logical_match_start_is_valid(
        prefix_len,
        prefix_base,
        match_start,
        prefix_low,
        source_low,
    )
    .then_some(match_start)
}

fn extdict_extend_back_logical_match_with_min_start(
    prefix: &[u8],
    src: &[u8],
    prefix_base: usize,
    anchor: usize,
    mut found: DoubleFastMatch,
    min_match_start: usize,
) -> DoubleFastMatch {
    let source_base = prefix_base + prefix.len();
    let mut match_index = source_base + found.start - found.offset;
    while found.start > anchor
        && match_index > min_match_start
        && src[found.start - 1]
            == extdict_logical_byte(prefix, src, prefix_base, match_index - 1)
                .expect("valid extdict logical byte")
    {
        found.start -= 1;
        found.length += 1;
        match_index -= 1;
    }
    found
}

fn plan_sequences_double_fast_with_ext_dict_from_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    prefix: &[u8],
    mut repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prefix_finder: &DoubleFastFinder,
    src_finder: &mut DoubleFastFinder,
) -> Result<()> {
    let prefix_len = prefix.len();
    let prefix_base = params.ext_dict_index_bias;
    let source_base = prefix_base + prefix_len;
    // This parser merges the prefix and the source into one logical index
    // space, so the floor it compares candidates against has to be in that
    // space too. The pair it is handed is not: `prefix_low` indexes the prefix
    // and `source_low` the source, and testing candidates against
    // `prefix_base + prefix_low` alone leaves every source candidate unbounded
    // -- it is the bottom of the whole space. That let a block whose floor
    // forbade any match at all emit one reaching twice the declared window.
    //
    // While any of the prefix is inside the window the floor lands there and
    // the source is reachable throughout (`prefixed_window_lows` returns
    // `source_low == 0` in exactly that case); once the prefix has aged out no
    // part of it is reachable and the floor lands in the source. The same shape
    // as `BinaryTreeFinder::stored_index_floor`, which merges the two regions
    // the same way.
    let logical_low = if prefix_low < prefix_len {
        prefix_base + prefix_low
    } else {
        source_base + source_low
    };
    let search_limit = src.len().saturating_sub(8);
    let mut anchor = block_start;
    let mut ip = block_start;
    if ip >= search_limit {
        plan.literals.extend_from_slice(&src[anchor..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    let mut rep_offsets = repeat_offsets12(repeat_offsets);
    let mut combined_finder = prefix_finder.clone();
    let source_base_u32 = source_base as u32;
    let prefix_base_u32 = prefix_base as u32;
    for (dst, src_entry) in combined_finder
        .short_heads
        .iter_mut()
        .zip(src_finder.short_heads.iter().copied())
    {
        if src_entry != NO_TAGGED_ENTRY {
            let pos = tagged_pos(src_entry);
            *dst = tagged_entry(source_base + pos, src_entry as usize);
        } else if *dst != NO_TAGGED_ENTRY {
            let pos = tagged_pos(*dst);
            *dst = tagged_entry(prefix_base + pos, *dst as usize);
        }
    }
    for (dst, src_entry) in combined_finder
        .long_entries
        .iter_mut()
        .zip(src_finder.long_entries.iter().copied())
    {
        let src_pos = long_entry_pos(src_entry);
        if src_pos != NO_POS {
            // The source side wins the slot outright, tag and all.
            *dst = long_entry_with_pos(src_entry, source_base_u32 + src_pos);
        } else {
            let dst_pos = long_entry_pos(*dst);
            if dst_pos != NO_POS {
                *dst = long_entry_with_pos(*dst, prefix_base_u32 + dst_pos);
            }
        }
    }

    while ip < search_limit {
        let sht = hash_short_cache_src_at_mls(
            src,
            ip,
            combined_finder.short_hash_bits,
            combined_finder.min_match,
        );
        let short_entry = combined_finder.short_heads[tagged_index(sht)];
        let match_index = tagged_pos(short_entry);
        let long_hash = hash_long_at(src, ip, combined_finder.long_hash_bits);
        let match_long_index =
            long_entry_pos(combined_finder.long_entries[tagged_index(long_hash)]) as usize;

        let current = ip;
        let current_logical = source_base + current;

        combined_finder.short_heads[tagged_index(sht)] = tagged_entry(current_logical, sht);
        combined_finder.file_long_entry(long_hash, current_logical as u32);

        if let Some(rep_match_start) = extdict_prefixed_offset_match_start(
            prefix_len,
            prefix_base,
            ip + 1,
            rep_offsets.0,
            prefix_low,
            source_low,
        )
        .filter(|match_start| {
            extdict_match_u32(prefix, src, prefix_base, *match_start, source_base + ip + 1)
        }) {
            let rep_length = MIN_MATCH
                + extdict_count_match_length_with_prefix(
                    prefix,
                    src,
                    prefix_base,
                    rep_match_start + MIN_MATCH,
                    source_base + ip + 1 + MIN_MATCH,
                );
            store_lazy_sequence(
                plan,
                src,
                &mut anchor,
                &mut repeat_offsets,
                ip + 1,
                rep_offsets.0,
                rep_length,
            )?;
            ip = anchor;
        } else if match_long_index != NO_POS as usize
            && match_long_index >= logical_low
            && match_long_index < current_logical
            && extdict_match_u64(prefix, src, prefix_base, match_long_index, current_logical)
        {
            let match_min_start = if match_long_index < source_base {
                prefix_base + prefix_low
            } else {
                source_base + source_low
            };
            let found = extdict_extend_back_logical_match_with_min_start(
                prefix,
                src,
                prefix_base,
                anchor,
                DoubleFastMatch {
                    start: ip,
                    offset: current_logical - match_long_index,
                    length: extdict_count_match_length_with_prefix(
                        prefix,
                        src,
                        prefix_base,
                        match_long_index,
                        current_logical,
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
            ip = anchor;
        } else if short_entry != NO_TAGGED_ENTRY
            && match_index >= logical_low
            && match_index < current_logical
            && extdict_match_u32(prefix, src, prefix_base, match_index, current_logical)
        {
            let next_long_hash = hash_long_at(src, ip + 1, combined_finder.long_hash_bits);
            let next_match_long_index =
                long_entry_pos(combined_finder.long_entries[tagged_index(next_long_hash)]) as usize;
            combined_finder.file_long_entry(next_long_hash, (current_logical + 1) as u32);

            let mut start = ip;
            let mut chosen_candidate_index = match_index;
            let mut offset = current_logical - match_index;
            let mut length = extdict_count_match_length_with_prefix(
                prefix,
                src,
                prefix_base,
                match_index,
                current_logical,
            );

            if next_match_long_index != NO_POS as usize
                && next_match_long_index >= logical_low
                && next_match_long_index < current_logical + 1
                && extdict_match_u64(
                    prefix,
                    src,
                    prefix_base,
                    next_match_long_index,
                    current_logical + 1,
                )
            {
                let next_length = extdict_count_match_length_with_prefix(
                    prefix,
                    src,
                    prefix_base,
                    next_match_long_index,
                    current_logical + 1,
                );
                if next_length > length {
                    start = ip + 1;
                    chosen_candidate_index = next_match_long_index;
                    offset = current_logical + 1 - next_match_long_index;
                    length = next_length;
                }
            }

            let match_min_start = if chosen_candidate_index < source_base {
                prefix_base + prefix_low
            } else {
                source_base + source_low
            };
            let found = extdict_extend_back_logical_match_with_min_start(
                prefix,
                src,
                prefix_base,
                anchor,
                DoubleFastMatch {
                    start,
                    offset,
                    length,
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
            ip = anchor;
        } else {
            ip =
                ip.saturating_add(((ip.saturating_sub(anchor)) >> params.skip_search_strength) + 1);
            continue;
        }

        rep_offsets = repeat_offsets12(repeat_offsets);
        if ip <= search_limit {
            extdict_insert_double_fast_match_positions(
                &mut combined_finder,
                src,
                source_base,
                current,
                ip,
            );
            while let Some(rep_match_start) = extdict_prefixed_offset_match_start(
                prefix_len,
                prefix_base,
                ip,
                rep_offsets.1,
                prefix_low,
                source_low,
            ) {
                if !extdict_match_u32(prefix, src, prefix_base, rep_match_start, source_base + ip) {
                    break;
                }
                extdict_insert_src_short_position(&mut combined_finder, src, source_base, ip);
                extdict_insert_src_long_position(&mut combined_finder, src, source_base, ip);
                let rep_ip = ip;
                let rep_length = MIN_MATCH
                    + extdict_count_match_length_with_prefix(
                        prefix,
                        src,
                        prefix_base,
                        rep_match_start + MIN_MATCH,
                        source_base + rep_ip + MIN_MATCH,
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
                ip = anchor;
                rep_offsets = repeat_offsets12(repeat_offsets);
                if ip > search_limit {
                    break;
                }
            }
        }
    }

    write_back_extdict_source_tables(source_base, &combined_finder, src_finder);
    plan.literals.extend_from_slice(&src[anchor..]);
    plan.repeat_offsets = repeat_offsets;
    Ok(())
}

pub(crate) fn plan_sequences_double_fast_with_prepared_dict_from_into(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    prefix: &[u8],
    repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prepared: &PreparedDoubleFastDictionaryTables,
    src_finder: &mut DoubleFastFinder,
) -> Result<()> {
    match src_finder.min_match {
        4 => plan_sequences_double_fast_with_prepared_dict_inner::<4>(
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
        5 => plan_sequences_double_fast_with_prepared_dict_inner::<5>(
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
        6 => plan_sequences_double_fast_with_prepared_dict_inner::<6>(
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
        _ => plan_sequences_double_fast_with_prepared_dict_inner::<7>(
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

fn plan_sequences_double_fast_with_prepared_dict_inner<const MLS: u32>(
    plan: &mut SequencePlan,
    src: &[u8],
    block_start: usize,
    prefix: &[u8],
    mut repeat_offsets: RepeatOffsets,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    prepared: &PreparedDoubleFastDictionaryTables,
    src_finder: &mut DoubleFastFinder,
) -> Result<()> {
    let prefix_len = prefix.len();
    let search_limit = src.len().saturating_sub(8);
    let mut anchor = block_start;
    let mut ip = block_start;
    if ip >= search_limit {
        plan.literals.extend_from_slice(&src[anchor..]);
        plan.repeat_offsets = repeat_offsets;
        return Ok(());
    }

    let mut rep_offsets = repeat_offsets12(repeat_offsets);
    // SAFETY: hash values are produced by hash functions with
    // finder.*_hash_bits, guaranteeing they are < finder.*_heads.len().
    #[allow(unsafe_code)]
    while ip < search_limit {
        let sht = hash_at_mls_const_tagged::<MLS>(src, ip, src_finder.short_hash_bits);
        let src_short_entry = unsafe { src_finder.get_short_head(tagged_index(sht)) };
        let src_short_candidate = tagged_pos(src_short_entry);
        let dict_short_candidate_raw = prepared.short_candidate_at_mls::<MLS>(src, ip);

        let long_hash = hash_long_at(src, ip, src_finder.long_hash_bits);
        let src_long_candidate =
            long_entry_pos(unsafe { src_finder.get_long_entry(long_hash) }) as usize;
        let dict_long_candidate_raw = prepared.long_candidate_at_fast(src, ip);
        let dict_short_candidate: Option<usize> =
            (dict_short_candidate_raw != u32::MAX).then_some(dict_short_candidate_raw as usize);
        let dict_long_candidate: Option<usize> =
            (dict_long_candidate_raw != u32::MAX).then_some(dict_long_candidate_raw as usize);

        let current = ip;
        unsafe { src_finder.set_short_head(tagged_index(sht), tagged_entry(current, sht)) };
        unsafe { src_finder.set_long_head(long_hash, current as u32) };

        if let Some(rep_match_start) =
            prefixed_offset_match_start(prefix_len, ip + 1, rep_offsets.0, prefix_low, source_low)
                .filter(|match_start| {
                    logical_match_has_length(
                        prefix,
                        src,
                        *match_start,
                        prefix_len + ip + 1,
                        MIN_MATCH,
                    )
                })
        {
            let rep_length =
                prefixed_match_length_at(prefix, src, rep_match_start, ip + 1, MIN_MATCH);
            store_lazy_sequence_with_source(
                plan,
                src,
                &mut anchor,
                &mut repeat_offsets,
                ip + 1,
                rep_offsets.0,
                rep_length,
                SequenceTraceMatchSource::Rep,
            )?;
            ip = anchor;
        } else {
            let source_long_candidate = (src_long_candidate != NO_POS as usize
                && src_long_candidate >= source_low
                && src_long_candidate < ip)
                .then(|| prefix_len + src_long_candidate);
            let mut found = None;
            let mut found_source = SequenceTraceMatchSource::Unknown;

            let source_long_match = source_long_candidate
                .filter(|candidate_index| {
                    match_prefix_8bytes_logical(prefix, src, *candidate_index, prefix_len + ip)
                })
                .map(|candidate_index| {
                    (
                        extend_back_logical_match_with_min_start(
                            prefix,
                            src,
                            anchor,
                            DoubleFastMatch {
                                start: ip,
                                offset: prefix_len + ip - candidate_index,
                                length: 8 + count_match_length_with_prefix(
                                    prefix,
                                    src,
                                    candidate_index + 8,
                                    prefix_len + ip + 8,
                                ),
                            },
                            prefix_len + source_low,
                        ),
                        SequenceTraceMatchSource::Source,
                    )
                });
            let dict_long_match = dict_long_candidate
                .filter(|candidate| *candidate >= prefix_low)
                .filter(|candidate_index| {
                    match_prefix_8bytes_logical(prefix, src, *candidate_index, prefix_len + ip)
                })
                .map(|candidate_index| {
                    (
                        extend_back_logical_match_with_min_start(
                            prefix,
                            src,
                            anchor,
                            DoubleFastMatch {
                                start: ip,
                                offset: prefix_len + ip - candidate_index,
                                length: 8 + count_match_length_with_prefix(
                                    prefix,
                                    src,
                                    candidate_index + 8,
                                    prefix_len + ip + 8,
                                ),
                            },
                            prefix_low,
                        ),
                        SequenceTraceMatchSource::Dict,
                    )
                });

            // Match C's ZSTD_compressBlock_doubleFast_dictMatchState_generic
            // ordering (zstd_double_fast.c:416-433): prefer the source-long
            // match unconditionally if it matches, and only fall back to the
            // dict-long match if the source slot doesn't match. Picking the
            // "longer of source/dict" instead lets Rust take a long dict
            // match at a far offset when a shorter, closer source match was
            // already available — that is cheaper to encode in zstd.
            if let Some((long_match, long_source)) = source_long_match.or(dict_long_match) {
                found = Some(long_match);
                found_source = long_source;
            } else {
                let source_short_candidate = (src_short_entry != NO_TAGGED_ENTRY
                    && src_short_candidate >= source_low
                    && src_short_candidate < ip)
                    .then(|| prefix_len + src_short_candidate);
                let short_match = if let Some(candidate_index) =
                    source_short_candidate.filter(|candidate_index| {
                        match_prefix_4bytes_logical(prefix, src, *candidate_index, prefix_len + ip)
                    }) {
                    Some((candidate_index, SequenceTraceMatchSource::Source))
                } else {
                    dict_short_candidate
                        .filter(|candidate| *candidate >= prefix_low)
                        .filter(|candidate_index| {
                            match_prefix_4bytes_logical(
                                prefix,
                                src,
                                *candidate_index,
                                prefix_len + ip,
                            )
                        })
                        .map(|candidate_index| (candidate_index, SequenceTraceMatchSource::Dict))
                };

                if let Some((short_candidate_index, short_source)) = short_match {
                    let short_length = MIN_MATCH
                        + count_match_length_with_prefix(
                            prefix,
                            src,
                            short_candidate_index + MIN_MATCH,
                            prefix_len + ip + MIN_MATCH,
                        );
                    let next_long_hash = hash_long_at(src, ip + 1, src_finder.long_hash_bits);
                    let next_src_long_candidate =
                        long_entry_pos(unsafe { src_finder.get_long_entry(next_long_hash) })
                            as usize;
                    let next_source_long = (next_src_long_candidate != NO_POS as usize
                        && next_src_long_candidate >= source_low
                        && next_src_long_candidate < ip + 1)
                        .then(|| prefix_len + next_src_long_candidate);
                    let next_dict_long_raw = prepared.long_candidate_at_fast(src, ip + 1);
                    let next_dict_long: Option<usize> =
                        (next_dict_long_raw != u32::MAX).then_some(next_dict_long_raw as usize);
                    unsafe { src_finder.set_long_head(next_long_hash, (current + 1) as u32) };

                    let next_source_long_match = next_source_long
                        .filter(|candidate_index| {
                            match_prefix_8bytes_logical(
                                prefix,
                                src,
                                *candidate_index,
                                prefix_len + ip + 1,
                            )
                        })
                        .map(|next_candidate_index| {
                            (
                                extend_back_logical_match_with_min_start(
                                    prefix,
                                    src,
                                    anchor,
                                    DoubleFastMatch {
                                        start: ip + 1,
                                        offset: prefix_len + ip + 1 - next_candidate_index,
                                        length: 8 + count_match_length_with_prefix(
                                            prefix,
                                            src,
                                            next_candidate_index + 8,
                                            prefix_len + ip + 1 + 8,
                                        ),
                                    },
                                    prefix_len + source_low,
                                ),
                                SequenceTraceMatchSource::Source,
                            )
                        });
                    let next_dict_long_match = next_dict_long
                        .filter(|candidate| *candidate >= prefix_low)
                        .filter(|candidate_index| {
                            match_prefix_8bytes_logical(
                                prefix,
                                src,
                                *candidate_index,
                                prefix_len + ip + 1,
                            )
                        })
                        .map(|next_candidate_index| {
                            (
                                extend_back_logical_match_with_min_start(
                                    prefix,
                                    src,
                                    anchor,
                                    DoubleFastMatch {
                                        start: ip + 1,
                                        offset: prefix_len + ip + 1 - next_candidate_index,
                                        length: 8 + count_match_length_with_prefix(
                                            prefix,
                                            src,
                                            next_candidate_index + 8,
                                            prefix_len + ip + 1 + 8,
                                        ),
                                    },
                                    prefix_low,
                                ),
                                SequenceTraceMatchSource::Dict,
                            )
                        });

                    if let Some((next_long_match, next_long_source)) =
                        match (next_source_long_match, next_dict_long_match) {
                            (Some(source_match), Some(dict_match)) => {
                                if dict_match.0.length > source_match.0.length {
                                    Some(dict_match)
                                } else {
                                    Some(source_match)
                                }
                            }
                            (Some(source_match), None) => Some(source_match),
                            (None, Some(dict_match)) => Some(dict_match),
                            (None, None) => None,
                        }
                    {
                        if next_long_match.length > short_length {
                            found = Some(next_long_match);
                            found_source = next_long_source;
                        }
                    }

                    // The ladder stops at `ip+1`. C's `_search_next_long`
                    // (`zstd_double_fast.c:456`) looks one position past the
                    // short match and no further, and a `ip+2` rung once lived
                    // here for raw-content dictionaries. It cost up to 1.14x
                    // upstream's size: a long match two bytes ahead abandons
                    // the short match's two leading bytes to literals and
                    // spends a fresh offset code to do it, so it has to be much
                    // longer to pay, and length alone does not know that.
                    if found.is_none() {
                        found = Some(extend_back_logical_match_with_min_start(
                            prefix,
                            src,
                            anchor,
                            DoubleFastMatch {
                                start: ip,
                                offset: prefix_len + ip - short_candidate_index,
                                length: short_length,
                            },
                            if short_source == SequenceTraceMatchSource::Source {
                                prefix_len + source_low
                            } else {
                                prefix_low
                            },
                        ));
                        found_source = short_source;
                    }
                }
            }

            let Some(found) = found else {
                ip = ip.saturating_add(
                    ((ip.saturating_sub(anchor)) >> params.skip_search_strength) + 1,
                );
                continue;
            };

            store_lazy_regular_sequence_with_source(
                plan,
                src,
                &mut anchor,
                &mut repeat_offsets,
                found.start,
                found.offset,
                found.length,
                found_source,
            )?;
            ip = anchor;
        }

        rep_offsets = repeat_offsets12(repeat_offsets);
        if ip <= search_limit {
            insert_double_fast_match_positions(src_finder, src, current, ip);
            while let Some(rep_match_start) =
                prefixed_offset_match_start(prefix_len, ip, rep_offsets.1, prefix_low, source_low)
            {
                if !logical_match_has_length(
                    prefix,
                    src,
                    rep_match_start,
                    prefix_len + ip,
                    MIN_MATCH,
                ) {
                    break;
                }
                let rep_ip = ip;
                let rep_length =
                    prefixed_match_length_at(prefix, src, rep_match_start, rep_ip, MIN_MATCH);
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
                ip = anchor;
                rep_offsets = repeat_offsets12(repeat_offsets);
                if ip > search_limit {
                    break;
                }
            }
        }
    }

    plan.literals.extend_from_slice(&src[anchor..]);
    plan.repeat_offsets = repeat_offsets;
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn search_double_fast_without_prefix(
    src: &[u8],
    pos: usize,
    anchor: usize,
    repeat_offsets: [u32; 3],
    finder: &mut DoubleFastFinder,
    window_low: usize,
) -> Option<DoubleFastMatch> {
    if pos + 8 > src.len() {
        return None;
    }

    let sht = hash_short_cache_src_at_mls(src, pos, finder.short_hash_bits, finder.min_match);
    let short_entry = finder.short_heads[tagged_index(sht)];
    let short_candidate = tagged_pos(short_entry);
    finder.short_heads[tagged_index(sht)] = tagged_entry(pos, sht);

    let long_candidate = if pos + 8 <= src.len() {
        let long_hash = hash_long_at(src, pos, finder.long_hash_bits);
        let candidate = long_entry_pos(finder.long_entries[tagged_index(long_hash)]) as usize;
        finder.file_long_entry(long_hash, pos as u32);
        Some(candidate)
    } else {
        None
    };

    if let Some(length) =
        repeat_match_length_without_prefix(src, pos, repeat_offsets[0] as usize, window_low)
    {
        return Some(DoubleFastMatch {
            start: pos + 1,
            offset: repeat_offsets[0] as usize,
            length,
        });
    }

    if let Some(candidate) = long_candidate.filter(|candidate| {
        *candidate != NO_POS as usize && *candidate >= window_low && *candidate < pos
    }) {
        #[allow(unsafe_code)]
        let length = unsafe { count_match_length_unchecked(src, candidate, pos) };
        if length >= 8 {
            return Some(extend_back_source_match(
                src,
                anchor,
                DoubleFastMatch {
                    start: pos,
                    offset: pos - candidate,
                    length,
                },
            ));
        }
    }

    let short_match =
        if short_entry != NO_TAGGED_ENTRY && short_candidate >= window_low && short_candidate < pos
        {
            #[allow(unsafe_code)]
            let length = unsafe { count_match_length_unchecked(src, short_candidate, pos) };
            (length >= MIN_MATCH).then(|| DoubleFastMatch {
                start: pos,
                offset: pos - short_candidate,
                length,
            })
        } else {
            None
        };

    let next_long_match = if pos + 1 + 8 <= src.len() {
        let long_hash = hash_long_at(src, pos + 1, finder.long_hash_bits);
        let candidate = long_entry_pos(finder.long_entries[tagged_index(long_hash)]) as usize;
        finder.file_long_entry(long_hash, (pos + 1) as u32);
        if candidate != NO_POS as usize && candidate >= window_low && candidate < pos + 1 {
            #[allow(unsafe_code)]
            let length = unsafe { count_match_length_unchecked(src, candidate, pos + 1) };
            (length >= 8).then(|| DoubleFastMatch {
                start: pos + 1,
                offset: pos + 1 - candidate,
                length,
            })
        } else {
            None
        }
    } else {
        None
    };

    if let Some(found) = next_long_match {
        return Some(extend_back_source_match(src, anchor, found));
    }

    short_match.map(|found| extend_back_source_match(src, anchor, found))
}

#[allow(dead_code)]
pub(crate) fn search_double_fast_with_prefix(
    prefix: &[u8],
    src: &[u8],
    pos: usize,
    anchor: usize,
    repeat_offsets: [u32; 3],
    prefix_finder: &DoubleFastFinder,
    src_finder: &mut DoubleFastFinder,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
) -> Option<DoubleFastMatch> {
    if pos + 8 > src.len() {
        return None;
    }

    let sht =
        hash_short_cache_src_at_mls(src, pos, src_finder.short_hash_bits, src_finder.min_match);
    let src_short_entry = src_finder.short_heads[tagged_index(sht)];
    let src_short_candidate = tagged_pos(src_short_entry);
    let prefix_short_entry = prefix_finder.short_heads[tagged_index(sht)];
    let prefix_short_candidate = tagged_pos(prefix_short_entry);
    src_finder.short_heads[tagged_index(sht)] = tagged_entry(pos, sht);

    let (src_long_candidate, prefix_long_candidate) = if pos + 8 <= src.len() {
        let long_hash = hash_long_at(src, pos, src_finder.long_hash_bits);
        let src_candidate =
            long_entry_pos(src_finder.long_entries[tagged_index(long_hash)]) as usize;
        let prefix_candidate =
            long_entry_pos(prefix_finder.long_entries[tagged_index(long_hash)]) as usize;
        src_finder.file_long_entry(long_hash, pos as u32);
        (Some(src_candidate), Some(prefix_candidate))
    } else {
        (None, None)
    };

    if let Some(length) = repeat_match_length_with_prefix(
        prefix,
        src,
        pos,
        repeat_offsets[0] as usize,
        prefix_low,
        source_low,
    ) {
        return Some(DoubleFastMatch {
            start: pos + 1,
            offset: repeat_offsets[0] as usize,
            length,
        });
    }

    let literal_length = pos.saturating_sub(anchor);
    let src_long_match = src_long_candidate
        .filter(|candidate| {
            *candidate != NO_POS as usize && *candidate >= source_low && *candidate < pos
        })
        .and_then(|candidate| {
            #[allow(unsafe_code)]
            let length = unsafe { count_match_length_unchecked(src, candidate, pos) };
            (length >= 8).then(|| MatchCandidate {
                offset: pos - candidate,
                length,
            })
        });
    let prefix_long_match = prefix_long_candidate
        .filter(|candidate| {
            *candidate != NO_POS as usize
                && *candidate >= prefix_low
                && *candidate + 8 <= prefix.len()
        })
        .filter(|candidate| {
            logical_match_has_length(prefix, src, *candidate, prefix.len() + pos, 8)
        })
        .map(|candidate| MatchCandidate {
            offset: prefix.len() + pos - candidate,
            length: count_match_length_with_prefix(prefix, src, candidate, prefix.len() + pos),
        });
    if let Some(best) = choose_better_regular_match_with_adjustment(
        prefix_long_match,
        src_long_match,
        literal_length,
        -params.source_score_penalty_with_prefix,
    ) {
        return Some(extend_back_logical_match(
            prefix,
            src,
            anchor,
            DoubleFastMatch {
                start: pos,
                offset: best.offset,
                length: best.length,
            },
        ));
    }

    let src_short_match = if src_short_entry != NO_TAGGED_ENTRY
        && src_short_candidate >= source_low
        && src_short_candidate < pos
    {
        #[allow(unsafe_code)]
        let length = unsafe { count_match_length_unchecked(src, src_short_candidate, pos) };
        (length >= MIN_MATCH).then(|| MatchCandidate {
            offset: pos - src_short_candidate,
            length,
        })
    } else {
        None
    };

    let prefix_short_match = (prefix_short_entry != NO_TAGGED_ENTRY
        && prefix_short_candidate >= prefix_low
        && prefix_short_candidate + MIN_MATCH <= prefix.len()
        && logical_match_has_length(
            prefix,
            src,
            prefix_short_candidate,
            prefix.len() + pos,
            MIN_MATCH,
        ))
    .then(|| MatchCandidate {
        offset: prefix.len() + pos - prefix_short_candidate,
        length: count_match_length_with_prefix(
            prefix,
            src,
            prefix_short_candidate,
            prefix.len() + pos,
        ),
    });

    let next_long_match = if pos + 1 + 8 <= src.len() {
        let long_hash = hash_long_at(src, pos + 1, src_finder.long_hash_bits);
        let src_candidate =
            long_entry_pos(src_finder.long_entries[tagged_index(long_hash)]) as usize;
        let prefix_candidate =
            long_entry_pos(prefix_finder.long_entries[tagged_index(long_hash)]) as usize;
        src_finder.file_long_entry(long_hash, (pos + 1) as u32);

        let src_next_long_match = if src_candidate != NO_POS as usize
            && src_candidate >= source_low
            && src_candidate < pos + 1
        {
            #[allow(unsafe_code)]
            let length = unsafe { count_match_length_unchecked(src, src_candidate, pos + 1) };
            (length >= 8).then(|| MatchCandidate {
                offset: pos + 1 - src_candidate,
                length,
            })
        } else {
            None
        };
        let prefix_next_long_match = (prefix_candidate != NO_POS as usize
            && prefix_candidate >= prefix_low
            && prefix_candidate + 8 <= prefix.len()
            && logical_match_has_length(prefix, src, prefix_candidate, prefix.len() + pos + 1, 8))
        .then(|| MatchCandidate {
            offset: prefix.len() + pos + 1 - prefix_candidate,
            length: count_match_length_with_prefix(
                prefix,
                src,
                prefix_candidate,
                prefix.len() + pos + 1,
            ),
        });
        choose_better_regular_match_with_adjustment(
            prefix_next_long_match,
            src_next_long_match,
            pos + 1 - anchor,
            -params.source_score_penalty_with_prefix,
        )
        .map(|best| DoubleFastMatch {
            start: pos + 1,
            offset: best.offset,
            length: best.length,
        })
    } else {
        None
    };

    if let Some(found) = next_long_match {
        return Some(extend_back_logical_match(prefix, src, anchor, found));
    }

    choose_better_regular_match_with_adjustment(
        prefix_short_match,
        src_short_match,
        literal_length,
        -params.source_score_penalty_with_prefix,
    )
    .map(|best| {
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
    })
}
