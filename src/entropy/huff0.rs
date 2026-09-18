use core::cmp;

use crate::{
    entropy::{
        bitstream::{BitCStream, BitDStream, BitDStreamStatus, CONTAINER_BITS, CONTAINER_BYTES},
        fse,
        hist::{HIST_WKSP_SIZE_U32, count_simple, count_wksp},
        mem::highbit32,
    },
    error::{Error, Result},
};

pub(crate) const BLOCKSIZE_MAX: usize = 128 * 1024;
pub(crate) const TABLELOG_MAX: usize = 12;
/// Table depth the double-symbol decoder expands a shallower table up to.
///
/// C's `HUF_DECODER_FAST_TABLELOG`. A description whose own `table_log` is at
/// most this deep is built into a table indexed by exactly this many bits, so
/// the spare depth can hold a second symbol per entry. Deeper descriptions keep
/// their own depth and mostly decode one symbol at a time, which is why the
/// decoder selector below stops choosing this shape as literals get harder.
const DECODER_FAST_TABLELOG: usize = 11;
const TABLELOG_DEFAULT: usize = 11;
const SYMBOLVALUE_MAX: usize = 255;
const STARTNODE: usize = SYMBOLVALUE_MAX + 1;
const CTABLE_WORKSPACE_SIZE_U32: usize = (2 * SYMBOLVALUE_MAX) + 2;
const BLOCKBOUND_MIN: usize = 6 + 1 + 1 + 1 + 8;
const MAX_FSE_TABLELOG_FOR_HUFF_HEADER: usize = 6;

#[derive(Clone, Copy, Default)]
struct CElt {
    val: u16,
    nb_bits: u8,
}

#[derive(Clone, Copy, Default)]
struct NodeElt {
    count: u32,
    parent: u16,
    byte: u8,
    nb_bits: u8,
}

#[derive(Clone, Copy, Default)]
struct RankPos {
    base: u32,
    current: u32,
}

/// C's `RANK_POSITION_TABLE_SIZE` (`huf_compress.c:508`).
const RANK_POSITION_TABLE_SIZE: usize = 192;

/// C's `RANK_POSITION_LOG_BUCKETS_BEGIN`: `(192 - 1) - 32 - 1`.
const RANK_POSITION_LOG_BUCKETS_BEGIN: u32 = 158;

/// C's `RANK_POSITION_DISTINCT_COUNT_CUTOFF`, which is
/// `RANK_POSITION_LOG_BUCKETS_BEGIN + ZSTD_highbit32(RANK_POSITION_LOG_BUCKETS_BEGIN)`.
/// The comment above it in C says 166; the expression says 165.
const RANK_POSITION_DISTINCT_COUNT_CUTOFF: u32 = RANK_POSITION_LOG_BUCKETS_BEGIN + 7;

#[derive(Clone, Copy)]
struct BuildCTableWorkspace {
    nodes: [NodeElt; CTABLE_WORKSPACE_SIZE_U32],
    rank_position: [RankPos; RANK_POSITION_TABLE_SIZE],
}

impl Default for BuildCTableWorkspace {
    fn default() -> Self {
        Self {
            nodes: [NodeElt::default(); CTABLE_WORKSPACE_SIZE_U32],
            rank_position: [RankPos::default(); RANK_POSITION_TABLE_SIZE],
        }
    }
}

#[derive(Clone)]
struct WeightCompressionWorkspace {
    count: [u32; TABLELOG_MAX + 1],
    norm: [i16; TABLELOG_MAX + 1],
    ctable: fse::CTable,
}

impl Default for WeightCompressionWorkspace {
    fn default() -> Self {
        Self {
            count: [0; TABLELOG_MAX + 1],
            norm: [0; TABLELOG_MAX + 1],
            ctable: fse::CTable::default(),
        }
    }
}

#[derive(Clone)]
/// Workspace for Huffman compression. ~13KB on the stack.
/// Allocate once and reuse across blocks to avoid repeated zeroing.
pub(crate) struct CompressWorkspace {
    count: [u32; SYMBOLVALUE_MAX + 1],
    ctable: [CElt; SYMBOLVALUE_MAX + 1],
    build: BuildCTableWorkspace,
    weights: WeightCompressionWorkspace,
    hist: [u32; HIST_WKSP_SIZE_U32],
}

impl Default for CompressWorkspace {
    fn default() -> Self {
        Self {
            count: [0; SYMBOLVALUE_MAX + 1],
            ctable: [CElt::default(); SYMBOLVALUE_MAX + 1],
            build: BuildCTableWorkspace::default(),
            weights: WeightCompressionWorkspace::default(),
            hist: [0; HIST_WKSP_SIZE_U32],
        }
    }
}

impl CompressWorkspace {
    /// Build a literals table from an explicit symbol histogram rather than by
    /// counting a buffer, returning the table log the builder settled on.
    ///
    /// Dictionary training needs this: its histogram is accumulated across many
    /// samples, and it inspects the resulting table log before deciding whether
    /// the distribution is writable at all. Pairs with
    /// [`Self::write_literal_ctable`], which serializes whatever this last
    /// built, so the caller can rebuild from a different histogram in between.
    pub(crate) fn build_literal_ctable(
        &mut self,
        count: &[u32; SYMBOLVALUE_MAX + 1],
        max_symbol_value: u32,
        max_nb_bits: u32,
    ) -> Result<u32> {
        build_ctable(
            &mut self.ctable,
            count,
            max_symbol_value,
            max_nb_bits,
            &mut self.build,
        )
    }

    /// Serialize the table built by [`Self::build_literal_ctable`] into `dst`,
    /// returning the bytes written.
    pub(crate) fn write_literal_ctable(
        &mut self,
        dst: &mut [u8],
        max_symbol_value: u32,
        huff_log: u32,
    ) -> Result<usize> {
        write_ctable(
            dst,
            &self.ctable,
            max_symbol_value,
            huff_log,
            &mut self.weights,
        )
    }
}

#[derive(Clone, Copy, Default)]
struct DEltX1 {
    byte: u8,
    nb_bits: u8,
}

#[derive(Clone, Copy)]
pub(crate) struct CTableX1 {
    table_log: u8,
    entries: [CElt; SYMBOLVALUE_MAX + 1],
}

impl Default for CTableX1 {
    fn default() -> Self {
        Self {
            table_log: 0,
            entries: [CElt::default(); SYMBOLVALUE_MAX + 1],
        }
    }
}

impl CTableX1 {
    /// Return the number of bits needed to encode `symbol`.
    /// Matches C's `HUF_getNbBitsFromCTable`.
    #[inline(always)]
    pub(crate) fn symbol_nb_bits(&self, symbol: u8) -> u8 {
        self.entries[symbol as usize].nb_bits
    }
}

/// Entry count of a `DTableX1`.
///
/// `read_stats` admits a `table_log` up to `TABLELOG_MAX`, and both the fill
/// loop in `read_dtable_x1` and `decode_symbol_x1` index the table with
/// exactly `table_log` bits, so it has to hold `1 << TABLELOG_MAX` entries.
/// Upstream sizes the same table from `ZSTD_HUFFDTABLE_CAPACITY_LOG`, which is
/// also 12.
///
/// This was `1 << (TABLELOG_MAX - 1)`, half the size the code around it
/// assumed. A literals section whose weights sum to a `table_log` of 12 — 40
/// bytes of input is enough — ran the fill loop off the end of the array, and
/// `decode_symbol_x1` would have read off the end of it *without* a bounds
/// check, since it indexes with `get_unchecked` on a safety argument that
/// named `1 << TABLELOG_MAX` while the array was half that.
const DTABLE_X1_SIZE: usize = 1 << TABLELOG_MAX;

#[derive(Clone, Copy)]
pub(crate) struct DTableX1 {
    table_log: u8,
    entries: [DEltX1; DTABLE_X1_SIZE],
}

impl Default for DTableX1 {
    fn default() -> Self {
        Self {
            table_log: 0,
            entries: [DEltX1::default(); DTABLE_X1_SIZE],
        }
    }
}

#[inline(always)]
fn read_segment_length(src: &[u8], offset: usize) -> usize {
    u16::from_le_bytes([src[offset], src[offset + 1]]) as usize
}

fn read_stats(
    huff_weight: &mut [u8; SYMBOLVALUE_MAX + 1],
    rank_stats: &mut [u32; TABLELOG_MAX + 1],
    nb_symbols: &mut u32,
    table_log: &mut u32,
    src: &[u8],
) -> Result<usize> {
    if src.is_empty() {
        return Err(Error::SrcSizeWrong);
    }

    let mut i_size = src[0] as usize;
    let o_size;
    if i_size >= 128 {
        o_size = i_size - 127;
        i_size = o_size.div_ceil(2);
        if i_size + 1 > src.len() || o_size >= huff_weight.len() {
            return Err(Error::SrcSizeWrong);
        }
        for index in (0..o_size).step_by(2) {
            huff_weight[index] = src[1 + (index / 2)] >> 4;
            huff_weight[index + 1] = src[1 + (index / 2)] & 15;
        }
    } else {
        if i_size + 1 > src.len() {
            return Err(Error::SrcSizeWrong);
        }
        let hw_len = huff_weight.len() - 1;
        o_size = fse::decompress(
            &mut huff_weight[..hw_len],
            &src[1..=i_size],
            TABLELOG_MAX as u32,
            MAX_FSE_TABLELOG_FOR_HUFF_HEADER,
        )?;
    }

    rank_stats.fill(0);
    let mut weight_total = 0u32;
    for &weight in &huff_weight[..o_size] {
        if weight as usize >= TABLELOG_MAX {
            return Err(Error::Corruption("invalid Huff0 weight"));
        }
        rank_stats[weight as usize] += 1;
        weight_total += (1u32 << weight) >> 1;
    }
    if weight_total == 0 {
        return Err(Error::Corruption("empty Huff0 weight table"));
    }

    let log = highbit32(weight_total) + 1;
    if log as usize > TABLELOG_MAX {
        return Err(Error::Corruption("Huff0 table log too large"));
    }
    *table_log = log;
    let total = 1u32 << log;
    let rest = total - weight_total;
    let verif = 1u32 << highbit32(rest);
    let last_weight = highbit32(rest) + 1;
    if verif != rest {
        return Err(Error::Corruption("non power-of-two Huff0 weight remainder"));
    }
    huff_weight[o_size] = last_weight as u8;
    rank_stats[last_weight as usize] += 1;
    if rank_stats[1] < 2 || (rank_stats[1] & 1) != 0 {
        return Err(Error::Corruption("invalid Huff0 rank-1 weight count"));
    }
    *nb_symbols = o_size as u32 + 1;
    Ok(i_size + 1)
}

pub(crate) fn read_dtable_x1(src: &[u8], dtable: &mut DTableX1) -> Result<usize> {
    let mut rank_val = [0u32; TABLELOG_MAX + 1];
    let mut huff_weight = [0u8; SYMBOLVALUE_MAX + 1];
    let mut table_log = 0u32;
    let mut nb_symbols = 0u32;
    let i_size = read_stats(
        &mut huff_weight,
        &mut rank_val,
        &mut nb_symbols,
        &mut table_log,
        src,
    )?;
    // `read_stats` already bounds `table_log` by `TABLELOG_MAX`, which is what
    // `DTABLE_X1_SIZE` is derived from, so this cannot fire today. It stays as
    // a real check rather than a `debug_assert` because it is the precondition
    // for the `get_unchecked` in `decode_symbol_x1`, and deriving it from the
    // array it guards is what keeps the two from drifting apart again. They had
    // already drifted: the bound lived only in a safety comment, which named a
    // table twice the size of the one actually declared.
    if (1usize << table_log) > dtable.entries.len() {
        return Err(Error::Corruption(
            "Huff0 table log exceeds decode table capacity",
        ));
    }
    dtable.table_log = table_log as u8;

    let mut next_rank_start = 0u32;
    for index in 1..=table_log as usize {
        let current = next_rank_start;
        next_rank_start += rank_val[index] << (index - 1);
        rank_val[index] = current;
    }

    for index in 0..nb_symbols as usize {
        let weight = huff_weight[index] as usize;
        let length = (1usize << weight) >> 1;
        let start = rank_val[weight] as usize;
        let end = start + length;
        let entry = DEltX1 {
            byte: index as u8,
            nb_bits: (table_log + 1 - weight as u32) as u8,
        };
        rank_val[weight] = end as u32;
        for slot in &mut dtable.entries[start..end] {
            *slot = entry;
        }
    }
    Ok(i_size)
}

pub(crate) fn read_ctable_x1(src: &[u8], ctable: &mut CTableX1) -> Result<usize> {
    Ok(read_ctable_x1_with_repeat_validity(src, ctable)?.0)
}

pub(crate) fn read_ctable_x1_with_repeat_validity(
    src: &[u8],
    ctable: &mut CTableX1,
) -> Result<(usize, bool)> {
    let mut rank_val = [0u32; TABLELOG_MAX + 1];
    let mut huff_weight = [0u8; SYMBOLVALUE_MAX + 1];
    let mut table_log = 0u32;
    let mut nb_symbols = 0u32;
    let i_size = read_stats(
        &mut huff_weight,
        &mut rank_val,
        &mut nb_symbols,
        &mut table_log,
        src,
    )?;
    ctable.table_log = table_log as u8;
    build_ctable_from_weights(
        &mut ctable.entries,
        &huff_weight,
        nb_symbols as usize,
        table_log,
    )?;
    let repeat_valid = nb_symbols as usize == SYMBOLVALUE_MAX + 1
        && huff_weight[..nb_symbols as usize]
            .iter()
            .all(|&weight| weight != 0);
    Ok((i_size, repeat_valid))
}

#[allow(unsafe_code)]
#[inline(always)]
fn decode_symbol_x1(bit_d: &mut BitDStream, table: &[DEltX1], table_log: u32) -> u8 {
    let value = bit_d.look_bits_fast(table_log) as usize;
    // Safety: value = look_bits_fast(table_log) returns at most (1 << table_log) - 1.
    // `table` is a `DTableX1::entries`, which has DTABLE_X1_SIZE = 1 << TABLELOG_MAX
    // entries, and `read_dtable_x1` refuses to populate a table whose table_log
    // would exceed that, so value < table.len() always holds.
    let entry = unsafe { *table.get_unchecked(value) };
    bit_d.skip_bits(entry.nb_bits as u32);
    entry.byte
}

#[allow(unsafe_code)]
fn decode_stream_x1(
    dst: &mut [u8],
    mut pos: usize,
    bit_d: &mut BitDStream,
    end: usize,
    table: &[DEltX1],
    table_log: u32,
) -> usize {
    // Safety for all get_unchecked_mut: the caller guarantees end <= dst.len(),
    // and pos < end is checked by each loop condition before writing.
    while bit_d.reload() == BitDStreamStatus::Unfinished && pos < end.saturating_sub(3) {
        unsafe { *dst.get_unchecked_mut(pos) = decode_symbol_x1(bit_d, table, table_log) };
        pos += 1;
        unsafe { *dst.get_unchecked_mut(pos) = decode_symbol_x1(bit_d, table, table_log) };
        pos += 1;
        unsafe { *dst.get_unchecked_mut(pos) = decode_symbol_x1(bit_d, table, table_log) };
        pos += 1;
        unsafe { *dst.get_unchecked_mut(pos) = decode_symbol_x1(bit_d, table, table_log) };
        pos += 1;
    }
    while pos < end {
        unsafe { *dst.get_unchecked_mut(pos) = decode_symbol_x1(bit_d, table, table_log) };
        pos += 1;
    }
    pos
}

fn decompress_1x1_using_dtable(dst: &mut [u8], src: &[u8], dtable: &DTableX1) -> Result<usize> {
    let mut bit_d = BitDStream::new(src)?;
    let table_log = dtable.table_log as u32;
    let written = decode_stream_x1(dst, 0, &mut bit_d, dst.len(), &dtable.entries, table_log);
    if written != dst.len() || !bit_d.end_of_stream() {
        return Err(Error::Corruption("Huff0 stream did not terminate cleanly"));
    }
    Ok(written)
}

fn decompress_4x1_using_dtable(dst: &mut [u8], src: &[u8], dtable: &DTableX1) -> Result<usize> {
    if src.len() < 10 {
        return Err(Error::Corruption("truncated Huff0 4X stream"));
    }
    // Upstream rejects a 4-stream section whose regenerated size is below 6
    // ("stream 4-split doesn't work", `huf_decompress.c`). Without it the
    // segment starts computed below can land past the end of `dst`: at
    // `dst.len() == 5` the fourth segment starts at `3 * ceil(5 / 4) == 6`.
    // The four `get_unchecked_mut` sites in the main loop and the
    // `decode_stream_x1` calls that finish each segment all write up to those
    // bounds, so this was an out-of-bounds *write* in a release build rather
    // than a panic. Lengths 1 and 2 overflow the same way.
    if dst.len() < 6 {
        return Err(Error::Corruption("Huff0 4X output too small to split"));
    }
    let length1 = read_segment_length(src, 0);
    let length2 = read_segment_length(src, 2);
    let length3 = read_segment_length(src, 4);
    let length4 = src.len().wrapping_sub(length1 + length2 + length3 + 6);
    if length4 > src.len() {
        return Err(Error::Corruption("invalid Huff0 4X segment lengths"));
    }

    let istart1 = 6;
    let istart2 = istart1 + length1;
    let istart3 = istart2 + length2;
    let istart4 = istart3 + length3;
    let segment_size = dst.len().div_ceil(4);

    let mut bit_d1 = BitDStream::new(&src[istart1..istart2])?;
    let mut bit_d2 = BitDStream::new(&src[istart2..istart3])?;
    let mut bit_d3 = BitDStream::new(&src[istart3..istart4])?;
    let mut bit_d4 = BitDStream::new(&src[istart4..])?;

    let op_start2 = segment_size;
    let op_start3 = op_start2 + segment_size;
    let op_start4 = op_start3 + segment_size;
    // Implied by the `dst.len() < 6` rejection above — the tightest case is
    // exactly 6, where the fourth segment starts at the end — but this is the
    // precondition every `get_unchecked_mut` below stands on, and the argument
    // for it is a modular one about `ceil(len / 4)` rather than something
    // visible at the point of use. Checking it costs one comparison per
    // literals section and keeps the unsafe blocks locally justified.
    if op_start4 > dst.len() {
        return Err(Error::Corruption("Huff0 4X segment starts past the output"));
    }
    let olimit = dst.len().saturating_sub(3);
    let mut op1 = 0usize;
    let mut op2 = op_start2;
    let mut op3 = op_start3;
    let mut op4 = op_start4;
    let mut end_signal = true;
    let table_log = dtable.table_log as u32;

    // Safety for all get_unchecked_mut below: op4 < olimit = dst.len() - 3,
    // and op1 <= op2 <= op3 <= op4, so all indices are within bounds.
    // 4 symbols are decoded per stream per outer iteration (16 total), and the
    // loop guard ensures 4 bytes of headroom.
    #[allow(unsafe_code)]
    while end_signal && op4 < olimit {
        unsafe {
            *dst.get_unchecked_mut(op1) = decode_symbol_x1(&mut bit_d1, &dtable.entries, table_log)
        };
        op1 += 1;
        unsafe {
            *dst.get_unchecked_mut(op2) = decode_symbol_x1(&mut bit_d2, &dtable.entries, table_log)
        };
        op2 += 1;
        unsafe {
            *dst.get_unchecked_mut(op3) = decode_symbol_x1(&mut bit_d3, &dtable.entries, table_log)
        };
        op3 += 1;
        unsafe {
            *dst.get_unchecked_mut(op4) = decode_symbol_x1(&mut bit_d4, &dtable.entries, table_log)
        };
        op4 += 1;

        unsafe {
            *dst.get_unchecked_mut(op1) = decode_symbol_x1(&mut bit_d1, &dtable.entries, table_log)
        };
        op1 += 1;
        unsafe {
            *dst.get_unchecked_mut(op2) = decode_symbol_x1(&mut bit_d2, &dtable.entries, table_log)
        };
        op2 += 1;
        unsafe {
            *dst.get_unchecked_mut(op3) = decode_symbol_x1(&mut bit_d3, &dtable.entries, table_log)
        };
        op3 += 1;
        unsafe {
            *dst.get_unchecked_mut(op4) = decode_symbol_x1(&mut bit_d4, &dtable.entries, table_log)
        };
        op4 += 1;

        unsafe {
            *dst.get_unchecked_mut(op1) = decode_symbol_x1(&mut bit_d1, &dtable.entries, table_log)
        };
        op1 += 1;
        unsafe {
            *dst.get_unchecked_mut(op2) = decode_symbol_x1(&mut bit_d2, &dtable.entries, table_log)
        };
        op2 += 1;
        unsafe {
            *dst.get_unchecked_mut(op3) = decode_symbol_x1(&mut bit_d3, &dtable.entries, table_log)
        };
        op3 += 1;
        unsafe {
            *dst.get_unchecked_mut(op4) = decode_symbol_x1(&mut bit_d4, &dtable.entries, table_log)
        };
        op4 += 1;

        unsafe {
            *dst.get_unchecked_mut(op1) = decode_symbol_x1(&mut bit_d1, &dtable.entries, table_log)
        };
        op1 += 1;
        unsafe {
            *dst.get_unchecked_mut(op2) = decode_symbol_x1(&mut bit_d2, &dtable.entries, table_log)
        };
        op2 += 1;
        unsafe {
            *dst.get_unchecked_mut(op3) = decode_symbol_x1(&mut bit_d3, &dtable.entries, table_log)
        };
        op3 += 1;
        unsafe {
            *dst.get_unchecked_mut(op4) = decode_symbol_x1(&mut bit_d4, &dtable.entries, table_log)
        };
        op4 += 1;

        end_signal &= bit_d1.reload_fast() == BitDStreamStatus::Unfinished;
        end_signal &= bit_d2.reload_fast() == BitDStreamStatus::Unfinished;
        end_signal &= bit_d3.reload_fast() == BitDStreamStatus::Unfinished;
        end_signal &= bit_d4.reload_fast() == BitDStreamStatus::Unfinished;
    }

    if op1 > op_start2 || op2 > op_start3 || op3 > op_start4 {
        return Err(Error::Corruption("Huff0 segment overrun"));
    }

    op1 = decode_stream_x1(dst, op1, &mut bit_d1, op_start2, &dtable.entries, table_log);
    op2 = decode_stream_x1(dst, op2, &mut bit_d2, op_start3, &dtable.entries, table_log);
    op3 = decode_stream_x1(dst, op3, &mut bit_d3, op_start4, &dtable.entries, table_log);
    op4 = decode_stream_x1(dst, op4, &mut bit_d4, dst.len(), &dtable.entries, table_log);

    debug_assert_eq!(op1, op_start2);
    debug_assert_eq!(op2, op_start3);
    debug_assert_eq!(op3, op_start4);
    debug_assert_eq!(op4, dst.len());

    if !bit_d1.end_of_stream()
        || !bit_d2.end_of_stream()
        || !bit_d3.end_of_stream()
        || !bit_d4.end_of_stream()
    {
        return Err(Error::Corruption("Huff0 stream did not consume all bits"));
    }
    Ok(dst.len())
}

/// One entry of the double-symbol decode table.
///
/// `sequence` holds the bytes this entry emits in output order, first symbol in
/// the low half, and `length` says how many of them are real. C stores the pair
/// in native byte order so it can `memcpy` the field straight out, which forces
/// it to build the `u16` differently on big-endian hosts; writing
/// `sequence.to_le_bytes()` states the order once and removes that split.
#[derive(Clone, Copy, Default)]
struct DEltX2 {
    sequence: u16,
    nb_bits: u8,
    length: u8,
}

/// Entry count of a `DTableX2`.
///
/// A description of depth `TABLELOG_MAX` keeps that depth, and everything
/// shallower is expanded to `DECODER_FAST_TABLELOG`, so the deepest table this
/// can be asked to hold is `1 << TABLELOG_MAX` entries — the same bound
/// `DTABLE_X1_SIZE` carries, over entries twice as wide.
const DTABLE_X2_SIZE: usize = 1 << TABLELOG_MAX;

#[derive(Clone, Copy)]
pub(crate) struct DTableX2 {
    table_log: u8,
    entries: [DEltX2; DTABLE_X2_SIZE],
}

impl Default for DTableX2 {
    fn default() -> Self {
        Self {
            table_log: 0,
            entries: [DEltX2::default(); DTABLE_X2_SIZE],
        }
    }
}

/// Build one entry.
///
/// At level 1 the entry emits `symbol` alone and `base_symbol` is unused; at
/// level 2 it emits `base_symbol` then `symbol`.
#[inline]
fn build_delt_x2(symbol: u8, nb_bits: u32, base_symbol: u8, level: u8) -> DEltX2 {
    debug_assert!(level == 1 || level == 2);
    let sequence = if level == 1 {
        symbol as u16
    } else {
        base_symbol as u16 | ((symbol as u16) << 8)
    };
    DEltX2 {
        sequence,
        nb_bits: nb_bits as u8,
        length: level,
    }
}

/// Fill the run of table slots owned by every symbol in `symbols`.
///
/// Each symbol codes in `nb_bits` and therefore owns `1 << (table_log -
/// nb_bits)` consecutive slots, which all decode to the same entry.
fn fill_dtable_x2_for_weight(
    slots: &mut [DEltX2],
    symbols: &[u8],
    nb_bits: u32,
    table_log: u32,
    base_symbol: u8,
    level: u8,
) -> Result<()> {
    // `nb_bits <= table_log` follows from the `min_weight` floor its callers
    // apply, but the arithmetic that establishes it is three functions away, so
    // it is checked rather than assumed.
    let shift = table_log.checked_sub(nb_bits).ok_or(Error::Corruption(
        "Huff0 X2 symbol is deeper than its table",
    ))?;
    let length = 1usize << shift;
    let mut at = 0usize;
    for &symbol in symbols {
        let entry = build_delt_x2(symbol, nb_bits, base_symbol, level);
        let end = at
            .checked_add(length)
            .ok_or(Error::Corruption("Huff0 X2 rank overflows the table"))?;
        slots
            .get_mut(at..end)
            .ok_or(Error::Corruption("Huff0 X2 rank runs past the table"))?
            .fill(entry);
        at = end;
    }
    Ok(())
}

/// Fill the sub-table reached after `base_symbol` has been decoded.
///
/// `consumed_bits` is what `base_symbol` cost, so `table_log - consumed_bits`
/// bits are left to spend on a second symbol.
#[allow(clippy::too_many_arguments)]
fn fill_dtable_x2_level2(
    sub_table: &mut [DEltX2],
    table_log: u32,
    consumed_bits: u32,
    rank_val: &[u32; TABLELOG_MAX + 1],
    min_weight: usize,
    weight_end: usize,
    sorted_symbols: &[u8],
    rank_start: &[u32; TABLELOG_MAX + 2],
    nb_bits_baseline: u32,
    base_symbol: u8,
) -> Result<()> {
    // Prefixes too short to admit any second symbol decode to `base_symbol`
    // alone. They sit at the front because the sub-table is ordered by the
    // second symbol's weight, and light weights code in the most bits.
    if min_weight > 1 {
        let skip = *rank_val
            .get(min_weight)
            .ok_or(Error::Corruption("Huff0 X2 weight exceeds the table log"))?
            as usize;
        let entry = build_delt_x2(base_symbol, consumed_bits, 0, 1);
        sub_table
            .get_mut(..skip)
            .ok_or(Error::Corruption("Huff0 X2 single-symbol run overflows"))?
            .fill(entry);
    }

    for weight in min_weight..weight_end {
        let begin = rank_start[weight] as usize;
        let end = rank_start[weight + 1] as usize;
        let nb_bits = nb_bits_baseline - weight as u32;
        let at = rank_val[weight] as usize;
        fill_dtable_x2_for_weight(
            sub_table
                .get_mut(at..)
                .ok_or(Error::Corruption("Huff0 X2 rank starts past the table"))?,
            sorted_symbols
                .get(begin..end)
                .ok_or(Error::Corruption("Huff0 X2 rank bounds are inconsistent"))?,
            nb_bits + consumed_bits,
            table_log,
            base_symbol,
            2,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn fill_dtable_x2(
    entries: &mut [DEltX2],
    table_log: u32,
    sorted_symbols: &[u8],
    rank_start: &[u32; TABLELOG_MAX + 2],
    rank_val: &[[u32; TABLELOG_MAX + 1]; TABLELOG_MAX],
    max_weight: usize,
    nb_bits_baseline: u32,
) -> Result<()> {
    let scale_log = nb_bits_baseline as i32 - table_log as i32;
    let min_bits = nb_bits_baseline - max_weight as u32;

    for weight in 1..=max_weight {
        let begin = rank_start[weight] as usize;
        let end = rank_start[weight + 1] as usize;
        let nb_bits = nb_bits_baseline - weight as u32;
        let symbols = sorted_symbols
            .get(begin..end)
            .ok_or(Error::Corruption("Huff0 X2 rank bounds are inconsistent"))?;
        let at = rank_val[0][weight] as usize;

        // `min_bits` is what the cheapest symbol in the alphabet costs. If that
        // still fits in the bits this symbol leaves behind, every slot it owns
        // becomes a sub-table holding a second symbol; otherwise the symbol
        // decodes alone and the slots are filled flat.
        if table_log - nb_bits >= min_bits {
            let length = 1usize << (table_log - nb_bits);
            let min_weight = (nb_bits as i32 + scale_log).max(1) as usize;
            let mut start = at;
            for &symbol in symbols {
                let sub_end = start
                    .checked_add(length)
                    .ok_or(Error::Corruption("Huff0 X2 rank overflows the table"))?;
                fill_dtable_x2_level2(
                    entries
                        .get_mut(start..sub_end)
                        .ok_or(Error::Corruption("Huff0 X2 rank runs past the table"))?,
                    table_log,
                    nb_bits,
                    &rank_val[nb_bits as usize],
                    min_weight,
                    max_weight + 1,
                    sorted_symbols,
                    rank_start,
                    nb_bits_baseline,
                    symbol,
                )?;
                start = sub_end;
            }
        } else {
            fill_dtable_x2_for_weight(
                entries
                    .get_mut(at..)
                    .ok_or(Error::Corruption("Huff0 X2 rank starts past the table"))?,
                symbols,
                nb_bits,
                table_log,
                0,
                1,
            )?;
        }
    }
    Ok(())
}

pub(crate) fn read_dtable_x2(src: &[u8], dtable: &mut DTableX2) -> Result<usize> {
    let mut rank_stats = [0u32; TABLELOG_MAX + 1];
    let mut huff_weight = [0u8; SYMBOLVALUE_MAX + 1];
    let mut table_log = 0u32;
    let mut nb_symbols = 0u32;
    let i_size = read_stats(
        &mut huff_weight,
        &mut rank_stats,
        &mut nb_symbols,
        &mut table_log,
        src,
    )?;
    if table_log as usize > TABLELOG_MAX {
        return Err(Error::Corruption(
            "Huff0 table log exceeds decode table capacity",
        ));
    }

    // A description shallower than the fast depth is expanded to it, which is
    // what leaves room for a second symbol in most entries. A description at
    // the maximum depth is built at its own depth and gains far less, which the
    // decoder selector accounts for by rarely picking this shape for it.
    let target_log = if (table_log as usize) <= DECODER_FAST_TABLELOG {
        DECODER_FAST_TABLELOG as u32
    } else {
        TABLELOG_MAX as u32
    };
    dtable.table_log = target_log as u8;

    // `read_stats` rejects a description whose weight-1 count is below 2, so
    // some rank at or below `table_log` is always populated and this walk stops
    // at 1 at the latest.
    let mut max_weight = table_log as usize;
    while rank_stats[max_weight] == 0 {
        max_weight -= 1;
    }

    // `rank_start[w]` is where weight-`w` symbols begin in `sorted_symbols`.
    let mut rank_start = [0u32; TABLELOG_MAX + 2];
    let mut next_start = 0u32;
    for weight in 1..=max_weight {
        rank_start[weight] = next_start;
        next_start += rank_stats[weight];
    }
    rank_start[max_weight + 1] = next_start;

    // Sort by weight. Weight 0 means the symbol is absent from the alphabet;
    // those go past every real rank, where nothing reads them again.
    let mut sorted_symbols = [0u8; SYMBOLVALUE_MAX + 1];
    let mut cursor = rank_start;
    let mut absent_cursor = next_start;
    for symbol in 0..nb_symbols as usize {
        let weight = huff_weight[symbol] as usize;
        let slot = if weight == 0 {
            &mut absent_cursor
        } else {
            &mut cursor[weight]
        };
        sorted_symbols[*slot as usize] = symbol as u8;
        *slot += 1;
    }

    // `rank_val[consumed][w]` is where weight-`w` symbols start in a table
    // reached after `consumed` bits have already been spent. Row 0 is the
    // top-level table; each deeper row is that row shifted down, because
    // spending a bit halves the span every remaining symbol occupies.
    let mut rank_val = [[0u32; TABLELOG_MAX + 1]; TABLELOG_MAX];
    let rescale = target_log as i32 - table_log as i32 - 1;
    let mut next_val = 0u32;
    for weight in 1..=max_weight {
        rank_val[0][weight] = next_val;
        next_val += rank_stats[weight] << (weight as i32 + rescale) as u32;
    }
    // The weights sum to a full tree — `read_stats` rejects anything else — so
    // the ranks tile the table exactly. Everything below indexes off that, so
    // it is confirmed here rather than trusted at each use.
    if next_val as usize != 1usize << target_log {
        return Err(Error::Corruption("Huff0 X2 ranks do not tile the table"));
    }

    let min_bits = table_log + 1 - max_weight as u32;
    debug_assert!(min_bits <= target_log);
    for consumed in min_bits..=(target_log - min_bits) {
        for weight in 1..=max_weight {
            rank_val[consumed as usize][weight] = rank_val[0][weight] >> consumed;
        }
    }

    fill_dtable_x2(
        &mut dtable.entries[..1usize << target_log],
        target_log,
        &sorted_symbols,
        &rank_start,
        &rank_val,
        max_weight,
        table_log + 1,
    )?;
    Ok(i_size)
}

/// Decode one entry, emitting one or two bytes and returning how many.
///
/// Always writes two bytes: the second is garbage when `length` is 1, and the
/// return value is what keeps it from being counted. Callers must therefore
/// leave two bytes of room even for the one-byte case, which is why every loop
/// below stops two bytes short of its end and hands the last position to
/// `decode_last_symbol_x2`.
///
/// # Safety
///
/// `pos + 2 <= dst.len()` must hold.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn decode_symbol_x2(
    dst: &mut [u8],
    pos: usize,
    bit_d: &mut BitDStream,
    table: &[DEltX2],
    table_log: u32,
) -> usize {
    let value = bit_d.look_bits_fast(table_log) as usize;
    // Safety: `look_bits_fast(table_log)` returns at most `(1 << table_log) - 1`
    // and `table` is sliced to exactly `1 << table_log` entries by every caller,
    // so `value < table.len()`.
    debug_assert!(value < table.len());
    let entry = unsafe { *table.get_unchecked(value) };
    // Safety: the caller guarantees two writable bytes at `pos`. This is a
    // two-byte store rather than a `copy_from_slice`, which LLVM would not
    // reliably narrow to one: keeping the bounds check here cost more than the
    // whole double-symbol win on `log-lines`.
    unsafe {
        core::ptr::write_unaligned(
            dst.as_mut_ptr().add(pos) as *mut u16,
            entry.sequence.to_le(),
        );
    }
    bit_d.skip_bits(entry.nb_bits as u32);
    entry.length as usize
}

/// Decode the final byte of a stream, where only one byte of room is left.
#[inline(always)]
fn decode_last_symbol_x2(
    dst: &mut [u8],
    pos: usize,
    bit_d: &mut BitDStream,
    table: &[DEltX2],
    table_log: u32,
) -> usize {
    let value = bit_d.look_bits_fast(table_log) as usize;
    debug_assert!(value < table.len());
    let entry = table[value & (table.len() - 1)];
    dst[pos] = entry.sequence as u8;
    if entry.length == 1 {
        bit_d.skip_bits(entry.nb_bits as u32);
    } else if bit_d.bits_consumed < CONTAINER_BITS {
        // Only the first of the pair is emitted, so the entry's bit cost
        // overstates what was consumed and can push the reader past the end of
        // its container. C pins it at the boundary instead of unwinding,
        // because the split cost of a pair cannot be recovered from the entry —
        // and it is sound only because no symbol follows this one.
        bit_d.skip_bits(entry.nb_bits as u32);
        if bit_d.bits_consumed > CONTAINER_BITS {
            bit_d.bits_consumed = CONTAINER_BITS;
        }
    }
    1
}

#[allow(unsafe_code)]
fn decode_stream_x2(
    dst: &mut [u8],
    mut pos: usize,
    bit_d: &mut BitDStream,
    end: usize,
    table: &[DEltX2],
    table_log: u32,
) -> usize {
    // Every `decode_symbol_x2` below is bounded against `end`, so this is the
    // one place that has to tie `end` back to the buffer. Asserted rather than
    // debug-asserted: it is the whole safety argument, and it costs one
    // comparison per stream against millions of symbols.
    assert!(pos <= end && end <= dst.len());
    let width = CONTAINER_BYTES;

    if end - pos >= width {
        if table_log <= DECODER_FAST_TABLELOG as u32 {
            // Five entries is up to ten bytes, and a table this shallow cannot
            // spend more than 55 bits on them, so one reload covers the group.
            // Safety: the guard leaves ten bytes, and five entries advance at
            // most eight, so the last write starts no later than `end - 2`.
            while bit_d.reload() == BitDStreamStatus::Unfinished && pos + 10 <= end {
                unsafe {
                    pos += decode_symbol_x2(dst, pos, bit_d, table, table_log);
                    pos += decode_symbol_x2(dst, pos, bit_d, table, table_log);
                    pos += decode_symbol_x2(dst, pos, bit_d, table, table_log);
                    pos += decode_symbol_x2(dst, pos, bit_d, table, table_log);
                    pos += decode_symbol_x2(dst, pos, bit_d, table, table_log);
                }
            }
        } else {
            // Safety: the guard leaves `width` bytes; four entries advance at
            // most `width`, so the last write starts no later than `end - 2`.
            while bit_d.reload() == BitDStreamStatus::Unfinished && pos + width <= end {
                unsafe {
                    pos += decode_symbol_x2(dst, pos, bit_d, table, table_log);
                    pos += decode_symbol_x2(dst, pos, bit_d, table, table_log);
                    pos += decode_symbol_x2(dst, pos, bit_d, table, table_log);
                    pos += decode_symbol_x2(dst, pos, bit_d, table, table_log);
                }
            }
        }
    } else {
        bit_d.reload();
    }

    // Safety for both loops: the guard is exactly the two bytes a write needs.
    if end - pos >= 2 {
        while bit_d.reload() == BitDStreamStatus::Unfinished && pos + 2 <= end {
            pos += unsafe { decode_symbol_x2(dst, pos, bit_d, table, table_log) };
        }
        while pos + 2 <= end {
            pos += unsafe { decode_symbol_x2(dst, pos, bit_d, table, table_log) };
        }
    }

    if pos < end {
        pos += decode_last_symbol_x2(dst, pos, bit_d, table, table_log);
    }
    pos
}

fn decompress_1x2_using_dtable(dst: &mut [u8], src: &[u8], dtable: &DTableX2) -> Result<usize> {
    let mut bit_d = BitDStream::new(src)?;
    let table_log = dtable.table_log as u32;
    let table = &dtable.entries[..1usize << table_log];
    let written = decode_stream_x2(dst, 0, &mut bit_d, dst.len(), table, table_log);
    if written != dst.len() || !bit_d.end_of_stream() {
        return Err(Error::Corruption("Huff0 stream did not terminate cleanly"));
    }
    Ok(written)
}

fn decompress_4x2_using_dtable(dst: &mut [u8], src: &[u8], dtable: &DTableX2) -> Result<usize> {
    if src.len() < 10 {
        return Err(Error::Corruption("truncated Huff0 4X stream"));
    }
    if dst.len() < 6 {
        return Err(Error::Corruption("Huff0 4X output too small to split"));
    }
    let length1 = read_segment_length(src, 0);
    let length2 = read_segment_length(src, 2);
    let length3 = read_segment_length(src, 4);
    let length4 = src.len().wrapping_sub(length1 + length2 + length3 + 6);
    if length4 > src.len() {
        return Err(Error::Corruption("invalid Huff0 4X segment lengths"));
    }

    let istart1 = 6;
    let istart2 = istart1 + length1;
    let istart3 = istart2 + length2;
    let istart4 = istart3 + length3;
    let segment_size = dst.len().div_ceil(4);

    let mut bit_d1 = BitDStream::new(&src[istart1..istart2])?;
    let mut bit_d2 = BitDStream::new(&src[istart2..istart3])?;
    let mut bit_d3 = BitDStream::new(&src[istart3..istart4])?;
    let mut bit_d4 = BitDStream::new(&src[istart4..])?;

    let op_start2 = segment_size;
    let op_start3 = op_start2 + segment_size;
    let op_start4 = op_start3 + segment_size;
    if op_start4 > dst.len() {
        return Err(Error::Corruption("Huff0 4X segment starts past the output"));
    }
    let mut op1 = 0usize;
    let mut op2 = op_start2;
    let mut op3 = op_start3;
    let mut op4 = op_start4;
    let mut end_signal = true;
    let table_log = dtable.table_log as u32;
    let table = &dtable.entries[..1usize << table_log];
    let width = CONTAINER_BYTES;

    // C bounds this loop on `op4` alone. That is sound for the single-symbol
    // decoder, where every entry advances all four cursors by exactly one byte
    // and they stay in lockstep, but an X2 entry advances by one byte or two,
    // so the cursors drift apart and `op4` says nothing about where `op1` is.
    // Each cursor is bounded against the end of the segment it owns instead.
    // Leaving the loop early is not a behavior change: the per-stream tails
    // below resume from wherever a cursor stopped and emit the same bytes.
    //
    // Safety for every `decode_symbol_x2` in the body: each guard leaves
    // `width` bytes before that cursor's segment end, and the four entries
    // decoded from a stream per iteration advance it by at most `width`, so the
    // last write starts no later than two bytes before that end. The segment
    // ends themselves are ordered `op_start2 <= op_start3 <= op_start4 <=
    // dst.len()`, the last by the check above.
    #[allow(unsafe_code)]
    while end_signal
        && op1 + width <= op_start2
        && op2 + width <= op_start3
        && op3 + width <= op_start4
        && op4 + width <= dst.len()
    {
        unsafe {
            op1 += decode_symbol_x2(dst, op1, &mut bit_d1, table, table_log);
            op2 += decode_symbol_x2(dst, op2, &mut bit_d2, table, table_log);
            op3 += decode_symbol_x2(dst, op3, &mut bit_d3, table, table_log);
            op4 += decode_symbol_x2(dst, op4, &mut bit_d4, table, table_log);
            op1 += decode_symbol_x2(dst, op1, &mut bit_d1, table, table_log);
            op2 += decode_symbol_x2(dst, op2, &mut bit_d2, table, table_log);
            op3 += decode_symbol_x2(dst, op3, &mut bit_d3, table, table_log);
            op4 += decode_symbol_x2(dst, op4, &mut bit_d4, table, table_log);
            op1 += decode_symbol_x2(dst, op1, &mut bit_d1, table, table_log);
            op2 += decode_symbol_x2(dst, op2, &mut bit_d2, table, table_log);
            op3 += decode_symbol_x2(dst, op3, &mut bit_d3, table, table_log);
            op4 += decode_symbol_x2(dst, op4, &mut bit_d4, table, table_log);
            op1 += decode_symbol_x2(dst, op1, &mut bit_d1, table, table_log);
            op2 += decode_symbol_x2(dst, op2, &mut bit_d2, table, table_log);
            op3 += decode_symbol_x2(dst, op3, &mut bit_d3, table, table_log);
            op4 += decode_symbol_x2(dst, op4, &mut bit_d4, table, table_log);
        }

        end_signal &= bit_d1.reload_fast() == BitDStreamStatus::Unfinished;
        end_signal &= bit_d2.reload_fast() == BitDStreamStatus::Unfinished;
        end_signal &= bit_d3.reload_fast() == BitDStreamStatus::Unfinished;
        end_signal &= bit_d4.reload_fast() == BitDStreamStatus::Unfinished;
    }

    // Implied by the per-stream bounds above, and kept because it is the
    // precondition `decode_stream_x2` needs from each cursor.
    if op1 > op_start2 || op2 > op_start3 || op3 > op_start4 {
        return Err(Error::Corruption("Huff0 segment overrun"));
    }

    op1 = decode_stream_x2(dst, op1, &mut bit_d1, op_start2, table, table_log);
    op2 = decode_stream_x2(dst, op2, &mut bit_d2, op_start3, table, table_log);
    op3 = decode_stream_x2(dst, op3, &mut bit_d3, op_start4, table, table_log);
    op4 = decode_stream_x2(dst, op4, &mut bit_d4, dst.len(), table, table_log);

    debug_assert_eq!(op1, op_start2);
    debug_assert_eq!(op2, op_start3);
    debug_assert_eq!(op3, op_start4);
    debug_assert_eq!(op4, dst.len());

    if !bit_d1.end_of_stream()
        || !bit_d2.end_of_stream()
        || !bit_d3.end_of_stream()
        || !bit_d4.end_of_stream()
    {
        return Err(Error::Corruption("Huff0 stream did not consume all bits"));
    }
    Ok(dst.len())
}

/// Cost model deciding which decoder shape to build, matching C's
/// `HUF_selectDecoder`.
///
/// Each row is one sixteenth of compression ratio, and each pair is a fixed
/// table-build cost plus a per-256-bytes decode cost. The double-symbol table
/// is dearer to build and cheaper to run, so it wins on large, well-compressed
/// literals and loses on small or barely-compressed ones.
pub(crate) fn prefers_double_symbol_decoder(
    regenerated_size: usize,
    compressed_size: usize,
) -> bool {
    /// `(table build, decode per 256 bytes)` for the single- then
    /// double-symbol decoder, indexed by compression ratio in sixteenths.
    const ALGO_TIME: [[(u64, u64); 2]; 16] = [
        [(0, 0), (1, 1)],
        [(0, 0), (1, 1)],
        [(150, 216), (381, 119)],
        [(170, 205), (514, 112)],
        [(177, 199), (539, 110)],
        [(197, 194), (644, 107)],
        [(221, 192), (735, 107)],
        [(256, 189), (881, 106)],
        [(359, 188), (1167, 109)],
        [(582, 187), (1570, 114)],
        [(688, 187), (1712, 122)],
        [(825, 186), (1965, 136)],
        [(976, 185), (2131, 150)],
        [(1180, 186), (2070, 175)],
        [(1377, 185), (1731, 202)],
        [(1412, 185), (1695, 202)],
    ];

    if regenerated_size == 0 {
        return false;
    }
    let quantized = if compressed_size >= regenerated_size {
        15
    } else {
        compressed_size * 16 / regenerated_size
    };
    let per_256 = (regenerated_size >> 8) as u64;
    let [(single_build, single_rate), (double_build, double_rate)] = ALGO_TIME[quantized];
    let single = single_build + single_rate * per_256;
    let double = double_build + double_rate * per_256;
    // A small handicap for the table that occupies twice the cache.
    double + (double >> 5) < single
}

/// A built Huffman decode table, in whichever shape it was read into.
///
/// C keeps one buffer and reinterprets its bytes according to a type tag; this
/// holds the shape itself. Switching shapes therefore replaces the whole value,
/// which only happens when the selector's answer changes between blocks — the
/// common case reads a new description into the shape already there.
///
/// The variants differ by 8 KiB, so a single-symbol table stored here wastes
/// that much. Clippy's fix for the gap is to box the larger variant, which is
/// the wrong trade twice over: it puts a heap allocation and a pointer chase on
/// the decode path, and it gives up the in-place construction the whole design
/// is for. Holding both shapes side by side instead would cost 24 KiB in every
/// decoder rather than 16 KiB, and this value is created per frame.
#[derive(Clone, Copy)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum HuffmanDTable {
    Single(DTableX1),
    Double(DTableX2),
}

impl HuffmanDTable {
    /// The single-symbol table, re-tagging `slot` if it holds the other shape.
    pub(crate) fn single_slot(slot: &mut Option<Self>) -> &mut DTableX1 {
        if !matches!(slot, Some(Self::Single(_))) {
            *slot = Some(Self::Single(DTableX1::default()));
        }
        match slot {
            Some(Self::Single(table)) => table,
            _ => unreachable!("just installed"),
        }
    }

    /// The double-symbol table, re-tagging `slot` if it holds the other shape.
    pub(crate) fn double_slot(slot: &mut Option<Self>) -> &mut DTableX2 {
        if !matches!(slot, Some(Self::Double(_))) {
            *slot = Some(Self::Double(DTableX2::default()));
        }
        match slot {
            Some(Self::Double(table)) => table,
            _ => unreachable!("just installed"),
        }
    }
}

pub(crate) fn decompress_1x_using_dtable(
    dst: &mut [u8],
    src: &[u8],
    dtable: &HuffmanDTable,
) -> Result<usize> {
    match dtable {
        HuffmanDTable::Single(table) => decompress_1x1_using_dtable(dst, src, table),
        HuffmanDTable::Double(table) => decompress_1x2_using_dtable(dst, src, table),
    }
}

pub(crate) fn decompress_4x_using_dtable(
    dst: &mut [u8],
    src: &[u8],
    dtable: &HuffmanDTable,
) -> Result<usize> {
    match dtable {
        HuffmanDTable::Single(table) => decompress_4x1_using_dtable(dst, src, table),
        HuffmanDTable::Double(table) => decompress_4x2_using_dtable(dst, src, table),
    }
}

pub(crate) fn decompress_into(dst: &mut [u8], src: &[u8]) -> Result<usize> {
    if dst.is_empty() {
        return Err(Error::DstSizeTooSmall);
    }
    if src.len() > dst.len() {
        return Err(Error::Corruption("Huff0 input larger than output"));
    }
    if src.len() == dst.len() {
        dst.copy_from_slice(src);
        return Ok(dst.len());
    }
    if src.len() == 1 {
        dst.fill(src[0]);
        return Ok(dst.len());
    }

    // Mirrors C's `HUF_decompress4X_hufOnly_wksp`: pick the shape from the
    // ratio, then build and run that one.
    let mut dtable = None;
    let hsize = if prefers_double_symbol_decoder(dst.len(), src.len()) {
        read_dtable_x2(src, HuffmanDTable::double_slot(&mut dtable))?
    } else {
        read_dtable_x1(src, HuffmanDTable::single_slot(&mut dtable))?
    };
    if hsize >= src.len() {
        return Err(Error::SrcSizeWrong);
    }
    let dtable = dtable.expect("installed above");
    decompress_4x_using_dtable(dst, &src[hsize..], &dtable)
}

pub(crate) fn compress_bound(size: usize) -> usize {
    129 + size + (size >> 8) + 8
}

pub(crate) fn compress(src: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut dst = vec![0u8; compress_bound(src.len())];
    let written = compress_into(&mut dst, src)?;
    if written == 0 {
        return Ok(None);
    }
    dst.truncate(written);
    Ok(Some(dst))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamMode {
    Single,
    Four,
}

/// How hard to work at choosing the Huffman table's depth.
///
/// C keeps both routes in `HUF_optimalTableLog` and picks between them with
/// `HUF_flags_optimalDepth`, which `ZSTD_compressLiterals` sets only when
/// `strategy >= HUF_OPTIMAL_DEPTH_THRESHOLD` — that is, `ZSTD_btultra` and
/// above. Running the search at every level is not a free improvement: it is a
/// different table than upstream writes, so the frames stop matching even
/// where the search wins a byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TableDepth {
    /// `FSE_optimalTableLog_internal`: one closed-form guess from the input
    /// size and alphabet, no trees built.
    Estimated,
    /// Build the tree at every candidate depth and keep the smallest total of
    /// table description plus payload.
    Searched,
}

/// Whether the caller has reason to expect these literals will not compress.
///
/// C decides this in `ZSTD_entropyCompressSeqStore_internal` from the ratio of
/// literal bytes to sequences and passes it down as
/// `HUF_flags_suspectUncompressible`. `HUF_compress_internal` then counts a
/// sample from each end before committing to a pass over the whole block, which
/// is what stops incompressible input from paying a full histogram only to be
/// rejected by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Compressibility {
    /// Nothing suggests the literals are unusual; count every byte.
    Unknown,
    /// The parse produced few sequences for many literals; sample first.
    Suspect,
}

/// Bytes counted at each end of the block by the sampled check.
const SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE: usize = 4096;
/// How many times the sample the block must be before sampling is worth it.
const SUSPECT_INCOMPRESSIBLE_SAMPLE_RATIO: usize = 10;

/// C's sampled incompressibility check from `HUF_compress_internal`.
///
/// Counts the first and last 4 KiB and holds the sum of the two most common
/// symbol counts to the same threshold the full histogram applies to the whole
/// block. Uniformly distributed bytes put about `2 * 4096 / 256` in the largest
/// bin, far under the bound, so random input is turned away after 8 KiB instead
/// of after a count over every byte. Anything with a symbol worth coding
/// clears the bound easily, so the check costs a compressible block 8 KiB of
/// counting it would have done anyway.
fn looks_incompressible_from_sample(src: &[u8], count: &mut [u32; SYMBOLVALUE_MAX + 1]) -> bool {
    if src.len() < SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE * SUSPECT_INCOMPRESSIBLE_SAMPLE_RATIO {
        return false;
    }

    let mut largest_total = 0u32;
    for sample in [
        &src[..SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE],
        &src[src.len() - SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE..],
    ] {
        let mut max_symbol_value = SYMBOLVALUE_MAX as u32;
        largest_total += count_simple(count, &mut max_symbol_value, sample);
    }
    largest_total as usize <= ((2 * SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE) >> 7) + 4
}

#[inline(always)]
fn compress_using_ctable_mode(
    dst: &mut [u8],
    src: &[u8],
    ctable: &[CElt; SYMBOLVALUE_MAX + 1],
    table_log: u32,
    stream_mode: StreamMode,
) -> usize {
    match stream_mode {
        StreamMode::Single => compress_1x_using_ctable(dst, src, ctable, table_log),
        StreamMode::Four => compress_4x_using_ctable(dst, src, ctable, table_log),
    }
}

pub(crate) fn compress_with_table(src: &[u8], ctable: &CTableX1) -> Result<Option<Vec<u8>>> {
    compress_with_table_mode(src, ctable, StreamMode::Four)
}

pub(crate) fn compress_with_table_mode(
    src: &[u8],
    ctable: &CTableX1,
    stream_mode: StreamMode,
) -> Result<Option<Vec<u8>>> {
    if src.is_empty() || !table_supports_all_symbols(ctable, src) {
        return Ok(None);
    }

    let mut dst = vec![0u8; src.len() + BLOCKBOUND_MIN];
    let written = compress_using_ctable_mode(
        &mut dst,
        src,
        &ctable.entries,
        ctable.table_log as u32,
        stream_mode,
    );
    if written == 0 || written >= src.len().saturating_sub(1) {
        return Ok(None);
    }
    dst.truncate(written);
    Ok(Some(dst))
}

pub(crate) fn compress_with_table_into(
    dst: &mut [u8],
    src: &[u8],
    ctable: &CTableX1,
) -> Result<usize> {
    compress_with_table_into_mode(dst, src, ctable, StreamMode::Four)
}

pub(crate) fn compress_with_table_into_mode(
    dst: &mut [u8],
    src: &[u8],
    ctable: &CTableX1,
    stream_mode: StreamMode,
) -> Result<usize> {
    if src.is_empty() || !table_supports_all_symbols(ctable, src) {
        return Ok(0);
    }
    if matches!(stream_mode, StreamMode::Four) && dst.len() < BLOCKBOUND_MIN {
        return Ok(0);
    }

    let written = compress_using_ctable_mode(
        dst,
        src,
        &ctable.entries,
        ctable.table_log as u32,
        stream_mode,
    );
    if written == 0 || written >= src.len().saturating_sub(1) {
        return Ok(0);
    }
    Ok(written)
}

#[derive(Clone, Copy)]
pub(crate) struct PreferredTableCompression {
    pub(crate) written: usize,
    pub(crate) reused_table: bool,
    pub(crate) table: CTableX1,
}

pub(crate) fn compress_prefer_existing_table_into(
    dst: &mut [u8],
    src: &[u8],
    previous: Option<&CTableX1>,
) -> Result<Option<PreferredTableCompression>> {
    let mut workspace = CompressWorkspace::default();
    compress_prefer_existing_table_into_mode(
        dst,
        src,
        previous,
        StreamMode::Four,
        TableDepth::Estimated,
        Compressibility::Unknown,
        &mut workspace,
    )
}

pub(crate) fn compress_prefer_existing_table_into_mode(
    dst: &mut [u8],
    src: &[u8],
    previous: Option<&CTableX1>,
    stream_mode: StreamMode,
    table_depth: TableDepth,
    compressibility: Compressibility,
    workspace: &mut CompressWorkspace,
) -> Result<Option<PreferredTableCompression>> {
    if src.is_empty() || dst.is_empty() {
        return Ok(None);
    }
    if src.len() > BLOCKSIZE_MAX {
        return Err(Error::SrcSizeWrong);
    }

    if compressibility == Compressibility::Suspect
        && looks_incompressible_from_sample(src, &mut workspace.count)
    {
        return Ok(None);
    }

    let mut max_symbol_value = SYMBOLVALUE_MAX as u32;
    let mut huff_log = TABLELOG_DEFAULT as u32;
    let largest = count_wksp(
        &mut workspace.count,
        &mut max_symbol_value,
        src,
        &mut workspace.hist,
    )?;
    if largest as usize == src.len() || largest as usize <= (src.len() >> 7) + 4 {
        return Ok(None);
    }

    huff_log = match table_depth {
        TableDepth::Estimated => optimal_table_log(huff_log, src.len(), max_symbol_value),
        TableDepth::Searched => {
            optimal_table_log_search(huff_log, &workspace.count, max_symbol_value)
        }
    };
    huff_log = build_ctable(
        &mut workspace.ctable,
        &workspace.count,
        max_symbol_value,
        huff_log,
        &mut workspace.build,
    )?;
    workspace.ctable[(max_symbol_value as usize + 1)..].fill(CElt::default());
    let new_table = CTableX1 {
        table_log: huff_log as u8,
        entries: workspace.ctable,
    };

    let header_size = match write_ctable(
        dst,
        &workspace.ctable,
        max_symbol_value,
        huff_log,
        &mut workspace.weights,
    ) {
        Ok(size) => Some(size),
        Err(Error::Generic) => None,
        Err(err) => return Err(err),
    };
    let previous = previous
        .filter(|table| table_covers_counted_symbols(table, &workspace.count, max_symbol_value));

    if let Some(previous) = previous {
        let old_size =
            estimate_compressed_size(&previous.entries, &workspace.count, max_symbol_value);
        let new_size =
            estimate_compressed_size(&workspace.ctable, &workspace.count, max_symbol_value);
        let previous_looks_better = match header_size {
            Some(header_size) => {
                old_size <= header_size + new_size || header_size + 12 >= src.len()
            }
            None => true,
        };
        if previous_looks_better {
            let written = compress_using_ctable_mode(
                dst,
                src,
                &previous.entries,
                previous.table_log as u32,
                stream_mode,
            );
            if written != 0 && written < src.len().saturating_sub(1) {
                return Ok(Some(PreferredTableCompression {
                    written,
                    reused_table: true,
                    table: *previous,
                }));
            }
        }
    }

    let Some(header_size) = header_size else {
        return Ok(None);
    };
    if header_size + 12 >= src.len() {
        return Ok(None);
    }

    let compressed_size = compress_using_ctable_mode(
        &mut dst[header_size..],
        src,
        &workspace.ctable,
        huff_log,
        stream_mode,
    );
    if compressed_size == 0 {
        return Ok(None);
    }
    let total = header_size + compressed_size;
    if total >= src.len().saturating_sub(1) {
        return Ok(None);
    }
    Ok(Some(PreferredTableCompression {
        written: total,
        reused_table: false,
        table: new_table,
    }))
}

/// Estimate the encoded size (in bytes) of a Huffman-compressed literal section,
/// including the tree header. Returns `None` if the data is incompressible (RLE,
/// too few symbols, etc.), meaning raw encoding should be assumed.
pub(crate) fn estimate_literal_section_bytes(src: &[u8]) -> Option<usize> {
    if src.is_empty() {
        return Some(0);
    }
    let mut workspace = CompressWorkspace::default();
    let mut max_symbol_value = SYMBOLVALUE_MAX as u32;
    let largest = count_wksp(
        &mut workspace.count,
        &mut max_symbol_value,
        src,
        &mut workspace.hist,
    )
    .ok()?;
    // RLE: 1 byte header
    if largest as usize == src.len() {
        return Some(1);
    }
    // Incompressible: raw
    if largest as usize <= (src.len() >> 7) + 4 {
        return None;
    }
    let huff_log = optimal_table_log(TABLELOG_DEFAULT as u32, src.len(), max_symbol_value);
    let huff_log = build_ctable(
        &mut workspace.ctable,
        &workspace.count,
        max_symbol_value,
        huff_log,
        &mut workspace.build,
    )
    .ok()?;
    // Header size
    let header_size = write_ctable(
        &mut [0u8; 256],
        &workspace.ctable,
        max_symbol_value,
        huff_log,
        &mut workspace.weights,
    )
    .ok()?;
    // Data cost
    let data_size = estimate_compressed_size(&workspace.ctable, &workspace.count, max_symbol_value);
    // Jump table for 4-stream mode (6 bytes for >= 256 literals)
    let jump_table = if src.len() >= 256 { 6 } else { 0 };
    Some(header_size + data_size + jump_table)
}

/// Like [`estimate_literal_section_bytes`], but also considers reusing a previous
/// block's Huffman table (repeat mode). Matches C's `ZSTD_buildBlockEntropyStats_literals`
/// + `ZSTD_estimateBlockSize_literal` pipeline used during block-split cost estimation.
///
/// When repeat mode is chosen (previous table produces better results), the Huffman
/// description header is omitted from the estimate (zero cost), matching C's
/// `writeLitEntropy = (hType == set_compressed)` logic.
///
/// When `previous` is `None` or invalid, behaves identically to
/// [`estimate_literal_section_bytes`].
pub(crate) fn estimate_literal_section_bytes_with_repeat(
    src: &[u8],
    previous: Option<&CTableX1>,
) -> Option<usize> {
    if src.is_empty() {
        return Some(0);
    }
    let mut workspace = CompressWorkspace::default();
    let mut max_symbol_value = SYMBOLVALUE_MAX as u32;
    let largest = count_wksp(
        &mut workspace.count,
        &mut max_symbol_value,
        src,
        &mut workspace.hist,
    )
    .ok()?;
    // RLE: 1 byte
    if largest as usize == src.len() {
        return Some(1);
    }
    // Incompressible: raw
    if largest as usize <= (src.len() >> 7) + 4 {
        return None;
    }
    // Validate previous table supports all symbols present in the data
    let previous = previous.filter(|table| {
        table.table_log > 0
            && validate_ctable_counts(&table.entries, &workspace.count, max_symbol_value)
    });
    // Build new Huffman table
    let huff_log = optimal_table_log(TABLELOG_DEFAULT as u32, src.len(), max_symbol_value);
    let huff_log = build_ctable(
        &mut workspace.ctable,
        &workspace.count,
        max_symbol_value,
        huff_log,
        &mut workspace.build,
    )
    .ok()?;
    // Header size for new table
    let header_size = write_ctable(
        &mut [0u8; 256],
        &workspace.ctable,
        max_symbol_value,
        huff_log,
        &mut workspace.weights,
    )
    .ok()?;
    // Data cost with new table
    let new_data_size =
        estimate_compressed_size(&workspace.ctable, &workspace.count, max_symbol_value);
    let jump_table = if src.len() >= 256 { 6 } else { 0 };
    // Check repeat mode: can we reuse the previous table?
    // C: if (oldCSize < srcSize && (oldCSize <= hSize + newCSize || hSize + 12 >= srcSize))
    if let Some(prev) = previous {
        let old_data_size =
            estimate_compressed_size(&prev.entries, &workspace.count, max_symbol_value);
        if old_data_size < src.len()
            && (old_data_size <= header_size + new_data_size || header_size + 12 >= src.len())
        {
            // Repeat mode: no Huffman description header needed
            return Some(old_data_size + jump_table);
        }
    }
    Some(header_size + new_data_size + jump_table)
}

/// Check that a Huffman CTable can encode all symbols with non-zero counts.
fn validate_ctable_counts(
    ctable: &[CElt; SYMBOLVALUE_MAX + 1],
    count: &[u32; SYMBOLVALUE_MAX + 1],
    max_symbol_value: u32,
) -> bool {
    for symbol in 0..=max_symbol_value as usize {
        if count[symbol] > 0 && ctable[symbol].nb_bits == 0 {
            return false;
        }
    }
    true
}

pub(crate) fn compress_into(dst: &mut [u8], src: &[u8]) -> Result<usize> {
    Ok(compress_prefer_existing_table_into(dst, src, None)?.map_or(0, |result| result.written))
}

#[inline(always)]
fn encode_symbol(bit_c: &mut BitCStream<'_>, symbol: u8, ctable: &[CElt; SYMBOLVALUE_MAX + 1]) {
    let entry = ctable[symbol as usize];
    bit_c.add_bits_fast(entry.val.into(), entry.nb_bits as u32);
}

#[inline(always)]
fn flush_bits_1(bit_c: &mut BitCStream<'_>) {
    if CONTAINER_BITS < (TABLELOG_MAX * 2 + 7) as u32 {
        bit_c.flush_bits();
    }
}

#[inline(always)]
fn flush_bits_2(bit_c: &mut BitCStream<'_>) {
    if CONTAINER_BITS < (TABLELOG_MAX * 4 + 7) as u32 {
        bit_c.flush_bits();
    }
}

/// Huff0 1X compress entry point: builds a packed CTable and delegates.
fn compress_1x_using_ctable(
    dst: &mut [u8],
    src: &[u8],
    ctable: &[CElt; SYMBOLVALUE_MAX + 1],
    table_log: u32,
) -> usize {
    let mut packed = [0u64; SYMBOLVALUE_MAX + 1];
    for i in 0..=SYMBOLVALUE_MAX {
        let nb = ctable[i].nb_bits as u32;
        let val = ctable[i].val as u64;
        if nb > 0 {
            packed[i] = (val << (64 - nb)) | nb as u64;
        }
    }
    compress_1x_packed(dst, src, &packed, table_log)
}

/// Huff0 1X compress using C zstd's packed top-down bitstream.
///
/// Packed entry format (matches C's HUF_buildCElt):
///   bits [0, 4)            = nb_bits
///   bits [4, 64 - nb_bits) = 0
///   bits [64 - nb_bits, 64) = value
#[allow(unsafe_code)]
fn compress_1x_packed(
    dst: &mut [u8],
    src: &[u8],
    packed: &[u64; SYMBOLVALUE_MAX + 1],
    table_log: u32,
) -> usize {
    if dst.len() <= 8 || src.is_empty() {
        return 0;
    }

    // When the output buffer is large enough, skip the per-flush bounds
    // clamp (`min(end)`) — matching C's HUF_tightCompressBound check.
    let tight_bound = ((src.len() * table_log as usize) >> 3) + 8;
    if dst.len() < tight_bound || table_log > 11 {
        return compress_1x_loop::<4, false, false>(dst, src, packed);
    }

    // `K_UNROLL` is how many symbols accumulate in one container between
    // flushes, so it is bounded by the container: a flush leaves up to 7 bits
    // behind, and each symbol adds at most `table_log`, so
    // `7 + K_UNROLL * table_log <= 64`. `K_LAST_FAST` is bounded the same way
    // with the dirty low bits `add_fast` leaves in place, `ilog2(nb_bits) + 1`
    // of them, added on: it is only allowed where the group's worst case still
    // clears 64. Both are exactly C's `HUF_compress1X_usingCTable_internal_body`
    // dispatch, which is the reason the pairs look arbitrary — 11 gets a
    // slow last symbol where 10 does not because 7 + 5 * 11 + 4 is 66.
    match table_log {
        11 => compress_1x_loop::<5, false, true>(dst, src, packed),
        10 => compress_1x_loop::<5, true, true>(dst, src, packed),
        9 => compress_1x_loop::<6, false, true>(dst, src, packed),
        8 => compress_1x_loop::<7, false, true>(dst, src, packed),
        7 => compress_1x_loop::<8, false, true>(dst, src, packed),
        _ => compress_1x_loop::<9, true, true>(dst, src, packed),
    }
}

/// Core Huff0 encoding loop, parameterized by unroll factor, last-fast flag,
/// and fast-flush flag.
///
/// `K_UNROLL`: symbols per flush (more = fewer flushes but higher max bits)
/// `K_LAST_FAST`: if true, the last symbol before flush uses add_fast
///   instead of add_slow (safe when dirty bits can't reach the extraction zone)
/// `K_FAST_FLUSH`: if true, flush skips the `min(end)` bounds clamp
///
/// # Two containers
///
/// Appending a symbol is `c >>= nb_bits; c |= value`, a two-instruction
/// dependency on `c` that the next symbol cannot start until it retires. A
/// single container therefore runs at two cycles per symbol no matter how wide
/// the machine is: it is latency-bound, not throughput-bound.
///
/// The main loop below encodes `2 * K_UNROLL` symbols per iteration, the second
/// half into an independent container that starts at zero. Because the stream
/// is built downward from the top of the register, appending a group to a fresh
/// container and then shifting the first container right by the group's width
/// and OR-ing the two produces the identical bit pattern — see `merge_index1`.
/// The second group's chain has no dependency on the first, so it issues in the
/// first group's shadow, and the iteration costs `K_UNROLL + 1` shift/or pairs
/// instead of `2 * K_UNROLL`.
///
/// This is C's `HUF_compress1X_usingCTable_internal_body_loop` structure, down
/// to the `HUF_zeroIndex1`/`HUF_mergeIndex1` split.
#[allow(unsafe_code)]
#[inline(always)]
fn compress_1x_loop<const K_UNROLL: usize, const K_LAST_FAST: bool, const K_FAST_FLUSH: bool>(
    dst: &mut [u8],
    src: &[u8],
    packed: &[u64; SYMBOLVALUE_MAX + 1],
) -> usize {
    let packed_ptr = packed.as_ptr();
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let end_pos = dst.len() - 8;

    let mut c: u64 = 0;
    let mut bp: u64 = 0;
    let mut byte_pos: usize = 0;

    // --- Inline helpers matching C's HUF_addBits / HUF_flushBits ---

    /// Add symbol (fast): dirty nb_bits in low bits are harmless.
    /// Uses wrapping_shr because the packed entry has value bits beyond
    /// bit 5, but x86-64/ARM64 only read the low 6 bits of the shift
    /// amount — Rust's checked shift would panic in debug mode.
    ///
    /// # Safety
    ///
    /// `pos` must be a readable index of `sp`, and `pt` must have 256 entries
    /// — it is indexed by the symbol byte, so every value of that byte is
    /// reachable.
    #[inline(always)]
    unsafe fn add_fast(c: &mut u64, bp: &mut u64, pt: *const u64, sp: *const u8, pos: usize) {
        // SAFETY: `pos` is in bounds of `sp` by contract, and the byte it
        // yields is at most 255, which `pt`'s 256 entries cover.
        let elt = unsafe { *pt.add(*sp.add(pos) as usize) };
        *c = c.wrapping_shr(elt as u32);
        *c |= elt;
        *bp = bp.wrapping_add(elt);
    }

    /// Add symbol (slow): masks off low 8 bits for clean encoding.
    ///
    /// # Safety
    ///
    /// As [`add_fast`].
    #[inline(always)]
    unsafe fn add_slow(c: &mut u64, bp: &mut u64, pt: *const u64, sp: *const u8, pos: usize) {
        // SAFETY: as in `add_fast`.
        let elt = unsafe { *pt.add(*sp.add(pos) as usize) };
        let nb = elt & 0xFF;
        *c = c.wrapping_shr(nb as u32);
        *c |= elt & 0xFFFF_FFFF_FFFF_FF00;
        *bp = bp.wrapping_add(nb);
    }

    /// Add the last symbol before flush, using fast or slow path.
    ///
    /// # Safety
    ///
    /// As [`add_fast`]; this only dispatches.
    #[inline(always)]
    unsafe fn add_last<const FAST: bool>(
        c: &mut u64,
        bp: &mut u64,
        pt: *const u64,
        sp: *const u8,
        pos: usize,
    ) {
        // SAFETY: this function's contract is exactly the callee's.
        unsafe {
            if FAST {
                add_fast(c, bp, pt, sp, pos);
            } else {
                add_slow(c, bp, pt, sp, pos);
            }
        }
    }

    /// Flush with fast path (no bounds clamp) or safe path.
    ///
    /// # Safety
    ///
    /// `*byte_pos + 8` must be within the allocation `dp` points into. The
    /// write is a full 8-byte word regardless of `nb_bytes`, so the slack must
    /// be there even when only one byte is being committed.
    #[inline(always)]
    unsafe fn flush<const FAST: bool>(
        c: &u64,
        bp: &mut u64,
        dp: *mut u8,
        byte_pos: &mut usize,
        end: usize,
    ) {
        let nb_bits = (*bp & 0xFF) as u32;
        let nb_bytes = (nb_bits >> 3) as usize;
        let out = *c >> ((64u32.wrapping_sub(nb_bits)) & 63);
        // SAFETY: the caller guarantees eight writable bytes at `*byte_pos`.
        // Unaligned, so `dp` needs no alignment.
        unsafe { core::ptr::write_unaligned(dp.add(*byte_pos) as *mut u64, out.to_le()) };
        if FAST {
            *byte_pos += nb_bytes;
        } else {
            *byte_pos = (*byte_pos + nb_bytes).min(end);
        }
        *bp &= 7;
    }

    /// Fold the second container into the first.
    ///
    /// Both are filled from bit 63 downward, so the first container's occupied
    /// bits are its top `bp0 & 0xFF` and the second's are its top `bp1 & 0xFF`.
    /// Shifting the first right by the second's width and OR-ing puts the
    /// second group above the first, which is where it belongs: the loop walks
    /// the input backwards, so the second group's symbols precede the first
    /// group's in the source and must sit later in the reversed bitstream.
    ///
    /// `wrapping_shr` because `bp1` carries value-bit noise above bit 7, the
    /// same reason `add_fast` uses it. The masked width is under 64 for every
    /// `(K_UNROLL, table_log)` pair the caller dispatches.
    #[inline(always)]
    fn merge_index1(c0: &mut u64, bp0: &mut u64, c1: u64, bp1: u64) {
        debug_assert!((bp1 & 0xFF) < 64);
        *c0 = c0.wrapping_shr((bp1 & 0xFF) as u32);
        *c0 |= c1;
        *bp0 = bp0.wrapping_add(bp1);
    }

    let mut n = src.len() as isize;

    unsafe {
        // Handle remainder symbols so n becomes divisible by kUnroll.
        let rem = n % K_UNROLL as isize;
        if rem > 0 {
            for _i in 0..rem {
                n -= 1;
                add_slow(&mut c, &mut bp, packed_ptr, src_ptr, n as usize);
            }
            flush::<K_FAST_FLUSH>(&c, &mut bp, dst_ptr, &mut byte_pos, end_pos);
        }
        debug_assert!((n as usize).is_multiple_of(K_UNROLL));

        // One more group, so n becomes divisible by 2 * kUnroll and the main
        // loop below can always take two.
        if !(n as usize).is_multiple_of(2 * K_UNROLL) {
            for u in 1..K_UNROLL {
                add_fast(&mut c, &mut bp, packed_ptr, src_ptr, (n as usize) - u);
            }
            add_last::<K_LAST_FAST>(
                &mut c,
                &mut bp,
                packed_ptr,
                src_ptr,
                (n as usize) - K_UNROLL,
            );
            flush::<K_FAST_FLUSH>(&c, &mut bp, dst_ptr, &mut byte_pos, end_pos);
            n -= K_UNROLL as isize;
        }
        debug_assert!((n as usize).is_multiple_of(2 * K_UNROLL));

        // Main loop: 2 * kUnroll symbols per iteration, the second group
        // accumulated in its own container so its shift/or chain does not wait
        // on the first group's.
        while n > 0 {
            for u in 1..K_UNROLL {
                add_fast(&mut c, &mut bp, packed_ptr, src_ptr, (n as usize) - u);
            }
            add_last::<K_LAST_FAST>(
                &mut c,
                &mut bp,
                packed_ptr,
                src_ptr,
                (n as usize) - K_UNROLL,
            );
            flush::<K_FAST_FLUSH>(&c, &mut bp, dst_ptr, &mut byte_pos, end_pos);

            let mut c1: u64 = 0;
            let mut bp1: u64 = 0;
            for u in 1..K_UNROLL {
                add_fast(
                    &mut c1,
                    &mut bp1,
                    packed_ptr,
                    src_ptr,
                    (n as usize) - K_UNROLL - u,
                );
            }
            add_last::<K_LAST_FAST>(
                &mut c1,
                &mut bp1,
                packed_ptr,
                src_ptr,
                (n as usize) - 2 * K_UNROLL,
            );
            merge_index1(&mut c, &mut bp, c1, bp1);
            flush::<K_FAST_FLUSH>(&c, &mut bp, dst_ptr, &mut byte_pos, end_pos);

            n -= 2 * K_UNROLL as isize;
        }

        // Close: add end-mark (1 bit at MSB), final flush.
        c >>= 1u64;
        c |= 1u64 << 63;
        bp = bp.wrapping_add(1);
        flush::<false>(&c, &mut bp, dst_ptr, &mut byte_pos, end_pos);
    }

    if byte_pos >= end_pos {
        return 0;
    }
    byte_pos + usize::from((bp & 7) > 0)
}

#[inline(always)]
fn write_segment_length(dst: &mut [u8], offset: usize, value: usize) {
    debug_assert!(value <= u16::MAX as usize);
    dst[offset..offset + 2].copy_from_slice(&(value as u16).to_le_bytes());
}

fn compress_4x_using_ctable(
    dst: &mut [u8],
    src: &[u8],
    ctable: &[CElt; SYMBOLVALUE_MAX + 1],
    table_log: u32,
) -> usize {
    if dst.len() < BLOCKBOUND_MIN || src.len() < 12 {
        return 0;
    }
    // Build packed CTable once for all 4 segments.
    let mut packed = [0u64; SYMBOLVALUE_MAX + 1];
    for i in 0..=SYMBOLVALUE_MAX {
        let nb = ctable[i].nb_bits as u32;
        let val = ctable[i].val as u64;
        if nb > 0 {
            packed[i] = (val << (64 - nb)) | nb as u64;
        }
    }

    let segment_size = src.len().div_ceil(4);
    let mut op = 6usize;

    let c_size1 = compress_1x_packed(&mut dst[op..], &src[..segment_size], &packed, table_log);
    if c_size1 == 0 || c_size1 > u16::MAX as usize {
        return 0;
    }
    write_segment_length(dst, 0, c_size1);
    op += c_size1;

    let c_size2 = compress_1x_packed(
        &mut dst[op..],
        &src[segment_size..segment_size * 2],
        &packed,
        table_log,
    );
    if c_size2 == 0 || c_size2 > u16::MAX as usize {
        return 0;
    }
    write_segment_length(dst, 2, c_size2);
    op += c_size2;

    let c_size3 = compress_1x_packed(
        &mut dst[op..],
        &src[segment_size * 2..segment_size * 3],
        &packed,
        table_log,
    );
    if c_size3 == 0 || c_size3 > u16::MAX as usize {
        return 0;
    }
    write_segment_length(dst, 4, c_size3);
    op += c_size3;

    let c_size4 = compress_1x_packed(&mut dst[op..], &src[segment_size * 3..], &packed, table_log);
    if c_size4 == 0 {
        return 0;
    }
    op + c_size4
}

#[allow(dead_code)]
fn compress_internal(
    dst: &mut [u8],
    src: &[u8],
    mut max_symbol_value: u32,
    mut huff_log: u32,
) -> Result<usize> {
    if src.is_empty() || dst.is_empty() {
        return Ok(0);
    }
    if src.len() > BLOCKSIZE_MAX {
        return Err(Error::SrcSizeWrong);
    }
    if huff_log as usize > TABLELOG_MAX {
        return Err(Error::TableLogTooLarge);
    }
    if max_symbol_value as usize > SYMBOLVALUE_MAX {
        return Err(Error::MaxSymbolValueTooLarge);
    }
    if max_symbol_value == 0 {
        max_symbol_value = SYMBOLVALUE_MAX as u32;
    }
    if huff_log == 0 {
        huff_log = TABLELOG_DEFAULT as u32;
    }

    let mut workspace = CompressWorkspace::default();
    let largest = count_wksp(
        &mut workspace.count,
        &mut max_symbol_value,
        src,
        &mut workspace.hist,
    )?;
    if largest as usize == src.len() {
        return Ok(0);
    }
    if largest as usize <= (src.len() >> 7) + 4 {
        return Ok(0);
    }

    huff_log = optimal_table_log(huff_log, src.len(), max_symbol_value);
    huff_log = build_ctable(
        &mut workspace.ctable,
        &workspace.count,
        max_symbol_value,
        huff_log,
        &mut workspace.build,
    )?;
    workspace.ctable[(max_symbol_value as usize + 1)..].fill(CElt::default());

    let header_size = match write_ctable(
        dst,
        &workspace.ctable,
        max_symbol_value,
        huff_log,
        &mut workspace.weights,
    ) {
        Ok(size) => size,
        Err(Error::Generic) => return Ok(0),
        Err(err) => return Err(err),
    };
    if header_size + 12 >= src.len() {
        return Ok(0);
    }
    let compressed_size =
        compress_4x_using_ctable(&mut dst[header_size..], src, &workspace.ctable, huff_log);
    if compressed_size == 0 {
        return Ok(0);
    }
    let total = header_size + compressed_size;
    if total >= src.len().saturating_sub(1) {
        return Ok(0);
    }
    Ok(total)
}

fn estimate_compressed_size(
    ctable: &[CElt; SYMBOLVALUE_MAX + 1],
    count: &[u32; SYMBOLVALUE_MAX + 1],
    max_symbol_value: u32,
) -> usize {
    let mut nb_bits = 0usize;
    for symbol in 0..=max_symbol_value as usize {
        nb_bits += ctable[symbol].nb_bits as usize * count[symbol] as usize;
    }
    nb_bits >> 3
}

fn optimal_table_log(max_table_log: u32, src_size: usize, max_symbol_value: u32) -> u32 {
    debug_assert!(src_size > 1);
    let mut table_log = max_table_log;
    let max_bits_src = highbit32((src_size - 1) as u32).saturating_sub(1);
    let min_bits_src = highbit32(src_size as u32) + 1;
    let min_bits_symbols = highbit32(max_symbol_value) + 2;
    let min_bits = cmp::min(min_bits_src, min_bits_symbols);
    if max_bits_src < table_log {
        table_log = max_bits_src;
    }
    if min_bits > table_log {
        table_log = min_bits;
    }
    table_log.clamp(5, TABLELOG_MAX as u32)
}

/// Optimal table log search: try each huffLog from min to max, build the tree
/// at each depth, and pick the one that produces the smallest total (header + data).
/// Matches C's HUF_optimalTableLog with HUF_flags_optimalDepth.
fn optimal_table_log_search(
    max_table_log: u32,
    count: &[u32; SYMBOLVALUE_MAX + 1],
    max_symbol_value: u32,
) -> u32 {
    // Compute cardinality (number of non-zero symbols)
    let cardinality = count[..=max_symbol_value as usize]
        .iter()
        .filter(|&&c| c > 0)
        .count() as u32;
    let min_table_log = if cardinality == 0 {
        1
    } else {
        highbit32(cardinality) + 1
    };

    let mut opt_size = usize::MAX - 1;
    let mut opt_log = max_table_log;
    let mut ctable = [CElt::default(); SYMBOLVALUE_MAX + 1];
    let mut build_ws = BuildCTableWorkspace::default();
    let mut weight_ws = WeightCompressionWorkspace::default();

    for log_guess in min_table_log..=max_table_log {
        let Ok(max_bits) = build_ctable(
            &mut ctable,
            count,
            max_symbol_value,
            log_guess,
            &mut build_ws,
        ) else {
            continue;
        };
        if max_bits < log_guess && log_guess > min_table_log {
            break;
        }
        let data_size = estimate_compressed_size(&ctable, count, max_symbol_value);
        let header_size = match write_ctable(
            &mut [0u8; 256],
            &ctable,
            max_symbol_value,
            max_bits,
            &mut weight_ws,
        ) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let new_size = data_size + header_size;
        if new_size > opt_size + 1 {
            break;
        }
        if new_size < opt_size {
            opt_size = new_size;
            opt_log = log_guess;
        }
    }
    opt_log
}

fn read_node(nodes: &[NodeElt; CTABLE_WORKSPACE_SIZE_U32], idx: i32) -> NodeElt {
    if idx < 0 {
        nodes[0]
    } else {
        nodes[idx as usize + 1]
    }
}

fn write_node_mut(nodes: &mut [NodeElt; CTABLE_WORKSPACE_SIZE_U32], idx: usize) -> &mut NodeElt {
    &mut nodes[idx + 1]
}

/// C's `HUF_getIndex` (`huf_compress.c:530`): which bucket a count sorts into.
///
/// Counts below the cutoff get a bucket each, so they need no sorting at all;
/// everything above shares one bucket per power of two and is sorted within it.
fn rank_index(count: u32) -> u32 {
    if count < RANK_POSITION_DISTINCT_COUNT_CUTOFF {
        count
    } else {
        highbit32(count) + RANK_POSITION_LOG_BUCKETS_BEGIN
    }
}

/// C's `HUF_insertionSort` (`huf_compress.c:555`), by descending count.
///
/// `low` and `high` are inclusive and signed because C's callers pass
/// `idx - 1`, which is `-1` when the partition landed on the first element.
/// That range is empty and the loop below does nothing with it, which is the
/// behaviour being reproduced rather than a case to guard against.
fn insertion_sort_nodes(nodes: &mut [NodeElt], low: isize, high: isize) {
    let size = high - low + 1;
    for i in 1..size {
        let key = nodes[(low + i) as usize];
        let mut j = i - 1;
        while j >= 0 && nodes[(low + j) as usize].count < key.count {
            nodes[(low + j + 1) as usize] = nodes[(low + j) as usize];
            j -= 1;
        }
        nodes[(low + j + 1) as usize] = key;
    }
}

/// C's `HUF_quickSortPartition` (`huf_compress.c:571`), pivoting on the
/// rightmost element.
fn partition_nodes(nodes: &mut [NodeElt], low: isize, high: isize) -> isize {
    let pivot = nodes[high as usize].count;
    let mut boundary = low - 1;
    for j in low..high {
        if nodes[j as usize].count > pivot {
            boundary += 1;
            nodes.swap(boundary as usize, j as usize);
        }
    }
    nodes.swap((boundary + 1) as usize, high as usize);
    boundary + 1
}

/// C's `HUF_simpleQuickSort` (`huf_compress.c:591`).
///
/// Which permutation this leaves for equal counts is not incidental: symbols
/// that tie are ordered by where the partitioning happened to put them, and
/// the Huffman tree built next assigns them different code lengths depending
/// on that order. A stable sort here is a *different* table from upstream's on
/// the same histogram -- same total cost, different bytes. That is worth one
/// byte in a block of json records, and it is the only reason a frame this
/// crate emits can differ from upstream's while parsing identically.
fn quick_sort_nodes(nodes: &mut [NodeElt], mut low: isize, mut high: isize) {
    const INSERTION_SORT_THRESHOLD: isize = 8;
    if high - low < INSERTION_SORT_THRESHOLD {
        insertion_sort_nodes(nodes, low, high);
        return;
    }
    // C recurses into the smaller half and loops on the larger one, and only
    // the entry above consults the threshold: a small range reached through
    // the loop is partitioned rather than insertion-sorted.
    while low < high {
        let pivot = partition_nodes(nodes, low, high);
        if pivot - low < high - pivot {
            quick_sort_nodes(nodes, low, pivot - 1);
            low = pivot + 1;
        } else {
            quick_sort_nodes(nodes, pivot + 1, high);
            high = pivot - 1;
        }
    }
}

/// C's `HUF_sort` (`huf_compress.c:620`): symbols by descending count.
///
/// `nodes` is offset by one from C's `huffNode`, which is why every write here
/// lands at `position + 1`: index zero is the sentinel the tree builder reads
/// through [`read_node`].
fn sort_nodes(
    nodes: &mut [NodeElt; CTABLE_WORKSPACE_SIZE_U32],
    count: &[u32; SYMBOLVALUE_MAX + 1],
    max_symbol_value: u32,
    rank_position: &mut [RankPos; RANK_POSITION_TABLE_SIZE],
) {
    rank_position.fill(RankPos::default());
    for &c in &count[..=max_symbol_value as usize] {
        rank_position[rank_index(c) as usize].base += 1;
    }
    // Counting from the top, so `base` is how many symbols out-count this
    // bucket and therefore where the bucket starts.
    for n in (1..RANK_POSITION_TABLE_SIZE).rev() {
        rank_position[n - 1].base += rank_position[n].base;
        rank_position[n - 1].current = rank_position[n - 1].base;
    }
    for (symbol, &c) in count[..=max_symbol_value as usize].iter().enumerate() {
        let rank = rank_index(c) as usize + 1;
        let position = rank_position[rank].current as usize;
        rank_position[rank].current += 1;
        nodes[position + 1].count = c;
        nodes[position + 1].byte = symbol as u8;
    }
    // Only the shared buckets need sorting; a bucket of one distinct count is
    // already in order. C indexes them one past the bucket they hold, and
    // stops one short of the table, so the widest bucket of all is left
    // unsorted -- it takes a count of 2^31 to reach.
    for n in RANK_POSITION_DISTINCT_COUNT_CUTOFF as usize..RANK_POSITION_TABLE_SIZE - 1 {
        let bucket_size = (rank_position[n].current - rank_position[n].base) as isize;
        let bucket_start = rank_position[n].base as usize;
        if bucket_size > 1 {
            quick_sort_nodes(&mut nodes[bucket_start + 1..], 0, bucket_size - 1);
        }
    }
}

fn set_max_height(
    nodes: &mut [NodeElt; CTABLE_WORKSPACE_SIZE_U32],
    last_non_null: usize,
    max_nb_bits: u32,
) -> u32 {
    let largest_bits = nodes[last_non_null + 1].nb_bits as u32;
    if largest_bits <= max_nb_bits {
        return largest_bits;
    }

    let mut total_cost = 0i32;
    let base_cost = 1i32 << (largest_bits - max_nb_bits);
    let mut n = last_non_null as i32;
    while read_node(nodes, n).nb_bits as u32 > max_nb_bits {
        let node = read_node(nodes, n);
        total_cost += base_cost - (1i32 << (largest_bits - node.nb_bits as u32));
        write_node_mut(nodes, n as usize).nb_bits = max_nb_bits as u8;
        n -= 1;
    }
    while n >= 0 && read_node(nodes, n).nb_bits as u32 == max_nb_bits {
        n -= 1;
    }
    total_cost >>= largest_bits - max_nb_bits;

    const NO_SYMBOL: usize = usize::MAX;
    let mut rank_last = [NO_SYMBOL; TABLELOG_MAX + 2];
    let mut current_nb_bits = max_nb_bits;
    let mut pos = n;
    while pos >= 0 {
        let nb_bits = read_node(nodes, pos).nb_bits as u32;
        if nb_bits < current_nb_bits {
            current_nb_bits = nb_bits;
            rank_last[(max_nb_bits - current_nb_bits) as usize] = pos as usize;
        }
        pos -= 1;
    }

    while total_cost > 0 {
        let mut n_bits_to_decrease = highbit32(total_cost as u32) + 1;
        while n_bits_to_decrease > 1 {
            let high_pos = rank_last[n_bits_to_decrease as usize];
            let low_pos = rank_last[(n_bits_to_decrease - 1) as usize];
            if high_pos == NO_SYMBOL {
                n_bits_to_decrease -= 1;
                continue;
            }
            if low_pos == NO_SYMBOL {
                break;
            }
            let high_total = nodes[high_pos + 1].count;
            let low_total = 2 * nodes[low_pos + 1].count;
            if high_total <= low_total {
                break;
            }
            n_bits_to_decrease -= 1;
        }
        while n_bits_to_decrease as usize <= TABLELOG_MAX
            && rank_last[n_bits_to_decrease as usize] == NO_SYMBOL
        {
            n_bits_to_decrease += 1;
        }
        total_cost -= 1 << (n_bits_to_decrease - 1);
        if rank_last[(n_bits_to_decrease - 1) as usize] == NO_SYMBOL {
            rank_last[(n_bits_to_decrease - 1) as usize] = rank_last[n_bits_to_decrease as usize];
        }
        let idx = rank_last[n_bits_to_decrease as usize] + 1;
        nodes[idx].nb_bits += 1;
        if rank_last[n_bits_to_decrease as usize] == 0 {
            rank_last[n_bits_to_decrease as usize] = NO_SYMBOL;
        } else {
            rank_last[n_bits_to_decrease as usize] -= 1;
            if nodes[rank_last[n_bits_to_decrease as usize] + 1].nb_bits as u32
                != max_nb_bits - n_bits_to_decrease
            {
                rank_last[n_bits_to_decrease as usize] = NO_SYMBOL;
            }
        }
    }

    while total_cost < 0 {
        if rank_last[1] == NO_SYMBOL {
            while n >= 0 && read_node(nodes, n).nb_bits as u32 == max_nb_bits {
                n -= 1;
            }
            nodes[n as usize + 2].nb_bits -= 1;
            rank_last[1] = n as usize + 1;
            total_cost += 1;
            continue;
        }
        nodes[rank_last[1] + 2].nb_bits -= 1;
        rank_last[1] += 1;
        total_cost += 1;
    }

    max_nb_bits
}

fn build_ctable(
    tree: &mut [CElt; SYMBOLVALUE_MAX + 1],
    count: &[u32; SYMBOLVALUE_MAX + 1],
    max_symbol_value: u32,
    mut max_nb_bits: u32,
    workspace: &mut BuildCTableWorkspace,
) -> Result<u32> {
    if max_nb_bits == 0 {
        max_nb_bits = TABLELOG_DEFAULT as u32;
    }
    if max_symbol_value as usize > SYMBOLVALUE_MAX {
        return Err(Error::MaxSymbolValueTooLarge);
    }

    workspace.nodes.fill(NodeElt::default());
    sort_nodes(
        &mut workspace.nodes,
        count,
        max_symbol_value,
        &mut workspace.rank_position,
    );

    let mut non_null_rank = max_symbol_value as i32;
    while workspace.nodes[non_null_rank as usize + 1].count == 0 {
        non_null_rank -= 1;
    }

    let mut low_s = non_null_rank;
    let mut node_nb = STARTNODE as i32;
    let node_root = node_nb + low_s - 1;
    let mut low_n = node_nb;
    write_node_mut(&mut workspace.nodes, node_nb as usize).count =
        read_node(&workspace.nodes, low_s).count + read_node(&workspace.nodes, low_s - 1).count;
    write_node_mut(&mut workspace.nodes, low_s as usize).parent = node_nb as u16;
    write_node_mut(&mut workspace.nodes, (low_s - 1) as usize).parent = node_nb as u16;
    node_nb += 1;
    low_s -= 2;

    for n in node_nb..=node_root {
        write_node_mut(&mut workspace.nodes, n as usize).count = 1u32 << 30;
    }
    workspace.nodes[0].count = 1u32 << 31;

    while node_nb <= node_root {
        let n1 = if read_node(&workspace.nodes, low_s).count
            < read_node(&workspace.nodes, low_n).count
        {
            let value = low_s;
            low_s -= 1;
            value
        } else {
            let value = low_n;
            low_n += 1;
            value
        };
        let n2 = if read_node(&workspace.nodes, low_s).count
            < read_node(&workspace.nodes, low_n).count
        {
            let value = low_s;
            low_s -= 1;
            value
        } else {
            let value = low_n;
            low_n += 1;
            value
        };
        write_node_mut(&mut workspace.nodes, node_nb as usize).count =
            read_node(&workspace.nodes, n1).count + read_node(&workspace.nodes, n2).count;
        write_node_mut(&mut workspace.nodes, n1 as usize).parent = node_nb as u16;
        write_node_mut(&mut workspace.nodes, n2 as usize).parent = node_nb as u16;
        node_nb += 1;
    }

    write_node_mut(&mut workspace.nodes, node_root as usize).nb_bits = 0;
    for n in (STARTNODE as i32..node_root).rev() {
        let parent = read_node(&workspace.nodes, n).parent as i32;
        write_node_mut(&mut workspace.nodes, n as usize).nb_bits =
            read_node(&workspace.nodes, parent).nb_bits + 1;
    }
    for n in 0..=non_null_rank as usize {
        let parent = workspace.nodes[n + 1].parent as i32;
        workspace.nodes[n + 1].nb_bits = read_node(&workspace.nodes, parent).nb_bits + 1;
    }

    max_nb_bits = set_max_height(&mut workspace.nodes, non_null_rank as usize, max_nb_bits);

    let mut nb_per_rank = [0u16; TABLELOG_MAX + 1];
    let mut val_per_rank = [0u16; TABLELOG_MAX + 1];
    let alphabet_size = max_symbol_value as usize + 1;
    if max_nb_bits as usize > TABLELOG_MAX {
        return Err(Error::Generic);
    }
    for n in 0..=non_null_rank as usize {
        nb_per_rank[workspace.nodes[n + 1].nb_bits as usize] += 1;
    }
    let mut min = 0u16;
    for n in (1..=max_nb_bits as usize).rev() {
        val_per_rank[n] = min;
        min = (min + nb_per_rank[n]) >> 1;
    }
    for n in 0..alphabet_size {
        tree[workspace.nodes[n + 1].byte as usize].nb_bits = workspace.nodes[n + 1].nb_bits;
    }
    for item in tree.iter_mut().take(alphabet_size) {
        item.val = val_per_rank[item.nb_bits as usize];
        val_per_rank[item.nb_bits as usize] += 1;
    }
    Ok(max_nb_bits)
}

fn write_ctable(
    dst: &mut [u8],
    ctable: &[CElt; SYMBOLVALUE_MAX + 1],
    max_symbol_value: u32,
    huff_log: u32,
    workspace: &mut WeightCompressionWorkspace,
) -> Result<usize> {
    if dst.is_empty() {
        return Err(Error::DstSizeTooSmall);
    }

    let mut bits_to_weight = [0u8; TABLELOG_MAX + 1];
    let mut huff_weight = [0u8; SYMBOLVALUE_MAX + 1];
    for n in 1..=huff_log as usize {
        bits_to_weight[n] = (huff_log as usize + 1 - n) as u8;
    }
    for n in 0..max_symbol_value as usize {
        huff_weight[n] = bits_to_weight[ctable[n].nb_bits as usize];
    }

    let compressed_size = compress_weights(
        &mut dst[1..],
        &huff_weight[..max_symbol_value as usize],
        workspace,
    )?;
    if compressed_size > 1 && compressed_size < (max_symbol_value as usize / 2) {
        dst[0] = compressed_size as u8;
        return Ok(compressed_size + 1);
    }

    write_ctable_raw(dst, &huff_weight, max_symbol_value)
}

fn compress_weights(
    dst: &mut [u8],
    weight_table: &[u8],
    workspace: &mut WeightCompressionWorkspace,
) -> Result<usize> {
    if weight_table.len() <= 1 {
        return Ok(0);
    }

    workspace.count.fill(0);
    let mut max_symbol_value = 0u32;
    let mut max_count = 0u32;
    for &weight in weight_table {
        let slot = workspace
            .count
            .get_mut(weight as usize)
            .ok_or(Error::Generic)?;
        *slot += 1;
        max_count = max_count.max(*slot);
        max_symbol_value = max_symbol_value.max(weight as u32);
    }
    while max_symbol_value > 0 && workspace.count[max_symbol_value as usize] == 0 {
        max_symbol_value -= 1;
    }

    if max_count as usize == weight_table.len() {
        return Ok(1);
    }
    if max_count == 1 {
        return Ok(0);
    }

    let table_log = fse::optimal_table_log(
        MAX_FSE_TABLELOG_FOR_HUFF_HEADER as u32,
        weight_table.len(),
        max_symbol_value,
    );
    fse::normalize_count(
        &mut workspace.norm,
        table_log,
        &workspace.count,
        weight_table.len(),
        max_symbol_value,
        false,
    )?;
    let header_size = fse::write_ncount(dst, &workspace.norm, max_symbol_value, table_log)?;
    fse::build_ctable(
        &mut workspace.ctable,
        &workspace.norm,
        max_symbol_value,
        table_log,
    )?;
    let compressed_size =
        fse::compress_using_ctable(&mut dst[header_size..], weight_table, &workspace.ctable)?;
    if compressed_size == 0 {
        return Ok(0);
    }
    Ok(header_size + compressed_size)
}

fn write_ctable_raw(
    dst: &mut [u8],
    huff_weight: &[u8; SYMBOLVALUE_MAX + 1],
    max_symbol_value: u32,
) -> Result<usize> {
    if max_symbol_value > 127 {
        return Err(Error::Generic);
    }
    if dst.is_empty() {
        return Err(Error::DstSizeTooSmall);
    }

    let raw_size = (max_symbol_value as usize).div_ceil(2) + 1;
    if raw_size > dst.len() || max_symbol_value == 0 {
        return Err(Error::DstSizeTooSmall);
    }
    dst[0] = 128u8 + (max_symbol_value as u8 - 1);
    let mut huff_weight = *huff_weight;
    huff_weight[max_symbol_value as usize] = 0;
    for n in (0..max_symbol_value as usize).step_by(2) {
        dst[(n / 2) + 1] = (huff_weight[n] << 4) | huff_weight[n + 1];
    }
    Ok(raw_size)
}

fn build_ctable_from_weights(
    ctable: &mut [CElt; SYMBOLVALUE_MAX + 1],
    huff_weight: &[u8; SYMBOLVALUE_MAX + 1],
    nb_symbols: usize,
    table_log: u32,
) -> Result<()> {
    if table_log == 0 || table_log as usize > TABLELOG_MAX {
        return Err(Error::Corruption("invalid Huff0 table log"));
    }

    ctable.fill(CElt::default());

    let mut nb_per_rank = [0u16; TABLELOG_MAX + 1];
    let mut val_per_rank = [0u16; TABLELOG_MAX + 1];
    for &weight in &huff_weight[..nb_symbols] {
        if weight == 0 {
            continue;
        }
        if weight as u32 > table_log {
            return Err(Error::Corruption("invalid Huff0 weight"));
        }
        let nb_bits = (table_log + 1 - u32::from(weight)) as usize;
        if nb_bits == 0 || nb_bits > TABLELOG_MAX {
            return Err(Error::Corruption("invalid Huff0 weight"));
        }
        nb_per_rank[nb_bits] += 1;
    }

    let mut min = 0u16;
    for n in (1..=table_log as usize).rev() {
        val_per_rank[n] = min;
        min = (min + nb_per_rank[n]) >> 1;
    }

    for symbol in 0..nb_symbols {
        if huff_weight[symbol] == 0 {
            continue;
        }
        let nb_bits = (table_log + 1 - u32::from(huff_weight[symbol])) as usize;
        ctable[symbol].nb_bits = nb_bits as u8;
        ctable[symbol].val = val_per_rank[nb_bits];
        val_per_rank[nb_bits] += 1;
    }
    Ok(())
}

fn table_supports_all_symbols(ctable: &CTableX1, src: &[u8]) -> bool {
    src.iter()
        .all(|&symbol| ctable.entries[symbol as usize].nb_bits != 0)
}

/// Whether `ctable` assigns a code to every symbol the block actually contains,
/// decided from the histogram instead of from the block.
///
/// A symbol the table cannot encode is one with a nonzero count and a zero code
/// length, so the question is answered in `max_symbol_value + 1` comparisons
/// against counts that `count_wksp` has already produced. The equivalent
/// question asked of the input directly, as [`table_supports_all_symbols`] asks
/// it, is a second pass over every literal byte, and each byte costs a
/// dependent load into the code table. On a text block that scan was about half
/// the literal stage. This is C's `HUF_validateCTable`, which asks it the same
/// way and for the same reason.
///
/// Only correct where `count` and `max_symbol_value` describe `src`; the
/// input-scanning form stays for the paths that have no histogram in hand.
fn table_covers_counted_symbols(
    ctable: &CTableX1,
    count: &[u32; SYMBOLVALUE_MAX + 1],
    max_symbol_value: u32,
) -> bool {
    let last = max_symbol_value as usize;
    !count[..=last]
        .iter()
        .zip(&ctable.entries[..=last])
        .any(|(&frequency, entry)| frequency != 0 && entry.nb_bits == 0)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    /// Symbols that tie on count get their code lengths from where the sort
    /// left them, and upstream's sort is a quicksort: the order it leaves is
    /// not the order they were counted in.
    ///
    /// The histogram is the one that made this visible -- the hex digits of a
    /// block of json records, with `'2'`, `'a'` and `'c'` all at 169. Fourteen
    /// of the sixteen share a bucket, which is over the eight-symbol threshold
    /// where upstream stops insertion-sorting, and the three-way tie is broken
    /// by the partitioning. The expected lengths are read off a frame upstream
    /// produced from these exact literals, not derived here.
    #[test]
    fn tied_symbol_counts_take_upstream_code_lengths() {
        let mut count = [0u32; SYMBOLVALUE_MAX + 1];
        for (symbol, frequency) in [
            (b'0', 172),
            (b'1', 171),
            (b'2', 169),
            (b'3', 173),
            (b'4', 178),
            (b'5', 166),
            (b'6', 176),
            (b'7', 405),
            (b'8', 294),
            (b'9', 170),
            (b'a', 169),
            (b'b', 182),
            (b'c', 169),
            (b'd', 179),
            (b'e', 171),
            (b'f', 175),
        ] {
            count[symbol as usize] = frequency;
        }

        let mut workspace = CompressWorkspace::default();
        workspace
            .build_literal_ctable(&count, b'f' as u32, TABLELOG_DEFAULT as u32)
            .unwrap();

        let lengths: Vec<(u8, u8)> = (b'0'..=b'f')
            .filter(|symbol| count[*symbol as usize] != 0)
            .map(|symbol| (symbol, workspace.ctable[symbol as usize].nb_bits))
            .collect();
        let expected: Vec<(u8, u8)> = [
            (b'0', 4),
            (b'1', 4),
            (b'2', 4),
            (b'3', 4),
            (b'4', 4),
            (b'5', 5),
            (b'6', 4),
            (b'7', 3),
            (b'8', 4),
            (b'9', 4),
            (b'a', 5),
            (b'b', 4),
            (b'c', 4),
            (b'd', 4),
            (b'e', 4),
            (b'f', 4),
        ]
        .into_iter()
        .collect();
        assert_eq!(lengths, expected);
    }

    #[test]
    fn decompress_special_cases() {
        let mut dst = [0u8; 8];
        assert_eq!(decompress_into(&mut dst, b"x").unwrap(), 8);
        assert_eq!(dst, [b'x'; 8]);

        let src = *b"rawblock";
        let mut out = [0u8; 8];
        assert_eq!(decompress_into(&mut out, &src).unwrap(), 8);
        assert_eq!(out, src);
    }

    #[test]
    fn decodes_sibling_huff0_fixture_when_present() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../Huff0/tests/data");
        if !root.exists() {
            eprintln!("skipping Huff0 fixture test: sibling Huff0 checkout not found");
            return;
        }

        let raw = fs::read(root.join("c_fixture_raw.bin")).unwrap();
        let compressed = fs::read(root.join("c_fixture_huf.bin")).unwrap();
        let mut decoded = vec![0u8; raw.len()];
        let written = decompress_into(&mut decoded, &compressed).unwrap();

        assert_eq!(written, raw.len());
        assert_eq!(decoded, raw);
    }

    #[test]
    fn compresses_and_roundtrips_small_alphabet_data() {
        let raw = (0..12_000)
            .map(|index| match index & 0x0f {
                0..=8 => b'A',
                9..=12 => b'B',
                13..=14 => b'C',
                _ => b'D',
            })
            .collect::<Vec<_>>();

        let compressed = compress(&raw)
            .unwrap()
            .expect("small alphabet input should be Huff0-compressible");
        let mut decoded = vec![0u8; raw.len()];
        let written = decompress_into(&mut decoded, &compressed).unwrap();

        assert_eq!(written, raw.len());
        assert_eq!(decoded, raw);
    }

    #[test]
    fn sparse_high_symbols_use_fse_compressed_weight_headers() {
        let raw = (0..64 * 1024)
            .map(|index| match index % 16 {
                0..=13 => 0u8,
                14 => 127u8,
                _ => 255u8,
            })
            .collect::<Vec<_>>();

        let compressed = compress(&raw)
            .unwrap()
            .expect("sparse high-symbol input should be Huff0-compressible");
        assert!(
            compressed[0] < 128,
            "expected an FSE-compressed Huffman weight header, got descriptor {}",
            compressed[0]
        );

        let mut ctable = CTableX1::default();
        let ctable_header_size = read_ctable_x1(&compressed, &mut ctable).unwrap();
        let mut dtable = DTableX1::default();
        let dtable_header_size = read_dtable_x1(&compressed, &mut dtable).unwrap();
        assert_eq!(ctable_header_size, dtable_header_size);

        let mut decoded = vec![0u8; raw.len()];
        let written = decompress_into(&mut decoded, &compressed).unwrap();
        assert_eq!(written, raw.len());
        assert_eq!(decoded, raw);
    }

    #[test]
    fn small_dense_alphabet_keeps_raw_weight_headers() {
        let raw = (0..16_000)
            .map(|index| match index % 8 {
                0..=3 => 0u8,
                4..=5 => 1u8,
                6 => 2u8,
                _ => 3u8,
            })
            .collect::<Vec<_>>();

        let compressed = compress(&raw)
            .unwrap()
            .expect("small dense input should be Huff0-compressible");
        assert!(
            compressed[0] >= 128,
            "expected a raw Huffman weight header, got descriptor {}",
            compressed[0]
        );

        let mut decoded = vec![0u8; raw.len()];
        let written = decompress_into(&mut decoded, &compressed).unwrap();
        assert_eq!(written, raw.len());
        assert_eq!(decoded, raw);
    }

    #[test]
    fn reuses_a_parsed_huffman_table_for_treeless_compression() {
        let raw = (0..16_000)
            .map(|index| match index % 7 {
                0..=3 => b'a',
                4 => b'b',
                5 => b'c',
                _ => b'd',
            })
            .collect::<Vec<_>>();

        let compressed = compress(&raw)
            .unwrap()
            .expect("test input should be Huff0-compressible");

        let mut ctable = CTableX1::default();
        let ctable_header_size = read_ctable_x1(&compressed, &mut ctable).unwrap();
        let mut dtable = DTableX1::default();
        let dtable_header_size = read_dtable_x1(&compressed, &mut dtable).unwrap();
        assert_eq!(ctable_header_size, dtable_header_size);

        let treeless = compress_with_table(&raw, &ctable)
            .unwrap()
            .expect("parsed table should support the same alphabet");
        let mut decoded = vec![0u8; raw.len()];
        let dtable = HuffmanDTable::Single(dtable);
        let written = decompress_4x_using_dtable(&mut decoded, &treeless, &dtable).unwrap();

        assert_eq!(written, raw.len());
        assert_eq!(decoded, raw);
    }

    #[test]
    fn prefer_existing_table_compresses_once_with_reused_table() {
        let raw = (0..16_000)
            .map(|index| match index % 7 {
                0..=3 => b'a',
                4 => b'b',
                5 => b'c',
                _ => b'd',
            })
            .collect::<Vec<_>>();

        let compressed = compress(&raw)
            .unwrap()
            .expect("test input should be Huff0-compressible");
        let mut ctable = CTableX1::default();
        let ctable_header_size = read_ctable_x1(&compressed, &mut ctable).unwrap();
        let mut dtable = DTableX1::default();
        let dtable_header_size = read_dtable_x1(&compressed, &mut dtable).unwrap();
        assert_eq!(ctable_header_size, dtable_header_size);

        let mut dst = vec![0u8; compress_bound(raw.len())];
        let preferred = compress_prefer_existing_table_into(&mut dst, &raw, Some(&ctable))
            .unwrap()
            .expect("existing table should remain usable");
        assert!(preferred.reused_table);
        assert_eq!(preferred.table.table_log, ctable.table_log);

        let mut decoded = vec![0u8; raw.len()];
        let dtable = HuffmanDTable::Single(dtable);
        let written =
            decompress_4x_using_dtable(&mut decoded, &dst[..preferred.written], &dtable).unwrap();

        assert_eq!(written, raw.len());
        assert_eq!(decoded, raw);
    }

    /// Reference (old) bottom-up encoder for comparing output byte-by-byte.
    #[allow(unsafe_code)]
    fn compress_1x_reference(
        dst: &mut [u8],
        src: &[u8],
        ctable: &[super::CElt; super::SYMBOLVALUE_MAX + 1],
    ) -> usize {
        if dst.len() < 8 {
            return 0;
        }
        let mut bit_c = match super::BitCStream::new(dst) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        let src_ptr = src.as_ptr();
        let mut n = src.len() & !3;
        unsafe {
            match src.len() & 3 {
                3 => {
                    super::encode_symbol(&mut bit_c, *src_ptr.add(n + 2), ctable);
                    super::encode_symbol(&mut bit_c, *src_ptr.add(n + 1), ctable);
                    super::encode_symbol(&mut bit_c, *src_ptr.add(n), ctable);
                    bit_c.flush_bits_unchecked();
                }
                2 => {
                    super::encode_symbol(&mut bit_c, *src_ptr.add(n + 1), ctable);
                    super::encode_symbol(&mut bit_c, *src_ptr.add(n), ctable);
                    bit_c.flush_bits_unchecked();
                }
                1 => {
                    super::encode_symbol(&mut bit_c, *src_ptr.add(n), ctable);
                    bit_c.flush_bits_unchecked();
                }
                _ => {}
            }
            while n > 0 {
                super::encode_symbol(&mut bit_c, *src_ptr.add(n - 1), ctable);
                super::encode_symbol(&mut bit_c, *src_ptr.add(n - 2), ctable);
                super::encode_symbol(&mut bit_c, *src_ptr.add(n - 3), ctable);
                super::encode_symbol(&mut bit_c, *src_ptr.add(n - 4), ctable);
                bit_c.flush_bits_unchecked();
                n -= 4;
            }
        }
        bit_c.close()
    }

    #[test]
    fn packed_matches_reference_encoder() {
        // Test with various sizes to exercise remainder paths (0,1,2,3).
        for &extra in &[0, 1, 2, 3] {
            let size = 200 + extra;
            let src: Vec<u8> = (0..size)
                .map(|i| match i % 5 {
                    0 => 0,
                    1 => 1,
                    2 => 2,
                    3 => 0,
                    _ => 1,
                })
                .collect();

            let mut ctable = [super::CElt::default(); super::SYMBOLVALUE_MAX + 1];
            ctable[0] = super::CElt { val: 0, nb_bits: 1 };
            ctable[1] = super::CElt { val: 2, nb_bits: 2 };
            ctable[2] = super::CElt { val: 3, nb_bits: 2 };

            let mut ref_dst = vec![0u8; src.len() + 64];
            let mut new_dst = vec![0u8; src.len() + 64];

            let ref_len = compress_1x_reference(&mut ref_dst, &src, &ctable);
            let new_len = super::compress_1x_using_ctable(&mut new_dst, &src, &ctable, 2);

            assert_eq!(
                new_len, ref_len,
                "size={size}: length mismatch new={new_len} ref={ref_len}"
            );
            if new_dst[..new_len] != ref_dst[..ref_len] {
                for i in 0..new_len {
                    if new_dst[i] != ref_dst[i] {
                        panic!(
                            "size={size}: first mismatch at byte {i}: new=0x{:02x} ref=0x{:02x}",
                            new_dst[i], ref_dst[i]
                        );
                    }
                }
            }
        }

        // Larger test with realistic alphabet.
        let src: Vec<u8> = (0..12_000)
            .map(|i| match i & 0x0f {
                0..=8 => b'A',
                9..=12 => b'B',
                13..=14 => b'C',
                _ => b'D',
            })
            .collect();
        let mut ctable = [super::CElt::default(); super::SYMBOLVALUE_MAX + 1];
        ctable[b'A' as usize] = super::CElt { val: 0, nb_bits: 1 };
        ctable[b'B' as usize] = super::CElt { val: 2, nb_bits: 2 };
        ctable[b'C' as usize] = super::CElt { val: 6, nb_bits: 3 };
        ctable[b'D' as usize] = super::CElt { val: 7, nb_bits: 3 };

        let mut ref_dst = vec![0u8; src.len() + 64];
        let mut new_dst = vec![0u8; src.len() + 64];

        let ref_len = compress_1x_reference(&mut ref_dst, &src, &ctable);
        let new_len = super::compress_1x_using_ctable(&mut new_dst, &src, &ctable, 3);

        assert_eq!(
            new_len, ref_len,
            "large: length mismatch new={new_len} ref={ref_len}"
        );
        if new_dst[..new_len] != ref_dst[..ref_len] {
            for i in 0..new_len {
                if new_dst[i] != ref_dst[i] {
                    panic!(
                        "large: first mismatch at byte {i}: new=0x{:02x} ref=0x{:02x}",
                        new_dst[i], ref_dst[i]
                    );
                }
            }
        }
    }

    /// A weight table whose weights sum to `1 << 12` yields `table_log == 12`,
    /// the largest `read_stats` admits, and needs all `1 << TABLELOG_MAX`
    /// entries to fill. `DTableX1::entries` used to be half that, so this
    /// header ran the fill loop off the end of the array — and
    /// `decode_symbol_x1` would have read past it with no bounds check at all,
    /// on a safety comment that claimed the full size.
    ///
    /// The bytes are the Huffman description lifted from a 40-byte frame the
    /// fuzzer found in under 30 seconds: a direct-weight header (`0x9f`, so 32
    /// weights in 16 packed bytes) whose weights are 10, 4, 1, 3, 7, 1, 9, 6,
    /// 2, 8, 11 and 5, summing to 2048 and implying a final weight of 12.
    #[test]
    fn table_log_twelve_fills_the_decode_table_without_overrunning_it() {
        let header = [
            0x9f, 0xa4, 0x00, 0x00, 0x00, 0x00, 0x01, 0x37, 0x00, 0x00, 0x00, 0x01, 0x00, 0x90,
            0x60, 0x28, 0xb5,
        ];

        let mut dtable = DTableX1::default();
        let consumed = read_dtable_x1(&header, &mut dtable).expect("table log 12 is representable");

        assert_eq!(consumed, header.len());
        assert_eq!(dtable.table_log, 12);
        assert_eq!(dtable.entries.len(), 1 << TABLELOG_MAX);
        assert!(
            dtable.entries.iter().all(|entry| entry.nb_bits != 0),
            "every slot of a full table log 12 table should have been written"
        );

        // The same description in the other shape. A table already at the
        // maximum depth is the one case the double-symbol build does not
        // expand, so `rescale` goes negative and the ranks tile the table at
        // exactly the width they were read at — the branch a shallow
        // description never reaches.
        let mut double = DTableX2::default();
        let consumed = read_dtable_x2(&header, &mut double).expect("table log 12 is representable");

        assert_eq!(consumed, header.len());
        assert_eq!(double.table_log, 12);
        assert!(
            double.entries.iter().all(|entry| entry.nb_bits != 0),
            "every slot of a full table log 12 double-symbol table should have been written"
        );
    }

    /// The four streams start at multiples of `ceil(dst.len() / 4)`, so for
    /// output lengths 1, 2 and 5 the fourth start lands past the end of `dst` —
    /// at length 5 it is `3 * 2 == 6`. Every write in the decode loop goes
    /// through `get_unchecked_mut` bounded by those starts, so this was an
    /// out-of-bounds write rather than a panic. Upstream rejects the whole
    /// range with `dstSize < 6` ("stream 4-split doesn't work").
    ///
    /// Lengths 3, 4 and 6 do not overflow on their own; they are rejected only
    /// to keep accept/reject parity with upstream, which is why this walks the
    /// whole range rather than just the three overflowing values.
    #[test]
    fn four_stream_decode_rejects_outputs_too_small_to_split() {
        // Two symbols of weight 1: a direct-weight header for one explicit
        // weight, with the second implied. Enough to make a valid table_log 1
        // table, since what is under test is the output length, not the tree.
        let mut dtable = DTableX1::default();
        read_dtable_x1(&[0x80, 0x10], &mut dtable).expect("two-symbol table is valid");
        let dtable = HuffmanDTable::Single(dtable);

        // A jump table declaring three one-byte segments, then four one-byte
        // streams holding nothing but their end marker. The streams have to be
        // non-empty and well-formed or `BitDStream::new` rejects them and the
        // output length is never reached — which is exactly how the first
        // version of this test passed against the unfixed decoder.
        let src = [0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x01];

        // Collected rather than asserted per iteration so a failure names every
        // length that got through, not just the first. Length 5 is the one that
        // wrote out of bounds; the rest are parity with upstream.
        let accepted: Vec<usize> = (0..6)
            .filter(|&len| {
                let mut dst = vec![0u8; len];
                decompress_4x_using_dtable(&mut dst, &src, &dtable).is_ok()
            })
            .collect();

        assert!(
            accepted.is_empty(),
            "outputs of {accepted:?} bytes cannot be split across four streams, \
             but were accepted"
        );
    }

    /// Pseudorandom bytes with no structure a Huffman coder can exploit.
    ///
    /// Deliberately not a strided or periodic pattern: the check under test
    /// reads a contiguous 4 KiB from each end, and a fixture whose period lines
    /// up with the sample would decide the outcome by construction rather than
    /// by entropy.
    fn incompressible_bytes(len: usize) -> Vec<u8> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 33) as u8
            })
            .collect()
    }

    fn english_like_bytes(len: usize) -> Vec<u8> {
        let phrase = b"the quick brown fox jumps over the lazy dog, and then it does so again. ";
        phrase.iter().copied().cycle().take(len).collect()
    }

    fn compress_with(src: &[u8], compressibility: Compressibility) -> Option<usize> {
        let mut dst = vec![0u8; compress_bound(src.len())];
        let mut workspace = CompressWorkspace::default();
        compress_prefer_existing_table_into_mode(
            &mut dst,
            src,
            None,
            StreamMode::Four,
            TableDepth::Estimated,
            compressibility,
            &mut workspace,
        )
        .expect("compression must not error")
        .map(|choice| choice.written)
    }

    /// The sample has to reach the same verdict as the full count, or gating on
    /// it would trade output for speed. Both directions are asserted because a
    /// sample that rejected everything would pass the first half alone.
    #[test]
    fn sampled_check_agrees_with_the_full_count() {
        let incompressible = incompressible_bytes(64 * 1024);
        assert_eq!(
            compress_with(&incompressible, Compressibility::Unknown),
            None
        );
        assert_eq!(
            compress_with(&incompressible, Compressibility::Suspect),
            None
        );

        let compressible = english_like_bytes(64 * 1024);
        let full = compress_with(&compressible, Compressibility::Unknown)
            .expect("english-like text compresses");
        assert_eq!(
            compress_with(&compressible, Compressibility::Suspect),
            Some(full),
            "the sample must not turn away literals the full count would code"
        );
    }

    /// C only samples when the block is at least ten times the sample, and this
    /// crate has to draw the line in the same place or the two disagree on
    /// blocks between the two thresholds.
    #[test]
    fn sampled_check_is_inert_below_ten_samples() {
        let size = SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE * SUSPECT_INCOMPRESSIBLE_SAMPLE_RATIO;
        let mut count = [0u32; SYMBOLVALUE_MAX + 1];

        let just_under = incompressible_bytes(size - 1);
        assert!(!looks_incompressible_from_sample(&just_under, &mut count));

        let just_over = incompressible_bytes(size);
        assert!(looks_incompressible_from_sample(&just_over, &mut count));
    }

    /// Alphabets that produce Huffman trees of different depths and shapes.
    ///
    /// Depth is what decides whether the double-symbol table can pair symbols
    /// at all — a description already at `TABLELOG_MAX` has no spare bits — so
    /// a suite that only exercised one distribution would be testing one branch
    /// of the fill and reporting it as coverage of both.
    fn alphabets_of_varying_depth() -> Vec<(&'static str, Vec<u8>)> {
        // Deliberately not a clean repeat: an exactly periodic body lets the
        // encoder and both decoders agree on a single short cycle, which would
        // leave most of each table untouched.
        let skewed = (0..40_000u32)
            .map(
                |index| match (index.wrapping_mul(2_654_435_761) >> 24) % 100 {
                    0..=74 => b'e',
                    75..=89 => b't',
                    90..=96 => b'a',
                    97..=98 => b'o',
                    _ => b'z',
                },
            )
            .collect::<Vec<_>>();
        let two_symbols = (0..20_000u32)
            .map(|index| if index % 7 == 0 { 1u8 } else { 0u8 })
            .collect::<Vec<_>>();
        let full_byte_range = (0..60_000u32)
            .map(|index| {
                let spread = (index.wrapping_mul(2_246_822_519) >> 20) as u8;
                // Bias towards the low half so the tree is unbalanced rather
                // than the flat 8-bit tree a uniform byte stream would give.
                if index % 3 == 0 { spread >> 2 } else { spread }
            })
            .collect::<Vec<_>>();
        let sparse_high = (0..30_000u32)
            .map(|index| match index % 23 {
                0..=17 => 0u8,
                18..=20 => 128u8,
                21 => 200u8,
                _ => 255u8,
            })
            .collect::<Vec<_>>();

        vec![
            ("skewed five-symbol", skewed),
            ("two symbols", two_symbols),
            ("full byte range", full_byte_range),
            ("sparse high symbols", sparse_high),
        ]
    }

    /// The two table shapes decode the same bits, so their output must agree
    /// byte for byte. X1 is the already-trusted side of this comparison.
    #[test]
    fn both_decode_table_shapes_decode_a_stream_identically() {
        for (name, raw) in alphabets_of_varying_depth() {
            let compressed = compress(&raw)
                .unwrap()
                .unwrap_or_else(|| panic!("{name} should be Huff0-compressible"));

            let mut single = DTableX1::default();
            let single_header = read_dtable_x1(&compressed, &mut single).unwrap();
            let mut double = DTableX2::default();
            let double_header = read_dtable_x2(&compressed, &mut double).unwrap();
            assert_eq!(
                single_header, double_header,
                "{name}: both shapes read the same description"
            );

            let streams = &compressed[single_header..];
            let mut by_single = vec![0u8; raw.len()];
            decompress_4x1_using_dtable(&mut by_single, streams, &single).unwrap();
            let mut by_double = vec![0u8; raw.len()];
            decompress_4x2_using_dtable(&mut by_double, streams, &double).unwrap();

            assert_eq!(by_single, raw, "{name}: single-symbol decode");
            assert_eq!(by_double, raw, "{name}: double-symbol decode");
        }
    }

    #[test]
    fn the_double_symbol_table_decodes_a_single_stream() {
        for (name, raw) in alphabets_of_varying_depth() {
            let mut ctable = CTableX1::default();
            let compressed = compress(&raw)
                .unwrap()
                .unwrap_or_else(|| panic!("{name} should be Huff0-compressible"));
            let header_size = read_ctable_x1(&compressed, &mut ctable).unwrap();

            let single_stream = compress_with_table_mode(&raw, &ctable, StreamMode::Single)
                .unwrap()
                .unwrap_or_else(|| panic!("{name} should re-encode as one stream"));

            let mut double = DTableX2::default();
            assert_eq!(
                read_dtable_x2(&compressed, &mut double).unwrap(),
                header_size
            );

            let mut decoded = vec![0u8; raw.len()];
            decompress_1x2_using_dtable(&mut decoded, &single_stream, &double).unwrap();
            assert_eq!(decoded, raw, "{name}: single-stream double-symbol decode");
        }
    }

    /// The point of the double-symbol table is that most entries emit *two*
    /// bytes. A build that silently produced a table of length-1 entries would
    /// still decode correctly and be no faster, and every round-trip test above
    /// would pass, so the pairing is asserted directly.
    #[test]
    fn a_shallow_description_is_expanded_so_entries_hold_symbol_pairs() {
        let raw = (0..40_000u32)
            .map(
                |index| match (index.wrapping_mul(2_654_435_761) >> 25) % 8 {
                    0..=4 => b'x',
                    5..=6 => b'y',
                    _ => b'z',
                },
            )
            .collect::<Vec<_>>();
        let compressed = compress(&raw).unwrap().expect("compressible");

        let mut single = DTableX1::default();
        read_dtable_x1(&compressed, &mut single).unwrap();
        let mut double = DTableX2::default();
        read_dtable_x2(&compressed, &mut double).unwrap();

        assert!(
            (single.table_log as usize) <= DECODER_FAST_TABLELOG,
            "a three-symbol alphabet should not need the full table depth"
        );
        assert_eq!(
            double.table_log as usize, DECODER_FAST_TABLELOG,
            "a shallow description should be expanded to the fast depth"
        );

        let live = &double.entries[..1 << double.table_log];
        let paired = live.iter().filter(|entry| entry.length == 2).count();
        assert!(
            paired * 2 > live.len(),
            "expected most of the {} entries to emit a pair, got {paired}",
            live.len()
        );
    }

    /// Pinned against C's `HUF_selectDecoder`, recomputed by hand from its
    /// `algoTime` table. The values decide throughput only — either shape
    /// decodes the same bytes — but drifting away from C's choice would make
    /// the decode benchmark measure a different algorithm than upstream runs.
    #[test]
    fn the_decoder_shape_selector_matches_upstreams_cost_model() {
        // 32 KiB at a 9/16 ratio: single = 582 + 187*128, double = 1570 +
        // 114*128 plus its 1/32 handicap. The wide table wins comfortably.
        assert!(prefers_double_symbol_decoder(32 * 1024, 18 * 1024));
        // The same ratio over 1 KiB: the build cost no longer amortizes.
        assert!(!prefers_double_symbol_decoder(1024, 576));
        // Barely-compressible literals of any size stay on the narrow table.
        assert!(!prefers_double_symbol_decoder(64 * 1024, 64 * 1024));
        // Zero-length output is not a ratio; it must not divide by it.
        assert!(!prefers_double_symbol_decoder(0, 0));
    }

    /// A shape switch has to replace the whole table, and the replacement must
    /// not leave the previous shape's contents reachable.
    #[test]
    fn switching_table_shape_replaces_the_stored_table() {
        let raw = (0..24_000u32)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 28) as u8)
            .collect::<Vec<_>>();
        let compressed = compress(&raw).unwrap().expect("compressible");

        let mut slot = None;
        let header = read_dtable_x1(&compressed, HuffmanDTable::single_slot(&mut slot)).unwrap();
        assert!(matches!(slot, Some(HuffmanDTable::Single(_))));

        assert_eq!(
            read_dtable_x2(&compressed, HuffmanDTable::double_slot(&mut slot)).unwrap(),
            header
        );
        assert!(matches!(slot, Some(HuffmanDTable::Double(_))));

        let table = slot.expect("installed");
        let mut decoded = vec![0u8; raw.len()];
        decompress_4x_using_dtable(&mut decoded, &compressed[header..], &table).unwrap();
        assert_eq!(decoded, raw);
    }
}
