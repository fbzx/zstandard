use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::{
    decode_out::DecodeOut,
    entropy::{
        bitstream::{
            BitCStream, BitDStreamStatus, CONTAINER_BITS, CONTAINER_BYTES, STREAM_ACCUMULATOR_MIN,
        },
        fse, huff0,
        mem::highbit32,
    },
    error::{Error, Result},
    literals::DecodedLiterals,
    outbuf::OutBuf,
    window::{ParserStrategy, SeqStore},
};

const LITERAL_LENGTH_BASELINES: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];
const LITERAL_LENGTH_EXTRA_BITS: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
const MATCH_LENGTH_BASELINES: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];
const MATCH_LENGTH_EXTRA_BITS: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];
const LITERAL_LENGTH_DEFAULT_DISTRIBUTION: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
const OFFSET_DEFAULT_DISTRIBUTION: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];
const MATCH_LENGTH_DEFAULT_DISTRIBUTION: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
const LITERAL_LENGTH_CODE_TABLE: [u8; 64] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 16, 17, 17, 18, 18, 19, 19, 20, 20,
    20, 20, 21, 21, 21, 21, 22, 22, 22, 22, 22, 22, 22, 22, 23, 23, 23, 23, 23, 23, 23, 23, 24, 24,
    24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24,
];
const MATCH_LENGTH_CODE_TABLE: [u8; 128] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 30, 31, 32, 32, 33, 33, 34, 34, 35, 35, 36, 36, 36, 36, 37, 37, 37, 37, 38, 38,
    38, 38, 38, 38, 38, 38, 39, 39, 39, 39, 39, 39, 39, 39, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40,
    40, 40, 40, 40, 40, 40, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 42, 42,
    42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42,
    42, 42, 42, 42, 42, 42,
];
const LL_DEFAULT_LOG: u32 = 6;
const OF_DEFAULT_LOG: u32 = 5;
const ML_DEFAULT_LOG: u32 = 6;
const COST_ACCURACY_LOG: u32 = 8;
const FAST_SELECTOR_REPEAT_MAX_SEQUENCES: usize = 1000;
const INVERSE_PROBABILITY_LOG256: [u16; 256] = [
    0, 2048, 1792, 1642, 1536, 1453, 1386, 1329, 1280, 1236, 1197, 1162, 1130, 1100, 1073, 1047,
    1024, 1001, 980, 960, 941, 923, 906, 889, 874, 859, 844, 830, 817, 804, 791, 779, 768, 756,
    745, 734, 724, 714, 704, 694, 685, 676, 667, 658, 650, 642, 633, 626, 618, 610, 603, 595, 588,
    581, 574, 567, 561, 554, 548, 542, 535, 529, 523, 517, 512, 506, 500, 495, 489, 484, 478, 473,
    468, 463, 458, 453, 448, 443, 438, 434, 429, 424, 420, 415, 411, 407, 402, 398, 394, 390, 386,
    382, 377, 373, 370, 366, 362, 358, 354, 350, 347, 343, 339, 336, 332, 329, 325, 322, 318, 315,
    311, 308, 305, 302, 298, 295, 292, 289, 286, 282, 279, 276, 273, 270, 267, 264, 261, 258, 256,
    253, 250, 247, 244, 241, 239, 236, 233, 230, 228, 225, 222, 220, 217, 215, 212, 209, 207, 204,
    202, 199, 197, 194, 192, 190, 187, 185, 182, 180, 178, 175, 173, 171, 168, 166, 164, 162, 159,
    157, 155, 153, 151, 149, 146, 144, 142, 140, 138, 136, 134, 132, 130, 128, 126, 123, 121, 119,
    117, 115, 114, 112, 110, 108, 106, 104, 102, 100, 98, 96, 94, 93, 91, 89, 87, 85, 83, 82, 80,
    78, 76, 74, 73, 71, 69, 67, 66, 64, 62, 61, 59, 57, 55, 54, 52, 50, 49, 47, 46, 44, 42, 41, 39,
    37, 36, 34, 33, 31, 30, 28, 26, 25, 23, 22, 20, 19, 17, 16, 14, 13, 11, 10, 8, 7, 5, 4, 2, 1,
];

static PREDEFINED_SEQUENCE_CTABLES: OnceLock<PredefinedSequenceCTables> = OnceLock::new();
static PREDEFINED_SEQUENCE_DTABLES: OnceLock<PredefinedSequenceDTables> = OnceLock::new();
static PREDEFINED_SEQ_DTABLES: OnceLock<PredefinedSeqDTables> = OnceLock::new();

/// The three tables the format defines, built once and shared from there.
///
/// Held as [`SequenceEncodingTable`] rather than raw `fse::CTable` so that
/// choosing predefined mode for a block is a refcount bump. Their coverage
/// array is all-true and nothing reads it: a block that encodes with a
/// predefined table never persists it, because there is nothing for the next
/// block to reuse that it could not name for itself.
struct PredefinedSequenceCTables {
    literal_lengths: SequenceEncodingTable,
    offsets: SequenceEncodingTable,
    match_lengths: SequenceEncodingTable,
}

struct PredefinedSequenceDTables {
    literal_lengths: fse::DTable,
    offsets: fse::DTable,
    match_lengths: fse::DTable,
}

/// Pre-computed SequenceDTables for Predefined mode. Built once on first use,
/// avoiding per-block DTable→SequenceDTable conversion for Predefined tables.
struct PredefinedSeqDTables {
    literal_lengths: fse::SequenceDTable,
    offsets: fse::SequenceDTable,
    match_lengths: fse::SequenceDTable,
}

#[derive(Debug, Clone, Copy)]
struct EncodedSequence {
    ll_code: u8,
    ll_extra: u32,
    of_code: u8,
    of_extra: u32,
    ml_code: u8,
    ml_extra: u32,
}

#[derive(Default)]
pub(crate) struct SequenceEncodeScratch {
    encoded: Vec<EncodedSequence>,
    literal_codes: Vec<u8>,
    offset_codes: Vec<u8>,
    match_codes: Vec<u8>,
    /// When true, codes are precomputed in the store and the bitstream can
    /// be encoded directly from code slices + SequenceCommands, skipping
    /// the intermediate `encoded` Vec.
    use_direct_path: bool,
    /// Table allocations recycled across blocks; see [`SequenceTablePool`].
    table_pool: SequenceTablePool,
}

#[derive(Clone)]
struct SequenceTableChoice {
    mode: CompressionMode,
    header: Vec<u8>,
    encoding: SequenceEncodingTable,
    /// If `Some`, the encoding table should be persisted into the next block's
    /// state with this repeat mode.  `None` for predefined/RLE (not reusable).
    persist_mode: Option<SequenceRepeatMode>,
}

impl SequenceTableChoice {
    /// Move the encoding table into a `SequenceEncodingPartState` for the next
    /// block, consuming this choice.  Returns `None` for predefined/RLE modes.
    fn into_part_state(self) -> Option<SequenceEncodingPartState> {
        self.persist_mode
            .map(|mode| SequenceEncodingPartState::new(self.encoding, mode))
    }
}

struct SequenceStatisticsBuild {
    literal_choice: SequenceTableChoice,
    offset_choice: SequenceTableChoice,
    match_choice: SequenceTableChoice,
    stats: SequenceSectionStats,
    /// Raw payload bits the block's sequences will write, summed over all of
    /// them. Sizes the bitstream buffer; see [`extra_bits_from_code_counts`].
    extra_bits: usize,
}

impl SequenceStatisticsBuild {
    /// Consume the choices and move their encoding tables into the next
    /// block's `SequenceEncodingState`.  Must be called after the encoding
    /// tables have been used for bitstream encoding.
    fn into_next_state(self) -> (SequenceEncodingState, SequenceSectionStats) {
        let next_state = SequenceEncodingState::with_states(
            self.literal_choice.into_part_state(),
            self.offset_choice.into_part_state(),
            self.match_choice.into_part_state(),
        );
        (next_state, self.stats)
    }
}

/// Where the sequence section's time went, for the profiling entry points.
///
/// The pieces line up with upstream's own boundaries, so a stage comparison has
/// something to compare against: `statistics` is `ZSTD_buildSequencesStatistics`
/// less `ZSTD_seqToCodes`, `bitstream` is `ZSTD_encodeSequences`, and `assembly`
/// has no upstream counterpart at all -- C writes the section straight into the
/// output buffer where this path stages it in a scratch `Vec` and copies. Kept
/// separate so a comparison can set that copy aside rather than charge it to
/// either of the other two.
///
/// Filling these costs the shipped encoder nothing: the only callers of the
/// function that reports them are the trace and profile entry points. The real
/// path goes through [`encode_sequence_section_direct`], which shares both timed
/// pieces and writes without the copy.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SequenceSectionTimings {
    pub(crate) statistics: Duration,
    pub(crate) bitstream: Duration,
    pub(crate) assembly: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SequenceSectionStats {
    pub(crate) header_size: usize,
    pub(crate) bitstream_size: usize,
    pub(crate) last_count_size: usize,
    pub(crate) long_offsets: bool,
    pub(crate) modes: Option<SequenceCompressionModes>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SequenceRepeatMode {
    #[default]
    None,
    Check,
    Valid,
}

/// A part's FSE compression table, and which symbols it can encode.
///
/// Shared rather than owned. `fse::CTable` holds its state table and its 256
/// symbol transforms inline, so the pair is 3332 bytes and every clone or move
/// of it by value was a `memcpy` of all of it. Three paths did that on the
/// per-block hot path -- predefined mode cloning one of the three tables the
/// format defines out of its `OnceLock`, repeat mode carrying the previous
/// block's table forward, and a dictionary handing over its encoding state once
/// a frame -- and on a corpus whose blocks carry a single sequence it was 91%
/// of the samples in the table-building stage. Behind an `Arc` all three are an
/// atomic increment, and the one real copy left is building a new table into
/// its allocation.
#[derive(Clone)]
pub(crate) struct SequenceEncodingTable {
    inner: Arc<SequenceEncodingTableInner>,
}

struct SequenceEncodingTableInner {
    table: fse::CTable,
    supported_symbols: [bool; fse::SYMBOLVALUE_MAX + 1],
}

impl Default for SequenceEncodingTableInner {
    fn default() -> Self {
        Self {
            table: fse::CTable::default(),
            supported_symbols: [false; fse::SYMBOLVALUE_MAX + 1],
        }
    }
}

impl SequenceEncodingTableInner {
    /// Overwrite this table in place from a normalized distribution.
    ///
    /// Everything here is written rather than accumulated, so a recycled table
    /// needs no separate wipe first: `fse::build_ctable` clears the state table
    /// and the symbol transforms itself, and the coverage array is refilled
    /// from scratch.
    fn fill_from_normalized_counts(
        &mut self,
        normalized: &[i16; fse::SYMBOLVALUE_MAX + 1],
        max_symbol_value: u32,
        table_log: u32,
    ) -> Result<()> {
        fse::build_ctable(&mut self.table, normalized, max_symbol_value, table_log)?;
        self.supported_symbols.fill(false);
        for symbol in 0..=max_symbol_value as usize {
            self.supported_symbols[symbol] = normalized[symbol] != 0;
        }
        Ok(())
    }
}

/// How many spare table allocations the encoder keeps per sequence part.
///
/// Two, because one is never free when it is wanted. The table a block builds
/// is still held by the encoding state while the *next* block builds its
/// replacement, and only comes free once that replacement has taken its place.
/// Alternating between two settles into no allocation at all from the third
/// block on; with one it would allocate every block.
const SEQUENCE_TABLE_POOL_DEPTH: usize = 2;

/// Recycled storage for the FSE tables a block builds.
///
/// A built table outlives the block that built it, because the next block may
/// reuse it under repeat mode, so it has to be owned rather than borrowed from
/// scratch -- which makes it a heap allocation. One per part per block would
/// put a 3 KB allocation on the block hot path, and
/// `a_warm_slice_encode_never_allocates_anything_output_sized` rejects that,
/// rightly: nothing on this path should allocate anything near the size of the
/// output. So the encoder keeps its own and rebuilds into whichever copy
/// nothing else still refers to.
#[derive(Default)]
struct SequenceTablePool {
    slots: [[Option<Arc<SequenceEncodingTableInner>>; SEQUENCE_TABLE_POOL_DEPTH]; 3],
}

impl SequenceTablePool {
    /// Build a table for `part`, reusing an allocation when one is free.
    ///
    /// A slot is free when the pool holds the only reference to it: whatever
    /// block last built there has finished, and nothing can be reading the
    /// table while it is overwritten. The slots are searched rather than taken
    /// in turn, because taking them in turn allocates forever on a frame of a
    /// single block -- the cursor lands on the other slot every frame, and the
    /// other slot is the one that was never filled.
    fn build(
        &mut self,
        part: SequencePart,
        fill: impl FnOnce(&mut SequenceEncodingTableInner) -> Result<()>,
    ) -> Result<SequenceEncodingTable> {
        let slots = &mut self.slots[part.index()];
        let chosen = slots
            .iter()
            .position(|slot| {
                slot.as_ref()
                    .is_some_and(|inner| Arc::strong_count(inner) == 1)
            })
            .or_else(|| slots.iter().position(Option::is_none))
            // Every slot is still in use by an earlier block, so this one is
            // replaced rather than reused and the old table lives on with its
            // other holder.
            .unwrap_or(0);

        let mut inner = slots[chosen]
            .take()
            .filter(|inner| Arc::strong_count(inner) == 1)
            .unwrap_or_default();
        fill(Arc::get_mut(&mut inner).expect("a reused table has no other holder"))?;
        slots[chosen] = Some(Arc::clone(&inner));
        Ok(SequenceEncodingTable { inner })
    }
}

#[derive(Clone)]
pub(crate) struct SequenceEncodingPartState {
    table: SequenceEncodingTable,
    repeat_mode: SequenceRepeatMode,
}

pub(crate) struct SequenceCodeStats {
    pub(crate) counts: [u32; fse::SYMBOLVALUE_MAX + 1],
    pub(crate) max_symbol: u8,
    pub(crate) most_frequent: usize,
    pub(crate) total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SequencePart {
    LiteralLength,
    Offset,
    MatchLength,
}

impl SequencePart {
    /// Position in the per-part arrays [`SequenceTablePool`] keeps.
    fn index(self) -> usize {
        match self {
            Self::LiteralLength => 0,
            Self::Offset => 1,
            Self::MatchLength => 2,
        }
    }
}

/// Which of the two decode table representations a caller needs left behind.
///
/// Every FSE mode can be turned into either a plain [`fse::DTable`] or a
/// [`fse::SequenceDTable`], and each is built by its own full symbol-spreading
/// pass over the table. Building both costs twice what building one does.
///
/// The fused decoder reads only the `SequenceDTable`s, so for it the `DTable`
/// is a table nothing ever looks at: on `json-records` at level 16 it was 9% of
/// decode time. The paths that materialize a `SequenceCommand` buffer before
/// executing it still start from the plain `DTable`, so they ask for both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TableTarget {
    /// Build only the `SequenceDTable`s, and clear the `DTable` slot that would
    /// have been rebuilt so a table left by an earlier block cannot be mistaken
    /// for this one's. Repeat mode is untouched either way: it rebuilds nothing,
    /// and the dictionary path relies on the `DTable` it carried in surviving.
    SequenceOnly,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompressionMode {
    Predefined,
    Rle,
    FseCompressed,
    Repeat,
}

impl CompressionMode {
    fn from_bits(bits: u8) -> Self {
        match bits & 0x3 {
            0 => Self::Predefined,
            1 => Self::Rle,
            2 => Self::FseCompressed,
            3 => Self::Repeat,
            _ => unreachable!(),
        }
    }

    fn bits(self) -> u8 {
        match self {
            Self::Predefined => 0,
            Self::Rle => 1,
            Self::FseCompressed => 2,
            Self::Repeat => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SequenceCompressionModes {
    pub(crate) literal_lengths: CompressionMode,
    pub(crate) offsets: CompressionMode,
    pub(crate) match_lengths: CompressionMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SequenceCommand {
    pub(crate) literal_length: u32,
    pub(crate) offset_value: u32,
    pub(crate) match_length: u32,
}

#[derive(Clone)]
pub(crate) struct SequenceTablesState {
    literal_lengths: Option<fse::DTable>,
    offsets: Option<fse::DTable>,
    match_lengths: Option<fse::DTable>,
    /// Cached SequenceDTables — rebuilt only when the underlying DTable changes.
    /// Storing these here (rather than as MaybeUninit on the stack) avoids a 25KB
    /// stack frame with page-probe loop on every block entry, and allows reuse
    /// across Repeat-mode blocks.
    seq_ll: fse::SequenceDTable,
    seq_of: fse::SequenceDTable,
    seq_ml: fse::SequenceDTable,
    /// Dirty flags: when true, the corresponding SequenceDTable must be rebuilt
    /// from the DTable before the next decode. Set when parse_sequence_section
    /// updates a DTable (Predefined/RLE/FSE_Compressed modes), cleared after
    /// rebuild. Repeat-mode leaves the flag unset, skipping the rebuild.
    ll_dirty: bool,
    of_dirty: bool,
    ml_dirty: bool,
    /// Has-table flags for the direct SequenceDTable build path (decode-only).
    /// These track whether seq_ll/seq_of/seq_ml have been initialized, replacing
    /// the Option<DTable> None check for Repeat mode validation.
    has_ll: bool,
    has_of: bool,
    has_ml: bool,
}

impl Default for SequenceTablesState {
    fn default() -> Self {
        Self {
            literal_lengths: None,
            offsets: None,
            match_lengths: None,
            seq_ll: fse::SequenceDTable::default(),
            seq_of: fse::SequenceDTable::default(),
            seq_ml: fse::SequenceDTable::default(),
            ll_dirty: true,
            of_dirty: true,
            ml_dirty: true,
            has_ll: false,
            has_of: false,
            has_ml: false,
        }
    }
}

impl SequenceTablesState {
    pub(crate) fn with_tables(
        literal_lengths: Option<fse::DTable>,
        offsets: Option<fse::DTable>,
        match_lengths: Option<fse::DTable>,
    ) -> Self {
        let has_ll = literal_lengths.is_some();
        let has_of = offsets.is_some();
        let has_ml = match_lengths.is_some();
        Self {
            literal_lengths,
            offsets,
            match_lengths,
            seq_ll: fse::SequenceDTable::default(),
            seq_of: fse::SequenceDTable::default(),
            seq_ml: fse::SequenceDTable::default(),
            ll_dirty: true,
            of_dirty: true,
            ml_dirty: true,
            has_ll,
            has_of,
            has_ml,
        }
    }
}

#[derive(Default, Clone)]
pub(crate) struct SequenceEncodingState {
    literal_lengths: Option<SequenceEncodingPartState>,
    offsets: Option<SequenceEncodingPartState>,
    match_lengths: Option<SequenceEncodingPartState>,
}

impl SequenceEncodingState {
    pub(crate) fn with_states(
        literal_lengths: Option<SequenceEncodingPartState>,
        offsets: Option<SequenceEncodingPartState>,
        match_lengths: Option<SequenceEncodingPartState>,
    ) -> Self {
        Self {
            literal_lengths,
            offsets,
            match_lengths,
        }
    }

    fn entry(&self, part: SequencePart) -> Option<&SequenceEncodingPartState> {
        match part {
            SequencePart::LiteralLength => self.literal_lengths.as_ref(),
            SequencePart::Offset => self.offsets.as_ref(),
            SequencePart::MatchLength => self.match_lengths.as_ref(),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn table(&self, part: SequencePart) -> Option<&SequenceEncodingTable> {
        self.entry(part).map(SequenceEncodingPartState::table)
    }
}

impl SequenceEncodingTable {
    fn new(table: fse::CTable, supported_symbols: [bool; fse::SYMBOLVALUE_MAX + 1]) -> Self {
        Self {
            inner: Arc::new(SequenceEncodingTableInner {
                table,
                supported_symbols,
            }),
        }
    }

    /// The table itself, for the encoder and the cost model.
    fn ctable(&self) -> &fse::CTable {
        &self.inner.table
    }

    fn supports_all(&self, codes: &[u8]) -> bool {
        codes
            .iter()
            .all(|&code| self.inner.supported_symbols[code as usize])
    }

    /// Check support using a histogram: only check symbols that actually appear.
    /// This is O(max_symbol) instead of O(n_sequences).
    fn supports_all_from_stats(&self, stats: &SequenceCodeStats) -> bool {
        for symbol in 0..=stats.max_symbol as usize {
            if stats.counts[symbol] > 0 && !self.inner.supported_symbols[symbol] {
                return false;
            }
        }
        true
    }

    /// Build an owned table, allocating for it.
    ///
    /// For the paths that build a table once and keep it -- a dictionary's
    /// encoding state, and tests. The block encoder goes through
    /// [`SequenceTablePool`] instead, which recycles its allocations.
    pub(crate) fn from_normalized_counts(
        normalized: &[i16; fse::SYMBOLVALUE_MAX + 1],
        max_symbol_value: u32,
        table_log: u32,
    ) -> Result<Self> {
        let mut inner = SequenceEncodingTableInner::default();
        inner.fill_from_normalized_counts(normalized, max_symbol_value, table_log)?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }
}

impl SequenceEncodingPartState {
    pub(crate) fn new(table: SequenceEncodingTable, repeat_mode: SequenceRepeatMode) -> Self {
        Self { table, repeat_mode }
    }

    fn table(&self) -> &SequenceEncodingTable {
        &self.table
    }

    fn repeat_mode(&self) -> SequenceRepeatMode {
        self.repeat_mode
    }

    /// Access the underlying FSE encoding table for cost estimation.
    pub(crate) fn fse_ctable(&self) -> &fse::CTable {
        self.table.ctable()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedSequenceSection<'a> {
    pub(crate) number_of_sequences: usize,
    pub(crate) header_size: usize,
    pub(crate) modes: Option<SequenceCompressionModes>,
    pub(crate) bitstream: &'a [u8],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DecodedSequenceStats {
    pub(crate) total_match_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RepeatOffsets {
    values: [u32; 3],
}

impl Default for RepeatOffsets {
    fn default() -> Self {
        Self { values: [1, 4, 8] }
    }
}

impl RepeatOffsets {
    pub(crate) fn from_values(values: [u32; 3]) -> Self {
        Self { values }
    }

    pub(crate) fn values(self) -> [u32; 3] {
        self.values
    }

    /// Encode raw_offset into offset_value AND update repeat state in one pass.
    /// Classify raw_offset as repcode or explicit offset and return the
    /// offset_value, WITHOUT mutating the repeat offset state. Used during
    /// match collection where we need the offset classification but defer
    /// rep-state updates to the DP pricing step (matching C's lazy approach).
    #[inline(always)]
    pub(crate) fn classify_offset(&self, raw_offset: u32, literal_length: u32) -> u32 {
        debug_assert!(raw_offset > 0, "raw_offset must be non-zero");
        let [rep1, rep2, rep3] = self.values;
        let ll_zero = literal_length == 0;

        if !ll_zero && raw_offset == rep1 {
            return 1;
        }
        if raw_offset == rep2 {
            return if ll_zero { 1 } else { 2 };
        }
        if raw_offset == rep3 {
            return if ll_zero { 2 } else { 3 };
        }
        if ll_zero && rep1 > 1 && raw_offset == rep1 - 1 {
            return 3;
        }
        raw_offset + 3
    }

    /// Combines classification + state update without the round-trip through
    /// `resolve()`, matching C zstd's inline rep-offset update in `ZSTD_storeSeq`.
    #[inline(always)]
    pub(crate) fn encode_offset_value_and_update(
        &mut self,
        raw_offset: u32,
        literal_length: u32,
    ) -> u32 {
        debug_assert!(raw_offset > 0, "raw_offset must be non-zero");
        let [rep1, rep2, rep3] = self.values;
        let ll_zero = literal_length == 0;

        if !ll_zero && raw_offset == rep1 {
            // rep1 match with ll>0: offset_value=1, no state change
            return 1;
        }
        if raw_offset == rep2 {
            // rep2 match: rotate rep2 to front
            self.values = [rep2, rep1, rep3];
            return if ll_zero { 1 } else { 2 };
        }
        if raw_offset == rep3 {
            // rep3 match: rotate rep3 to front
            self.values = [rep3, rep1, rep2];
            return if ll_zero { 2 } else { 3 };
        }
        if ll_zero && rep1 > 1 && raw_offset == rep1 - 1 {
            // rep1-1 special case
            self.values = [rep1 - 1, rep1, rep2];
            return 3;
        }
        // Explicit offset: shift rep offsets
        self.values = [raw_offset, rep1, rep2];
        raw_offset + 3
    }

    /// Update repeat state for a known-explicit offset (not a rep match).
    /// Skips rep-offset comparison entirely — just shifts and returns offbase.
    #[inline(always)]
    pub(crate) fn update_explicit_offset(&mut self, raw_offset: u32) -> u32 {
        debug_assert!(raw_offset > 0, "raw_offset must be non-zero");
        let [rep1, rep2, _] = self.values;
        self.values = [raw_offset, rep1, rep2];
        raw_offset + 3
    }

    pub(crate) fn encode_offset_value(
        &mut self,
        raw_offset: u32,
        literal_length: u32,
    ) -> Result<u32> {
        if raw_offset == 0 {
            return Err(Error::InvalidParameter("raw_offset must be non-zero"));
        }
        Ok(self.encode_offset_value_and_update(raw_offset, literal_length))
    }

    /// Infallible zero-literal rep2 resolve for encoder hot paths.
    #[inline(always)]
    /// Checked counterpart to [`Self::resolve_zero_literal_rep2_encode`], kept
    /// so the tests can assert the two agree on well-formed input and diverge
    /// only on the `rep2 == 0` case the encoder's `debug_assert` assumes away.
    /// Nothing on the decode path calls it, hence `cfg(test)`.
    #[cfg(test)]
    pub(crate) fn resolve_zero_literal_rep2(&mut self) -> Result<u32> {
        let [rep1, rep2, rep3] = self.values;
        if rep2 == 0 {
            return Err(Error::Corruption("repeat offset 2 is zero"));
        }
        self.values = [rep2, rep1, rep3];
        Ok(rep2)
    }

    pub(crate) fn resolve_zero_literal_rep2_encode(&mut self) -> u32 {
        let [rep1, rep2, rep3] = self.values;
        debug_assert!(rep2 > 0, "repeat offset 2 is zero");
        self.values = [rep2, rep1, rep3];
        rep2
    }
}

pub(crate) fn parse_sequence_count(src: &[u8]) -> Result<(usize, usize)> {
    let byte0 = *src.first().ok_or(Error::UnexpectedEof)?;
    if byte0 < 128 {
        Ok((byte0 as usize, 1))
    } else if byte0 < 255 {
        let byte1 = *src.get(1).ok_or(Error::UnexpectedEof)?;
        Ok((((byte0 as usize - 128) << 8) + byte1 as usize, 2))
    } else {
        let byte1 = *src.get(1).ok_or(Error::UnexpectedEof)?;
        let byte2 = *src.get(2).ok_or(Error::UnexpectedEof)?;
        Ok((0x7F00 + byte1 as usize + ((byte2 as usize) << 8), 3))
    }
}

pub(crate) fn parse_sequence_section<'a>(
    src: &'a [u8],
    tables: &mut SequenceTablesState,
    target: TableTarget,
) -> Result<ParsedSequenceSection<'a>> {
    let (number_of_sequences, count_size) = parse_sequence_count(src)?;
    if number_of_sequences == 0 {
        return Ok(ParsedSequenceSection {
            number_of_sequences,
            header_size: count_size,
            modes: None,
            bitstream: &[],
        });
    }

    let modes_byte = *src.get(count_size).ok_or(Error::UnexpectedEof)?;
    if (modes_byte & 0x3) != 0 {
        return Err(Error::Corruption(
            "sequence compression modes reserved bits are set",
        ));
    }

    let modes = SequenceCompressionModes {
        literal_lengths: CompressionMode::from_bits(modes_byte >> 6),
        offsets: CompressionMode::from_bits((modes_byte >> 4) & 0x3),
        match_lengths: CompressionMode::from_bits((modes_byte >> 2) & 0x3),
    };

    let mut cursor = count_size + 1;
    // Build SequenceDTables directly, and the DTable slots only when `target`
    // says a caller will read them. Either way this avoids the separate
    // DTable→SequenceDTable conversion step in decode_setup.
    decode_seq_table_both(
        &mut tables.literal_lengths,
        &mut tables.seq_ll,
        &mut tables.has_ll,
        src,
        &mut cursor,
        SequencePart::LiteralLength,
        modes.literal_lengths,
        target,
    )?;
    // dirty=false: SequenceDTable is already built by the direct path.
    if modes.literal_lengths != CompressionMode::Repeat {
        tables.ll_dirty = false;
    }
    decode_seq_table_both(
        &mut tables.offsets,
        &mut tables.seq_of,
        &mut tables.has_of,
        src,
        &mut cursor,
        SequencePart::Offset,
        modes.offsets,
        target,
    )?;
    if modes.offsets != CompressionMode::Repeat {
        tables.of_dirty = false;
    }
    decode_seq_table_both(
        &mut tables.match_lengths,
        &mut tables.seq_ml,
        &mut tables.has_ml,
        src,
        &mut cursor,
        SequencePart::MatchLength,
        modes.match_lengths,
        target,
    )?;
    if modes.match_lengths != CompressionMode::Repeat {
        tables.ml_dirty = false;
    }

    let bitstream = src.get(cursor..).ok_or(Error::UnexpectedEof)?;
    if bitstream.is_empty() {
        return Err(Error::UnexpectedEof);
    }

    Ok(ParsedSequenceSection {
        number_of_sequences,
        header_size: cursor,
        modes: Some(modes),
        bitstream,
    })
}

/// Decode a block's sequences into a freshly allocated list.
///
/// Only the paths that inspect the sequences rather than execute them: the
/// encoder's round-trip checks and the fuzz targets. Decoding proper goes
/// through [`decode_and_execute_sequences`], which never materializes the list
/// at all.
#[cfg(any(test, feature = "internal-fuzz"))]
pub(crate) fn decode_sequence_commands(
    section: &ParsedSequenceSection<'_>,
    tables: &SequenceTablesState,
) -> Result<Vec<SequenceCommand>> {
    let mut sequences = Vec::new();
    decode_sequence_commands_into_stats(section, tables, &mut sequences)?;
    Ok(sequences)
}

pub(crate) fn decode_sequence_commands_into_stats(
    section: &ParsedSequenceSection<'_>,
    tables: &SequenceTablesState,
    sequences: &mut Vec<SequenceCommand>,
) -> Result<DecodedSequenceStats> {
    sequences.clear();
    if section.number_of_sequences == 0 {
        return Ok(DecodedSequenceStats::default());
    }

    let literal_lengths = tables
        .literal_lengths
        .as_ref()
        .ok_or(Error::Corruption("missing literal-length FSE table"))?;
    let offsets = tables
        .offsets
        .as_ref()
        .ok_or(Error::Corruption("missing offset FSE table"))?;
    let match_lengths = tables
        .match_lengths
        .as_ref()
        .ok_or(Error::Corruption("missing match-length FSE table"))?;

    let mut reader = crate::entropy::bitstream::BitDStream::new(section.bitstream)?;
    let mut literal_state = fse::init_dstate(&mut reader, literal_lengths);
    let mut offset_state = fse::init_dstate(&mut reader, offsets);
    let mut match_state = fse::init_dstate(&mut reader, match_lengths);

    sequences.reserve(section.number_of_sequences);
    let mut total_match_bytes = 0usize;
    for index in 0..section.number_of_sequences {
        let literal_entry = fse::peek_entry_fast(&literal_state, literal_lengths);
        let offset_entry = fse::peek_entry_fast(&offset_state, offsets);
        let match_entry = fse::peek_entry_fast(&match_state, match_lengths);

        let literal_code = literal_entry.symbol as usize;
        let offset_code = offset_entry.symbol as usize;
        let match_code = match_entry.symbol as usize;

        // Symbol bounds are validated during FSE table construction, so no
        // per-sequence range checks are needed here.

        let match_extra_bits = u32::from(MATCH_LENGTH_EXTRA_BITS[match_code]);
        let literal_extra_bits = u32::from(LITERAL_LENGTH_EXTRA_BITS[literal_code]);

        let offset_extra = reader.read_bits_fast_zero_safe(offset_code as u32) as u32;
        let match_extra = reader.read_bits_fast_zero_safe(match_extra_bits) as u32;
        if literal_extra_bits != 0 && !reader.can_read_fast(literal_extra_bits) {
            let _ = reader.reload();
        }
        let literal_extra = reader.read_bits_fast_zero_safe(literal_extra_bits) as u32;

        let match_length = MATCH_LENGTH_BASELINES[match_code] + match_extra;
        total_match_bytes = total_match_bytes
            .checked_add(match_length as usize)
            .ok_or(Error::OutputSizeOverflow)?;
        sequences.push(SequenceCommand {
            literal_length: LITERAL_LENGTH_BASELINES[literal_code] + literal_extra,
            offset_value: (1u32 << offset_code) + offset_extra,
            match_length,
        });

        if index + 1 != section.number_of_sequences {
            fse::update_state_with_entry_fast(&mut literal_state, &mut reader, literal_entry);
            fse::update_state_with_entry_fast(&mut match_state, &mut reader, match_entry);
            fse::update_state_with_entry_fast(&mut offset_state, &mut reader, offset_entry);
            if reader.reload() == BitDStreamStatus::Overflow {
                return Err(corruption_error("sequence bitstream overflow"));
            }
        }
    }
    if reader.reload() == BitDStreamStatus::Overflow || !reader.end_of_stream() {
        return Err(corruption_error(
            "sequence bitstream was not fully consumed",
        ));
    }

    Ok(DecodedSequenceStats { total_match_bytes })
}

pub(crate) fn decode_and_execute_sequences(
    section: &ParsedSequenceSection<'_>,
    tables: &mut SequenceTablesState,
    out: &mut DecodeOut<'_>,
    frame_start: usize,
    window_size: usize,
    block_size_max: usize,
    dictionary: Option<&[u8]>,
    literals: DecodedLiterals<'_>,
    repeat_offsets: &mut RepeatOffsets,
    limit: OutputLimit,
) -> Result<()> {
    if section.number_of_sequences == 0 {
        let mut remaining_block_bytes = block_size_max;
        let mut remaining_output_bytes = limit.remaining(out.len())?;
        return append_trailing_literals(
            out,
            literals.as_slice(),
            0,
            &mut remaining_block_bytes,
            &mut remaining_output_bytes,
            limit,
        );
    }
    decode_and_execute_sequences_unified(
        section,
        tables,
        out,
        frame_start,
        window_size,
        block_size_max,
        dictionary,
        literals,
        repeat_offsets,
        limit,
    )
}

/// Reproduce a long match by doubling instead of by a fixed-stride wildcopy.
///
/// Everything a match produces is periodic with period `offset`, so once `done`
/// of its bytes exist, the next `done` are a copy of what is already there —
/// from a distance that doubles every round. The wildcopies in
/// [`copy_match_inline`] advance a fixed 32 or 8 bytes per iteration however
/// long the run is, so a 128 KiB match costs thousands of iterations where this
/// costs about a dozen `memcpy` calls. An offset at least as long as the match
/// degenerates to the single `memcpy` it always was.
///
/// C zstd copies the same way this used to, which is why `small-alphabet` sat
/// at parity with upstream in `BENCHMARKS.md` and read as having nothing left
/// to win: it is a shape the reference implementation is equally slow on.
///
/// Outlined and `#[cold]` deliberately. Inlined, the extra branch and its body
/// cost 10-20% across every other corpus — measured, and measured again with
/// the threshold raised sixteenfold, which changed nothing and so ruled out the
/// path itself being taken. What it costs is code size and register pressure in
/// the decoder's hottest function, not work.
///
/// # Safety
///
/// Same contract as [`copy_match_inline`], of which this is one branch:
/// `src` is `offset` bytes behind `dst`, everything before `dst` is
/// initialized, and `dst[..match_length]` lies within the caller's
/// reservation. This branch writes exactly `match_length` bytes and so does not
/// use the trailing wildcopy slack at all.
#[allow(unsafe_code)]
#[cold]
#[inline(never)]
unsafe fn copy_match_by_doubling(dst: *mut u8, src: *const u8, offset: usize, match_length: usize) {
    // SAFETY: the seed copy takes at most `offset` bytes from `offset` behind
    // `dst`, so source and destination abut rather than overlap. Each round
    // after it copies `chunk <= done` bytes from `dst` to `dst + done`, which
    // for the same reason cannot overlap either. Every write stays within
    // `dst[..match_length]`.
    unsafe {
        let seed = offset.min(match_length);
        core::ptr::copy_nonoverlapping(src, dst, seed);
        let mut done = seed;
        while done < match_length {
            let chunk = done.min(match_length - done);
            core::ptr::copy_nonoverlapping(dst, dst.add(done), chunk);
            done += chunk;
        }
    }
}

/// Match length at which the copy switches from a fixed-stride wildcopy to
/// pattern doubling.
///
/// Chosen by measurement, not by counting instructions. Doubling replaces
/// thousands of wildcopy iterations with about a dozen `memcpy` calls, but each
/// call has setup the wildcopy does not, so it only pays on runs long enough to
/// amortize them — and a threshold set too low is not neutral, it is a loss. At
/// 64 it cost 8-14% on `wikipedia` and `trained-dictionary`, whose matches are
/// medium-length; at 4096 every corpus is within about 2% and `small-alphabet`,
/// whose matches are the length of a whole block, keeps its tenfold win.
///
/// Matches this long are the tail of the distribution on ordinary data and the
/// whole of it on runs, which is the case being fixed.
const PATTERN_DOUBLING_MIN: usize = 4096;

/// Copy match data from output history using raw pointers.
///
/// Match copy matching C zstd's `ZSTD_execSequence` structure:
/// - offset >= 16 (WILDCOPY_VECLEN): non-overlapping 16-byte wildcopy
/// - offset < 16: `ZSTD_overlapCopy8` + overlap-safe 8-byte wildcopy
///
/// The small-offset path uses a single `if offset < 8` inside the else
/// branch, matching C's 2-branch structure exactly.
///
/// # Safety
///
/// All of the following must hold:
/// - `base[match_src..out_pos]` contains initialized data, so the match source
///   is readable for the whole copy.
/// - `base[out_pos..out_pos + match_length + WILDCOPY_OVERLENGTH]` lies within
///   the allocation, which `reserve_block_output` is what guarantees. The
///   trailing slack is load-bearing: the final wildcopy of either branch may
///   overshoot `match_length` by up to 31 bytes.
/// - `match_src <= out_pos`, since `offset` is computed as their difference and
///   would underflow otherwise.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn copy_match_inline(base: *mut u8, out_pos: usize, match_src: usize, match_length: usize) {
    let offset = out_pos - match_src;
    // SAFETY: every operation below rests on the one contract documented above
    // — the caller has reserved `out_pos + match_length + WILDCOPY_OVERLENGTH`
    // bytes and initialized everything before `out_pos`. Both pointers are
    // derived from `base` and stay inside that reservation, including the
    // deliberate overshoot of the final wildcopy in each branch. Splitting this
    // into per-operation blocks would repeat that same sentence thirty times
    // rather than justify anything additional.
    unsafe {
        let dst = base.add(out_pos);
        let src = base.add(match_src);

        if match_length >= PATTERN_DOUBLING_MIN {
            copy_match_by_doubling(dst, src, offset, match_length);
            return;
        }

        if offset >= 16 {
            // Non-overlapping: initial copy_16 + unrolled 2× copy_16 loop.
            // The initial copy handles the common short-match case (ml <= 16).
            // The unrolled loop copies 32 bytes per iteration, matching C zstd's
            // ZSTD_wildcopy which does two COPY16 per loop. This halves the
            // branch overhead for long matches (128KB → 4000 vs 8000 iterations).
            // Last copy may overshoot by up to 31 bytes (WILDCOPY_OVERLENGTH).
            copy_16(dst, src);
            if match_length > 16 {
                let mut pos = 16usize;
                loop {
                    copy_16(dst.add(pos), src.add(pos));
                    pos += 16;
                    copy_16(dst.add(pos), src.add(pos));
                    pos += 16;
                    if pos >= match_length {
                        break;
                    }
                }
            }
        } else {
            // ZSTD_overlapCopy8: establish first 8 bytes with op-ip gap >= 8,
            // then overlap-safe 8-byte wildcopy for the remainder.
            let mut ip = src;
            let mut op = dst;

            if offset < 8 {
                // Small overlap: dec32/dec64 table fixup to spread the pattern.
                // Handles all offsets 1-7 including offset==1 (memset pattern).
                const DEC32TABLE: [u32; 8] = [0, 1, 2, 1, 4, 4, 4, 4];
                const DEC64TABLE: [i32; 8] = [8, 8, 8, 7, 8, 9, 10, 11];
                *op = *ip;
                *op.add(1) = *ip.add(1);
                *op.add(2) = *ip.add(2);
                *op.add(3) = *ip.add(3);
                ip = ip.add(DEC32TABLE[offset] as usize);
                core::ptr::copy_nonoverlapping(ip, op.add(4), 4);
                // C's `*ip -= dec64table[offset]` and `*ip += 8`, fused into one
                // move. The difference is negative for offsets 5, 6 and 7, so it
                // has to be applied as a signed `offset`. Routing it through
                // `usize` handed `add` a value near `usize::MAX`, which is
                // undefined no matter where the address lands — and it lands in
                // bounds every time, which is exactly why nothing short of an
                // interpreter could see it.
                ip = ip.offset(8isize - DEC64TABLE[offset] as isize);
            } else {
                // Offset 8-15: gap already >= 8, direct 8-byte copy.
                core::ptr::copy_nonoverlapping(ip, op, 8);
                ip = ip.add(8);
            }
            op = op.add(8);

            // Shared 8-byte wildcopy for remaining bytes.
            if match_length > 8 {
                let end = dst.add(match_length);
                while op < end {
                    core::ptr::copy_nonoverlapping(ip, op, 8);
                    ip = ip.add(8);
                    op = op.add(8);
                }
            }
        }
    }
}

/// Cold path: handle match copy from dictionary history or error when
/// offset exceeds the available output prefix. Extracted from the hot
/// loop to reduce register pressure — LLVM won't allocate registers
/// for dictionary variables in the main loop body.
#[cold]
#[inline(never)]
#[allow(unsafe_code)]
fn execute_dictionary_match(
    out: &mut DecodeOut<'_>,
    out_pos: usize,
    frame_start: usize,
    window_size: usize,
    dictionary: Option<&[u8]>,
    offset: usize,
    match_length: usize,
) -> Result<(usize, *mut u8)> {
    // Sentinel from resolve_offset_branchless: invalid offset (offset_value
    // was 0, or rep1-1 underflowed to zero).
    if offset == OFFSET_SENTINEL {
        return Err(corruption_error("repeat offset 1 minus 1 is zero"));
    }
    let produced_in_frame = out_pos - frame_start;
    if let Some(dict) = dictionary {
        if produced_in_frame > window_size {
            return Err(corruption_error(
                "sequence offset exceeds the available history window",
            ));
        }
        let available_history = dict
            .len()
            .checked_add(produced_in_frame)
            .ok_or_else(output_size_overflow_error)?;
        if offset > available_history {
            return Err(corruption_error(
                "sequence offset exceeds the available history window",
            ));
        }
        // SAFETY: `out_pos` is where the sequence executor has written to, and
        // everything below it is initialized; the appends below need the
        // destination's own length to agree with that before they extend it.
        unsafe {
            out.set_len(out_pos);
        }
        append_match_from_dictionary_history(out, frame_start, dict, offset, match_length)?;
        Ok((out.len(), out.as_mut_ptr()))
    } else {
        Err(corruption_error(
            "sequence offset exceeds the available history window",
        ))
    }
}

/// Execute one sequence without the overshoot the wildcopy path relies on.
///
/// Upstream's `ZSTD_execSequenceEnd`. [`copy_match_inline`] and the literal
/// copy in the main loop both write in 16-byte chunks, so each writes past what
/// the sequence actually produces -- by up to `WILDCOPY_OVERLENGTH - 1` bytes
/// for a match. A growable destination reserves that slack once per block and
/// never notices it; a caller's exact-sized slice has none to give, and the
/// last sequences of the last block would write past its end.
///
/// So the executor measures each sequence against the slack still ahead of it
/// and sends the ones that would run out here, where every copy writes exactly
/// the bytes the sequence names and a destination without room for them reports
/// [`Error::DstSizeTooSmall`] instead of overrunning.
///
/// Cold and outlined for the same reason [`execute_dictionary_match`] is: a
/// growable destination can never reach it, and a fixed one reaches it only for
/// the handful of sequences landing in the final `WILDCOPY_OVERLENGTH` bytes of
/// the whole decode.
#[cold]
#[inline(never)]
#[allow(unsafe_code)]
#[allow(clippy::too_many_arguments)]
fn execute_sequence_exact(
    out: &mut DecodeOut<'_>,
    out_pos: usize,
    literals: *const u8,
    literal_length: usize,
    offset: usize,
    match_length: usize,
    frame_start: usize,
    window_size: usize,
    dictionary: Option<&[u8]>,
) -> Result<(usize, *mut u8)> {
    debug_assert!(
        !out.is_growable(),
        "a growable destination reserves the wildcopy slack, so it never gets here"
    );
    // SAFETY: `out_pos` is what the sequence executor has written up to and
    // everything below it is initialized. The appends below extend from the
    // destination's own length, which has to agree with that first.
    unsafe {
        out.set_len(out_pos);
    }
    // SAFETY: the caller checked `literals + literal_length` against the end of
    // the literals buffer before reaching here, which is the same check the
    // wildcopy path makes. Literals live either in the compressed input or in
    // the decoder's scratch, and neither overlaps the output.
    let literal_run = unsafe { core::slice::from_raw_parts(literals, literal_length) };
    out.append(literal_run)?;

    let out_pos = out.len();
    if offset <= out_pos - frame_start && offset <= window_size {
        append_match_from_history(out, out_pos - offset, match_length)?;
        Ok((out.len(), out.as_mut_ptr()))
    } else {
        execute_dictionary_match(
            out,
            out_pos,
            frame_start,
            window_size,
            dictionary,
            offset,
            match_length,
        )
    }
}

/// Sentinel value for invalid offsets returned by `resolve_offset_branchless`.
/// This must exceed `available_prefix` for any valid frame so the sentinel
/// always falls through to the cold `execute_dictionary_match` error path.
///
/// The zstd spec caps `Window_Log` at 31, giving a max window of 2^31 bytes.
/// `u32::MAX` (2^32−1) exceeds this on both 32-bit and 64-bit platforms,
/// so `available_prefix < OFFSET_SENTINEL` always holds for valid frames.
const OFFSET_SENTINEL: usize = u32::MAX as usize;

/// Resolve rare rep-offset cases (rep2, rep3, rep1-1) during decode.
/// Called only when offset_value <= 3 AND NOT the common rep1 case.
/// Returns `OFFSET_SENTINEL` for invalid offsets (offset_value == 0
/// or rep1-1 underflow), which always fails the prefix check.
#[cold]
#[inline(never)]
fn resolve_rep_offset_decode(
    repeat_offsets: &mut RepeatOffsets,
    offset_value: u32,
    literal_length: usize,
) -> usize {
    let [rep1, rep2, rep3] = repeat_offsets.values;
    if offset_value == 0 {
        return OFFSET_SENTINEL;
    }
    let rep_index = offset_value + (literal_length == 0) as u32;
    if rep_index == 2 {
        repeat_offsets.values = [rep2, rep1, rep3];
        rep2 as usize
    } else if rep_index == 3 {
        repeat_offsets.values = [rep3, rep1, rep2];
        rep3 as usize
    } else {
        if rep1 <= 1 {
            return OFFSET_SENTINEL;
        }
        let o = rep1 - 1;
        repeat_offsets.values = [o, rep1, rep2];
        o as usize
    }
}

/// Fallible setup for the decode loop: validates tables, reserves output,
/// Fallible setup: validates tables, reserves output, initializes bitstream.
/// Builds SequenceDTables into the cached slots in `tables`, writing only
/// active entries to avoid zero-filling 4KB per table.
#[inline(never)]
fn decode_setup(
    tables: &mut SequenceTablesState,
    out: &mut DecodeOut<'_>,
    block_size_max: usize,
    limit: OutputLimit,
) -> Result<(
    usize, // remaining_block_bytes
    usize, // remaining_output_bytes
)> {
    // Build SequenceDTables from DTables only when dirty (dictionary path).
    // The direct path (parse_sequence_section) builds SequenceDTables directly
    // and sets dirty=false, so these branches are skipped for non-dictionary blocks.
    if tables.ll_dirty {
        let ll = tables
            .literal_lengths
            .as_ref()
            .ok_or(Error::Corruption("missing literal-length FSE table"))?;
        fse::build_sequence_dtable_from(
            ll,
            &LITERAL_LENGTH_BASELINES,
            &LITERAL_LENGTH_EXTRA_BITS,
            &mut tables.seq_ll,
        );
        tables.ll_dirty = false;
        tables.has_ll = true;
    } else if !tables.has_ll {
        return Err(Error::Corruption("missing literal-length FSE table"));
    }
    if tables.of_dirty {
        let of = tables
            .offsets
            .as_ref()
            .ok_or(Error::Corruption("missing offset FSE table"))?;
        fse::build_offset_sequence_dtable_from(of, &mut tables.seq_of);
        tables.of_dirty = false;
        tables.has_of = true;
    } else if !tables.has_of {
        return Err(Error::Corruption("missing offset FSE table"));
    }
    if tables.ml_dirty {
        let ml = tables
            .match_lengths
            .as_ref()
            .ok_or(Error::Corruption("missing match-length FSE table"))?;
        fse::build_sequence_dtable_from(
            ml,
            &MATCH_LENGTH_BASELINES,
            &MATCH_LENGTH_EXTRA_BITS,
            &mut tables.seq_ml,
        );
        tables.ml_dirty = false;
        tables.has_ml = true;
    } else if !tables.has_ml {
        return Err(Error::Corruption("missing match-length FSE table"));
    }
    reserve_block_output(out, block_size_max, limit)?;
    let remaining_output_bytes = limit.remaining(out.len())?;
    Ok((block_size_max, remaining_output_bytes))
}

#[allow(unsafe_code)]
fn decode_and_execute_sequences_unified(
    section: &ParsedSequenceSection<'_>,
    tables: &mut SequenceTablesState,
    out: &mut DecodeOut<'_>,
    frame_start: usize,
    window_size: usize,
    block_size_max: usize,
    dictionary: Option<&[u8]>,
    literals: DecodedLiterals<'_>,
    repeat_offsets: &mut RepeatOffsets,
    limit: OutputLimit,
) -> Result<()> {
    debug_assert!(section.number_of_sequences > 0);

    // SequenceDTables are cached in `tables` (persists across blocks).
    // This avoids a 25KB stack frame + page-probe loop per block entry.
    // decode_setup writes only active entries into tables.seq_{ll,of,ml}.
    let (remaining_block_bytes_init, remaining_output_bytes_init) =
        decode_setup(tables, out, block_size_max, limit)?;

    // Highest output position at which the wildcopies may still overshoot by
    // `WILDCOPY_OVERLENGTH`, and the decision of whether anything has to watch
    // for it.
    //
    // `reserve_block_output` has just put a growable destination's whole block
    // under this bound, and a fixed destination is under it for every block but
    // the last one or two of a decode sized to the byte. So the question is
    // settled here, once per block, and the executor is monomorphized on the
    // answer: the common instantiation is the code that was here before any of
    // this existed, with no per-sequence comparison and nothing extra live
    // across the loop. Deciding it per sequence instead cost 1-1.5% on the
    // sequence-dense corpora, which is what this shape exists to avoid.
    let wildcopy_end = out.capacity().saturating_sub(WILDCOPY_OVERLENGTH);
    let block_fits_with_slack =
        out.len() + remaining_block_bytes_init.min(remaining_output_bytes_init) <= wildcopy_end;

    if block_fits_with_slack {
        execute_block_sequences::<false>(
            section,
            tables,
            out,
            frame_start,
            window_size,
            dictionary,
            literals,
            repeat_offsets,
            limit,
            remaining_block_bytes_init,
            remaining_output_bytes_init,
            wildcopy_end,
        )
    } else {
        execute_block_sequences::<true>(
            section,
            tables,
            out,
            frame_start,
            window_size,
            dictionary,
            literals,
            repeat_offsets,
            limit,
            remaining_block_bytes_init,
            remaining_output_bytes_init,
            wildcopy_end,
        )
    }
}

/// Decode and execute every sequence of one block.
///
/// `CHECKED` says whether the destination might run out of wildcopy slack
/// before the block ends. It is `false` for every growable destination and for
/// all but the tail of a fixed one, and that instantiation compiles to exactly
/// the loop that was here before `decode_into_slice`: the comparison below
/// folds away and `execute_sequence_exact` goes with it.
#[allow(unsafe_code)]
#[allow(clippy::too_many_arguments)]
fn execute_block_sequences<const CHECKED: bool>(
    section: &ParsedSequenceSection<'_>,
    tables: &SequenceTablesState,
    out: &mut DecodeOut<'_>,
    frame_start: usize,
    window_size: usize,
    dictionary: Option<&[u8]>,
    literals: DecodedLiterals<'_>,
    repeat_offsets: &mut RepeatOffsets,
    limit: OutputLimit,
    remaining_block_bytes_init: usize,
    remaining_output_bytes_init: usize,
    wildcopy_end: usize,
) -> Result<()> {
    let mut reader = crate::entropy::bitstream::BitDStream::new(section.bitstream)?;
    // Merge two remaining counters into one, matching C's single oend_w.
    // Both decrement by the same `total` each iteration, so min stays correct.
    // The original values are recoverable for append_trailing_literals.
    let merged_budget_init = remaining_block_bytes_init.min(remaining_output_bytes_init);
    let mut remaining_budget = merged_budget_init;

    let mut literal_state = fse::init_dstate_seq(&mut reader, &tables.seq_ll);
    let mut offset_state = fse::init_dstate_seq(&mut reader, &tables.seq_of);
    let mut match_state = fse::init_dstate_seq(&mut reader, &tables.seq_ml);
    // Pointer-based literal tracking: single advancing pointer replaces
    // literal_cursor + lit_base (2 vars → 1). End pointer replaces lit_len.
    // This matches C's litPtr/litEnd approach.
    // `ip` walks the padded slice, because the fixed-width literal copy below
    // reads up to 16 bytes past whatever run it is on. `ip_end` is still the
    // true end of the literals: it bounds how many a block may claim, and
    // padding must not loosen that.
    #[allow(unsafe_code)]
    let mut ip = literals.padded_ptr();
    #[allow(unsafe_code)]
    let ip_end = unsafe { literals.padded_ptr().add(literals.len()) };

    let mut out_base = out.as_mut_ptr();
    let mut out_pos = out.len();

    // Hoist repeat offsets into local variables matching C zstd's aarch64 path
    // (prevOffset0/1/2 in registers). Avoids load/store through &mut RepeatOffsets
    // each iteration, which LLVM treats as potentially aliased memory.
    let [mut rep0, mut rep1, mut rep2] = repeat_offsets.values;

    // Shared execution body: validation, offset resolve, copy literals, copy match.
    // Used by both the main loop and last-sequence path to avoid duplication.
    // Accesses mutable locals (remaining_block_bytes, out_pos, etc.) from the
    // enclosing scope at each call site.
    macro_rules! execute_sequence {
        ($literal_length:expr, $match_length:expr, $offset_value:expr) => {
            let literal_length = $literal_length;
            let match_length = $match_length;
            let offset_value = $offset_value;
            let total = literal_length + match_length;
            // `wrapping_add`, not `add`: a corrupt frame can declare a literal
            // length far past the end of the literals buffer, and `ptr::add` is
            // undefined the moment it leaves the allocation — before the check
            // on the next line gets to reject it. Wrapping keeps the provenance
            // and moves only the address, so `new_ip` is usable once the check
            // has passed. The address cannot itself wrap around: `||` tests
            // `total > remaining_budget` first, which bounds `literal_length`
            // by the block size limit.
            let new_ip = ip.wrapping_add(literal_length);
            if total > remaining_budget || new_ip > ip_end {
                return Err(if new_ip > ip_end {
                    corruption_error("sequence literal length exceeds literals buffer")
                } else {
                    sequence_budget_error(
                        total,
                        remaining_block_bytes_init - (merged_budget_init - remaining_budget),
                        out_pos,
                        limit,
                    )
                });
            }
            remaining_budget -= total;

            let offset = if offset_value > 3 {
                let o = offset_value - 3;
                rep2 = rep1;
                rep1 = rep0;
                rep0 = o;
                o as usize
            } else if offset_value == 1 && literal_length != 0 {
                rep0 as usize
            } else {
                // Cold: uncommon repeat offset patterns. Use a local copy
                // to avoid keeping &mut RepeatOffsets pointer live in loop.
                let mut local_reps = RepeatOffsets {
                    values: [rep0, rep1, rep2],
                };
                let result =
                    resolve_rep_offset_decode(&mut local_reps, offset_value, literal_length);
                [rep0, rep1, rep2] = local_reps.values;
                result
            };

            // `offset` is still unvalidated here — it is checked against the
            // output produced so far and the window below, and a frame that
            // declares a larger one takes the dictionary path. So this address
            // can wrap, and `wrapping_add` is what makes forming it legal:
            // `ptr::add` is undefined behavior once the offset leaves `isize`,
            // before the prefetch it feeds ever runs. Prefetching a wild
            // address is harmless, being a hint that never dereferences.
            unsafe {
                prefetch_match(
                    out_base.wrapping_add((out_pos + literal_length).wrapping_sub(offset)),
                );
            }

            if !CHECKED || out_pos + total <= wildcopy_end {
                // Unconditional literal copy matching C zstd: always copy_16.
                unsafe {
                    let lit_dst = out_base.add(out_pos);
                    copy_16(lit_dst, ip);
                    if literal_length > 16 {
                        let mut pos = 16usize;
                        while pos < literal_length {
                            copy_16(lit_dst.add(pos), ip.add(pos));
                            pos += 16;
                        }
                    }
                }
                out_pos += literal_length;
                ip = new_ip;

                if offset <= out_pos - frame_start && offset <= window_size {
                    let match_src = out_pos - offset;
                    unsafe {
                        copy_match_inline(out_base, out_pos, match_src, match_length);
                    }
                    out_pos += match_length;
                } else {
                    repeat_offsets.values = [rep0, rep1, rep2];
                    (out_pos, out_base) = execute_dictionary_match(
                        out,
                        out_pos,
                        frame_start,
                        window_size,
                        dictionary,
                        offset,
                        match_length,
                    )?;
                }
            } else {
                // Too close to the end of the destination for the wildcopies to
                // overshoot into. See `execute_sequence_exact`.
                repeat_offsets.values = [rep0, rep1, rep2];
                (out_pos, out_base) = execute_sequence_exact(
                    out,
                    out_pos,
                    ip,
                    literal_length,
                    offset,
                    match_length,
                    frame_start,
                    window_size,
                    dictionary,
                )?;
                ip = new_ip;
            }
        };
    }

    // Pre-read FSE entries as raw u64 for the first sequence.
    // Raw u64 forces LLVM to use a single 8-byte load per entry instead of
    // splitting into 3-4 smaller loads (ldrh/ldrb/ldr w on aarch64).
    let mut lit_raw = unsafe { tables.seq_ll.get_entry_raw(literal_state.state) };
    let mut off_raw = unsafe { tables.seq_of.get_entry_raw(offset_state.state) };
    let mut ml_raw = unsafe { tables.seq_ml.get_entry_raw(match_state.state) };

    // Decode extra bits and compute final values from pre-read entries.
    // Separate reads for offset, match, and literal (matching C zstd's order)
    // instead of combining offset+match into one read. This eliminates the
    // shift/mask extraction from the combined value and simplifies LLVM's
    // register allocation, improving throughput for high-sequence-count blocks.
    // Shared by both the main loop and the last-sequence path.
    macro_rules! decode_sequence {
        () => {{
            let offset_code = fse::raw_entry_nb_additional_bits(off_raw);
            let match_extra_bits = fse::raw_entry_nb_additional_bits(ml_raw);
            let literal_extra_bits = fse::raw_entry_nb_additional_bits(lit_raw);

            // Read offset extra bits first (highest in the bitstream).
            // Use conditional read_bits_fast (no mask) instead of
            // read_bits_fast_zero_safe (mask always). The branch is
            // well-predicted and saves 2 instructions per call on the
            // common nb_bits > 0 path.
            let offset_extra = if offset_code > 0 {
                reader.read_bits_fast(offset_code) as u32
            } else {
                0
            };

            // Read match extra bits.
            let match_extra = if match_extra_bits > 0 {
                reader.read_bits_fast(match_extra_bits) as u32
            } else {
                0
            };

            // Conditional reload between match and literal reads.
            let combined_om = offset_code + match_extra_bits;
            if combined_om + literal_extra_bits >= 31 {
                let _ = reader.reload();
            }

            // Read literal extra bits.
            let literal_extra = if literal_extra_bits > 0 {
                reader.read_bits_fast(literal_extra_bits) as u32
            } else {
                0
            };

            let literal_length = (fse::raw_entry_baseline(lit_raw) + literal_extra) as usize;
            let match_length = (fse::raw_entry_baseline(ml_raw) + match_extra) as usize;
            let offset_value = (1u32 << offset_code) + offset_extra;
            (literal_length, match_length, offset_value)
        }};
    }

    // Main loop: sequences 1..N-1. FSE state updates and reload are
    // unconditional, eliminating `is_last` branch overhead per iteration.
    // Aligning to 64 bytes matches C zstd's `.p2align 6`. Skipped under Miri,
    // which has no instruction stream to align and cannot execute asm.
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    unsafe {
        core::arch::asm!(".p2align 6", options(nomem, nostack, preserves_flags));
    }
    let mut remaining_sequences = section.number_of_sequences;
    while remaining_sequences > 1 {
        remaining_sequences -= 1;

        let (literal_length, match_length, offset_value) = decode_sequence!();

        // FSE state updates (unconditional in this loop). All three fit in one
        // read: 9 + 9 + 8 bits, against the accumulator's 57-bit floor.
        let ll_bits = fse::raw_entry_nb_bits(lit_raw);
        let ml_bits = fse::raw_entry_nb_bits(ml_raw);
        let of_bits = fse::raw_entry_nb_bits(off_raw);
        let combined_state_bits = ll_bits + ml_bits + of_bits;
        // Combined state bits is >= 1 for any non-degenerate FSE table,
        // so use read_bits_fast (no zero-safe mask needed). For the
        // all-RLE edge case (combined == 0), state_val would be 0 and
        // new_state + 0 = new_state, which is correct.
        let state_val = if combined_state_bits > 0 {
            reader.read_bits_fast(combined_state_bits) as usize
        } else {
            0
        };
        literal_state.state = fse::raw_entry_new_state(lit_raw)
            + ((state_val >> (ml_bits + of_bits)) & ((1usize << ll_bits).wrapping_sub(1)));
        match_state.state = fse::raw_entry_new_state(ml_raw)
            + ((state_val >> of_bits) & ((1usize << ml_bits).wrapping_sub(1)));
        offset_state.state =
            fse::raw_entry_new_state(off_raw) + (state_val & ((1usize << of_bits).wrapping_sub(1)));
        lit_raw = unsafe { tables.seq_ll.get_entry_raw(literal_state.state) };
        off_raw = unsafe { tables.seq_of.get_entry_raw(offset_state.state) };
        ml_raw = unsafe { tables.seq_ml.get_entry_raw(match_state.state) };

        execute_sequence!(literal_length, match_length, offset_value);

        // Reload AFTER copies. Overflow = corrupt bitstream; don't write back
        // repeat_offsets (caller discards on error). This keeps the
        // &mut RepeatOffsets pointer dead in the hot loop body.
        if reader.reload() == BitDStreamStatus::Overflow {
            return Err(corruption_error("sequence bitstream overflow"));
        }
    }

    // Last sequence: no state updates, no reload. Outlined into a cold
    // function to avoid duplicating the execute_sequence body (~138 insns).
    // Called once per block, so function-call overhead is immeasurable.
    let (
        last_out_pos,
        last_out_base,
        last_rep0,
        last_rep1,
        last_rep2,
        last_ip,
        last_remaining_budget,
    ) = decode_and_execute_last_sequence(
        &mut reader,
        lit_raw,
        off_raw,
        ml_raw,
        out_pos,
        out_base,
        rep0,
        rep1,
        rep2,
        ip,
        ip_end,
        remaining_budget,
        repeat_offsets,
        out,
        wildcopy_end,
        frame_start,
        window_size,
        dictionary,
        remaining_block_bytes_init - (merged_budget_init - remaining_budget),
        limit,
    )?;
    out_pos = last_out_pos;
    let _ = last_out_base;
    rep0 = last_rep0;
    rep1 = last_rep1;
    rep2 = last_rep2;
    ip = last_ip;
    remaining_budget = last_remaining_budget;

    repeat_offsets.values = [rep0, rep1, rep2];
    unsafe {
        out.set_len(out_pos);
    }

    if reader.reload() == BitDStreamStatus::Overflow || !reader.end_of_stream() {
        return Err(corruption_error(
            "sequence bitstream was not fully consumed",
        ));
    }

    // Recover individual remaining counters from merged budget.
    let consumed = remaining_block_bytes_init.min(remaining_output_bytes_init) - remaining_budget;
    let mut remaining_block_bytes = remaining_block_bytes_init - consumed;
    let mut remaining_output_bytes = remaining_output_bytes_init - consumed;
    #[allow(unsafe_code)]
    let literal_cursor = unsafe { ip.offset_from(literals.padded_ptr()) as usize };
    append_trailing_literals(
        out,
        literals.as_slice(),
        literal_cursor,
        &mut remaining_block_bytes,
        &mut remaining_output_bytes,
        limit,
    )
}

/// Cold outlined function for the last sequence of each block. Contains
/// the full decode + execute body so it doesn't need to be duplicated
/// as a macro expansion in the main function. Returns the modified state.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
#[allow(unsafe_code)]
fn decode_and_execute_last_sequence(
    reader: &mut crate::entropy::bitstream::BitDStream<'_>,
    lit_raw: u64,
    off_raw: u64,
    ml_raw: u64,
    mut out_pos: usize,
    mut out_base: *mut u8,
    mut rep0: u32,
    mut rep1: u32,
    mut rep2: u32,
    mut ip: *const u8,
    ip_end: *const u8,
    // Already the minimum of the block-size budget and what `max_output_size`
    // leaves, folded into one counter by the caller.
    mut remaining_budget: usize,
    repeat_offsets: &mut RepeatOffsets,
    out: &mut DecodeOut<'_>,
    // Highest output position the wildcopies may overshoot from; see the
    // matching local in the main loop.
    wildcopy_end: usize,
    frame_start: usize,
    window_size: usize,
    dictionary: Option<&[u8]>,
    // Neither of these is redundant with `remaining_budget`, which is the
    // merge of two limits and cannot say which one it is. Both are needed only
    // to attribute the error when that budget runs out; see
    // `sequence_budget_error`.
    block_remaining: usize,
    limit: OutputLimit,
) -> Result<(usize, *mut u8, u32, u32, u32, *const u8, usize)> {
    // Decode: extract extra bits and compute sequence values.
    let offset_code = fse::raw_entry_nb_additional_bits(off_raw);
    let match_extra_bits = fse::raw_entry_nb_additional_bits(ml_raw);
    let literal_extra_bits = fse::raw_entry_nb_additional_bits(lit_raw);

    let offset_extra = if offset_code > 0 {
        reader.read_bits_fast(offset_code) as u32
    } else {
        0
    };
    let match_extra = if match_extra_bits > 0 {
        reader.read_bits_fast(match_extra_bits) as u32
    } else {
        0
    };
    let combined_om = offset_code + match_extra_bits;
    if combined_om + literal_extra_bits >= 31 {
        let _ = reader.reload();
    }
    let literal_extra = if literal_extra_bits > 0 {
        reader.read_bits_fast(literal_extra_bits) as u32
    } else {
        0
    };

    let literal_length = (fse::raw_entry_baseline(lit_raw) + literal_extra) as usize;
    let match_length = (fse::raw_entry_baseline(ml_raw) + match_extra) as usize;
    let offset_value = (1u32 << offset_code) + offset_extra;

    // Execute: bounds check, offset resolve, copy literals, copy match.
    let total = literal_length + match_length;
    // `wrapping_add` for the reason given on the matching advance in the main
    // loop: the length is still unvalidated here.
    let new_ip = ip.wrapping_add(literal_length);
    if total > remaining_budget || new_ip > ip_end {
        repeat_offsets.values = [rep0, rep1, rep2];
        return Err(if new_ip > ip_end {
            corruption_error("sequence literal length exceeds literals buffer")
        } else {
            sequence_budget_error(total, block_remaining, out_pos, limit)
        });
    }
    remaining_budget -= total;

    let offset = if offset_value > 3 {
        let o = offset_value - 3;
        rep2 = rep1;
        rep1 = rep0;
        rep0 = o;
        o as usize
    } else if offset_value == 1 && literal_length != 0 {
        rep0 as usize
    } else {
        repeat_offsets.values = [rep0, rep1, rep2];
        let result = resolve_rep_offset_decode(repeat_offsets, offset_value, literal_length);
        [rep0, rep1, rep2] = repeat_offsets.values;
        result
    };

    // Unvalidated offset, so the address can wrap; see the matching prefetch in
    // the main loop for why this has to be `wrapping_add`.
    unsafe {
        prefetch_match(out_base.wrapping_add((out_pos + literal_length).wrapping_sub(offset)));
    }

    if out_pos + total <= wildcopy_end {
        unsafe {
            let lit_dst = out_base.add(out_pos);
            copy_16(lit_dst, ip);
            if literal_length > 16 {
                let mut pos = 16usize;
                while pos < literal_length {
                    copy_16(lit_dst.add(pos), ip.add(pos));
                    pos += 16;
                }
            }
        }
        out_pos += literal_length;
        ip = new_ip;

        if offset <= out_pos - frame_start && offset <= window_size {
            let match_src = out_pos - offset;
            unsafe {
                copy_match_inline(out_base, out_pos, match_src, match_length);
            }
            out_pos += match_length;
        } else {
            repeat_offsets.values = [rep0, rep1, rep2];
            (out_pos, out_base) = execute_dictionary_match(
                out,
                out_pos,
                frame_start,
                window_size,
                dictionary,
                offset,
                match_length,
            )?;
        }
    } else {
        repeat_offsets.values = [rep0, rep1, rep2];
        (out_pos, out_base) = execute_sequence_exact(
            out,
            out_pos,
            ip,
            literal_length,
            offset,
            match_length,
            frame_start,
            window_size,
            dictionary,
        )?;
        ip = new_ip;
    }

    Ok((out_pos, out_base, rep0, rep1, rep2, ip, remaining_budget))
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn encode_sequence_section(sequences: &[SequenceCommand]) -> Result<Vec<u8>> {
    let (encoded, _) =
        encode_sequence_section_with_state(sequences, &SequenceEncodingState::default())?;
    Ok(encoded)
}

pub(crate) fn encode_sequence_section_with_state(
    sequences: &[SequenceCommand],
    state: &SequenceEncodingState,
) -> Result<(Vec<u8>, SequenceEncodingState)> {
    encode_sequence_section_with_strategy(sequences, state, ParserStrategy::Lazy)
}

pub(crate) fn encode_sequence_section_with_strategy(
    sequences: &[SequenceCommand],
    state: &SequenceEncodingState,
    parser_strategy: ParserStrategy,
) -> Result<(Vec<u8>, SequenceEncodingState)> {
    let mut bitstream_scratch = Vec::new();
    let mut encode_scratch = SequenceEncodeScratch::default();
    encode_sequence_section_with_strategy_and_scratch(
        sequences,
        state,
        parser_strategy,
        &mut bitstream_scratch,
        &mut encode_scratch,
    )
}

pub(crate) fn encode_sequence_section_with_strategy_and_scratch(
    sequences: &[SequenceCommand],
    state: &SequenceEncodingState,
    parser_strategy: ParserStrategy,
    bitstream_scratch: &mut Vec<u8>,
    encode_scratch: &mut SequenceEncodeScratch,
) -> Result<(Vec<u8>, SequenceEncodingState)> {
    if sequences.is_empty() {
        return Ok((vec![0], state.clone()));
    }

    prepare_sequence_encode_scratch(sequences, encode_scratch)?;
    encode_sequence_section_with_strategy_and_prepared_scratch(
        state,
        parser_strategy,
        bitstream_scratch,
        encode_scratch,
    )
}

fn encode_sequence_section_with_strategy_and_prepared_scratch(
    state: &SequenceEncodingState,
    parser_strategy: ParserStrategy,
    bitstream_scratch: &mut Vec<u8>,
    encode_scratch: &mut SequenceEncodeScratch,
) -> Result<(Vec<u8>, SequenceEncodingState)> {
    let mut payload = Vec::new();
    let (next_state, _) = encode_sequence_section_with_strategy_and_prepared_scratch_into_stats(
        &mut payload,
        state,
        parser_strategy,
        bitstream_scratch,
        encode_scratch,
    )?;
    Ok((payload, next_state))
}

fn encode_sequence_section_with_strategy_and_prepared_scratch_into(
    payload: &mut Vec<u8>,
    state: &SequenceEncodingState,
    parser_strategy: ParserStrategy,
    bitstream_scratch: &mut Vec<u8>,
    encode_scratch: &mut SequenceEncodeScratch,
) -> Result<SequenceEncodingState> {
    Ok(
        encode_sequence_section_with_strategy_and_prepared_scratch_into_stats(
            payload,
            state,
            parser_strategy,
            bitstream_scratch,
            encode_scratch,
        )?
        .0,
    )
}

fn encode_sequence_section_with_strategy_and_prepared_scratch_into_stats(
    payload: &mut Vec<u8>,
    state: &SequenceEncodingState,
    parser_strategy: ParserStrategy,
    bitstream_scratch: &mut Vec<u8>,
    encode_scratch: &mut SequenceEncodeScratch,
) -> Result<(SequenceEncodingState, SequenceSectionStats)> {
    let encoded = &encode_scratch.encoded;
    let mut build = build_sequences_statistics(
        encoded.len(),
        &encode_scratch.literal_codes,
        &encode_scratch.offset_codes,
        &encode_scratch.match_codes,
        state,
        parser_strategy,
        &mut encode_scratch.table_pool,
    )?;

    encode_sequence_bitstream_into(
        bitstream_scratch,
        encoded,
        build.extra_bits,
        build.literal_choice.encoding.ctable(),
        build.offset_choice.encoding.ctable(),
        build.match_choice.encoding.ctable(),
        build.stats.long_offsets,
    )?;
    build.stats.bitstream_size = bitstream_scratch.len();

    let payload_len = build.stats.header_size + bitstream_scratch.len();
    payload.clear();
    // Cleared, so `reserve` counts from zero and this is the whole payload.
    // Subtracting the existing capacity here would ask for less than the buffer
    // already had and pre-allocate nothing.
    payload.reserve(payload_len);
    encode_sequence_count(encoded.len(), &mut OutBuf::growable(payload))?;
    payload.push(
        (build.literal_choice.mode.bits() << 6)
            | (build.offset_choice.mode.bits() << 4)
            | (build.match_choice.mode.bits() << 2),
    );
    payload.extend_from_slice(&build.literal_choice.header);
    payload.extend_from_slice(&build.offset_choice.header);
    payload.extend_from_slice(&build.match_choice.header);
    payload.extend_from_slice(bitstream_scratch);

    Ok(build.into_next_state())
}

#[allow(dead_code)]
pub(crate) fn encode_prepared_sequence_section_with_strategy_and_scratch(
    state: &SequenceEncodingState,
    parser_strategy: ParserStrategy,
    bitstream_scratch: &mut Vec<u8>,
    encode_scratch: &mut SequenceEncodeScratch,
) -> Result<(Vec<u8>, SequenceEncodingState)> {
    if encode_scratch.encoded.is_empty() {
        return Ok((vec![0], state.clone()));
    }
    encode_sequence_section_with_strategy_and_prepared_scratch(
        state,
        parser_strategy,
        bitstream_scratch,
        encode_scratch,
    )
}

#[allow(dead_code)]
pub(crate) fn encode_prepared_sequence_section_with_strategy_and_scratch_into(
    payload: &mut Vec<u8>,
    state: &SequenceEncodingState,
    parser_strategy: ParserStrategy,
    bitstream_scratch: &mut Vec<u8>,
    encode_scratch: &mut SequenceEncodeScratch,
) -> Result<SequenceEncodingState> {
    if encode_scratch.encoded.is_empty() {
        payload.clear();
        payload.push(0);
        return Ok(state.clone());
    }
    encode_sequence_section_with_strategy_and_prepared_scratch_into(
        payload,
        state,
        parser_strategy,
        bitstream_scratch,
        encode_scratch,
    )
}

pub(crate) fn prepare_sequence_encode_scratch(
    sequences: &[SequenceCommand],
    scratch: &mut SequenceEncodeScratch,
) -> Result<()> {
    scratch.encoded.clear();
    scratch.literal_codes.clear();
    scratch.offset_codes.clear();
    scratch.match_codes.clear();
    scratch.encoded.reserve(sequences.len());
    scratch.literal_codes.reserve(sequences.len());
    scratch.offset_codes.reserve(sequences.len());
    scratch.match_codes.reserve(sequences.len());

    for sequence in sequences {
        let encoded = encode_sequence_command(sequence)?;
        scratch.literal_codes.push(encoded.ll_code);
        scratch.offset_codes.push(encoded.of_code);
        scratch.match_codes.push(encoded.ml_code);
        scratch.encoded.push(encoded);
    }

    Ok(())
}

pub(crate) fn prepare_seq_store_encode_scratch(
    store: &mut SeqStore,
    scratch: &mut SequenceEncodeScratch,
) -> Result<()> {
    let codes_precomputed = store.literal_codes.len() == store.sequences.len();
    scratch.encoded.clear();

    if codes_precomputed {
        // Codes were computed inline during push_lazy_sequence_no_trace.
        // Skip building the intermediate EncodedSequence vec entirely;
        // extras will be computed inline in the bitstream hot loop.
        scratch.use_direct_path = true;
    } else {
        // Fallback: compute codes from scratch (tracing/dictionary paths).
        scratch.use_direct_path = false;
        scratch.encoded.reserve(store.sequences.len());
        store.literal_codes.clear();
        store.offset_codes.clear();
        store.match_codes.clear();
        store.literal_codes.reserve(store.sequences.len());
        store.offset_codes.reserve(store.sequences.len());
        store.match_codes.reserve(store.sequences.len());

        for sequence in &store.sequences {
            let encoded = encode_sequence_command(sequence)?;
            store.literal_codes.push(encoded.ll_code);
            store.offset_codes.push(encoded.of_code);
            store.match_codes.push(encoded.ml_code);
            scratch.encoded.push(encoded);
        }
    }

    Ok(())
}

/// Append the sequence section directly to `out`, returning the next encoding
/// state and stats.  Unlike `encode_prepared_seq_store_section_with_strategy_and_scratch_into_stats`
/// this does NOT clear `out` — it appends to whatever is already there.
pub(crate) fn encode_sequence_section_direct(
    out: &mut OutBuf<'_>,
    state: &SequenceEncodingState,
    parser_strategy: ParserStrategy,
    bitstream_scratch: &mut Vec<u8>,
    store: &SeqStore,
    encode_scratch: &mut SequenceEncodeScratch,
) -> Result<(SequenceEncodingState, SequenceSectionStats)> {
    let sequence_count = if encode_scratch.use_direct_path {
        store.sequences.len()
    } else {
        encode_scratch.encoded.len()
    };

    if sequence_count == 0 {
        out.push(0);
        return Ok((state.clone(), SequenceSectionStats::default()));
    }

    let mut build = build_sequences_statistics(
        sequence_count,
        &store.literal_codes,
        &store.offset_codes,
        &store.match_codes,
        state,
        parser_strategy,
        &mut encode_scratch.table_pool,
    )?;

    if encode_scratch.use_direct_path {
        encode_sequence_bitstream_direct_into(
            bitstream_scratch,
            &store.sequences,
            &store.literal_codes,
            &store.offset_codes,
            &store.match_codes,
            build.extra_bits,
            build.literal_choice.encoding.ctable(),
            build.offset_choice.encoding.ctable(),
            build.match_choice.encoding.ctable(),
            build.stats.long_offsets,
        )?;
    } else {
        encode_sequence_bitstream_into(
            bitstream_scratch,
            &encode_scratch.encoded,
            build.extra_bits,
            build.literal_choice.encoding.ctable(),
            build.offset_choice.encoding.ctable(),
            build.match_choice.encoding.ctable(),
            build.stats.long_offsets,
        )?;
    }
    build.stats.bitstream_size = bitstream_scratch.len();

    let payload_len = build.stats.header_size + bitstream_scratch.len();
    out.reserve(payload_len);
    encode_sequence_count(sequence_count, out)?;
    out.push(
        (build.literal_choice.mode.bits() << 6)
            | (build.offset_choice.mode.bits() << 4)
            | (build.match_choice.mode.bits() << 2),
    );
    out.extend_from_slice(&build.literal_choice.header);
    out.extend_from_slice(&build.offset_choice.header);
    out.extend_from_slice(&build.match_choice.header);
    out.extend_from_slice(bitstream_scratch);

    Ok(build.into_next_state())
}

pub(crate) fn encode_prepared_seq_store_section_with_strategy_and_scratch_into_stats(
    payload: &mut Vec<u8>,
    state: &SequenceEncodingState,
    parser_strategy: ParserStrategy,
    bitstream_scratch: &mut Vec<u8>,
    store: &SeqStore,
    encode_scratch: &mut SequenceEncodeScratch,
    timings: &mut SequenceSectionTimings,
) -> Result<(SequenceEncodingState, SequenceSectionStats)> {
    let sequence_count = if encode_scratch.use_direct_path {
        store.sequences.len()
    } else {
        encode_scratch.encoded.len()
    };

    if sequence_count == 0 {
        payload.clear();
        payload.push(0);
        return Ok((state.clone(), SequenceSectionStats::default()));
    }

    let statistics_start = Instant::now();
    let mut build = build_sequences_statistics(
        sequence_count,
        &store.literal_codes,
        &store.offset_codes,
        &store.match_codes,
        state,
        parser_strategy,
        &mut encode_scratch.table_pool,
    )?;
    timings.statistics = statistics_start.elapsed();

    let bitstream_start = Instant::now();
    if encode_scratch.use_direct_path {
        // Direct path: encode from code slices + SequenceCommands,
        // computing extras inline in the bitstream hot loop.
        encode_sequence_bitstream_direct_into(
            bitstream_scratch,
            &store.sequences,
            &store.literal_codes,
            &store.offset_codes,
            &store.match_codes,
            build.extra_bits,
            build.literal_choice.encoding.ctable(),
            build.offset_choice.encoding.ctable(),
            build.match_choice.encoding.ctable(),
            build.stats.long_offsets,
        )?;
    } else {
        encode_sequence_bitstream_into(
            bitstream_scratch,
            &encode_scratch.encoded,
            build.extra_bits,
            build.literal_choice.encoding.ctable(),
            build.offset_choice.encoding.ctable(),
            build.match_choice.encoding.ctable(),
            build.stats.long_offsets,
        )?;
    }
    timings.bitstream = bitstream_start.elapsed();
    build.stats.bitstream_size = bitstream_scratch.len();

    let assembly_start = Instant::now();
    let payload_len = build.stats.header_size + bitstream_scratch.len();
    payload.clear();
    // Cleared, so `reserve` counts from zero and this is the whole payload.
    // Subtracting the existing capacity here would ask for less than the buffer
    // already had and pre-allocate nothing.
    payload.reserve(payload_len);
    encode_sequence_count(sequence_count, &mut OutBuf::growable(payload))?;
    payload.push(
        (build.literal_choice.mode.bits() << 6)
            | (build.offset_choice.mode.bits() << 4)
            | (build.match_choice.mode.bits() << 2),
    );
    payload.extend_from_slice(&build.literal_choice.header);
    payload.extend_from_slice(&build.offset_choice.header);
    payload.extend_from_slice(&build.match_choice.header);
    payload.extend_from_slice(bitstream_scratch);
    timings.assembly = assembly_start.elapsed();

    Ok(build.into_next_state())
}

fn build_sequence_part_choice_with_stats(
    part: SequencePart,
    codes: &[u8],
    previous: Option<&SequenceEncodingPartState>,
    parser_strategy: ParserStrategy,
    stats: &SequenceCodeStats,
    pool: &mut SequenceTablePool,
) -> Result<SequenceTableChoice> {
    let choice = select_table_choice(part, codes, previous, parser_strategy, stats)?;
    build_selected_table_choice(part, codes, previous, stats, choice, pool)
}

fn build_sequences_statistics(
    sequence_count: usize,
    literal_codes: &[u8],
    offset_codes: &[u8],
    match_codes: &[u8],
    state: &SequenceEncodingState,
    parser_strategy: ParserStrategy,
    pool: &mut SequenceTablePool,
) -> Result<SequenceStatisticsBuild> {
    // Single-pass histogram: build all three code stats in one loop iteration.
    let (ll_stats, of_stats, ml_stats) =
        analyze_all_codes(literal_codes, offset_codes, match_codes);

    let literal_choice = build_sequence_part_choice_with_stats(
        SequencePart::LiteralLength,
        literal_codes,
        state.entry(SequencePart::LiteralLength),
        parser_strategy,
        &ll_stats,
        pool,
    )?;
    let offset_choice = build_sequence_part_choice_with_stats(
        SequencePart::Offset,
        offset_codes,
        state.entry(SequencePart::Offset),
        parser_strategy,
        &of_stats,
        pool,
    )?;
    let match_choice = build_sequence_part_choice_with_stats(
        SequencePart::MatchLength,
        match_codes,
        state.entry(SequencePart::MatchLength),
        parser_strategy,
        &ml_stats,
        pool,
    )?;

    let modes = SequenceCompressionModes {
        literal_lengths: literal_choice.mode,
        offsets: offset_choice.mode,
        match_lengths: match_choice.mode,
    };
    let header_size = encoded_sequence_count_size(sequence_count)
        + 1
        + literal_choice.header.len()
        + offset_choice.header.len()
        + match_choice.header.len();
    let last_count_size =
        last_compressed_count_size(&literal_choice, &offset_choice, &match_choice);
    let long_offsets = detect_long_offsets(offset_codes);
    let extra_bits = extra_bits_from_code_counts(&ll_stats, &of_stats, &ml_stats);

    Ok(SequenceStatisticsBuild {
        literal_choice,
        offset_choice,
        match_choice,
        extra_bits,
        stats: SequenceSectionStats {
            header_size,
            bitstream_size: 0,
            last_count_size,
            long_offsets,
            modes: Some(modes),
        },
    })
}

fn last_compressed_count_size(
    literal_choice: &SequenceTableChoice,
    offset_choice: &SequenceTableChoice,
    match_choice: &SequenceTableChoice,
) -> usize {
    for choice in [match_choice, offset_choice, literal_choice] {
        if choice.mode == CompressionMode::FseCompressed {
            return choice.header.len();
        }
    }
    0
}

#[cfg(test)]
pub(crate) fn execute_sequences(
    out: &mut DecodeOut<'_>,
    frame_start: usize,
    window_size: usize,
    block_size_max: usize,
    dictionary: Option<&[u8]>,
    literals: &[u8],
    sequences: &[SequenceCommand],
    repeat_offsets: &mut RepeatOffsets,
    max_output_size: Option<usize>,
) -> Result<()> {
    let total_match_bytes = sequences.iter().try_fold(0usize, |size, sequence| {
        size.checked_add(sequence.match_length as usize)
            .ok_or(Error::OutputSizeOverflow)
    })?;
    execute_sequences_with_total_match_bytes(
        out,
        frame_start,
        window_size,
        block_size_max,
        dictionary,
        literals,
        sequences,
        repeat_offsets,
        max_output_size,
        total_match_bytes,
    )
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SequenceExecuteProfile {
    pub(crate) literal_copy: std::time::Duration,
    pub(crate) prefix_match_copy: std::time::Duration,
    pub(crate) dictionary_match_copy: std::time::Duration,
}

#[cfg(test)]
pub(crate) fn execute_sequences_with_total_match_bytes(
    out: &mut DecodeOut<'_>,
    frame_start: usize,
    window_size: usize,
    block_size_max: usize,
    dictionary: Option<&[u8]>,
    literals: &[u8],
    sequences: &[SequenceCommand],
    repeat_offsets: &mut RepeatOffsets,
    max_output_size: Option<usize>,
    total_match_bytes: usize,
) -> Result<()> {
    let block_output_size = literals
        .len()
        .checked_add(total_match_bytes)
        .ok_or(Error::OutputSizeOverflow)?;
    if block_output_size > block_size_max {
        return Err(Error::Corruption(
            "compressed block output exceeds the frame block size limit",
        ));
    }

    let final_len = out
        .len()
        .checked_add(block_output_size)
        .ok_or(Error::OutputSizeOverflow)?;
    if let Some(limit) = max_output_size {
        if final_len > limit {
            return Err(Error::OutputSizeTooLarge {
                output_size: final_len as u64,
                max_output_size: limit,
            });
        }
    }
    let block_start = out.len();
    out.try_reserve(block_output_size + WILDCOPY_OVERLENGTH)?;
    let literal_cursor = if let Some(dictionary) = dictionary {
        execute_sequences_with_dictionary(
            out,
            frame_start,
            window_size,
            dictionary,
            literals,
            sequences,
            repeat_offsets,
        )?
    } else {
        execute_sequences_without_dictionary(
            out,
            frame_start,
            window_size,
            literals,
            sequences,
            repeat_offsets,
        )?
    };
    out.append(literals.get(literal_cursor..).ok_or(Error::Corruption(
        "sequence literal cursor exceeded literals buffer",
    ))?)?;

    debug_assert_eq!(out.len() - block_start, block_output_size);
    Ok(())
}

pub(crate) fn execute_sequences_with_total_match_bytes_profiled(
    out: &mut DecodeOut<'_>,
    frame_start: usize,
    window_size: usize,
    block_size_max: usize,
    dictionary: Option<&[u8]>,
    literals: &[u8],
    sequences: &[SequenceCommand],
    repeat_offsets: &mut RepeatOffsets,
    max_output_size: Option<usize>,
    total_match_bytes: usize,
) -> Result<SequenceExecuteProfile> {
    let block_output_size = literals
        .len()
        .checked_add(total_match_bytes)
        .ok_or(Error::OutputSizeOverflow)?;
    if block_output_size > block_size_max {
        return Err(Error::Corruption(
            "compressed block output exceeds the frame block size limit",
        ));
    }

    let final_len = out
        .len()
        .checked_add(block_output_size)
        .ok_or(Error::OutputSizeOverflow)?;
    if let Some(limit) = max_output_size {
        if final_len > limit {
            return Err(Error::OutputSizeTooLarge {
                output_size: final_len as u64,
                max_output_size: limit,
            });
        }
    }
    let block_start = out.len();
    out.try_reserve(block_output_size + WILDCOPY_OVERLENGTH)?;
    let (literal_cursor, profile) = if let Some(dictionary) = dictionary {
        execute_sequences_with_dictionary_profiled(
            out,
            frame_start,
            window_size,
            dictionary,
            literals,
            sequences,
            repeat_offsets,
        )?
    } else {
        execute_sequences_without_dictionary_profiled(
            out,
            frame_start,
            window_size,
            literals,
            sequences,
            repeat_offsets,
        )?
    };
    let trailing_literals = literals.get(literal_cursor..).ok_or(Error::Corruption(
        "sequence literal cursor exceeded literals buffer",
    ))?;
    let literal_start = std::time::Instant::now();
    out.append(trailing_literals)?;
    let mut profile = profile;
    profile.literal_copy += literal_start.elapsed();

    debug_assert_eq!(out.len() - block_start, block_output_size);
    Ok(profile)
}

#[cfg(test)]
#[allow(unsafe_code)]
fn execute_sequences_without_dictionary(
    out: &mut DecodeOut<'_>,
    frame_start: usize,
    window_size: usize,
    literals: &[u8],
    sequences: &[SequenceCommand],
    repeat_offsets: &mut RepeatOffsets,
) -> Result<usize> {
    let mut literal_cursor = 0usize;
    for sequence in sequences {
        // Resolve offset early so we can prefetch match data while copying literals.
        let offset = repeat_offsets.resolve(sequence)? as usize;
        let predicted_match_pos =
            (out.len() + (sequence.literal_length as usize)).wrapping_sub(offset);
        // Prefetch is unconditional: prefetch instructions are hints and do not
        // fault on out-of-range addresses on modern x86/ARM hardware.
        unsafe { crate::entropy::mem::prefetch_l1(out.as_ptr().add(predicted_match_pos)) };

        literal_cursor = append_sequence_literals(out, literals, literal_cursor, sequence)?;

        // frame_start <= out.len() is maintained by the caller (frame always
        // starts at or before current output position, and output only grows).
        debug_assert!(out.len() >= frame_start);
        let produced_in_frame = out.len() - frame_start;
        let available_history = produced_in_frame.min(window_size);
        if offset > available_history {
            return Err(offset_exceeds_history_error());
        }
        // offset <= available_history <= produced_in_frame <= out.len(),
        // so this subtraction cannot underflow.
        debug_assert!(out.len() >= offset);
        let match_start = out.len() - offset;
        append_match_from_history(out, match_start, sequence.match_length as usize)?;
    }
    Ok(literal_cursor)
}

#[allow(unsafe_code)]
fn execute_sequences_without_dictionary_profiled(
    out: &mut DecodeOut<'_>,
    frame_start: usize,
    window_size: usize,
    literals: &[u8],
    sequences: &[SequenceCommand],
    repeat_offsets: &mut RepeatOffsets,
) -> Result<(usize, SequenceExecuteProfile)> {
    let mut literal_cursor = 0usize;
    let mut profile = SequenceExecuteProfile::default();
    for sequence in sequences {
        let offset = repeat_offsets.resolve(sequence)? as usize;
        let predicted_match_pos =
            (out.len() + (sequence.literal_length as usize)).wrapping_sub(offset);
        // Prefetch is unconditional: prefetch instructions are hints and do not
        // fault on out-of-range addresses on modern x86/ARM hardware.
        unsafe { crate::entropy::mem::prefetch_l1(out.as_ptr().add(predicted_match_pos)) };

        let literal_start = std::time::Instant::now();
        literal_cursor = append_sequence_literals(out, literals, literal_cursor, sequence)?;
        profile.literal_copy += literal_start.elapsed();

        debug_assert!(out.len() >= frame_start);
        let produced_in_frame = out.len() - frame_start;
        let available_history = produced_in_frame.min(window_size);
        if offset > available_history {
            return Err(offset_exceeds_history_error());
        }
        debug_assert!(out.len() >= offset);
        let match_start = out.len() - offset;
        let match_start_time = std::time::Instant::now();
        append_match_from_history(out, match_start, sequence.match_length as usize)?;
        profile.prefix_match_copy += match_start_time.elapsed();
    }
    Ok((literal_cursor, profile))
}

#[cfg(test)]
#[allow(unsafe_code)]
fn execute_sequences_with_dictionary(
    out: &mut DecodeOut<'_>,
    frame_start: usize,
    window_size: usize,
    dictionary: &[u8],
    literals: &[u8],
    sequences: &[SequenceCommand],
    repeat_offsets: &mut RepeatOffsets,
) -> Result<usize> {
    let mut literal_cursor = 0usize;
    for sequence in sequences {
        let offset = repeat_offsets.resolve(sequence)? as usize;
        let predicted_match_pos =
            (out.len() + (sequence.literal_length as usize)).wrapping_sub(offset);
        // Prefetch is unconditional: prefetch instructions are hints and do not
        // fault on out-of-range addresses on modern x86/ARM hardware.
        unsafe { crate::entropy::mem::prefetch_l1(out.as_ptr().add(predicted_match_pos)) };

        literal_cursor = append_sequence_literals(out, literals, literal_cursor, sequence)?;

        debug_assert!(out.len() >= frame_start);
        let produced_in_frame = out.len() - frame_start;
        let available_prefix = produced_in_frame.min(window_size);

        if offset <= available_prefix {
            // offset <= available_prefix <= produced_in_frame <= out.len()
            debug_assert!(out.len() >= offset);
            let match_start = out.len() - offset;
            append_match_from_history(out, match_start, sequence.match_length as usize)?;
            continue;
        }
        if produced_in_frame > window_size {
            return Err(offset_exceeds_history_error());
        }
        // dictionary.len() + produced_in_frame: both are bounded by their
        // respective maximum sizes (dictionary ≤ 64 KiB, frame ≤ window ≤ 2^31)
        // so overflow is not possible on any supported platform.
        let available_history = dictionary.len() + produced_in_frame;
        if offset > available_history {
            return Err(offset_exceeds_history_error());
        }
        append_match_from_dictionary_history(
            out,
            frame_start,
            dictionary,
            offset,
            sequence.match_length as usize,
        )?;
    }
    Ok(literal_cursor)
}

#[allow(unsafe_code)]
fn execute_sequences_with_dictionary_profiled(
    out: &mut DecodeOut<'_>,
    frame_start: usize,
    window_size: usize,
    dictionary: &[u8],
    literals: &[u8],
    sequences: &[SequenceCommand],
    repeat_offsets: &mut RepeatOffsets,
) -> Result<(usize, SequenceExecuteProfile)> {
    let mut literal_cursor = 0usize;
    let mut profile = SequenceExecuteProfile::default();
    for sequence in sequences {
        let offset = repeat_offsets.resolve(sequence)? as usize;
        let predicted_match_pos =
            (out.len() + (sequence.literal_length as usize)).wrapping_sub(offset);
        // Prefetch is unconditional: prefetch instructions are hints and do not
        // fault on out-of-range addresses on modern x86/ARM hardware.
        unsafe { crate::entropy::mem::prefetch_l1(out.as_ptr().add(predicted_match_pos)) };

        let literal_start = std::time::Instant::now();
        literal_cursor = append_sequence_literals(out, literals, literal_cursor, sequence)?;
        profile.literal_copy += literal_start.elapsed();

        debug_assert!(out.len() >= frame_start);
        let produced_in_frame = out.len() - frame_start;
        let available_prefix = produced_in_frame.min(window_size);

        if offset <= available_prefix {
            debug_assert!(out.len() >= offset);
            let match_start = out.len() - offset;
            let match_start_time = std::time::Instant::now();
            append_match_from_history(out, match_start, sequence.match_length as usize)?;
            profile.prefix_match_copy += match_start_time.elapsed();
            continue;
        }
        if produced_in_frame > window_size {
            return Err(offset_exceeds_history_error());
        }
        let available_history = dictionary.len() + produced_in_frame;
        if offset > available_history {
            return Err(offset_exceeds_history_error());
        }
        let match_start_time = std::time::Instant::now();
        append_match_from_dictionary_history(
            out,
            frame_start,
            dictionary,
            offset,
            sequence.match_length as usize,
        )?;
        profile.dictionary_match_copy += match_start_time.elapsed();
    }
    Ok((literal_cursor, profile))
}

fn append_sequence_literals(
    out: &mut DecodeOut<'_>,
    literals: &[u8],
    literal_cursor: usize,
    sequence: &SequenceCommand,
) -> Result<usize> {
    let literal_length = sequence.literal_length as usize;
    let literal_end = literal_cursor
        .checked_add(literal_length)
        .ok_or_else(output_size_overflow_error)?;
    let literal_chunk = literals
        .get(literal_cursor..literal_end)
        .ok_or_else(literal_length_exceeds_buffer_error)?;
    out.append(literal_chunk)?;
    Ok(literal_end)
}

#[cold]
#[inline(never)]
fn offset_exceeds_history_error() -> Error {
    Error::Corruption("sequence offset exceeds the available history window")
}

#[cold]
#[inline(never)]
fn literal_length_exceeds_buffer_error() -> Error {
    Error::Corruption("sequence literal length exceeds literals buffer")
}

impl RepeatOffsets {
    #[inline(always)]
    pub(crate) fn resolve_values(&mut self, literal_length: u32, offset_value: u32) -> Result<u32> {
        let [rep1, rep2, rep3] = self.values;

        // Fast path: explicit offset (most common case in typical data).
        if offset_value > 3 {
            let offset = offset_value - 3;
            self.values = [offset, rep1, rep2];
            return Ok(offset);
        }

        if offset_value == 0 {
            return Err(corruption_error("sequence offset value is zero"));
        }

        // Repeat offset cases (offset_value 1-3).
        // Combine offset_value with the literal_length==0 flag to compute an
        // effective repeat index (1-4), matching C zstd's approach:
        //   rep_index 1 -> rep1 (offset_value=1, ll>0)
        //   rep_index 2 -> rep2 (offset_value=2, ll>0  OR  offset_value=1, ll=0)
        //   rep_index 3 -> rep3 (offset_value=3, ll>0  OR  offset_value=2, ll=0)
        //   rep_index 4 -> rep1-1 (offset_value=3, ll=0)  [special case]
        let ll_zero = literal_length == 0;
        let rep_index = offset_value + ll_zero as u32;

        // Resolve the offset from the repeat index.
        let offset = match rep_index {
            1 => rep1,
            2 => rep2,
            3 => rep3,
            _ => {
                // rep_index == 4: special rep1-1 case (offset_value=3, ll=0)
                if rep1 <= 1 {
                    return Err(corruption_error("repeat offset 1 minus 1 is zero"));
                }
                rep1 - 1
            }
        };

        // Update repeat offsets: rotate the resolved offset to position 0.
        // For rep_index 1 (offset_value=1, ll>0): no update needed (rep1 stays).
        if rep_index > 1 {
            self.values[2] = if rep_index == 2 { rep3 } else { rep2 };
            self.values[1] = rep1;
            self.values[0] = offset;
        }

        Ok(offset)
    }

    /// Infallible resolve for encoder hot paths. The encoder guarantees
    /// `offset_value > 0` and valid rep state, so error branches are impossible.
    #[inline(always)]
    pub(crate) fn resolve_encode(&mut self, literal_length: u32, offset_value: u32) -> u32 {
        debug_assert!(offset_value > 0, "offset_value must be non-zero");
        let [rep1, rep2, rep3] = self.values;

        // Fast path: explicit offset (most common case).
        if offset_value > 3 {
            let offset = offset_value - 3;
            self.values = [offset, rep1, rep2];
            return offset;
        }

        let ll_zero = literal_length == 0;
        let rep_index = offset_value + ll_zero as u32;

        let offset = match rep_index {
            1 => rep1,
            2 => rep2,
            3 => rep3,
            _ => {
                debug_assert!(rep1 > 1, "rep1-1 would be zero");
                rep1 - 1
            }
        };

        if rep_index > 1 {
            self.values[2] = if rep_index == 2 { rep3 } else { rep2 };
            self.values[1] = rep1;
            self.values[0] = offset;
        }

        offset
    }

    #[inline(always)]
    pub(crate) fn resolve(&mut self, sequence: &SequenceCommand) -> Result<u32> {
        self.resolve_values(sequence.literal_length, sequence.offset_value)
    }
}

const WILDCOPY_OVERLENGTH: usize = 32;

/// Prefetch match source at ptr and ptr+64 for the upcoming match copy.
/// Uses a single inline asm block on aarch64 to enable the [reg, #64]
/// addressing mode, saving one instruction vs two separate prefetch calls.
///
/// Compiled away under Miri, which cannot execute inline assembly or the x86
/// prefetch intrinsics. Note that the x86 arm also computes `ptr.add(64)`,
/// which for a match near the end of the window is an out-of-bounds pointer;
/// that is fine for a prefetch on hardware and is exactly the kind of thing
/// Miri would object to, so there is nothing lost by not running it there.
///
/// # Safety
///
/// None on `ptr`; see [`crate::entropy::mem::prefetch_l1`]. This one is called
/// specifically with addresses that have *not* been validated yet, which is why
/// the second prefetch uses `wrapping_add`.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn prefetch_match(ptr: *const u8) {
    #[cfg(miri)]
    let _ = ptr;
    #[cfg(not(miri))]
    {
        // SAFETY: `prfm` is a hint that cannot fault on any address, touches no
        // stack, and writes no condition flags.
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!(
                "prfm pldl1keep, [{ptr}]",
                "prfm pldl1keep, [{ptr}, #64]",
                ptr = in(reg) ptr,
                options(nostack, preserves_flags),
            )
        };
        #[cfg(target_arch = "x86_64")]
        {
            #[cfg(target_feature = "sse")]
            {
                // SAFETY: `_mm_prefetch` imposes no validity requirement on its
                // argument.
                unsafe {
                    core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(
                        ptr as *const i8,
                    )
                };
                // `wrapping_add` for the same reason callers pass a wrapped
                // address: this runs before the offset that produced `ptr` has
                // been validated, so `ptr` itself may be nowhere near the
                // output buffer.
                //
                // SAFETY: as above. `wrapping_add` keeps the arithmetic itself
                // defined even when the result is out of bounds.
                unsafe {
                    core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(
                        ptr.wrapping_add(64) as *const i8,
                    )
                };
            }
        }
        #[cfg(not(any(
            all(target_arch = "x86_64", target_feature = "sse"),
            target_arch = "aarch64"
        )))]
        {
            let _ = ptr;
        }
    }
}

/// Copy exactly 16 bytes using SIMD when available (NEON on aarch64, SSE2 on
/// x86_64), falling back to `copy_nonoverlapping`.
///
/// # Safety
///
/// `src` must be readable and `dst` writable for 16 bytes. Note that this is 16
/// regardless of how many bytes the caller actually wants: every arm moves a
/// full vector, so a caller with fewer than 16 bytes left must have slack.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn copy_16(dst: *mut u8, src: *const u8) {
    // SAFETY: the caller guarantees 16 readable bytes at `src` and 16 writable
    // at `dst`. All three arms are unaligned moves, so neither pointer carries
    // an alignment requirement.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let v = core::arch::aarch64::vld1q_u8(src);
        core::arch::aarch64::vst1q_u8(dst, v);
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: as above.
        #[cfg(target_feature = "sse2")]
        unsafe {
            let v = core::arch::x86_64::_mm_loadu_si128(src as *const core::arch::x86_64::__m128i);
            core::arch::x86_64::_mm_storeu_si128(dst as *mut core::arch::x86_64::__m128i, v);
        }
        // SAFETY: as above.
        #[cfg(not(target_feature = "sse2"))]
        unsafe {
            core::ptr::copy_nonoverlapping(src, dst, 16)
        };
    }
    // SAFETY: as above.
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    unsafe {
        core::ptr::copy_nonoverlapping(src, dst, 16)
    };
}

#[allow(unsafe_code)]
#[inline(always)]
fn append_match_from_history(
    out: &mut DecodeOut<'_>,
    match_start: usize,
    match_length: usize,
) -> Result<()> {
    if match_length == 0 {
        return Ok(());
    }

    out.ensure_room(match_length)?;

    let out_len = out.len();
    let offset = out_len - match_start;

    // Non-overlapping: single memcpy.
    if match_length <= offset {
        unsafe {
            let base = out.as_mut_ptr();
            core::ptr::copy_nonoverlapping(base.add(match_start), base.add(out_len), match_length);
            out.set_len(out_len + match_length);
        }
        return Ok(());
    }

    // Offset 1 (memset): write_bytes is the fastest for repeated single byte.
    if offset == 1 {
        let value = out.as_slice()[match_start];
        unsafe {
            core::ptr::write_bytes(out.as_mut_ptr().add(out_len), value, match_length);
            out.set_len(out_len + match_length);
        }
        return Ok(());
    }

    // Every other overlapping offset: seed the destination with the offset's
    // bytes and double what is there until the match is filled.
    //
    // A short offset used to get its own expander, which tiled the offset into
    // a 32-byte buffer and stamped that buffer across the destination every 32
    // bytes. The buffer repeats with period `offset`, so it may only be stamped
    // where the period lines up; advancing 32 restarted it out of phase for
    // every offset that does not divide 32 -- 3, 5, 6 and 7 -- corrupting the
    // match past its first 32 bytes. Doubling has no such precondition: each
    // round copies from a distance of `copied`, a multiple of `offset` on every
    // round but the last, and the last is truncated to what remains.
    //
    // Doubling is the slower of the two for matches below a few hundred bytes
    // -- tens of nanoseconds per call, since it issues several small copies
    // where the stamp issued one wide one -- and the faster above a few
    // kilobytes. Neither matters: this is reached only for the last sequences
    // of a fixed-destination decode and for matches crossing a dictionary
    // boundary, a handful of calls per frame. The hot path is
    // `copy_match_inline`, which is untouched.
    unsafe {
        let base = out.as_mut_ptr();
        let dst = base.add(out_len);
        let src_start = base.add(match_start);
        core::ptr::copy_nonoverlapping(src_start, dst, offset);
        let mut copied = offset;
        while copied < match_length {
            let chunk = copied.min(match_length - copied);
            core::ptr::copy_nonoverlapping(dst, dst.add(copied), chunk);
            copied += chunk;
        }
        out.set_len(out_len + match_length);
    }
    Ok(())
}

#[inline(always)]
fn append_match_from_dictionary_history(
    out: &mut DecodeOut<'_>,
    frame_start: usize,
    dictionary: &[u8],
    offset: usize,
    match_length: usize,
) -> Result<()> {
    if match_length == 0 {
        return Ok(());
    }

    let history_len = dictionary.len() + (out.len() - frame_start);
    let source_index = history_len - offset;
    if source_index < dictionary.len() {
        let dict_chunk_len = (dictionary.len() - source_index).min(match_length);
        out.append(&dictionary[source_index..source_index + dict_chunk_len])?;
        let remaining = match_length - dict_chunk_len;
        if remaining != 0 {
            append_match_from_history(out, frame_start, remaining)?;
        }
        return Ok(());
    }

    let match_start = frame_start + (source_index - dictionary.len());
    append_match_from_history(out, match_start, match_length)
}

/// The caller's output-size cap, paired with what `out` no longer holds.
///
/// `max_output_size` bounds everything a decoder produces, but the executors
/// below measure progress as `out.len()`. Those are the same number only for
/// the one-shot decoder, which keeps every byte it has produced. The streaming
/// decoder releases bytes off the front of its buffer once the caller has
/// drained them and they have fallen out of match range, so there `out.len()`
/// is a window rather than a total, and the released count has to be added
/// back before the cap means anything.
///
/// The two travel together so a caller cannot supply a limit without also
/// saying what it is measured against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OutputLimit {
    /// Bytes produced before `out[0]`, which `out` no longer holds. Counted in
    /// `u64` because it accumulates over a whole stream, and a 32-bit `usize`
    /// would wrap on any stream past 4 GiB.
    produced_before: u64,
    max_output_size: Option<usize>,
}

impl OutputLimit {
    /// For a buffer that holds everything produced so far.
    pub(crate) fn whole_output(max_output_size: Option<usize>) -> Self {
        Self {
            produced_before: 0,
            max_output_size,
        }
    }

    /// For a buffer whose first byte is preceded by `produced_before` bytes
    /// that have already been released.
    pub(crate) fn after(produced_before: u64, max_output_size: Option<usize>) -> Self {
        Self {
            produced_before,
            max_output_size,
        }
    }

    /// Total bytes the stream has produced once `out` holds `out_len`.
    #[inline(always)]
    fn produced(&self, out_len: usize) -> u64 {
        self.produced_before.saturating_add(out_len as u64)
    }

    /// Reject a buffer that has grown to `out_len` if the stream would then be
    /// over the cap. For the block appenders, which grow `out` in one step
    /// rather than walking a per-sequence budget.
    #[inline(always)]
    pub(crate) fn check_total(&self, out_len: usize) -> Result<()> {
        let Some(max_output_size) = self.max_output_size else {
            return Ok(());
        };
        let produced = self.produced(out_len);
        if produced > max_output_size as u64 {
            return Err(Error::OutputSizeTooLarge {
                output_size: produced,
                max_output_size,
            });
        }
        Ok(())
    }

    /// How many more bytes the cap allows. `usize::MAX` when uncapped, which
    /// makes the callers' arithmetic uniform: an uncapped decode simply never
    /// exhausts it.
    #[inline(always)]
    fn remaining(&self, out_len: usize) -> Result<usize> {
        let Some(limit) = self.max_output_size else {
            return Ok(usize::MAX);
        };
        let produced = self.produced(out_len);
        let remaining = (limit as u64)
            .checked_sub(produced)
            .ok_or(Error::OutputSizeTooLarge {
                output_size: produced,
                max_output_size: limit,
            })?;
        // Bounded above by `limit`, which is a `usize`, so this cannot truncate.
        Ok(remaining as usize)
    }
}

#[inline(always)]
fn reserve_block_output(
    out: &mut DecodeOut<'_>,
    block_size_max: usize,
    limit: OutputLimit,
) -> Result<()> {
    let reserve = limit.remaining(out.len())?.min(block_size_max);
    out.try_reserve(reserve + WILDCOPY_OVERLENGTH)
}

/// Attribute an exhausted sequence-loop budget to the limit that actually
/// bound it.
///
/// The hot loop folds two limits into one counter for speed:
/// `min(remaining block bytes, remaining output bytes)`. That is fine until it
/// runs out, at which point the counter no longer knows which of the two said
/// no, and either answer alone is wrong some of the time:
///
/// - Blaming the block size limit tells a caller who set `max_output_size` as
///   a decompression-bomb guard that their archive is damaged, when it is well
///   formed and merely larger than they allowed. That made the one-shot
///   decoder disagree with the streaming one on the same input.
/// - Blaming the cap excuses a genuinely corrupt frame. A caller whose limit
///   was nowhere near binding is told to raise it and try again, which is the
///   worse of the two: it sends them back to a damaged archive with a bigger
///   buffer.
///
/// So the order matters, and the block size limit has to come first. A
/// sequence that overruns it describes a block the format cannot represent,
/// which is true whatever the caller allowed.
///
/// Cold: only reached on the way out with an error.
#[cold]
#[inline(never)]
fn sequence_budget_error(
    additional: usize,
    block_remaining: usize,
    out_len: usize,
    limit: OutputLimit,
) -> Error {
    if additional > block_remaining {
        return corruption_error("compressed block output exceeds the frame block size limit");
    }
    if let Some(max_output_size) = limit.max_output_size {
        let final_len = limit.produced(out_len).saturating_add(additional as u64);
        if final_len > max_output_size as u64 {
            return Error::OutputSizeTooLarge {
                output_size: final_len,
                max_output_size,
            };
        }
    }
    // Unreachable by construction: the merged budget is the minimum of the
    // two, so exhausting it means at least one of them was exceeded. Reported
    // rather than asserted because this is an error path in a decoder for
    // untrusted input, where the safe answer to "neither limit fired" is still
    // to reject.
    corruption_error("compressed block output exceeds the frame block size limit")
}

#[inline(always)]
fn ensure_decode_output_room(
    out_len: usize,
    additional: usize,
    remaining_block_bytes: &mut usize,
    remaining_output_bytes: &mut usize,
    limit: OutputLimit,
) -> Result<()> {
    if additional > *remaining_block_bytes {
        return Err(corruption_error(
            "compressed block output exceeds the frame block size limit",
        ));
    }
    *remaining_block_bytes -= additional;

    if additional > *remaining_output_bytes {
        let max_output_size = limit
            .max_output_size
            .expect("output limit present when tracking remaining bytes");
        let output_size = limit
            .produced(out_len)
            .checked_add(additional as u64)
            .ok_or_else(output_size_overflow_error)?;
        return Err(output_size_too_large_error(output_size, max_output_size));
    }
    *remaining_output_bytes -= additional;
    Ok(())
}

#[inline(always)]
fn append_trailing_literals(
    out: &mut DecodeOut<'_>,
    literals: &[u8],
    literal_cursor: usize,
    remaining_block_bytes: &mut usize,
    remaining_output_bytes: &mut usize,
    limit: OutputLimit,
) -> Result<()> {
    let trailing = literals.get(literal_cursor..).ok_or(Error::Corruption(
        "sequence literal cursor exceeded literals buffer",
    ))?;
    ensure_decode_output_room(
        out.len(),
        trailing.len(),
        remaining_block_bytes,
        remaining_output_bytes,
        limit,
    )?;
    out.append(trailing)
}

fn predefined_sequence_dtable(part: SequencePart) -> &'static fse::DTable {
    let tables = PREDEFINED_SEQUENCE_DTABLES.get_or_init(|| {
        let mut literal_lengths = fse::DTable::default();
        let mut offsets = fse::DTable::default();
        let mut match_lengths = fse::DTable::default();
        fse::build_dtable(
            &mut literal_lengths,
            &LITERAL_LENGTH_DEFAULT_DISTRIBUTION,
            (LITERAL_LENGTH_DEFAULT_DISTRIBUTION.len() - 1) as u32,
            LL_DEFAULT_LOG,
        )
        .expect("literal-length default dtable must build");
        fse::build_dtable(
            &mut offsets,
            &OFFSET_DEFAULT_DISTRIBUTION,
            (OFFSET_DEFAULT_DISTRIBUTION.len() - 1) as u32,
            OF_DEFAULT_LOG,
        )
        .expect("offset default dtable must build");
        fse::build_dtable(
            &mut match_lengths,
            &MATCH_LENGTH_DEFAULT_DISTRIBUTION,
            (MATCH_LENGTH_DEFAULT_DISTRIBUTION.len() - 1) as u32,
            ML_DEFAULT_LOG,
        )
        .expect("match-length default dtable must build");
        PredefinedSequenceDTables {
            literal_lengths,
            offsets,
            match_lengths,
        }
    });
    match part {
        SequencePart::LiteralLength => &tables.literal_lengths,
        SequencePart::Offset => &tables.offsets,
        SequencePart::MatchLength => &tables.match_lengths,
    }
}

fn predefined_seq_dtable(part: SequencePart) -> &'static fse::SequenceDTable {
    let tables = PREDEFINED_SEQ_DTABLES.get_or_init(|| {
        let mut ll = fse::SequenceDTable::default();
        let mut of = fse::SequenceDTable::default();
        let mut ml = fse::SequenceDTable::default();
        fse::build_sequence_dtable_direct(
            &mut ll,
            &LITERAL_LENGTH_DEFAULT_DISTRIBUTION,
            (LITERAL_LENGTH_DEFAULT_DISTRIBUTION.len() - 1) as u32,
            LL_DEFAULT_LOG,
            &LITERAL_LENGTH_BASELINES,
            &LITERAL_LENGTH_EXTRA_BITS,
        )
        .expect("literal-length default seq dtable must build");
        fse::build_offset_sequence_dtable_direct(
            &mut of,
            &OFFSET_DEFAULT_DISTRIBUTION,
            (OFFSET_DEFAULT_DISTRIBUTION.len() - 1) as u32,
            OF_DEFAULT_LOG,
        )
        .expect("offset default seq dtable must build");
        fse::build_sequence_dtable_direct(
            &mut ml,
            &MATCH_LENGTH_DEFAULT_DISTRIBUTION,
            (MATCH_LENGTH_DEFAULT_DISTRIBUTION.len() - 1) as u32,
            ML_DEFAULT_LOG,
            &MATCH_LENGTH_BASELINES,
            &MATCH_LENGTH_EXTRA_BITS,
        )
        .expect("match-length default seq dtable must build");
        PredefinedSeqDTables {
            literal_lengths: ll,
            offsets: of,
            match_lengths: ml,
        }
    });
    match part {
        SequencePart::LiteralLength => &tables.literal_lengths,
        SequencePart::Offset => &tables.offsets,
        SequencePart::MatchLength => &tables.match_lengths,
    }
}

/// Parse one table's header and leave behind whatever `target` asks for.
///
/// For FseCompressed: parses the FSE header once, then builds the
/// `SequenceDTable` directly from the normalized counts (no conversion step)
/// and, only when asked, the plain `DTable` in its slot as well. For
/// Predefined/RLE: builds from precomputed/inline data. For Repeat: validates
/// that a previous table exists.
fn decode_seq_table_both(
    dt_slot: &mut Option<fse::DTable>,
    seq_dt: &mut fse::SequenceDTable,
    has_table: &mut bool,
    src: &[u8],
    cursor: &mut usize,
    part: SequencePart,
    mode: CompressionMode,
    target: TableTarget,
) -> Result<()> {
    let want_dtable = target == TableTarget::Both;
    match mode {
        CompressionMode::Predefined => {
            // DTable: clone from precomputed (for encoder/test use).
            // Build in-place to avoid stack temporary.
            if want_dtable {
                let precomputed_dt = predefined_sequence_dtable(part);
                let dt = dt_slot.get_or_insert_with(fse::DTable::default);
                let ts = if precomputed_dt.table_log() == 0 {
                    1
                } else {
                    1usize << precomputed_dt.table_log()
                };
                dt.copy_active_from(precomputed_dt, ts);
            } else {
                *dt_slot = None;
            }
            // SequenceDTable: copy from precomputed.
            let precomputed_seq = predefined_seq_dtable(part);
            seq_dt.table_log = precomputed_seq.table_log;
            let seq_ts = if precomputed_seq.table_log == 0 {
                1
            } else {
                1usize << precomputed_seq.table_log
            };
            seq_dt.entries[..seq_ts].copy_from_slice(&precomputed_seq.entries[..seq_ts]);
            *has_table = true;
        }
        CompressionMode::Rle => {
            let symbol = *src.get(*cursor).ok_or(Error::UnexpectedEof)?;
            if symbol > part.max_symbol_value() {
                return Err(Error::Corruption(
                    "sequence RLE symbol exceeds the supported code range",
                ));
            }
            // DTable: build in-place.
            if want_dtable {
                let dt = dt_slot.get_or_insert_with(fse::DTable::default);
                fse::build_rle_dtable(dt, symbol);
            } else {
                *dt_slot = None;
            }
            // SequenceDTable: build directly.
            match part {
                SequencePart::Offset => {
                    fse::build_rle_offset_sequence_dtable(seq_dt, symbol);
                }
                SequencePart::LiteralLength => {
                    fse::build_rle_sequence_dtable(
                        seq_dt,
                        symbol,
                        &LITERAL_LENGTH_BASELINES,
                        &LITERAL_LENGTH_EXTRA_BITS,
                    );
                }
                SequencePart::MatchLength => {
                    fse::build_rle_sequence_dtable(
                        seq_dt,
                        symbol,
                        &MATCH_LENGTH_BASELINES,
                        &MATCH_LENGTH_EXTRA_BITS,
                    );
                }
            }
            *has_table = true;
            *cursor += 1;
        }
        CompressionMode::FseCompressed => {
            let mut normalized = [0i16; fse::SYMBOLVALUE_MAX + 1];
            let mut max_symbol_value = u32::from(part.max_symbol_value());
            let mut table_log = 0u32;
            let consumed = fse::read_ncount(
                &mut normalized,
                &mut max_symbol_value,
                &mut table_log,
                &src[*cursor..],
                part.max_accuracy_log(),
            )?;
            // DTable: build in-place (no stack temp, no memcpy to slot). This is
            // a second full spreading pass over the same counts, so it is done
            // only for the callers that go on to read it.
            if want_dtable {
                let dt = dt_slot.get_or_insert_with(fse::DTable::default);
                fse::build_dtable(dt, &normalized, max_symbol_value, table_log)?;
            } else {
                *dt_slot = None;
            }
            // SequenceDTable: build directly from normalized counts.
            match part {
                SequencePart::Offset => {
                    fse::build_offset_sequence_dtable_direct(
                        seq_dt,
                        &normalized,
                        max_symbol_value,
                        table_log,
                    )?;
                }
                SequencePart::LiteralLength => {
                    fse::build_sequence_dtable_direct(
                        seq_dt,
                        &normalized,
                        max_symbol_value,
                        table_log,
                        &LITERAL_LENGTH_BASELINES,
                        &LITERAL_LENGTH_EXTRA_BITS,
                    )?;
                }
                SequencePart::MatchLength => {
                    fse::build_sequence_dtable_direct(
                        seq_dt,
                        &normalized,
                        max_symbol_value,
                        table_log,
                        &MATCH_LENGTH_BASELINES,
                        &MATCH_LENGTH_EXTRA_BITS,
                    )?;
                }
            }
            *has_table = true;
            *cursor += consumed;
        }
        CompressionMode::Repeat => {
            if !*has_table {
                return Err(Error::Corruption(
                    "sequence repeat mode requires a previous FSE table",
                ));
            }
        }
    }
    Ok(())
}

impl SequencePart {
    fn max_symbol_value(self) -> u8 {
        match self {
            Self::LiteralLength => 35,
            Self::Offset => 31,
            Self::MatchLength => 52,
        }
    }

    fn default_allowed(self, max_symbol: u8) -> bool {
        max_symbol <= self.default_max_symbol()
    }

    fn default_max_symbol(self) -> u8 {
        match self {
            Self::LiteralLength => 35,
            Self::Offset => 28,
            Self::MatchLength => 52,
        }
    }

    fn max_accuracy_log(self) -> usize {
        match self {
            Self::LiteralLength | Self::MatchLength => 9,
            Self::Offset => 8,
        }
    }

    fn default_distribution(self) -> (&'static [i16], u32) {
        match self {
            Self::LiteralLength => (&LITERAL_LENGTH_DEFAULT_DISTRIBUTION, 6),
            Self::Offset => (&OFFSET_DEFAULT_DISTRIBUTION, 5),
            Self::MatchLength => (&MATCH_LENGTH_DEFAULT_DISTRIBUTION, 6),
        }
    }
}

fn encode_sequence_count(number_of_sequences: usize, dst: &mut OutBuf<'_>) -> Result<()> {
    if number_of_sequences >= (1usize << 16) + 0x7F00 {
        return Err(Error::InvalidParameter("too many sequences in one block"));
    }
    encode_sequence_count_unchecked(number_of_sequences, dst);
    Ok(())
}

fn encode_sequence_count_unchecked(number_of_sequences: usize, dst: &mut OutBuf<'_>) {
    if number_of_sequences < 128 {
        dst.push(number_of_sequences as u8);
    } else if number_of_sequences < 0x7F00 {
        dst.push(128 + ((number_of_sequences >> 8) as u8));
        dst.push((number_of_sequences & 0xFF) as u8);
    } else {
        let value = number_of_sequences - 0x7F00;
        dst.push(255);
        dst.push((value & 0xFF) as u8);
        dst.push(((value >> 8) & 0xFF) as u8);
    }
}

fn encode_sequence_command(sequence: &SequenceCommand) -> Result<EncodedSequence> {
    let ll_code = literal_length_code(sequence.literal_length)? as usize;
    let ml_code = match_length_code(sequence.match_length)? as usize;
    let of_code = offset_code(sequence.offset_value)? as usize;

    Ok(EncodedSequence {
        ll_code: ll_code as u8,
        ll_extra: sequence.literal_length - LITERAL_LENGTH_BASELINES[ll_code],
        of_code: of_code as u8,
        of_extra: sequence.offset_value - (1u32 << of_code),
        ml_code: ml_code as u8,
        ml_extra: sequence.match_length - MATCH_LENGTH_BASELINES[ml_code],
    })
}

#[cfg(test)]
fn build_sequence_part_choice(
    part: SequencePart,
    codes: &[u8],
    previous: Option<&SequenceEncodingPartState>,
    parser_strategy: ParserStrategy,
    pool: &mut SequenceTablePool,
) -> Result<SequenceTableChoice> {
    let stats = analyze_codes(part, codes)?;
    let choice = select_table_choice(part, codes, previous, parser_strategy, &stats)?;
    build_selected_table_choice(part, codes, previous, &stats, choice, pool)
}

fn select_table_choice(
    part: SequencePart,
    codes: &[u8],
    previous: Option<&SequenceEncodingPartState>,
    parser_strategy: ParserStrategy,
    stats: &SequenceCodeStats,
) -> Result<CompressionMode> {
    if stats.most_frequent == stats.total {
        if part.default_allowed(stats.max_symbol) && stats.total <= 2 {
            return Ok(CompressionMode::Predefined);
        }
        return Ok(CompressionMode::Rle);
    }

    if let Some(choice) =
        select_table_choice_fast_path(part, codes, previous, parser_strategy, stats)
    {
        return Ok(choice);
    }

    let basic_cost = if part.default_allowed(stats.max_symbol) {
        {
            let (default_norm, default_log) = part.default_distribution();
            cross_entropy_cost_bits(default_norm, default_log, &stats.counts, stats.max_symbol)
        }
    } else {
        u64::MAX
    };
    let repeat_choice = previous.filter(|entry| {
        entry.repeat_mode() != SequenceRepeatMode::None
            && entry.table().supports_all_from_stats(stats)
    });
    let repeat_cost = match repeat_choice {
        Some(entry) => sequence_table_cost_bits(entry.table(), stats)?,
        None => u64::MAX,
    };
    let compressed_cost = (ncount_cost_bytes(part, stats)? as u64 * 8)
        + entropy_cost_bits(&stats.counts, stats.max_symbol, stats.total);

    if basic_cost <= repeat_cost && basic_cost <= compressed_cost {
        return Ok(CompressionMode::Predefined);
    }
    if repeat_cost <= compressed_cost {
        return Ok(CompressionMode::Repeat);
    }

    Ok(CompressionMode::FseCompressed)
}

fn select_table_choice_fast_path(
    part: SequencePart,
    _codes: &[u8],
    previous: Option<&SequenceEncodingPartState>,
    parser_strategy: ParserStrategy,
    stats: &SequenceCodeStats,
) -> Option<CompressionMode> {
    if parser_strategy.zstd_rank() >= ParserStrategy::Lazy.zstd_rank() {
        return None;
    }

    if !part.default_allowed(stats.max_symbol) {
        return Some(CompressionMode::FseCompressed);
    }

    let repeat_choice = previous.filter(|entry| {
        entry.repeat_mode() == SequenceRepeatMode::Valid
            && entry.table().supports_all_from_stats(stats)
    });
    if repeat_choice.is_some() && stats.total < FAST_SELECTOR_REPEAT_MAX_SEQUENCES {
        return Some(CompressionMode::Repeat);
    }

    let (_, default_log) = part.default_distribution();
    let multiplier = 10usize.saturating_sub(parser_strategy.zstd_rank() as usize);
    let dynamic_fse_nb_seq_min = ((1usize << default_log) * multiplier) >> 3;
    if stats.total < dynamic_fse_nb_seq_min
        || stats.most_frequent < (stats.total >> (default_log - 1))
    {
        return Some(CompressionMode::Predefined);
    }

    Some(CompressionMode::FseCompressed)
}

fn build_selected_table_choice(
    part: SequencePart,
    codes: &[u8],
    previous: Option<&SequenceEncodingPartState>,
    stats: &SequenceCodeStats,
    mode: CompressionMode,
    pool: &mut SequenceTablePool,
) -> Result<SequenceTableChoice> {
    match mode {
        CompressionMode::Predefined => Ok(build_predefined_table_choice(part)),
        CompressionMode::Rle => build_rle_table_choice(part, stats.max_symbol, pool),
        CompressionMode::Repeat => previous
            .filter(|entry| entry.repeat_mode() != SequenceRepeatMode::None)
            .filter(|entry| entry.table().supports_all(codes))
            .map(build_repeat_table_choice)
            .ok_or(Error::Generic),
        CompressionMode::FseCompressed => {
            build_compressed_table_choice(part, codes, stats, pool)?.ok_or(Error::Generic)
        }
    }
}

#[cfg(test)]
fn analyze_codes(part: SequencePart, codes: &[u8]) -> Result<SequenceCodeStats> {
    let max_allowed = part.max_symbol_value();
    let mut counts = [0u32; fse::SYMBOLVALUE_MAX + 1];
    // Tight histogram loop. Since codes are produced by our own encoder
    // (literal_length_code, match_length_code, offset_code) they are
    // guaranteed to be within range, so we use unchecked indexing.
    #[allow(unsafe_code)]
    for &code in codes {
        debug_assert!(code <= max_allowed);
        unsafe { *counts.get_unchecked_mut(code as usize) += 1 };
    }
    let mut max_symbol = 0u8;
    let mut most_frequent = 0u32;
    for (symbol, &count) in counts[..=max_allowed as usize].iter().enumerate() {
        if count > most_frequent {
            most_frequent = count;
            max_symbol = symbol as u8;
        }
    }
    // max_symbol should be the highest code, not the most frequent one
    for symbol in (0..=max_allowed as usize).rev() {
        if counts[symbol] > 0 {
            max_symbol = symbol as u8;
            break;
        }
    }
    Ok(SequenceCodeStats {
        counts,
        max_symbol,
        most_frequent: most_frequent as usize,
        total: codes.len(),
    })
}

/// Independent accumulators the fused histogram spreads its increments across
/// once a block is large enough to pay for them. Four, as
/// `HIST_count_parallel_wksp` uses.
const HISTOGRAM_LANES: usize = 4;

/// Sequences a block needs before [`count_codes_interleaved`] is worth its
/// setup over [`count_codes_serial`].
///
/// Bracketed by measurement rather than derived. At 3313 sequences a block
/// (`binary-structured`) and at 7582 (`log-lines`) the lanes are worth 2.8-3.3%
/// of the frame; at 1032 (`wikipedia`) they are worth nothing measurable; and
/// on the corpora that emit a single sequence a block (`mixed-entropy`,
/// `small-alphabet`, `repeated-chunk`) an unconditional four-lane histogram
/// cost 6-10%. This sits between the two ends that were measured. Below it the
/// serial loop runs, which is the code that shipped before the lanes existed,
/// so the floor here is "no change" rather than a regression.
const HISTOGRAM_LANE_MIN_SEQUENCES: usize = 2048;

type CodeCounts = [u32; fse::SYMBOLVALUE_MAX + 1];

/// One accumulator per part. Right for a short block, where there is no
/// dependency chain worth breaking and the lanes are pure setup cost.
///
/// Counts are accumulated into the caller's arrays rather than returned.
/// Returning three of them by value costs a 3 KiB copy per block, which is
/// nothing against a long block and about 1.5% of a frame whose blocks carry
/// one sequence each.
#[inline]
fn count_codes_serial(
    literal_codes: &[u8],
    offset_codes: &[u8],
    match_codes: &[u8],
    ll_counts: &mut CodeCounts,
    of_counts: &mut CodeCounts,
    ml_counts: &mut CodeCounts,
) {
    #[allow(unsafe_code)]
    for index in 0..literal_codes.len() {
        // The three slices are the same length, asserted by the caller, and a
        // `u8` always indexes a 256-wide table.
        unsafe {
            let ll = *literal_codes.get_unchecked(index);
            let of = *offset_codes.get_unchecked(index);
            let ml = *match_codes.get_unchecked(index);
            *ll_counts.get_unchecked_mut(ll as usize) += 1;
            *of_counts.get_unchecked_mut(of as usize) += 1;
            *ml_counts.get_unchecked_mut(ml as usize) += 1;
        }
    }
}

/// [`HISTOGRAM_LANES`] accumulators per part, recombined into the caller's
/// arrays, which is what `HIST_count_parallel_wksp` does and for the same
/// reason.
///
/// A histogram with one table per part serializes on store-to-load forwarding
/// whenever a code repeats, because consecutive increments hit the same
/// address. The double-fast corpora are exactly that case: `binary-structured`
/// takes the first repeat offset on 3795 of every 3835 sequences, so its offset
/// code is very nearly a constant and every increment waits on the one before
/// it. Four lanes put four of them in flight at once.
///
/// Out of line deliberately: the lanes are a 12 KiB stack frame, and there is
/// no reason to grow `analyze_all_codes` for the callers that never reach here.
#[inline(never)]
fn count_codes_interleaved(
    literal_codes: &[u8],
    offset_codes: &[u8],
    match_codes: &[u8],
    ll_counts: &mut CodeCounts,
    of_counts: &mut CodeCounts,
    ml_counts: &mut CodeCounts,
) {
    let total = literal_codes.len();
    let mut ll_lanes = [[0u32; fse::SYMBOLVALUE_MAX + 1]; HISTOGRAM_LANES];
    let mut of_lanes = [[0u32; fse::SYMBOLVALUE_MAX + 1]; HISTOGRAM_LANES];
    let mut ml_lanes = [[0u32; fse::SYMBOLVALUE_MAX + 1]; HISTOGRAM_LANES];

    #[allow(unsafe_code)]
    {
        let whole = total - total % HISTOGRAM_LANES;
        let mut index = 0;
        while index < whole {
            for (lane, ((ll_lane, of_lane), ml_lane)) in ll_lanes
                .iter_mut()
                .zip(of_lanes.iter_mut())
                .zip(ml_lanes.iter_mut())
                .enumerate()
            {
                // `index + lane < whole <= total`, the three slices are the
                // same length, and a `u8` always indexes a 256-wide table.
                unsafe {
                    let ll = *literal_codes.get_unchecked(index + lane);
                    let of = *offset_codes.get_unchecked(index + lane);
                    let ml = *match_codes.get_unchecked(index + lane);
                    *ll_lane.get_unchecked_mut(ll as usize) += 1;
                    *of_lane.get_unchecked_mut(of as usize) += 1;
                    *ml_lane.get_unchecked_mut(ml as usize) += 1;
                }
            }
            index += HISTOGRAM_LANES;
        }
        while index < total {
            unsafe {
                let ll = *literal_codes.get_unchecked(index);
                let of = *offset_codes.get_unchecked(index);
                let ml = *match_codes.get_unchecked(index);
                *ll_lanes[0].get_unchecked_mut(ll as usize) += 1;
                *of_lanes[0].get_unchecked_mut(of as usize) += 1;
                *ml_lanes[0].get_unchecked_mut(ml as usize) += 1;
            }
            index += 1;
        }
    }

    for lane in 0..HISTOGRAM_LANES {
        for symbol in 0..=fse::SYMBOLVALUE_MAX {
            ll_counts[symbol] += ll_lanes[lane][symbol];
            of_counts[symbol] += of_lanes[lane][symbol];
            ml_counts[symbol] += ml_lanes[lane][symbol];
        }
    }
}

/// Build all three sequence code histograms in a single pass over the code vectors.
/// This fuses the three separate `analyze_codes` calls into one loop iteration,
/// reducing loop overhead for the common case.
pub(crate) fn analyze_all_codes(
    literal_codes: &[u8],
    offset_codes: &[u8],
    match_codes: &[u8],
) -> (SequenceCodeStats, SequenceCodeStats, SequenceCodeStats) {
    debug_assert_eq!(literal_codes.len(), offset_codes.len());
    debug_assert_eq!(literal_codes.len(), match_codes.len());
    let total = literal_codes.len();
    let ll_max = SequencePart::LiteralLength.max_symbol_value() as usize;
    let of_max = SequencePart::Offset.max_symbol_value() as usize;
    let ml_max = SequencePart::MatchLength.max_symbol_value() as usize;

    // C's HIST_count_simple zeroes only (maxSymbolValue+1) entries, but its
    // arrays are sized to exactly that. These three are 256 wide, so zeroing
    // the prefix alone leaves the tail uninitialized, and an earlier version
    // did that behind `MaybeUninit::assume_init`. Materializing a `[u32; 256]`
    // that is only initialized up to `max` is undefined behavior whether or not
    // the tail is ever read, and it does not stay unread: the whole array is
    // copied by value into `SequenceCodeStats` and copied again by
    // `build_compressed_table_choice`. Zeroing all three is 3 KiB of memset
    // against a histogram pass over every sequence in the block plus the
    // normalization and cost estimation that follow, which is where the time
    // actually goes.
    // How the increments are spread depends on how many there are, and the two
    // cases are far apart: a `binary-structured` block carries about 3300
    // sequences, while `mixed-entropy`, `small-alphabet` and `repeated-chunk`
    // carry exactly one. Counting one symbol does not pay for lanes, and an
    // unconditional four-lane histogram cost those three corpora 6 to 10%.
    let mut ll_counts = [0u32; fse::SYMBOLVALUE_MAX + 1];
    let mut of_counts = [0u32; fse::SYMBOLVALUE_MAX + 1];
    let mut ml_counts = [0u32; fse::SYMBOLVALUE_MAX + 1];
    if total >= HISTOGRAM_LANE_MIN_SEQUENCES {
        count_codes_interleaved(
            literal_codes,
            offset_codes,
            match_codes,
            &mut ll_counts,
            &mut of_counts,
            &mut ml_counts,
        );
    } else {
        count_codes_serial(
            literal_codes,
            offset_codes,
            match_codes,
            &mut ll_counts,
            &mut of_counts,
            &mut ml_counts,
        );
    }

    #[inline(always)]
    fn finalize_stats(
        counts: [u32; fse::SYMBOLVALUE_MAX + 1],
        max_allowed: usize,
        total: usize,
    ) -> SequenceCodeStats {
        let mut most_frequent = 0u32;
        let mut max_symbol = 0u8;
        for symbol in (0..=max_allowed).rev() {
            let count = counts[symbol];
            if count > 0 && max_symbol == 0 {
                max_symbol = symbol as u8;
            }
            if count > most_frequent {
                most_frequent = count;
            }
        }
        SequenceCodeStats {
            counts,
            max_symbol,
            most_frequent: most_frequent as usize,
            total,
        }
    }

    (
        finalize_stats(ll_counts, ll_max, total),
        finalize_stats(of_counts, of_max, total),
        finalize_stats(ml_counts, ml_max, total),
    )
}

fn cross_entropy_cost_bits(
    default_norm: &[i16],
    default_log: u32,
    counts: &[u32; fse::SYMBOLVALUE_MAX + 1],
    max_symbol: u8,
) -> u64 {
    let shift = COST_ACCURACY_LOG - default_log;
    let mut cost = 0u64;
    for symbol in 0..=max_symbol as usize {
        let norm_acc = if default_norm[symbol] == -1 {
            1u32
        } else {
            default_norm[symbol] as u32
        };
        let norm256 = (norm_acc << shift) as usize;
        cost += u64::from(counts[symbol]) * u64::from(INVERSE_PROBABILITY_LOG256[norm256]);
    }
    cost >> COST_ACCURACY_LOG
}

fn entropy_cost_bits(
    counts: &[u32; fse::SYMBOLVALUE_MAX + 1],
    max_symbol: u8,
    total: usize,
) -> u64 {
    debug_assert!(total > 0);
    let mut cost = 0u64;
    for symbol in 0..=max_symbol as usize {
        let count = counts[symbol];
        if count == 0 {
            continue;
        }

        let mut norm = ((256u64 * u64::from(count)) / total as u64) as usize;
        if norm == 0 {
            norm = 1;
        }
        cost += u64::from(count) * u64::from(INVERSE_PROBABILITY_LOG256[norm]);
    }
    cost >> COST_ACCURACY_LOG
}

/// Prices the NCount header from the *undecremented* counts, while
/// [`build_compressed_table_choice`] normalizes the decremented ones. The two
/// therefore disagree, and deliberately so: upstream's `ZSTD_NCountCost`
/// makes the same choice.
///
/// Dropping one from the last symbol's count perturbs the normalized
/// distribution by at most one quantum, so the disagreement is bounded at one
/// byte: the header either crosses a byte boundary or it does not. Measured
/// across 9 corpora at 24 levels, 42 of 18,084 sections priced differently
/// from what the builder emitted (32 over, 10 under), and exactly one of those
/// flipped a mode decision — a 158-sequence match-length section that took
/// `Predefined` over `FseCompressed`. Correcting it is worth **one byte per
/// 187 MB**, and costs a second normalize plus `write_ncount` on the selection
/// path to collect.
///
/// The margins are genuinely that tight — 1.5% of mode decisions sit within
/// one header byte of their nearest rival — so this is a real mispricing and
/// not a cosmetic one. It is simply not worth the hot-path work. Do not
/// "fix" it without measuring what the fix buys.
///
///
/// The block splitter does not read this price. Its candidates are priced by
/// [`compressed_part_estimate`] from the decremented counts, the table
/// `ZSTD_estimateBlockSize` prices with.
fn ncount_cost_bytes(part: SequencePart, stats: &SequenceCodeStats) -> Result<usize> {
    let table_log = fse::optimal_table_log(
        part.max_accuracy_log() as u32,
        stats.total,
        stats.max_symbol.into(),
    );
    let mut normalized = [0i16; fse::SYMBOLVALUE_MAX + 1];
    if fse::normalize_count(
        &mut normalized,
        table_log,
        &stats.counts,
        stats.total,
        stats.max_symbol.into(),
        stats.total >= 2048,
    )? == 0
    {
        return Err(Error::Generic);
    }

    // On the stack, not the heap. This runs three times per block to price the
    // sequence code tables, and the header it writes is tens of bytes against a
    // statically known ceiling — `Encoder::encode_into_slice` was making 19
    // allocations per frame and every one of them came from here.
    let bound = fse::ncount_write_bound(stats.max_symbol.into(), table_log)?;
    let mut header = [0u8; fse::NCOUNT_WRITE_BOUND_MAX];
    fse::write_ncount(
        &mut header[..bound],
        &normalized,
        stats.max_symbol.into(),
        table_log,
    )
}

fn sequence_table_cost_bits(
    table: &SequenceEncodingTable,
    stats: &SequenceCodeStats,
) -> Result<u64> {
    if fse::ctable_max_symbol_value(table.ctable()) < u32::from(stats.max_symbol) {
        return Ok(u64::MAX);
    }

    let bad_cost = (fse::ctable_log(table.ctable()) + 1) << COST_ACCURACY_LOG;
    let mut cost = 0u64;
    for symbol in 0..=stats.max_symbol as usize {
        let count = stats.counts[symbol];
        if count == 0 {
            continue;
        }
        let bit_cost = fse::ctable_bit_cost(table.ctable(), symbol as u8, COST_ACCURACY_LOG)?;
        if bit_cost >= bad_cost {
            return Ok(u64::MAX);
        }
        cost += u64::from(count) * u64::from(bit_cost);
    }
    Ok(cost >> COST_ACCURACY_LOG)
}

/// Build a prefix-sum table of cumulative literal byte offsets.
/// `result[i]` = total literal bytes consumed by `sequences[0..i]`.
/// `result[n]` = total literal bytes consumed by all n sequences (not including trailing literals).
pub(crate) fn build_literal_offset_table(sequences: &[SequenceCommand]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(sequences.len() + 1);
    offsets.push(0);
    let mut cum = 0usize;
    for seq in sequences {
        cum += seq.literal_length as usize;
        offsets.push(cum);
    }
    offsets
}

/// What pricing a sub-block's literals needs from outside the sub-block.
#[derive(Clone, Copy)]
pub(crate) struct LiteralCostInputs<'a> {
    pub(crate) previous_table: Option<&'a huff0::CTableX1>,
    /// Upstream's `HUF_repeat_valid`. A block that built its own table leaves
    /// `HUF_repeat_check`, so this is narrower than `previous_table.is_some()`.
    pub(crate) repeat_valid: bool,
    /// Upstream's `ZSTD_literalsCompressionIsDisabled`.
    pub(crate) compression_disabled: bool,
    /// Upstream's `HUF_flags_optimalDepth`.
    pub(crate) table_depth: huff0::TableDepth,
}

/// Estimate the compressed size in bits for a sub-block defined by code slices
/// and a literal byte slice. Used by the post-sequence block splitter to decide
/// whether splitting improves compression. Matches C zstd's
/// `ZSTD_buildEntropyStatisticsAndEstimateSubBlockSize`.
///
/// When `prev_seq_tables` is provided, repeat mode is considered: if the
/// previous block's FSE tables can encode the sub-block more cheaply than
/// building new tables, we use the repeat cost (zero header). This matches
/// C zstd which passes `prevCBlock->entropy` through the block-split estimator.
pub(crate) fn estimate_subblock_cost_bits(
    literal_codes: &[u8],
    offset_codes: &[u8],
    match_codes: &[u8],
    literals: &[u8],
    prev_seq_tables: Option<&SequenceEncodingState>,
    literal_inputs: LiteralCostInputs<'_>,
) -> u64 {
    let nb_seq = literal_codes.len();
    if nb_seq == 0 {
        // Literals-only: raw literal cost + block header
        return (literals.len() as u64 * 8) + 24 + 24;
    }

    let (ll_stats, of_stats, ml_stats) =
        analyze_all_codes(literal_codes, offset_codes, match_codes);

    // Sequence cost: matching C's two-phase approach.
    // Phase 1 (select encoding type) uses Shannon entropy for compressed cost.
    // Phase 2 (estimate size) uses the selected type's cost function.
    // FSE table headers are returned separately and added after per-part truncation,
    // matching C's `ZSTD_estimateBlockSize_sequences` which adds `fseTablesSize` after.
    let ll_est = estimate_part_cost(
        SequencePart::LiteralLength,
        &ll_stats,
        prev_seq_tables.and_then(|s| s.entry(SequencePart::LiteralLength)),
        literal_codes[literal_codes.len() - 1],
    );
    let of_est = estimate_part_cost(
        SequencePart::Offset,
        &of_stats,
        prev_seq_tables.and_then(|s| s.entry(SequencePart::Offset)),
        offset_codes[offset_codes.len() - 1],
    );
    let ml_est = estimate_part_cost(
        SequencePart::MatchLength,
        &ml_stats,
        prev_seq_tables.and_then(|s| s.entry(SequencePart::MatchLength)),
        match_codes[match_codes.len() - 1],
    );
    // Per-symbol costs (byte-truncated per part) + FSE headers (exact bytes, added after)
    let seq_cost_bytes =
        ll_est.symbol_cost_bytes + of_est.symbol_cost_bytes + ml_est.symbol_cost_bytes;
    let fse_tables_size = ll_est.header_bytes + of_est.header_bytes + ml_est.header_bytes;

    // Sequence header: 1 (seqHead) + 1-3 (seq count) bytes
    let seq_count_bytes = if nb_seq < 128 {
        1u64
    } else if nb_seq < 0x7F00 {
        2
    } else {
        3
    };
    let seq_header = 1 + seq_count_bytes;

    // Literal cost: actual Huffman table construction matching C's approach,
    // with repeat-mode check against previous block's table.
    let lit_cost_bytes = if literals.is_empty() {
        0
    } else {
        estimate_literal_cost_bytes(literals, literal_inputs)
    };

    // Block header: 3 bytes. Total in bytes (matching C's ZSTD_estimateBlockSize).
    let total_bytes = seq_cost_bytes + fse_tables_size + seq_header + lit_cost_bytes + 3;
    // Return in bits for caller compatibility
    total_bytes * 8
}

/// Result of per-part cost estimation, split into symbol cost (byte-truncated)
/// and FSE header bytes (added separately), matching C's architecture where
/// `ZSTD_estimateBlockSize_symbolType` returns per-symbol costs and
/// `fseMetadata->fseTablesSize` is added afterward.
struct PartCostEstimate {
    /// Per-symbol FSE cost + extra bits, truncated to bytes per-part (>>3).
    symbol_cost_bytes: u64,
    /// What the part's table description takes on the wire: the NCount for
    /// `set_compressed`, the one symbol for `set_rle`.
    header_bytes: u64,
}

/// Estimate the cost of one sequence part (LL, OF, or ML), matching C's
/// two-phase approach: `ZSTD_selectEncodingType` (selection using Shannon
/// entropy) then `ZSTD_estimateBlockSize_symbolType` (estimation using the
/// chosen type's cost function).
fn estimate_part_cost(
    part: SequencePart,
    stats: &SequenceCodeStats,
    prev_state: Option<&SequenceEncodingPartState>,
    last_code: u8,
) -> PartCostEstimate {
    if stats.total == 0 {
        return PartCostEstimate {
            symbol_cost_bytes: 0,
            header_bytes: 0,
        };
    }

    // Compute total extra bits for all sequences: sum(count[code] * extra_bits(code)).
    let total_extra_bits: u64 = match part {
        SequencePart::LiteralLength => (0..=stats.max_symbol as usize)
            .map(|c| u64::from(stats.counts[c]) * u64::from(ll_bits(c as u8)))
            .sum(),
        SequencePart::Offset => (0..=stats.max_symbol as usize)
            .map(|c| u64::from(stats.counts[c]) * c as u64)
            .sum(),
        SequencePart::MatchLength => (0..=stats.max_symbol as usize)
            .map(|c| u64::from(stats.counts[c]) * u64::from(ml_bits(c as u8)))
            .sum(),
    };

    if stats.most_frequent == stats.total {
        // C: if (isDefaultAllowed && nbSeq <= 2) → set_basic, else → set_rle
        if part.default_allowed(stats.max_symbol) && stats.total <= 2 {
            let (default_norm, default_log) = part.default_distribution();
            let basic_bits =
                cross_entropy_cost_bits(default_norm, default_log, &stats.counts, stats.max_symbol);
            return PartCostEstimate {
                symbol_cost_bytes: (basic_bits + total_extra_bits) >> 3,
                header_bytes: 0,
            };
        }
        return PartCostEstimate {
            symbol_cost_bytes: total_extra_bits >> 3,
            header_bytes: 1,
        };
    }

    // --- Phase 1: Select encoding type (matching ZSTD_selectEncodingType) ---
    // For strategy >= ZSTD_lazy (always true for L16+), C compares costs:
    //   basicCost    = ZSTD_crossEntropyCost(defaultNorm, defaultNormLog, count, max)
    //   repeatCost   = ZSTD_fseBitCost(prevCTable, count, max)
    //   compressedCost = (NCountCost << 3) + ZSTD_entropyCost(count, max, nbSeq)
    let basic_cost = if part.default_allowed(stats.max_symbol) {
        let (default_norm, default_log) = part.default_distribution();
        cross_entropy_cost_bits(default_norm, default_log, &stats.counts, stats.max_symbol)
    } else {
        u64::MAX
    };

    let repeat_cost = match prev_state {
        Some(entry)
            if entry.repeat_mode() != SequenceRepeatMode::None
                && entry.table().supports_all_from_stats(stats) =>
        {
            sequence_table_cost_bits(entry.table(), stats).unwrap_or(u64::MAX)
        }
        _ => u64::MAX,
    };

    // C: compressedCost = (NCountCost << 3) + ZSTD_entropyCost(count, max, nbSeq)
    // Uses Shannon entropy for selection, not normalized cross-entropy.
    let ncount_header_bytes = ncount_cost_bytes(part, stats).unwrap_or(usize::MAX);
    let compressed_selection_cost = if ncount_header_bytes < usize::MAX {
        let shannon_bits = entropy_cost_bits(&stats.counts, stats.max_symbol, stats.total);
        (ncount_header_bytes as u64 * 8).saturating_add(shannon_bits)
    } else {
        u64::MAX
    };

    // C selection priority: basic <= repeat <= compressed
    // "if (basicCost <= repeatCost && basicCost <= compressedCost) → set_basic"
    // "if (repeatCost <= compressedCost) → set_repeat"
    // "else → set_compressed"
    enum SelectedType {
        Basic,
        Repeat,
        Compressed,
    }
    let selected = if basic_cost <= repeat_cost && basic_cost <= compressed_selection_cost {
        SelectedType::Basic
    } else if repeat_cost <= compressed_selection_cost {
        SelectedType::Repeat
    } else {
        SelectedType::Compressed
    };

    // --- Phase 2: Estimate using chosen type (matching ZSTD_estimateBlockSize_symbolType) ---
    // C: per-symbol cost uses the CHOSEN type's cost function, then adds extra bits,
    // then truncates to bytes. FSE headers are separate.
    match selected {
        SelectedType::Basic => {
            // C: ZSTD_crossEntropyCost(defaultNorm, defaultNormLog, countWksp, max)
            PartCostEstimate {
                symbol_cost_bytes: (basic_cost + total_extra_bits) >> 3,
                header_bytes: 0,
            }
        }
        SelectedType::Repeat => {
            // C: ZSTD_fseBitCost(fseCTable, countWksp, max)
            PartCostEstimate {
                symbol_cost_bytes: (repeat_cost + total_extra_bits) >> 3,
                header_bytes: 0,
            }
        }
        SelectedType::Compressed => match compressed_part_estimate(part, stats, last_code) {
            Some((code_cost, header_bytes)) => PartCostEstimate {
                symbol_cost_bytes: (code_cost + total_extra_bits) >> 3,
                header_bytes,
            },
            // `ZSTD_estimateBlockSize_symbolType` prices a table it cannot
            // build at `nbSeq * 10`.
            None => PartCostEstimate {
                symbol_cost_bytes: stats.total as u64 * 10,
                header_bytes: ncount_header_bytes as u64,
            },
        },
    }
}

/// The NCount header and per-symbol cost of the table the encoder will build,
/// which is not the table the selection priced: `ZSTD_buildCTable` drops one
/// from the last code's count before normalizing, `ZSTD_NCountCost` does not.
fn compressed_part_estimate(
    part: SequencePart,
    stats: &SequenceCodeStats,
    last_code: u8,
) -> Option<(u64, u64)> {
    let table_log = fse::optimal_table_log(
        part.max_accuracy_log() as u32,
        stats.total,
        stats.max_symbol.into(),
    );
    let mut counts = stats.counts;
    let mut total = stats.total;
    if counts[usize::from(last_code)] > 1 {
        counts[usize::from(last_code)] -= 1;
        total -= 1;
    }
    let mut normalized = [0i16; fse::SYMBOLVALUE_MAX + 1];
    let effective_table_log = match fse::normalize_count(
        &mut normalized,
        table_log,
        &counts,
        total,
        stats.max_symbol.into(),
        total >= 2048,
    ) {
        Ok(tl) if tl > 0 => tl,
        _ => return None,
    };
    let bound = fse::ncount_write_bound(stats.max_symbol.into(), table_log).ok()?;
    let mut header = [0u8; fse::NCOUNT_WRITE_BOUND_MAX];
    let header_bytes = fse::write_ncount(
        &mut header[..bound],
        &normalized,
        stats.max_symbol.into(),
        table_log,
    )
    .ok()?;

    let mut ctable = fse::CTable::default();
    fse::build_ctable(
        &mut ctable,
        &normalized,
        stats.max_symbol as u32,
        effective_table_log,
    )
    .ok()?;
    let bad_cost = (effective_table_log + 1) << COST_ACCURACY_LOG;
    let mut cost = 0u64;
    for symbol in 0..=stats.max_symbol as usize {
        if stats.counts[symbol] == 0 {
            continue;
        }
        let bit_cost = fse::ctable_bit_cost(&ctable, symbol as u8, COST_ACCURACY_LOG).ok()?;
        if bit_cost >= bad_cost {
            return None;
        }
        cost += u64::from(stats.counts[symbol]) * u64::from(bit_cost);
    }
    Some((cost >> COST_ACCURACY_LOG, header_bytes as u64))
}

fn estimate_literal_cost_bytes(literals: &[u8], inputs: LiteralCostInputs<'_>) -> u64 {
    // Upstream answers `set_basic` before it counts anything.
    if inputs.compression_disabled {
        return literals.len() as u64;
    }
    huff0::estimate_literal_cost_bytes(
        literals,
        inputs.previous_table,
        inputs.repeat_valid,
        inputs.table_depth,
    ) as u64
}

/// Reconcile repcodes for a sub-block when the previous sub-block was emitted
/// as raw/RLE (meaning the decoder's repcode state didn't advance through its
/// sequences). Matches C zstd's `ZSTD_seqStore_resolveOffCodes`.
pub(crate) fn reconcile_subblock_repcodes(
    sequences: &mut [SequenceCommand],
    offset_codes: &mut [u8],
    decoder_reps: &mut RepeatOffsets,
    compressor_reps: &mut RepeatOffsets,
) {
    for (i, seq) in sequences.iter_mut().enumerate() {
        let ll0 = seq.literal_length == 0;

        if seq.offset_value >= 1 && seq.offset_value <= 3 {
            // This sequence uses a repcode. Resolve what raw offset it means
            // in the compressor's state vs the decoder's state.
            let c_raw = resolve_repcode_to_raw(compressor_reps.values(), seq.offset_value, ll0);
            let d_raw = resolve_repcode_to_raw(decoder_reps.values(), seq.offset_value, ll0);

            if c_raw != d_raw {
                // Decoder would get wrong offset. Replace with explicit offset.
                seq.offset_value = c_raw + 3;
                offset_codes[i] = offset_code_unchecked(seq.offset_value);
            }
        }

        // Advance both repcode states with the (possibly patched) offset_value
        advance_rep_state(compressor_reps, seq.offset_value, ll0);
        advance_rep_state(decoder_reps, seq.offset_value, ll0);
    }
}

fn resolve_repcode_to_raw(reps: [u32; 3], offset_value: u32, ll0: bool) -> u32 {
    let adj = if ll0 { offset_value } else { offset_value - 1 };
    if adj == 3 {
        reps[0].saturating_sub(1)
    } else {
        reps[adj as usize]
    }
}

fn advance_rep_state(reps: &mut RepeatOffsets, offset_value: u32, ll0: bool) {
    // Simulate what encode_offset_value_and_update does, but without modifying the sequence
    let vals = reps.values();
    if offset_value >= 4 {
        // Explicit offset
        let raw = offset_value - 3;
        *reps = RepeatOffsets::from_values([raw, vals[0], vals[1]]);
    } else {
        // Repcode
        let adj = if ll0 { offset_value } else { offset_value - 1 };
        match adj {
            0 => { /* rep0 with ll>0: no change */ }
            1 => *reps = RepeatOffsets::from_values([vals[1], vals[0], vals[2]]),
            2 => *reps = RepeatOffsets::from_values([vals[2], vals[0], vals[1]]),
            3 => {
                let new0 = vals[0].saturating_sub(1);
                *reps = RepeatOffsets::from_values([new0, vals[0], vals[1]]);
            }
            _ => {}
        }
    }
}

/// Whether an offset code is wide enough that its extra bits have to be split
/// across two flushes, C zstd's `longOffsets`.
///
/// An offset code never exceeds 31 and the accumulator holds
/// `STREAM_ACCUMULATOR_MIN` bits between flushes, so this is false today. It
/// stays a function of the accumulator width rather than a constant because
/// that is what it depends on: the split exists for accumulators too narrow to
/// hold one offset, and the encoder has no other guard against that.
fn detect_long_offsets(offset_codes: &[u8]) -> bool {
    offset_codes
        .iter()
        .copied()
        .any(|code| u32::from(code) >= STREAM_ACCUMULATOR_MIN)
}

fn build_predefined_table_choice(part: SequencePart) -> SequenceTableChoice {
    let tables = predefined_sequence_ctables();
    SequenceTableChoice {
        mode: CompressionMode::Predefined,
        header: Vec::new(),
        encoding: match part {
            SequencePart::LiteralLength => tables.literal_lengths.clone(),
            SequencePart::Offset => tables.offsets.clone(),
            SequencePart::MatchLength => tables.match_lengths.clone(),
        },
        persist_mode: None,
    }
}

fn build_repeat_table_choice(previous: &SequenceEncodingPartState) -> SequenceTableChoice {
    SequenceTableChoice {
        mode: CompressionMode::Repeat,
        header: Vec::new(),
        encoding: previous.table().clone(),
        persist_mode: Some(previous.repeat_mode()),
    }
}

fn build_rle_table_choice(
    part: SequencePart,
    symbol: u8,
    pool: &mut SequenceTablePool,
) -> Result<SequenceTableChoice> {
    let encoding = pool.build(part, |inner| {
        fse::build_rle_ctable(&mut inner.table, symbol);
        inner.supported_symbols.fill(false);
        inner.supported_symbols[symbol as usize] = true;
        Ok(())
    })?;
    Ok(SequenceTableChoice {
        mode: CompressionMode::Rle,
        header: vec![symbol],
        encoding,
        persist_mode: None,
    })
}

fn build_compressed_table_choice(
    part: SequencePart,
    codes: &[u8],
    stats: &SequenceCodeStats,
    pool: &mut SequenceTablePool,
) -> Result<Option<SequenceTableChoice>> {
    if codes.len() <= 1 {
        return Ok(None);
    }

    let table_log = fse::optimal_table_log(
        part.max_accuracy_log() as u32,
        codes.len(),
        stats.max_symbol.into(),
    );
    let mut effective_counts = stats.counts;
    let last_symbol = *codes.last().ok_or(Error::UnexpectedEof)? as usize;
    let mut effective_total = codes.len();
    if effective_counts[last_symbol] > 1 {
        effective_counts[last_symbol] -= 1;
        effective_total -= 1;
    }
    if effective_total <= 1 {
        return Ok(None);
    }
    let mut normalized = [0i16; fse::SYMBOLVALUE_MAX + 1];
    if fse::normalize_count(
        &mut normalized,
        table_log,
        &effective_counts,
        effective_total,
        stats.max_symbol.into(),
        effective_total >= 2048,
    )? == 0
    {
        return Ok(None);
    }

    let mut header = vec![0u8; fse::ncount_write_bound(stats.max_symbol.into(), table_log)?];
    let header_size =
        fse::write_ncount(&mut header, &normalized, stats.max_symbol.into(), table_log)?;
    header.truncate(header_size);

    let encoding = pool.build(part, |inner| {
        inner.fill_from_normalized_counts(&normalized, stats.max_symbol.into(), table_log)
    })?;
    // `from_normalized_counts` has already recorded which symbols the table can
    // encode, and asking the normalized counts is the authoritative way to know:
    // the question is what the *table* covers, not what this block happened to
    // contain. For a table built from this block's own codes the two agree --
    // `normalize_count` gives every symbol with a nonzero count a nonzero share,
    // `low_prob_count` of -1 or 1 when it falls under the threshold, and leaves
    // absent symbols at zero -- so deriving the set a second time by scanning
    // `codes` was a pass over every sequence in the block to rebuild the array
    // that was already there. It was 39% of this stage's samples on `log-lines`.
    debug_assert_eq!(
        encoding.inner.supported_symbols,
        supported_symbols_from_codes(codes),
        "the table's own symbol coverage must agree with the codes it was built from",
    );
    Ok(Some(SequenceTableChoice {
        mode: CompressionMode::FseCompressed,
        header,
        encoding,
        persist_mode: Some(SequenceRepeatMode::Check),
    }))
}

pub(crate) fn repeat_mode_for_normalized_counts(
    normalized: &[i16; fse::SYMBOLVALUE_MAX + 1],
    dict_max_symbol_value: u32,
    required_max_symbol_value: u32,
) -> SequenceRepeatMode {
    if dict_max_symbol_value < required_max_symbol_value {
        return SequenceRepeatMode::Check;
    }

    for symbol in 0..=required_max_symbol_value as usize {
        if normalized[symbol] == 0 {
            return SequenceRepeatMode::Check;
        }
    }

    SequenceRepeatMode::Valid
}

fn all_symbols_supported() -> [bool; fse::SYMBOLVALUE_MAX + 1] {
    [true; fse::SYMBOLVALUE_MAX + 1]
}

/// The symbols a block's codes actually use.
///
/// Debug-only: this is the reference the assertion in
/// [`build_compressed_table_choice`] checks the table's own coverage against.
/// Nothing on the encoding path asks the codes any more, because a pass over
/// every sequence is a needless way to learn what the normalized counts
/// already say. `debug_assert_eq!` still type-checks its arguments in release,
/// so this stays compiled in and is optimized out rather than being `cfg`-gated.
fn supported_symbols_from_codes(codes: &[u8]) -> [bool; fse::SYMBOLVALUE_MAX + 1] {
    let mut supported = [false; fse::SYMBOLVALUE_MAX + 1];
    for &code in codes {
        supported[code as usize] = true;
    }
    supported
}

pub(crate) fn literal_length_code(literal_length: u32) -> Result<u32> {
    if literal_length <= 63 {
        Ok(u32::from(
            LITERAL_LENGTH_CODE_TABLE[literal_length as usize],
        ))
    } else if literal_length <= 0x1_FFFF {
        Ok(highbit32(literal_length) + 19)
    } else {
        Err(Error::InvalidParameter(
            "literal length exceeds the supported range",
        ))
    }
}

pub(crate) fn match_length_code(match_length: u32) -> Result<u32> {
    if match_length < 3 {
        return Err(Error::InvalidParameter(
            "match length must be at least 3 bytes",
        ));
    }

    let ml_base = match_length - 3;
    if ml_base <= 127 {
        Ok(u32::from(MATCH_LENGTH_CODE_TABLE[ml_base as usize]))
    } else if ml_base <= 0x1_FFFF {
        Ok(highbit32(ml_base) + 36)
    } else {
        Err(Error::InvalidParameter(
            "match length exceeds the supported range",
        ))
    }
}

pub(crate) fn offset_code(offset_value: u32) -> Result<u32> {
    if offset_value == 0 {
        return Err(Error::InvalidParameter("offset value must be non-zero"));
    }
    Ok(highbit32(offset_value))
}

/// Infallible literal_length_code for encoder hot paths where the value is
/// guaranteed to be in range (produced by our own match finder).
#[inline(always)]
pub(crate) fn literal_length_code_unchecked(literal_length: u32) -> u8 {
    if literal_length <= 63 {
        LITERAL_LENGTH_CODE_TABLE[literal_length as usize]
    } else {
        debug_assert!(literal_length <= 0x1_FFFF);
        (highbit32(literal_length) + 19) as u8
    }
}

/// Infallible match_length_code for encoder hot paths.
#[inline(always)]
pub(crate) fn match_length_code_unchecked(match_length: u32) -> u8 {
    debug_assert!(match_length >= 3);
    let ml_base = match_length - 3;
    if ml_base <= 127 {
        MATCH_LENGTH_CODE_TABLE[ml_base as usize]
    } else {
        debug_assert!(ml_base <= 0x1_FFFF);
        (highbit32(ml_base) + 36) as u8
    }
}

/// Infallible offset_code for encoder hot paths.
#[inline(always)]
pub(crate) fn offset_code_unchecked(offset_value: u32) -> u8 {
    debug_assert!(offset_value > 0);
    highbit32(offset_value) as u8
}

pub(crate) fn estimate_sequence_bit_cost(
    raw_offset: u32,
    literal_length: u32,
    match_length: u32,
    repeat_offsets: RepeatOffsets,
) -> Result<u32> {
    let mut repeat_offsets = repeat_offsets;
    let offset_value = repeat_offsets.encode_offset_value(raw_offset, literal_length)?;
    let ll_code = literal_length_code(literal_length)? as u8;
    let ml_code = match_length_code(match_length)? as u8;
    let of_code = offset_code(offset_value)?;

    // Approximate the FSE symbol contribution with a small constant budget.
    Ok(ll_bits(ll_code) + ml_bits(ml_code) + of_code + 22)
}

fn encode_sequence_bitstream_into(
    dst: &mut Vec<u8>,
    sequences: &[EncodedSequence],
    extra_bits: usize,
    literal_lengths: &fse::CTable,
    offsets: &fse::CTable,
    match_lengths: &fse::CTable,
    long_offsets: bool,
) -> Result<()> {
    let bit_capacity = sequence_bitstream_bound_bits(
        sequences.len(),
        extra_bits,
        literal_lengths,
        offsets,
        match_lengths,
    );
    let byte_capacity = bit_capacity.div_ceil(8) + CONTAINER_BYTES + 8;
    if dst.len() < byte_capacity {
        dst.resize(byte_capacity, 0);
    }

    let written = {
        let mut stream = BitCStream::new(&mut dst[..byte_capacity])?;
        let last = sequences.last().ok_or(Error::UnexpectedEof)?;
        let mut state_match_lengths = fse::CState::default();
        let mut state_offsets = fse::CState::default();
        let mut state_literal_lengths = fse::CState::default();

        fse::init_cstate2(&mut state_match_lengths, match_lengths, last.ml_code)?;
        fse::init_cstate2(&mut state_offsets, offsets, last.of_code)?;
        fse::init_cstate2(&mut state_literal_lengths, literal_lengths, last.ll_code)?;

        add_bits_checked(&mut stream, last.ll_extra.into(), ll_bits(last.ll_code))?;
        add_bits_checked(&mut stream, last.ml_extra.into(), ml_bits(last.ml_code))?;
        add_offset_bits_checked(
            &mut stream,
            last.of_extra.into(),
            u32::from(last.of_code),
            long_offsets,
        )?;
        // Matches the `BIT_flushBits` that closes the first-symbol section of
        // C's `ZSTD_encodeSequences_body`. Without it the extra bits written
        // above (up to 16 + 16 + 31) stay in the accumulator and the hot loop
        // below starts with `bit_pos` far above the 7 it assumes, overflowing
        // the accumulator and silently corrupting the bitstream.
        stream.flush_bits();

        // Hot loop matching C zstd's ZSTD_encodeSequences_body on 64-bit.
        // After the previous flush, bit_pos ≤ 7. The 3 FSE encodes add at
        // most 8+9+9 = 26 bits (OffFSELog=8, ML/LLFSELog=9) → max 33.
        // ll_extra adds at most 16 → max 49. All under 64 for add_bits.
        // Before ml_extra + of_extra (max 16+31=47), a conditional flush
        // ensures bit_pos ≤ 7 when accumulated bits would reach ≥ 57.
        // For typical data (rep1, small extras), the conditional rarely fires
        // and only the final flush writes to memory.
        //
        // SAFETY: symbol codes are bounded by the FSE table construction
        // (literal_length_code ≤ 35, match_length_code ≤ 52, offset_code ≤ 31,
        // all within SYMBOLVALUE_MAX=255). State table indices are bounded by
        // the FSE normalization. Bit capacity is pre-allocated above.
        #[allow(unsafe_code)]
        if !long_offsets {
            for sequence in sequences[..sequences.len() - 1].iter().rev() {
                let ml_nbits = ml_bits(sequence.ml_code);
                let of_nbits = u32::from(sequence.of_code);
                unsafe {
                    fse::encode_symbol_unchecked(
                        &mut stream,
                        &mut state_offsets,
                        offsets,
                        sequence.of_code,
                    );
                    fse::encode_symbol_unchecked(
                        &mut stream,
                        &mut state_match_lengths,
                        match_lengths,
                        sequence.ml_code,
                    );
                    fse::encode_symbol_unchecked(
                        &mut stream,
                        &mut state_literal_lengths,
                        literal_lengths,
                        sequence.ll_code,
                    );
                }
                stream.add_bits_fast(sequence.ll_extra.into(), ll_bits(sequence.ll_code));
                if stream.bit_pos + ml_nbits + of_nbits >= STREAM_ACCUMULATOR_MIN {
                    stream.flush_bits_fast();
                }
                stream.add_bits_fast(sequence.ml_extra.into(), ml_nbits);
                stream.add_bits_fast(sequence.of_extra.into(), of_nbits);
                stream.flush_bits_fast();
            }
        } else {
            for sequence in sequences[..sequences.len() - 1].iter().rev() {
                ensure_bits_capacity(&mut stream, fse::ctable_log(offsets))?;
                fse::encode_symbol(&mut stream, &mut state_offsets, offsets, sequence.of_code)?;
                ensure_bits_capacity(&mut stream, fse::ctable_log(match_lengths))?;
                fse::encode_symbol(
                    &mut stream,
                    &mut state_match_lengths,
                    match_lengths,
                    sequence.ml_code,
                )?;
                ensure_bits_capacity(&mut stream, fse::ctable_log(literal_lengths))?;
                fse::encode_symbol(
                    &mut stream,
                    &mut state_literal_lengths,
                    literal_lengths,
                    sequence.ll_code,
                )?;

                add_bits_checked(
                    &mut stream,
                    sequence.ll_extra.into(),
                    ll_bits(sequence.ll_code),
                )?;
                add_bits_checked(
                    &mut stream,
                    sequence.ml_extra.into(),
                    ml_bits(sequence.ml_code),
                )?;
                add_offset_bits_checked(
                    &mut stream,
                    sequence.of_extra.into(),
                    u32::from(sequence.of_code),
                    long_offsets,
                )?;
            }
        }

        ensure_bits_capacity(&mut stream, fse::ctable_log(match_lengths))?;
        fse::flush_cstate(&mut stream, &state_match_lengths, match_lengths)?;
        ensure_bits_capacity(&mut stream, fse::ctable_log(offsets))?;
        fse::flush_cstate(&mut stream, &state_offsets, offsets)?;
        ensure_bits_capacity(&mut stream, fse::ctable_log(literal_lengths))?;
        fse::flush_cstate(&mut stream, &state_literal_lengths, literal_lengths)?;

        stream.close()
    };

    if written == 0 {
        return Err(Error::DstSizeTooSmall);
    }
    dst.truncate(written);
    Ok(())
}

/// Exact size, in bits, of the sequence bitstream the block will produce.
///
/// Each sequence writes three FSE symbols and then its raw payload, and each
/// FSE symbol costs at most its table's log; the three initial states cost one
/// full set of logs on top. `extra_bits` is the payload total, from
/// [`extra_bits_from_code_counts`].
fn sequence_bitstream_bound_bits(
    sequence_count: usize,
    extra_bits: usize,
    literal_lengths: &fse::CTable,
    offsets: &fse::CTable,
    match_lengths: &fse::CTable,
) -> usize {
    let state_bits = (fse::ctable_log(literal_lengths)
        + fse::ctable_log(offsets)
        + fse::ctable_log(match_lengths)) as usize;
    let mut bits = state_bits + extra_bits;
    if sequence_count > 1 {
        bits += (sequence_count - 1) * state_bits;
    }
    bits + 1
}

/// Raw payload bits the block's sequences write, read off the code histograms.
///
/// Alongside its three FSE symbols every sequence writes
/// `LL_bits[ll_code] + ML_bits[ml_code] + of_code` bits verbatim, and the
/// bitstream buffer has to be sized for the sum of those. Adding them up one
/// sequence at a time is a fourth pass over the three code arrays, which on a
/// text block cost around a sixth of the bitstream stage -- nineteen
/// instructions per sequence against the encoding loop's hundred, and four of
/// them bounds checks. The histograms `analyze_all_codes` has already built
/// answer the same question over the alphabets instead, in a count of terms
/// that does not grow with the sequence count at all. Upstream sizes its buffer
/// from the block's remaining output capacity and so never asks.
///
/// Each sum runs its part's alphabet, not the full 256-wide histogram: a block
/// can carry a single sequence, and a fixed 768-term sweep costs such a block
/// more than the per-sequence pass it replaces -- measurably, at 1.5 to 2.9% of
/// frame throughput on `mixed-entropy`, `small-alphabet` and `repeated-chunk`.
/// `max_symbol` bounds it to what the block actually used.
///
/// That makes the result exact only while no code exceeds its part's alphabet,
/// since `max_symbol` is itself capped there and a code above it would be
/// counted as zero -- and a bound that is too small sizes a buffer
/// `flush_bits_unchecked` then writes past. The encoding loop rests on the same
/// invariant one line further on, where it reads the length baselines with
/// `get_unchecked` on the same codes, so this adds no new requirement; the
/// debug assertion states it where it is relied on.
fn extra_bits_from_code_counts(
    literal_stats: &SequenceCodeStats,
    offset_stats: &SequenceCodeStats,
    match_stats: &SequenceCodeStats,
) -> usize {
    /// Sum `counts[code] * width(code)` over `0..=last`.
    fn weighted(stats: &SequenceCodeStats, last: usize, width: impl Fn(usize) -> usize) -> usize {
        debug_assert_eq!(
            stats.counts[..=last].iter().sum::<u32>() as usize,
            stats.total,
            "a sequence code outside its alphabet would size the bitstream buffer short"
        );
        (0..=last)
            .map(|code| stats.counts[code] as usize * width(code))
            .sum()
    }

    /// `max_symbol` is capped at the part's alphabet already; the `min` is what
    /// lets the table index below carry no bounds check.
    fn alphabet_end(stats: &SequenceCodeStats, extra_bits: &[u8]) -> usize {
        (stats.max_symbol as usize).min(extra_bits.len() - 1)
    }

    let literal_end = alphabet_end(literal_stats, &LITERAL_LENGTH_EXTRA_BITS);
    let match_end = alphabet_end(match_stats, &MATCH_LENGTH_EXTRA_BITS);
    weighted(literal_stats, literal_end, |code| {
        LITERAL_LENGTH_EXTRA_BITS[code] as usize
    }) + weighted(match_stats, match_end, |code| {
        MATCH_LENGTH_EXTRA_BITS[code] as usize
    })
        // An offset code is a bit position, so it is its own field width.
        + weighted(offset_stats, offset_stats.max_symbol as usize, |code| code)
}

/// Encode sequences directly from code slices + SequenceCommands, computing
/// extras inline in the hot loop. This eliminates the intermediate
/// Vec<EncodedSequence> allocation and O(n) preparation pass.
fn encode_sequence_bitstream_direct_into(
    dst: &mut Vec<u8>,
    sequences: &[SequenceCommand],
    ll_codes: &[u8],
    of_codes: &[u8],
    ml_codes: &[u8],
    extra_bits: usize,
    literal_lengths_table: &fse::CTable,
    offsets_table: &fse::CTable,
    match_lengths_table: &fse::CTable,
    long_offsets: bool,
) -> Result<()> {
    let bit_capacity = sequence_bitstream_bound_bits(
        sequences.len(),
        extra_bits,
        literal_lengths_table,
        offsets_table,
        match_lengths_table,
    );
    let byte_capacity = bit_capacity.div_ceil(8) + CONTAINER_BYTES + 8;
    if dst.len() < byte_capacity {
        dst.resize(byte_capacity, 0);
    }

    let n = sequences.len();
    let written = {
        let mut stream = BitCStream::new(&mut dst[..byte_capacity])?;
        let mut state_match_lengths = fse::CState::default();
        let mut state_offsets = fse::CState::default();
        let mut state_literal_lengths = fse::CState::default();

        let last_ll_code = ll_codes[n - 1];
        let last_of_code = of_codes[n - 1];
        let last_ml_code = ml_codes[n - 1];
        let last = &sequences[n - 1];

        fse::init_cstate2(&mut state_match_lengths, match_lengths_table, last_ml_code)?;
        fse::init_cstate2(&mut state_offsets, offsets_table, last_of_code)?;
        fse::init_cstate2(
            &mut state_literal_lengths,
            literal_lengths_table,
            last_ll_code,
        )?;

        let last_ll_extra = last.literal_length - LITERAL_LENGTH_BASELINES[last_ll_code as usize];
        let last_of_extra = last.offset_value - (1u32 << last_of_code);
        let last_ml_extra = last.match_length - MATCH_LENGTH_BASELINES[last_ml_code as usize];

        add_bits_checked(&mut stream, last_ll_extra.into(), ll_bits(last_ll_code))?;
        add_bits_checked(&mut stream, last_ml_extra.into(), ml_bits(last_ml_code))?;
        add_offset_bits_checked(
            &mut stream,
            last_of_extra.into(),
            u32::from(last_of_code),
            long_offsets,
        )?;
        // See the matching comment in `encode_sequences_section`: C flushes
        // here, and the hot loop below depends on it.
        stream.flush_bits();

        #[allow(unsafe_code)]
        if !long_offsets {
            for i in (0..n - 1).rev() {
                // All arrays have length n, so i < n-1 < n is always in bounds.
                // Codes are bounded by their FSE table construction (ll: 0..35,
                // ml: 0..52, of: 0..31). Use unchecked access to eliminate bounds
                // checking branches from the inner loop.
                unsafe {
                    let seq = sequences.get_unchecked(i);
                    let ll_code = *ll_codes.get_unchecked(i);
                    let of_code = *of_codes.get_unchecked(i);
                    let ml_code = *ml_codes.get_unchecked(i);
                    let ml_nbits = ml_bits(ml_code);
                    let of_nbits = u32::from(of_code);
                    fse::encode_symbol_unchecked(
                        &mut stream,
                        &mut state_offsets,
                        offsets_table,
                        of_code,
                    );
                    fse::encode_symbol_unchecked(
                        &mut stream,
                        &mut state_match_lengths,
                        match_lengths_table,
                        ml_code,
                    );
                    fse::encode_symbol_unchecked(
                        &mut stream,
                        &mut state_literal_lengths,
                        literal_lengths_table,
                        ll_code,
                    );
                    let ll_extra = seq.literal_length
                        - *LITERAL_LENGTH_BASELINES.get_unchecked(ll_code as usize);
                    let of_extra = seq.offset_value - (1u32 << of_code);
                    let ml_extra =
                        seq.match_length - *MATCH_LENGTH_BASELINES.get_unchecked(ml_code as usize);
                    stream.add_bits_fast(ll_extra.into(), ll_bits(ll_code));
                    if stream.bit_pos + ml_nbits + of_nbits >= STREAM_ACCUMULATOR_MIN {
                        stream.flush_bits_unchecked();
                    }
                    stream.add_bits_fast(ml_extra.into(), ml_nbits);
                    stream.add_bits_fast(of_extra.into(), of_nbits);
                    stream.flush_bits_unchecked();
                }
            }
        } else {
            for i in (0..n - 1).rev() {
                let seq = &sequences[i];
                let ll_code = ll_codes[i];
                let of_code = of_codes[i];
                let ml_code = ml_codes[i];
                ensure_bits_capacity(&mut stream, fse::ctable_log(offsets_table))?;
                fse::encode_symbol(&mut stream, &mut state_offsets, offsets_table, of_code)?;
                ensure_bits_capacity(&mut stream, fse::ctable_log(match_lengths_table))?;
                fse::encode_symbol(
                    &mut stream,
                    &mut state_match_lengths,
                    match_lengths_table,
                    ml_code,
                )?;
                ensure_bits_capacity(&mut stream, fse::ctable_log(literal_lengths_table))?;
                fse::encode_symbol(
                    &mut stream,
                    &mut state_literal_lengths,
                    literal_lengths_table,
                    ll_code,
                )?;

                let ll_extra = seq.literal_length - LITERAL_LENGTH_BASELINES[ll_code as usize];
                let of_extra = seq.offset_value - (1u32 << of_code);
                let ml_extra = seq.match_length - MATCH_LENGTH_BASELINES[ml_code as usize];
                add_bits_checked(&mut stream, ll_extra.into(), ll_bits(ll_code))?;
                add_bits_checked(&mut stream, ml_extra.into(), ml_bits(ml_code))?;
                add_offset_bits_checked(
                    &mut stream,
                    of_extra.into(),
                    u32::from(of_code),
                    long_offsets,
                )?;
            }
        }

        ensure_bits_capacity(&mut stream, fse::ctable_log(match_lengths_table))?;
        fse::flush_cstate(&mut stream, &state_match_lengths, match_lengths_table)?;
        ensure_bits_capacity(&mut stream, fse::ctable_log(offsets_table))?;
        fse::flush_cstate(&mut stream, &state_offsets, offsets_table)?;
        ensure_bits_capacity(&mut stream, fse::ctable_log(literal_lengths_table))?;
        fse::flush_cstate(&mut stream, &state_literal_lengths, literal_lengths_table)?;

        stream.close()
    };

    if written == 0 {
        return Err(Error::DstSizeTooSmall);
    }
    dst.truncate(written);
    Ok(())
}

fn encoded_sequence_count_size(number_of_sequences: usize) -> usize {
    if number_of_sequences < 128 {
        1
    } else if number_of_sequences < 0x7F00 {
        2
    } else {
        3
    }
}

fn predefined_sequence_ctables() -> &'static PredefinedSequenceCTables {
    PREDEFINED_SEQUENCE_CTABLES.get_or_init(|| {
        let mut literal_lengths = fse::CTable::default();
        fse::build_ctable(
            &mut literal_lengths,
            &LITERAL_LENGTH_DEFAULT_DISTRIBUTION,
            (LITERAL_LENGTH_DEFAULT_DISTRIBUTION.len() - 1) as u32,
            LL_DEFAULT_LOG,
        )
        .expect("literal-length predefined FSE table is valid");

        let mut offsets = fse::CTable::default();
        fse::build_ctable(
            &mut offsets,
            &OFFSET_DEFAULT_DISTRIBUTION,
            (OFFSET_DEFAULT_DISTRIBUTION.len() - 1) as u32,
            OF_DEFAULT_LOG,
        )
        .expect("offset predefined FSE table is valid");

        let mut match_lengths = fse::CTable::default();
        fse::build_ctable(
            &mut match_lengths,
            &MATCH_LENGTH_DEFAULT_DISTRIBUTION,
            (MATCH_LENGTH_DEFAULT_DISTRIBUTION.len() - 1) as u32,
            ML_DEFAULT_LOG,
        )
        .expect("match-length predefined FSE table is valid");

        PredefinedSequenceCTables {
            literal_lengths: SequenceEncodingTable::new(literal_lengths, all_symbols_supported()),
            offsets: SequenceEncodingTable::new(offsets, all_symbols_supported()),
            match_lengths: SequenceEncodingTable::new(match_lengths, all_symbols_supported()),
        }
    })
}

pub(crate) fn ll_bits(code: u8) -> u32 {
    u32::from(LITERAL_LENGTH_EXTRA_BITS[code as usize])
}

pub(crate) fn ml_bits(code: u8) -> u32 {
    u32::from(MATCH_LENGTH_EXTRA_BITS[code as usize])
}

fn ensure_bits_capacity(bit_c: &mut BitCStream<'_>, nb_bits: u32) -> Result<()> {
    if nb_bits >= CONTAINER_BITS {
        return Err(Error::InvalidParameter(
            "bitstream write exceeds the accumulator width",
        ));
    }
    if bit_c.bit_pos + nb_bits >= STREAM_ACCUMULATOR_MIN {
        bit_c.flush_bits();
    }
    Ok(())
}

fn add_bits_checked(bit_c: &mut BitCStream<'_>, value: u64, nb_bits: u32) -> Result<()> {
    if nb_bits == 0 {
        return Ok(());
    }
    ensure_bits_capacity(bit_c, nb_bits)?;
    bit_c.add_bits(value, nb_bits);
    Ok(())
}

fn add_offset_bits_checked(
    bit_c: &mut BitCStream<'_>,
    value: u64,
    nb_bits: u32,
    long_offsets: bool,
) -> Result<()> {
    if !long_offsets || nb_bits < STREAM_ACCUMULATOR_MIN {
        return add_bits_checked(bit_c, value, nb_bits);
    }

    let extra_bits = nb_bits - (STREAM_ACCUMULATOR_MIN - 1);
    if extra_bits != 0 {
        add_bits_checked(bit_c, value, extra_bits)?;
        bit_c.flush_bits();
    }
    add_bits_checked(bit_c, value >> extra_bits, nb_bits - extra_bits)
}

// Cold error constructors for decode hot paths. Marking these `#[cold]`
// and `#[inline(never)]` tells LLVM to lay out the error branches out-of-line,
// improving instruction cache behavior and branch prediction on the hot path.

#[cold]
#[inline(never)]
fn corruption_error(msg: &'static str) -> Error {
    Error::Corruption(msg)
}

#[cold]
#[inline(never)]
fn output_size_overflow_error() -> Error {
    Error::OutputSizeOverflow
}

#[cold]
#[inline(never)]
fn output_size_too_large_error(output_size: u64, max_output_size: usize) -> Error {
    Error::OutputSizeTooLarge {
        output_size,
        max_output_size,
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        block::{BlockHeader, BlockType},
        decode_all,
        entropy::bitstream::BitCStream,
        frame::write_single_segment_header,
        window::plan_sequences,
    };

    use super::*;

    #[test]
    fn parses_zero_sequence_sections_without_touching_table_state() {
        let mut tables = SequenceTablesState::default();
        let parsed = parse_sequence_section(&[0], &mut tables, TableTarget::Both).unwrap();

        assert_eq!(parsed.number_of_sequences, 0);
        assert_eq!(parsed.header_size, 1);
        assert!(parsed.bitstream.is_empty());
        assert!(parsed.modes.is_none());
    }

    #[test]
    fn rejects_reserved_mode_bits() {
        let mut tables = SequenceTablesState::default();
        let err =
            parse_sequence_section(&[1, 0x01, 1], &mut tables, TableTarget::Both).unwrap_err();
        assert_eq!(
            err,
            Error::Corruption("sequence compression modes reserved bits are set")
        );
    }

    #[test]
    fn rejects_repeat_mode_without_previous_tables() {
        let mut tables = SequenceTablesState::default();
        let err = parse_sequence_section(&[1, 0b1111_1100, 1], &mut tables, TableTarget::Both)
            .unwrap_err();
        assert_eq!(
            err,
            Error::Corruption("sequence repeat mode requires a previous FSE table")
        );
    }

    #[test]
    fn parses_predefined_sequence_tables_and_bitstream() {
        let mut tables = SequenceTablesState::default();
        let parsed = parse_sequence_section(
            &[1, 0b0000_0000, 0, 0, 0, 0, 1],
            &mut tables,
            TableTarget::Both,
        )
        .unwrap();

        assert_eq!(parsed.number_of_sequences, 1);
        assert_eq!(parsed.header_size, 2);
        assert_eq!(parsed.bitstream, &[0, 0, 0, 0, 1]);
        assert_eq!(
            parsed.modes,
            Some(SequenceCompressionModes {
                literal_lengths: CompressionMode::Predefined,
                offsets: CompressionMode::Predefined,
                match_lengths: CompressionMode::Predefined,
            })
        );
    }

    #[test]
    fn repeat_mode_reuses_the_previous_tables() {
        let mut tables = SequenceTablesState::default();
        parse_sequence_section(
            &[1, 0b0000_0000, 0, 0, 0, 0, 1],
            &mut tables,
            TableTarget::Both,
        )
        .unwrap();
        let parsed = parse_sequence_section(
            &[1, 0b1111_1100, 0, 0, 0, 0, 1],
            &mut tables,
            TableTarget::Both,
        )
        .unwrap();

        assert_eq!(parsed.number_of_sequences, 1);
        assert_eq!(parsed.header_size, 2);
    }

    #[test]
    fn parses_rle_modes() {
        let mut tables = SequenceTablesState::default();
        let parsed = parse_sequence_section(
            &[1, 0b0101_0100, 7, 0, 4, 1],
            &mut tables,
            TableTarget::Both,
        )
        .unwrap();

        assert_eq!(parsed.number_of_sequences, 1);
        assert_eq!(parsed.header_size, 5);
        assert_eq!(parsed.bitstream, &[1]);
    }

    #[test]
    fn decodes_single_rle_sequence_command() {
        let mut tables = SequenceTablesState::default();
        let parsed = parse_sequence_section(
            &[1, 0b0101_0100, 7, 0, 4, 1],
            &mut tables,
            TableTarget::Both,
        )
        .unwrap();
        let sequences = decode_sequence_commands(&parsed, &tables).unwrap();

        assert_eq!(
            sequences,
            vec![SequenceCommand {
                literal_length: 7,
                offset_value: 1,
                match_length: 7,
            }]
        );
    }

    #[test]
    fn decodes_two_rle_sequences() {
        let mut tables = SequenceTablesState::default();
        let parsed = parse_sequence_section(
            &[2, 0b0101_0100, 7, 0, 4, 1],
            &mut tables,
            TableTarget::Both,
        )
        .unwrap();
        let sequences = decode_sequence_commands(&parsed, &tables).unwrap();

        assert_eq!(sequences.len(), 2);
        assert_eq!(sequences[0].literal_length, 7);
        assert_eq!(sequences[1].match_length, 7);
    }

    #[test]
    fn decodes_predefined_initial_states() {
        let mut tables = SequenceTablesState::default();
        let bitstream = encode_reverse_bits(&[(0, 6), (0, 5), (0, 6)]);
        let mut section = vec![1, 0b0000_0000];
        section.extend_from_slice(&bitstream);
        let parsed = parse_sequence_section(&section, &mut tables, TableTarget::Both).unwrap();
        let sequences = decode_sequence_commands(&parsed, &tables).unwrap();

        assert_eq!(sequences.len(), 1);
    }

    #[test]
    fn rejects_leftover_bits_after_sequence_decode() {
        let mut tables = SequenceTablesState::default();
        let parsed = parse_sequence_section(
            &[1, 0b0101_0100, 7, 0, 4, 0b0000_0011],
            &mut tables,
            TableTarget::Both,
        )
        .unwrap();
        let err = decode_sequence_commands(&parsed, &tables).unwrap_err();

        assert_eq!(
            err,
            Error::Corruption("sequence bitstream was not fully consumed")
        );
    }

    #[test]
    fn repeat_offsets_follow_the_spec_update_rules() {
        let mut repeat_offsets = RepeatOffsets::default();
        let cases = [
            (
                SequenceCommand {
                    literal_length: 11,
                    offset_value: 1114,
                    match_length: 3,
                },
                1111,
                [1111, 1, 4],
            ),
            (
                SequenceCommand {
                    literal_length: 22,
                    offset_value: 1,
                    match_length: 3,
                },
                1111,
                [1111, 1, 4],
            ),
            (
                SequenceCommand {
                    literal_length: 22,
                    offset_value: 2225,
                    match_length: 3,
                },
                2222,
                [2222, 1111, 1],
            ),
            (
                SequenceCommand {
                    literal_length: 111,
                    offset_value: 1114,
                    match_length: 3,
                },
                1111,
                [1111, 2222, 1111],
            ),
            (
                SequenceCommand {
                    literal_length: 33,
                    offset_value: 3336,
                    match_length: 3,
                },
                3333,
                [3333, 1111, 2222],
            ),
            (
                SequenceCommand {
                    literal_length: 22,
                    offset_value: 2,
                    match_length: 3,
                },
                1111,
                [1111, 3333, 2222],
            ),
            (
                SequenceCommand {
                    literal_length: 33,
                    offset_value: 3,
                    match_length: 3,
                },
                2222,
                [2222, 1111, 3333],
            ),
            (
                SequenceCommand {
                    literal_length: 0,
                    offset_value: 3,
                    match_length: 3,
                },
                2221,
                [2221, 2222, 1111],
            ),
            (
                SequenceCommand {
                    literal_length: 0,
                    offset_value: 1,
                    match_length: 3,
                },
                2222,
                [2222, 2221, 1111],
            ),
        ];

        for (sequence, expected_offset, expected_state) in cases {
            let actual_offset = repeat_offsets.resolve(&sequence).unwrap();
            assert_eq!(actual_offset, expected_offset);
            assert_eq!(repeat_offsets.values(), expected_state);
        }
    }

    #[test]
    fn encodes_offset_values_following_the_spec() {
        let mut repeat_offsets = RepeatOffsets::from_values([1111, 2222, 3333]);
        let cases = [
            (1111, 12, 1, [1111, 2222, 3333]),
            (2222, 0, 1, [2222, 1111, 3333]),
            (2221, 0, 3, [2221, 2222, 1111]),
            (3333, 5, 3336, [3333, 2221, 2222]),
            (4444, 7, 4447, [4444, 3333, 2221]),
        ];

        for (raw_offset, literal_length, expected, expected_state) in cases {
            let actual = repeat_offsets
                .encode_offset_value(raw_offset, literal_length)
                .unwrap();
            assert_eq!(actual, expected);
            assert_eq!(repeat_offsets.values(), expected_state);
        }
    }

    #[test]
    fn rejects_repeat_offset_one_minus_one_when_it_reaches_zero() {
        let mut repeat_offsets = RepeatOffsets::default();
        let err = repeat_offsets
            .resolve(&SequenceCommand {
                literal_length: 0,
                offset_value: 3,
                match_length: 3,
            })
            .unwrap_err();

        assert_eq!(err, Error::Corruption("repeat offset 1 minus 1 is zero"));
    }

    #[test]
    fn resolves_zero_literal_rep2_like_generic_offset_value_one() {
        let mut generic = RepeatOffsets::from_values([1111, 2222, 3333]);
        let raw_offset = generic
            .resolve(&SequenceCommand {
                literal_length: 0,
                offset_value: 1,
                match_length: 3,
            })
            .unwrap();
        assert_eq!(raw_offset, 2222);
        assert_eq!(generic.values(), [2222, 1111, 3333]);

        let mut specialized = RepeatOffsets::from_values([1111, 2222, 3333]);
        let specialized_raw = specialized.resolve_zero_literal_rep2().unwrap();
        assert_eq!(specialized_raw, raw_offset);
        assert_eq!(specialized.values(), generic.values());
    }

    #[test]
    fn executes_sequences_with_overlap_and_last_literals() {
        let mut repeat_offsets = RepeatOffsets::default();
        let mut out = Vec::new();
        let sequences = [
            SequenceCommand {
                literal_length: 1,
                offset_value: 4,
                match_length: 5,
            },
            SequenceCommand {
                literal_length: 2,
                offset_value: 1,
                match_length: 4,
            },
        ];

        execute_sequences(
            &mut DecodeOut::growable(&mut out),
            0,
            128 * 1024,
            128 * 1024,
            None,
            b"abc",
            &sequences,
            &mut repeat_offsets,
            None,
        )
        .unwrap();

        assert_eq!(out, b"aaaaaabccccc");
        assert_eq!(repeat_offsets.values(), [1, 1, 4]);
    }

    #[test]
    fn rejects_sequence_offsets_outside_the_available_window() {
        let mut repeat_offsets = RepeatOffsets::default();
        let mut out = b"abcdef".to_vec();
        let err = execute_sequences(
            &mut DecodeOut::growable(&mut out),
            0,
            4,
            128 * 1024,
            None,
            b"",
            &[SequenceCommand {
                literal_length: 0,
                offset_value: 10,
                match_length: 3,
            }],
            &mut repeat_offsets,
            None,
        )
        .unwrap_err();

        assert_eq!(
            err,
            Error::Corruption("sequence offset exceeds the available history window")
        );
    }

    #[test]
    fn executes_dictionary_matches_across_the_prefix_boundary() {
        let mut repeat_offsets = RepeatOffsets::default();
        let mut out = b"abc".to_vec();

        execute_sequences(
            &mut DecodeOut::growable(&mut out),
            0,
            128 * 1024,
            128 * 1024,
            Some(b"XY"),
            b"",
            &[SequenceCommand {
                literal_length: 0,
                offset_value: 8,
                match_length: 9,
            }],
            &mut repeat_offsets,
            None,
        )
        .unwrap();

        assert_eq!(out, b"abcXYabcXYab");
    }

    #[test]
    fn executes_dictionary_only_overlap_matches() {
        let mut repeat_offsets = RepeatOffsets::default();
        let mut out = Vec::new();

        execute_sequences(
            &mut DecodeOut::growable(&mut out),
            0,
            128 * 1024,
            128 * 1024,
            Some(b"wxyz"),
            b"",
            &[SequenceCommand {
                literal_length: 0,
                offset_value: 5,
                match_length: 6,
            }],
            &mut repeat_offsets,
            None,
        )
        .unwrap();

        assert_eq!(out, b"yzyzyz");
    }

    #[test]
    fn encodes_predefined_sequence_sections_roundtrip() {
        let sequences = vec![
            SequenceCommand {
                literal_length: 3,
                offset_value: 7,
                match_length: 11,
            },
            SequenceCommand {
                literal_length: 0,
                offset_value: 1,
                match_length: 7,
            },
            SequenceCommand {
                literal_length: 5,
                offset_value: 10,
                match_length: 4,
            },
        ];

        let encoded = encode_sequence_section(&sequences).unwrap();
        let mut tables = SequenceTablesState::default();
        let parsed = parse_sequence_section(&encoded, &mut tables, TableTarget::Both).unwrap();
        let decoded = decode_sequence_commands(&parsed, &tables).unwrap();

        assert_eq!(decoded, sequences);
    }

    #[test]
    fn encodes_planned_sequence_sections_roundtrip() {
        let input = build_repeated_chunk_pattern(24_000);
        let plan = plan_sequences(&input, RepeatOffsets::default()).unwrap();
        let encoded = encode_sequence_section(&plan.sequences).unwrap();
        let mut tables = SequenceTablesState::default();
        let parsed = parse_sequence_section(&encoded, &mut tables, TableTarget::Both).unwrap();
        let decoded = decode_sequence_commands(&parsed, &tables).unwrap();

        assert_eq!(decoded, plan.sequences);
    }

    #[test]
    fn encodes_pseudorandom_planned_sequence_sections_roundtrip() {
        let input = build_pattern(32_000);
        let plan = plan_sequences(&input, RepeatOffsets::default()).unwrap();
        let encoded = encode_sequence_section(&plan.sequences).unwrap();
        let mut tables = SequenceTablesState::default();
        let parsed = parse_sequence_section(&encoded, &mut tables, TableTarget::Both).unwrap();
        let decoded = decode_sequence_commands(&parsed, &tables).unwrap();

        assert_eq!(decoded, plan.sequences);
    }

    #[test]
    fn encodes_large_sequence_counts_roundtrip() {
        let mut encoded = Vec::new();
        encode_sequence_count(500, &mut OutBuf::growable(&mut encoded)).unwrap();
        assert_eq!(parse_sequence_count(&encoded).unwrap(), (500, 2));
    }

    #[test]
    fn encodes_custom_fse_tables_for_skewed_sequences() {
        let sequences = build_skewed_sequence_commands(2048);
        let (encoded, next_state) =
            encode_sequence_section_with_state(&sequences, &SequenceEncodingState::default())
                .unwrap();

        let mut tables = SequenceTablesState::default();
        let parsed = parse_sequence_section(&encoded, &mut tables, TableTarget::Both).unwrap();
        let decoded = decode_sequence_commands(&parsed, &tables).unwrap();

        assert_eq!(decoded, sequences);
        let modes = parsed.modes.unwrap();
        assert!(
            [modes.literal_lengths, modes.offsets, modes.match_lengths]
                .into_iter()
                .any(|mode| mode == CompressionMode::FseCompressed),
            "expected at least one custom FSE table mode, got {modes:?}"
        );
        assert!(next_state.table(SequencePart::LiteralLength).is_some());
        assert!(next_state.table(SequencePart::Offset).is_some());
        assert!(next_state.table(SequencePart::MatchLength).is_some());
    }

    #[test]
    fn fast_strategy_prefers_predefined_tables_for_small_sequence_sections() {
        let sequences = build_skewed_sequence_commands(32);
        let (encoded, _) = encode_sequence_section_with_strategy(
            &sequences,
            &SequenceEncodingState::default(),
            ParserStrategy::Fast,
        )
        .unwrap();

        let mut tables = SequenceTablesState::default();
        let parsed = parse_sequence_section(&encoded, &mut tables, TableTarget::Both).unwrap();
        let decoded = decode_sequence_commands(&parsed, &tables).unwrap();

        assert_eq!(decoded, sequences);
        assert_eq!(
            parsed.modes.unwrap(),
            SequenceCompressionModes {
                literal_lengths: CompressionMode::Predefined,
                offsets: CompressionMode::Predefined,
                match_lengths: CompressionMode::Predefined,
            }
        );
    }

    #[test]
    fn fast_strategy_does_not_auto_repeat_tables_in_check_mode() {
        let seed_sequences = build_skewed_sequence_commands(2048);
        let (_, state_after_first) = encode_sequence_section_with_strategy(
            &seed_sequences,
            &SequenceEncodingState::default(),
            ParserStrategy::Fast,
        )
        .unwrap();
        assert!(
            state_after_first
                .table(SequencePart::LiteralLength)
                .is_some()
                || state_after_first.table(SequencePart::Offset).is_some()
                || state_after_first.table(SequencePart::MatchLength).is_some()
        );

        let sequences = build_skewed_sequence_commands(256);
        let (encoded, _) = encode_sequence_section_with_strategy(
            &sequences,
            &state_after_first,
            ParserStrategy::Fast,
        )
        .unwrap();

        let mut tables = SequenceTablesState::default();
        let seed_encoded = encode_sequence_section_with_strategy(
            &seed_sequences,
            &SequenceEncodingState::default(),
            ParserStrategy::Fast,
        )
        .unwrap()
        .0;
        parse_sequence_section(&seed_encoded, &mut tables, TableTarget::Both).unwrap();

        let parsed = parse_sequence_section(&encoded, &mut tables, TableTarget::Both).unwrap();
        let decoded = decode_sequence_commands(&parsed, &tables).unwrap();

        assert_eq!(decoded, sequences);
        let modes = parsed.modes.unwrap();
        assert!(
            [modes.literal_lengths, modes.offsets, modes.match_lengths]
                .into_iter()
                .all(|mode| mode != CompressionMode::Repeat),
            "expected fast strategy to avoid repeat mode when prior tables are only in check mode, got {modes:?}"
        );
    }

    #[test]
    fn fast_strategy_reuses_valid_tables_for_small_repeated_sections() {
        let seed_sequences = build_skewed_sequence_commands(2048);
        let (seed_encoded, mut state_after_first) = encode_sequence_section_with_strategy(
            &seed_sequences,
            &SequenceEncodingState::default(),
            ParserStrategy::Fast,
        )
        .unwrap();
        for entry in [
            state_after_first.literal_lengths.as_mut(),
            state_after_first.offsets.as_mut(),
            state_after_first.match_lengths.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            entry.repeat_mode = SequenceRepeatMode::Valid;
        }

        let sequences = build_skewed_sequence_commands(256);
        let (encoded, _) = encode_sequence_section_with_strategy(
            &sequences,
            &state_after_first,
            ParserStrategy::Fast,
        )
        .unwrap();

        let mut tables = SequenceTablesState::default();
        parse_sequence_section(&seed_encoded, &mut tables, TableTarget::Both).unwrap();

        let parsed = parse_sequence_section(&encoded, &mut tables, TableTarget::Both).unwrap();
        let decoded = decode_sequence_commands(&parsed, &tables).unwrap();

        assert_eq!(decoded, sequences);
        let modes = parsed.modes.unwrap();
        assert!(
            [modes.literal_lengths, modes.offsets, modes.match_lengths]
                .into_iter()
                .any(|mode| mode == CompressionMode::Repeat),
            "expected fast strategy to reuse a valid previous table, got {modes:?}"
        );
    }

    #[test]
    fn repeat_cost_rejects_tables_with_zero_probability_symbols() {
        let previous_codes = [0u8, 1, 0, 1, 0, 1, 0, 1];
        let previous_stats = analyze_codes(SequencePart::LiteralLength, &previous_codes).unwrap();
        let mut pool = SequenceTablePool::default();
        let previous_choice = build_compressed_table_choice(
            SequencePart::LiteralLength,
            &previous_codes,
            &previous_stats,
            &mut pool,
        )
        .unwrap()
        .unwrap();
        let previous =
            SequenceEncodingPartState::new(previous_choice.encoding, SequenceRepeatMode::Check);

        let codes = [0u8, 1, 2, 0, 1, 2, 0, 1];
        let choice = build_sequence_part_choice(
            SequencePart::LiteralLength,
            &codes,
            Some(&previous),
            ParserStrategy::Lazy,
            &mut pool,
        )
        .unwrap();

        assert_ne!(choice.mode, CompressionMode::Repeat);
    }

    #[test]
    fn build_sequences_statistics_tracks_header_order_and_last_count_size() {
        let sequences = build_skewed_sequence_commands(2048);
        let mut scratch = SequenceEncodeScratch::default();
        prepare_sequence_encode_scratch(&sequences, &mut scratch).unwrap();

        let build = build_sequences_statistics(
            scratch.encoded.len(),
            &scratch.literal_codes,
            &scratch.offset_codes,
            &scratch.match_codes,
            &SequenceEncodingState::default(),
            ParserStrategy::Lazy,
            &mut SequenceTablePool::default(),
        )
        .unwrap();

        let expected_last_count_size = if build.match_choice.mode == CompressionMode::FseCompressed
        {
            build.match_choice.header.len()
        } else if build.offset_choice.mode == CompressionMode::FseCompressed {
            build.offset_choice.header.len()
        } else if build.literal_choice.mode == CompressionMode::FseCompressed {
            build.literal_choice.header.len()
        } else {
            0
        };

        assert_eq!(
            build.stats.header_size,
            encoded_sequence_count_size(scratch.encoded.len())
                + 1
                + build.literal_choice.header.len()
                + build.offset_choice.header.len()
                + build.match_choice.header.len()
        );
        assert_eq!(build.stats.last_count_size, expected_last_count_size);
        assert_eq!(
            build.stats.modes,
            Some(SequenceCompressionModes {
                literal_lengths: build.literal_choice.mode,
                offsets: build.offset_choice.mode,
                match_lengths: build.match_choice.mode,
            })
        );
    }

    #[test]
    fn build_sequences_statistics_reuses_seeded_valid_tables_on_the_first_block() {
        let seed_sequences = build_skewed_sequence_commands(2048);
        let mut seed_scratch = SequenceEncodeScratch::default();
        prepare_sequence_encode_scratch(&seed_sequences, &mut seed_scratch).unwrap();
        let mut seeded_state = build_sequences_statistics(
            seed_scratch.encoded.len(),
            &seed_scratch.literal_codes,
            &seed_scratch.offset_codes,
            &seed_scratch.match_codes,
            &SequenceEncodingState::default(),
            ParserStrategy::Fast,
            &mut SequenceTablePool::default(),
        )
        .unwrap()
        .into_next_state()
        .0;
        for entry in [
            seeded_state.literal_lengths.as_mut(),
            seeded_state.offsets.as_mut(),
            seeded_state.match_lengths.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            entry.repeat_mode = SequenceRepeatMode::Valid;
        }

        let sequences = build_skewed_sequence_commands(256);
        let mut scratch = SequenceEncodeScratch::default();
        prepare_sequence_encode_scratch(&sequences, &mut scratch).unwrap();
        let build = build_sequences_statistics(
            scratch.encoded.len(),
            &scratch.literal_codes,
            &scratch.offset_codes,
            &scratch.match_codes,
            &seeded_state,
            ParserStrategy::Fast,
            &mut SequenceTablePool::default(),
        )
        .unwrap();

        let modes = build.stats.modes.unwrap();
        assert!(
            [modes.literal_lengths, modes.offsets, modes.match_lengths]
                .into_iter()
                .any(|mode| mode == CompressionMode::Repeat),
            "expected seeded valid tables to trigger repeat-mode reuse, got {modes:?}"
        );
    }

    #[test]
    fn reusable_sequence_workspace_matches_one_shot_encoding() {
        let first_sequences = build_skewed_sequence_commands(2048);
        let second_sequences = build_skewed_sequence_commands(257);
        let mut bitstream_scratch = Vec::new();
        let mut encode_scratch = SequenceEncodeScratch::default();

        let (expected_first, expected_first_state) = encode_sequence_section_with_strategy(
            &first_sequences,
            &SequenceEncodingState::default(),
            ParserStrategy::DoubleFast,
        )
        .unwrap();
        let (actual_first, actual_first_state) = encode_sequence_section_with_strategy_and_scratch(
            &first_sequences,
            &SequenceEncodingState::default(),
            ParserStrategy::DoubleFast,
            &mut bitstream_scratch,
            &mut encode_scratch,
        )
        .unwrap();
        assert_eq!(actual_first, expected_first);
        assert_eq!(
            actual_first_state
                .table(SequencePart::LiteralLength)
                .is_some(),
            expected_first_state
                .table(SequencePart::LiteralLength)
                .is_some()
        );
        assert_eq!(
            actual_first_state.table(SequencePart::Offset).is_some(),
            expected_first_state.table(SequencePart::Offset).is_some()
        );
        assert_eq!(
            actual_first_state
                .table(SequencePart::MatchLength)
                .is_some(),
            expected_first_state
                .table(SequencePart::MatchLength)
                .is_some()
        );

        let (expected_second, expected_second_state) = encode_sequence_section_with_strategy(
            &second_sequences,
            &expected_first_state,
            ParserStrategy::DoubleFast,
        )
        .unwrap();
        let (actual_second, actual_second_state) =
            encode_sequence_section_with_strategy_and_scratch(
                &second_sequences,
                &actual_first_state,
                ParserStrategy::DoubleFast,
                &mut bitstream_scratch,
                &mut encode_scratch,
            )
            .unwrap();
        assert_eq!(actual_second, expected_second);
        assert_eq!(
            actual_second_state
                .table(SequencePart::LiteralLength)
                .is_some(),
            expected_second_state
                .table(SequencePart::LiteralLength)
                .is_some()
        );
        assert_eq!(
            actual_second_state.table(SequencePart::Offset).is_some(),
            expected_second_state.table(SequencePart::Offset).is_some()
        );
        assert_eq!(
            actual_second_state
                .table(SequencePart::MatchLength)
                .is_some(),
            expected_second_state
                .table(SequencePart::MatchLength)
                .is_some()
        );
    }

    #[test]
    fn repeated_dynamic_distributions_keep_using_dynamic_sequence_tables() {
        let sequences = build_skewed_sequence_commands(2048);
        let (_, state_after_first) =
            encode_sequence_section_with_state(&sequences, &SequenceEncodingState::default())
                .unwrap();
        let (encoded, _) =
            encode_sequence_section_with_state(&sequences, &state_after_first).unwrap();

        let mut tables = SequenceTablesState::default();
        let first_encoded =
            encode_sequence_section_with_state(&sequences, &SequenceEncodingState::default())
                .unwrap()
                .0;
        parse_sequence_section(&first_encoded, &mut tables, TableTarget::Both).unwrap();

        let parsed = parse_sequence_section(&encoded, &mut tables, TableTarget::Both).unwrap();
        let decoded = decode_sequence_commands(&parsed, &tables).unwrap();

        assert_eq!(decoded, sequences);
        let modes = parsed.modes.unwrap();
        assert!(
            [modes.literal_lengths, modes.offsets, modes.match_lengths]
                .into_iter()
                .any(|mode| matches!(
                    mode,
                    CompressionMode::Repeat | CompressionMode::FseCompressed
                )),
            "expected the repeated distribution to keep at least one dynamic FSE mode, got {modes:?}"
        );
    }

    #[test]
    fn decodes_frames_with_repeated_sequence_tables_across_blocks() {
        let (literals, sequences, block_output) = build_repeat_friendly_block(256);
        let (first_section, state_after_first) =
            encode_sequence_section_with_state(&sequences, &SequenceEncodingState::default())
                .unwrap();
        let (second_section, _) =
            encode_sequence_section_with_state(&sequences, &state_after_first).unwrap();

        let mut tables = SequenceTablesState::default();
        let first_modes = parse_sequence_section(&first_section, &mut tables, TableTarget::Both)
            .unwrap()
            .modes
            .unwrap();
        let second_modes = parse_sequence_section(&second_section, &mut tables, TableTarget::Both)
            .unwrap()
            .modes
            .unwrap();
        assert!(
            [
                first_modes.literal_lengths,
                first_modes.offsets,
                first_modes.match_lengths
            ]
            .into_iter()
            .any(|mode| { matches!(mode, CompressionMode::Rle | CompressionMode::FseCompressed) }),
            "expected the first block to seed a reusable table, got {first_modes:?}"
        );
        assert!(
            [
                second_modes.literal_lengths,
                second_modes.offsets,
                second_modes.match_lengths
            ]
            .into_iter()
            .any(|mode| mode == CompressionMode::Repeat),
            "expected the second block to reuse a previous table, got {second_modes:?}"
        );

        let literals_section = raw_literals_section(&literals);
        let mut first_payload = literals_section.clone();
        first_payload.extend_from_slice(&first_section);
        let mut second_payload = literals_section;
        second_payload.extend_from_slice(&second_section);

        let mut frame = Vec::new();
        write_single_segment_header(
            &mut OutBuf::growable(&mut frame),
            (block_output.len() * 2) as u64,
            false,
        );
        BlockHeader {
            last_block: false,
            block_type: BlockType::Compressed,
            block_size: first_payload.len() as u32,
        }
        .write_to(&mut OutBuf::growable(&mut frame));
        frame.extend_from_slice(&first_payload);
        BlockHeader {
            last_block: true,
            block_type: BlockType::Compressed,
            block_size: second_payload.len() as u32,
        }
        .write_to(&mut OutBuf::growable(&mut frame));
        frame.extend_from_slice(&second_payload);

        let decoded = decode_all(&frame).unwrap();
        let mut expected = block_output.clone();
        expected.extend_from_slice(&block_output);
        assert_eq!(decoded, expected);
    }

    /// A body whose repeats sit far enough apart that matching them needs wide
    /// offset codes.
    ///
    /// Three copies of one pseudo-random block separated by long runs of a
    /// single byte. The runs cost almost nothing to encode and push each copy
    /// hundreds of kilobytes from the last, which is the only way to make the
    /// encoder reach for an offset code near the top of the range.
    fn distant_repeat_body() -> Vec<u8> {
        const BLOCK: usize = 4096;
        let block: Vec<u8> = (0..BLOCK)
            .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();

        let mut body = Vec::with_capacity(BLOCK * 3 + 500_000);
        body.extend_from_slice(&block);
        body.extend(core::iter::repeat_n(0u8, 300_000));
        body.extend_from_slice(&block);
        body.extend(core::iter::repeat_n(7u8, 200_000));
        body.extend_from_slice(&block);
        body
    }

    /// One sequence can spend 63 bits of the accumulator between reloads: an
    /// offset code of up to 31 bits, then 16 of match-length extra, then 16 of
    /// literal-length extra. While the accumulator was the machine word, a
    /// 32-bit target overflowed on the reload and rejected the frame.
    #[test]
    fn a_frame_with_distant_repeats_round_trips() {
        let body = distant_repeat_body();
        let frame = crate::encode_all(&body).unwrap();

        // The guard that keeps this test covering wide offsets: the body holds
        // 4 KiB of unique content, so a frame this small can only have come
        // from matching the two distant copies rather than re-encoding them.
        assert!(
            frame.len() < 8 * 1024,
            "expected the distant copies to be matched, got a {}-byte frame",
            frame.len()
        );

        assert_eq!(decode_all(&frame).unwrap(), body);
    }

    fn encode_reverse_bits(values: &[(u64, u32)]) -> Vec<u8> {
        let mut bytes = vec![0u8; 64];
        let written = {
            let mut stream = BitCStream::new(&mut bytes).unwrap();
            for &(value, nb_bits) in values {
                if stream.bit_pos + nb_bits >= STREAM_ACCUMULATOR_MIN {
                    stream.flush_bits();
                }
                stream.add_bits(value, nb_bits);
            }
            stream.close()
        };
        bytes.truncate(written);
        bytes
    }

    fn build_repeated_chunk_pattern(size: usize) -> Vec<u8> {
        const CHUNK: &[u8] = b"zstd-rs-window-repcode-pattern-0123456789ABCDEF";

        let mut out = Vec::with_capacity(size);
        while out.len() < size {
            let remaining = size - out.len();
            let take = remaining.min(CHUNK.len());
            out.extend_from_slice(&CHUNK[..take]);
        }
        out
    }

    fn build_pattern(size: usize) -> Vec<u8> {
        (0..size)
            .map(|index| ((index * 31) as u8).wrapping_add((index >> 7) as u8))
            .collect()
    }

    fn build_skewed_sequence_commands(count: usize) -> Vec<SequenceCommand> {
        (0..count)
            .map(|index| SequenceCommand {
                literal_length: if index % 31 == 0 {
                    96
                } else if index % 7 == 0 {
                    12
                } else {
                    0
                },
                offset_value: if index % 53 == 0 {
                    1024
                } else if index % 9 == 0 {
                    17
                } else {
                    1
                },
                match_length: if index % 41 == 0 {
                    35
                } else if index % 5 == 0 {
                    7
                } else {
                    3
                },
            })
            .collect()
    }

    fn build_repeat_friendly_block(count: usize) -> (Vec<u8>, Vec<SequenceCommand>, Vec<u8>) {
        let mut literals = Vec::new();
        let mut sequences = Vec::new();
        let mut repeat_offsets = RepeatOffsets::default();
        let mut prng = 0x1234_5678u32;

        for index in 0..count {
            let literal_length = if index == 0 {
                1024
            } else if index % 31 == 0 {
                96
            } else if index % 7 == 0 {
                12
            } else {
                1
            };
            let match_length = if index % 41 == 0 {
                35
            } else if index % 5 == 0 {
                7
            } else {
                3
            };
            let raw_offset = if index == 0 || index % 53 == 0 {
                1024
            } else if index % 9 == 0 {
                17
            } else {
                1024
            };

            for _ in 0..literal_length {
                prng ^= prng << 13;
                prng ^= prng >> 17;
                prng ^= prng << 5;
                literals.push(b'A' + (prng & 0x0f) as u8);
            }

            let offset_value = repeat_offsets
                .encode_offset_value(raw_offset, literal_length as u32)
                .unwrap();
            sequences.push(SequenceCommand {
                literal_length: literal_length as u32,
                offset_value,
                match_length: match_length as u32,
            });
        }

        let mut output = Vec::new();
        let mut decode_repeat_offsets = RepeatOffsets::default();
        execute_sequences(
            &mut DecodeOut::growable(&mut output),
            0,
            128 * 1024,
            128 * 1024,
            None,
            &literals,
            &sequences,
            &mut decode_repeat_offsets,
            None,
        )
        .unwrap();

        (literals, sequences, output)
    }

    fn raw_literals_section(literals: &[u8]) -> Vec<u8> {
        let mut payload = Vec::with_capacity(3 + literals.len());
        if literals.len() <= 31 {
            payload.push((literals.len() as u8) << 3);
        } else if literals.len() <= 0x0fff {
            let value = (1u32 << 2) | ((literals.len() as u32) << 4);
            payload.extend_from_slice(&[(value & 0xff) as u8, ((value >> 8) & 0xff) as u8]);
        } else {
            let value = (3u32 << 2) | ((literals.len() as u32) << 4);
            payload.extend_from_slice(&[
                (value & 0xff) as u8,
                ((value >> 8) & 0xff) as u8,
                ((value >> 16) & 0xff) as u8,
            ]);
        }
        payload.extend_from_slice(literals);
        payload
    }
}
