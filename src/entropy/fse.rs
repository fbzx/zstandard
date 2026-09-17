use crate::{
    entropy::{
        bitstream::{BitCStream, BitDStream, BitDStreamStatus, CONTAINER_BITS, CONTAINER_BYTES},
        mem::highbit32,
    },
    error::{Error, Result},
};

pub(crate) const TABLELOG_MAX: usize = 9;
pub(crate) const SYMBOLVALUE_MAX: usize = 255;
const MIN_TABLELOG: usize = 5;
const TABLESIZE_MAX: usize = 1 << TABLELOG_MAX;
const RTB_TABLE: [u32; 8] = [
    0, 473_195, 504_333, 520_860, 550_000, 700_000, 750_000, 830_000,
];

#[derive(Clone, Copy, Default)]
pub(crate) struct DecodeEntry {
    pub(crate) new_state: u16,
    pub(crate) symbol: u8,
    pub(crate) nb_bits: u8,
}

#[derive(Clone)]
pub(crate) struct DTable {
    table_log: u16,
    fast_mode: bool,
    entries: [DecodeEntry; TABLESIZE_MAX],
}

impl DTable {
    /// Average symbol value across all table entries. For offset tables, the
    /// symbol is the offset code, so this estimates the average offset magnitude.
    pub(crate) fn avg_symbol(&self) -> u32 {
        let table_size = if self.table_log == 0 {
            1
        } else {
            1usize << self.table_log
        };
        let mut total = 0u64;
        for i in 0..table_size {
            total += self.entries[i].symbol as u64;
        }
        (total / table_size as u64) as u32
    }

    pub(crate) fn table_log(&self) -> u16 {
        self.table_log
    }

    /// Copy only the active entries from `src`, avoiding a full 2KB clone.
    pub(crate) fn copy_active_from(&mut self, src: &DTable, table_size: usize) {
        self.table_log = src.table_log;
        self.fast_mode = src.fast_mode;
        self.entries[..table_size].copy_from_slice(&src.entries[..table_size]);
    }
}

impl Default for DTable {
    fn default() -> Self {
        Self {
            table_log: 0,
            fast_mode: false,
            entries: [DecodeEntry::default(); TABLESIZE_MAX],
        }
    }
}

/// FSE decode entry with embedded baseline and extra-bits count.
/// Matches C zstd's `ZSTD_seqSymbol` — avoids secondary table lookups per sequence.
/// `repr(C)` guarantees the field layout so `get_entry_raw` can use u64 loads
/// with bit-field extraction: [new_state:16][nb_bits:8][nb_add:8][baseline:32].
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub(crate) struct SequenceDecodeEntry {
    pub(crate) new_state: u16,
    pub(crate) nb_bits: u8,
    pub(crate) nb_additional_bits: u8,
    pub(crate) baseline: u32,
}

/// Sequence-specialized decode table with embedded baselines and extra-bits.
#[derive(Clone)]
#[repr(align(64))]
pub(crate) struct SequenceDTable {
    pub(crate) table_log: u16,
    pub(crate) entries: [SequenceDecodeEntry; TABLESIZE_MAX],
}

impl Default for SequenceDTable {
    fn default() -> Self {
        Self {
            table_log: 0,
            entries: [SequenceDecodeEntry::default(); TABLESIZE_MAX],
        }
    }
}

impl SequenceDTable {
    /// Unchecked entry access.
    ///
    /// # Safety
    ///
    /// `state` must be less than `TABLESIZE_MAX`, which FSE table construction
    /// is what guarantees.
    #[inline(always)]
    #[allow(unsafe_code)]
    pub(crate) unsafe fn get_entry_unchecked(&self, state: usize) -> SequenceDecodeEntry {
        debug_assert!(state < TABLESIZE_MAX);
        // SAFETY: `state < TABLESIZE_MAX` by contract, and `entries` is that
        // long, so the index is in bounds.
        unsafe { *self.entries.get_unchecked(state) }
    }

    /// Load entry as a raw u64. Forces LLVM to emit a single 8-byte load
    /// instead of splitting into multiple smaller loads (ldrh/ldrb/ldr w).
    /// Fields are packed little-endian: [new_state:16][nb_bits:8][nb_add:8][baseline:32].
    ///
    /// # Safety
    ///
    /// `state` must be less than `TABLESIZE_MAX`.
    #[inline(always)]
    #[allow(unsafe_code)]
    pub(crate) unsafe fn get_entry_raw(&self, state: usize) -> u64 {
        debug_assert!(state < TABLESIZE_MAX);
        // SAFETY: `state < TABLESIZE_MAX` by contract, so the offset stays
        // inside `entries`. Each entry is 8 bytes, so the word read covers
        // exactly the one entry. Unaligned, so no alignment requirement.
        let ptr = unsafe { self.entries.as_ptr().add(state) as *const u64 };
        unsafe { core::ptr::read_unaligned(ptr) }
    }
}

/// Extract `nb_additional_bits` (offset 24..32) from a raw u64 entry.
#[inline(always)]
pub(crate) const fn raw_entry_nb_additional_bits(raw: u64) -> u32 {
    ((raw >> 24) & 0xFF) as u32
}

/// Extract `nb_bits` (offset 16..24) from a raw u64 entry.
#[inline(always)]
pub(crate) const fn raw_entry_nb_bits(raw: u64) -> u32 {
    ((raw >> 16) & 0xFF) as u32
}

/// Extract `new_state` (offset 0..16) from a raw u64 entry.
#[inline(always)]
pub(crate) const fn raw_entry_new_state(raw: u64) -> usize {
    (raw & 0xFFFF) as usize
}

/// Extract `baseline` (offset 32..64) from a raw u64 entry.
#[inline(always)]
pub(crate) const fn raw_entry_baseline(raw: u64) -> u32 {
    (raw >> 32) as u32
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DState {
    pub(crate) state: usize,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct SymbolCompressionTransform {
    pub(crate) delta_find_state: i32,
    pub(crate) delta_nb_bits: u32,
}

#[derive(Clone)]
pub(crate) struct CTable {
    table_log: u16,
    max_symbol_value: u16,
    state_table: [u16; TABLESIZE_MAX],
    symbol_tt: [SymbolCompressionTransform; SYMBOLVALUE_MAX + 1],
}

impl Default for CTable {
    fn default() -> Self {
        Self {
            table_log: 0,
            max_symbol_value: 0,
            state_table: [0; TABLESIZE_MAX],
            symbol_tt: [SymbolCompressionTransform::default(); SYMBOLVALUE_MAX + 1],
        }
    }
}

impl CTable {
    /// Maximum number of bits to encode `symbol`.
    /// Matches C's `FSE_getMaxNbBits` — ceiling division of deltaNbBits by 2^16.
    /// C: `(symbolTT[symbolValue].deltaNbBits + ((1<<16)-1)) >> 16`
    #[inline(always)]
    pub(crate) fn max_nb_bits(&self, symbol: usize) -> u32 {
        (self.symbol_tt[symbol]
            .delta_nb_bits
            .wrapping_add((1 << 16) - 1))
            >> 16
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CState {
    value: u32,
}

#[inline(always)]
const fn table_step(table_size: usize) -> usize {
    (table_size >> 1) + (table_size >> 3) + 3
}

/// The narrowest table log that can hold every symbol the counts can contain.
///
/// The `min` is what stops a wide alphabet from demanding a wide table on a
/// short input: at most `src_size` distinct symbols can actually occur, so
/// `min_bits_src` bounds what the alphabet is entitled to ask for. Either term
/// alone guarantees `1 << result` is at least the number of distinct symbols,
/// which is the property [`normalize_count`] relies on.
fn min_table_log(src_size: usize, max_symbol_value: u32) -> u32 {
    let min_bits_src = highbit32(src_size.max(2) as u32) + 1;
    let min_bits_symbols = if max_symbol_value == 0 {
        1
    } else {
        highbit32(max_symbol_value) + 2
    };
    min_bits_src.min(min_bits_symbols)
}

/// One closed-form guess, deliberately, where `huff0` searches.
///
/// [`huff0::TableDepth::Searched`](crate::entropy::huff0) builds the literals
/// tree at every candidate depth and keeps the smallest header-plus-payload
/// total. Giving the sequence tables the same treatment was measured on
/// 2026-09-17 and rejected: a brute-force argmin over every legal table log,
/// costed on true `write_ncount` bytes plus exact payload bits, saves **3,988
/// bytes of 107,278,123** across 9 corpora at 14 levels. That is 0.0037%, and
/// it is the *oracle* — a perfect chooser at zero search cost.
///
/// The shape of the win is why no implementation recovers it. The median is 4
/// bits; the top 100 of ~4,100 suboptimal sections are 17% of the total; 4 of
/// 9 corpora save nothing; 56% of sections already land on the optimum. It is
/// also not separable from mode selection — a greedy per-section argmin
/// *regressed* a row even as an oracle, because the table log feeds the
/// compressed-vs-repeat-vs-basic choice downstream.
///
/// Two things a future search must know. Sections of 63 sequences or fewer
/// have no room in either direction (108 of 108 already optimal), because
/// `max_bits_src` is small there and this function returns [`min_table_log`]
/// itself — the chosen log *is* the floor. And the error is not one-sided:
/// 1,891 sections want a lower log but 2,346 want a *higher* one, so
/// `max_bits_src` withholds accuracy more often than the cap over-constrains.
///
/// Do not build the search without re-measuring that ceiling.
pub(crate) fn optimal_table_log(max_table_log: u32, src_size: usize, max_symbol_value: u32) -> u32 {
    let src_size = src_size.max(2);
    let max_table_log = max_table_log.clamp(MIN_TABLELOG as u32, TABLELOG_MAX as u32);
    let max_bits_src = highbit32((src_size - 1) as u32).saturating_sub(2);
    let min_bits = min_table_log(src_size, max_symbol_value);

    let mut table_log = max_table_log;
    if max_bits_src < table_log {
        table_log = max_bits_src.max(MIN_TABLELOG as u32);
    }
    if min_bits > table_log {
        table_log = min_bits.min(max_table_log);
    }
    table_log.clamp(MIN_TABLELOG as u32, max_table_log)
}

pub(crate) fn normalize_count(
    normalized_counter: &mut [i16],
    table_log: u32,
    count: &[u32],
    total: usize,
    max_symbol_value: u32,
    use_low_prob_count: bool,
) -> Result<u32> {
    let max_symbol_index =
        usize::try_from(max_symbol_value).map_err(|_| Error::MaxSymbolValueTooLarge)?;
    if max_symbol_index > SYMBOLVALUE_MAX
        || max_symbol_index >= normalized_counter.len()
        || max_symbol_index >= count.len()
    {
        return Err(Error::MaxSymbolValueTooLarge);
    }
    if total < 2 {
        return Err(Error::InvalidParameter(
            "FSE normalization requires at least two symbols",
        ));
    }
    if !(MIN_TABLELOG as u32..=TABLELOG_MAX as u32).contains(&table_log) {
        return Err(Error::TableLogTooLarge);
    }
    // C's guard at `fse_compress.c:473`, which this crate had omitted. A table
    // log below the floor reaches `normalize_count_m2`, where `(1 << table_log)
    // - distributed` subtracts below zero as soon as the distinct symbols
    // outnumber the table's slots: a panic in debug, a wrapped count in
    // release. No caller reaches it today, because every caller's accuracy cap
    // exceeds the `min_table_log` its inputs can produce -- but that is an
    // assumption holding the invariant up rather than a check, and it is a
    // tuning change away from being load-bearing.
    if table_log < min_table_log(total, max_symbol_value) {
        return Err(Error::Generic);
    }
    let low_prob_count = if use_low_prob_count { -1 } else { 1 };
    let scale = 62 - table_log;
    let step = (1u64 << 62) / total as u64;
    let v_step = 1u64 << (scale - 20);
    let mut still_to_distribute = 1i32 << table_log;
    let mut largest = 0usize;
    let mut largest_p = 0i16;
    let low_threshold = (total >> table_log) as u32;

    for symbol in 0..=max_symbol_index {
        match count[symbol] {
            0 => normalized_counter[symbol] = 0,
            value if value as usize == total => return Ok(0),
            value if value <= low_threshold => {
                normalized_counter[symbol] = low_prob_count;
                still_to_distribute -= 1;
            }
            value => {
                let mut proba = ((u64::from(value) * step) >> scale) as i16;
                if proba < 8 {
                    let rest_to_beat = v_step * u64::from(RTB_TABLE[proba as usize]);
                    let current = u64::from(value) * step;
                    proba += (current - ((u64::from(proba as u16)) << scale) > rest_to_beat) as i16;
                }
                if proba > largest_p {
                    largest_p = proba;
                    largest = symbol;
                }
                normalized_counter[symbol] = proba;
                still_to_distribute -= i32::from(proba);
            }
        }
    }

    if -still_to_distribute >= (i32::from(normalized_counter[largest]) >> 1) {
        normalize_count_m2(
            normalized_counter,
            table_log,
            count,
            total,
            max_symbol_index,
            low_prob_count,
        )?;
    } else {
        normalized_counter[largest] += still_to_distribute as i16;
    }

    Ok(table_log)
}

/// Largest value [`ncount_write_bound`] can return.
///
/// `TABLELOG_MAX` and `SYMBOLVALUE_MAX` cap both inputs, so the bound is a
/// constant and the header can live on the stack. Callers that size a buffer
/// per call otherwise pay an allocation for tens of bytes.
pub(crate) const NCOUNT_WRITE_BOUND_MAX: usize =
    (((SYMBOLVALUE_MAX + 1) * TABLELOG_MAX + 4 + 2) / 8) + 1 + 2;

pub(crate) fn ncount_write_bound(max_symbol_value: u32, table_log: u32) -> Result<usize> {
    let max_symbol_index =
        usize::try_from(max_symbol_value).map_err(|_| Error::MaxSymbolValueTooLarge)?;
    if max_symbol_index > SYMBOLVALUE_MAX {
        return Err(Error::MaxSymbolValueTooLarge);
    }
    if !(MIN_TABLELOG as u32..=TABLELOG_MAX as u32).contains(&table_log) {
        return Err(Error::TableLogTooLarge);
    }

    Ok((((max_symbol_index + 1) * table_log as usize + 4 + 2) / 8) + 1 + 2)
}

pub(crate) fn write_ncount(
    dst: &mut [u8],
    normalized_counter: &[i16],
    max_symbol_value: u32,
    table_log: u32,
) -> Result<usize> {
    let max_symbol_index =
        usize::try_from(max_symbol_value).map_err(|_| Error::MaxSymbolValueTooLarge)?;
    if max_symbol_index > SYMBOLVALUE_MAX || max_symbol_index >= normalized_counter.len() {
        return Err(Error::MaxSymbolValueTooLarge);
    }
    if !(MIN_TABLELOG as u32..=TABLELOG_MAX as u32).contains(&table_log) {
        return Err(Error::TableLogTooLarge);
    }

    let table_size = 1i32 << table_log;
    let mut remaining = table_size + 1;
    let mut threshold = table_size;
    let mut nb_bits = table_log as i32 + 1;
    let mut bit_stream = table_log - MIN_TABLELOG as u32;
    let mut bit_count = 4usize;
    let mut symbol = 0usize;
    let alphabet_size = max_symbol_index + 1;
    let mut previous_is_zero = false;
    let mut out = 0usize;

    while symbol < alphabet_size && remaining > 1 {
        if previous_is_zero {
            let start = symbol;
            while symbol < alphabet_size && normalized_counter[symbol] == 0 {
                symbol += 1;
            }
            if symbol == alphabet_size {
                break;
            }

            let mut skipped = symbol - start;
            while skipped >= 24 {
                skipped -= 24;
                bit_stream = bit_stream.wrapping_add(0xFFFFu32 << bit_count);
                flush_two_bytes(dst, &mut out, &mut bit_stream)?;
            }
            while skipped >= 3 {
                skipped -= 3;
                bit_stream = bit_stream.wrapping_add(3u32 << bit_count);
                bit_count += 2;
            }
            bit_stream = bit_stream.wrapping_add((skipped as u32) << bit_count);
            bit_count += 2;
            if bit_count > 16 {
                flush_two_bytes(dst, &mut out, &mut bit_stream)?;
                bit_count -= 16;
            }
        }

        let mut count = i32::from(normalized_counter[symbol]);
        symbol += 1;

        let max = (2 * threshold - 1) - remaining;
        remaining -= if count < 0 { -count } else { count };
        count += 1;
        if count >= threshold {
            count += max;
        }
        bit_stream = bit_stream.wrapping_add((count as u32) << bit_count);
        bit_count += nb_bits as usize;
        if count < max {
            bit_count -= 1;
        }
        previous_is_zero = count == 1;
        if remaining < 1 {
            return Err(Error::Generic);
        }
        while remaining < threshold {
            nb_bits -= 1;
            threshold >>= 1;
        }
        if bit_count > 16 {
            flush_two_bytes(dst, &mut out, &mut bit_stream)?;
            bit_count -= 16;
        }
    }

    if remaining != 1 {
        return Err(Error::Generic);
    }
    if out + 2 > dst.len() {
        return Err(Error::DstSizeTooSmall);
    }
    dst[out] = bit_stream as u8;
    dst[out + 1] = (bit_stream >> 8) as u8;
    out += bit_count.div_ceil(8);
    Ok(out)
}

fn normalize_count_m2(
    normalized_counter: &mut [i16],
    table_log: u32,
    count: &[u32],
    mut total: usize,
    max_symbol_index: usize,
    low_prob_count: i16,
) -> Result<()> {
    const NOT_YET_ASSIGNED: i16 = -2;

    let mut distributed = 0u32;
    let low_threshold = (total >> table_log) as u32;
    let mut low_one = ((total * 3) >> (table_log + 1)) as u32;

    for symbol in 0..=max_symbol_index {
        if count[symbol] == 0 {
            normalized_counter[symbol] = 0;
            continue;
        }
        if count[symbol] <= low_threshold {
            normalized_counter[symbol] = low_prob_count;
            distributed += 1;
            total -= count[symbol] as usize;
            continue;
        }
        if count[symbol] <= low_one {
            normalized_counter[symbol] = 1;
            distributed += 1;
            total -= count[symbol] as usize;
            continue;
        }
        normalized_counter[symbol] = NOT_YET_ASSIGNED;
    }

    let mut to_distribute = (1u32 << table_log) - distributed;
    if to_distribute == 0 {
        return Ok(());
    }

    if (total / to_distribute as usize) as u32 > low_one {
        low_one = ((total * 3) / (to_distribute as usize * 2)) as u32;
        for symbol in 0..=max_symbol_index {
            if normalized_counter[symbol] == NOT_YET_ASSIGNED && count[symbol] <= low_one {
                normalized_counter[symbol] = 1;
                distributed += 1;
                total -= count[symbol] as usize;
            }
        }
        to_distribute = (1u32 << table_log) - distributed;
    }

    if distributed == max_symbol_index as u32 + 1 {
        let mut max_symbol = 0usize;
        let mut max_count = 0u32;
        for symbol in 0..=max_symbol_index {
            if count[symbol] > max_count {
                max_symbol = symbol;
                max_count = count[symbol];
            }
        }
        normalized_counter[max_symbol] += to_distribute as i16;
        return Ok(());
    }

    if total == 0 {
        let mut symbol = 0usize;
        while to_distribute > 0 {
            if normalized_counter[symbol] > 0 {
                normalized_counter[symbol] += 1;
                to_distribute -= 1;
            }
            symbol = (symbol + 1) % (max_symbol_index + 1);
        }
        return Ok(());
    }

    let v_step_log = 62 - table_log;
    let mid = (1u64 << (v_step_log - 1)) - 1;
    let r_step = (((1u64 << v_step_log) * u64::from(to_distribute)) + mid) / total as u64;
    let mut tmp_total = mid;
    for symbol in 0..=max_symbol_index {
        if normalized_counter[symbol] == NOT_YET_ASSIGNED {
            let end = tmp_total + (u64::from(count[symbol]) * r_step);
            let symbol_start = (tmp_total >> v_step_log) as u32;
            let symbol_end = (end >> v_step_log) as u32;
            let weight = symbol_end - symbol_start;
            if weight < 1 {
                return Err(Error::Generic);
            }
            normalized_counter[symbol] = weight as i16;
            tmp_total = end;
        }
    }

    Ok(())
}

fn flush_two_bytes(dst: &mut [u8], out: &mut usize, bit_stream: &mut u32) -> Result<()> {
    if *out + 2 > dst.len() {
        return Err(Error::DstSizeTooSmall);
    }
    dst[*out] = *bit_stream as u8;
    dst[*out + 1] = (*bit_stream >> 8) as u8;
    *out += 2;
    *bit_stream >>= 16;
    Ok(())
}

pub(crate) fn read_ncount(
    normalized_counter: &mut [i16],
    max_symbol_value: &mut u32,
    table_log: &mut u32,
    src: &[u8],
    max_table_log: usize,
) -> Result<usize> {
    let max_symbol_index =
        usize::try_from(*max_symbol_value).map_err(|_| Error::MaxSymbolValueTooLarge)?;
    if max_symbol_index > SYMBOLVALUE_MAX || max_symbol_index >= normalized_counter.len() {
        return Err(Error::MaxSymbolValueTooLarge);
    }
    if max_table_log > TABLELOG_MAX {
        return Err(Error::TableLogTooLarge);
    }

    if src.len() < 4 {
        let mut buffer = [0u8; 4];
        buffer[..src.len()].copy_from_slice(src);
        let count_size = read_ncount(
            normalized_counter,
            max_symbol_value,
            table_log,
            &buffer,
            max_table_log,
        )?;
        if count_size > src.len() {
            return Err(Error::Corruption("truncated FSE ncount"));
        }
        return Ok(count_size);
    }

    normalized_counter[..=max_symbol_index].fill(0);
    let mut bit_stream = u32::from_le_bytes(src[..4].try_into().unwrap());
    let mut nb_bits = ((bit_stream & 0xF) as i32) + MIN_TABLELOG as i32;
    if nb_bits > max_table_log as i32 {
        return Err(Error::TableLogTooLarge);
    }
    bit_stream >>= 4;
    let mut bit_count = 4i32;
    *table_log = nb_bits as u32;
    let mut remaining = (1i32 << nb_bits) + 1;
    let mut threshold = 1i32 << nb_bits;
    nb_bits += 1;
    let mut charnum = 0usize;
    let mut previous0 = false;
    let mut ip = 0usize;

    while remaining > 1 && charnum <= max_symbol_index {
        if previous0 {
            let mut n0 = charnum;
            while (bit_stream & 0xFFFF) == 0xFFFF {
                n0 += 24;
                if ip + 6 <= src.len() {
                    ip += 2;
                    bit_stream =
                        u32::from_le_bytes(src[ip..ip + 4].try_into().unwrap()) >> bit_count;
                } else {
                    bit_stream >>= 16;
                    bit_count += 16;
                }
            }
            while (bit_stream & 3) == 3 {
                n0 += 3;
                bit_stream >>= 2;
                bit_count += 2;
            }
            n0 += (bit_stream & 3) as usize;
            bit_count += 2;
            if n0 > max_symbol_index {
                return Err(Error::MaxSymbolValueTooSmall);
            }
            while charnum < n0 {
                normalized_counter[charnum] = 0;
                charnum += 1;
            }
            if can_reload_u32(src.len(), ip, bit_count) {
                ip += (bit_count >> 3) as usize;
                bit_count &= 7;
                bit_stream = u32::from_le_bytes(src[ip..ip + 4].try_into().unwrap()) >> bit_count;
            } else {
                bit_stream >>= 2;
            }
        }

        let max = (2 * threshold - 1) - remaining;
        let mut count;
        if (bit_stream & (threshold as u32 - 1)) < max as u32 {
            count = (bit_stream & (threshold as u32 - 1)) as i32;
            bit_count += nb_bits - 1;
        } else {
            count = (bit_stream & ((2 * threshold - 1) as u32)) as i32;
            if count >= threshold {
                count -= max;
            }
            bit_count += nb_bits;
        }

        count -= 1;
        remaining -= if count < 0 { -count } else { count };
        normalized_counter[charnum] = count as i16;
        charnum += 1;
        previous0 = count == 0;
        while remaining < threshold {
            nb_bits -= 1;
            threshold >>= 1;
        }
        if can_reload_u32(src.len(), ip, bit_count) {
            ip += (bit_count >> 3) as usize;
            bit_count &= 7;
        } else {
            bit_count -= (8 * (src.len().saturating_sub(4).saturating_sub(ip))) as i32;
            ip = src.len() - 4;
        }
        bit_stream = u32::from_le_bytes(src[ip..ip + 4].try_into().unwrap()) >> (bit_count & 31);
    }

    if remaining != 1 || bit_count > 32 {
        return Err(Error::Corruption("invalid FSE ncount"));
    }
    *max_symbol_value = charnum as u32 - 1;
    Ok(ip + ((bit_count + 7) >> 3) as usize)
}

#[inline(always)]
fn can_reload_u32(src_len: usize, ip: usize, bit_count: i32) -> bool {
    let advance = (bit_count >> 3) as usize;
    ip.checked_add(advance)
        .and_then(|next| next.checked_add(4))
        .is_some_and(|end| end <= src_len)
}

pub(crate) fn build_dtable(
    dt: &mut DTable,
    normalized_counter: &[i16],
    max_symbol_value: u32,
    table_log: u32,
) -> Result<()> {
    let max_symbol_index =
        usize::try_from(max_symbol_value).map_err(|_| Error::MaxSymbolValueTooLarge)?;
    if max_symbol_index > SYMBOLVALUE_MAX || max_symbol_index >= normalized_counter.len() {
        return Err(Error::MaxSymbolValueTooLarge);
    }
    if table_log == 0 {
        return Err(Error::Corruption("FSE table log must be non-zero"));
    }
    if table_log as usize > TABLELOG_MAX {
        return Err(Error::TableLogTooLarge);
    }

    let table_size = 1usize << table_log;
    let table_mask = table_size - 1;
    let mut symbol_next = [0u16; SYMBOLVALUE_MAX + 1];
    let mut high_threshold = table_size - 1;

    dt.table_log = table_log as u16;
    dt.fast_mode = true;

    let large_limit = 1i16 << (table_log - 1);
    for symbol in 0..=max_symbol_index {
        if normalized_counter[symbol] == -1 {
            dt.entries[high_threshold].symbol = symbol as u8;
            symbol_next[symbol] = 1;
            high_threshold = high_threshold.saturating_sub(1);
        } else {
            if normalized_counter[symbol] >= large_limit {
                dt.fast_mode = false;
            }
            symbol_next[symbol] = normalized_counter[symbol].max(0) as u16;
        }
    }

    let step = table_step(table_size);
    let mut position = 0usize;
    for symbol in 0..=max_symbol_index {
        for _ in 0..normalized_counter[symbol].max(0) {
            dt.entries[position].symbol = symbol as u8;
            position = (position + step) & table_mask;
            while position > high_threshold {
                position = (position + step) & table_mask;
            }
        }
    }
    if position != 0 {
        return Err(Error::Generic);
    }

    for index in 0..table_size {
        let symbol = dt.entries[index].symbol as usize;
        let next_state = symbol_next[symbol] as u32;
        symbol_next[symbol] = symbol_next[symbol].wrapping_add(1);
        dt.entries[index].nb_bits = (table_log - highbit32(next_state)) as u8;
        dt.entries[index].new_state =
            ((next_state << dt.entries[index].nb_bits) - table_size as u32) as u16;
    }
    Ok(())
}

pub(crate) fn build_rle_dtable(dt: &mut DTable, symbol: u8) {
    dt.table_log = 0;
    dt.fast_mode = false;
    dt.entries[0] = DecodeEntry {
        new_state: 0,
        symbol,
        nb_bits: 0,
    };
}

/// Build an RLE SequenceDTable directly (single entry) with embedded baseline.
pub(crate) fn build_rle_sequence_dtable(
    out: &mut SequenceDTable,
    symbol: u8,
    baselines: &[u32],
    extra_bits: &[u8],
) {
    let sym = symbol as usize;
    out.table_log = 0;
    out.entries[0] = SequenceDecodeEntry {
        new_state: 0,
        nb_bits: 0,
        nb_additional_bits: if sym < extra_bits.len() {
            extra_bits[sym]
        } else {
            0
        },
        baseline: if sym < baselines.len() {
            baselines[sym]
        } else {
            0
        },
    };
}

/// Build an RLE SequenceDTable for offsets directly.
pub(crate) fn build_rle_offset_sequence_dtable(out: &mut SequenceDTable, symbol: u8) {
    out.table_log = 0;
    out.entries[0] = SequenceDecodeEntry {
        new_state: 0,
        nb_bits: 0,
        nb_additional_bits: symbol,
        baseline: 0,
    };
}

/// Build a SequenceDTable from a DTable by embedding baseline and extra-bits per entry.
/// Build a SequenceDTable, writing ONLY the active entries (0..table_size).
/// Avoids zero-filling all 512 entries (4KB) for small tables (e.g., RLE with 1 entry).
/// Unused entries are left uninitialized — safe because `get_entry_unchecked` only
/// accesses states within `0..table_size`, guaranteed by FSE construction.
#[allow(unsafe_code)]
pub(crate) fn build_sequence_dtable_from(
    dt: &DTable,
    baselines: &[u32],
    extra_bits: &[u8],
    out: &mut SequenceDTable,
) {
    let table_size = if dt.table_log == 0 {
        1
    } else {
        1usize << dt.table_log
    };
    out.table_log = dt.table_log;
    for i in 0..table_size {
        let e = &dt.entries[i];
        let sym = e.symbol as usize;
        // Safety: i < table_size <= TABLESIZE_MAX
        unsafe {
            *out.entries.get_unchecked_mut(i) = SequenceDecodeEntry {
                new_state: e.new_state,
                nb_bits: e.nb_bits,
                nb_additional_bits: if sym < extra_bits.len() {
                    extra_bits[sym]
                } else {
                    0
                },
                baseline: if sym < baselines.len() {
                    baselines[sym]
                } else {
                    0
                },
            };
        }
    }
}

/// Build a SequenceDTable for offsets, writing ONLY active entries.
#[allow(unsafe_code)]
pub(crate) fn build_offset_sequence_dtable_from(dt: &DTable, out: &mut SequenceDTable) {
    let table_size = if dt.table_log == 0 {
        1
    } else {
        1usize << dt.table_log
    };
    out.table_log = dt.table_log;
    for i in 0..table_size {
        let e = &dt.entries[i];
        unsafe {
            *out.entries.get_unchecked_mut(i) = SequenceDecodeEntry {
                new_state: e.new_state,
                nb_bits: e.nb_bits,
                nb_additional_bits: e.symbol,
                baseline: 0,
            };
        }
    }
}

/// Build a SequenceDTable directly from normalized counts, skipping the DTable
/// intermediate. Fuses `build_dtable` + `build_sequence_dtable_from` into a
/// single pass, avoiding ~2KB of stack allocation and memcpy per table.
#[allow(unsafe_code)]
pub(crate) fn build_sequence_dtable_direct(
    out: &mut SequenceDTable,
    normalized_counter: &[i16],
    max_symbol_value: u32,
    table_log: u32,
    baselines: &[u32],
    extra_bits: &[u8],
) -> Result<()> {
    let max_symbol_index =
        usize::try_from(max_symbol_value).map_err(|_| Error::MaxSymbolValueTooLarge)?;
    if max_symbol_index > SYMBOLVALUE_MAX || max_symbol_index >= normalized_counter.len() {
        return Err(Error::MaxSymbolValueTooLarge);
    }
    if table_log == 0 {
        return Err(Error::Corruption("FSE table log must be non-zero"));
    }
    if table_log as usize > TABLELOG_MAX {
        return Err(Error::TableLogTooLarge);
    }

    let table_size = 1usize << table_log;
    let table_mask = table_size - 1;
    let mut symbol_next = [0u16; SYMBOLVALUE_MAX + 1];
    let mut high_threshold = table_size - 1;

    // Temporary symbol array — we need symbols placed before computing states.
    // Using a fixed array on the stack (512 bytes) avoids the full DTable (2KB+).
    let mut table_symbol = [0u8; TABLESIZE_MAX];

    for symbol in 0..=max_symbol_index {
        if normalized_counter[symbol] == -1 {
            table_symbol[high_threshold] = symbol as u8;
            symbol_next[symbol] = 1;
            high_threshold = high_threshold.saturating_sub(1);
        } else {
            symbol_next[symbol] = normalized_counter[symbol].max(0) as u16;
        }
    }

    let step = table_step(table_size);
    let mut position = 0usize;
    for symbol in 0..=max_symbol_index {
        for _ in 0..normalized_counter[symbol].max(0) {
            table_symbol[position] = symbol as u8;
            position = (position + step) & table_mask;
            while position > high_threshold {
                position = (position + step) & table_mask;
            }
        }
    }
    if position != 0 {
        return Err(Error::Generic);
    }

    out.table_log = table_log as u16;
    for index in 0..table_size {
        let sym = table_symbol[index] as usize;
        let next_state = symbol_next[sym] as u32;
        symbol_next[sym] = symbol_next[sym].wrapping_add(1);
        let nb_bits = (table_log - highbit32(next_state)) as u8;
        let new_state = ((next_state << nb_bits) - table_size as u32) as u16;
        unsafe {
            *out.entries.get_unchecked_mut(index) = SequenceDecodeEntry {
                new_state,
                nb_bits,
                nb_additional_bits: if sym < extra_bits.len() {
                    extra_bits[sym]
                } else {
                    0
                },
                baseline: if sym < baselines.len() {
                    baselines[sym]
                } else {
                    0
                },
            };
        }
    }
    Ok(())
}

/// Build a SequenceDTable for offsets directly from normalized counts.
#[allow(unsafe_code)]
pub(crate) fn build_offset_sequence_dtable_direct(
    out: &mut SequenceDTable,
    normalized_counter: &[i16],
    max_symbol_value: u32,
    table_log: u32,
) -> Result<()> {
    let max_symbol_index =
        usize::try_from(max_symbol_value).map_err(|_| Error::MaxSymbolValueTooLarge)?;
    if max_symbol_index > SYMBOLVALUE_MAX || max_symbol_index >= normalized_counter.len() {
        return Err(Error::MaxSymbolValueTooLarge);
    }
    if table_log == 0 {
        return Err(Error::Corruption("FSE table log must be non-zero"));
    }
    if table_log as usize > TABLELOG_MAX {
        return Err(Error::TableLogTooLarge);
    }

    let table_size = 1usize << table_log;
    let table_mask = table_size - 1;
    let mut symbol_next = [0u16; SYMBOLVALUE_MAX + 1];
    let mut high_threshold = table_size - 1;
    let mut table_symbol = [0u8; TABLESIZE_MAX];

    for symbol in 0..=max_symbol_index {
        if normalized_counter[symbol] == -1 {
            table_symbol[high_threshold] = symbol as u8;
            symbol_next[symbol] = 1;
            high_threshold = high_threshold.saturating_sub(1);
        } else {
            symbol_next[symbol] = normalized_counter[symbol].max(0) as u16;
        }
    }

    let step = table_step(table_size);
    let mut position = 0usize;
    for symbol in 0..=max_symbol_index {
        for _ in 0..normalized_counter[symbol].max(0) {
            table_symbol[position] = symbol as u8;
            position = (position + step) & table_mask;
            while position > high_threshold {
                position = (position + step) & table_mask;
            }
        }
    }
    if position != 0 {
        return Err(Error::Generic);
    }

    out.table_log = table_log as u16;
    for index in 0..table_size {
        let sym = table_symbol[index];
        let next_state = symbol_next[sym as usize] as u32;
        symbol_next[sym as usize] = symbol_next[sym as usize].wrapping_add(1);
        let nb_bits = (table_log - highbit32(next_state)) as u8;
        let new_state = ((next_state << nb_bits) - table_size as u32) as u16;
        unsafe {
            *out.entries.get_unchecked_mut(index) = SequenceDecodeEntry {
                new_state,
                nb_bits,
                nb_additional_bits: sym,
                baseline: 0,
            };
        }
    }
    Ok(())
}

/// Initialize a DState from a SequenceDTable.
#[inline(always)]
pub(crate) fn init_dstate_seq(bit_d: &mut BitDStream, dt: &SequenceDTable) -> DState {
    let state = bit_d.read_bits(dt.table_log as u32) as usize;
    let _ = bit_d.reload();
    DState { state }
}

/// Update FSE state using a SequenceDecodeEntry. Branchless:
/// `read_bits_fast_zero_safe(0)` returns 0 with no side effects,
/// so the `nb_bits == 0` case is handled without a branch.
#[inline(always)]
pub(crate) fn update_state_with_seq_entry_fast(
    state: &mut DState,
    bit_d: &mut BitDStream,
    entry: SequenceDecodeEntry,
) {
    let nb_bits = entry.nb_bits as u32;
    let low_bits = bit_d.read_bits_fast_zero_safe(nb_bits) as usize;
    state.state = entry.new_state as usize + low_bits;
}

pub(crate) fn build_ctable(
    ct: &mut CTable,
    normalized_counter: &[i16],
    max_symbol_value: u32,
    table_log: u32,
) -> Result<()> {
    let max_symbol_index =
        usize::try_from(max_symbol_value).map_err(|_| Error::MaxSymbolValueTooLarge)?;
    if max_symbol_index > SYMBOLVALUE_MAX || max_symbol_index >= normalized_counter.len() {
        return Err(Error::MaxSymbolValueTooLarge);
    }
    if table_log == 0 {
        return Err(Error::Corruption("FSE table log must be non-zero"));
    }
    if table_log as usize > TABLELOG_MAX {
        return Err(Error::TableLogTooLarge);
    }

    let table_size = 1usize << table_log;
    let table_mask = table_size - 1;
    let max_sv1 = max_symbol_index + 1;
    let step = table_step(table_size);
    let mut cumul = [0u16; SYMBOLVALUE_MAX + 2];
    let mut table_symbol = [0u8; TABLESIZE_MAX];
    let mut high_threshold = table_size - 1;

    ct.table_log = table_log as u16;
    ct.max_symbol_value = max_symbol_value as u16;
    ct.state_table.fill(0);
    ct.symbol_tt.fill(SymbolCompressionTransform::default());

    cumul[0] = 0;
    for u in 1..=max_sv1 {
        let count = normalized_counter[u - 1];
        if count == -1 {
            cumul[u] = cumul[u - 1] + 1;
            table_symbol[high_threshold] = (u - 1) as u8;
            high_threshold = high_threshold.saturating_sub(1);
        } else {
            if count < 0 {
                return Err(Error::Corruption("invalid normalized FSE count"));
            }
            cumul[u] = cumul[u - 1] + count as u16;
        }
    }
    cumul[max_sv1] = table_size as u16 + 1;

    let mut position = 0usize;
    for (symbol, &count) in normalized_counter[..max_sv1].iter().enumerate() {
        if count < 0 {
            continue;
        }
        for _ in 0..count {
            table_symbol[position] = symbol as u8;
            position = (position + step) & table_mask;
            while position > high_threshold {
                position = (position + step) & table_mask;
            }
        }
    }
    if position != 0 {
        return Err(Error::Generic);
    }

    for (u, &symbol) in table_symbol[..table_size].iter().enumerate() {
        let slot = &mut cumul[symbol as usize];
        ct.state_table[*slot as usize] = (table_size + u) as u16;
        *slot = slot.wrapping_add(1);
    }

    let mut total = 0u32;
    for (symbol, &count) in normalized_counter[..=max_symbol_index].iter().enumerate() {
        ct.symbol_tt[symbol] = match count {
            0 => SymbolCompressionTransform {
                delta_find_state: 0,
                delta_nb_bits: ((table_log + 1) << 16) - table_size as u32,
            },
            -1 | 1 => {
                let transform = SymbolCompressionTransform {
                    delta_find_state: total as i32 - 1,
                    delta_nb_bits: (table_log << 16) - table_size as u32,
                };
                total += 1;
                transform
            }
            count if count > 1 => {
                let count = count as u32;
                let max_bits_out = table_log - highbit32(count - 1);
                let min_state_plus = count << max_bits_out;
                let transform = SymbolCompressionTransform {
                    delta_find_state: total as i32 - count as i32,
                    delta_nb_bits: (max_bits_out << 16) - min_state_plus,
                };
                total += count;
                transform
            }
            _ => return Err(Error::Corruption("invalid normalized FSE count")),
        };
    }
    // C's FSE_buildCTable fills entries beyond maxSymbolValue with the
    // zero-count deltaNbBits so that FSE_getMaxNbBits returns tableLog+1.
    // Without this, max_nb_bits returns 0 for these entries, causing the
    // optimal parser's dictionary price seed to diverge from C's.
    let zero_count_delta = ((table_log + 1) << 16) - table_size as u32;
    for symbol in (max_symbol_index + 1)..ct.symbol_tt.len() {
        ct.symbol_tt[symbol].delta_nb_bits = zero_count_delta;
    }

    Ok(())
}

pub(crate) fn build_rle_ctable(ct: &mut CTable, symbol: u8) {
    ct.table_log = 0;
    ct.max_symbol_value = symbol.into();
    ct.state_table.fill(0);
    ct.symbol_tt.fill(SymbolCompressionTransform::default());
    ct.symbol_tt[symbol as usize] = SymbolCompressionTransform {
        delta_find_state: 0,
        delta_nb_bits: 0,
    };
}

pub(crate) fn ctable_log(ct: &CTable) -> u32 {
    u32::from(ct.table_log)
}

pub(crate) fn ctable_max_symbol_value(ct: &CTable) -> u32 {
    u32::from(ct.max_symbol_value)
}

pub(crate) fn ctable_bit_cost(ct: &CTable, symbol: u8, accuracy_log: u32) -> Result<u32> {
    if u16::from(symbol) > ct.max_symbol_value {
        return Err(Error::Corruption("FSE symbol exceeds compression table"));
    }

    let transform = ct.symbol_tt[symbol as usize];
    let min_nb_bits = transform.delta_nb_bits >> 16;
    let threshold = (min_nb_bits + 1) << 16;
    let table_log = u32::from(ct.table_log);
    let table_size = 1u32 << table_log;
    let delta_from_threshold = threshold - (transform.delta_nb_bits + table_size);
    let normalized_delta_from_threshold = (delta_from_threshold << accuracy_log) >> table_log;
    let bit_multiplier = 1u32 << accuracy_log;

    Ok((min_nb_bits + 1) * bit_multiplier - normalized_delta_from_threshold)
}

pub(crate) fn init_cstate(state: &mut CState, ct: &CTable) {
    state.value = 1u32 << ct.table_log;
}

pub(crate) fn init_cstate2(state: &mut CState, ct: &CTable, symbol: u8) -> Result<()> {
    if symbol as u16 > ct.max_symbol_value {
        return Err(Error::Corruption("FSE symbol exceeds compression table"));
    }

    init_cstate(state, ct);
    let transform = ct.symbol_tt[symbol as usize];
    let nb_bits_out = (transform.delta_nb_bits + (1 << 15)) >> 16;
    state.value = (nb_bits_out << 16) - transform.delta_nb_bits;
    state.value = u32::from(
        *ct.state_table
            .get(((state.value >> nb_bits_out) as i32 + transform.delta_find_state) as usize)
            .ok_or(Error::Corruption(
                "FSE compression state exceeds table size",
            ))?,
    );
    Ok(())
}

pub(crate) fn encode_symbol(
    bit_c: &mut BitCStream<'_>,
    state: &mut CState,
    ct: &CTable,
    symbol: u8,
) -> Result<()> {
    if symbol as u16 > ct.max_symbol_value {
        return Err(Error::Corruption("FSE symbol exceeds compression table"));
    }

    let transform = ct.symbol_tt[symbol as usize];
    let nb_bits_out = (state.value + transform.delta_nb_bits) >> 16;
    bit_c.add_bits(state.value.into(), nb_bits_out);
    let next_index = ((state.value >> nb_bits_out) as i32 + transform.delta_find_state) as usize;
    state.value = u32::from(*ct.state_table.get(next_index).ok_or(Error::Corruption(
        "FSE compression state exceeds table size",
    ))?);
    Ok(())
}

/// Encode an FSE symbol without bounds checks, matching C zstd's inline
/// `FSE_encodeSymbol`.
///
/// # Safety
///
/// `symbol <= ct.max_symbol_value` must hold, and `ct`'s state table must have
/// been correctly built. The second half is load-bearing rather than a formality:
/// `next_index` is derived from the table's own `delta_find_state`, so a
/// malformed table produces an out-of-bounds index from in-range inputs.
#[inline(always)]
#[allow(unsafe_code)]
pub(crate) unsafe fn encode_symbol_unchecked(
    bit_c: &mut BitCStream<'_>,
    state: &mut CState,
    ct: &CTable,
    symbol: u8,
) {
    // SAFETY: `symbol <= ct.max_symbol_value` by contract, and `symbol_tt` is
    // sized to hold every symbol up to that value.
    let transform = unsafe { *ct.symbol_tt.get_unchecked(symbol as usize) };
    let nb_bits_out = (state.value + transform.delta_nb_bits) >> 16;
    bit_c.add_bits(state.value.into(), nb_bits_out);
    let next_index = ((state.value >> nb_bits_out) as i32 + transform.delta_find_state) as usize;
    // SAFETY: for a correctly built table, `delta_find_state` maps the shifted
    // state into `state_table`'s range by construction — that is the invariant
    // the caller is being asked to uphold.
    state.value = u32::from(unsafe { *ct.state_table.get_unchecked(next_index) });
}

pub(crate) fn flush_cstate(bit_c: &mut BitCStream<'_>, state: &CState, ct: &CTable) -> Result<()> {
    bit_c.add_bits(state.value.into(), u32::from(ct.table_log));
    bit_c.flush_bits();
    Ok(())
}

#[inline(always)]
fn block_bound(size: usize) -> usize {
    size + (size >> 7) + 4 + CONTAINER_BYTES
}

fn compress_using_ctable_generic(
    dst: &mut [u8],
    src: &[u8],
    ct: &CTable,
    fast: bool,
) -> Result<usize> {
    if src.len() <= 2 {
        return Ok(0);
    }

    let mut bit_c = match BitCStream::new(dst) {
        Ok(stream) => stream,
        Err(Error::DstSizeTooSmall) => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut state1 = CState::default();
    let mut state2 = CState::default();
    let mut ip = src.len();

    if (src.len() & 1) != 0 {
        init_cstate2(&mut state1, ct, src[ip - 1])?;
        ip -= 1;
        init_cstate2(&mut state2, ct, src[ip - 1])?;
        ip -= 1;
        encode_symbol(&mut bit_c, &mut state1, ct, src[ip - 1])?;
        ip -= 1;
        if fast {
            bit_c.flush_bits_fast();
        } else {
            bit_c.flush_bits();
        }
    } else {
        init_cstate2(&mut state2, ct, src[ip - 1])?;
        ip -= 1;
        init_cstate2(&mut state1, ct, src[ip - 1])?;
        ip -= 1;
    }

    let src_size = src.len() - 2;
    if CONTAINER_BITS > (TABLELOG_MAX * 4 + 7) as u32 && (src_size & 2) != 0 {
        encode_symbol(&mut bit_c, &mut state2, ct, src[ip - 1])?;
        ip -= 1;
        encode_symbol(&mut bit_c, &mut state1, ct, src[ip - 1])?;
        ip -= 1;
        if fast {
            bit_c.flush_bits_fast();
        } else {
            bit_c.flush_bits();
        }
    }

    while ip > 0 {
        encode_symbol(&mut bit_c, &mut state2, ct, src[ip - 1])?;
        ip -= 1;

        if CONTAINER_BITS < (TABLELOG_MAX * 2 + 7) as u32 {
            if fast {
                bit_c.flush_bits_fast();
            } else {
                bit_c.flush_bits();
            }
        }

        encode_symbol(&mut bit_c, &mut state1, ct, src[ip - 1])?;
        ip -= 1;

        if CONTAINER_BITS > (TABLELOG_MAX * 4 + 7) as u32 {
            encode_symbol(&mut bit_c, &mut state2, ct, src[ip - 1])?;
            ip -= 1;
            encode_symbol(&mut bit_c, &mut state1, ct, src[ip - 1])?;
            ip -= 1;
        }

        if fast {
            bit_c.flush_bits_fast();
        } else {
            bit_c.flush_bits();
        }
    }

    flush_cstate(&mut bit_c, &state2, ct)?;
    flush_cstate(&mut bit_c, &state1, ct)?;
    Ok(bit_c.close())
}

pub(crate) fn compress_using_ctable(dst: &mut [u8], src: &[u8], ct: &CTable) -> Result<usize> {
    let fast = dst.len() >= block_bound(src.len());
    compress_using_ctable_generic(dst, src, ct, fast)
}

pub(crate) fn table_log(dt: &DTable) -> u32 {
    u32::from(dt.table_log)
}

pub(crate) fn decode_entry(dt: &DTable, state: usize) -> Result<DecodeEntry> {
    let table_size = if dt.table_log == 0 {
        1usize
    } else {
        1usize << dt.table_log
    };
    let entry = dt
        .entries
        .get(state)
        .copied()
        .ok_or(Error::Corruption("FSE state exceeds table size"))?;
    if state >= table_size {
        return Err(Error::Corruption("FSE state exceeds table size"));
    }
    Ok(entry)
}

#[inline(always)]
pub(crate) fn init_dstate(bit_d: &mut BitDStream, dt: &DTable) -> DState {
    let state = bit_d.read_bits(dt.table_log as u32) as usize;
    let _ = bit_d.reload();
    DState { state }
}

#[inline(always)]
pub(crate) fn peek_entry(state: &DState, dt: &DTable) -> Result<DecodeEntry> {
    let table_size = if dt.table_log == 0 {
        1usize
    } else {
        1usize << dt.table_log
    };
    if state.state >= table_size {
        return Err(Error::Corruption("FSE state exceeds table size"));
    }
    Ok(dt.entries[state.state])
}

pub(crate) fn peek_symbol(state: &DState, dt: &DTable) -> u8 {
    dt.entries[state.state].symbol
}

#[inline(always)]
pub(crate) fn peek_entry_fast(state: &DState, dt: &DTable) -> DecodeEntry {
    debug_assert!(
        state.state
            < if dt.table_log == 0 {
                1usize
            } else {
                1usize << dt.table_log
            }
    );
    dt.entries[state.state]
}

#[inline(always)]
pub(crate) fn update_state_with_entry(
    state: &mut DState,
    bit_d: &mut BitDStream,
    entry: DecodeEntry,
) {
    let low_bits = if bit_d.can_read_fast(entry.nb_bits as u32) {
        bit_d.read_bits_fast(entry.nb_bits as u32)
    } else {
        bit_d.read_bits(entry.nb_bits as u32)
    } as usize;
    state.state = entry.new_state as usize + low_bits;
}

/// Like `update_state_with_entry`, but skips the `can_read_fast` check.
/// Only safe to call when a recent reload guarantees enough bits remain
/// (e.g. after reload, the three state updates need at most 9+9+8=26 bits,
/// well within the 57-bit minimum on 64-bit).
#[inline(always)]
pub(crate) fn update_state_with_entry_fast(
    state: &mut DState,
    bit_d: &mut BitDStream,
    entry: DecodeEntry,
) {
    // Branchless: read_bits_fast_zero_safe handles nb_bits == 0 by masking
    // rather than branching.
    let low_bits = bit_d.read_bits_fast_zero_safe(entry.nb_bits as u32) as usize;
    state.state = entry.new_state as usize + low_bits;
}

pub(crate) fn update_state(state: &mut DState, bit_d: &mut BitDStream, dt: &DTable) {
    let info = dt.entries[state.state];
    update_state_with_entry(state, bit_d, info);
}

pub(crate) fn decode_symbol(state: &mut DState, bit_d: &mut BitDStream, dt: &DTable) -> u8 {
    let info = dt.entries[state.state];
    let low_bits = if bit_d.can_read_fast(info.nb_bits as u32) {
        bit_d.read_bits_fast(info.nb_bits as u32)
    } else {
        bit_d.read_bits(info.nb_bits as u32)
    } as usize;
    state.state = info.new_state as usize + low_bits;
    info.symbol
}

pub(crate) fn decode_symbol_fast(state: &mut DState, bit_d: &mut BitDStream, dt: &DTable) -> u8 {
    let info = dt.entries[state.state];
    let low_bits = bit_d.read_bits_fast(info.nb_bits as u32) as usize;
    state.state = info.new_state as usize + low_bits;
    info.symbol
}

pub(crate) fn end_of_dstate(state: &DState) -> bool {
    state.state == 0
}

fn decompress_using_dtable_generic(
    dst: &mut [u8],
    src: &[u8],
    dt: &DTable,
    fast: bool,
) -> Result<usize> {
    if dt.table_log == 0 {
        return Err(Error::Corruption(
            "FSE interleaved decode does not support RLE tables",
        ));
    }

    let mut bit_d = BitDStream::new(src)?;
    let mut state1 = init_dstate(&mut bit_d, dt);
    let mut state2 = init_dstate(&mut bit_d, dt);

    // Upstream checks this in the same position, right after the two
    // `FSE_initDState` calls (`fse_decompress.c`). Reading the two initial
    // states costs `2 * table_log` bits; if the stream held fewer than that,
    // the reload reports overflow and every symbol decoded from here on is
    // invented out of bits that were never in the input.
    //
    // Without it the decode still terminates — the tail loop breaks on the
    // same overflow — so this was not a crash but a silent wrong answer: a
    // truncated Huffman weight description would produce a plausible table and
    // `decode_all` would hand the caller `Ok(garbage)` for a frame upstream
    // refuses. See `truncated_huff_state.zst` in the upstream
    // `tests/golden-decompression-errors` corpus.
    //
    // The loop below opens with its own `reload()`. Repeating it is harmless:
    // the first call leaves fewer than 8 unconsumed bits, so the second moves
    // no bytes. Upstream does the same.
    if bit_d.reload() == BitDStreamStatus::Overflow {
        return Err(Error::Corruption(
            "FSE bitstream is too short to hold both initial states",
        ));
    }

    let mut op = 0usize;
    let olimit = dst.len().saturating_sub(3);

    macro_rules! get_symbol {
        ($state:expr) => {{
            if fast {
                decode_symbol_fast($state, &mut bit_d, dt)
            } else {
                decode_symbol($state, &mut bit_d, dt)
            }
        }};
    }

    while bit_d.reload() == BitDStreamStatus::Unfinished && op < olimit {
        dst[op] = get_symbol!(&mut state1);
        op += 1;
        if (TABLELOG_MAX * 2 + 7) as u32 > CONTAINER_BITS {
            let _ = bit_d.reload();
        }
        dst[op] = get_symbol!(&mut state2);
        op += 1;
        if (TABLELOG_MAX * 4 + 7) as u32 > CONTAINER_BITS
            && bit_d.reload() != BitDStreamStatus::Unfinished
        {
            break;
        }
        dst[op] = get_symbol!(&mut state1);
        op += 1;
        if (TABLELOG_MAX * 2 + 7) as u32 > CONTAINER_BITS {
            let _ = bit_d.reload();
        }
        dst[op] = get_symbol!(&mut state2);
        op += 1;
    }

    loop {
        if op > dst.len().saturating_sub(2) {
            return Err(Error::DstSizeTooSmall);
        }
        dst[op] = get_symbol!(&mut state1);
        op += 1;
        if bit_d.reload() == BitDStreamStatus::Overflow {
            dst[op] = get_symbol!(&mut state2);
            op += 1;
            break;
        }
        if op > dst.len().saturating_sub(2) {
            return Err(Error::DstSizeTooSmall);
        }
        dst[op] = get_symbol!(&mut state2);
        op += 1;
        if bit_d.reload() == BitDStreamStatus::Overflow {
            dst[op] = get_symbol!(&mut state1);
            op += 1;
            break;
        }
    }

    Ok(op)
}

pub(crate) fn decompress_interleaved2(dst: &mut [u8], src: &[u8], dt: &DTable) -> Result<usize> {
    if dt.fast_mode {
        decompress_using_dtable_generic(dst, src, dt, true)
    } else {
        decompress_using_dtable_generic(dst, src, dt, false)
    }
}

pub(crate) fn decompress(
    dst: &mut [u8],
    src: &[u8],
    max_symbol_value: u32,
    max_table_log: usize,
) -> Result<usize> {
    let mut normalized = [0i16; SYMBOLVALUE_MAX + 1];
    let mut max_symbol_value = max_symbol_value;
    let mut table_log = 0u32;
    let ncount_len = read_ncount(
        &mut normalized,
        &mut max_symbol_value,
        &mut table_log,
        src,
        max_table_log,
    )?;
    let mut dtable = DTable::default();
    build_dtable(&mut dtable, &normalized, max_symbol_value, table_log)?;
    decompress_interleaved2(dst, &src[ncount_len..], &dtable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_table_logs_above_the_configured_limit() {
        let mut normalized = [0i16; 16];
        let mut max_symbol = 15u32;
        let mut table_log = 0u32;
        let err = read_ncount(
            &mut normalized,
            &mut max_symbol,
            &mut table_log,
            &[0xff, 0, 0, 0],
            6,
        )
        .unwrap_err();
        assert_eq!(err, Error::TableLogTooLarge);
    }

    /// The floor guard exists to keep `normalize_count_m2` from subtracting
    /// below zero, so the case worth pinning is the one that would.
    ///
    /// Forty symbols each seen once, into a table log of 5. Every count lands
    /// at or under `low_threshold`, so m2 would mark all forty as distributed
    /// and then evaluate `(1 << 5) - 40` — a panic in debug, a wrapped count
    /// in release. `min_table_log` is 6 here, and the guard rejects first.
    #[test]
    fn rejects_a_table_log_below_the_floor_its_symbols_need() {
        let mut counts = [0u32; SYMBOLVALUE_MAX + 1];
        for count in counts.iter_mut().take(40) {
            *count = 1;
        }
        let mut normalized = [0i16; SYMBOLVALUE_MAX + 1];

        assert_eq!(min_table_log(40, 39), 6);
        let err = normalize_count(&mut normalized, 5, &counts, 40, 39, false).unwrap_err();
        assert_eq!(err, Error::Generic);

        // And the guard is a floor, not a ceiling: the same counts normalize
        // fine at the log it asked for.
        assert!(normalize_count(&mut normalized, 6, &counts, 40, 39, false).is_ok());
    }

    #[test]
    fn rejects_max_symbol_values_past_the_provided_buffer() {
        let mut normalized = [0i16; 8];
        let mut max_symbol = 8u32;
        let mut table_log = 0u32;
        let err =
            read_ncount(&mut normalized, &mut max_symbol, &mut table_log, &[0; 4], 6).unwrap_err();
        assert_eq!(err, Error::MaxSymbolValueTooLarge);
    }

    #[test]
    fn rle_dtable_decodes_the_configured_symbol() {
        let mut dtable = DTable::default();
        build_rle_dtable(&mut dtable, b'Z');

        let mut bit_d = BitDStream::new(&[1]).unwrap();
        let mut state = init_dstate(&mut bit_d, &dtable);
        assert_eq!(peek_symbol(&state, &dtable), b'Z');
        assert_eq!(decode_symbol(&mut state, &mut bit_d, &dtable), b'Z');
        assert!(end_of_dstate(&state));
    }

    #[test]
    fn invalid_short_ncount_returns_an_error() {
        let mut normalized = [0i16; SYMBOLVALUE_MAX + 1];
        let mut max_symbol = 35u32;
        let mut table_log = 0u32;
        let err =
            read_ncount(&mut normalized, &mut max_symbol, &mut table_log, &[0], 9).unwrap_err();
        assert_eq!(err, Error::Corruption("invalid FSE ncount"));
    }

    #[test]
    fn ncount_roundtrips_normalized_distributions() {
        let normalized = [
            4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1,
            1, 1, 1, -1, -1, -1, -1,
        ];
        let mut encoded = vec![0u8; ncount_write_bound(35, 6).unwrap()];
        let written = write_ncount(&mut encoded, &normalized, 35, 6).unwrap();
        encoded.truncate(written);

        let mut decoded = [0i16; SYMBOLVALUE_MAX + 1];
        let mut max_symbol = 35u32;
        let mut table_log = 0u32;
        let consumed =
            read_ncount(&mut decoded, &mut max_symbol, &mut table_log, &encoded, 9).unwrap();

        assert_eq!(consumed, encoded.len());
        assert_eq!(table_log, 6);
        assert_eq!(max_symbol, 35);
        assert_eq!(&decoded[..=35], &normalized);
    }

    #[test]
    fn normalize_count_produces_roundtrippable_headers() {
        let counts = [1800u32, 150, 60, 38];
        let mut normalized = [0i16; SYMBOLVALUE_MAX + 1];
        let table_log = optimal_table_log(8, 2048, 3);
        normalize_count(&mut normalized, table_log, &counts, 2048, 3, true).unwrap();

        let mut encoded = vec![0u8; ncount_write_bound(3, table_log).unwrap()];
        let written = write_ncount(&mut encoded, &normalized, 3, table_log).unwrap();
        encoded.truncate(written);

        let mut decoded = [0i16; SYMBOLVALUE_MAX + 1];
        let mut max_symbol = 3u32;
        let mut decoded_table_log = 0u32;
        read_ncount(
            &mut decoded,
            &mut max_symbol,
            &mut decoded_table_log,
            &encoded,
            8,
        )
        .unwrap();

        assert_eq!(decoded_table_log, table_log);
        assert_eq!(max_symbol, 3);
        assert_eq!(&decoded[..=3], &normalized[..=3]);
    }

    #[test]
    fn compress_using_ctable_roundtrips_weight_like_symbols() {
        let src = (0..96)
            .map(|index| match index % 12 {
                0..=4 => 1u8,
                5..=7 => 2u8,
                8..=9 => 3u8,
                10 => 4u8,
                _ => 5u8,
            })
            .collect::<Vec<_>>();

        let mut counts = [0u32; SYMBOLVALUE_MAX + 1];
        for &symbol in &src {
            counts[symbol as usize] += 1;
        }

        let max_symbol_value = 5u32;
        let table_log = optimal_table_log(6, src.len(), max_symbol_value);
        let mut normalized = [0i16; SYMBOLVALUE_MAX + 1];
        normalize_count(
            &mut normalized,
            table_log,
            &counts,
            src.len(),
            max_symbol_value,
            false,
        )
        .unwrap();

        let mut ctable = CTable::default();
        build_ctable(&mut ctable, &normalized, max_symbol_value, table_log).unwrap();
        let mut dtable = DTable::default();
        build_dtable(&mut dtable, &normalized, max_symbol_value, table_log).unwrap();

        let mut compressed = vec![0u8; block_bound(src.len())];
        let written = compress_using_ctable(&mut compressed, &src, &ctable).unwrap();
        assert!(written > 0, "expected FSE-compressible symbols");

        let mut decoded = vec![0u8; src.len()];
        let decoded_size =
            decompress_interleaved2(&mut decoded, &compressed[..written], &dtable).unwrap();
        assert_eq!(decoded_size, src.len());
        assert_eq!(decoded, src);
    }
}
