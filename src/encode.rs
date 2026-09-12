use std::time::{Duration, Instant};

use crate::{
    block::{BLOCK_HEADER_SIZE, BLOCK_SIZE_MAX, BlockHeader, BlockType},
    dictionary::{Dictionary, EncoderDictionary},
    entropy::huff0,
    error::{Error, Result},
    frame::{
        Format, MAX_DECLARABLE_WINDOW_SIZE, write_single_segment_header_with_dict,
        write_windowed_header_with_content_size, write_windowed_header_with_dict,
    },
    outbuf::OutBuf,
    sequence::{
        CompressionMode as SequenceCompressionMode, RepeatOffsets, SequenceCommand,
        SequenceCompressionModes, SequenceEncodeScratch, SequenceEncodingState,
        SequenceSectionTimings, build_literal_offset_table,
        encode_prepared_seq_store_section_with_strategy_and_scratch_into_stats,
        encode_sequence_section_direct, estimate_subblock_cost_bits, offset_code,
        prepare_seq_store_encode_scratch, reconcile_subblock_repcodes,
    },
    window::{
        ContiguousBlockMatchState, LdmFrameState, LdmMode, LdmParameterOverrides, LdmParameters,
        MatchFinderParameters, OPT_PREDEFINED_THRESHOLD, ParserStrategy, PrefixMatchMode,
        PrefixedBlockMatchState, PreparedDictionaryMatchState, SequencePlan,
        SequenceTraceEmissionKind, SequenceTraceMatchSource, SequenceTraceRowSearchContest,
        force_window_log, plan_sequences_for_block_into, plan_sequences_for_contiguous_block_into,
        plan_sequences_for_contiguous_block_with_ldm_into,
        plan_sequences_for_prefixed_contiguous_block_into,
        plan_sequences_for_prefixed_contiguous_block_with_ldm_into,
    },
    xxhash::xxh64,
};

const MIN_COMPRESSIBLE_BLOCK_SIZE: usize = 7;
const UPSTREAM_CLEVEL_DEFAULT: u8 = 3;
/// Upstream's `ZSTD_TARGETLENGTH_MAX`, which it defines as `ZSTD_BLOCKSIZE_MAX`.
/// Bounds the acceleration factor a negative level can ask for, and so bounds
/// the negative levels themselves: `ZSTD_minCLevel()` is its negation.
const UPSTREAM_TARGETLENGTH_MAX: u32 = BLOCK_SIZE_MAX as u32;
const UPSTREAM_ROW_MATCH_FINDER_WINDOWLOG_LOWER_BOUND: u32 = 14;
const UPSTREAM_UNKNOWN_SRC_SIZE_DICT_ADJUST: usize = 500;
const UPSTREAM_CREATE_CDICT_MIN_SRC_SIZE: usize = 513;
const UPSTREAM_HASHLOG_MIN: u32 = 6;
const UPSTREAM_WINDOWLOG_ABSOLUTEMIN: u32 = 10;
const UPSTREAM_WINDOWLOG_MAX: u32 = if usize::BITS == 32 { 30 } else { 31 };
const UPSTREAM_ROW_HASH_TAG_BITS: u32 = 8;
const UPSTREAM_SHORT_CACHE_TAG_BITS: u32 = 8;
const UPSTREAM_SPLIT_FULL_BLOCK_SIZE: usize = 128 * 1024;
pub(crate) const UPSTREAM_SPLIT_CHUNK_SIZE: usize = 8 * 1024;

/// Widest a Zstandard frame header can be: magic, frame header descriptor,
/// window descriptor, a 4-byte dictionary ID and an 8-byte content size.
pub(crate) const FRAME_HEADER_MAX: usize = 18;
/// Bytes the optional content checksum adds to the end of a frame.
pub(crate) const CHECKSUM_SIZE: usize = 4;
const UPSTREAM_SPLIT_THRESHOLD_PENALTY_RATE: u64 = 16;
const UPSTREAM_SPLIT_THRESHOLD_BASE: u64 = UPSTREAM_SPLIT_THRESHOLD_PENALTY_RATE - 2;
const UPSTREAM_SPLIT_THRESHOLD_PENALTY: u64 = 3;
const UPSTREAM_SPLIT_HASHLOG_MAX: usize = 10;
const UPSTREAM_SPLIT_HASHTABLE_SIZE: usize = 1 << UPSTREAM_SPLIT_HASHLOG_MAX;
const UPSTREAM_SPLIT_HASH_MASK: usize = UPSTREAM_SPLIT_HASHTABLE_SIZE - 1;
const UPSTREAM_SPLIT_SEGMENT_SIZE: usize = 512;
const UPSTREAM_SPLIT_MIN_FINGERPRINT_DISTANCE: u64 =
    (UPSTREAM_SPLIT_SEGMENT_SIZE * UPSTREAM_SPLIT_SEGMENT_SIZE / 3) as u64;
const UPSTREAM_SPLIT_KNUTH: u32 = 0x9e37_79b9;

/// Ordered, and numbered as C numbers `ZSTD_strategy`: several upstream rules
/// compare strategies with `>=` or divide the code (`ZSTD_ldm_adjustParameters`
/// maps `7 - strategy / 3`), so both the ordering and the 1-based values are
/// load-bearing rather than incidental.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum UpstreamStrategy {
    Fast = 1,
    DoubleFast,
    Greedy,
    Lazy,
    Lazy2,
    BinaryTreeLazy2,
    BinaryTreeOpt,
    BinaryTreeUltra,
    BinaryTreeUltra2,
}

impl UpstreamStrategy {
    /// The value C's `ZSTD_strategy` takes, `1..=9`.
    pub(crate) const fn as_upstream_code(self) -> u32 {
        self as u32
    }

    fn supports_row_match_finder(self) -> bool {
        matches!(self, Self::Greedy | Self::Lazy | Self::Lazy2)
    }

    fn is_binary_tree(self) -> bool {
        matches!(
            self,
            Self::BinaryTreeLazy2
                | Self::BinaryTreeOpt
                | Self::BinaryTreeUltra
                | Self::BinaryTreeUltra2
        )
    }

    pub(crate) fn is_optimal(self) -> bool {
        matches!(
            self,
            Self::BinaryTreeOpt | Self::BinaryTreeUltra | Self::BinaryTreeUltra2
        )
    }

    fn default_block_split_level(self) -> usize {
        match self {
            Self::Fast => 0,
            Self::DoubleFast => 1,
            Self::Greedy | Self::Lazy => 2,
            Self::Lazy2 | Self::BinaryTreeLazy2 => 3,
            Self::BinaryTreeOpt | Self::BinaryTreeUltra | Self::BinaryTreeUltra2 => 4,
        }
    }
}

#[derive(Clone)]
struct BlockSplitFingerprint {
    events: [u32; UPSTREAM_SPLIT_HASHTABLE_SIZE],
    count: usize,
}

impl Default for BlockSplitFingerprint {
    fn default() -> Self {
        Self {
            events: [0; UPSTREAM_SPLIT_HASHTABLE_SIZE],
            count: 0,
        }
    }
}

impl BlockSplitFingerprint {
    fn clear_prefix<const HASH_LOG: usize>(&mut self) {
        self.events[..(1usize << HASH_LOG)].fill(0);
        self.count = 0;
    }

    /// Sample every `SAMPLING_RATE`th position of `src` into the fingerprint.
    ///
    /// The rate and the hash width are const parameters rather than arguments
    /// because upstream's are: `addEvents_generic` carries the comment "The
    /// speed of this method relies on compile-time constant propagation", and
    /// `ZSTD_splitBlock_byChunks` reaches its four specialisations through a
    /// table of function pointers so that each one sees both as literals.
    /// Passed at run time they cost a variable stride here and, in
    /// [`split_block_hash2`], a branch and a variable shift.
    fn add_sampled_bytes<const SAMPLING_RATE: usize, const HASH_LOG: usize>(&mut self, src: &[u8]) {
        const { assert!(HASH_LOG <= UPSTREAM_SPLIT_HASHLOG_MAX) }
        if src.len() < 2 {
            return;
        }
        let limit = src.len() - 1;
        for pos in (0..limit).step_by(SAMPLING_RATE) {
            let bucket = split_block_hash2::<HASH_LOG>(&src[pos..]);
            self.events[bucket] = self.events[bucket].saturating_add(1);
        }
        self.count += limit / SAMPLING_RATE;
    }

    fn record_sampled_bytes<const SAMPLING_RATE: usize, const HASH_LOG: usize>(
        &mut self,
        src: &[u8],
    ) {
        self.clear_prefix::<HASH_LOG>();
        self.add_sampled_bytes::<SAMPLING_RATE, HASH_LOG>(src);
    }

    /// Fold `other`'s counts in, over the `hash_log` prefix that is the whole
    /// live table.
    ///
    /// Both bounds matter. `zip(other.events)` moved the `[u32; 1024]` by
    /// value, so each call copied 4 KiB before adding anything, and the walk
    /// then covered all 1024 slots when `record_sampled_bytes` only ever fills
    /// `1 << hash_log` of them and `block_split_fp_distance` only ever reads
    /// that many -- 256 at the split level the double-fast strategies use.
    fn merge_from<const HASH_LOG: usize>(&mut self, other: &Self) {
        const { assert!(HASH_LOG <= UPSTREAM_SPLIT_HASHLOG_MAX) }
        let live = 1usize << HASH_LOG;
        for (left, right) in self.events[..live].iter_mut().zip(&other.events[..live]) {
            *left = left.saturating_add(*right);
        }
        self.count += other.count;
    }

    fn add_histogram_bytes(&mut self, src: &[u8]) {
        for &byte in src {
            self.events[byte as usize] = self.events[byte as usize].saturating_add(1);
        }
        self.count += src.len();
    }
}

/// Upstream's `hash2`: the raw byte at a hash width of 8, two hashed bytes
/// above it.
///
/// The mask is redundant -- the shift already leaves at most `HASH_LOG` bits --
/// and is kept only so the index is visibly in range for `events`, which
/// removes the bounds check rather than relying on the shift to be understood.
fn split_block_hash2<const HASH_LOG: usize>(src: &[u8]) -> usize {
    const { assert!(HASH_LOG >= 8 && HASH_LOG <= UPSTREAM_SPLIT_HASHLOG_MAX) }
    if HASH_LOG == 8 {
        return src[0] as usize;
    }
    let value = u16::from_le_bytes([src[0], src[1]]) as u32;
    ((value.wrapping_mul(UPSTREAM_SPLIT_KNUTH)) >> (32 - HASH_LOG)) as usize
        & UPSTREAM_SPLIT_HASH_MASK
}

/// Upstream's `fpDistance`, over the `HASH_LOG` prefix that is the live table.
///
/// `HASH_LOG` is const where upstream's is an argument. Every caller on both
/// sides already knows it as a literal, and a constant trip count is what lets
/// the loop unroll.
fn block_split_fp_distance<const HASH_LOG: usize>(
    left: &BlockSplitFingerprint,
    right: &BlockSplitFingerprint,
) -> u64 {
    let mut distance = 0u64;
    for index in 0..(1usize << HASH_LOG) {
        let left_scaled = left.events[index] as i64 * right.count as i64;
        let right_scaled = right.events[index] as i64 * left.count as i64;
        distance += left_scaled.abs_diff(right_scaled);
    }
    distance
}

fn block_split_fingerprints_differ<const HASH_LOG: usize>(
    reference: &BlockSplitFingerprint,
    candidate: &BlockSplitFingerprint,
    penalty: u64,
) -> bool {
    if reference.count == 0 || candidate.count == 0 {
        return false;
    }
    let p50 = reference.count as u64 * candidate.count as u64;
    let deviation = block_split_fp_distance::<HASH_LOG>(reference, candidate);
    let threshold =
        p50 * (UPSTREAM_SPLIT_THRESHOLD_BASE + penalty) / UPSTREAM_SPLIT_THRESHOLD_PENALTY_RATE;
    deviation >= threshold
}

fn upstream_split_block_from_borders(block: &[u8]) -> usize {
    debug_assert_eq!(block.len(), UPSTREAM_SPLIT_FULL_BLOCK_SIZE);
    let mut begin = BlockSplitFingerprint::default();
    let mut end = BlockSplitFingerprint::default();
    let mut middle = BlockSplitFingerprint::default();
    begin.add_histogram_bytes(&block[..UPSTREAM_SPLIT_SEGMENT_SIZE]);
    end.add_histogram_bytes(&block[block.len() - UPSTREAM_SPLIT_SEGMENT_SIZE..block.len()]);
    if !block_split_fingerprints_differ::<8>(&begin, &end, 0) {
        return block.len();
    }
    let middle_start = block.len() / 2 - UPSTREAM_SPLIT_SEGMENT_SIZE / 2;
    middle.add_histogram_bytes(&block[middle_start..middle_start + UPSTREAM_SPLIT_SEGMENT_SIZE]);
    let dist_from_begin = block_split_fp_distance::<8>(&begin, &middle);
    let dist_from_end = block_split_fp_distance::<8>(&end, &middle);
    if dist_from_begin.abs_diff(dist_from_end) < UPSTREAM_SPLIT_MIN_FINGERPRINT_DISTANCE {
        return 64 * 1024;
    }
    if dist_from_begin > dist_from_end {
        32 * 1024
    } else {
        96 * 1024
    }
}

/// Upstream's four `(samplingRate, hashLog)` pairs, one specialisation each.
///
/// The dispatch is a `match` where upstream indexes a table of function
/// pointers; both exist so that the chunk walk below sees the pair as
/// literals. Sharing one run-time-parameterised body instead measured 1.20x
/// upstream at the greedy and lazy levels, which are the ones whose hash width
/// is above 8 and so cannot take `split_block_hash2`'s raw-byte path.
fn upstream_split_block_by_chunks(block: &[u8], level: usize) -> usize {
    match level {
        0 => upstream_split_block_by_chunks_at::<43, 8>(block),
        1 => upstream_split_block_by_chunks_at::<11, 9>(block),
        2 => upstream_split_block_by_chunks_at::<5, 10>(block),
        3 => upstream_split_block_by_chunks_at::<1, 10>(block),
        _ => unreachable!("invalid block split level"),
    }
}

fn upstream_split_block_by_chunks_at<const SAMPLING_RATE: usize, const HASH_LOG: usize>(
    block: &[u8],
) -> usize {
    debug_assert_eq!(block.len(), UPSTREAM_SPLIT_FULL_BLOCK_SIZE);
    let mut past = BlockSplitFingerprint::default();
    let mut current = BlockSplitFingerprint::default();
    let mut penalty = UPSTREAM_SPLIT_THRESHOLD_PENALTY;
    past.record_sampled_bytes::<SAMPLING_RATE, HASH_LOG>(&block[..UPSTREAM_SPLIT_CHUNK_SIZE]);
    for pos in (UPSTREAM_SPLIT_CHUNK_SIZE..=block.len() - UPSTREAM_SPLIT_CHUNK_SIZE)
        .step_by(UPSTREAM_SPLIT_CHUNK_SIZE)
    {
        current.record_sampled_bytes::<SAMPLING_RATE, HASH_LOG>(
            &block[pos..pos + UPSTREAM_SPLIT_CHUNK_SIZE],
        );
        if block_split_fingerprints_differ::<HASH_LOG>(&past, &current, penalty) {
            return pos;
        }
        past.merge_from::<HASH_LOG>(&current);
        penalty = penalty.saturating_sub(1);
    }
    block.len()
}

fn upstream_split_block(block: &[u8], split_level: usize) -> usize {
    debug_assert_eq!(block.len(), UPSTREAM_SPLIT_FULL_BLOCK_SIZE);
    if split_level == 0 {
        return upstream_split_block_from_borders(block);
    }
    upstream_split_block_by_chunks(block, split_level - 1)
}

pub(crate) fn upstream_optimal_block_size(
    src: &[u8],
    block_start: usize,
    block_size_max: usize,
    strategy: UpstreamStrategy,
    savings: i64,
) -> usize {
    let remaining = src.len().saturating_sub(block_start);
    if remaining < UPSTREAM_SPLIT_FULL_BLOCK_SIZE || block_size_max < UPSTREAM_SPLIT_FULL_BLOCK_SIZE
    {
        return remaining.min(block_size_max);
    }
    if savings < 3 {
        return UPSTREAM_SPLIT_FULL_BLOCK_SIZE;
    }
    let split_level = strategy.default_block_split_level();
    upstream_split_block(
        &src[block_start..block_start + UPSTREAM_SPLIT_FULL_BLOCK_SIZE],
        split_level,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UpstreamCompressionParameters {
    pub(crate) window_log: u32,
    pub(crate) chain_log: u32,
    pub(crate) hash_log: u32,
    pub(crate) search_log: u32,
    pub(crate) min_match: u32,
    pub(crate) target_length: u32,
    pub(crate) strategy: UpstreamStrategy,
}

const fn upstream_cparams(
    window_log: u32,
    chain_log: u32,
    hash_log: u32,
    search_log: u32,
    min_match: u32,
    target_length: u32,
    strategy: UpstreamStrategy,
) -> UpstreamCompressionParameters {
    UpstreamCompressionParameters {
        window_log,
        chain_log,
        hash_log,
        search_log,
        min_match,
        target_length,
        strategy,
    }
}

/// Which of upstream's `ZSTD_CParamMode_e` a parameter resolution is running
/// under. The mode is not decoration: it decides what `dictSize` *means* at two
/// separate points, and the two disagree by up to six window logs.
///
/// The shared `Dict` postfix is deliberate and the lint against it is waived
/// rather than satisfied: these are transliterations of `ZSTD_cpm_noAttachDict`,
/// `ZSTD_cpm_createCDict` and `ZSTD_cpm_attachDict`, and the whole value of the
/// names is that a reader can grep C for them. Shortening them to `Attach` and
/// `Create` would break that for the sake of a stutter the enum's own name
/// already carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
enum UpstreamCParamMode {
    NoAttachDict,
    CreateCDict,
    /// C's `ZSTD_cpm_attachDict`, whose comment reads "Dictionary has its own
    /// dedicated parameters which have already been selected. We are selecting
    /// parameters for only the source."
    ///
    /// It **zeroes `dictSize`** in both `ZSTD_getCParamRowSize`
    /// (`zstd_compress.c:7741`) and `ZSTD_adjustCParams_internal` (`:1540`), so
    /// the row comes from the source alone. With an unknown source size that
    /// flips `unknown && dictSize == 0` and the row is the unknown tier rather
    /// than the `dictSize + 500` one — a level's own window instead of a
    /// dictionary-sized one.
    AttachDict,
}

/// Validated compression level, `-131072..=22` (see Zstandard's
/// `ZSTD_compressionLevel`). Higher values trade encode CPU for output size.
///
/// Levels `1..=22` select progressively more thorough parsers. Level `0` is an
/// alias for [`Self::DEFAULT`], matching upstream. Negative levels are
/// upstream's "fast mode": they all share one parameter set, the row upstream
/// calls the base for negative levels, and use the level's magnitude as an
/// acceleration factor that makes the fast parser skip further between match
/// attempts. More negative is faster and compresses less.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompressionLevel(i32);

impl CompressionLevel {
    /// Minimum supported compression level, upstream's `ZSTD_minCLevel()`.
    ///
    /// The value is `-131072`, which is `-ZSTD_TARGETLENGTH_MAX`: the
    /// acceleration factor a negative level sets is its own magnitude, and
    /// that factor is a target length, so the largest representable target
    /// length is what bounds the level.
    pub const MIN: Self = Self(-(UPSTREAM_TARGETLENGTH_MAX as i32));
    /// Maximum supported compression level (`22`).
    pub const MAX: Self = Self(22);
    /// Lowest non-negative level (`1`), the fastest that is not "fast mode".
    pub const MIN_POSITIVE: Self = Self(1);
    /// Fastest of the ordinary levels (`1`).
    ///
    /// This is [`Self::MIN_POSITIVE`], not [`Self::MIN`]: the negative levels
    /// are faster still, but they trade away enough ratio that they are a
    /// deliberate choice rather than the default meaning of "fastest".
    pub const FASTEST: Self = Self(1);
    /// Default level used when no level is specified.
    pub const DEFAULT: Self = Self(UPSTREAM_CLEVEL_DEFAULT as i32);
    /// Higher-ratio level suitable for "better" compression presets.
    pub const BETTER: Self = Self(6);
    /// Highest level supported (alias for [`Self::MAX`]).
    pub const BEST: Self = Self(22);

    /// Construct a compression level, returning [`Error::InvalidParameter`]
    /// for values outside `-131072..=22`.
    ///
    /// Upstream clamps out-of-range levels to the same bounds rather than
    /// reporting them. This rejects instead: a caller who asks for `-200000`
    /// has a bug, and compressing at a level they did not name would hide it.
    /// Every level this accepts is byte-compatible with upstream at the same
    /// level, so the divergence is in error reporting only.
    pub fn try_new(level: i32) -> Result<Self> {
        if (Self::MIN.0..=Self::MAX.0).contains(&level) {
            Ok(Self(level))
        } else {
            Err(Error::InvalidParameter(
                "compression_level must be in -131072..=22",
            ))
        }
    }

    /// Underlying level value (`-131072..=22`).
    pub const fn as_i32(self) -> i32 {
        self.0
    }

    #[allow(dead_code)]
    pub(crate) fn parameters(self) -> CompressionParameters {
        compression_parameters_for_input(self, None, None)
    }
}

impl Default for CompressionLevel {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<i32> for CompressionLevel {
    type Error = Error;

    fn try_from(value: i32) -> Result<Self> {
        Self::try_new(value)
    }
}

impl TryFrom<u8> for CompressionLevel {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self> {
        Self::try_new(i32::from(value))
    }
}

impl From<CompressionLevel> for i32 {
    fn from(value: CompressionLevel) -> Self {
        value.as_i32()
    }
}

/// Match-finding strategy, upstream's `ZSTD_strategy`.
///
/// A [`CompressionLevel`] picks one of these along with the table sizes that
/// suit it; [`ParameterOverrides::strategy`] names one directly. The order is
/// upstream's, cheapest first, and [`Ord`] follows it — `Fast` finds the
/// fewest matches for the least work and `BinaryTreeUltra2` the most for the
/// most.
///
/// The discriminants are upstream's own, so [`Self::as_u32`] is directly
/// comparable with a `ZSTD_c_strategy` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Strategy {
    /// `ZSTD_fast`: one hash table, one probe per position.
    Fast = 1,
    /// `ZSTD_dfast`: two hash tables at different match lengths.
    DoubleFast = 2,
    /// `ZSTD_greedy`: hash chains, taking the first match found.
    Greedy = 3,
    /// `ZSTD_lazy`: greedy, but looks one position ahead for a better match.
    Lazy = 4,
    /// `ZSTD_lazy2`: looks two positions ahead.
    Lazy2 = 5,
    /// `ZSTD_btlazy2`: `Lazy2` over a binary tree instead of hash chains.
    BinaryTreeLazy2 = 6,
    /// `ZSTD_btopt`: price-based optimal parse.
    BinaryTreeOpt = 7,
    /// `ZSTD_btultra`: optimal parse with more accurate prices.
    BinaryTreeUltra = 8,
    /// `ZSTD_btultra2`: `BinaryTreeUltra` with a price-table warm-up pass.
    BinaryTreeUltra2 = 9,
}

impl Strategy {
    /// The value upstream's `ZSTD_c_strategy` takes for this strategy.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    const fn to_upstream(self) -> UpstreamStrategy {
        match self {
            Self::Fast => UpstreamStrategy::Fast,
            Self::DoubleFast => UpstreamStrategy::DoubleFast,
            Self::Greedy => UpstreamStrategy::Greedy,
            Self::Lazy => UpstreamStrategy::Lazy,
            Self::Lazy2 => UpstreamStrategy::Lazy2,
            Self::BinaryTreeLazy2 => UpstreamStrategy::BinaryTreeLazy2,
            Self::BinaryTreeOpt => UpstreamStrategy::BinaryTreeOpt,
            Self::BinaryTreeUltra => UpstreamStrategy::BinaryTreeUltra,
            Self::BinaryTreeUltra2 => UpstreamStrategy::BinaryTreeUltra2,
        }
    }
}

/// Inclusive bounds for one compression parameter, upstream's `ZSTD_bounds`.
///
/// The values live as associated constants on [`ParameterOverrides`], so a
/// caller can size a slider or reject a configuration without compressing
/// anything first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParameterBounds {
    /// Smallest accepted value.
    pub min: u32,
    /// Largest accepted value.
    pub max: u32,
}

impl ParameterBounds {
    const fn new(min: u32, max: u32) -> Self {
        Self { min, max }
    }

    /// Whether `value` lies within these bounds.
    pub const fn contains(self, value: u32) -> bool {
        value >= self.min && value <= self.max
    }

    fn check(self, value: Option<u32>, message: &'static str) -> Result<()> {
        match value {
            Some(value) if !self.contains(value) => Err(Error::InvalidParameter(message)),
            _ => Ok(()),
        }
    }
}

/// Whether the row-based match finder runs, upstream's
/// `ZSTD_c_useRowMatchFinder`.
///
/// Three states rather than a `bool` because [`Self::Auto`] is upstream's
/// default and is neither: it turns the row finder on for some parameter sets
/// and not others, from the window the parameters were finally fitted to.
///
/// The row finder is a different search over the same candidates as the plain
/// hash chain, so it changes throughput and, because the two do not always
/// settle on the same match, output. It exists only for the three lazy
/// strategies; asking for it under any other is accepted and does nothing,
/// which is upstream's behaviour too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowMatchFinderMode {
    /// Decide from the resolved compression parameters, upstream's
    /// `ZSTD_ps_auto`: on for the lazy strategies once the window exceeds
    /// `1 << 14`.
    #[default]
    Auto,
    /// Always on where the strategy supports it, upstream's `ZSTD_ps_enable`.
    Enabled,
    /// Always off, upstream's `ZSTD_ps_disable`.
    Disabled,
}

/// Whether the literals section is entropy-coded, upstream's
/// `ZSTD_c_literalCompressionMode`.
///
/// Three states rather than a `bool` because [`Self::Auto`] is upstream's
/// default and is neither: it Huffman-codes literals for every parameter set
/// but the accelerated ones, which is the shape the negative
/// [`CompressionLevel`]s take.
///
/// Literals are the bytes no match covered. Coding them costs a table in the
/// block and a pass over the bytes, and buys whatever their entropy is worth;
/// [`Self::Disabled`] stores them verbatim instead, trading ratio for speed on
/// both sides of the wire, since the decoder then has no Huffman stream to
/// read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiteralCompressionMode {
    /// Decide from the resolved compression parameters, upstream's
    /// `ZSTD_ps_auto`: coded unless the strategy is [`Strategy::Fast`] with a
    /// non-zero `target_length`, which is what the negative levels resolve to.
    #[default]
    Auto,
    /// Always Huffman-code them, upstream's `ZSTD_ps_enable`. This is the only
    /// way to reach a coded literals section under a negative level.
    Enabled,
    /// Always store them verbatim, upstream's `ZSTD_ps_disable`.
    Disabled,
}

/// Compression parameters to use instead of the ones the level would choose,
/// upstream's `ZSTD_c_windowLog` and friends.
///
/// `None` means "whatever the level chose". Upstream spells that `0`, which
/// costs it the ability to *set* a parameter to zero; expressing it as an
/// `Option` keeps the sentinel out of the value space. The one place that
/// matters is [`Self::target_length`], where `Some(0)` asks for something
/// upstream's API cannot request at all.
///
/// Overrides land on the parameters a level and source size selected, and the
/// result is then re-adjusted to the source — so asking for a `window_log`
/// larger than the input can use still yields the smaller window, exactly as
/// upstream does. Every field is checked against the bounds below when the
/// encoder starts, so an unusable combination is reported rather than clamped.
///
/// ```
/// use zstandard::{EncoderOptions, ParameterOverrides, Strategy, decode_all, encode_all_with_options};
///
/// let options = EncoderOptions {
///     parameters: ParameterOverrides {
///         strategy: Some(Strategy::BinaryTreeUltra2),
///         ..Default::default()
///     },
///     ..Default::default()
/// };
/// let compressed = encode_all_with_options(b"override the level's strategy", options)?;
/// assert_eq!(decode_all(&compressed)?, b"override the level's strategy");
/// # Ok::<(), zstandard::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ParameterOverrides {
    /// Base-2 log of the match window, upstream's `ZSTD_c_windowLog`.
    pub window_log: Option<u32>,
    /// Base-2 log of the primary hash table size, upstream's `ZSTD_c_hashLog`.
    pub hash_log: Option<u32>,
    /// Base-2 log of the match chain (or binary tree) size, upstream's `ZSTD_c_chainLog`.
    pub chain_log: Option<u32>,
    /// Base-2 log of the number of search attempts, upstream's `ZSTD_c_searchLog`.
    pub search_log: Option<u32>,
    /// Shortest match the parser will emit, upstream's `ZSTD_c_minMatch`.
    pub min_match: Option<u32>,
    /// Match length treated as good enough to stop searching, upstream's
    /// `ZSTD_c_targetLength`. Under [`Strategy::Fast`] it is instead the
    /// acceleration factor, which is what the negative [`CompressionLevel`]s
    /// set.
    pub target_length: Option<u32>,
    /// Match-finding strategy, upstream's `ZSTD_c_strategy`.
    pub strategy: Option<Strategy>,
    /// Whether the row-based match finder runs, upstream's
    /// `ZSTD_c_useRowMatchFinder`. Defaults to [`RowMatchFinderMode::Auto`].
    ///
    /// This reaches more than the choice of parser. Upstream folds the row
    /// finder's tag bits into the hash, so it caps `hash_log` when the finder
    /// is in use, and it applies that cap whenever the mode is not explicitly
    /// disabled -- `auto` counts as on for the purpose of sizing. Turning the
    /// finder off therefore lets a large [`Self::hash_log`] through where
    /// `auto` would have clamped it.
    pub use_row_match_finder: RowMatchFinderMode,
    /// Whether the literals section is entropy-coded, upstream's
    /// `ZSTD_c_literalCompressionMode`. Defaults to
    /// [`LiteralCompressionMode::Auto`].
    ///
    /// Under the optimal strategies this also reaches the parse. Their cost
    /// model prices a literal at whatever the block's statistics say it is
    /// worth, and [`LiteralCompressionMode::Disabled`] prices it at the eight
    /// bits it will actually occupy — so the parser trades matches for literals
    /// differently, and the frame differs by more than its literals section.
    pub literal_compression: LiteralCompressionMode,
    /// Whether to run the long-distance matcher, upstream's
    /// `ZSTD_c_enableLongDistanceMatching`. Defaults to [`LdmMode::Auto`].
    ///
    /// Enabling it explicitly also *sets* the window to `1 << 27`, before
    /// [`Self::window_log`] is applied and before the window is fitted to the
    /// source. Long-distance matching only earns its keep on a window too wide
    /// for the parser's own tables, so upstream does not treat the two as
    /// independent.
    ///
    /// [`LdmMode::Enabled`] alongside a dictionary is reported as an error
    /// rather than encoded without it, since a frame that quietly omits it is
    /// indistinguishable from one that used it. Both encoders take it
    /// otherwise.
    pub long_distance_matching: LdmMode,
    /// Base-2 log of the long-distance hash table, upstream's `ZSTD_c_ldmHashLog`.
    ///
    /// Each of these four defaults to a value derived from the window and the
    /// strategy, and supplying one changes how the others are derived rather
    /// than only replacing its own. See `LdmParameters::resolve`.
    pub ldm_hash_log: Option<u32>,
    /// Shortest long-distance match, upstream's `ZSTD_c_ldmMinMatch`.
    pub ldm_min_match: Option<u32>,
    /// Base-2 log of the candidates kept per long-distance hash bucket,
    /// upstream's `ZSTD_c_ldmBucketSizeLog`.
    pub ldm_bucket_size_log: Option<u32>,
    /// Base-2 log of how many positions the long-distance matcher skips
    /// between insertions, upstream's `ZSTD_c_ldmHashRateLog`.
    pub ldm_hash_rate_log: Option<u32>,
}

impl ParameterOverrides {
    /// Bounds for [`Self::window_log`].
    ///
    /// The ceiling is `27`, not upstream's `ZSTD_WINDOWLOG_MAX` of `31`. A
    /// frame declaring more than `1 << 27` is refused by the reference
    /// decoder at its default settings, so this encoder never emits one; see
    /// `MAX_DECLARABLE_WINDOW_SIZE`. Levels `1..=22` top out at `27` too, so
    /// nothing a level can reach is excluded.
    pub const WINDOW_LOG: ParameterBounds = ParameterBounds::new(
        UPSTREAM_WINDOWLOG_ABSOLUTEMIN,
        MAX_DECLARABLE_WINDOW_SIZE.trailing_zeros(),
    );
    /// Bounds for [`Self::hash_log`], upstream's `ZSTD_HASHLOG_MIN/MAX`.
    ///
    /// The table is `1 << hash_log` 32-bit entries, so the top of this range
    /// asks for four gibibytes. Adjustment shrinks it to fit whenever the
    /// source size is known, which is every one-shot call. The fast and
    /// double-fast parsers cap it lower still, at 24, because their tables
    /// carry an 8-bit tag in the same 32-bit hash.
    pub const HASH_LOG: ParameterBounds = ParameterBounds::new(UPSTREAM_HASHLOG_MIN, 30);
    /// Bounds for [`Self::chain_log`], upstream's `ZSTD_CHAINLOG_MIN/MAX`.
    pub const CHAIN_LOG: ParameterBounds = ParameterBounds::new(UPSTREAM_HASHLOG_MIN, 30);
    /// Bounds for [`Self::search_log`], upstream's `ZSTD_SEARCHLOG_MIN/MAX`.
    pub const SEARCH_LOG: ParameterBounds = ParameterBounds::new(1, UPSTREAM_WINDOWLOG_MAX - 1);
    /// Bounds for [`Self::min_match`], upstream's `ZSTD_MINMATCH_MIN/MAX`.
    ///
    /// Not every strategy uses the whole range: upstream documents `3` as
    /// reachable only by [`Strategy::BinaryTreeOpt`] and above, and `7` only
    /// by [`Strategy::Fast`], and clamps the rest inside the parsers rather
    /// than rejecting them (`zstd_lazy.c:1531`, `zstd_opt.c:896`). This
    /// accepts the full range for the same reason, and a value a strategy
    /// cannot use is quietly narrowed rather than reported.
    pub const MIN_MATCH: ParameterBounds = ParameterBounds::new(3, 7);
    /// Bounds for [`Self::target_length`], upstream's `ZSTD_TARGETLENGTH_MIN/MAX`.
    pub const TARGET_LENGTH: ParameterBounds = ParameterBounds::new(0, UPSTREAM_TARGETLENGTH_MAX);
    /// Bounds for [`Self::ldm_hash_log`], upstream's `ZSTD_HASHLOG_MIN/MAX`.
    pub const LDM_HASH_LOG: ParameterBounds = ParameterBounds::new(UPSTREAM_HASHLOG_MIN, 30);
    /// Bounds for [`Self::ldm_min_match`], upstream's
    /// `ZSTD_LDM_MINMATCH_MIN/MAX`.
    pub const LDM_MIN_MATCH: ParameterBounds = ParameterBounds::new(4, 4096);
    /// Bounds for [`Self::ldm_bucket_size_log`], upstream's
    /// `ZSTD_LDM_BUCKETSIZELOG_MIN/MAX`.
    pub const LDM_BUCKET_SIZE_LOG: ParameterBounds = ParameterBounds::new(1, 8);
    /// Bounds for [`Self::ldm_hash_rate_log`], upstream's
    /// `ZSTD_LDM_HASHRATELOG_MIN/MAX`.
    pub const LDM_HASH_RATE_LOG: ParameterBounds =
        ParameterBounds::new(0, UPSTREAM_WINDOWLOG_MAX - UPSTREAM_HASHLOG_MIN);

    fn validate(self) -> Result<()> {
        Self::WINDOW_LOG.check(self.window_log, "window_log must be in 10..=27")?;
        Self::HASH_LOG.check(self.hash_log, "hash_log must be in 6..=30")?;
        Self::CHAIN_LOG.check(self.chain_log, "chain_log must be in 6..=30")?;
        Self::SEARCH_LOG.check(self.search_log, "search_log must be in 1..=30")?;
        Self::MIN_MATCH.check(self.min_match, "min_match must be in 3..=7")?;
        Self::TARGET_LENGTH.check(self.target_length, "target_length must be in 0..=131072")?;
        Self::LDM_HASH_LOG.check(self.ldm_hash_log, "ldm_hash_log must be in 6..=30")?;
        Self::LDM_MIN_MATCH.check(self.ldm_min_match, "ldm_min_match must be in 4..=4096")?;
        Self::LDM_BUCKET_SIZE_LOG.check(
            self.ldm_bucket_size_log,
            "ldm_bucket_size_log must be in 1..=8",
        )?;
        Self::LDM_HASH_RATE_LOG.check(
            self.ldm_hash_rate_log,
            "ldm_hash_rate_log must be in 0..=25",
        )?;
        Ok(())
    }

    /// The four long-distance parameters, in the shape the resolver takes.
    fn ldm_overrides(self) -> LdmParameterOverrides {
        LdmParameterOverrides {
            hash_log: self.ldm_hash_log,
            min_match_length: self.ldm_min_match,
            bucket_size_log: self.ldm_bucket_size_log,
            hash_rate_log: self.ldm_hash_rate_log,
        }
    }

    /// Upstream's `ZSTD_overrideCParams`.
    fn apply_to(self, cparams: &mut UpstreamCompressionParameters) {
        if let Some(window_log) = self.window_log {
            cparams.window_log = window_log;
        }
        if let Some(hash_log) = self.hash_log {
            cparams.hash_log = hash_log;
        }
        if let Some(chain_log) = self.chain_log {
            cparams.chain_log = chain_log;
        }
        if let Some(search_log) = self.search_log {
            cparams.search_log = search_log;
        }
        if let Some(min_match) = self.min_match {
            cparams.min_match = min_match;
        }
        if let Some(target_length) = self.target_length {
            cparams.target_length = target_length;
        }
        if let Some(strategy) = self.strategy {
            cparams.strategy = strategy.to_upstream();
        }
    }
}

/// The parameters a level resolves to, with nothing overridden.
///
/// Callers that have an [`EncoderOptions`] should use
/// [`compression_parameters_for_options`] instead, so the caller's overrides
/// and pledged size are honoured.
pub(crate) fn compression_parameters_for_input(
    level: CompressionLevel,
    src_size_hint: Option<usize>,
    dictionary: Option<&Dictionary<'_>>,
) -> CompressionParameters {
    compression_parameters_with_overrides(
        level,
        src_size_hint,
        ParameterOverrides::default(),
        dictionary,
    )
}

/// The parameters `options` resolves to for an input of this size.
///
/// `src_size_hint` is what the caller *knows*: the real length for a one-shot
/// encode, `None` for a stream that has not been handed one. Upstream falls
/// back to the configured pledge only when the caller has nothing better
/// (`ZSTD_getCParamsFromCCtxParams`, `zstd_compress.c:1641`), and so does this.
pub(crate) fn compression_parameters_for_options(
    options: EncoderOptions,
    src_size_hint: Option<usize>,
    dictionary: Option<&Dictionary<'_>>,
) -> CompressionParameters {
    let src_size_hint = src_size_hint.or_else(|| {
        options
            .pledged_src_size
            .and_then(|pledged| usize::try_from(pledged).ok())
    });
    compression_parameters_with_overrides(
        options.compression_level,
        src_size_hint,
        options.parameters,
        dictionary,
    )
}

fn compression_parameters_with_overrides(
    level: CompressionLevel,
    src_size_hint: Option<usize>,
    overrides: ParameterOverrides,
    dictionary: Option<&Dictionary<'_>>,
) -> CompressionParameters {
    // A dictionary of no bytes is not a dictionary. C settles this before any
    // parameter is chosen: `ZSTD_CCtx_loadDictionary_advanced` clears the
    // dictionary slot and returns for `dictSize == 0`
    // (`zstd_compress.c:1293`), and `ZSTD_compressBegin_internal` gates the
    // CDict path on `cdict->dictContentSize > 0` (`:5255`). Either way the
    // row size is `srcSizeHint + 0`, which is the no-dictionary row.
    //
    // Dispatching on presence instead reached `upstream_full_dict_cparams_for
    // _level`, which skips the source-size adjustment a dictionary makes
    // unnecessary. On a 1074-byte source at level 13 that is chain_log 22
    // against 12, and btlazy2 against btultra: a whole different parser, and
    // every empty-dictionary frame measurably larger than the same bytes
    // encoded with no dictionary at all.
    let dictionary = dictionary.filter(|dictionary| dictionary.source_size() != 0);
    let dict_size = dictionary.map_or(0, Dictionary::source_size);
    let resolved = match dictionary {
        Some(_) => {
            // C creates a CDict for ALL dictionary types (including raw-content)
            // via ZSTD_initLocalDict → ZSTD_createCDict_advanced2. The CDict's
            // smaller hash/chain tables are then copied into the match state via
            // byCopyingCDict (params.cParams = *cdict_cParams; windowLog restored).
            upstream_full_dict_cparams_for_level(level, src_size_hint, dict_size, overrides)
        }
        None => {
            let applied = upstream_cparams_for_level(level, src_size_hint, dict_size, overrides);
            DictionaryCParams {
                applied,
                cdict: applied,
                requested: applied,
                use_row_match_finder: resolve_upstream_row_match_finder_mode(
                    overrides.use_row_match_finder,
                    applied,
                ),
            }
        }
    };
    let DictionaryCParams {
        applied: upstream_cparams,
        cdict: dict_cparams,
        requested: source_cparams,
        use_row_match_finder,
    } = resolved;
    let mut params =
        internal_compression_parameters_from_upstream(upstream_cparams, use_row_match_finder);
    // The dictionary's own tables are sized from the CDict's parameters, never
    // from the adjusted ones. Set whenever a dictionary is present, not only
    // when it attaches: the copying path (`ZSTD_resetCCtx_byCopyingCDict`)
    // leaves `applied` equal to the CDict's parameters anyway, so the two
    // agree there and the branch would only be a way to get it wrong.
    if let Some(dictionary) = dictionary {
        params.match_finder.dictionary_hash_bits = Some(dict_cparams.hash_log);
        params.match_finder.dictionary_chain_log = Some(dict_cparams.chain_log);
        params.match_finder.dictionary_window_log = Some(dict_cparams.window_log);
        // The prefix the match finders are given is `matching_content`, which
        // for a formatted dictionary starts before the content the decoder
        // gets. The difference is padding and no match may begin inside it.
        params.match_finder.prefix_low_limit = dictionary
            .matching_content()
            .len()
            .saturating_sub(dictionary.content().len());
    }
    // Whether C attaches the CDict or copies it. Attaching gives the dictionary
    // its own match state and a second search phase; copying folds it into one
    // unified extDict tree. Nothing but the dispatch depends on this.
    //
    // The *source* match state takes `applied` either way, which is why no
    // source-side parameter override survives here. C is explicit about it:
    // `ZSTD_resetCCtx_byAttachingCDict` writes the adjusted CDict parameters
    // into `params.cParams` under the comment "Resize working context table
    // params for input only, since the dict has its own tables"
    // (`zstd_compress.c:2337`), hands that to `ZSTD_resetCCtx_internal`, and
    // then asserts the working strategy *is* the CDict's (`:2353`). There is no
    // second parameter set for the source to be sized from.
    //
    // This crate carried one anyway, built from `requested` — the resolution
    // whose only surviving contribution in C is its `windowLog`. Its tier is
    // chosen from the source size with `dictSize` zeroed, so on a small source
    // it can land in a different table row from the CDict entirely, giving the
    // source tree a geometry C never builds.
    params.match_finder.dictionary_attaches = dictionary.is_some_and(|d| !d.is_raw_content())
        && should_attach_full_dictionary(src_size_hint, upstream_cparams.strategy);
    let _ = source_cparams;
    // A raw-content dictionary changes how a match is *scored* and how the
    // external-dictionary index is biased, and nothing about how hard to look:
    // `search_depth` stays whatever `search_log` asked for. It used to be
    // raised to a floor of 8 here, which had no counterpart in C —
    // `ZSTD_insertBt1` and the lazy searches take `1U << cParams->searchLog` as
    // given, and raw content reaches `ZSTD_loadDictionaryContent` by the same
    // route a full dictionary does. The floor was invisible at every level,
    // because no level pairs a `search_log` below 3 with a strategy that reads
    // `search_depth`, and it silently swallowed a caller's override that did.
    if dictionary.is_some_and(Dictionary::is_raw_content) {
        params.match_finder.source_score_penalty_with_prefix = 32;
        params.match_finder.ext_dict_index_bias = 2;
    }
    // Resolved last, and from the *adjusted* parameters: C settles this at
    // `zstd_compress.c:6378`, after `ZSTD_getCParamsFromCCtxParams` has fitted
    // the window to the source, and derives the table from the same fitted
    // values (`ZSTD_ldm_adjustParameters`, called at `:2126` with the applied
    // cParams). Reading the level's window instead would enable it on sources
    // far too small to reach past a block.
    //
    // [`LdmMode::Auto`] is deliberately *not* run through
    // [`resolve_enable_ldm`] here yet. The dictionary that used to block it no
    // longer does; what is left is that honouring the rule changes *default*
    // output, and only in one place -- level 22 above 64 MiB, the single level
    // whose window reaches the 27 the rule requires. Nothing in the suite
    // encodes a body that large, so turning it on would be a behaviour change
    // with no test able to see it either way. It wants the auto-boundary case
    // in Phase 3, which needs a corpus over 64 MiB behind the long-running
    // feature. The rule itself is implemented and tested against C.
    params.ldm = matches!(overrides.long_distance_matching, LdmMode::Enabled).then(|| {
        LdmParameters::resolve(
            overrides.ldm_overrides(),
            upstream_cparams.strategy,
            upstream_cparams.window_log,
        )
    });
    params.literal_compression = overrides.literal_compression;
    // C's `ZSTD_compressedLiterals`, which the optimal parser's price model
    // reads out of `optState_t` (`zstd_opt.c:77`). Note what it does *not* do:
    // it never resolves `auto`, so a mode left alone prices literals as coded
    // whatever the strategy says. [`CompressionParameters::
    // literals_compression_disabled`] is the other question and answers it
    // differently.
    params.match_finder.compressed_literals =
        overrides.literal_compression != LiteralCompressionMode::Disabled;
    params
}

/// Compression parameters for dictionary *training*, which derives them
/// differently from compression *with* a dictionary.
///
/// `ZDICT_analyzeEntropy` calls `ZSTD_getParams` and passes the result straight
/// to `ZSTD_createCDict_advanced`, which takes the parameters as given.
/// Compressing with a dictionary instead goes through `ZSTD_initLocalDict`,
/// which sizes a CDict to the dictionary and shrinks its hash and chain tables
/// to fit — which is what [`compression_parameters_for_input`] models.
///
/// The difference is not cosmetic. At level 3 against a 400-byte dictionary the
/// shrunk tables come out a bit smaller in both logs, find measurably fewer
/// matches, and leave more bytes as literals. Training on that parse would fit
/// the entropy tables to a search upstream never runs.
pub(crate) fn compression_parameters_for_dictionary_training(
    level: CompressionLevel,
    src_size_hint: Option<usize>,
    dictionary: &Dictionary<'_>,
) -> CompressionParameters {
    // No overrides: `ZDICT_analyzeEntropy` goes through `ZSTD_getParams`,
    // which takes a level and nothing else. A caller's `ParameterOverrides`
    // configure *their* encoder, and letting them reach the trainer would fit
    // the dictionary's entropy tables to a parse upstream's trainer never runs.
    let upstream_cparams = upstream_cparams_for_level(
        level,
        src_size_hint,
        dictionary.source_size(),
        ParameterOverrides::default(),
    );
    let use_row_match_finder =
        resolve_upstream_row_match_finder_mode(RowMatchFinderMode::Auto, upstream_cparams);
    let mut params =
        internal_compression_parameters_from_upstream(upstream_cparams, use_row_match_finder);
    // The same two raw-content adjustments the compression path makes, and the
    // same absence of a `search_depth` floor — see the note there.
    if dictionary.is_raw_content() {
        params.match_finder.source_score_penalty_with_prefix = 32;
        params.match_finder.ext_dict_index_bias = 2;
    }
    params
}

fn prefix_match_mode_for_dictionary(
    dictionary: &Dictionary<'_>,
    src_size_hint: Option<usize>,
    strategy: UpstreamStrategy,
) -> PrefixMatchMode {
    if dictionary.is_raw_content() {
        return match strategy {
            UpstreamStrategy::DoubleFast => PrefixMatchMode::DictMatchState,
            _ => PrefixMatchMode::ExtDict,
        };
    }

    if should_attach_full_dictionary(src_size_hint, strategy) {
        PrefixMatchMode::DictMatchState
    } else {
        PrefixMatchMode::ExtDict
    }
}

fn should_attach_full_dictionary(src_size_hint: Option<usize>, strategy: UpstreamStrategy) -> bool {
    let cutoff = match strategy {
        UpstreamStrategy::Fast => 8 * 1024,
        UpstreamStrategy::DoubleFast => 16 * 1024,
        UpstreamStrategy::Greedy
        | UpstreamStrategy::Lazy
        | UpstreamStrategy::Lazy2
        | UpstreamStrategy::BinaryTreeLazy2
        | UpstreamStrategy::BinaryTreeOpt => 32 * 1024,
        UpstreamStrategy::BinaryTreeUltra | UpstreamStrategy::BinaryTreeUltra2 => 8 * 1024,
    };

    src_size_hint.is_none_or(|src_size| src_size <= cutoff)
}

fn internal_compression_parameters_from_upstream(
    upstream_cparams: UpstreamCompressionParameters,
    use_row_match_finder: bool,
) -> CompressionParameters {
    // The parsers bound every offset they emit by this, so it is also the
    // `Window_Size` the frame declares and must stay inside what the reference
    // decoder accepts. See `MAX_DECLARABLE_WINDOW_SIZE`.
    let max_history_bytes = (1usize << upstream_cparams.window_log).min(MAX_DECLARABLE_WINDOW_SIZE);
    let parser_strategy =
        parser_strategy_from_upstream(upstream_cparams.strategy, use_row_match_finder);
    let search_depth = 1usize << upstream_cparams.search_log.min(10);
    let lazy_search_depth = match upstream_cparams.strategy {
        UpstreamStrategy::Fast | UpstreamStrategy::DoubleFast | UpstreamStrategy::Greedy => 0,
        UpstreamStrategy::Lazy => 1,
        UpstreamStrategy::Lazy2 | UpstreamStrategy::BinaryTreeLazy2 => 2,
        UpstreamStrategy::BinaryTreeOpt
        | UpstreamStrategy::BinaryTreeUltra
        | UpstreamStrategy::BinaryTreeUltra2 => 2,
    };
    let fast_search_step = match upstream_cparams.strategy {
        UpstreamStrategy::Fast => upstream_cparams.target_length.max(1) as usize + 1,
        UpstreamStrategy::DoubleFast => upstream_cparams.target_length.max(1) as usize,
        _ => 1,
    };
    let good_enough_match_length = match upstream_cparams.strategy {
        UpstreamStrategy::Fast => 24,
        UpstreamStrategy::DoubleFast => 32,
        UpstreamStrategy::Greedy | UpstreamStrategy::Lazy | UpstreamStrategy::Lazy2 => 64,
        UpstreamStrategy::BinaryTreeLazy2 => 64,
        UpstreamStrategy::BinaryTreeOpt => 80,
        UpstreamStrategy::BinaryTreeUltra | UpstreamStrategy::BinaryTreeUltra2 => 96,
    };
    let match_finder = MatchFinderParameters {
        parser_strategy,
        hash_bits: upstream_cparams.hash_log,
        chain_log: upstream_cparams.chain_log,
        secondary_hash_bits: match upstream_cparams.strategy {
            UpstreamStrategy::DoubleFast => upstream_cparams.chain_log,
            _ => upstream_cparams.chain_log.saturating_sub(1),
        },
        search_log: upstream_cparams.search_log,
        min_match: upstream_cparams.min_match,
        fast_search_step,
        search_depth,
        dictionary_search_depth: search_depth,
        source_score_penalty_with_prefix: 0,
        lazy_search_depth,
        skip_search_strength: 8,
        min_match_length_zero_literals: 4,
        min_match_length_after_literals: 4,
        good_enough_match_length,
        target_length: upstream_cparams.target_length as usize,
        ext_dict_index_bias: 0,
        // C: `if (optLevel < 2)` — btopt has optLevel=0, btultra/btultra2
        // have optLevel=2.  Only btopt penalizes long offsets (off_code >= 20).
        long_offset_penalty: matches!(upstream_cparams.strategy, UpstreamStrategy::BinaryTreeOpt),
        // Overridden by `compression_parameters_with_overrides`, the only
        // caller that has the mode. `true` is what `Auto` and `Enabled` both
        // resolve to.
        compressed_literals: true,
        window_log: upstream_cparams.window_log,
        dictionary_attaches: false,
        dictionary_hash_bits: None,
        dictionary_chain_log: None,
        dictionary_window_log: None,
        prefix_low_limit: 0,
    };

    CompressionParameters {
        match_finder,
        max_history_bytes,
        upstream_cparams,
        use_row_match_finder,
        // Filled in by `compression_parameters_with_overrides`, which is the
        // only place that has both the mode and the adjusted window.
        ldm: None,
        // Likewise the caller's, and likewise not visible from the upstream
        // parameters alone. `Auto` is what every level resolves to on its own,
        // so a path that never applies an override keeps the behaviour it had.
        literal_compression: LiteralCompressionMode::Auto,
    }
}

fn parser_strategy_from_upstream(
    strategy: UpstreamStrategy,
    use_row_match_finder: bool,
) -> ParserStrategy {
    match strategy {
        UpstreamStrategy::Fast => ParserStrategy::Fast,
        UpstreamStrategy::DoubleFast => ParserStrategy::DoubleFast,
        UpstreamStrategy::Greedy => {
            if use_row_match_finder {
                ParserStrategy::GreedyRow
            } else {
                ParserStrategy::Greedy
            }
        }
        UpstreamStrategy::Lazy => {
            if use_row_match_finder {
                ParserStrategy::LazyRow
            } else {
                ParserStrategy::Lazy
            }
        }
        UpstreamStrategy::Lazy2 => {
            if use_row_match_finder {
                ParserStrategy::Lazy2Row
            } else {
                ParserStrategy::Lazy2
            }
        }
        UpstreamStrategy::BinaryTreeLazy2 => ParserStrategy::BinaryTreeLazy2,
        UpstreamStrategy::BinaryTreeOpt => ParserStrategy::BinaryTreeOpt,
        UpstreamStrategy::BinaryTreeUltra | UpstreamStrategy::BinaryTreeUltra2 => {
            ParserStrategy::BinaryTreeUltra
        }
    }
}

fn upstream_cparams_for_level(
    level: CompressionLevel,
    src_size_hint: Option<usize>,
    dict_size: usize,
    overrides: ParameterOverrides,
) -> UpstreamCompressionParameters {
    upstream_cparams_for_level_with_mode(
        level,
        src_size_hint,
        dict_size,
        overrides,
        UpstreamCParamMode::NoAttachDict,
    )
}

/// Returns (applied_cparams, use_row_match_finder, source_cparams).
///
/// `applied_cparams` uses CDict-adjusted parameters (smaller hash/chain tables
/// appropriate for the dictionary size) with the source's window_log.  These
/// drive the dictionary BST and legacy non-opt binary-tree prefix_finder.
///
/// `source_cparams` carries the source-level parameters (larger hash/chain
/// tables, potentially deeper search) that C uses for the source match state
/// (`ms`).  The optimal parser uses these for the source BST.
///
/// `overrides` reach **both** sides. `EncoderDictionary` models
/// `ZSTD_CCtx_loadDictionary`, which builds its CDict through
/// `ZSTD_createCDict_advanced2` (`zstd_compress.c:1271`); that in turn calls
/// `ZSTD_getCParamsFromCCtxParams(..., ZSTD_cpm_createCDict)` (`:5680`), and
/// so applies `ZSTD_overrideCParams` to the CDict's parameters as well as the
/// source's. Applying them to only the requested side produces a frame that is
/// valid, decodes correctly, and silently differs from upstream.
/// The three parameter sets a dictionary compression resolves to. C keeps them
/// in three separate places and reads a different one at each stage; collapsing
/// any two of them is what item 1 of the handoff was.
struct DictionaryCParams {
    /// C's `cctx->appliedParams.cParams`: the CDict's parameters re-adjusted
    /// against the source, with the requested window put back over the top.
    /// Sizes the *source* match state and drives the parser.
    applied: UpstreamCompressionParameters,
    /// C's `cdict->matchState.cParams`, which attaching points
    /// `dictMatchState` at and never rewrites. Sizes the *dictionary's* tables
    /// and bounds the searches over them.
    cdict: UpstreamCompressionParameters,
    /// C's `ZSTD_getCParamsFromCCtxParams(...)` for this source and mode. Only
    /// its `window_log` survives into `applied`.
    requested: UpstreamCompressionParameters,
    use_row_match_finder: bool,
}

fn upstream_full_dict_cparams_for_level(
    level: CompressionLevel,
    src_size_hint: Option<usize>,
    dict_size: usize,
    overrides: ParameterOverrides,
) -> DictionaryCParams {
    // The CDict's own parameters come first, because whether the dictionary
    // attaches is decided from *its* strategy — `ZSTD_shouldAttachDict` indexes
    // `attachDictSizeCutoffs` by `cdict->matchState.cParams.strategy` — and the
    // answer picks the mode everything below resolves under.
    let cdict = upstream_cparams_for_level_with_mode(
        level,
        None,
        dict_size,
        overrides,
        UpstreamCParamMode::CreateCDict,
    );
    let attaches = should_attach_full_dictionary(src_size_hint, cdict.strategy);

    let requested = upstream_cparams_for_level_with_mode(
        level,
        src_size_hint,
        dict_size,
        overrides,
        if attaches {
            UpstreamCParamMode::AttachDict
        } else {
            UpstreamCParamMode::NoAttachDict
        },
    );
    // C: `ZSTD_resetCCtx_byAttachingCDict` re-adjusts the CDict's parameters
    // against the *source* before adopting them, while
    // `ZSTD_resetCCtx_byCopyingCDict` takes them as they stand. Both then put
    // the requested `windowLog` back over the top.
    let mut applied = if attaches {
        adjust_upstream_cparams(
            cdict,
            src_size_hint,
            dict_size,
            UpstreamCParamMode::AttachDict,
            overrides.use_row_match_finder,
        )
    } else {
        cdict
    };
    applied.window_log = requested.window_log;
    let use_row_match_finder =
        resolve_upstream_row_match_finder_mode(overrides.use_row_match_finder, cdict);
    DictionaryCParams {
        applied,
        cdict,
        requested,
        use_row_match_finder,
    }
}

/// Upstream's `ZSTD_getCParamsFromCCtxParams` (`zstd_compress.c:1637`), whose
/// order is load-bearing:
///
/// 1. the table row for this level and source-size tier, plus the negative
///    level's acceleration factor, and
/// 2. an adjustment pass to the source and dictionary — both inside
///    `ZSTD_getCParams_internal` (`:7759`);
/// 3. then the caller's overrides (`ZSTD_overrideCParams`, `:1647`);
/// 4. then a *second* adjustment pass (`:1650`).
///
/// The second pass is not redundant. Adjustment shrinks the window to the
/// source and then derives the hash and chain sizes from the window it
/// arrived at, so an override that lands between the two passes is re-fitted
/// rather than taken literally — asking for `window_log: 15` on a 200 KB
/// source also pulls `hash_log` down to 16 and `chain_log` to 15. An override
/// of `strategy` matters most: pass 1 sizes the chain using the *table's*
/// strategy, and only pass 2 sees the one the caller asked for.
///
/// Both passes run with row-match-finder resolution left on `auto`, which is
/// what a `ZSTD_CCtx` carries unless the caller sets `ZSTD_c_useRowMatchFinder`
/// — a parameter this crate does not expose. Upstream resolves it only *after*
/// this function returns (`:6379`).
///
/// Phase 3's LDM window force belongs between steps 2 and 3 (`:1646`).
fn upstream_cparams_for_level_with_mode(
    level: CompressionLevel,
    src_size_hint: Option<usize>,
    dict_size: usize,
    overrides: ParameterOverrides,
    mode: UpstreamCParamMode,
) -> UpstreamCompressionParameters {
    let tier = upstream_cparams_tier(src_size_hint, dict_size, mode);
    let mut cparams = upstream_cparams_table_entry(tier, upstream_cparams_row(level));
    if let Some(acceleration) = upstream_negative_level_acceleration(level) {
        cparams.target_length = acceleration;
    }
    // `ZSTD_ps_auto`, not the caller's mode: this pass is the one inside
    // `ZSTD_getCParams_internal`, which has no `ZSTD_CCtx_params` to read and
    // hardcodes it (`zstd_compress.c:7780`). Only the second pass below sees
    // what the caller asked for (`:1650`).
    let mut cparams = adjust_upstream_cparams(
        cparams,
        src_size_hint,
        dict_size,
        mode,
        RowMatchFinderMode::Auto,
    );
    // Between the two adjustments and before the overrides, which is where
    // `ZSTD_getCParamsFromCCtxParams` puts it (`zstd_compress.c:1646`). Sitting
    // before `ZSTD_overrideCParams` is what lets an explicit `window_log` beat
    // it, and sitting before the second adjustment is what lets a small source
    // shrink the forced window back down.
    cparams.window_log = force_window_log(overrides.long_distance_matching, cparams.window_log);
    overrides.apply_to(&mut cparams);
    adjust_upstream_cparams(
        cparams,
        src_size_hint,
        dict_size,
        mode,
        overrides.use_row_match_finder,
    )
}

/// Which row of the level table a level selects.
///
/// Matches `ZSTD_getCParams_internal`: level `0` means the default level,
/// every negative level shares row `0`, and positive levels index directly.
/// The clamp above `ZSTD_MAX_CLEVEL` is unreachable through
/// [`CompressionLevel::try_new`] but is kept so the mapping reads the same as
/// upstream's.
fn upstream_cparams_row(level: CompressionLevel) -> u8 {
    let level = level.as_i32();
    if level == 0 {
        UPSTREAM_CLEVEL_DEFAULT
    } else if level < 0 {
        0
    } else {
        (level as u32).min(CompressionLevel::MAX.as_i32() as u32) as u8
    }
}

/// The acceleration factor a negative level asks for, or `None` for level `0`
/// and above.
///
/// `ZSTD_getCParams_internal` overwrites `targetLength` with the level's
/// magnitude, clamped at `ZSTD_minCLevel()`. The fast parser reads that as its
/// step size, so a more negative level skips further between match attempts.
fn upstream_negative_level_acceleration(level: CompressionLevel) -> Option<u32> {
    let level = level.as_i32();
    if level >= 0 {
        return None;
    }
    let clamped = level.max(CompressionLevel::MIN.as_i32());
    Some(clamped.unsigned_abs())
}

fn upstream_cparams_tier(
    src_size_hint: Option<usize>,
    dict_size: usize,
    mode: UpstreamCParamMode,
) -> usize {
    let Some(row_size) = upstream_cparams_row_size(src_size_hint, dict_size, mode) else {
        return 0;
    };
    usize::from(row_size <= 256 * 1024)
        + usize::from(row_size <= 128 * 1024)
        + usize::from(row_size <= 16 * 1024)
}

fn upstream_cparams_row_size(
    src_size_hint: Option<usize>,
    dict_size: usize,
    mode: UpstreamCParamMode,
) -> Option<usize> {
    let dict_size = match mode {
        UpstreamCParamMode::NoAttachDict | UpstreamCParamMode::CreateCDict => dict_size,
        UpstreamCParamMode::AttachDict => 0,
    };
    match src_size_hint {
        Some(src_size) => Some(src_size.saturating_add(dict_size)),
        None if dict_size == 0 => None,
        None => Some(dict_size.saturating_add(UPSTREAM_UNKNOWN_SRC_SIZE_DICT_ADJUST)),
    }
}

/// Upstream's `ZSTD_defaultCParameters`, indexed by tier and by the row a
/// level selects (see [`upstream_cparams_row`]).
///
/// Row `0` is upstream's "base for negative levels". Every negative level
/// shares it and differs only in the `target_length` acceleration factor that
/// [`upstream_cparams_for_level_with_mode`] writes over it, which is why all
/// four row-`0` entries name `Fast`.
fn upstream_cparams_table_entry(tier: usize, row: u8) -> UpstreamCompressionParameters {
    match tier {
        0 => match row {
            0 => upstream_cparams(19, 12, 13, 1, 6, 1, UpstreamStrategy::Fast),
            1 => upstream_cparams(19, 13, 14, 1, 7, 0, UpstreamStrategy::Fast),
            2 => upstream_cparams(20, 15, 16, 1, 6, 0, UpstreamStrategy::Fast),
            3 => upstream_cparams(21, 16, 17, 1, 5, 0, UpstreamStrategy::DoubleFast),
            4 => upstream_cparams(21, 18, 18, 1, 5, 0, UpstreamStrategy::DoubleFast),
            5 => upstream_cparams(21, 18, 19, 3, 5, 2, UpstreamStrategy::Greedy),
            6 => upstream_cparams(21, 18, 19, 3, 5, 4, UpstreamStrategy::Lazy),
            7 => upstream_cparams(21, 19, 20, 4, 5, 8, UpstreamStrategy::Lazy),
            8 => upstream_cparams(21, 19, 20, 4, 5, 16, UpstreamStrategy::Lazy2),
            9 => upstream_cparams(22, 20, 21, 4, 5, 16, UpstreamStrategy::Lazy2),
            10 => upstream_cparams(22, 21, 22, 5, 5, 16, UpstreamStrategy::Lazy2),
            11 => upstream_cparams(22, 21, 22, 6, 5, 16, UpstreamStrategy::Lazy2),
            12 => upstream_cparams(22, 22, 23, 6, 5, 32, UpstreamStrategy::Lazy2),
            13 => upstream_cparams(22, 22, 22, 4, 5, 32, UpstreamStrategy::BinaryTreeLazy2),
            14 => upstream_cparams(22, 22, 23, 5, 5, 32, UpstreamStrategy::BinaryTreeLazy2),
            15 => upstream_cparams(22, 23, 23, 6, 5, 32, UpstreamStrategy::BinaryTreeLazy2),
            16 => upstream_cparams(22, 22, 22, 5, 5, 48, UpstreamStrategy::BinaryTreeOpt),
            17 => upstream_cparams(23, 23, 22, 5, 4, 64, UpstreamStrategy::BinaryTreeOpt),
            18 => upstream_cparams(23, 23, 22, 6, 3, 64, UpstreamStrategy::BinaryTreeUltra),
            19 => upstream_cparams(23, 24, 22, 7, 3, 256, UpstreamStrategy::BinaryTreeUltra2),
            20 => upstream_cparams(25, 25, 23, 7, 3, 256, UpstreamStrategy::BinaryTreeUltra2),
            21 => upstream_cparams(26, 26, 24, 7, 3, 512, UpstreamStrategy::BinaryTreeUltra2),
            22 => upstream_cparams(27, 27, 25, 9, 3, 999, UpstreamStrategy::BinaryTreeUltra2),
            _ => unreachable!(),
        },
        1 => match row {
            0 => upstream_cparams(18, 12, 13, 1, 5, 1, UpstreamStrategy::Fast),
            1 => upstream_cparams(18, 13, 14, 1, 6, 0, UpstreamStrategy::Fast),
            2 => upstream_cparams(18, 14, 14, 1, 5, 0, UpstreamStrategy::DoubleFast),
            3 => upstream_cparams(18, 16, 16, 1, 4, 0, UpstreamStrategy::DoubleFast),
            4 => upstream_cparams(18, 16, 17, 3, 5, 2, UpstreamStrategy::Greedy),
            5 => upstream_cparams(18, 17, 18, 5, 5, 2, UpstreamStrategy::Greedy),
            6 => upstream_cparams(18, 18, 19, 3, 5, 4, UpstreamStrategy::Lazy),
            7 => upstream_cparams(18, 18, 19, 4, 4, 4, UpstreamStrategy::Lazy),
            8 => upstream_cparams(18, 18, 19, 4, 4, 8, UpstreamStrategy::Lazy2),
            9 => upstream_cparams(18, 18, 19, 5, 4, 8, UpstreamStrategy::Lazy2),
            10 => upstream_cparams(18, 18, 19, 6, 4, 8, UpstreamStrategy::Lazy2),
            11 => upstream_cparams(18, 18, 19, 5, 4, 12, UpstreamStrategy::BinaryTreeLazy2),
            12 => upstream_cparams(18, 19, 19, 7, 4, 12, UpstreamStrategy::BinaryTreeLazy2),
            13 => upstream_cparams(18, 18, 19, 4, 4, 16, UpstreamStrategy::BinaryTreeOpt),
            14 => upstream_cparams(18, 18, 19, 4, 3, 32, UpstreamStrategy::BinaryTreeOpt),
            15 => upstream_cparams(18, 18, 19, 6, 3, 128, UpstreamStrategy::BinaryTreeOpt),
            16 => upstream_cparams(18, 19, 19, 6, 3, 128, UpstreamStrategy::BinaryTreeUltra),
            17 => upstream_cparams(18, 19, 19, 8, 3, 256, UpstreamStrategy::BinaryTreeUltra),
            18 => upstream_cparams(18, 19, 19, 6, 3, 128, UpstreamStrategy::BinaryTreeUltra2),
            19 => upstream_cparams(18, 19, 19, 8, 3, 256, UpstreamStrategy::BinaryTreeUltra2),
            20 => upstream_cparams(18, 19, 19, 10, 3, 512, UpstreamStrategy::BinaryTreeUltra2),
            21 => upstream_cparams(18, 19, 19, 12, 3, 512, UpstreamStrategy::BinaryTreeUltra2),
            22 => upstream_cparams(18, 19, 19, 13, 3, 999, UpstreamStrategy::BinaryTreeUltra2),
            _ => unreachable!(),
        },
        2 => match row {
            0 => upstream_cparams(17, 12, 12, 1, 5, 1, UpstreamStrategy::Fast),
            1 => upstream_cparams(17, 12, 13, 1, 6, 0, UpstreamStrategy::Fast),
            2 => upstream_cparams(17, 13, 15, 1, 5, 0, UpstreamStrategy::Fast),
            3 => upstream_cparams(17, 15, 16, 2, 5, 0, UpstreamStrategy::DoubleFast),
            4 => upstream_cparams(17, 17, 17, 2, 4, 0, UpstreamStrategy::DoubleFast),
            5 => upstream_cparams(17, 16, 17, 3, 4, 2, UpstreamStrategy::Greedy),
            6 => upstream_cparams(17, 16, 17, 3, 4, 4, UpstreamStrategy::Lazy),
            7 => upstream_cparams(17, 16, 17, 3, 4, 8, UpstreamStrategy::Lazy2),
            8 => upstream_cparams(17, 16, 17, 4, 4, 8, UpstreamStrategy::Lazy2),
            9 => upstream_cparams(17, 16, 17, 5, 4, 8, UpstreamStrategy::Lazy2),
            10 => upstream_cparams(17, 16, 17, 6, 4, 8, UpstreamStrategy::Lazy2),
            11 => upstream_cparams(17, 17, 17, 5, 4, 8, UpstreamStrategy::BinaryTreeLazy2),
            12 => upstream_cparams(17, 18, 17, 7, 4, 12, UpstreamStrategy::BinaryTreeLazy2),
            13 => upstream_cparams(17, 18, 17, 3, 4, 12, UpstreamStrategy::BinaryTreeOpt),
            14 => upstream_cparams(17, 18, 17, 4, 3, 32, UpstreamStrategy::BinaryTreeOpt),
            15 => upstream_cparams(17, 18, 17, 6, 3, 256, UpstreamStrategy::BinaryTreeOpt),
            16 => upstream_cparams(17, 18, 17, 6, 3, 128, UpstreamStrategy::BinaryTreeUltra),
            17 => upstream_cparams(17, 18, 17, 8, 3, 256, UpstreamStrategy::BinaryTreeUltra),
            18 => upstream_cparams(17, 18, 17, 10, 3, 512, UpstreamStrategy::BinaryTreeUltra),
            19 => upstream_cparams(17, 18, 17, 5, 3, 256, UpstreamStrategy::BinaryTreeUltra2),
            20 => upstream_cparams(17, 18, 17, 7, 3, 512, UpstreamStrategy::BinaryTreeUltra2),
            21 => upstream_cparams(17, 18, 17, 9, 3, 512, UpstreamStrategy::BinaryTreeUltra2),
            22 => upstream_cparams(17, 18, 17, 11, 3, 999, UpstreamStrategy::BinaryTreeUltra2),
            _ => unreachable!(),
        },
        3 => match row {
            0 => upstream_cparams(14, 12, 13, 1, 5, 1, UpstreamStrategy::Fast),
            1 => upstream_cparams(14, 14, 15, 1, 5, 0, UpstreamStrategy::Fast),
            2 => upstream_cparams(14, 14, 15, 1, 4, 0, UpstreamStrategy::Fast),
            3 => upstream_cparams(14, 14, 15, 2, 4, 0, UpstreamStrategy::DoubleFast),
            4 => upstream_cparams(14, 14, 14, 4, 4, 2, UpstreamStrategy::Greedy),
            5 => upstream_cparams(14, 14, 14, 3, 4, 4, UpstreamStrategy::Lazy),
            6 => upstream_cparams(14, 14, 14, 4, 4, 8, UpstreamStrategy::Lazy2),
            7 => upstream_cparams(14, 14, 14, 6, 4, 8, UpstreamStrategy::Lazy2),
            8 => upstream_cparams(14, 14, 14, 8, 4, 8, UpstreamStrategy::Lazy2),
            9 => upstream_cparams(14, 15, 14, 5, 4, 8, UpstreamStrategy::BinaryTreeLazy2),
            10 => upstream_cparams(14, 15, 14, 9, 4, 8, UpstreamStrategy::BinaryTreeLazy2),
            11 => upstream_cparams(14, 15, 14, 3, 4, 12, UpstreamStrategy::BinaryTreeOpt),
            12 => upstream_cparams(14, 15, 14, 4, 3, 24, UpstreamStrategy::BinaryTreeOpt),
            13 => upstream_cparams(14, 15, 14, 5, 3, 32, UpstreamStrategy::BinaryTreeUltra),
            14 => upstream_cparams(14, 15, 15, 6, 3, 64, UpstreamStrategy::BinaryTreeUltra),
            15 => upstream_cparams(14, 15, 15, 7, 3, 256, UpstreamStrategy::BinaryTreeUltra),
            16 => upstream_cparams(14, 15, 15, 5, 3, 48, UpstreamStrategy::BinaryTreeUltra2),
            17 => upstream_cparams(14, 15, 15, 6, 3, 128, UpstreamStrategy::BinaryTreeUltra2),
            18 => upstream_cparams(14, 15, 15, 7, 3, 256, UpstreamStrategy::BinaryTreeUltra2),
            19 => upstream_cparams(14, 15, 15, 8, 3, 256, UpstreamStrategy::BinaryTreeUltra2),
            20 => upstream_cparams(14, 15, 15, 8, 3, 512, UpstreamStrategy::BinaryTreeUltra2),
            21 => upstream_cparams(14, 15, 15, 9, 3, 512, UpstreamStrategy::BinaryTreeUltra2),
            22 => upstream_cparams(14, 15, 15, 10, 3, 999, UpstreamStrategy::BinaryTreeUltra2),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    }
}

fn adjust_upstream_cparams(
    mut cparams: UpstreamCompressionParameters,
    src_size_hint: Option<usize>,
    dict_size: usize,
    mode: UpstreamCParamMode,
    row_match_finder: RowMatchFinderMode,
) -> UpstreamCompressionParameters {
    let max_window_resize = 1u64 << (UPSTREAM_WINDOWLOG_MAX - 1);
    let src_size_hint = match mode {
        UpstreamCParamMode::NoAttachDict | UpstreamCParamMode::AttachDict => src_size_hint,
        UpstreamCParamMode::CreateCDict => {
            if dict_size == 0 || src_size_hint.is_some() {
                src_size_hint
            } else {
                Some(UPSTREAM_CREATE_CDICT_MIN_SRC_SIZE)
            }
        }
    };
    // C's `ZSTD_cpm_attachDict` arm, which zeroes `dictSize` here as well as in
    // the row-size step: the dictionary's own parameters are already chosen, so
    // both the window resize and `ZSTD_dictAndWindowLog` see the source alone.
    let dict_size = match mode {
        UpstreamCParamMode::NoAttachDict | UpstreamCParamMode::CreateCDict => dict_size,
        UpstreamCParamMode::AttachDict => 0,
    };

    if let Some(src_size) = src_size_hint {
        let src_size_u64 = src_size as u64;
        let dict_size_u64 = dict_size as u64;
        if src_size_u64 <= max_window_resize && dict_size_u64 <= max_window_resize {
            let total_size = src_size.saturating_add(dict_size) as u32;
            let hash_size_min = 1u32 << UPSTREAM_HASHLOG_MIN;
            let src_log = if total_size < hash_size_min {
                UPSTREAM_HASHLOG_MIN
            } else {
                highbit32_local(total_size - 1) + 1
            };
            if cparams.window_log > src_log {
                cparams.window_log = src_log;
            }
        }

        let dict_and_window_log =
            upstream_dict_and_window_log(cparams.window_log, src_size_u64, dict_size as u64);
        let cycle_log = cparams.chain_log - u32::from(cparams.strategy.is_binary_tree());
        if cparams.hash_log > dict_and_window_log + 1 {
            cparams.hash_log = dict_and_window_log + 1;
        }
        if cycle_log > dict_and_window_log {
            cparams.chain_log -= cycle_log - dict_and_window_log;
        }
    }

    if cparams.window_log < UPSTREAM_WINDOWLOG_ABSOLUTEMIN {
        cparams.window_log = UPSTREAM_WINDOWLOG_ABSOLUTEMIN;
    }

    if mode == UpstreamCParamMode::CreateCDict && upstream_cdict_indices_are_tagged(cparams) {
        let max_short_cache_hash_log = 32 - UPSTREAM_SHORT_CACHE_TAG_BITS;
        if cparams.hash_log > max_short_cache_hash_log {
            cparams.hash_log = max_short_cache_hash_log;
        }
        if cparams.chain_log > max_short_cache_hash_log {
            cparams.chain_log = max_short_cache_hash_log;
        }
    }

    // C does not know yet whether the row finder will run, so it assumes it
    // will unless explicitly told otherwise -- `if (useRowMatchFinder ==
    // ZSTD_ps_auto) useRowMatchFinder = ZSTD_ps_enable;` immediately above the
    // clamp (`zstd_compress.c:1592`), with the reasoning that the finder is
    // only ever auto-disabled for small sources, where a slightly smaller hash
    // costs nothing. So `Auto` clamps and only `Disabled` does not.
    if cparams.strategy.supports_row_match_finder()
        && row_match_finder != RowMatchFinderMode::Disabled
    {
        let row_log = cparams.search_log.clamp(4, 6);
        let max_row_hash_log = 32 - UPSTREAM_ROW_HASH_TAG_BITS;
        let max_hash_log = max_row_hash_log + row_log;
        if cparams.hash_log > max_hash_log {
            cparams.hash_log = max_hash_log;
        }
    }

    cparams
}

/// Whether the row match finder actually runs: C's `ZSTD_rowMatchFinderUsed`
/// applied to the output of `ZSTD_resolveRowMatchFinderMode`
/// (`zstd_compress.c:232` and `:238`).
///
/// The two are folded together because only the conjunction is observable
/// here. Resolution returns an explicit mode untouched -- C's comment is "if
/// requested enabled, but no SIMD, we still will use row matchfinder" -- and
/// `ZSTD_rowMatchFinderUsed` then re-tests the strategy, so a caller who forces
/// it on under `fast` gets a resolved mode of `enable` and no row finder. The
/// strategy test therefore leads here, ahead of the requested mode.
fn resolve_upstream_row_match_finder_mode(
    requested: RowMatchFinderMode,
    cparams: UpstreamCompressionParameters,
) -> bool {
    cparams.strategy.supports_row_match_finder()
        && match requested {
            RowMatchFinderMode::Enabled => true,
            RowMatchFinderMode::Disabled => false,
            RowMatchFinderMode::Auto => {
                cparams.window_log > UPSTREAM_ROW_MATCH_FINDER_WINDOWLOG_LOWER_BOUND
            }
        }
}

fn upstream_cdict_indices_are_tagged(cparams: UpstreamCompressionParameters) -> bool {
    matches!(
        cparams.strategy,
        UpstreamStrategy::Fast | UpstreamStrategy::DoubleFast
    )
}

fn upstream_dict_and_window_log(window_log: u32, src_size: u64, dict_size: u64) -> u32 {
    if dict_size == 0 {
        return window_log;
    }
    let window_size = 1u64 << window_log;
    if window_size >= dict_size + src_size {
        return window_log;
    }
    let dict_and_window_size = dict_size + window_size;
    let max_window_size = 1u64 << UPSTREAM_WINDOWLOG_MAX;
    if dict_and_window_size >= max_window_size {
        return UPSTREAM_WINDOWLOG_MAX;
    }
    highbit64(dict_and_window_size - 1) + 1
}

fn highbit64(value: u64) -> u32 {
    63 - value.leading_zeros()
}

fn highbit32_local(value: u32) -> u32 {
    31 - value.leading_zeros()
}

#[cfg(test)]
mod tests {
    // Relative paths, not `concat!(env!("CARGO_MANIFEST_DIR"), ..)`. The
    // absolute form worked here and still resolved to a file the published
    // crate does not contain, so `cargo test` on the crates.io tarball failed
    // to compile while `cargo package` reported success — it verifies a build,
    // never a test build. A relative path can only reach a file that ships
    // alongside this one, which is why `src/support/` exists.
    mod benchmark_corpora {
        #![allow(dead_code)]
        include!("support/corpora.rs");
    }
    mod upstream_trace_helper {
        #![allow(dead_code)]
        include!("support/upstream_zstd.rs");
    }

    use super::*;

    /// The Rust half of `oracles/cparams/compare.sh`: our *applied* compression
    /// parameters over the same grid the C oracle sweeps, as CSV on stdout.
    ///
    /// Ignored because it prints rather than asserts — the comparison lives in
    /// the shell script, which is the only place that has C's answer to diff
    /// against. Mirrors the dispatch in
    /// [`compression_parameters_with_overrides`] and nothing else, so a change
    /// to how a `Dictionary` is *parsed* cannot move these rows: the grid is
    /// about parameter resolution alone.
    #[test]
    #[ignore = "prints the grid for oracles/cparams/compare.sh"]
    fn print_cparams_grid() {
        const SRC_SIZES: [i64; 7] = [-1, 256, 1024, 32768, 262144, 2097152, 8388608];
        const DICT_SIZES: [usize; 4] = [0, 512, 16384, 114688];
        const LEVELS: [i32; 24] = [
            -5, -1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22,
        ];

        println!(
            "level,dict_size,src_hint,window_log,hash_log,chain_log,search_log,\
             min_match,target_length,strategy,attached,\
             dms_window_log,dms_hash_log,dms_chain_log,dms_search_log,dms_min_match,\
             dms_target_length,dms_strategy"
        );
        for level in LEVELS {
            let level = CompressionLevel::try_new(level).unwrap();
            for dict_size in DICT_SIZES {
                for src_hint in SRC_SIZES {
                    let hint = (src_hint >= 0).then_some(src_hint as usize);
                    let overrides = ParameterOverrides::default();
                    let (applied, cdict) = if dict_size == 0 {
                        let applied = upstream_cparams_for_level(level, hint, 0, overrides);
                        (applied, applied)
                    } else {
                        let resolved =
                            upstream_full_dict_cparams_for_level(level, hint, dict_size, overrides);
                        (resolved.applied, resolved.cdict)
                    };
                    // C's attach decision, minus the `is_raw_content` guard that
                    // has no counterpart in `ZSTD_shouldAttachDict`: the grid
                    // drives raw content on both sides, and C attaches it.
                    let attached =
                        dict_size > 0 && should_attach_full_dictionary(hint, applied.strategy);
                    // The dictionary match state only exists while the
                    // dictionary is attached, so its columns are zero
                    // otherwise — matching what the oracle prints for a null
                    // `dictMatchState`. Printing `cdict` unconditionally would
                    // compare a set of parameters C never materialises.
                    let dms = if attached {
                        format!(
                            "{},{},{},{},{},{},{}",
                            cdict.window_log,
                            cdict.hash_log,
                            cdict.chain_log,
                            cdict.search_log,
                            cdict.min_match,
                            cdict.target_length,
                            cdict.strategy.as_upstream_code(),
                        )
                    } else {
                        "0,0,0,0,0,0,0".to_string()
                    };
                    println!(
                        "{},{},{},{},{},{},{},{},{},{},{},{dms}",
                        level.as_i32(),
                        dict_size,
                        src_hint,
                        applied.window_log,
                        applied.hash_log,
                        applied.chain_log,
                        applied.search_log,
                        applied.min_match,
                        applied.target_length,
                        applied.strategy.as_upstream_code(),
                        u8::from(attached),
                    );
                }
            }
        }
    }

    use crate::sequence::{
        SequenceCommand, SequenceTablesState, TableTarget, decode_sequence_commands,
        parse_sequence_section,
    };
    use crate::window::{
        DoubleFastFinder, DoubleFastMatch, MIN_MATCH, NO_POS, NO_TAGGED_ENTRY,
        PreparedDictionaryMatchState, ROW_LAZY_TRACE_MAX_STEPS, SequenceTraceChainSearch,
        SequenceTraceEmission, SequenceTraceEmissionKind, SequenceTraceMatchSource,
        SequenceTraceRowLazyProbe, SequenceTraceRowLazyStopReason, SequenceTraceRowSearch,
        build_prepared_dictionary_match_state, count_match_length_with_prefix,
        debug_row_hash_for_params, explicit_offbase, extend_back_logical_match_with_min_start,
        hash_long_at, hash_short_cache_src_at_mls, logical_match_has_length, long_entry_pos,
        long_entry_with_pos, prefixed_offset_match_start, repeat_offsets12,
        store_lazy_regular_sequence_with_source, store_lazy_sequence, tagged_entry, tagged_index,
        tagged_pos,
    };

    #[derive(Debug)]
    struct PlannedBlockTrace {
        emitted_matches: Vec<BlockTraceEmittedMatch>,
        trace_emissions: Vec<SequenceTraceEmission>,
        trace_row_searches: Vec<SequenceTraceRowSearch>,
        trace_row_lazy_probes: Vec<SequenceTraceRowLazyProbe>,
        trace_chain_searches: Vec<SequenceTraceChainSearch>,
    }

    #[allow(dead_code)]
    #[derive(Debug)]
    struct MismatchWindow {
        mismatch_index: usize,
        rust_sequences: Vec<Option<BlockTraceEmittedMatch>>,
        upstream_sequences: Vec<Option<upstream_trace_helper::UpstreamSequenceTrace>>,
        rust_emissions: Vec<Option<SequenceTraceEmission>>,
        row_searches: Vec<SequenceTraceRowSearch>,
        chain_searches: Vec<SequenceTraceChainSearch>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct RustExtDictLazyProbe {
        anchor: usize,
        offset_1: usize,
        offset_2: usize,
        baseline_rep_length: usize,
        baseline_regular_source: upstream_trace_helper::UpstreamExtDictProbeSource,
        baseline_regular_length: usize,
        baseline_regular_off_base: usize,
        depth1_rep_length: usize,
        depth1_regular_source: upstream_trace_helper::UpstreamExtDictProbeSource,
        depth1_regular_length: usize,
        depth1_regular_off_base: usize,
        depth2_rep_length: usize,
        depth2_regular_source: upstream_trace_helper::UpstreamExtDictProbeSource,
        depth2_regular_length: usize,
        depth2_regular_off_base: usize,
        chosen_kind: upstream_trace_helper::UpstreamLazyProbeMatchKind,
        chosen_source: upstream_trace_helper::UpstreamExtDictProbeSource,
        chosen_start: usize,
        chosen_length: usize,
        chosen_off_base: usize,
        literal_length: usize,
        immediate_rep2_length: usize,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct RustNoDictRowLazyProbe {
        anchor: usize,
        offset_1: usize,
        offset_2: usize,
        baseline_rep_length: usize,
        baseline_regular_next_to_update: usize,
        baseline_regular_hash: usize,
        baseline_regular_rel_row: usize,
        baseline_regular_tag: usize,
        baseline_regular_low_limit: usize,
        baseline_regular_attempt_budget: usize,
        baseline_regular_head_index: usize,
        baseline_regular_insert_index: usize,
        baseline_regular_group_width: usize,
        baseline_regular_match_count: usize,
        baseline_regular_match_positions: [usize; 4],
        baseline_regular_match_indices: [usize; 4],
        baseline_regular_visit_count: usize,
        baseline_regular_visit_positions: [usize; 4],
        baseline_regular_visit_indices: [usize; 4],
        baseline_regular_visit_lengths: [usize; 4],
        baseline_regular_length: usize,
        baseline_regular_off_base: usize,
        depth1_rep_length: usize,
        depth1_regular_length: usize,
        depth1_regular_off_base: usize,
        depth2_rep_length: usize,
        depth2_regular_length: usize,
        depth2_regular_off_base: usize,
        chosen_kind: upstream_trace_helper::UpstreamLazyProbeMatchKind,
        chosen_start: usize,
        chosen_length: usize,
        chosen_off_base: usize,
        literal_length: usize,
        immediate_rep2_length: usize,
        continue_step_count: usize,
        continue_positions: [usize; ROW_LAZY_TRACE_MAX_STEPS],
        continue_rep_lengths: [usize; ROW_LAZY_TRACE_MAX_STEPS],
        continue_rep_improved: [bool; ROW_LAZY_TRACE_MAX_STEPS],
        continue_regular_lengths: [usize; ROW_LAZY_TRACE_MAX_STEPS],
        continue_regular_off_bases: [usize; ROW_LAZY_TRACE_MAX_STEPS],
        continue_regular_improved: [bool; ROW_LAZY_TRACE_MAX_STEPS],
        continue_current_kinds:
            [upstream_trace_helper::UpstreamLazyProbeMatchKind; ROW_LAZY_TRACE_MAX_STEPS],
        continue_current_starts: [usize; ROW_LAZY_TRACE_MAX_STEPS],
        continue_current_lengths: [usize; ROW_LAZY_TRACE_MAX_STEPS],
        continue_current_off_bases: [usize; ROW_LAZY_TRACE_MAX_STEPS],
        stop_reason: upstream_trace_helper::UpstreamNoDictRowLazyStopReason,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct RustNoDictRowSearchProbe {
        state_pos: usize,
        probe_pos: usize,
        next_to_update_before_search: usize,
        hash: usize,
        rel_row: usize,
        tag: usize,
        low_limit: usize,
        attempt_budget: usize,
        head_index: usize,
        insert_index: usize,
        group_width: usize,
        match_count: usize,
        match_positions: [usize; 4],
        match_indices: [usize; 4],
        visit_count: usize,
        visit_positions: [usize; 4],
        visit_indices: [usize; 4],
        visit_lengths: [usize; 4],
        visit_gate_passes: [bool; 4],
        visit_winner_lengths: [usize; 4],
        visit_winner_off_bases: [usize; 4],
        match_length: usize,
        off_base: usize,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    struct NoDictRowSearchVisitState {
        pos: usize,
        index: usize,
        gate_passed: bool,
        length: usize,
        winner_length: usize,
        winner_off_base: usize,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct NoDictRowSearchDecisionState {
        next_to_update_before_search: usize,
        low_limit: usize,
        attempt_budget: usize,
        head_index: usize,
        insert_index: usize,
        match_count: usize,
        match_positions: [usize; 4],
        match_indices: [usize; 4],
        visit_count: usize,
        visits: [NoDictRowSearchVisitState; 4],
        final_length: usize,
        final_off_base: usize,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct NoDictRowLazyDecisionState {
        baseline_kind: upstream_trace_helper::UpstreamLazyProbeMatchKind,
        baseline_length: usize,
        baseline_off_base: usize,
        depth1_rep_improves: bool,
        depth1_regular_improves: bool,
        depth2_rep_improves: bool,
        depth2_regular_improves: bool,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct NoDictRowLazyContinueStepState {
        pos: usize,
        rep_length: usize,
        rep_improved: bool,
        regular_length: usize,
        regular_off_base: usize,
        regular_improved: bool,
        current_kind: upstream_trace_helper::UpstreamLazyProbeMatchKind,
        current_start: usize,
        current_length: usize,
        current_off_base: usize,
    }

    impl Default for NoDictRowLazyContinueStepState {
        fn default() -> Self {
            Self {
                pos: 0,
                rep_length: 0,
                rep_improved: false,
                regular_length: 0,
                regular_off_base: 0,
                regular_improved: false,
                current_kind: upstream_trace_helper::UpstreamLazyProbeMatchKind::None,
                current_start: 0,
                current_length: 0,
                current_off_base: 0,
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct NoDictRowLazyContinueDecisionState {
        baseline_kind: upstream_trace_helper::UpstreamLazyProbeMatchKind,
        baseline_length: usize,
        baseline_off_base: usize,
        continue_step_count: usize,
        continue_steps: [NoDictRowLazyContinueStepState; ROW_LAZY_TRACE_MAX_STEPS],
        stop_reason: upstream_trace_helper::UpstreamNoDictRowLazyStopReason,
        chosen_kind: upstream_trace_helper::UpstreamLazyProbeMatchKind,
        chosen_start: usize,
        chosen_length: usize,
        chosen_off_base: usize,
    }

    fn mismatch_window(
        rust_sequences: &[BlockTraceEmittedMatch],
        upstream_sequences: &[upstream_trace_helper::UpstreamSequenceTrace],
        rust_emissions: &[SequenceTraceEmission],
        row_searches: &[SequenceTraceRowSearch],
        chain_searches: &[SequenceTraceChainSearch],
        mismatch_index: usize,
    ) -> MismatchWindow {
        let start = mismatch_index.saturating_sub(3);
        let end = (mismatch_index + 4)
            .min(rust_sequences.len())
            .min(upstream_sequences.len());
        let mismatch_anchor = rust_emissions
            .get(mismatch_index)
            .map(|emission| emission.anchor_before)
            .unwrap_or_default();
        let mismatch_end = rust_sequences
            .get(mismatch_index)
            .map(|sequence| sequence.start)
            .unwrap_or_default()
            .saturating_add(2);
        MismatchWindow {
            mismatch_index,
            rust_sequences: (start..end)
                .map(|index| rust_sequences.get(index).copied())
                .collect(),
            upstream_sequences: (start..end)
                .map(|index| upstream_sequences.get(index).copied())
                .collect(),
            rust_emissions: (start..end)
                .map(|index| rust_emissions.get(index).copied())
                .collect(),
            row_searches: row_searches
                .iter()
                .copied()
                .filter(|search| {
                    search.pos >= mismatch_anchor.saturating_sub(1) && search.pos <= mismatch_end
                })
                .collect(),
            chain_searches: chain_searches
                .iter()
                .copied()
                .filter(|search| {
                    search.anchor == mismatch_anchor
                        && search.pos >= mismatch_anchor
                        && search.pos <= mismatch_end
                })
                .collect(),
        }
    }

    fn upstream_extdict_source(
        source: SequenceTraceMatchSource,
    ) -> upstream_trace_helper::UpstreamExtDictProbeSource {
        match source {
            SequenceTraceMatchSource::Prefix => {
                upstream_trace_helper::UpstreamExtDictProbeSource::Prefix
            }
            // A long-distance match is a match into the frame's own history,
            // which is what `Source` names here.
            SequenceTraceMatchSource::Source | SequenceTraceMatchSource::LongDistance => {
                upstream_trace_helper::UpstreamExtDictProbeSource::Source
            }
            SequenceTraceMatchSource::Rep => upstream_trace_helper::UpstreamExtDictProbeSource::Rep,
            SequenceTraceMatchSource::Unknown | SequenceTraceMatchSource::Dict => {
                upstream_trace_helper::UpstreamExtDictProbeSource::None
            }
        }
    }

    fn upstream_lazy_probe_kind(
        kind: SequenceTraceEmissionKind,
    ) -> upstream_trace_helper::UpstreamLazyProbeMatchKind {
        match kind {
            SequenceTraceEmissionKind::Regular => {
                upstream_trace_helper::UpstreamLazyProbeMatchKind::Regular
            }
            SequenceTraceEmissionKind::Rep => {
                upstream_trace_helper::UpstreamLazyProbeMatchKind::Rep
            }
        }
    }

    fn upstream_row_lazy_stop_reason(
        stop_reason: SequenceTraceRowLazyStopReason,
    ) -> upstream_trace_helper::UpstreamNoDictRowLazyStopReason {
        match stop_reason {
            SequenceTraceRowLazyStopReason::None => {
                upstream_trace_helper::UpstreamNoDictRowLazyStopReason::None
            }
            SequenceTraceRowLazyStopReason::NoBaseline => {
                upstream_trace_helper::UpstreamNoDictRowLazyStopReason::NoBaseline
            }
            SequenceTraceRowLazyStopReason::Depth0 => {
                upstream_trace_helper::UpstreamNoDictRowLazyStopReason::Depth0
            }
            SequenceTraceRowLazyStopReason::Limit => {
                upstream_trace_helper::UpstreamNoDictRowLazyStopReason::Limit
            }
            SequenceTraceRowLazyStopReason::NoRegularImprove => {
                upstream_trace_helper::UpstreamNoDictRowLazyStopReason::NoRegularImprove
            }
        }
    }

    fn chain_regular_probe_details(
        search: SequenceTraceChainSearch,
    ) -> (
        upstream_trace_helper::UpstreamExtDictProbeSource,
        usize,
        usize,
    ) {
        match search.regular_winner {
            SequenceTraceMatchSource::Prefix => (
                upstream_trace_helper::UpstreamExtDictProbeSource::Prefix,
                search.dict_length,
                explicit_offbase(search.dict_offset) as usize,
            ),
            SequenceTraceMatchSource::Source => (
                upstream_trace_helper::UpstreamExtDictProbeSource::Source,
                search.source_length,
                explicit_offbase(search.source_offset) as usize,
            ),
            // `LongDistance` cannot win a row search: nothing searches for it,
            // its matches arrive already found.
            SequenceTraceMatchSource::Unknown
            | SequenceTraceMatchSource::Dict
            | SequenceTraceMatchSource::Rep
            | SequenceTraceMatchSource::LongDistance => (
                upstream_trace_helper::UpstreamExtDictProbeSource::None,
                search
                    .source_length
                    .max(search.dict_length)
                    .max(MIN_MATCH - 1),
                0,
            ),
        }
    }

    fn chain_search_group_for_pos(
        chain_searches: &[SequenceTraceChainSearch],
        pos: usize,
    ) -> Option<&[SequenceTraceChainSearch]> {
        let start = chain_searches
            .iter()
            .position(|search| search.probe_depth == 0 && search.pos == pos)?;
        let end = chain_searches[start + 1..]
            .iter()
            .position(|search| search.probe_depth == 0)
            .map(|offset| start + 1 + offset)
            .unwrap_or(chain_searches.len());
        Some(&chain_searches[start..end])
    }

    fn immediate_rep2_length_for_anchor(
        trace_emissions: &[SequenceTraceEmission],
        anchor: usize,
    ) -> usize {
        let Some(emission_index) = trace_emissions
            .iter()
            .position(|emission| emission.anchor_before == anchor)
        else {
            return 0;
        };
        let emission = trace_emissions[emission_index];
        let next_pos = emission.start.saturating_add(emission.match_length);
        trace_emissions
            .get(emission_index + 1)
            .filter(|next| {
                next.kind == SequenceTraceEmissionKind::Rep
                    && next.anchor_before == next_pos
                    && next.start == next_pos
                    && next.literal_length == 0
            })
            .map(|next| next.match_length)
            .unwrap_or(0)
    }

    fn rust_extdict_lazy_probe(
        trace_chain_searches: &[SequenceTraceChainSearch],
        trace_emissions: &[SequenceTraceEmission],
        pos: usize,
    ) -> Option<RustExtDictLazyProbe> {
        let group = chain_search_group_for_pos(trace_chain_searches, pos)?;
        let baseline = *group.first()?;
        let final_search = *group.last()?;
        let depth1 = group.iter().copied().find(|search| search.pos == pos + 1);
        let depth2 = group.iter().copied().find(|search| search.pos == pos + 2);
        let (baseline_regular_source, baseline_regular_length, baseline_regular_off_base) =
            chain_regular_probe_details(baseline);
        let (depth1_regular_source, depth1_regular_length, depth1_regular_off_base) =
            depth1.map(chain_regular_probe_details).unwrap_or((
                upstream_trace_helper::UpstreamExtDictProbeSource::None,
                0,
                0,
            ));
        let (depth2_regular_source, depth2_regular_length, depth2_regular_off_base) =
            depth2.map(chain_regular_probe_details).unwrap_or((
                upstream_trace_helper::UpstreamExtDictProbeSource::None,
                0,
                0,
            ));

        Some(RustExtDictLazyProbe {
            anchor: baseline.anchor,
            offset_1: baseline.offset_1,
            offset_2: baseline.offset_2,
            baseline_rep_length: baseline.rep_length,
            baseline_regular_source,
            baseline_regular_length,
            baseline_regular_off_base,
            depth1_rep_length: depth1.map(|search| search.rep_length).unwrap_or(0),
            depth1_regular_source,
            depth1_regular_length,
            depth1_regular_off_base,
            depth2_rep_length: depth2.map(|search| search.rep_length).unwrap_or(0),
            depth2_regular_source,
            depth2_regular_length,
            depth2_regular_off_base,
            chosen_kind: upstream_lazy_probe_kind(final_search.chosen_kind),
            chosen_source: upstream_extdict_source(final_search.chosen_source),
            chosen_start: final_search.chosen_start,
            chosen_length: final_search.chosen_length,
            chosen_off_base: final_search.chosen_offbase as usize,
            literal_length: final_search.chosen_start.saturating_sub(baseline.anchor),
            immediate_rep2_length: immediate_rep2_length_for_anchor(
                trace_emissions,
                baseline.anchor,
            ),
        })
    }

    fn hot_extdict_probe_positions(
        trace_chain_searches: &[SequenceTraceChainSearch],
        input_len: usize,
    ) -> Vec<usize> {
        let mut positions = trace_chain_searches
            .iter()
            .filter(|search| search.probe_depth == 0)
            .map(|search| search.pos)
            .filter(|&pos| pos + MIN_MATCH <= input_len)
            .collect::<Vec<_>>();
        positions.sort_unstable();
        positions.dedup();
        positions.truncate(16);
        positions
    }

    fn rust_no_dict_row_lazy_probe(
        trace_row_lazy_probes: &[SequenceTraceRowLazyProbe],
        trace_emissions: &[SequenceTraceEmission],
        pos: usize,
    ) -> Option<RustNoDictRowLazyProbe> {
        let probe = trace_row_lazy_probes
            .iter()
            .copied()
            .find(|probe| probe.pos == pos)?;
        let mut continue_current_kinds =
            [upstream_trace_helper::UpstreamLazyProbeMatchKind::None; ROW_LAZY_TRACE_MAX_STEPS];
        for (slot, kind) in continue_current_kinds
            .iter_mut()
            .zip(probe.continue_current_kinds)
            .take(probe.continue_step_count.min(ROW_LAZY_TRACE_MAX_STEPS))
        {
            *slot = upstream_lazy_probe_kind(kind);
        }
        Some(RustNoDictRowLazyProbe {
            anchor: probe.anchor,
            offset_1: probe.offset_1,
            offset_2: probe.offset_2,
            baseline_rep_length: probe.baseline_rep_length,
            baseline_regular_next_to_update: upstream_no_dict_first_block_index(
                probe.baseline_regular.next_to_update_before_search,
            ),
            baseline_regular_hash: probe.baseline_regular.hash,
            baseline_regular_rel_row: probe.baseline_regular.rel_row,
            baseline_regular_tag: probe.baseline_regular.tag as usize,
            baseline_regular_low_limit: upstream_no_dict_first_block_index(
                probe.baseline_regular.low_limit,
            ),
            baseline_regular_attempt_budget: probe.baseline_regular.attempt_budget,
            baseline_regular_head_index: probe.baseline_regular.head_index,
            baseline_regular_insert_index: probe.baseline_regular.insert_index,
            baseline_regular_group_width: probe.baseline_regular.group_width,
            baseline_regular_match_count: probe.baseline_regular.source_match_count,
            baseline_regular_match_positions: probe.baseline_regular.source_match_positions,
            baseline_regular_match_indices: upstream_no_dict_first_block_match_indices(
                probe.baseline_regular.source_match_indices,
                probe.baseline_regular.source_match_count,
            ),
            baseline_regular_visit_count: probe.baseline_regular.source_visit_count,
            baseline_regular_visit_positions: probe.baseline_regular.source_visit_positions,
            baseline_regular_visit_indices: upstream_no_dict_first_block_match_indices(
                probe.baseline_regular.source_visit_indices,
                probe.baseline_regular.source_visit_count,
            ),
            baseline_regular_visit_lengths: probe.baseline_regular.source_visit_lengths,
            baseline_regular_length: probe.baseline_regular.source_length.max(MIN_MATCH - 1),
            baseline_regular_off_base: if probe.baseline_regular.source_length >= MIN_MATCH {
                explicit_offbase(probe.baseline_regular.source_offset) as usize
            } else {
                0
            },
            depth1_rep_length: probe.depth1_rep_length,
            depth1_regular_length: probe.depth1_regular_length,
            depth1_regular_off_base: probe.depth1_regular_off_base,
            depth2_rep_length: probe.depth2_rep_length,
            depth2_regular_length: probe.depth2_regular_length,
            depth2_regular_off_base: probe.depth2_regular_off_base,
            chosen_kind: if probe.chosen_length >= MIN_MATCH {
                upstream_lazy_probe_kind(probe.chosen_kind)
            } else {
                upstream_trace_helper::UpstreamLazyProbeMatchKind::None
            },
            chosen_start: probe.chosen_start,
            chosen_length: probe.chosen_length,
            chosen_off_base: probe.chosen_off_base,
            literal_length: probe.literal_length,
            immediate_rep2_length: immediate_rep2_length_for_anchor(trace_emissions, probe.anchor),
            continue_step_count: probe.continue_step_count,
            continue_positions: probe.continue_positions,
            continue_rep_lengths: probe.continue_rep_lengths,
            continue_rep_improved: probe.continue_rep_improved,
            continue_regular_lengths: probe.continue_regular_lengths,
            continue_regular_off_bases: probe.continue_regular_off_bases,
            continue_regular_improved: probe.continue_regular_improved,
            continue_current_kinds,
            continue_current_starts: probe.continue_current_starts,
            continue_current_lengths: probe.continue_current_lengths,
            continue_current_off_bases: probe.continue_current_off_bases,
            stop_reason: upstream_row_lazy_stop_reason(probe.stop_reason),
        })
    }

    fn upstream_no_dict_first_block_index(index: usize) -> usize {
        index.saturating_add(2)
    }

    fn upstream_no_dict_first_block_match_index(index: usize) -> usize {
        upstream_no_dict_first_block_index(index)
    }

    fn upstream_no_dict_first_block_match_indices<const N: usize>(
        indices: [usize; N],
        count: usize,
    ) -> [usize; N] {
        let mut mapped = [0; N];
        for (slot, index) in mapped.iter_mut().zip(indices).take(count.min(N)) {
            *slot = upstream_no_dict_first_block_match_index(index);
        }
        mapped
    }

    fn rust_no_dict_row_search_probe(
        trace_row_searches: &[SequenceTraceRowSearch],
        pos: usize,
    ) -> Option<RustNoDictRowSearchProbe> {
        let search = trace_row_searches
            .iter()
            .copied()
            .find(|search| search.pos == pos)?;
        Some(RustNoDictRowSearchProbe {
            state_pos: pos.saturating_sub(1),
            probe_pos: pos,
            next_to_update_before_search: upstream_no_dict_first_block_index(
                search.next_to_update_before_search,
            ),
            hash: search.hash,
            rel_row: search.rel_row,
            tag: search.tag as usize,
            low_limit: upstream_no_dict_first_block_index(search.low_limit),
            attempt_budget: search.attempt_budget,
            head_index: search.head_index,
            insert_index: search.insert_index,
            group_width: search.group_width,
            match_count: search.source_match_count,
            match_positions: search.source_match_positions,
            match_indices: upstream_no_dict_first_block_match_indices(
                search.source_match_indices,
                search.source_match_count,
            ),
            visit_count: search.source_visit_count,
            visit_positions: search.source_visit_positions,
            visit_indices: upstream_no_dict_first_block_match_indices(
                search.source_visit_indices,
                search.source_visit_count,
            ),
            visit_lengths: search.source_visit_lengths,
            visit_gate_passes: search.source_visit_gate_passes,
            visit_winner_lengths: search.source_visit_winner_lengths,
            visit_winner_off_bases: search.source_visit_winner_off_bases,
            match_length: search.source_length.max(MIN_MATCH - 1),
            off_base: if search.source_length >= MIN_MATCH {
                explicit_offbase(search.source_offset) as usize
            } else {
                0
            },
        })
    }

    fn no_dict_row_search_decision_state(
        probe: RustNoDictRowSearchProbe,
    ) -> NoDictRowSearchDecisionState {
        let mut visits = [NoDictRowSearchVisitState::default(); 4];
        for index in 0..probe.visit_count.min(visits.len()) {
            visits[index] = NoDictRowSearchVisitState {
                pos: probe.visit_positions[index],
                index: probe.visit_indices[index],
                gate_passed: probe.visit_gate_passes[index],
                length: probe.visit_lengths[index],
                winner_length: probe.visit_winner_lengths[index],
                winner_off_base: probe.visit_winner_off_bases[index],
            };
        }
        NoDictRowSearchDecisionState {
            next_to_update_before_search: probe.next_to_update_before_search,
            low_limit: probe.low_limit,
            attempt_budget: probe.attempt_budget,
            head_index: probe.head_index,
            insert_index: probe.insert_index,
            match_count: probe.match_count,
            match_positions: probe.match_positions,
            match_indices: probe.match_indices,
            visit_count: probe.visit_count,
            visits,
            final_length: probe.match_length,
            final_off_base: probe.off_base,
        }
    }

    fn upstream_no_dict_row_search_decision_state(
        probe: upstream_trace_helper::UpstreamNoDictRowSearchProbe,
    ) -> NoDictRowSearchDecisionState {
        let mut visits = [NoDictRowSearchVisitState::default(); 4];
        for index in 0..probe.visit_count.min(visits.len()) {
            visits[index] = NoDictRowSearchVisitState {
                pos: probe.visit_positions[index],
                index: probe.visit_indices[index],
                gate_passed: probe.visit_gate_passes[index],
                length: probe.visit_lengths[index],
                winner_length: probe.visit_winner_lengths[index],
                winner_off_base: probe.visit_winner_off_bases[index],
            };
        }
        NoDictRowSearchDecisionState {
            next_to_update_before_search: probe.next_to_update_before_search,
            low_limit: probe.low_limit,
            attempt_budget: probe.attempt_budget,
            head_index: probe.head_index,
            insert_index: probe.insert_index,
            match_count: probe.match_count,
            match_positions: probe.match_positions,
            match_indices: probe.match_indices,
            visit_count: probe.visit_count,
            visits,
            final_length: probe.match_length,
            final_off_base: probe.off_base,
        }
    }

    fn hot_no_dict_row_probe_positions(
        trace_row_lazy_probes: &[SequenceTraceRowLazyProbe],
        emitted_matches: &[BlockTraceEmittedMatch],
        input_len: usize,
    ) -> Vec<usize> {
        let mut positions = trace_row_lazy_probes
            .iter()
            .filter(|probe| probe.chosen_length >= MIN_MATCH)
            .filter(|probe| {
                emitted_matches.iter().any(|sequence| {
                    sequence.kind
                        == match probe.chosen_kind {
                            SequenceTraceEmissionKind::Rep => BlockTraceEmittedMatchKind::Rep,
                            SequenceTraceEmissionKind::Regular => {
                                BlockTraceEmittedMatchKind::Regular
                            }
                        }
                        && sequence.start == probe.chosen_start
                        && sequence.length == probe.chosen_length
                        && sequence.off_base == probe.chosen_off_base
                })
            })
            .map(|probe| probe.pos)
            .filter(|&pos| pos + MIN_MATCH <= input_len)
            .collect::<Vec<_>>();
        positions.sort_unstable();
        positions.dedup();
        positions.truncate(16);
        positions
    }

    fn hot_no_dict_row_search_probe_positions(
        trace_row_searches: &[SequenceTraceRowSearch],
        trace_emissions: &[SequenceTraceEmission],
        input_len: usize,
    ) -> Vec<usize> {
        let mut positions = trace_row_searches
            .iter()
            .filter(|search| search.pos > 1 && search.pos + MIN_MATCH <= input_len)
            .filter(|search| {
                search.source_length >= MIN_MATCH
                    || trace_emissions.iter().any(|emission| {
                        emission.anchor_before <= search.pos && emission.start >= search.pos
                    })
            })
            .map(|search| search.pos)
            .collect::<Vec<_>>();
        positions.sort_unstable();
        positions.dedup();
        positions.truncate(24);
        positions
    }

    fn lazy_probe_repeat_improves(
        current_length: usize,
        current_off_base: usize,
        candidate_length: usize,
        probe_step: usize,
    ) -> bool {
        if candidate_length < MIN_MATCH {
            return false;
        }
        let multiplier = if probe_step == 1 { 3 } else { 4 };
        let gain2 = candidate_length as i32 * multiplier;
        let gain1 = current_length as i32 * multiplier
            - highbit32_local(current_off_base.min(u32::MAX as usize) as u32) as i32
            + 1;
        gain2 > gain1
    }

    fn lazy_probe_regular_improves(
        current_length: usize,
        current_off_base: usize,
        candidate_length: usize,
        candidate_off_base: usize,
        probe_step: usize,
    ) -> bool {
        if candidate_length < MIN_MATCH {
            return false;
        }
        let gain2 = candidate_length as i32 * 4
            - highbit32_local(candidate_off_base.min(u32::MAX as usize) as u32) as i32;
        let gain1 = current_length as i32 * 4
            - highbit32_local(current_off_base.min(u32::MAX as usize) as u32) as i32
            + if probe_step == 1 { 4 } else { 7 };
        gain2 > gain1
    }

    fn no_dict_row_lazy_decisions(
        depth: usize,
        baseline_rep_length: usize,
        baseline_regular_length: usize,
        baseline_regular_off_base: usize,
        depth1_rep_length: usize,
        depth1_regular_length: usize,
        depth1_regular_off_base: usize,
        depth2_rep_length: usize,
        depth2_regular_length: usize,
        depth2_regular_off_base: usize,
    ) -> Option<NoDictRowLazyDecisionState> {
        let baseline_rep_valid = baseline_rep_length >= MIN_MATCH;
        let baseline_regular_valid = baseline_regular_length >= MIN_MATCH;
        let mut baseline_kind = upstream_trace_helper::UpstreamLazyProbeMatchKind::None;
        let mut current_length = 0usize;
        let mut current_off_base = 0usize;

        if baseline_rep_valid {
            baseline_kind = upstream_trace_helper::UpstreamLazyProbeMatchKind::Rep;
            current_length = baseline_rep_length;
            current_off_base = 1;
        }
        if baseline_regular_valid
            && (!baseline_rep_valid || (depth != 0 && baseline_regular_length > current_length))
        {
            baseline_kind = upstream_trace_helper::UpstreamLazyProbeMatchKind::Regular;
            current_length = baseline_regular_length;
            current_off_base = baseline_regular_off_base;
        }
        if baseline_kind == upstream_trace_helper::UpstreamLazyProbeMatchKind::None {
            return None;
        }

        let mut decisions = NoDictRowLazyDecisionState {
            baseline_kind,
            baseline_length: current_length,
            baseline_off_base: current_off_base,
            depth1_rep_improves: false,
            depth1_regular_improves: false,
            depth2_rep_improves: false,
            depth2_regular_improves: false,
        };

        if depth >= 1 {
            if lazy_probe_repeat_improves(current_length, current_off_base, depth1_rep_length, 1) {
                decisions.depth1_rep_improves = true;
                current_length = depth1_rep_length;
                current_off_base = 1;
            }
            if lazy_probe_regular_improves(
                current_length,
                current_off_base,
                depth1_regular_length,
                depth1_regular_off_base,
                1,
            ) {
                decisions.depth1_regular_improves = true;
            } else if depth == 2 {
                if lazy_probe_repeat_improves(
                    current_length,
                    current_off_base,
                    depth2_rep_length,
                    2,
                ) {
                    decisions.depth2_rep_improves = true;
                    current_length = depth2_rep_length;
                    current_off_base = 1;
                }
                if lazy_probe_regular_improves(
                    current_length,
                    current_off_base,
                    depth2_regular_length,
                    depth2_regular_off_base,
                    2,
                ) {
                    decisions.depth2_regular_improves = true;
                }
            }
        }

        Some(decisions)
    }

    fn no_dict_row_lazy_continue_decision_state(
        probe: RustNoDictRowLazyProbe,
        depth: usize,
    ) -> NoDictRowLazyContinueDecisionState {
        let baseline = no_dict_row_lazy_decisions(
            depth,
            probe.baseline_rep_length,
            probe.baseline_regular_length,
            probe.baseline_regular_off_base,
            probe.depth1_rep_length,
            probe.depth1_regular_length,
            probe.depth1_regular_off_base,
            probe.depth2_rep_length,
            probe.depth2_regular_length,
            probe.depth2_regular_off_base,
        );
        let mut continue_steps =
            [NoDictRowLazyContinueStepState::default(); ROW_LAZY_TRACE_MAX_STEPS];
        for step in 0..probe.continue_step_count.min(ROW_LAZY_TRACE_MAX_STEPS) {
            continue_steps[step] = NoDictRowLazyContinueStepState {
                pos: probe.continue_positions[step],
                rep_length: probe.continue_rep_lengths[step],
                rep_improved: probe.continue_rep_improved[step],
                regular_length: probe.continue_regular_lengths[step],
                regular_off_base: probe.continue_regular_off_bases[step],
                regular_improved: probe.continue_regular_improved[step],
                current_kind: probe.continue_current_kinds[step],
                current_start: probe.continue_current_starts[step],
                current_length: probe.continue_current_lengths[step],
                current_off_base: probe.continue_current_off_bases[step],
            };
        }
        NoDictRowLazyContinueDecisionState {
            baseline_kind: baseline.map_or(
                upstream_trace_helper::UpstreamLazyProbeMatchKind::None,
                |state| state.baseline_kind,
            ),
            baseline_length: baseline.map_or(0, |state| state.baseline_length),
            baseline_off_base: baseline.map_or(0, |state| state.baseline_off_base),
            continue_step_count: probe.continue_step_count,
            continue_steps,
            stop_reason: probe.stop_reason,
            chosen_kind: probe.chosen_kind,
            chosen_start: probe.chosen_start,
            chosen_length: probe.chosen_length,
            chosen_off_base: probe.chosen_off_base,
        }
    }

    fn upstream_no_dict_row_lazy_continue_decision_state(
        probe: upstream_trace_helper::UpstreamNoDictRowLazyProbe,
    ) -> NoDictRowLazyContinueDecisionState {
        let baseline = no_dict_row_lazy_decisions(
            probe.depth,
            probe.baseline_rep_length,
            probe.baseline_regular_length,
            probe.baseline_regular_off_base,
            probe.depth1_rep_length,
            probe.depth1_regular_length,
            probe.depth1_regular_off_base,
            probe.depth2_rep_length,
            probe.depth2_regular_length,
            probe.depth2_regular_off_base,
        );
        let mut continue_steps =
            [NoDictRowLazyContinueStepState::default(); ROW_LAZY_TRACE_MAX_STEPS];
        for step in 0..probe
            .continue_step_count
            .min(upstream_trace_helper::UPSTREAM_NO_DICT_ROW_LAZY_TRACE_MAX_STEPS)
        {
            continue_steps[step] = NoDictRowLazyContinueStepState {
                pos: probe.continue_positions[step],
                rep_length: probe.continue_rep_lengths[step],
                rep_improved: probe.continue_rep_improved[step],
                regular_length: probe.continue_regular_lengths[step],
                regular_off_base: probe.continue_regular_off_bases[step],
                regular_improved: probe.continue_regular_improved[step],
                current_kind: probe.continue_current_kinds[step],
                current_start: probe.continue_current_starts[step],
                current_length: probe.continue_current_lengths[step],
                current_off_base: probe.continue_current_off_bases[step],
            };
        }
        NoDictRowLazyContinueDecisionState {
            baseline_kind: baseline.map_or(
                upstream_trace_helper::UpstreamLazyProbeMatchKind::None,
                |state| state.baseline_kind,
            ),
            baseline_length: baseline.map_or(0, |state| state.baseline_length),
            baseline_off_base: baseline.map_or(0, |state| state.baseline_off_base),
            continue_step_count: probe.continue_step_count,
            continue_steps,
            stop_reason: probe.stop_reason,
            chosen_kind: probe.chosen_kind,
            chosen_start: probe.chosen_start,
            chosen_length: probe.chosen_length,
            chosen_off_base: probe.chosen_off_base,
        }
    }

    fn upstream_cparams_from_helper(
        helper: upstream_trace_helper::UpstreamAppliedCParams,
    ) -> UpstreamCompressionParameters {
        UpstreamCompressionParameters {
            window_log: helper.window_log,
            chain_log: helper.chain_log,
            hash_log: helper.hash_log,
            search_log: helper.search_log,
            min_match: helper.min_match,
            target_length: helper.target_length,
            strategy: match helper.strategy {
                1 => UpstreamStrategy::Fast,
                2 => UpstreamStrategy::DoubleFast,
                3 => UpstreamStrategy::Greedy,
                4 => UpstreamStrategy::Lazy,
                5 => UpstreamStrategy::Lazy2,
                6 => UpstreamStrategy::BinaryTreeLazy2,
                7 => UpstreamStrategy::BinaryTreeOpt,
                8 => UpstreamStrategy::BinaryTreeUltra,
                9 => UpstreamStrategy::BinaryTreeUltra2,
                other => panic!("unexpected upstream strategy {other}"),
            },
        }
    }

    fn build_high_entropy_literals(len: usize) -> Vec<u8> {
        let mut literals = vec![0u8; len];
        for (chunk_index, chunk) in literals.chunks_mut(128).enumerate() {
            for (byte_index, byte) in chunk.iter_mut().enumerate() {
                *byte = (chunk_index as u8)
                    .wrapping_mul(37)
                    .wrapping_add(byte_index as u8);
            }
        }
        literals
    }

    fn collect_emitted_matches(
        plan: &SequencePlan,
        initial_repeat_offsets: RepeatOffsets,
        prefix_len: usize,
        dictionary_mode: BlockTraceDictionaryMode,
        start_offset: usize,
    ) -> Result<Vec<BlockTraceEmittedMatch>> {
        let mut emitted = Vec::with_capacity(plan.sequences.len());
        let mut repeat_offsets = initial_repeat_offsets;
        let mut current_pos = 0usize;

        for (index, sequence) in plan.sequences.iter().enumerate() {
            let raw_offset = repeat_offsets.resolve(sequence)?;
            let trace_source = plan
                .trace_match_sources
                .get(index)
                .copied()
                .unwrap_or(SequenceTraceMatchSource::Unknown);
            let trace_emission = plan.trace_emissions.get(index).copied().unwrap_or_default();
            let match_start = current_pos
                .checked_add(sequence.literal_length as usize)
                .and_then(|value| value.checked_add(start_offset))
                .ok_or(Error::OutputSizeOverflow)?;
            let kind = match block_trace_emitted_match_kind(trace_source) {
                BlockTraceEmittedMatchKind::Unknown => match trace_emission.kind {
                    SequenceTraceEmissionKind::Regular => BlockTraceEmittedMatchKind::Regular,
                    SequenceTraceEmissionKind::Rep => BlockTraceEmittedMatchKind::Rep,
                },
                known => known,
            };
            let source = match block_trace_emitted_match_source(
                trace_source,
                match_start,
                raw_offset as usize,
                prefix_len,
                dictionary_mode,
            ) {
                BlockTraceMatchSource::Unknown
                    if dictionary_mode == BlockTraceDictionaryMode::None =>
                {
                    BlockTraceMatchSource::Source
                }
                known => known,
            };
            emitted.push(BlockTraceEmittedMatch {
                kind,
                source,
                start: match_start,
                literal_length: sequence.literal_length as usize,
                length: sequence.match_length as usize,
                off_base: sequence.offset_value as usize,
                offset: raw_offset as usize,
            });
            current_pos = current_pos
                .checked_add(sequence.literal_length as usize)
                .and_then(|value| value.checked_add(sequence.match_length as usize))
                .ok_or(Error::OutputSizeOverflow)?;
        }

        Ok(emitted)
    }

    fn emitted_matches_from_commands(
        sequences: &[SequenceCommand],
        initial_repeat_offsets: RepeatOffsets,
        prefix_len: usize,
        dictionary_mode: BlockTraceDictionaryMode,
        start_offset: usize,
    ) -> Result<Vec<BlockTraceEmittedMatch>> {
        let mut emitted = Vec::with_capacity(sequences.len());
        let mut repeat_offsets = initial_repeat_offsets;
        let mut current_pos = 0usize;

        for sequence in sequences {
            let raw_offset = repeat_offsets.resolve(sequence)?;
            let match_start = current_pos
                .checked_add(sequence.literal_length as usize)
                .and_then(|value| value.checked_add(start_offset))
                .ok_or(Error::OutputSizeOverflow)?;
            emitted.push(BlockTraceEmittedMatch {
                kind: match classify_trace_repcode(sequence) {
                    TraceRepcodeKind::Explicit => BlockTraceEmittedMatchKind::Regular,
                    _ => BlockTraceEmittedMatchKind::Rep,
                },
                source: block_trace_rep_match_source(
                    match_start,
                    raw_offset as usize,
                    prefix_len,
                    dictionary_mode,
                ),
                start: match_start,
                literal_length: sequence.literal_length as usize,
                length: sequence.match_length as usize,
                off_base: sequence.offset_value as usize,
                offset: raw_offset as usize,
            });
            current_pos = current_pos
                .checked_add(sequence.literal_length as usize)
                .and_then(|value| value.checked_add(sequence.match_length as usize))
                .ok_or(Error::OutputSizeOverflow)?;
        }

        Ok(emitted)
    }

    fn planned_first_block_trace_with_prepared_dict(
        src: &[u8],
        dict: &EncoderDictionary<'_>,
        options: EncoderOptions,
    ) -> Result<PlannedBlockTrace> {
        validate_options(options)?;
        let dictionary = dict.as_inner();
        let params = compression_parameters_for_options(options, Some(src.len()), Some(dictionary));
        let repeat_offsets = dictionary.repeat_offsets();
        let matching_content = dictionary.matching_content();
        let prefix_len = matching_content.len();
        let dictionary_mode: BlockTraceDictionaryMode = prefix_match_mode_for_dictionary(
            dictionary,
            Some(src.len()),
            params.upstream_cparams.strategy,
        )
        .into();
        let prepared_match_state = dict.prepared_match_state(params.match_finder);
        let raw_size = src.len().min(options.block_size);
        let chunk = &src[..raw_size];
        let mut plan = SequencePlan::default();
        plan.enable_tracing();
        plan.opt_dict_price_seed = dictionary.optimal_price_seed();

        if !matching_content.is_empty() {
            let mut match_state = PrefixedBlockMatchState::new_with_prepared_match_state(
                matching_content,
                src.len(),
                params.match_finder,
                prefix_match_mode_for_dictionary(
                    dictionary,
                    Some(src.len()),
                    params.upstream_cparams.strategy,
                ),
                prepared_match_state.as_deref(),
            );
            plan_sequences_for_prefixed_contiguous_block_into(
                &mut plan,
                matching_content,
                chunk,
                0,
                repeat_offsets,
                params.match_finder,
                params.max_history_bytes,
                &mut match_state,
            )?;
        } else {
            // An empty dictionary is the contiguous case, so trace what the
            // encoder actually runs rather than the prefix planner.
            let mut match_state = ContiguousBlockMatchState::new(chunk.len(), params.match_finder);
            plan_sequences_for_contiguous_block_into(
                &mut plan,
                chunk,
                0,
                repeat_offsets,
                params.match_finder,
                params.max_history_bytes,
                &mut match_state,
            )?;
        }

        Ok(PlannedBlockTrace {
            emitted_matches: collect_emitted_matches(
                &plan,
                repeat_offsets,
                prefix_len,
                dictionary_mode,
                0,
            )?,
            trace_emissions: plan.trace_emissions.clone(),
            trace_row_searches: plan.trace_row_searches.clone(),
            trace_row_lazy_probes: plan.trace_row_lazy_probes.clone(),
            trace_chain_searches: plan.trace_chain_searches.clone(),
        })
    }

    fn planned_first_block_trace_without_dict(
        src: &[u8],
        options: EncoderOptions,
    ) -> Result<PlannedBlockTrace> {
        validate_options(options)?;
        let params = compression_parameters_for_options(options, Some(src.len()), None);
        let repeat_offsets = RepeatOffsets::default();
        let raw_size = src.len().min(options.block_size);
        let chunk = &src[..raw_size];
        let mut plan = SequencePlan::default();
        plan.enable_tracing();
        let mut match_state = ContiguousBlockMatchState::new(src.len(), params.match_finder);
        plan_sequences_for_contiguous_block_into(
            &mut plan,
            chunk,
            0,
            repeat_offsets,
            params.match_finder,
            params.max_history_bytes,
            &mut match_state,
        )?;
        Ok(PlannedBlockTrace {
            emitted_matches: collect_emitted_matches(
                &plan,
                repeat_offsets,
                0,
                BlockTraceDictionaryMode::None,
                0,
            )?,
            trace_emissions: plan.trace_emissions.clone(),
            trace_row_searches: plan.trace_row_searches.clone(),
            trace_row_lazy_probes: plan.trace_row_lazy_probes.clone(),
            trace_chain_searches: plan.trace_chain_searches.clone(),
        })
    }

    fn planned_block_trace_with_prepared_dict(
        src: &[u8],
        dict: &EncoderDictionary<'_>,
        options: EncoderOptions,
        block_index: usize,
    ) -> Result<PlannedBlockTrace> {
        validate_options(options)?;
        let dictionary = dict.as_inner();
        let params = compression_parameters_for_options(options, Some(src.len()), Some(dictionary));
        debug_assert!(
            params.ldm.is_none(),
            "this trace does not model long-distance matching and would not describe the encode"
        );
        let mut repeat_offsets = dictionary.repeat_offsets();
        let mut sequence_tables = dictionary.sequence_encoding_state();
        let mut literals_state = LiteralsEncodingState::new(Some(dictionary), params);
        let matching_content = dictionary.matching_content();
        let prefix_len = matching_content.len();
        let dictionary_mode: BlockTraceDictionaryMode = prefix_match_mode_for_dictionary(
            dictionary,
            Some(src.len()),
            params.upstream_cparams.strategy,
        )
        .into();
        let prepared_match_state = dict.prepared_match_state(params.match_finder);
        let mut scratch = EntropyEncodeScratch::default();
        scratch.planned_sequences.opt_dict_price_seed = dictionary.optimal_price_seed();
        let mut encoded = Vec::new();
        let mut block_start = 0usize;
        let mut current_block_index = 0usize;
        let mut savings = 0i64;

        if !matching_content.is_empty() {
            let mut match_state = PrefixedBlockMatchState::new_with_prepared_match_state(
                matching_content,
                src.len(),
                params.match_finder,
                prefix_match_mode_for_dictionary(
                    dictionary,
                    Some(src.len()),
                    params.upstream_cparams.strategy,
                ),
                prepared_match_state.as_deref(),
            );

            while block_start < src.len() {
                let block_size = upstream_optimal_block_size(
                    src,
                    block_start,
                    options.block_size,
                    params.upstream_cparams.strategy,
                    savings,
                );
                let block_end = block_start + block_size;
                if current_block_index == block_index {
                    let mut plan = SequencePlan::default();
                    plan.enable_tracing();
                    plan.opt_dict_price_seed = dictionary.optimal_price_seed();
                    plan_sequences_for_prefixed_contiguous_block_into(
                        &mut plan,
                        matching_content,
                        &src[..block_end],
                        block_start,
                        repeat_offsets,
                        params.match_finder,
                        params.max_history_bytes,
                        &mut match_state,
                    )?;
                    return Ok(PlannedBlockTrace {
                        emitted_matches: collect_emitted_matches(
                            &plan,
                            repeat_offsets,
                            prefix_len,
                            dictionary_mode,
                            block_start,
                        )?,
                        trace_emissions: plan.trace_emissions.clone(),
                        trace_row_searches: plan.trace_row_searches.clone(),
                        trace_row_lazy_probes: plan.trace_row_lazy_probes.clone(),
                        trace_chain_searches: plan.trace_chain_searches.clone(),
                    });
                }

                let encoded_start = encoded.len();
                encode_block_into_prefixed_contiguous(
                    &mut OutBuf::growable(&mut encoded),
                    matching_content,
                    src,
                    block_start,
                    block_end,
                    &mut match_state,
                    &mut literals_state,
                    block_end == src.len(),
                    &mut repeat_offsets,
                    &mut sequence_tables,
                    &mut scratch,
                    params,
                    // This helper plans each block itself, above, and does not
                    // run the long-distance matcher while doing so. Handing one
                    // to the encoder here would have it parse against matches
                    // the trace above never saw, so the two halves would
                    // describe different parses. Guarded rather than supported.
                    None,
                )?;
                savings += block_size as i64 - (encoded.len() - encoded_start) as i64;
                block_start = block_end;
                current_block_index += 1;
            }
        }

        Err(Error::InvalidParameter(
            "requested trained-dictionary block index exceeds the encoded frame",
        ))
    }

    fn decode_compressed_block_sequences_from_frame(
        frame: &[u8],
        dictionary: &EncoderDictionary<'_>,
        input_len: usize,
        options: EncoderOptions,
        block_index: usize,
    ) -> Result<Vec<BlockTraceEmittedMatch>> {
        let dictionary = dictionary.as_inner();
        let prefix_len = dictionary.matching_content().len();
        let dictionary_mode: BlockTraceDictionaryMode = prefix_match_mode_for_dictionary(
            dictionary,
            Some(input_len),
            compression_parameters_for_options(options, Some(input_len), Some(dictionary))
                .upstream_cparams
                .strategy,
        )
        .into();
        let mut literals_state = dictionary.literals_state();
        let mut sequence_tables = dictionary.sequence_tables();
        let mut repeat_offsets = dictionary.repeat_offsets();
        let header = match crate::parse_frame_header(frame).expect("frame header should parse") {
            crate::FrameHeader::Zstandard(header) => header,
            crate::FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
        };
        let mut block_header_offset = header.header_size;
        let mut current_block_index = 0usize;
        let mut block_start = 0usize;

        loop {
            let block = crate::parse_block_header(&frame[block_header_offset..])
                .expect("block header should parse");
            let payload_start = block_header_offset + BlockHeader::SIZE;
            let payload_end = payload_start + block.payload_size();
            let payload = &frame[payload_start..payload_end];

            if block.block_type == BlockType::Compressed {
                let (_, literals_size) = crate::literals::decode_literals_section(
                    payload,
                    &mut literals_state,
                    BLOCK_SIZE_MAX,
                )?;
                let sequence_section = &payload[literals_size..];
                let parsed = parse_sequence_section(
                    sequence_section,
                    &mut sequence_tables,
                    TableTarget::Both,
                )?;
                let sequences = decode_sequence_commands(&parsed, &sequence_tables)?;
                if current_block_index == block_index {
                    return emitted_matches_from_commands(
                        &sequences,
                        repeat_offsets,
                        prefix_len,
                        dictionary_mode,
                        block_start,
                    );
                }
                for sequence in &sequences {
                    let _ = repeat_offsets.resolve(sequence)?;
                }
            } else if current_block_index == block_index {
                return Ok(Vec::new());
            }

            block_start = ((current_block_index + 1) * options.block_size).min(input_len);
            if block.last_block {
                break;
            }
            block_header_offset = payload_end;
            current_block_index += 1;
        }

        Err(Error::InvalidParameter(
            "requested decoded block index exceeds the frame",
        ))
    }

    fn parse_first_block_literals_section(
        frame: &[u8],
    ) -> upstream_trace_helper::UpstreamFirstBlockLiterals {
        parse_first_block_sections(frame).literals
    }

    fn parse_first_block_sections(
        frame: &[u8],
    ) -> upstream_trace_helper::UpstreamFirstBlockSections {
        parse_compressed_block_sections(frame, 0)
    }

    fn parse_compressed_block_sections(
        frame: &[u8],
        block_index: usize,
    ) -> upstream_trace_helper::UpstreamFirstBlockSections {
        let header = match crate::parse_frame_header(frame).expect("frame header should parse") {
            crate::FrameHeader::Zstandard(header) => header,
            crate::FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
        };
        let mut block_header_offset = header.header_size;
        let mut remaining_block_index = block_index;
        let (block, payload_start, payload_end) = loop {
            let block = crate::parse_block_header(&frame[block_header_offset..])
                .expect("block header should parse");
            let payload_start = block_header_offset + BlockHeader::SIZE;
            let payload_end = payload_start + block.payload_size();
            if remaining_block_index == 0 {
                assert_eq!(block.block_type, BlockType::Compressed);
                break (block, payload_start, payload_end);
            }
            assert!(
                !block.last_block,
                "requested compressed block {block_index}, but frame ended after block {}",
                block_index - remaining_block_index,
            );
            block_header_offset = payload_end;
            remaining_block_index -= 1;
        };
        let payload = &frame[payload_start..payload_end];
        let header0 = payload[0];
        let block_type = match header0 & 0x3 {
            0 => upstream_trace_helper::UpstreamLiteralsBlockType::Raw,
            1 => upstream_trace_helper::UpstreamLiteralsBlockType::Rle,
            2 => upstream_trace_helper::UpstreamLiteralsBlockType::Compressed,
            3 => upstream_trace_helper::UpstreamLiteralsBlockType::Treeless,
            _ => unreachable!(),
        };
        let size_format = (header0 >> 2) & 0x3;
        let (header_size, regenerated_size, compressed_size) = match block_type {
            upstream_trace_helper::UpstreamLiteralsBlockType::Raw
            | upstream_trace_helper::UpstreamLiteralsBlockType::Rle => match size_format {
                0 | 2 => {
                    let size = (header0 >> 3) as usize;
                    (1, size, size)
                }
                1 => {
                    let size = ((payload[0] as usize) >> 4) | ((payload[1] as usize) << 4);
                    (2, size, size)
                }
                3 => {
                    let value = (payload[0] as usize)
                        | ((payload[1] as usize) << 8)
                        | ((payload[2] as usize) << 16);
                    let size = value >> 4;
                    (3, size, size)
                }
                _ => unreachable!(),
            },
            upstream_trace_helper::UpstreamLiteralsBlockType::Compressed
            | upstream_trace_helper::UpstreamLiteralsBlockType::Treeless => match size_format {
                0 | 1 => {
                    let value = (payload[0] as usize)
                        | ((payload[1] as usize) << 8)
                        | ((payload[2] as usize) << 16);
                    (3, (value >> 4) & 0x03ff, (value >> 14) & 0x03ff)
                }
                2 => {
                    let value = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]])
                        as usize;
                    (4, (value >> 4) & 0x3fff, (value >> 18) & 0x3fff)
                }
                3 => {
                    let value = (payload[0] as u64)
                        | ((payload[1] as u64) << 8)
                        | ((payload[2] as u64) << 16)
                        | ((payload[3] as u64) << 24)
                        | ((payload[4] as u64) << 32);
                    (
                        5,
                        ((value >> 4) & 0x3ffff) as usize,
                        ((value >> 22) & 0x3ffff) as usize,
                    )
                }
                _ => unreachable!(),
            },
        };
        let section = &payload[..header_size + compressed_size];
        let (huffman_table_mode, huffman_table_size) = match block_type {
            upstream_trace_helper::UpstreamLiteralsBlockType::Compressed => {
                let descriptor = payload[header_size];
                if descriptor >= 128 {
                    let highest_symbol = usize::from(descriptor - 127);
                    (
                        upstream_trace_helper::UpstreamHuffmanTableMode::Raw4BitWeights,
                        1 + highest_symbol.div_ceil(2),
                    )
                } else {
                    (
                        upstream_trace_helper::UpstreamHuffmanTableMode::FseCompressedWeights,
                        1 + descriptor as usize,
                    )
                }
            }
            upstream_trace_helper::UpstreamLiteralsBlockType::Treeless
            | upstream_trace_helper::UpstreamLiteralsBlockType::Raw
            | upstream_trace_helper::UpstreamLiteralsBlockType::Rle => {
                (upstream_trace_helper::UpstreamHuffmanTableMode::None, 0)
            }
        };
        let literals = upstream_trace_helper::UpstreamFirstBlockLiterals {
            block_type,
            section_size: section.len(),
            literals_header_size: header_size,
            regenerated_size,
            compressed_size,
            huffman_table_mode,
            huffman_table_size,
            payload_size: compressed_size.saturating_sub(huffman_table_size),
            section_bytes: section.to_vec(),
            section_prefix: section[..section.len().min(16)].to_vec(),
        };
        let sequence_section = &payload[literals.section_size..];
        let sequence_count = decode_sequence_count(sequence_section);
        let sequence_modes = parse_sequence_modes(sequence_section, sequence_count);

        upstream_trace_helper::UpstreamFirstBlockSections {
            last_block: block.last_block,
            payload_size: payload.len(),
            payload_bytes: payload.to_vec(),
            payload_prefix: payload[..payload.len().min(16)].to_vec(),
            literals,
            sequence_section_size: sequence_section.len(),
            sequence_section_bytes: sequence_section.to_vec(),
            sequence_section_prefix: sequence_section[..sequence_section.len().min(16)].to_vec(),
            sequence_count,
            sequence_modes,
        }
    }

    fn compressed_block_count(frame: &[u8]) -> usize {
        let header = match crate::parse_frame_header(frame).expect("frame header should parse") {
            crate::FrameHeader::Zstandard(header) => header,
            crate::FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
        };
        let mut count = 0usize;
        let mut block_header_offset = header.header_size;
        loop {
            let block = crate::parse_block_header(&frame[block_header_offset..])
                .expect("block header should parse");
            count += 1;
            block_header_offset += BlockHeader::SIZE + block.payload_size();
            if block.last_block {
                return count;
            }
        }
    }

    fn parse_sequence_modes(
        src: &[u8],
        sequence_count: usize,
    ) -> Option<upstream_trace_helper::UpstreamSequenceModes> {
        if sequence_count == 0 {
            return None;
        }

        let mode_index = if sequence_count < 128 {
            1
        } else if sequence_count < 0x7F00 {
            2
        } else {
            3
        };
        let modes = src[mode_index];
        Some(upstream_trace_helper::UpstreamSequenceModes {
            literal_lengths: (modes >> 6) & 0x3,
            offsets: (modes >> 4) & 0x3,
            match_lengths: (modes >> 2) & 0x3,
        })
    }

    fn decode_sequence_count(src: &[u8]) -> usize {
        let byte0 = src[0];
        if byte0 < 128 {
            byte0 as usize
        } else if byte0 < 255 {
            ((byte0 as usize - 128) << 8) + src[1] as usize
        } else {
            0x7F00 + src[1] as usize + ((src[2] as usize) << 8)
        }
    }

    #[derive(Debug)]
    struct DecodedCompressedBlockSequences {
        sequences: Vec<BlockTraceEmittedMatch>,
        final_repeat_offsets: RepeatOffsets,
    }

    fn decoded_compressed_block_sequences(
        frame: &[u8],
        block_index: usize,
        initial_repeat_offsets: RepeatOffsets,
    ) -> DecodedCompressedBlockSequences {
        let sections = parse_compressed_block_sections(frame, block_index);
        let mut sequence_tables = SequenceTablesState::default();
        let parsed = parse_sequence_section(
            &sections.sequence_section_bytes,
            &mut sequence_tables,
            TableTarget::Both,
        )
        .unwrap();
        let sequences = decode_sequence_commands(&parsed, &sequence_tables).unwrap();
        let mut repeat_offsets = initial_repeat_offsets;
        let emitted = emitted_matches_from_commands(
            &sequences,
            repeat_offsets,
            0,
            BlockTraceDictionaryMode::None,
            block_index * 128 * 1024,
        )
        .unwrap();
        for sequence in &sequences {
            let _ = repeat_offsets.resolve(sequence).unwrap();
        }
        DecodedCompressedBlockSequences {
            sequences: emitted,
            final_repeat_offsets: repeat_offsets,
        }
    }

    fn first_block_sections_summary(
        sections: &upstream_trace_helper::UpstreamFirstBlockSections,
    ) -> String {
        format!(
            "last_block={} payload={} payload_prefix={:02x?} literals=(type={:?} size={} header={} table={:?} table_size={} prefix={:02x?}) seqs=(count={} size={} modes={:?} prefix={:02x?})",
            sections.last_block,
            sections.payload_size,
            sections.payload_prefix,
            sections.literals.block_type,
            sections.literals.section_size,
            sections.literals.literals_header_size,
            sections.literals.huffman_table_mode,
            sections.literals.huffman_table_size,
            sections.literals.section_prefix,
            sections.sequence_count,
            sections.sequence_section_size,
            sections.sequence_modes,
            sections.sequence_section_prefix,
        )
    }

    fn regular_first_block_sequences_match_upstream(
        helper: &std::path::Path,
        level: u8,
        input: &[u8],
    ) -> bool {
        let (rust_trace, upstream_sequences) =
            regular_first_block_sequence_traces(helper, level, input);

        regular_first_block_sequence_first_mismatch(
            &rust_trace.emitted_matches,
            &upstream_sequences,
        )
        .is_none()
    }

    fn regular_first_block_sequence_traces(
        helper: &std::path::Path,
        level: u8,
        input: &[u8],
    ) -> (
        PlannedBlockTrace,
        Vec<upstream_trace_helper::UpstreamSequenceTrace>,
    ) {
        let options = EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
            ..Default::default()
        };
        let first_block_end = input.len().min(options.block_size);
        let rust_trace = planned_first_block_trace_without_dict(input, options).unwrap();
        let upstream_sequences = upstream_trace_helper::trace_regular_sequences(
            helper,
            i32::from(level),
            false,
            input.len(),
            input,
        )
        .into_iter()
        .take_while(|sequence| sequence.start < first_block_end)
        .collect();
        (rust_trace, upstream_sequences)
    }

    fn regular_first_block_sequence_first_mismatch(
        rust_sequences: &[BlockTraceEmittedMatch],
        upstream_sequences: &[upstream_trace_helper::UpstreamSequenceTrace],
    ) -> Option<usize> {
        let mismatch = rust_sequences
            .iter()
            .zip(upstream_sequences.iter())
            .enumerate()
            .find_map(|(index, (rust, upstream))| {
                (!regular_sequence_matches_upstream(*rust, *upstream)).then_some(index)
            });
        if mismatch.is_some() {
            return mismatch;
        }

        (rust_sequences.len() != upstream_sequences.len())
            .then_some(rust_sequences.len().min(upstream_sequences.len()))
    }

    fn emitted_sequence_first_mismatch(
        rust_sequences: &[BlockTraceEmittedMatch],
        upstream_sequences: &[BlockTraceEmittedMatch],
    ) -> Option<usize> {
        let mismatch = rust_sequences
            .iter()
            .zip(upstream_sequences.iter())
            .enumerate()
            .find_map(|(index, (rust, upstream))| (rust != upstream).then_some(index));
        if mismatch.is_some() {
            return mismatch;
        }

        (rust_sequences.len() != upstream_sequences.len())
            .then_some(rust_sequences.len().min(upstream_sequences.len()))
    }

    /// The one difference this comparator forgives: a match both sides found at
    /// the same place, with the same length and the same distance, which we
    /// encode as a repcode because that distance was still live in our repeat
    /// offsets and upstream encodes as an explicit offset because its lazy
    /// family never looks. Cheaper by an offset code and its extra bits; see
    /// "The repcode substitution" in `docs/PARITY_PLAN.md`.
    ///
    /// Deliberately stated as a conjunction of every field rather than as
    /// "kind or off_base differs", so that it cannot widen into a licence to
    /// disagree about the parse. The distances must be equal, upstream's
    /// `off_base` must be the explicit encoding of that distance, and ours must
    /// be one of the three repcodes. A real match-finder divergence -- a
    /// different position, length or distance -- fails as it always did.
    fn differs_only_by_repcode_substitution(
        rust: BlockTraceEmittedMatch,
        upstream: upstream_trace_helper::UpstreamSequenceTrace,
    ) -> bool {
        rust.kind == BlockTraceEmittedMatchKind::Rep
            && upstream.kind == upstream_trace_helper::UpstreamSequenceKind::Regular
            && rust.off_base >= 1
            && rust.off_base <= 3
            && upstream.off_base == upstream.raw_offset + 3
            && rust.offset == upstream.raw_offset
    }

    fn regular_sequence_matches_upstream(
        rust: BlockTraceEmittedMatch,
        upstream: upstream_trace_helper::UpstreamSequenceTrace,
    ) -> bool {
        let parse_matches = rust.source
            == match upstream.source {
                upstream_trace_helper::UpstreamSequenceSource::Dict => BlockTraceMatchSource::Dict,
                upstream_trace_helper::UpstreamSequenceSource::Prefix => {
                    BlockTraceMatchSource::Prefix
                }
                upstream_trace_helper::UpstreamSequenceSource::Source => {
                    BlockTraceMatchSource::Source
                }
            }
            && rust.start == upstream.start
            && rust.literal_length == upstream.literal_length
            && rust.length == upstream.match_length
            && rust.offset == upstream.raw_offset;
        if !parse_matches {
            return false;
        }

        let encoding_matches = rust.kind
            == match upstream.kind {
                upstream_trace_helper::UpstreamSequenceKind::Regular => {
                    BlockTraceEmittedMatchKind::Regular
                }
                upstream_trace_helper::UpstreamSequenceKind::Rep => BlockTraceEmittedMatchKind::Rep,
            }
            && rust.off_base == upstream.off_base;

        encoding_matches || differs_only_by_repcode_substitution(rust, upstream)
    }

    #[test]
    fn supported_levels_follow_upstream_backend_selection() {
        for level in 1..=2 {
            let level = CompressionLevel::try_new(level).unwrap();
            assert_eq!(
                level.parameters().match_finder.parser_strategy,
                ParserStrategy::Fast
            );
        }

        let level = CompressionLevel::try_new(3).unwrap();
        assert_eq!(
            level.parameters().match_finder.parser_strategy,
            ParserStrategy::DoubleFast
        );

        for (level, expected) in [
            (4, ParserStrategy::DoubleFast),
            (5, ParserStrategy::GreedyRow),
            (6, ParserStrategy::LazyRow),
            (7, ParserStrategy::LazyRow),
            (8, ParserStrategy::Lazy2Row),
            (9, ParserStrategy::Lazy2Row),
            (10, ParserStrategy::Lazy2Row),
            (11, ParserStrategy::Lazy2Row),
            (12, ParserStrategy::Lazy2Row),
            (13, ParserStrategy::BinaryTreeLazy2),
            (14, ParserStrategy::BinaryTreeLazy2),
            (15, ParserStrategy::BinaryTreeLazy2),
            (16, ParserStrategy::BinaryTreeOpt),
            (17, ParserStrategy::BinaryTreeOpt),
            (18, ParserStrategy::BinaryTreeUltra),
            (19, ParserStrategy::BinaryTreeUltra),
            (20, ParserStrategy::BinaryTreeUltra),
            (21, ParserStrategy::BinaryTreeUltra),
            (22, ParserStrategy::BinaryTreeUltra),
        ] {
            let level = CompressionLevel::try_new(level).unwrap();
            assert_eq!(
                level.parameters().match_finder.parser_strategy,
                expected,
                "level {}",
                level.as_i32()
            );
        }
    }

    #[test]
    fn level_five_and_six_reflect_upstream_greedy_and_lazy_row_backends() {
        let params = CompressionLevel::try_new(5).unwrap().parameters();
        let level_six = CompressionLevel::try_new(6).unwrap().parameters();

        assert_eq!(
            params.match_finder.parser_strategy,
            ParserStrategy::GreedyRow
        );
        assert_eq!(
            level_six.match_finder.parser_strategy,
            ParserStrategy::LazyRow
        );
        assert_eq!(params.match_finder.search_depth, 8);
        assert_eq!(params.match_finder.dictionary_search_depth, 8);
        assert_eq!(params.match_finder.lazy_search_depth, 0);
        assert_eq!(params.match_finder.min_match_length_zero_literals, 4);
        assert_eq!(params.match_finder.min_match_length_after_literals, 4);
        assert_eq!(params.max_history_bytes, 2 * 1024 * 1024);
        assert_eq!(level_six.match_finder.lazy_search_depth, 1);
    }

    /// The truth table of C's `ZSTD_rowMatchFinderUsed` composed with
    /// `ZSTD_resolveRowMatchFinderMode`, which is what
    /// [`resolve_upstream_row_match_finder_mode`] folds together.
    ///
    /// The asymmetry is the part worth pinning: an explicit mode is returned by
    /// the resolver untouched, so it escapes the *window* test, but
    /// `ZSTD_rowMatchFinderUsed` then re-applies the *strategy* test to it. So
    /// `Enabled` beats a narrow window and does not beat an unsupporting
    /// strategy.
    #[test]
    fn forcing_the_row_match_finder_escapes_the_window_but_not_the_strategy() {
        let at = |strategy, window_log| UpstreamCompressionParameters {
            strategy,
            window_log,
            ..upstream_cparams_table_entry(
                upstream_cparams_tier(None, 0, UpstreamCParamMode::NoAttachDict),
                upstream_cparams_row(CompressionLevel::try_new(5).unwrap()),
            )
        };
        let bound = UPSTREAM_ROW_MATCH_FINDER_WINDOWLOG_LOWER_BOUND;

        for (mode, wide, narrow) in [
            (RowMatchFinderMode::Auto, true, false),
            (RowMatchFinderMode::Enabled, true, true),
            (RowMatchFinderMode::Disabled, false, false),
        ] {
            assert_eq!(
                resolve_upstream_row_match_finder_mode(mode, at(UpstreamStrategy::Lazy, bound + 1)),
                wide,
                "{mode:?} above the window bound",
            );
            assert_eq!(
                resolve_upstream_row_match_finder_mode(mode, at(UpstreamStrategy::Lazy, bound)),
                narrow,
                "{mode:?} at the window bound, which is exclusive",
            );
            // No strategy outside greedy/lazy/lazy2 has a row parser, so every
            // mode is inert on one however wide the window.
            for strategy in [
                UpstreamStrategy::Fast,
                UpstreamStrategy::DoubleFast,
                UpstreamStrategy::BinaryTreeLazy2,
                UpstreamStrategy::BinaryTreeUltra,
            ] {
                assert!(
                    !resolve_upstream_row_match_finder_mode(mode, at(strategy, bound + 8)),
                    "{mode:?} reached the row finder under {strategy:?}",
                );
            }
        }
    }

    /// Upstream caps `hash_log` when the row finder is in play and treats
    /// `auto` as in-play for that decision, so only an explicit `Disabled`
    /// lets a larger hash through. Held on the unknown-source path, where no
    /// earlier clamp has already pulled `hash_log` below the cap.
    #[test]
    fn only_disabling_the_row_match_finder_lifts_the_row_hash_log_cap() {
        let level = CompressionLevel::try_new(5).unwrap();
        let requested = 30;
        let hashed = |mode| {
            upstream_cparams_for_level(
                level,
                None,
                0,
                ParameterOverrides {
                    hash_log: Some(requested),
                    search_log: Some(4),
                    strategy: Some(Strategy::Lazy),
                    use_row_match_finder: mode,
                    ..Default::default()
                },
            )
            .hash_log
        };

        let capped = 32 - UPSTREAM_ROW_HASH_TAG_BITS + 4;
        assert_eq!(hashed(RowMatchFinderMode::Auto), capped);
        assert_eq!(hashed(RowMatchFinderMode::Enabled), capped);
        assert_eq!(hashed(RowMatchFinderMode::Disabled), requested);
        assert!(
            capped < requested,
            "the cap has to bite for any of this to be a test",
        );
    }

    /// C's `ZSTD_literalsCompressionIsDisabled`, whose `auto` arm is the only
    /// one that reads the parameters.
    ///
    /// The accelerated `Fast` row is the shape a negative level resolves to and
    /// the one configuration `auto` answers "disabled" for; `Fast` with no
    /// acceleration is the neighbouring case that has to answer the other way,
    /// or the rule would be a strategy test wearing a target-length disguise.
    #[test]
    fn the_literals_mode_resolves_against_the_strategy_only_under_auto() {
        let resolved = |mode, strategy, target_length| {
            let mut params =
                compression_parameters_for_input(CompressionLevel::try_new(5).unwrap(), None, None);
            params.literal_compression = mode;
            params.upstream_cparams.strategy = strategy;
            params.upstream_cparams.target_length = target_length;
            params.literals_compression_disabled()
        };

        for (strategy, target_length, under_auto) in [
            (UpstreamStrategy::Fast, 4, true),
            (UpstreamStrategy::Fast, 0, false),
            (UpstreamStrategy::DoubleFast, 4, false),
            (UpstreamStrategy::BinaryTreeUltra, 999, false),
        ] {
            let case = format!("{strategy:?} at target length {target_length}");
            assert_eq!(
                resolved(LiteralCompressionMode::Auto, strategy, target_length),
                under_auto,
                "auto on {case}",
            );
            assert!(
                !resolved(LiteralCompressionMode::Enabled, strategy, target_length),
                "enabled was overruled on {case}",
            );
            assert!(
                resolved(LiteralCompressionMode::Disabled, strategy, target_length),
                "disabled was overruled on {case}",
            );
        }
    }

    /// The optimal parser's price model asks a different question of the same
    /// parameter -- C's `ZSTD_compressedLiterals`, which never resolves `auto`.
    ///
    /// Pinned separately from the rule above because the two agree on every
    /// configuration an optimal parse can reach, so a single test could not
    /// tell them apart and folding one into the other would go unnoticed.
    #[test]
    fn the_price_model_reads_the_mode_without_resolving_it() {
        let priced = |mode| {
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(19).unwrap(),
                parameters: ParameterOverrides {
                    literal_compression: mode,
                    // The one shape `auto` would call disabled. No optimal parse
                    // can reach it, which is exactly why it separates the two
                    // predicates.
                    strategy: Some(Strategy::Fast),
                    target_length: Some(4),
                    ..Default::default()
                },
                ..Default::default()
            };
            let params = compression_parameters_for_options(options, None, None);
            (
                params.match_finder.compressed_literals,
                params.literals_compression_disabled(),
            )
        };

        assert_eq!(priced(LiteralCompressionMode::Auto), (true, true));
        assert_eq!(priced(LiteralCompressionMode::Enabled), (true, false));
        assert_eq!(priced(LiteralCompressionMode::Disabled), (false, true));
    }

    #[test]
    fn supported_levels_select_upstream_cparams_by_source_size_tier() {
        let level_five = CompressionLevel::try_new(5).unwrap();

        assert_eq!(
            upstream_cparams_for_level(
                level_five,
                Some(512 * 1024),
                0,
                ParameterOverrides::default(),
            ),
            upstream_cparams(19, 18, 19, 3, 5, 2, UpstreamStrategy::Greedy)
        );
        assert_eq!(
            upstream_cparams_for_level(
                level_five,
                Some(128 * 1024),
                0,
                ParameterOverrides::default(),
            ),
            upstream_cparams(17, 16, 17, 3, 4, 2, UpstreamStrategy::Greedy)
        );
        assert_eq!(
            upstream_cparams_for_level(
                level_five,
                Some(8 * 1024),
                0,
                ParameterOverrides::default(),
            ),
            upstream_cparams(13, 13, 14, 3, 4, 4, UpstreamStrategy::Lazy)
        );
    }

    #[test]
    fn dictionary_size_contributes_to_upstream_tier_selection() {
        let level_six = CompressionLevel::try_new(6).unwrap();

        assert_eq!(
            upstream_cparams_for_level(
                level_six,
                Some(120 * 1024),
                16 * 1024,
                ParameterOverrides::default(),
            ),
            upstream_cparams(18, 18, 19, 3, 5, 4, UpstreamStrategy::Lazy)
        );
        assert_eq!(
            upstream_cparams_for_level(level_six, None, 8 * 1024, ParameterOverrides::default(),),
            upstream_cparams(14, 14, 14, 4, 4, 8, UpstreamStrategy::Lazy2)
        );
    }

    #[test]
    fn tier_two_no_dict_lazy_levels_match_upstream_hash_and_chain_logs() {
        let level_six = CompressionLevel::try_new(6).unwrap();
        let level_seven = CompressionLevel::try_new(7).unwrap();

        assert_eq!(
            upstream_cparams_for_level(
                level_six,
                Some(128 * 1024),
                0,
                ParameterOverrides::default(),
            ),
            upstream_cparams(17, 16, 17, 3, 4, 4, UpstreamStrategy::Lazy)
        );
        assert_eq!(
            upstream_cparams_for_level(
                level_seven,
                Some(128 * 1024),
                0,
                ParameterOverrides::default(),
            ),
            upstream_cparams(17, 16, 17, 3, 4, 8, UpstreamStrategy::Lazy2)
        );
    }

    #[test]
    fn tier_zero_no_dict_row_levels_match_upstream_hash_and_chain_logs() {
        let level_five = CompressionLevel::try_new(5).unwrap();
        let level_six = CompressionLevel::try_new(6).unwrap();
        let level_seven = CompressionLevel::try_new(7).unwrap();

        assert_eq!(
            upstream_cparams_for_level(
                level_five,
                Some(512 * 1024),
                0,
                ParameterOverrides::default(),
            ),
            upstream_cparams(19, 18, 19, 3, 5, 2, UpstreamStrategy::Greedy)
        );
        assert_eq!(
            upstream_cparams_for_level(
                level_six,
                Some(512 * 1024),
                0,
                ParameterOverrides::default(),
            ),
            upstream_cparams(19, 18, 19, 3, 5, 4, UpstreamStrategy::Lazy)
        );
        assert_eq!(
            upstream_cparams_for_level(
                level_seven,
                Some(512 * 1024),
                0,
                ParameterOverrides::default(),
            ),
            upstream_cparams(19, 19, 20, 4, 5, 8, UpstreamStrategy::Lazy)
        );
    }

    #[test]
    fn large_hash_chain_levels_track_upstream_hash_logs_and_row_match_usage() {
        let level_six = CompressionLevel::try_new(6).unwrap().parameters();
        let level_eight = CompressionLevel::try_new(8).unwrap().parameters();

        assert_eq!(level_six.upstream_cparams.hash_log, 19);
        assert!(level_six.use_row_match_finder);
        assert_eq!(level_six.match_finder.hash_bits, 19);

        assert_eq!(level_eight.upstream_cparams.hash_log, 20);
        assert!(level_eight.use_row_match_finder);
        assert_eq!(level_eight.match_finder.hash_bits, 20);
    }

    #[test]
    fn block_trace_carries_effective_upstream_cparams() {
        let src = vec![b'a'; 300 * 1024];
        let trace = trace_first_block_with_options(
            &src,
            EncoderOptions {
                block_size: 128 * 1024,
                checksum: false,
                write_dict_id: true,
                compression_level: CompressionLevel::try_new(5).unwrap(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(trace.compression_parameters.window_log, 19);
        assert_eq!(
            trace.compression_parameters.strategy,
            BlockTraceUpstreamStrategy::Greedy
        );
        assert_eq!(
            trace.compression_parameters.parser_strategy,
            BlockTraceParserStrategy::GreedyRow
        );
        assert!(trace.compression_parameters.use_row_match_finder);
        assert_eq!(
            trace.compression_parameters.dictionary_mode,
            BlockTraceDictionaryMode::None
        );
        assert!(!trace.compression_parameters.prepared_match_state);
        assert!(!trace.compression_parameters.chain_table_allocated);
        assert_eq!(trace.compression_parameters.row_hash_log, Some(15));
        assert_eq!(
            trace.compression_parameters.dict_table_source,
            BlockTraceDictionaryTableSource::None
        );
    }

    #[test]
    fn raw_dictionary_trace_uses_extdict_mode() {
        let src = b"customer=0012|region=us-east|tier=gold|invoice=42".repeat(64);
        let dictionary =
            EncoderDictionary::new(b"customer=0012|region=us-east|tier=gold|").unwrap();
        let trace = trace_first_block_with_prepared_dict_and_options(
            &src,
            &dictionary,
            EncoderOptions {
                block_size: 128 * 1024,
                checksum: false,
                write_dict_id: true,
                compression_level: CompressionLevel::try_new(1).unwrap(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            trace.compression_parameters.dictionary_mode,
            BlockTraceDictionaryMode::ExtDict
        );
        assert!(!trace.compression_parameters.prepared_match_state);
        assert_eq!(
            trace.compression_parameters.dict_table_source,
            BlockTraceDictionaryTableSource::Prefix
        );
    }

    #[test]
    fn planned_block_summary_keeps_zero_sequence_repetitive_literals_candidate() {
        let mut literals = Vec::with_capacity(16 * 1024);
        while literals.len() < 16 * 1024 {
            literals.extend_from_slice(b"GET /api/v1/orders?id=42&region=us-east\n");
        }
        literals.truncate(16 * 1024);
        let mut plan = SequencePlan {
            literals,
            sequences: Vec::new(),
            repeat_offsets: RepeatOffsets::default(),
            ..Default::default()
        };

        let summary =
            summarize_planned_block(&mut plan, &mut SequenceEncodeScratch::default()).unwrap();
        assert!(summary.should_try_zero_sequence_block());
    }

    /// C's rule from `ZSTD_entropyCompressSeqStore_internal`: a block the match
    /// finder found nothing in, or one where each sequence covers 20 literal
    /// bytes or more, is the shape incompressible input takes. Both sides of
    /// the boundary are pinned, because a rule that answered `Suspect` to
    /// everything would still pass a one-sided test and would then be deciding
    /// on a sample where C counts every byte.
    #[test]
    fn literals_compressibility_matches_upstreams_ratio() {
        assert_eq!(
            literals_compressibility(0, 0),
            huff0::Compressibility::Suspect
        );
        assert_eq!(
            literals_compressibility(4096, 0),
            huff0::Compressibility::Suspect
        );
        assert_eq!(
            literals_compressibility(19 * 8, 8),
            huff0::Compressibility::Unknown
        );
        assert_eq!(
            literals_compressibility(20 * 8, 8),
            huff0::Compressibility::Suspect
        );
    }

    #[test]
    fn literals_compression_threshold_matches_upstream_for_current_strategies() {
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
        ] {
            assert_eq!(
                minimum_literals_to_compress(strategy, LiteralsRepeatMode::None),
                64
            );
            assert_eq!(
                minimum_literals_to_compress(strategy, LiteralsRepeatMode::Check),
                64
            );
            assert_eq!(
                minimum_literals_to_compress(strategy, LiteralsRepeatMode::Valid),
                6
            );
        }

        assert_eq!(
            minimum_literals_to_compress(ParserStrategy::BinaryTreeOpt, LiteralsRepeatMode::None),
            32
        );
        assert_eq!(
            minimum_literals_to_compress(ParserStrategy::BinaryTreeUltra, LiteralsRepeatMode::None),
            16
        );
    }

    #[test]
    fn compressed_literals_layout_matches_upstream_single_stream_rules() {
        let small = compressed_literals_layout(200, LiteralsRepeatMode::None);
        assert_eq!(small.header_size, 3);
        assert_eq!(small.size_format, 0);
        assert!(small.single_stream);

        let small_check = compressed_literals_layout(200, LiteralsRepeatMode::Check);
        assert_eq!(small_check.size_format, 0);
        assert!(small_check.single_stream);

        let mid = compressed_literals_layout(400, LiteralsRepeatMode::None);
        assert_eq!(mid.header_size, 3);
        assert_eq!(mid.size_format, 1);
        assert!(!mid.single_stream);

        let mid_repeat = compressed_literals_layout(400, LiteralsRepeatMode::Valid);
        assert_eq!(mid_repeat.header_size, 3);
        assert_eq!(mid_repeat.size_format, 0);
        assert!(mid_repeat.single_stream);

        let large = compressed_literals_layout(2 * 1024, LiteralsRepeatMode::Valid);
        assert_eq!(large.header_size, 4);
        assert_eq!(large.size_format, 2);
        assert!(!large.single_stream);
    }

    #[test]
    fn zero_sequence_blocks_skip_small_literals_without_repeat_table() {
        let mut src = Vec::new();
        while src.len() < 63 {
            src.extend_from_slice(b"status=ok service=edge\n");
        }
        src.truncate(63);

        let mut huffman_dst = Vec::new();
        let mut huf_workspace = huff0::CompressWorkspace::default();
        let compressed = encode_zero_sequence_compressed_block_owned(
            &src,
            &LiteralsEncodingState::default(),
            ParserStrategy::Fast,
            &mut huffman_dst,
            &mut huf_workspace,
        )
        .unwrap();

        assert!(compressed.is_none());
    }

    #[test]
    fn compressed_literals_require_upstream_minimum_gain() {
        for strategy in [
            ParserStrategy::Fast,
            ParserStrategy::DoubleFast,
            ParserStrategy::Lazy2,
            ParserStrategy::Lazy2Row,
        ] {
            assert!(compressed_literals_clear_minimum_gain(64, 60, strategy));
            assert!(!compressed_literals_clear_minimum_gain(64, 61, strategy));
            assert!(compressed_literals_clear_minimum_gain(128, 123, strategy));
            assert!(!compressed_literals_clear_minimum_gain(128, 124, strategy));
        }

        assert!(compressed_literals_clear_minimum_gain(
            128,
            124,
            ParserStrategy::BinaryTreeUltra
        ));
        assert!(!compressed_literals_clear_minimum_gain(
            128,
            125,
            ParserStrategy::BinaryTreeUltra
        ));
    }

    #[test]
    fn legacy_sequence_header_bug_guard_matches_upstream_rule() {
        assert!(!sequence_section_triggers_legacy_decoder_bug(0, 1));
        assert!(sequence_section_triggers_legacy_decoder_bug(2, 1));
        assert!(!sequence_section_triggers_legacy_decoder_bug(3, 1));
        assert!(!sequence_section_triggers_legacy_decoder_bug(2, 2));
    }

    #[test]
    fn block_trace_parser_stats_classify_repeat_and_explicit_offsets() {
        let plan = SequencePlan {
            literals: vec![0; 11],
            sequences: vec![
                SequenceCommand {
                    literal_length: 2,
                    offset_value: 1,
                    match_length: 4,
                },
                SequenceCommand {
                    literal_length: 0,
                    offset_value: 1,
                    match_length: 5,
                },
                SequenceCommand {
                    literal_length: 3,
                    offset_value: 2,
                    match_length: 6,
                },
                SequenceCommand {
                    literal_length: 0,
                    offset_value: 2,
                    match_length: 7,
                },
                SequenceCommand {
                    literal_length: 1,
                    offset_value: 3,
                    match_length: 8,
                },
                SequenceCommand {
                    literal_length: 0,
                    offset_value: 3,
                    match_length: 9,
                },
                SequenceCommand {
                    literal_length: 5,
                    offset_value: 9,
                    match_length: 10,
                },
            ],
            trace_match_sources: vec![
                SequenceTraceMatchSource::Rep,
                SequenceTraceMatchSource::Rep,
                SequenceTraceMatchSource::Rep,
                SequenceTraceMatchSource::Rep,
                SequenceTraceMatchSource::Rep,
                SequenceTraceMatchSource::Rep,
                SequenceTraceMatchSource::Source,
            ],
            ..Default::default()
        };

        let stats = block_trace_parser_stats(
            &plan,
            RepeatOffsets::default(),
            0,
            BlockTraceDictionaryMode::None,
        )
        .unwrap();

        assert_eq!(stats.literal_bytes, 11);
        assert_eq!(stats.matched_bytes, 49);
        assert_eq!(
            stats.repcodes,
            BlockTraceRepcodeStats {
                rep1: 1,
                rep2: 2,
                rep3: 2,
                rep1_minus1: 1,
                explicit_offsets: 1,
            }
        );
        assert_eq!(
            stats.regular_match_sources,
            BlockTraceRegularMatchSourceCounts {
                dict: 0,
                prefix: 0,
                source: 1,
                unknown: 0,
            }
        );
        assert_eq!(
            stats.rep_match_sources,
            BlockTraceRepMatchSourceCounts {
                dict: 0,
                prefix: 0,
                source: 6,
                unknown: 0,
            }
        );
        assert_eq!(stats.explicit_offset_sum, 6);
        assert_eq!(stats.explicit_offset_count, 1);
        assert_eq!(stats.first_match_source, Some(BlockTraceMatchSource::Rep));
        assert_eq!(stats.offset_code_counts[0], 2);
        assert_eq!(stats.offset_code_counts[1], 4);
        assert_eq!(stats.offset_code_counts[3], 1);
        assert_eq!(stats.first_row_search_contest, None);
        assert_eq!(
            stats.first_emitted_match,
            Some(BlockTraceEmittedMatch {
                kind: BlockTraceEmittedMatchKind::Rep,
                source: BlockTraceMatchSource::Source,
                start: 2,
                literal_length: 2,
                length: 4,
                off_base: 1,
                offset: 1,
            })
        );
        assert_eq!(
            stats.second_emitted_match,
            Some(BlockTraceEmittedMatch {
                kind: BlockTraceEmittedMatchKind::Rep,
                source: BlockTraceMatchSource::Source,
                start: 6,
                literal_length: 0,
                length: 5,
                off_base: 1,
                offset: 4,
            })
        );
        assert_eq!(
            stats.first_accepted_regular_match,
            Some(BlockTraceAcceptedRegularMatch {
                source: BlockTraceMatchSource::Source,
                start: 50,
                length: 10,
                offset: 6,
            })
        );
    }

    #[test]
    fn block_trace_parser_stats_preserve_first_row_search_contest() {
        let plan = SequencePlan {
            trace_first_row_contest: Some(SequenceTraceRowSearchContest {
                winner: SequenceTraceMatchSource::Dict,
                source_length: 7,
                dict_length: 11,
                attempts_left_before_dict: 5,
            }),
            ..Default::default()
        };

        let stats = block_trace_parser_stats(
            &plan,
            RepeatOffsets::default(),
            0,
            BlockTraceDictionaryMode::DictMatchState,
        )
        .unwrap();

        assert_eq!(
            stats.first_row_search_contest,
            Some(BlockTraceRowSearchContest {
                winner: BlockTraceMatchSource::Dict,
                source_length: 7,
                dict_length: 11,
                attempts_left_before_dict: 5,
            })
        );
        assert_eq!(stats.first_emitted_match, None);
        assert_eq!(stats.second_emitted_match, None);
        assert_eq!(stats.third_emitted_match, None);
        assert_eq!(stats.fourth_emitted_match, None);
        assert_eq!(stats.first_accepted_regular_match, None);
    }

    #[test]
    fn trained_dictionary_matching_content_matches_upstream_cdict_window() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let prepared = EncoderDictionary::new(&dictionary).unwrap();
        let inner = prepared.as_inner();
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "trained-dictionary")
            .expect("trained-dictionary benchmark case should exist");
        let input = &case.input[..128 * 1024];
        let probe =
            upstream_trace_helper::trace_trained_dict_hc_probe(helper, 6, false, 6074, input);
        let upstream_prefix_len = probe
            .source_best_offset
            .checked_add(probe.source_head)
            .and_then(|current| current.checked_sub(probe.pos))
            .expect("upstream extdict probe should expose a valid prefix length");

        assert_eq!(inner.matching_content().len(), upstream_prefix_len);
        assert_eq!(
            &inner.matching_content()[inner.matching_content().len() - inner.content().len()..],
            inner.content()
        );
    }

    #[test]
    fn trained_dictionary_bad_block_extdict_lazy_probes_match_upstream() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let prepared = EncoderDictionary::new(&dictionary).unwrap();
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "trained-dictionary")
            .expect("trained-dictionary benchmark case should exist");
        let input = &case.input[..128 * 1024];

        for level in [5, 6, 7] {
            let rust_trace = planned_first_block_trace_with_prepared_dict(
                input,
                &prepared,
                EncoderOptions {
                    block_size: 128 * 1024,
                    checksum: false,
                    write_dict_id: true,
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    ..Default::default()
                },
            )
            .unwrap();
            let probe_positions =
                hot_extdict_probe_positions(&rust_trace.trace_chain_searches, input.len());

            for pos in probe_positions {
                let rust = rust_extdict_lazy_probe(
                    &rust_trace.trace_chain_searches,
                    &rust_trace.trace_emissions,
                    pos,
                )
                .unwrap_or_else(|| panic!("missing rust extdict lazy probe at position {pos}"));
                let upstream = upstream_trace_helper::trace_trained_dict_extdict_lazy_probe(
                    helper, level, false, pos, input,
                );

                assert_eq!(
                    upstream.backend,
                    upstream_trace_helper::UpstreamChainProbeBackend::ExtDict,
                    "level {level} probe {pos} should use extdict backend",
                );
                assert_eq!(
                    rust.anchor, upstream.anchor,
                    "level {level} probe {pos} anchor"
                );
                assert_eq!(
                    rust.offset_1, upstream.offset_1,
                    "level {level} probe {pos} offset_1",
                );
                assert_eq!(
                    rust.offset_2, upstream.offset_2,
                    "level {level} probe {pos} offset_2",
                );
                assert_eq!(
                    rust.baseline_rep_length, upstream.baseline_rep_length,
                    "level {level} probe {pos} baseline rep",
                );
                assert_eq!(
                    rust.baseline_regular_source, upstream.baseline_regular_source,
                    "level {level} probe {pos} baseline regular source",
                );
                assert_eq!(
                    rust.baseline_regular_length, upstream.baseline_regular_length,
                    "level {level} probe {pos} baseline regular length",
                );
                assert_eq!(
                    rust.baseline_regular_off_base, upstream.baseline_regular_off_base,
                    "level {level} probe {pos} baseline regular offbase",
                );
                assert_eq!(
                    rust.depth1_rep_length, upstream.depth1_rep_length,
                    "level {level} probe {pos} depth1 rep",
                );
                assert_eq!(
                    rust.depth1_regular_source, upstream.depth1_regular_source,
                    "level {level} probe {pos} depth1 regular source",
                );
                assert_eq!(
                    rust.depth1_regular_length, upstream.depth1_regular_length,
                    "level {level} probe {pos} depth1 regular length",
                );
                assert_eq!(
                    rust.depth1_regular_off_base, upstream.depth1_regular_off_base,
                    "level {level} probe {pos} depth1 regular offbase",
                );
                assert_eq!(
                    rust.depth2_rep_length, upstream.depth2_rep_length,
                    "level {level} probe {pos} depth2 rep",
                );
                assert_eq!(
                    rust.depth2_regular_source, upstream.depth2_regular_source,
                    "level {level} probe {pos} depth2 regular source",
                );
                assert_eq!(
                    rust.depth2_regular_length, upstream.depth2_regular_length,
                    "level {level} probe {pos} depth2 regular length",
                );
                assert_eq!(
                    rust.depth2_regular_off_base, upstream.depth2_regular_off_base,
                    "level {level} probe {pos} depth2 regular offbase",
                );
                assert_eq!(
                    rust.chosen_kind, upstream.chosen_kind,
                    "level {level} probe {pos} chosen kind",
                );
                assert_eq!(
                    rust.chosen_source, upstream.chosen_source,
                    "level {level} probe {pos} chosen source",
                );
                assert_eq!(
                    rust.chosen_start, upstream.chosen_start,
                    "level {level} probe {pos} chosen start",
                );
                assert_eq!(
                    rust.chosen_length, upstream.chosen_length,
                    "level {level} probe {pos} chosen length",
                );
                assert_eq!(
                    rust.chosen_off_base, upstream.chosen_off_base,
                    "level {level} probe {pos} chosen offbase",
                );
                assert_eq!(
                    rust.literal_length, upstream.literal_length,
                    "level {level} probe {pos} literal length",
                );
                assert_eq!(
                    rust.immediate_rep2_length, upstream.immediate_rep2_length,
                    "level {level} probe {pos} immediate rep2",
                );
            }
        }
    }

    #[test]
    fn trained_dictionary_bad_block_full_first_block_sequences_match_upstream() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let prepared = EncoderDictionary::new(&dictionary).unwrap();
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "trained-dictionary")
            .expect("trained-dictionary benchmark case should exist");
        let input = &case.input[..128 * 1024];

        for level in [5, 6, 7] {
            let params = compression_parameters_for_input(
                CompressionLevel::try_new(level).unwrap(),
                Some(input.len()),
                Some(prepared.as_inner()),
            );
            let upstream_cparams = upstream_trace_helper::trace_trained_dict_applied_cparams(
                helper, level, false, input,
            );
            let expected_cparams = upstream_cparams_from_helper(upstream_cparams);
            assert_eq!(
                params.upstream_cparams, expected_cparams,
                "level {level} should use upstream applied cparams for trained full dictionaries",
            );
            let rust_trace = planned_first_block_trace_with_prepared_dict(
                input,
                &prepared,
                EncoderOptions {
                    block_size: 128 * 1024,
                    checksum: false,
                    write_dict_id: true,
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    ..Default::default()
                },
            )
            .unwrap();
            let rust_sequences = &rust_trace.emitted_matches;
            let upstream_sequences = upstream_trace_helper::trace_trained_dict_sequences(
                helper,
                level,
                false,
                input.len(),
                input,
            );
            assert_eq!(
                rust_sequences.len(),
                upstream_sequences.len(),
                "level {level} emitted sequence count mismatch: rust={} upstream={} rust_tail={:?} upstream_tail={:?}",
                rust_sequences.len(),
                upstream_sequences.len(),
                &rust_sequences[rust_sequences.len().saturating_sub(4)..],
                &upstream_sequences[upstream_sequences.len().saturating_sub(4)..],
            );

            let first_mismatch = rust_sequences
                .iter()
                .zip(upstream_sequences.iter())
                .enumerate()
                .find(|(_, (rust, upstream))| {
                    rust.kind
                        != match upstream.kind {
                            upstream_trace_helper::UpstreamSequenceKind::Regular => {
                                BlockTraceEmittedMatchKind::Regular
                            }
                            upstream_trace_helper::UpstreamSequenceKind::Rep => {
                                BlockTraceEmittedMatchKind::Rep
                            }
                        }
                        || rust.source
                            != match upstream.source {
                                upstream_trace_helper::UpstreamSequenceSource::Dict => {
                                    BlockTraceMatchSource::Dict
                                }
                                upstream_trace_helper::UpstreamSequenceSource::Prefix => {
                                    BlockTraceMatchSource::Prefix
                                }
                                upstream_trace_helper::UpstreamSequenceSource::Source => {
                                    BlockTraceMatchSource::Source
                                }
                            }
                        || rust.start != upstream.start
                        || rust.literal_length != upstream.literal_length
                        || rust.length != upstream.match_length
                        || rust.off_base != upstream.off_base
                        || rust.offset != upstream.raw_offset
                });
            let mismatch_window = first_mismatch.map(|(index, _)| {
                mismatch_window(
                    rust_sequences,
                    &upstream_sequences,
                    &rust_trace.trace_emissions,
                    &rust_trace.trace_row_searches,
                    &rust_trace.trace_chain_searches,
                    index,
                )
            });
            if std::env::var_os("ZSTANDARD_PRINT_TRAINED_DICT_MISMATCH").is_some() {
                let probe_positions = mismatch_window
                    .as_ref()
                    .map(|window| {
                        let mut probe_positions = window
                            .chain_searches
                            .iter()
                            .map(|search| search.pos)
                            .collect::<Vec<_>>();
                        probe_positions.sort_unstable();
                        probe_positions.dedup();
                        probe_positions
                    })
                    .unwrap_or_default();
                let upstream_chain_probes = probe_positions
                    .iter()
                    .copied()
                    .map(|pos| {
                        upstream_trace_helper::trace_trained_dict_hc_probe(
                            helper, level, false, pos, input,
                        )
                    })
                    .collect::<Vec<_>>();
                let upstream_extdict_lazy_probes = probe_positions
                    .iter()
                    .copied()
                    .map(|pos| {
                        upstream_trace_helper::trace_trained_dict_extdict_lazy_probe(
                            helper, level, false, pos, input,
                        )
                    })
                    .collect::<Vec<_>>();
                eprintln!(
                    "level {level} mismatch_window={mismatch_window:?} upstream_chain_probes={upstream_chain_probes:?} upstream_extdict_lazy_probes={upstream_extdict_lazy_probes:?}",
                    mismatch_window = mismatch_window
                );
            }
            assert!(
                first_mismatch.is_none(),
                "level {level} first mismatch at sequence {}: rust={:?} upstream={:?} \
                 rust_prefix={:?} upstream_prefix={:?} emission_trace_prefix={:?} row_search_prefix={:?} \
                 mismatch_window={:?} hash38={} hash83={} match_finder={:?} upstream_cparams={:?}",
                first_mismatch
                    .map(|(index, _)| index + 1)
                    .unwrap_or_default(),
                first_mismatch.map(|(_, (rust, _))| *rust),
                first_mismatch.map(|(_, (_, upstream))| *upstream),
                &rust_sequences[..rust_sequences.len().min(14)],
                &upstream_sequences[..upstream_sequences.len().min(14)],
                &rust_trace.trace_emissions[..rust_trace.trace_emissions.len().min(14)],
                &rust_trace.trace_row_searches[..rust_trace.trace_row_searches.len().min(16)],
                mismatch_window,
                debug_row_hash_for_params(input, 38, params.match_finder, 0),
                debug_row_hash_for_params(input, 83, params.match_finder, 0),
                params.match_finder,
                upstream_cparams,
            );

            for (index, (rust, upstream)) in rust_sequences
                .iter()
                .zip(upstream_sequences)
                .take(100)
                .enumerate()
            {
                assert_eq!(
                    rust.kind,
                    match upstream.kind {
                        upstream_trace_helper::UpstreamSequenceKind::Regular => {
                            BlockTraceEmittedMatchKind::Regular
                        }
                        upstream_trace_helper::UpstreamSequenceKind::Rep => {
                            BlockTraceEmittedMatchKind::Rep
                        }
                    },
                    "level {level} sequence {} kind",
                    index + 1
                );
                assert_eq!(
                    rust.source,
                    match upstream.source {
                        upstream_trace_helper::UpstreamSequenceSource::Dict => {
                            BlockTraceMatchSource::Dict
                        }
                        upstream_trace_helper::UpstreamSequenceSource::Prefix => {
                            BlockTraceMatchSource::Prefix
                        }
                        upstream_trace_helper::UpstreamSequenceSource::Source => {
                            BlockTraceMatchSource::Source
                        }
                    },
                    "level {level} sequence {} source",
                    index + 1
                );
                assert_eq!(
                    rust.start,
                    upstream.start,
                    "level {level} sequence {} start",
                    index + 1
                );
                assert_eq!(
                    rust.literal_length,
                    upstream.literal_length,
                    "level {level} sequence {} literal length",
                    index + 1
                );
                assert_eq!(
                    rust.length,
                    upstream.match_length,
                    "level {level} sequence {} match length",
                    index + 1
                );
                assert_eq!(
                    rust.off_base,
                    upstream.off_base,
                    "level {level} sequence {} offbase",
                    index + 1
                );
                assert_eq!(
                    rust.offset,
                    upstream.raw_offset,
                    "level {level} sequence {} raw offset",
                    index + 1
                );
            }
        }
    }

    /// Byte-exact parity on the trained-dictionary encode path, at the three
    /// levels where it used to fail.
    ///
    /// The sequences always matched here; what diverged was the Huffman table
    /// written for the literals, because this crate ran C's optimal-depth
    /// search at every level while C runs it only at `ZSTD_btultra` and above.
    /// See `huffman_table_depth` in this file.
    #[test]
    fn trained_dictionary_bad_block_first_block_literals_match_upstream() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let prepared = EncoderDictionary::new(&dictionary).unwrap();
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "trained-dictionary")
            .expect("trained-dictionary benchmark case should exist");
        let input = &case.input[..128 * 1024];

        for level in [5, 6, 7] {
            let options = EncoderOptions {
                block_size: 128 * 1024,
                checksum: false,
                write_dict_id: true,
                compression_level: CompressionLevel::try_new(level).unwrap(),
                ..Default::default()
            };
            let rust = parse_first_block_literals_section(
                &encode_all_with_prepared_dict_and_options(input, &prepared, options).unwrap(),
            );
            let upstream = upstream_trace_helper::trace_trained_dict_first_block_literals(
                helper, level, false, input,
            );

            assert_eq!(
                rust, upstream,
                "level {level} first-block literals mismatch: rust={rust:?} upstream={upstream:?}",
            );
        }
    }

    /// See `trained_dictionary_bad_block_first_block_literals_match_upstream`
    /// for what used to make this diverge.
    #[test]
    fn trained_dictionary_benchmark_all_block_sections_match_upstream_at_level_five() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let prepared = EncoderDictionary::new(&dictionary).unwrap();
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "trained-dictionary")
            .expect("trained-dictionary benchmark case should exist");
        let input = &case.input;
        let options = EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::try_new(5).unwrap(),
            ..Default::default()
        };
        let rust_frame =
            encode_all_with_prepared_dict_and_options(input, &prepared, options).unwrap();
        let upstream_frame = upstream_trace_helper::compress_once(
            helper,
            "compress-trained-dict-configured",
            5,
            false,
            input,
        );
        let block_count = compressed_block_count(&rust_frame);
        assert_eq!(block_count, compressed_block_count(&upstream_frame));

        for block_index in 0..block_count {
            let rust = parse_compressed_block_sections(&rust_frame, block_index);
            let upstream = parse_compressed_block_sections(&upstream_frame, block_index);
            assert_eq!(
                rust,
                upstream,
                "trained-dictionary L5 block {block_index} mismatch: rust={} upstream={}",
                first_block_sections_summary(&rust),
                first_block_sections_summary(&upstream),
            );
        }
    }

    /// Decode both sides' Huffman table descriptions and print the code length
    /// each symbol got.
    ///
    /// This is what identified the depth-search divergence: the two tables
    /// covered the same 33 symbols with the same literal payload size, and the
    /// only difference was that upstream's deepest code was 10 bits and this
    /// crate's was 9. A literals section that differs while the sequences
    /// match is a table question, and the table is not readable from the
    /// section sizes alone.
    #[test]
    fn print_trained_dictionary_first_block_huffman_weights() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let prepared = EncoderDictionary::new(&dictionary).unwrap();
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "trained-dictionary")
            .expect("trained-dictionary benchmark case should exist");
        let input = &case.input[..128 * 1024];

        for level in [5u8] {
            let options = EncoderOptions {
                block_size: 128 * 1024,
                checksum: false,
                compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
                ..Default::default()
            };
            let rust = parse_first_block_literals_section(
                &encode_all_with_prepared_dict_and_options(input, &prepared, options).unwrap(),
            );
            let upstream = upstream_trace_helper::trace_trained_dict_first_block_literals(
                helper,
                i32::from(level),
                false,
                input,
            );

            for (label, section) in [("rust", &rust), ("zstd", &upstream)] {
                let description = &section.section_bytes[section.literals_header_size..];
                let mut ctable = crate::entropy::huff0::CTableX1::default();
                let read = crate::entropy::huff0::read_ctable_x1(description, &mut ctable).unwrap();
                let bits: Vec<u8> = (0..=255u8).map(|s| ctable.symbol_nb_bits(s)).collect();
                let present = bits.iter().filter(|&&b| b != 0).count();
                println!(
                    "L{level} {label}: table_desc={read} first8={:?} present={present} bits={bits:?}",
                    &description[..8.min(description.len())],
                );
            }
        }
    }

    #[test]
    fn trained_dictionary_benchmark_block_two_literals_match_upstream_at_levels_six_and_seven() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let prepared = EncoderDictionary::new(&dictionary).unwrap();
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "trained-dictionary")
            .expect("trained-dictionary benchmark case should exist");
        let input = &case.input;

        for level in [6u8, 7u8] {
            let options = EncoderOptions {
                block_size: 128 * 1024,
                checksum: false,
                write_dict_id: true,
                compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
                ..Default::default()
            };
            let rust_frame =
                encode_all_with_prepared_dict_and_options(input, &prepared, options).unwrap();
            let rust = parse_compressed_block_sections(&rust_frame, 2).literals;
            let upstream = upstream_trace_helper::trace_trained_dict_block_literals(
                helper,
                i32::from(level),
                false,
                2,
                input,
            );
            assert_eq!(
                rust, upstream,
                "trained-dictionary L{level} block 2 literals mismatch: rust={rust:?} upstream={upstream:?}",
            );
        }
    }

    /// See `trained_dictionary_bad_block_first_block_literals_match_upstream`
    /// for what used to make this diverge.
    #[test]
    fn trained_dictionary_benchmark_all_block_sections_match_upstream_at_levels_six_and_seven() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let prepared = EncoderDictionary::new(&dictionary).unwrap();
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "trained-dictionary")
            .expect("trained-dictionary benchmark case should exist");
        let input = &case.input;

        for level in [6u8, 7u8] {
            let options = EncoderOptions {
                block_size: 128 * 1024,
                checksum: false,
                write_dict_id: true,
                compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
                ..Default::default()
            };
            let rust_frame =
                encode_all_with_prepared_dict_and_options(input, &prepared, options).unwrap();
            let upstream_frame = upstream_trace_helper::compress_once(
                helper,
                "compress-trained-dict-configured",
                i32::from(level),
                false,
                input,
            );
            let block_count = compressed_block_count(&rust_frame);
            assert_eq!(block_count, compressed_block_count(&upstream_frame));
            let first_mismatch_block = (0..block_count).find(|&block_index| {
                parse_compressed_block_sections(&rust_frame, block_index)
                    != parse_compressed_block_sections(&upstream_frame, block_index)
            });

            if std::env::var_os("ZSTANDARD_PRINT_TRAINED_DICT_BLOCK_MISMATCH").is_some() {
                if let Some(block_index) = first_mismatch_block {
                    let rust = parse_compressed_block_sections(&rust_frame, block_index);
                    let upstream = parse_compressed_block_sections(&upstream_frame, block_index);
                    let rust_trace = planned_block_trace_with_prepared_dict(
                        input,
                        &prepared,
                        options,
                        block_index,
                    )
                    .unwrap();
                    let upstream_sequences = decode_compressed_block_sequences_from_frame(
                        &upstream_frame,
                        &prepared,
                        input.len(),
                        options,
                        block_index,
                    )
                    .unwrap();
                    let rust_frame_sequences = decode_compressed_block_sequences_from_frame(
                        &rust_frame,
                        &prepared,
                        input.len(),
                        options,
                        block_index,
                    )
                    .unwrap();
                    let first_sequence_mismatch = emitted_sequence_first_mismatch(
                        &rust_trace.emitted_matches,
                        &upstream_sequences,
                    );
                    let first_actual_sequence_mismatch =
                        emitted_sequence_first_mismatch(&rust_frame_sequences, &upstream_sequences);
                    let rust_sequence_window = first_sequence_mismatch.map(|index| {
                        let start = index.saturating_sub(2);
                        let end = (index + 3).min(rust_trace.emitted_matches.len());
                        rust_trace.emitted_matches[start..end].to_vec()
                    });
                    let rust_actual_sequence_window = first_actual_sequence_mismatch.map(|index| {
                        let start = index.saturating_sub(2);
                        let end = (index + 3).min(rust_frame_sequences.len());
                        rust_frame_sequences[start..end].to_vec()
                    });
                    let upstream_sequence_window = first_sequence_mismatch.map(|index| {
                        let start = index.saturating_sub(2);
                        let end = (index + 3).min(upstream_sequences.len());
                        upstream_sequences[start..end].to_vec()
                    });
                    let probe_pos = first_sequence_mismatch.and_then(|index| {
                        rust_trace.emitted_matches.get(index).map(|seq| seq.start)
                    });
                    let rust_chain_window = probe_pos.map(|pos| {
                        rust_trace
                            .trace_chain_searches
                            .iter()
                            .copied()
                            .filter(|search| {
                                pos.saturating_sub(8) <= search.pos && search.pos <= pos + 8
                            })
                            .collect::<Vec<_>>()
                    });
                    let upstream_chain_probe = probe_pos.map(|pos| {
                        upstream_trace_helper::trace_trained_dict_hc_probe(
                            helper,
                            i32::from(level),
                            false,
                            pos,
                            input,
                        )
                    });
                    let upstream_extdict_lazy_probes = probe_pos
                        .map(|pos| {
                            let mut probe_positions = Vec::with_capacity(3);
                            probe_positions.push(pos.saturating_sub(1));
                            probe_positions.push(pos);
                            probe_positions.push(pos.saturating_add(1));
                            probe_positions.sort_unstable();
                            probe_positions.dedup();
                            probe_positions
                        })
                        .unwrap_or_default()
                        .into_iter()
                        .map(|pos| {
                            (
                                pos,
                                upstream_trace_helper::trace_trained_dict_extdict_block_lazy_probe(
                                    helper,
                                    i32::from(level),
                                    false,
                                    block_index,
                                    pos,
                                    input,
                                ),
                            )
                        })
                        .collect::<Vec<_>>();
                    eprintln!(
                        "trained-dictionary L{level} first block mismatch at block {block_index}: rust={} upstream={} rust_sequence_count={} rust_actual_sequence_count={} upstream_sequence_count={} first_sequence_mismatch={first_sequence_mismatch:?} first_actual_sequence_mismatch={first_actual_sequence_mismatch:?} rust_sequence={:?} rust_actual_sequence={:?} upstream_sequence={:?} rust_sequence_window={rust_sequence_window:?} rust_actual_sequence_window={rust_actual_sequence_window:?} upstream_sequence_window={upstream_sequence_window:?} rust_chain_window={rust_chain_window:?} upstream_chain_probe={upstream_chain_probe:?} upstream_extdict_lazy_probes={upstream_extdict_lazy_probes:?}",
                        first_block_sections_summary(&rust),
                        first_block_sections_summary(&upstream),
                        rust_trace.emitted_matches.len(),
                        rust_frame_sequences.len(),
                        upstream_sequences.len(),
                        first_sequence_mismatch
                            .and_then(|index| rust_trace.emitted_matches.get(index).copied()),
                        first_actual_sequence_mismatch
                            .and_then(|index| rust_frame_sequences.get(index).copied()),
                        first_sequence_mismatch
                            .and_then(|index| upstream_sequences.get(index).copied()),
                    );
                }
            }

            assert!(
                first_mismatch_block.is_none(),
                "trained-dictionary L{level} should match upstream across all blocks: first_mismatch_block={first_mismatch_block:?}",
            );
        }
    }

    #[test]
    fn first_block_stage_profile_matches_emitted_block_counts_for_benchmark_paths() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let trained_dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let trained_prepared = EncoderDictionary::new(&trained_dictionary).unwrap();
        let cases = benchmark_corpora::benchmark_report_cases(512 * 1024);

        for case_name in ["json-records", "trained-dictionary"] {
            let case = cases
                .iter()
                .find(|case| case.name == case_name)
                .expect("benchmark case should exist");
            let options = EncoderOptions {
                block_size: 128 * 1024,
                checksum: false,
                write_dict_id: true,
                compression_level: CompressionLevel::try_new(6).unwrap(),
                ..Default::default()
            };
            let first_chunk = &case.input[..case.input.len().min(options.block_size)];
            let (profile, frame) = if case_name == "trained-dictionary" {
                (
                    profile_first_block_with_prepared_dict_and_options(
                        first_chunk,
                        &trained_prepared,
                        options,
                        PlannerPhases::On,
                    )
                    .unwrap(),
                    encode_all_with_prepared_dict_and_options(
                        first_chunk,
                        &trained_prepared,
                        options,
                    )
                    .unwrap(),
                )
            } else {
                (
                    profile_first_block_with_options(first_chunk, options, PlannerPhases::On)
                        .unwrap(),
                    encode_all_with_options(first_chunk, options).unwrap(),
                )
            };

            assert_eq!(profile.blocks, compressed_block_count(&frame));
            assert_eq!(
                profile.compressed_blocks + profile.raw_blocks + profile.rle_blocks,
                profile.blocks,
            );
            assert!(
                profile.total
                    >= profile.block_split
                        + profile.planning
                        + profile.literals
                        + profile.sequences
            );
            assert!(
                profile.planning
                    >= profile.planning_row_search
                        + profile.planning_chain_search
                        + profile.planning_rep_check
                        + profile.planning_match_count
                        + profile.planning_insert_update
                        + profile.planning_parser
            );
            assert!(
                profile.planning_parser
                    >= profile.planning_row_parser_baseline_rep
                        + profile.planning_row_parser_baseline_regular
                        + profile.planning_row_parser_continue
                        + profile.planning_row_parser_store
                        + profile.planning_row_parser_rep2
            );
        }
    }

    #[test]
    fn no_dict_bad_block_first_block_literals_match_upstream() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        for case_name in ["json-records", "log-lines"] {
            let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
                .into_iter()
                .find(|case| case.name == case_name)
                .unwrap_or_else(|| panic!("{case_name} benchmark case should exist"));
            let input = &case.input;

            for level in [5, 6, 7] {
                let options = EncoderOptions {
                    block_size: 128 * 1024,
                    checksum: false,
                    write_dict_id: true,
                    compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
                    ..Default::default()
                };
                let rust = parse_first_block_literals_section(
                    &encode_all_with_options(input, options).unwrap(),
                );
                let upstream = upstream_trace_helper::trace_first_block_literals(
                    helper,
                    "compress-regular-configured",
                    i32::from(level),
                    false,
                    input,
                );
                let sequences_match =
                    regular_first_block_sequences_match_upstream(helper, level, input);

                if sequences_match {
                    assert_eq!(
                        rust, upstream,
                        "{case_name} level {level} first-block literals mismatch: rust={rust:?} upstream={upstream:?}",
                    );
                } else {
                    assert_eq!(
                        rust.block_type, upstream.block_type,
                        "{case_name} level {level} literals block type diverged before sequence parity: rust={rust:?} upstream={upstream:?}",
                    );
                    assert_eq!(
                        rust.huffman_table_mode, upstream.huffman_table_mode,
                        "{case_name} level {level} Huffman table mode diverged before sequence parity: rust={rust:?} upstream={upstream:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn no_dict_benchmark_first_block_sections_match_upstream() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        for case_name in ["json-records", "log-lines"] {
            let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
                .into_iter()
                .find(|case| case.name == case_name)
                .unwrap_or_else(|| panic!("{case_name} benchmark case should exist"));
            let input = &case.input;

            for level in [5, 6, 7] {
                let options = EncoderOptions {
                    block_size: 128 * 1024,
                    checksum: false,
                    write_dict_id: true,
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    ..Default::default()
                };
                let rust =
                    parse_first_block_sections(&encode_all_with_options(input, options).unwrap());
                let upstream = upstream_trace_helper::trace_first_block_sections(
                    helper,
                    "compress-regular-configured",
                    level,
                    false,
                    input,
                );
                let rust_summary = first_block_sections_summary(&rust);
                let upstream_summary = first_block_sections_summary(&upstream);

                if std::env::var_os("ZSTANDARD_PRINT_NO_DICT_BLOCK_SECTION_MISMATCH").is_some() {
                    eprintln!(
                        "{case_name} level {level} first-block sections: rust={rust_summary} upstream={upstream_summary}",
                    );
                }

                // Everything except the sequence bitstream must still be equal
                // byte for byte. The literals section is the sharp one: it is
                // the input minus the matched regions, so byte-identical
                // literals mean identical match positions and lengths, which
                // pins the whole parse without asserting anything about how the
                // offsets were coded.
                assert_eq!(
                    rust.last_block, upstream.last_block,
                    "{case_name} level {level} block header diverged: rust={rust_summary} upstream={upstream_summary}",
                );
                assert_eq!(
                    rust.literals, upstream.literals,
                    "{case_name} level {level} first-block literals mismatch: rust={rust_summary} upstream={upstream_summary}",
                );
                assert_eq!(
                    rust.sequence_count, upstream.sequence_count,
                    "{case_name} level {level} sequence count mismatch: rust={rust_summary} upstream={upstream_summary}",
                );
                assert_eq!(
                    rust.sequence_modes, upstream.sequence_modes,
                    "{case_name} level {level} sequence compression modes mismatch: rust={rust_summary} upstream={upstream_summary}",
                );

                // The sequence bitstream is where the repcode substitution shows
                // up, and it is the one section allowed to differ -- in one
                // direction. Coding a distance that is still live as a repcode
                // replaces an offset code and its extra bits with code 0 or 1,
                // so the bitstream can only shrink; if it ever grows, the
                // substitution has stopped paying for itself somewhere and that
                // is worth failing over. See "The repcode substitution" in
                // `docs/PARITY_PLAN.md`.
                assert!(
                    rust.sequence_section_size <= upstream.sequence_section_size,
                    "{case_name} level {level} sequence bitstream grew against upstream: rust={rust_summary} upstream={upstream_summary}",
                );
                if rust.sequence_section_bytes == upstream.sequence_section_bytes {
                    assert_eq!(
                        rust, upstream,
                        "{case_name} level {level} sequence bitstreams are identical but the blocks are not: rust={rust_summary} upstream={upstream_summary}",
                    );
                }
            }
        }
    }

    #[test]
    fn no_dict_bad_block_uses_upstream_applied_cparams() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "json-records")
            .expect("json-records benchmark case should exist");
        let input = &case.input[..128 * 1024];
        let params = compression_parameters_for_input(
            CompressionLevel::try_new(5).unwrap(),
            Some(input.len()),
            None,
        );
        let upstream_cparams =
            upstream_trace_helper::trace_regular_applied_cparams(helper, 5, false, input);
        let expected_cparams = upstream_cparams_from_helper(upstream_cparams);

        assert_eq!(
            params.upstream_cparams, expected_cparams,
            "level 5 no-dict bad block should use upstream applied cparams",
        );
    }

    #[test]
    fn print_ratio_failure_applied_cparams() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        for (case_name, level) in [
            ("log-lines", 1u8),
            ("log-lines", 2u8),
            ("log-lines", 3u8),
            ("log-lines", 8u8),
            ("mixed-entropy", 2u8),
            ("mixed-entropy", 3u8),
            ("mixed-entropy", 4u8),
        ] {
            let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
                .into_iter()
                .find(|case| case.name == case_name)
                .unwrap_or_else(|| panic!("{case_name} benchmark case should exist"));
            let input = &case.input;
            let rust = compression_parameters_for_input(
                CompressionLevel::try_new(i32::from(level)).unwrap(),
                Some(input.len()),
                None,
            )
            .upstream_cparams;
            let upstream = upstream_trace_helper::trace_regular_applied_cparams(
                helper,
                i32::from(level),
                false,
                input,
            );
            eprintln!("{case_name} L{level}: rust={rust:?} upstream={upstream:?}");
        }
    }

    #[test]
    fn print_mixed_entropy_second_block_sequences() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "mixed-entropy")
            .expect("mixed-entropy benchmark case should exist");
        for level in [2u8, 3u8] {
            let options = EncoderOptions {
                block_size: 128 * 1024,
                checksum: false,
                write_dict_id: true,
                compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
                ..Default::default()
            };
            let rust = encode_all_with_options(&case.input, options).unwrap();
            let upstream = upstream_trace_helper::compress_once(
                helper,
                "compress-regular-configured",
                i32::from(level),
                false,
                &case.input,
            );
            let rust_block0 =
                decoded_compressed_block_sequences(&rust, 0, RepeatOffsets::default());
            let rust_block1 =
                decoded_compressed_block_sequences(&rust, 1, rust_block0.final_repeat_offsets);
            let upstream_block0 =
                decoded_compressed_block_sequences(&upstream, 0, RepeatOffsets::default());
            let upstream_block1 = decoded_compressed_block_sequences(
                &upstream,
                1,
                upstream_block0.final_repeat_offsets,
            );

            eprintln!("mixed-entropy L{level}:");
            eprintln!(
                "  rust block0 reps={:?} count={} last={:?}",
                rust_block0.final_repeat_offsets.values(),
                rust_block0.sequences.len(),
                rust_block0.sequences.last()
            );
            eprintln!(
                "  upstream block0 reps={:?} count={} last={:?}",
                upstream_block0.final_repeat_offsets.values(),
                upstream_block0.sequences.len(),
                upstream_block0.sequences.last()
            );
            eprintln!(
                "  rust block1 reps_before={:?} count={} seqs={:?}",
                rust_block0.final_repeat_offsets.values(),
                rust_block1.sequences.len(),
                rust_block1.sequences
            );
            eprintln!(
                "  upstream block1 reps_before={:?} count={} seqs={:?}",
                upstream_block0.final_repeat_offsets.values(),
                upstream_block1.sequences.len(),
                upstream_block1.sequences
            );
        }
    }

    /// The other side of the Huffman depth-search gate: it has to stay *on*
    /// where upstream runs it.
    ///
    /// The threshold is `HUF_OPTIMAL_DEPTH_THRESHOLD`, which is `ZSTD_btultra`
    /// — levels 18 and up on a body this size, per `clevels.h`. The three
    /// trained-dictionary parity tests fail if the search runs below the
    /// threshold; nothing failed if it stopped running above it, and the cost
    /// of that is real: on `binary-structured` at 4 MiB, turning the search
    /// off at level 19 moves this crate from 5 bytes over upstream to 312.
    ///
    /// `json-records` is the case that shows it at the 512 KiB the suite uses,
    /// where the difference is a single byte. Small, but this compares whole
    /// frames rather than lengths, so a one-byte table change cannot hide in
    /// it.
    #[test]
    fn ultra_levels_still_search_for_the_huffman_table_depth() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        assert!(
            ParserStrategy::BinaryTreeUltra.searches_huffman_table_depth(),
            "ZSTD_btultra and ZSTD_btultra2 are at and above HUF_OPTIMAL_DEPTH_THRESHOLD",
        );
        assert!(
            !ParserStrategy::BinaryTreeOpt.searches_huffman_table_depth(),
            "ZSTD_btopt is below HUF_OPTIMAL_DEPTH_THRESHOLD",
        );

        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "json-records")
            .expect("json-records benchmark case should exist");

        for level in [18u8, 19u8] {
            let options = EncoderOptions {
                block_size: 128 * 1024,
                checksum: false,
                compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
                ..Default::default()
            };
            let rust = encode_all_with_options(&case.input, options).unwrap();
            let upstream = upstream_trace_helper::compress_once(
                helper,
                "compress-regular-configured",
                i32::from(level),
                false,
                &case.input,
            );
            assert_eq!(
                rust.len(),
                upstream.len(),
                "json-records L{level}: frame length differs from upstream",
            );
            assert_eq!(
                rust, upstream,
                "json-records L{level}: frame differs from upstream"
            );
        }
    }

    /// Benchmark rows where this crate still emits more bytes than upstream at
    /// the pinned ref, and by how much.
    ///
    /// Matching upstream byte-for-byte is a goal, not a guarantee. Recording
    /// the outstanding gaps rather than asserting there are none keeps the
    /// check useful in both directions: a new row, or an existing row getting
    /// worse, fails; and so does a row that has been fixed, which forces the
    /// entry to be removed instead of quietly going stale.
    ///
    /// Two limits are worth stating, because both are places this table cannot
    /// see. It sweeps one corpus size, so a gap that only opens on a larger
    /// input is invisible here: `binary-structured` L16 and L17 are +39 bytes
    /// at 4 MiB in `BENCHMARKS.md` and exactly zero at the 512 KiB this test
    /// uses. And it only measures the one-shot encoder, so streaming size
    /// parity is unmeasured by anything that fails.
    const KNOWN_UPSTREAM_SIZE_GAPS: &[(&str, u8, isize)] = &[
        // Levels 18 and up on this case. The delta is not a fixed parse tie: it
        // is -7 at 256 KiB (we are smaller), +9 at 512 KiB, +51 at 1 MiB, and
        // +77 at 4 MiB, so it is a scatter of individual block decisions going
        // upstream's way slightly more often than ours rather than one
        // divergence. It shrinks in relative terms as the input grows, ending
        // at 0.006%.
        ("mixed-entropy", 18, 9),
        ("mixed-entropy", 19, 9),
        ("mixed-entropy", 20, 9),
        ("mixed-entropy", 21, 9),
        ("mixed-entropy", 22, 9),
        // `("wikipedia", 5, 3)` sat here until 2026-08-06. It was opened
        // deliberately, by deleting the eager tail insert at the end of a block,
        // and closed by the repcode substitution reaching the row-lazy parsers
        // -- that level now runs 2.93% under upstream rather than 3 bytes over,
        // so the row cannot be a gap in either size or cause.
        //
        // Same shape, and non-monotonic in size: 0 at 256 KiB, +38 at 512 KiB,
        // +29 at 1 MiB. A single cause would not come and go like that.
        ("tabular-csv", 19, 38),
        ("tabular-csv", 20, 38),
        ("tabular-csv", 21, 38),
        ("tabular-csv", 22, 15),
        // These are all ties rather than divergences, and unlike the two cases
        // above they hold their size: every one stays within 0 to 2 bytes
        // across 256 KiB, 512 KiB and 1 MiB while the compressed output itself
        // grows about fourfold.
        //
        // Which levels tie moved when `sort_nodes` was made C's `HUF_sort`
        // rather than the older insertion sort it had been ported from. Two
        // symbols with the same literal count get different code lengths
        // depending only on where the sort leaves them, so a tie-break is a
        // different table of the same cost, and the byte it is worth lands on
        // whichever levels happen to tie. L9 and L10 closed and L17 opened;
        // the total across the sweep went down by one byte.
        ("raw-dictionary", 16, 1),
        ("raw-dictionary", 17, 1),
        ("raw-dictionary", 18, 2),
        ("raw-dictionary", 19, 2),
        ("raw-dictionary", 20, 2),
        ("raw-dictionary", 21, 2),
        ("raw-dictionary", 22, 2),
    ];

    #[test]
    fn benchmark_output_is_no_larger_than_upstream_except_for_known_gaps() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
        let trained_dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let raw_prepared =
            EncoderDictionary::new(&raw_dictionary).expect("raw dictionary must parse");
        let trained_prepared =
            EncoderDictionary::new(&trained_dictionary).expect("trained dictionary must parse");
        let cases = benchmark_corpora::benchmark_report_cases(512 * 1024);
        let mut failures = Vec::new();

        for case in &cases {
            // The whole public range. This swept 1 to 9 until the levels above
            // it were measured and found to cost 13 seconds for the other 13,
            // which is not a reason to leave the binary-tree and optimal
            // parsers without a size gate.
            for level in 1u8..=22 {
                let options = EncoderOptions {
                    block_size: 128 * 1024,
                    checksum: false,
                    write_dict_id: true,
                    compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
                    ..Default::default()
                };
                let rust = match case.dict_kind {
                    benchmark_corpora::DictKind::None => {
                        encode_all_with_options(&case.input, options).unwrap()
                    }
                    benchmark_corpora::DictKind::Raw => encode_all_with_prepared_dict_and_options(
                        &case.input,
                        &raw_prepared,
                        options,
                    )
                    .unwrap(),
                    benchmark_corpora::DictKind::Trained => {
                        encode_all_with_prepared_dict_and_options(
                            &case.input,
                            &trained_prepared,
                            options,
                        )
                        .unwrap()
                    }
                };
                let mode = match case.dict_kind {
                    benchmark_corpora::DictKind::None => "compress-regular-configured",
                    benchmark_corpora::DictKind::Raw => "compress-raw-dict-configured",
                    benchmark_corpora::DictKind::Trained => "compress-trained-dict-configured",
                };
                let upstream = upstream_trace_helper::compress_once(
                    helper,
                    mode,
                    i32::from(level),
                    false,
                    &case.input,
                );
                if rust.len() > upstream.len() {
                    failures.push((case.name, level, rust.len(), upstream.len()));
                }
            }
        }

        for (case_name, level, rust, upstream) in &failures {
            eprintln!(
                "{case_name} L{level}: rust_size={rust} upstream_size={upstream} delta={}",
                *rust as isize - *upstream as isize
            );
        }

        let observed: Vec<(&str, u8, isize)> = failures
            .iter()
            .map(|(name, level, rust, upstream)| {
                (*name, *level, *rust as isize - *upstream as isize)
            })
            .collect();
        assert_eq!(
            observed,
            KNOWN_UPSTREAM_SIZE_GAPS.to_vec(),
            "size parity against upstream {} changed; update KNOWN_UPSTREAM_SIZE_GAPS if a gap \
             was closed, or investigate if one opened or widened",
            upstream_trace_helper::pinned_upstream_ref(),
        );
    }

    #[test]
    fn print_json_records_block_sections() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "json-records")
            .expect("json-records benchmark case should exist");
        for level in [3u8, 4u8] {
            let options = EncoderOptions {
                block_size: 128 * 1024,
                checksum: false,
                write_dict_id: true,
                compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
                ..Default::default()
            };
            let rust = encode_all_with_options(&case.input, options).unwrap();
            eprintln!("json-records L{level}:");
            for block_index in 0..4 {
                let rust_sections = parse_compressed_block_sections(&rust, block_index);
                let upstream_sections = upstream_trace_helper::trace_compressed_block_sections(
                    helper,
                    "compress-regular-configured",
                    i32::from(level),
                    false,
                    block_index,
                    &case.input,
                );
                eprintln!(
                    "  block {block_index}: rust={} upstream={}",
                    first_block_sections_summary(&rust_sections),
                    first_block_sections_summary(&upstream_sections),
                );
            }
        }
    }

    #[test]
    fn print_json_records_l3_l4_first_block_sequences() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "json-records")
            .expect("json-records benchmark case should exist");
        let input = &case.input[..128 * 1024];

        for level in [3u8, 4u8] {
            let (rust_trace, upstream_sequences) =
                regular_first_block_sequence_traces(helper, level, input);
            let rust_sequences = &rust_trace.emitted_matches;
            let first_mismatch =
                regular_first_block_sequence_first_mismatch(rust_sequences, &upstream_sequences);
            let mismatch_window = first_mismatch.map(|index| {
                mismatch_window(
                    rust_sequences,
                    &upstream_sequences,
                    &rust_trace.trace_emissions,
                    &rust_trace.trace_row_searches,
                    &rust_trace.trace_chain_searches,
                    index,
                )
            });

            eprintln!(
                "json-records L{level} first_mismatch={first_mismatch:?} rust_window={:?} upstream_window={:?}",
                first_mismatch
                    .map(|index| { &rust_sequences[index..rust_sequences.len().min(index + 4)] }),
                first_mismatch.map(|index| {
                    &upstream_sequences[index..upstream_sequences.len().min(index + 4)]
                }),
            );
            eprintln!("json-records L{level} mismatch_window={mismatch_window:?}");
        }
    }

    #[test]
    fn print_raw_dictionary_first_block_sequences() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
        let raw_prepared =
            EncoderDictionary::new(&raw_dictionary).expect("raw dictionary must parse");
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "raw-dictionary")
            .expect("raw-dictionary benchmark case should exist");
        let options = EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::try_new(3).unwrap(),
            ..Default::default()
        };
        let rust =
            encode_all_with_prepared_dict_and_options(&case.input, &raw_prepared, options).unwrap();
        let upstream = upstream_trace_helper::compress_once(
            helper,
            "compress-raw-dict-configured",
            3,
            false,
            &case.input,
        );
        let initial_repeat_offsets = raw_prepared.as_inner().repeat_offsets();
        let rust_block0 = decoded_compressed_block_sequences(&rust, 0, initial_repeat_offsets);
        let upstream_block0 =
            decoded_compressed_block_sequences(&upstream, 0, initial_repeat_offsets);
        let first_mismatch =
            emitted_sequence_first_mismatch(&rust_block0.sequences, &upstream_block0.sequences);
        eprintln!(
            "raw-dictionary L3 rust block0 reps_after={:?} count={} first={:?}",
            rust_block0.final_repeat_offsets.values(),
            rust_block0.sequences.len(),
            rust_block0.sequences.first()
        );
        eprintln!(
            "raw-dictionary L3 upstream block0 reps_after={:?} count={} first={:?}",
            upstream_block0.final_repeat_offsets.values(),
            upstream_block0.sequences.len(),
            upstream_block0.sequences.first()
        );
        eprintln!(
            "raw-dictionary L3 first_mismatch={first_mismatch:?} rust_window={:?} upstream_window={:?}",
            first_mismatch.map(|index| {
                &rust_block0.sequences[index..rust_block0.sequences.len().min(index + 4)]
            }),
            first_mismatch.map(|index| {
                &upstream_block0.sequences[index..upstream_block0.sequences.len().min(index + 4)]
            }),
        );
    }

    /// The ext-dict chain finder must file inserted positions under the same
    /// virtual base its searches use.
    ///
    /// One size cannot see this. The per-block insert used the prefix finder's
    /// `next_to_update` as the base where the searches use the prefix length,
    /// and those differ by `MIN_MATCH - 1` always, so every position that path
    /// inserted was filed under the hash of the bytes three later. The cost is
    /// a few bytes per block, which on a 512 KiB body is four blocks whose
    /// errors very nearly cancel: `raw-dictionary` L4 was three bytes over
    /// upstream there and read as a rounding tie for months. It is a fixed cost
    /// per block, so it only becomes legible as the block count grows.
    ///
    /// Hence two sizes: a gap that is flat in the block count is a tie, and one
    /// that scales is a divergence that recurs.
    #[test]
    fn raw_dictionary_excess_does_not_grow_with_the_block_count() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
        let raw_prepared =
            EncoderDictionary::new(&raw_dictionary).expect("raw dictionary must parse");
        for input_bytes in [512 * 1024, 2 * 1024 * 1024] {
            let case = benchmark_corpora::benchmark_report_cases(input_bytes)
                .into_iter()
                .find(|case| case.name == "raw-dictionary")
                .expect("raw-dictionary benchmark case should exist");
            for level in [4u8, 5] {
                let options = EncoderOptions {
                    block_size: 128 * 1024,
                    checksum: false,
                    write_dict_id: true,
                    compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
                    ..Default::default()
                };
                let rust =
                    encode_all_with_prepared_dict_and_options(&case.input, &raw_prepared, options)
                        .unwrap();
                let upstream = upstream_trace_helper::compress_once(
                    helper,
                    "compress-raw-dict-configured",
                    i32::from(level),
                    false,
                    &case.input,
                );
                assert!(
                    rust.len() <= upstream.len(),
                    "raw-dictionary L{level} at {input_bytes} bytes: {} vs upstream {} (+{})",
                    rust.len(),
                    upstream.len(),
                    rust.len() - upstream.len(),
                );
            }
        }
    }

    #[test]
    fn print_raw_dictionary_l16_optimal_sequences() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
        let raw_prepared =
            EncoderDictionary::new(&raw_dictionary).expect("raw dictionary must parse");
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "raw-dictionary")
            .expect("raw-dictionary benchmark case should exist");
        let options = EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::try_new(16).unwrap(),
            ..Default::default()
        };
        let initial_repeat_offsets = raw_prepared.as_inner().repeat_offsets();

        // Rust: compress and decode block 0 sequences
        let rust_compressed =
            encode_all_with_prepared_dict_and_options(&case.input, &raw_prepared, options).unwrap();
        let rust_block0 =
            decoded_compressed_block_sequences(&rust_compressed, 0, initial_repeat_offsets);
        let rust = &rust_block0.sequences;

        // C: compress and decode block 0 sequences
        let c_compressed = upstream_trace_helper::compress_once(
            helper,
            "compress-raw-dict-configured",
            16,
            false,
            &case.input,
        );
        let c_block0 = decoded_compressed_block_sequences(&c_compressed, 0, initial_repeat_offsets);
        let upstream = &c_block0.sequences;

        let core_mismatch = rust
            .iter()
            .zip(upstream.iter())
            .enumerate()
            .find_map(|(i, (r, u))| {
                if r.literal_length != u.literal_length
                    || r.length != u.length
                    || r.off_base != u.off_base
                {
                    Some(i)
                } else {
                    None
                }
            });
        let core_mismatch = core_mismatch.unwrap_or(rust.len().min(upstream.len()));

        eprintln!(
            "raw-dictionary L16 rust_count={} upstream_count={} core_mismatch_at={}",
            rust.len(),
            upstream.len(),
            core_mismatch,
        );

        // Show window around divergence
        let window_start = core_mismatch.saturating_sub(3);
        let window_end = core_mismatch + 10;
        eprintln!("  rust[{window_start}..{}]:", rust.len().min(window_end));
        for i in window_start..rust.len().min(window_end) {
            let s = &rust[i];
            let marker = if i == core_mismatch { " <--" } else { "" };
            eprintln!(
                "    [{:>4}] start={:>6} litlen={:>4} mlen={:>4} off_base={:>6} offset={:>6}{marker}",
                i, s.start, s.literal_length, s.length, s.off_base, s.offset,
            );
        }
        eprintln!(
            "  upstream[{window_start}..{}]:",
            upstream.len().min(window_end)
        );
        for i in window_start..upstream.len().min(window_end) {
            let s = &upstream[i];
            let marker = if i == core_mismatch { " <--" } else { "" };
            eprintln!(
                "    [{:>4}] start={:>6} litlen={:>4} mlen={:>4} off_base={:>6} offset={:>6}{marker}",
                i, s.start, s.literal_length, s.length, s.off_base, s.offset,
            );
        }

        // Summary stats
        let rust_matched: usize = rust.iter().map(|s| s.length).sum();
        let upstream_matched: usize = upstream.iter().map(|s| s.length).sum();
        let rust_lit: usize = rust.iter().map(|s| s.literal_length).sum();
        let upstream_lit: usize = upstream.iter().map(|s| s.literal_length).sum();
        eprintln!(
            "  rust total: matched={rust_matched} literal={rust_lit} seqs={}",
            rust.len()
        );
        eprintln!(
            "  upstream total: matched={upstream_matched} literal={upstream_lit} seqs={}",
            upstream.len()
        );
        if !rust.is_empty() && !upstream.is_empty() {
            eprintln!(
                "  rust avg mlen={:.1} upstream avg mlen={:.1}",
                rust_matched as f64 / rust.len() as f64,
                upstream_matched as f64 / upstream.len() as f64,
            );
        }
    }

    #[test]
    fn print_trained_dictionary_l17_optimal_sequences() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let prepared = EncoderDictionary::new(&dictionary).expect("trained dictionary must parse");
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "trained-dictionary")
            .expect("trained-dictionary benchmark case should exist");
        let options = EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::try_new(17).unwrap(),
            ..Default::default()
        };
        let initial_repeat_offsets = prepared.as_inner().repeat_offsets();

        // Rust: compress and decode block 0 sequences
        let rust_compressed =
            encode_all_with_prepared_dict_and_options(&case.input, &prepared, options).unwrap();
        let rust_block0 =
            decoded_compressed_block_sequences(&rust_compressed, 0, initial_repeat_offsets);
        let rust = &rust_block0.sequences;

        // C: compress and decode block 0 sequences
        let c_compressed = upstream_trace_helper::compress_once(
            helper,
            "compress-trained-dict-configured",
            17,
            false,
            &case.input,
        );
        let c_block0 = decoded_compressed_block_sequences(&c_compressed, 0, initial_repeat_offsets);
        let upstream = &c_block0.sequences;

        let core_mismatch = rust
            .iter()
            .zip(upstream.iter())
            .enumerate()
            .find_map(|(i, (r, u))| {
                if r.literal_length != u.literal_length
                    || r.length != u.length
                    || r.off_base != u.off_base
                {
                    Some(i)
                } else {
                    None
                }
            });
        let core_mismatch = core_mismatch.unwrap_or(rust.len().min(upstream.len()));

        eprintln!(
            "trained-dictionary L17 rust_count={} upstream_count={} core_mismatch_at={}",
            rust.len(),
            upstream.len(),
            core_mismatch,
        );

        let window_start = core_mismatch.saturating_sub(3);
        let window_end = core_mismatch + 10;
        eprintln!("  rust[{window_start}..{}]:", rust.len().min(window_end));
        for i in window_start..rust.len().min(window_end) {
            let s = &rust[i];
            let marker = if i == core_mismatch { " <--" } else { "" };
            eprintln!(
                "    [{:>4}] start={:>6} litlen={:>4} mlen={:>4} off_base={:>6} offset={:>6}{marker}",
                i, s.start, s.literal_length, s.length, s.off_base, s.offset,
            );
        }
        eprintln!(
            "  upstream[{window_start}..{}]:",
            upstream.len().min(window_end)
        );
        for i in window_start..upstream.len().min(window_end) {
            let s = &upstream[i];
            let marker = if i == core_mismatch { " <--" } else { "" };
            eprintln!(
                "    [{:>4}] start={:>6} litlen={:>4} mlen={:>4} off_base={:>6} offset={:>6}{marker}",
                i, s.start, s.literal_length, s.length, s.off_base, s.offset,
            );
        }

        let rust_matched: usize = rust.iter().map(|s| s.length).sum();
        let upstream_matched: usize = upstream.iter().map(|s| s.length).sum();
        let rust_lit: usize = rust.iter().map(|s| s.literal_length).sum();
        let upstream_lit: usize = upstream.iter().map(|s| s.literal_length).sum();
        eprintln!(
            "  rust total: matched={rust_matched} literal={rust_lit} seqs={}",
            rust.len()
        );
        eprintln!(
            "  upstream total: matched={upstream_matched} literal={upstream_lit} seqs={}",
            upstream.len()
        );
        if !rust.is_empty() && !upstream.is_empty() {
            eprintln!(
                "  rust avg mlen={:.1} upstream avg mlen={:.1}",
                rust_matched as f64 / rust.len() as f64,
                upstream_matched as f64 / upstream.len() as f64,
            );
        }
    }

    #[test]
    fn print_trained_dictionary_l14_optimal_sequences() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let prepared = EncoderDictionary::new(&dictionary).expect("trained dictionary must parse");
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "trained-dictionary")
            .expect("trained-dictionary benchmark case should exist");
        let level = 14u8;
        let options = EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
            ..Default::default()
        };
        let initial_repeat_offsets = prepared.as_inner().repeat_offsets();

        // Rust: compress and decode block 0 sequences
        let rust_compressed =
            encode_all_with_prepared_dict_and_options(&case.input, &prepared, options).unwrap();
        let rust_block0 =
            decoded_compressed_block_sequences(&rust_compressed, 0, initial_repeat_offsets);
        let rust = &rust_block0.sequences;

        // C: compress and decode block 0 sequences
        let c_compressed = upstream_trace_helper::compress_once(
            helper,
            "compress-trained-dict-configured",
            level as i32,
            false,
            &case.input,
        );
        let c_block0 = decoded_compressed_block_sequences(&c_compressed, 0, initial_repeat_offsets);
        let upstream = &c_block0.sequences;

        let core_mismatch = rust
            .iter()
            .zip(upstream.iter())
            .enumerate()
            .find_map(|(i, (r, u))| {
                if r.literal_length != u.literal_length
                    || r.length != u.length
                    || r.off_base != u.off_base
                {
                    Some(i)
                } else {
                    None
                }
            });
        let core_mismatch = core_mismatch.unwrap_or(rust.len().min(upstream.len()));

        eprintln!(
            "trained-dictionary L{level} rust_count={} upstream_count={} core_mismatch_at={}",
            rust.len(),
            upstream.len(),
            core_mismatch,
        );

        let window_start = core_mismatch.saturating_sub(3);
        let window_end = core_mismatch + 10;
        eprintln!("  rust[{window_start}..{}]:", rust.len().min(window_end));
        for i in window_start..rust.len().min(window_end) {
            let s = &rust[i];
            let marker = if i == core_mismatch { " <--" } else { "" };
            eprintln!(
                "    [{:>4}] start={:>6} litlen={:>4} mlen={:>4} off_base={:>6} offset={:>6}{marker}",
                i, s.start, s.literal_length, s.length, s.off_base, s.offset,
            );
        }
        eprintln!(
            "  upstream[{window_start}..{}]:",
            upstream.len().min(window_end)
        );
        for i in window_start..upstream.len().min(window_end) {
            let s = &upstream[i];
            let marker = if i == core_mismatch { " <--" } else { "" };
            eprintln!(
                "    [{:>4}] start={:>6} litlen={:>4} mlen={:>4} off_base={:>6} offset={:>6}{marker}",
                i, s.start, s.literal_length, s.length, s.off_base, s.offset,
            );
        }

        let rust_matched: usize = rust.iter().map(|s| s.length).sum();
        let upstream_matched: usize = upstream.iter().map(|s| s.length).sum();
        let rust_lit: usize = rust.iter().map(|s| s.literal_length).sum();
        let upstream_lit: usize = upstream.iter().map(|s| s.literal_length).sum();
        eprintln!(
            "  rust total: matched={rust_matched} literal={rust_lit} seqs={}",
            rust.len()
        );
        eprintln!(
            "  upstream total: matched={upstream_matched} literal={upstream_lit} seqs={}",
            upstream.len()
        );

        // --- Divergence analysis: verify the upstream match exists in the data ---
        if core_mismatch < upstream.len() {
            let u = &upstream[core_mismatch];
            let dict_content = prepared.as_inner().matching_content();
            let block_end = case.input.len().min(128 * 1024);
            let block = &case.input[..block_end];
            // Compute source-relative position of the match
            // start = accumulated position + literal_length
            // The match starts at source position `start` (already includes litlen)
            let match_src_pos = u.start;
            let offset = u.offset;
            eprintln!("\n  --- divergence analysis ---");
            eprintln!(
                "  upstream match: src_pos={match_src_pos} offset={offset} mlen={}",
                u.length
            );
            eprintln!(
                "  dict_len={} block_len={}",
                dict_content.len(),
                block.len()
            );
            if offset > match_src_pos {
                // Dictionary match: match_target = dict[dict_len - (offset - match_src_pos)]
                let dict_offset = offset - match_src_pos;
                let dict_target_start = dict_content.len().wrapping_sub(dict_offset);
                eprintln!(
                    "  dict match: dict_target_start={dict_target_start} dict_offset_from_end={dict_offset}"
                );
                if dict_target_start < dict_content.len() {
                    // Verify the match exists by comparing bytes
                    let mut verify_len = 0usize;
                    let src_bytes = &block[match_src_pos..];
                    while verify_len < src_bytes.len() {
                        let dict_pos = dict_target_start + verify_len;
                        let byte = if dict_pos < dict_content.len() {
                            dict_content[dict_pos]
                        } else {
                            // Crossed into source region
                            let src_offset = dict_pos - dict_content.len();
                            if src_offset < block.len() {
                                block[src_offset]
                            } else {
                                break;
                            }
                        };
                        if byte != src_bytes[verify_len] {
                            break;
                        }
                        verify_len += 1;
                    }
                    eprintln!(
                        "  verified match length: {verify_len} (expected {})",
                        u.length
                    );
                    if verify_len >= 4 {
                        eprintln!(
                            "  match bytes (first 16): {:02x?}",
                            &src_bytes[..16.min(src_bytes.len())]
                        );
                        let dict_bytes: Vec<u8> = (0..16.min(verify_len))
                            .map(|i| {
                                let dp = dict_target_start + i;
                                if dp < dict_content.len() {
                                    dict_content[dp]
                                } else {
                                    block[dp - dict_content.len()]
                                }
                            })
                            .collect();
                        eprintln!("  dict  bytes (first 16): {dict_bytes:02x?}");
                    }
                }
            } else {
                eprintln!("  source match: target_pos={}", match_src_pos - offset);
            }
        }
    }

    #[test]
    fn print_log_lines_l17_optimal_sequences() {
        compare_optimal_sequences("log-lines", 17);
    }

    fn compare_optimal_sequences(case_name: &str, level: u8) {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == case_name)
            .unwrap_or_else(|| panic!("{case_name} benchmark case should exist"));
        let options = EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
            ..Default::default()
        };

        // C: trace pre-split sequences for all blocks
        let c_traces = upstream_trace_helper::trace_regular_sequences(
            helper,
            level as i32,
            false,
            50000,
            &case.input,
        );

        // Compare each block separately
        let block_size = 128 * 1024usize;
        let num_blocks = case.input.len().div_ceil(block_size);
        for block_idx in 0..num_blocks {
            let block_start = block_idx * block_size;
            let block_end = ((block_idx + 1) * block_size).min(case.input.len());
            let c_block: Vec<_> = c_traces
                .iter()
                .filter(|t| t.start >= block_start && t.start < block_end)
                .collect();
            eprintln!(
                "  block {block_idx}: c_seqs={} (pos {block_start}..{block_end})",
                c_block.len()
            );
        }

        // For block 0: get Rust planned sequences and compare
        let rust_plan = planned_first_block_trace_without_dict(&case.input, options).unwrap();
        let rust = &rust_plan.emitted_matches;
        let c_block0: Vec<_> = c_traces.iter().filter(|t| t.start < block_size).collect();
        let upstream = &c_block0;

        // Compare Rust decoded sequences vs C traced sequences
        let core_mismatch = rust
            .iter()
            .zip(upstream.iter())
            .enumerate()
            .find_map(|(i, (r, u))| {
                if r.literal_length != u.literal_length
                    || r.length != u.match_length
                    || r.off_base != u.off_base
                {
                    Some(i)
                } else {
                    None
                }
            });
        let core_mismatch = core_mismatch.unwrap_or(rust.len().min(upstream.len()));

        eprintln!(
            "{case_name} L{level} rust_count={} upstream_count={} core_mismatch_at={}",
            rust.len(),
            upstream.len(),
            core_mismatch,
        );

        let window_start = core_mismatch.saturating_sub(3);
        let window_end = core_mismatch + 10;
        eprintln!("  rust[{window_start}..{}]:", rust.len().min(window_end));
        for i in window_start..rust.len().min(window_end) {
            let s = &rust[i];
            let marker = if i == core_mismatch { " <--" } else { "" };
            eprintln!(
                "    [{:>4}] start={:>6} litlen={:>4} mlen={:>4} off_base={:>6} offset={:>6}{marker}",
                i, s.start, s.literal_length, s.length, s.off_base, s.offset,
            );
        }
        eprintln!(
            "  upstream[{window_start}..{}]:",
            upstream.len().min(window_end)
        );
        for i in window_start..upstream.len().min(window_end) {
            let s = &upstream[i];
            let marker = if i == core_mismatch { " <--" } else { "" };
            eprintln!(
                "    [{:>4}] start={:>6} litlen={:>4} mlen={:>4} off_base={:>6} offset={:>6}{marker}",
                i, s.start, s.literal_length, s.match_length, s.off_base, s.raw_offset,
            );
        }

        let rust_matched: usize = rust.iter().map(|s| s.length).sum();
        let upstream_matched: usize = upstream.iter().map(|s| s.match_length).sum();
        let rust_lit: usize = rust.iter().map(|s| s.literal_length).sum();
        let upstream_lit: usize = upstream.iter().map(|s| s.literal_length).sum();
        eprintln!(
            "  rust total: matched={rust_matched} literal={rust_lit} seqs={}",
            rust.len()
        );
        eprintln!(
            "  upstream total: matched={upstream_matched} literal={upstream_lit} seqs={}",
            upstream.len()
        );
    }

    #[test]
    fn print_json_records_l18_optimal_sequences() {
        compare_optimal_sequences("json-records", 18);
    }

    #[test]
    fn print_tabular_csv_l16_optimal_sequences() {
        compare_optimal_sequences("tabular-csv", 16);
    }

    #[test]
    fn dump_trained_dict_applied_cparams_l14() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "trained-dictionary")
            .expect("trained-dictionary benchmark case should exist");
        let dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let prepared = EncoderDictionary::new(&dictionary).unwrap();
        eprintln!(
            "  dict_full_size={} content_size={}",
            dictionary.len(),
            prepared.as_inner().source_size()
        );
        eprintln!("  input_size={}", case.input.len());

        for level in [14, 15, 16, 17, 18] {
            let upstream_cparams = upstream_trace_helper::trace_trained_dict_applied_cparams(
                helper,
                level,
                false,
                &case.input,
            );
            let rust_params = compression_parameters_for_input(
                CompressionLevel::try_new(level).unwrap(),
                Some(case.input.len()),
                Some(prepared.as_inner()),
            );
            eprintln!(
                "  L{level}: C applied: wlog={} clog={} hlog={} slog={} mml={} tlen={} strat={}",
                upstream_cparams.window_log,
                upstream_cparams.chain_log,
                upstream_cparams.hash_log,
                upstream_cparams.search_log,
                upstream_cparams.min_match,
                upstream_cparams.target_length,
                upstream_cparams.strategy,
            );
            eprintln!(
                "  L{level}: Rust: parser={:?} attaches={} hash={} clog={} depth={} \
                 dict_hash={} dict_clog={}",
                rust_params.match_finder.parser_strategy,
                rust_params.match_finder.dictionary_attaches,
                rust_params.match_finder.hash_bits,
                rust_params.match_finder.chain_log,
                rust_params.match_finder.search_depth,
                rust_params.match_finder.dictionary_hash_bits(),
                rust_params.match_finder.dictionary_chain_log(),
            );
        }
    }

    #[test]
    fn compare_cdict_bt_state_l17() {
        use crate::window::{BinaryTreeFinder, PrefixChain};

        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };
        let dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "trained-dictionary")
            .expect("trained-dictionary case");

        // Get C's CDict BST state
        let c_state = upstream_trace_helper::dump_cdict_bt_state(helper, 17, false, &case.input);
        let c_hash_log: u32 = c_state["cdict_hashLog"].parse().unwrap();
        let c_chain_log: u32 = c_state["cdict_chainLog"].parse().unwrap();
        let c_min_match: u32 = c_state["cdict_minMatch"].parse().unwrap();
        let c_search_log: u32 = c_state["cdict_searchLog"].parse().unwrap();
        let c_bt_mask: u32 = c_state["cdict_btMask"].parse().unwrap();
        let c_heads_hash: u64 = c_state["cdict_heads_hash"].parse().unwrap();
        let c_children_hash: u64 = c_state["cdict_children_hash"].parse().unwrap();
        let c_dict_size: usize = c_state["cdict_dictSize"].parse().unwrap();
        let c_low_limit: u32 = c_state["cdict_lowLimit"].parse().unwrap();
        let c_dict_limit: u32 = c_state["cdict_dictLimit"].parse().unwrap();

        eprintln!("C CDict BST state (all keys):");
        let mut keys: Vec<_> = c_state.keys().collect();
        keys.sort();
        for k in &keys {
            eprintln!("  {k} = {}", c_state[*k]);
        }
        eprintln!("\nC CDict BST state:");
        eprintln!(
            "  hashLog={c_hash_log} chainLog={c_chain_log} minMatch={c_min_match} searchLog={c_search_log}"
        );
        eprintln!("  btMask={c_bt_mask} dictSize={c_dict_size}");
        eprintln!("  lowLimit={c_low_limit} dictLimit={c_dict_limit}");
        eprintln!("  heads_hash={c_heads_hash} children_hash={c_children_hash}");
        if let Some(h688) = c_state.get("cdict_heads_688") {
            eprintln!("  heads[688]={h688}");
        }

        // Build Rust's dict BST with the dictionary's own parameters, which is
        // what C's `dms->cParams` holds. Reading them off `applied` — as this
        // did, under a comment claiming they were already the CDict's — is the
        // defect the dictionary geometry fields were added for: applied is the
        // CDict's parameters re-fitted to the *source*, so on a source smaller
        // than the dictionary its table logs are several bits narrower.
        let resolved = upstream_full_dict_cparams_for_level(
            CompressionLevel::try_new(17).unwrap(),
            Some(case.input.len()),
            dictionary.len(),
            ParameterOverrides::default(),
        );
        let params = internal_compression_parameters_from_upstream(
            resolved.applied,
            resolved.use_row_match_finder,
        )
        .match_finder;
        let dms_hash_bits = resolved.cdict.hash_log;
        let dms_chain_log = resolved.cdict.chain_log;
        let dms_min_match = params.min_match;
        eprintln!("\nRust dict BST params:");
        eprintln!(
            "  hash_bits={dms_hash_bits} chain_log={dms_chain_log} min_match={dms_min_match}"
        );

        let prepared = EncoderDictionary::new(&dictionary).expect("trained dictionary must parse");
        let content = prepared.as_inner().content();
        eprintln!(
            "  dict_full_size={} content_size={}",
            dictionary.len(),
            content.len()
        );
        let prefix_refs = [content];
        let prefix_chain = PrefixChain::new(&prefix_refs).unwrap().unwrap();
        let mut bt = BinaryTreeFinder::new(dms_hash_bits, dms_chain_log, dms_min_match);
        bt.zero_tables();
        let dms_search_depth = params.search_depth.max(64);
        eprintln!("  search_depth={dms_search_depth}");
        bt.insert_prefix_into_unified(prefix_chain, dms_search_depth, params.window_log);

        // Verify content bytes and hashes match C
        if let Some(c_first8) = c_state.get("cdict_content_first8") {
            let c_bytes: Vec<u8> = c_first8
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            let r_bytes: Vec<u8> = (0..8.min(content.len())).map(|i| content[i]).collect();
            eprintln!(
                "  content first 8: C={c_bytes:?} Rust={r_bytes:?} match={}",
                c_bytes == r_bytes
            );
        }
        if let Some(c_h0) = c_state.get("cdict_hash_pos0") {
            use crate::window::hash_bytes_for_min_match;
            let c_h0: u32 = c_h0.parse().unwrap();
            let r_bytes: [u8; 8] =
                std::array::from_fn(|i| if i < content.len() { content[i] } else { 0 });
            let r_h0 = hash_bytes_for_min_match(r_bytes, dms_hash_bits, dms_min_match) as u32;
            eprintln!("  hash(pos0): C={c_h0} Rust={r_h0} match={}", c_h0 == r_h0);
        }
        if let Some(c_cs) = c_state.get("cdict_contentSize") {
            let c_cs: usize = c_cs.parse().unwrap();
            eprintln!(
                "  contentSize: C={c_cs} Rust={} match={}",
                content.len(),
                c_cs == content.len()
            );
        }

        // Compare parameters
        eprintln!("\nParameter comparison:");
        eprintln!(
            "  hashLog: C={c_hash_log} Rust={dms_hash_bits} match={}",
            c_hash_log == dms_hash_bits
        );
        eprintln!(
            "  chainLog: C={c_chain_log} Rust={dms_chain_log} match={}",
            c_chain_log == dms_chain_log
        );
        eprintln!(
            "  minMatch: C={c_min_match} Rust={dms_min_match} match={}",
            c_min_match == dms_min_match
        );
        eprintln!(
            "  btMask: C={c_bt_mask} Rust={} match={}",
            bt.bt_mask,
            c_bt_mask as usize == bt.bt_mask
        );

        // Compare heads array hash
        let rust_hash_size = 1usize << dms_hash_bits;
        let mut rust_heads_hash: u64 = 0;
        for (i, &h) in bt.heads.iter().enumerate().take(rust_hash_size) {
            rust_heads_hash ^= (h as u64).wrapping_mul((i as u64 + 1).wrapping_mul(2654435761));
        }
        eprintln!(
            "\nHeads hash: C={c_heads_hash} Rust={rust_heads_hash} match={}",
            c_heads_hash == rust_heads_hash
        );

        // Compare children array hash
        let rust_bt_size = 1usize << (dms_chain_log.saturating_sub(1).max(1));
        let mut rust_children_hash: u64 = 0;
        for (i, &c) in bt.children.iter().enumerate().take(rust_bt_size * 2) {
            rust_children_hash ^= (c as u64).wrapping_mul((i as u64 + 1).wrapping_mul(2654435761));
        }
        eprintln!(
            "Children hash: C={c_children_hash} Rust={rust_children_hash} match={}",
            c_children_hash == rust_children_hash
        );

        // If children differ, find first divergent slot
        if c_children_hash != rust_children_hash {
            if let Some(sample) = c_state.get("cdict_children_sample") {
                let c_children: Vec<u32> = sample
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                eprintln!("  First divergent children:");
                let mut diffs = 0;
                for i in 0..rust_bt_size * 2 {
                    let rc = bt.children[i];
                    let cc = if i < c_children.len() {
                        c_children[i]
                    } else {
                        u32::MAX
                    };
                    if i < c_children.len() && rc != cc && diffs < 10 {
                        eprintln!("    children[{i}]: C={cc} Rust={rc}");
                        diffs += 1;
                    }
                }
                // Also show first few non-zero Rust children
                eprintln!("  First 5 non-zero Rust children:");
                let mut nz = 0;
                for (i, &c) in bt.children.iter().enumerate() {
                    if c != 0 && nz < 5 {
                        eprintln!("    children[{i}] = {c}");
                        nz += 1;
                    }
                }
            }
        }

        // If heads differ, find first divergent slot
        if c_heads_hash != rust_heads_hash {
            if let Some(sample) = c_state.get("cdict_heads_sample") {
                let c_heads: Vec<u32> = sample
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                eprintln!("\nFirst 32 heads comparison:");
                for (i, &ch) in c_heads.iter().enumerate() {
                    let rh = if i < bt.heads.len() {
                        bt.heads[i]
                    } else {
                        u32::MAX
                    };
                    if ch != rh {
                        eprintln!("  DIFF slot={i}: C={ch} Rust={rh}");
                    }
                }
            }
        }

        // Show specific slot
        if let Some(h688) = c_state.get("cdict_heads_688") {
            let c_688: u32 = h688.parse().unwrap_or(0);
            let r_688 = if 688 < bt.heads.len() {
                bt.heads[688]
            } else {
                0
            };
            eprintln!(
                "  heads[688]: C={c_688} Rust={r_688} match={}",
                c_688 == r_688
            );
        }
    }

    #[test]
    fn print_json_records_l16_optimal_sequences() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "json-records")
            .expect("json-records benchmark case should exist");
        let options = EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::try_new(16).unwrap(),
            ..Default::default()
        };

        // Rust: compress and decode block 0 sequences
        let rust_compressed = encode_all_with_options(&case.input, options).unwrap();
        let rust_block0 =
            decoded_compressed_block_sequences(&rust_compressed, 0, RepeatOffsets::default());
        let rust = &rust_block0.sequences;

        // C: compress and decode block 0 sequences (same actual encoder path)
        let c_compressed = upstream_trace_helper::compress_once(
            helper,
            "compress-regular-configured",
            16,
            false,
            &case.input,
        );
        let c_block0 =
            decoded_compressed_block_sequences(&c_compressed, 0, RepeatOffsets::default());
        let upstream = &c_block0.sequences;

        let core_mismatch = rust
            .iter()
            .zip(upstream.iter())
            .enumerate()
            .find_map(|(i, (r, u))| {
                if r.literal_length != u.literal_length
                    || r.length != u.length
                    || r.off_base != u.off_base
                {
                    Some(i)
                } else {
                    None
                }
            });
        let core_mismatch = core_mismatch.unwrap_or(rust.len().min(upstream.len()));

        eprintln!(
            "json-records L16 rust_count={} upstream_count={} core_mismatch_at={}",
            rust.len(),
            upstream.len(),
            core_mismatch,
        );

        let window_start = core_mismatch.saturating_sub(3);
        let window_end = core_mismatch + 10;
        eprintln!("  rust[{window_start}..{}]:", rust.len().min(window_end));
        for i in window_start..rust.len().min(window_end) {
            let s = &rust[i];
            let marker = if i == core_mismatch { " <--" } else { "" };
            eprintln!(
                "    [{:>4}] start={:>6} litlen={:>4} mlen={:>4} off_base={:>6} offset={:>6}{marker}",
                i, s.start, s.literal_length, s.length, s.off_base, s.offset,
            );
        }
        eprintln!(
            "  upstream[{window_start}..{}]:",
            upstream.len().min(window_end)
        );
        for i in window_start..upstream.len().min(window_end) {
            let s = &upstream[i];
            let marker = if i == core_mismatch { " <--" } else { "" };
            eprintln!(
                "    [{:>4}] start={:>6} litlen={:>4} mlen={:>4} off_base={:>6} offset={:>6}{marker}",
                i, s.start, s.literal_length, s.length, s.off_base, s.offset,
            );
        }

        let rust_matched: usize = rust.iter().map(|s| s.length).sum();
        let upstream_matched: usize = upstream.iter().map(|s| s.length).sum();
        let rust_lit: usize = rust.iter().map(|s| s.literal_length).sum();
        let upstream_lit: usize = upstream.iter().map(|s| s.literal_length).sum();
        eprintln!(
            "  rust total: matched={rust_matched} literal={rust_lit} seqs={}",
            rust.len()
        );
        eprintln!(
            "  upstream total: matched={upstream_matched} literal={upstream_lit} seqs={}",
            upstream.len()
        );
        if !rust.is_empty() && !upstream.is_empty() {
            eprintln!(
                "  rust avg mlen={:.1} upstream avg mlen={:.1}",
                rust_matched as f64 / rust.len() as f64,
                upstream_matched as f64 / upstream.len() as f64,
            );
        }
    }

    #[test]
    fn print_wikipedia_l16_full_sequence_comparison() {
        print_wikipedia_level_full_sequence_comparison(16);
    }

    #[test]
    fn print_wikipedia_l18_full_sequence_comparison() {
        print_wikipedia_level_full_sequence_comparison(18);
    }

    #[test]
    fn print_wikipedia_l19_full_sequence_comparison() {
        print_wikipedia_level_full_sequence_comparison(19);
    }

    fn print_wikipedia_level_full_sequence_comparison(level: u8) {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "wikipedia")
            .expect("wikipedia benchmark case should exist");
        let options = EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
            ..Default::default()
        };

        let rust_compressed = encode_all_with_options(&case.input, options).unwrap();
        let c_compressed = upstream_trace_helper::compress_once(
            helper,
            "compress-regular-configured",
            level as i32,
            false,
            &case.input,
        );
        eprintln!(
            "wikipedia L{level} rust_size={} c_size={} gap={:.2}%",
            rust_compressed.len(),
            c_compressed.len(),
            (rust_compressed.len() as f64 / c_compressed.len() as f64 - 1.0) * 100.0,
        );

        // Decode ALL compressed blocks from both frames and collect the
        // full sequence stream. Track source position via litlen + mlen.
        struct DecodedFrame {
            seqs: Vec<SequenceCommand>,
            block_sizes: Vec<usize>,      // compressed payload sizes
            block_seq_counts: Vec<usize>, // sequences per block
        }
        fn decode_all_sequences(frame: &[u8]) -> DecodedFrame {
            let header = match crate::parse_frame_header(frame).unwrap() {
                crate::FrameHeader::Zstandard(h) => h,
                _ => panic!(),
            };
            let mut offset = header.header_size;
            let mut all_seqs = Vec::new();
            let mut block_sizes = Vec::new();
            let mut block_seq_counts = Vec::new();
            let mut seq_tables = SequenceTablesState::default();
            let mut literals_state = crate::literals::LiteralsState::default();
            loop {
                let block = crate::parse_block_header(&frame[offset..]).unwrap();
                let payload_start = offset + BlockHeader::SIZE;
                let payload_end = payload_start + block.payload_size();
                let payload = &frame[payload_start..payload_end];
                block_sizes.push(block.payload_size());
                if block.block_type == BlockType::Compressed {
                    let (_, lit_size) = crate::literals::decode_literals_section(
                        payload,
                        &mut literals_state,
                        BLOCK_SIZE_MAX,
                    )
                    .unwrap();
                    let seq_section = &payload[lit_size..];
                    let parsed =
                        parse_sequence_section(seq_section, &mut seq_tables, TableTarget::Both)
                            .unwrap();
                    let commands = decode_sequence_commands(&parsed, &seq_tables).unwrap();
                    block_seq_counts.push(commands.len());
                    all_seqs.extend_from_slice(&commands);
                } else {
                    block_seq_counts.push(0);
                }
                if block.last_block {
                    break;
                }
                offset = payload_end;
            }
            DecodedFrame {
                seqs: all_seqs,
                block_sizes,
                block_seq_counts,
            }
        }

        let rust_frame = decode_all_sequences(&rust_compressed);
        let c_frame = decode_all_sequences(&c_compressed);
        eprintln!(
            "Total sequences: rust={} c={}",
            rust_frame.seqs.len(),
            c_frame.seqs.len()
        );
        eprintln!("Per-block compressed sizes:");
        for (i, (rs, cs)) in rust_frame
            .block_sizes
            .iter()
            .zip(c_frame.block_sizes.iter())
            .enumerate()
        {
            let rsc = rust_frame.block_seq_counts.get(i).copied().unwrap_or(0);
            let csc = c_frame.block_seq_counts.get(i).copied().unwrap_or(0);
            let gap = if *cs > 0 {
                (*rs as f64 / *cs as f64 - 1.0) * 100.0
            } else {
                0.0
            };
            eprintln!("  block {i}: rust={rs} ({rsc} seqs) c={cs} ({csc} seqs) gap={gap:+.1}%");
        }

        let rust_seqs = &rust_frame.seqs;
        let c_seqs = &c_frame.seqs;

        // Find first mismatch by tracking source position
        let mut rust_pos = 0usize;
        let mut first_mismatch = rust_seqs.len().min(c_seqs.len());
        for (i, (r, c)) in rust_seqs.iter().zip(c_seqs.iter()).enumerate() {
            if r.literal_length != c.literal_length
                || r.match_length != c.match_length
                || r.offset_value != c.offset_value
            {
                first_mismatch = i;
                break;
            }
            rust_pos += r.literal_length as usize + r.match_length as usize;
        }
        if first_mismatch == rust_seqs.len().min(c_seqs.len()) && rust_seqs.len() != c_seqs.len() {
            first_mismatch = rust_seqs.len().min(c_seqs.len());
        }
        eprintln!(
            "First mismatch at seq {}, source_pos ~{}",
            first_mismatch, rust_pos
        );

        let w_start = first_mismatch.saturating_sub(3);
        let w_end = first_mismatch + 20;
        let mut rp = 0usize;
        for i in 0..w_start {
            rp += rust_seqs[i].literal_length as usize + rust_seqs[i].match_length as usize;
        }
        eprintln!("  rust[{w_start}..{}]:", rust_seqs.len().min(w_end));
        for i in w_start..rust_seqs.len().min(w_end) {
            let s = &rust_seqs[i];
            let marker = if i == first_mismatch { " <--" } else { "" };
            eprintln!(
                "    [{:>5}] pos={:>6} litlen={:>4} mlen={:>4} offval={:>6}{marker}",
                i, rp, s.literal_length, s.match_length, s.offset_value,
            );
            rp += s.literal_length as usize + s.match_length as usize;
        }
        let mut cp = 0usize;
        for i in 0..w_start {
            cp += c_seqs[i].literal_length as usize + c_seqs[i].match_length as usize;
        }
        eprintln!("  c[{w_start}..{}]:", c_seqs.len().min(w_end));
        for i in w_start..c_seqs.len().min(w_end) {
            let s = &c_seqs[i];
            let marker = if i == first_mismatch { " <--" } else { "" };
            eprintln!(
                "    [{:>5}] pos={:>6} litlen={:>4} mlen={:>4} offval={:>6}{marker}",
                i, cp, s.literal_length, s.match_length, s.offset_value,
            );
            cp += s.literal_length as usize + s.match_length as usize;
        }
    }

    #[test]
    fn print_raw_dictionary_l18_full_sequence_comparison() {
        print_raw_dictionary_level_full_sequence_comparison(18);
    }

    #[test]
    fn print_raw_dictionary_l16_full_sequence_comparison() {
        print_raw_dictionary_level_full_sequence_comparison(16);
    }

    /// Every `(offset_value, literal_length)` combination that exercises a
    /// distinct branch of the repeat-offset rule, plus explicit offsets on
    /// both sides of the repcode range.
    ///
    /// The `literal_length == 0` rows are the point: they shift which slot a
    /// given `offset_value` names, and they are what a hand-written rep model
    /// gets wrong.
    #[test]
    fn trace_resolve_offsets_tracks_repeat_state_like_the_encoder() {
        // Walk one sequence at a time so a divergence is attributed to the
        // sequence that caused it rather than to the end of the run.
        let cases: &[(u32, u32)] = &[
            (1, 5),
            (1, 0),
            (2, 5),
            (2, 0),
            (3, 5),
            (3, 0),
            (4, 5),
            (9, 0),
            (2, 0),
            (3, 0),
            (2, 0),
            (1, 7),
            (3, 3),
        ];

        let mut sequences = Vec::new();
        let mut production = RepeatOffsets::default();
        let mut expected = Vec::new();
        for &(offset_value, literal_length) in cases {
            sequences.push(SequenceCommand {
                literal_length,
                match_length: 4,
                offset_value,
            });
            expected.push(production.resolve_encode(literal_length, offset_value));
        }

        let initial = RepeatOffsets::default();
        let resolved = resolve_offsets(&sequences, initial.values());
        let observed: Vec<u32> = resolved.iter().map(|&(_, offset, ..)| offset).collect();

        // The `rep1 - 1` case is only meaningful while rep1 is above 1; below
        // that it resolves to a zero offset, which is corrupt rather than a
        // behaviour worth pinning. Assert the run stays in valid territory so
        // a future edit to the case list cannot quietly compare two encoders'
        // handling of an impossible state.
        assert!(
            observed.iter().all(|&offset| offset > 0),
            "case list drove a repeat offset to zero: {observed:?}"
        );

        assert_eq!(
            observed, expected,
            "the trace helper's repeat-offset model diverged from the encoder's; \
             the sequences are {sequences:?}"
        );

        // And the flag the printer keys its REP marker off.
        let is_rep: Vec<bool> = resolved.iter().map(|&(.., is_rep)| is_rep).collect();
        let expected_rep: Vec<bool> = cases.iter().map(|&(ov, _)| ov <= 3).collect();
        assert_eq!(is_rep, expected_rep);
    }

    // Resolve rep offsets to actual offsets, tracking where rep chains establish
    fn resolve_offsets(
        seqs: &[SequenceCommand],
        initial_reps: [u32; 3],
    ) -> Vec<(usize, u32, u32, u32, bool)> {
        // Returns: (source_pos, resolved_offset, mlen, litlen, is_rep)
        let mut reps = initial_reps;
        let mut pos = 0usize;
        let mut result = Vec::new();
        for s in seqs {
            pos += s.literal_length as usize;
            // `repCode` from C's `ZSTD_updateRep`:
            // `OFFBASE_TO_REPCODE(offBase) - 1 + ll0`. Having no literals
            // shifts which slot a given `offset_value` names, because
            // "repeat the last offset" would be a no-op there and the
            // format reuses the code for the next slot instead.
            //
            // Selection and update share this one value deliberately. They
            // used to be computed separately, and the update half ignored
            // `ll0` entirely — so a sequence would resolve against rep2 and
            // then rotate as though it had used rep1.
            let rep_code = if s.offset_value <= 3 {
                debug_assert!(s.offset_value >= 1, "0 is not a valid offset_value");
                Some((s.offset_value + u32::from(s.literal_length == 0)).saturating_sub(1) as usize)
            } else {
                None
            };
            let (resolved, is_rep) = match rep_code {
                // `repCode == ZSTD_REP_NUM` names `rep1 - 1`, not a slot.
                Some(3) => (reps[0].wrapping_sub(1), true),
                Some(index) => (reps[index], true),
                None => (s.offset_value - 3, false),
            };
            result.push((pos, resolved, s.match_length, s.literal_length, is_rep));
            // `ZSTD_updateRep`. A `repCode` of 0 is already at the front, so
            // it changes nothing; every other case moves the resolved offset
            // to the front, and only a `repCode` of 2 or more pushes rep2
            // down into rep3.
            match rep_code {
                Some(0) => {}
                Some(index) => {
                    if index >= 2 {
                        reps[2] = reps[1];
                    }
                    reps[1] = reps[0];
                    reps[0] = resolved;
                }
                None => {
                    reps[2] = reps[1];
                    reps[1] = reps[0];
                    reps[0] = resolved;
                }
            }
            pos += s.match_length as usize;
        }
        result
    }

    fn print_raw_dictionary_level_full_sequence_comparison(level: u8) {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
        let raw_prepared =
            EncoderDictionary::new(&raw_dictionary).expect("raw dictionary must parse");
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "raw-dictionary")
            .expect("raw-dictionary benchmark case should exist");
        let options = EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
            ..Default::default()
        };

        let rust_compressed =
            encode_all_with_prepared_dict_and_options(&case.input, &raw_prepared, options).unwrap();
        let c_compressed = upstream_trace_helper::compress_once(
            helper,
            "compress-raw-dict-configured",
            level as i32,
            false,
            &case.input,
        );
        eprintln!(
            "raw-dictionary L{level} rust_size={} c_size={} gap={:.2}%",
            rust_compressed.len(),
            c_compressed.len(),
            (rust_compressed.len() as f64 / c_compressed.len() as f64 - 1.0) * 100.0,
        );

        // Decode ALL compressed blocks from both frames
        fn decode_all_sequences(frame: &[u8]) -> (Vec<SequenceCommand>, Vec<usize>) {
            let header = match crate::parse_frame_header(frame).unwrap() {
                crate::FrameHeader::Zstandard(h) => h,
                _ => panic!(),
            };
            let mut offset = header.header_size;
            let mut all_seqs = Vec::new();
            let mut block_boundaries = Vec::new();
            let mut seq_tables = SequenceTablesState::default();
            let mut literals_state = crate::literals::LiteralsState::default();
            loop {
                let block = crate::parse_block_header(&frame[offset..]).unwrap();
                let payload_start = offset + BlockHeader::SIZE;
                let payload_end = payload_start + block.payload_size();
                let payload = &frame[payload_start..payload_end];
                if block.block_type == BlockType::Compressed {
                    let (_, lit_size) = crate::literals::decode_literals_section(
                        payload,
                        &mut literals_state,
                        BLOCK_SIZE_MAX,
                    )
                    .unwrap();
                    let seq_section = &payload[lit_size..];
                    let parsed =
                        parse_sequence_section(seq_section, &mut seq_tables, TableTarget::Both)
                            .unwrap();
                    let commands = decode_sequence_commands(&parsed, &seq_tables).unwrap();
                    all_seqs.extend_from_slice(&commands);
                }
                block_boundaries.push(all_seqs.len());
                if block.last_block {
                    break;
                }
                offset = payload_end;
            }
            (all_seqs, block_boundaries)
        }

        let (rust_seqs, rust_blocks) = decode_all_sequences(&rust_compressed);
        let (c_seqs, c_blocks) = decode_all_sequences(&c_compressed);
        eprintln!(
            "Total sequences: rust={} c={} | blocks: rust={:?} c={:?}",
            rust_seqs.len(),
            c_seqs.len(),
            rust_blocks,
            c_blocks,
        );

        // Summary per compressed block
        let mut prev_r = 0;
        for (bi, &boundary) in rust_blocks.iter().enumerate() {
            let block_seqs = &rust_seqs[prev_r..boundary];
            let matched: u32 = block_seqs.iter().map(|s| s.match_length).sum();
            let literal: u32 = block_seqs.iter().map(|s| s.literal_length).sum();
            eprintln!(
                "  rust block {bi}: seqs={} matched={matched} literal={literal}",
                block_seqs.len()
            );
            prev_r = boundary;
        }
        let mut prev_c = 0;
        for (bi, &boundary) in c_blocks.iter().enumerate() {
            let block_seqs = &c_seqs[prev_c..boundary];
            let matched: u32 = block_seqs.iter().map(|s| s.match_length).sum();
            let literal: u32 = block_seqs.iter().map(|s| s.literal_length).sum();
            eprintln!(
                "  c block {bi}: seqs={} matched={matched} literal={literal}",
                block_seqs.len()
            );
            prev_c = boundary;
        }

        // Build source-position-indexed maps for comparison.
        // Each entry: (seq_idx, litlen, mlen, offval)
        let mut rust_by_pos: Vec<(usize, u32, u32, u32)> = Vec::new();
        let mut rp = 0u32;
        for (i, s) in rust_seqs.iter().enumerate() {
            rp += s.literal_length;
            rust_by_pos.push((i, s.literal_length, s.match_length, s.offset_value));
            rp += s.match_length;
        }
        let rust_total = rp;

        let mut c_by_pos: Vec<(usize, u32, u32, u32)> = Vec::new();
        let mut cp = 0u32;
        for (i, s) in c_seqs.iter().enumerate() {
            cp += s.literal_length;
            c_by_pos.push((i, s.literal_length, s.match_length, s.offset_value));
            cp += s.match_length;
        }
        let c_total = cp;
        eprintln!("Total source coverage: rust={rust_total} c={c_total}");

        // Find first mismatch
        let mut rust_pos = 0usize;
        let first_mismatch =
            rust_seqs
                .iter()
                .zip(c_seqs.iter())
                .enumerate()
                .find_map(|(i, (r, c))| {
                    if r.literal_length != c.literal_length
                        || r.match_length != c.match_length
                        || r.offset_value != c.offset_value
                    {
                        Some(i)
                    } else {
                        None
                    }
                });
        if let Some(fm) = first_mismatch {
            for s in &rust_seqs[..fm] {
                rust_pos += s.literal_length as usize + s.match_length as usize;
            }
            eprintln!("First mismatch at seq {fm}, source_pos ~{rust_pos}");
            let w_start = fm.saturating_sub(2);
            let w_end = fm + 8;
            // rust window
            let mut rp2 = 0usize;
            for s in &rust_seqs[..w_start] {
                rp2 += s.literal_length as usize + s.match_length as usize;
            }
            eprintln!("  rust:");
            for i in w_start..rust_seqs.len().min(w_end) {
                let s = &rust_seqs[i];
                let m = if i == fm { " <--" } else { "" };
                eprintln!(
                    "    [{i:>5}] pos={rp2:>6} ll={:>3} ml={:>4} off={:>6}{m}",
                    s.literal_length, s.match_length, s.offset_value
                );
                rp2 += s.literal_length as usize + s.match_length as usize;
            }
            let mut cp2 = 0usize;
            for s in &c_seqs[..w_start] {
                cp2 += s.literal_length as usize + s.match_length as usize;
            }
            eprintln!("  c:");
            for i in w_start..c_seqs.len().min(w_end) {
                let s = &c_seqs[i];
                let m = if i == fm { " <--" } else { "" };
                eprintln!(
                    "    [{i:>5}] pos={cp2:>6} ll={:>3} ml={:>4} off={:>6}{m}",
                    s.literal_length, s.match_length, s.offset_value
                );
                cp2 += s.literal_length as usize + s.match_length as usize;
            }
        }

        let initial_reps = [1, 4, 8]; // default repeat offsets
        let rust_resolved = resolve_offsets(&rust_seqs, initial_reps);
        let c_resolved = resolve_offsets(&c_seqs, initial_reps);

        // Show first 60 sequences with resolved offsets
        eprintln!("\nRust first 60 seqs (resolved offsets):");
        for (i, &(pos, off, ml, ll, is_rep)) in rust_resolved.iter().enumerate().take(60) {
            let rep_marker = if is_rep { "REP" } else { "   " };
            eprintln!("  [{i:>4}] pos={pos:>6} ll={ll:>3} ml={ml:>4} off={off:>6} {rep_marker}");
        }
        eprintln!("\nC first 60 seqs (resolved offsets):");
        for (i, &(pos, off, ml, ll, is_rep)) in c_resolved.iter().enumerate().take(60) {
            let rep_marker = if is_rep { "REP" } else { "   " };
            eprintln!("  [{i:>4}] pos={pos:>6} ll={ll:>3} ml={ml:>4} off={off:>6} {rep_marker}");
        }

        // Count rep vs non-rep in 10K windows
        eprintln!("\nRep offset usage per 10K window:");
        let step = 10_000;
        for w in (0..case.input.len()).step_by(step) {
            let end = (w + step).min(case.input.len());
            let rust_reps: usize = rust_resolved
                .iter()
                .filter(|r| r.0 >= w && r.0 < end && r.4)
                .count();
            let rust_total: usize = rust_resolved
                .iter()
                .filter(|r| r.0 >= w && r.0 < end)
                .count();
            let c_reps: usize = c_resolved
                .iter()
                .filter(|r| r.0 >= w && r.0 < end && r.4)
                .count();
            let c_total: usize = c_resolved.iter().filter(|r| r.0 >= w && r.0 < end).count();
            if rust_total > 0 || c_total > 0 {
                eprintln!(
                    "  [{w:>6}..{end:>6}] rust: {rust_reps:>3}/{rust_total:>3} rep | c: {c_reps:>3}/{c_total:>3} rep"
                );
            }
        }
    }

    #[test]
    fn print_raw_dictionary_l3_double_fast_candidates() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
        let raw_prepared =
            EncoderDictionary::new(&raw_dictionary).expect("raw dictionary must parse");
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "raw-dictionary")
            .expect("raw-dictionary benchmark case should exist");
        let cparams = compression_parameters_for_input(
            CompressionLevel::try_new(3).unwrap(),
            Some(case.input.len()),
            Some(raw_prepared.as_inner()),
        );
        assert_eq!(
            cparams.match_finder.parser_strategy,
            ParserStrategy::DoubleFast
        );
        let params = cparams.match_finder;
        let prepared =
            match build_prepared_dictionary_match_state(raw_prepared.as_inner().content(), params)
                .expect("prepared dictionary state must build")
            {
                PreparedDictionaryMatchState::DoubleFast(prepared) => prepared,
                _ => panic!("expected double-fast prepared dictionary state"),
            };
        let mut src_finder = DoubleFastFinder::new(
            params.hash_bits,
            params.secondary_hash_bits,
            params.min_match,
        );

        for pos in 0..1400 {
            src_finder.insert_src_position(&case.input, pos);
        }

        for pos in [
            203usize, 275, 1400, 1426, 1454, 1558, 1578, 2371, 2406, 2510,
        ] {
            let sht = hash_short_cache_src_at_mls(
                &case.input,
                pos,
                src_finder.short_hash_bits,
                src_finder.min_match,
            );
            let long_hash = hash_long_at(&case.input, pos, src_finder.long_hash_bits);
            let src_short_entry = src_finder.short_heads[tagged_index(sht)];
            let src_short = if src_short_entry != NO_TAGGED_ENTRY {
                tagged_pos(src_short_entry) as u32
            } else {
                NO_POS
            };
            let src_long = long_entry_pos(src_finder.long_entries[tagged_index(long_hash)]);
            let dict_short = prepared.short_candidate_at(&case.input, pos);
            let dict_long = prepared.long_candidate_at(&case.input, pos);
            eprintln!(
                "raw-dictionary L3 pos={pos} src_short={src_short} src_long={src_long} dict_short={dict_short:?} dict_long={dict_long:?}"
            );
        }
    }

    #[test]
    fn print_raw_dictionary_l3_extdict_double_fast_candidates() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
        let raw_prepared =
            EncoderDictionary::new(&raw_dictionary).expect("raw dictionary must parse");
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "raw-dictionary")
            .expect("raw-dictionary benchmark case should exist");
        let cparams = compression_parameters_for_input(
            CompressionLevel::try_new(3).unwrap(),
            Some(case.input.len()),
            Some(raw_prepared.as_inner()),
        );
        assert_eq!(
            cparams.match_finder.parser_strategy,
            ParserStrategy::DoubleFast
        );
        let params = cparams.match_finder;
        let mut prefix_finder = DoubleFastFinder::new(
            params.hash_bits,
            params.secondary_hash_bits,
            params.min_match,
        );
        prefix_finder.insert_prefix_ext_dict(raw_prepared.as_inner().content());
        let mut src_finder = DoubleFastFinder::new(
            params.hash_bits,
            params.secondary_hash_bits,
            params.min_match,
        );

        for pos in 0..1400 {
            src_finder.insert_src_position(&case.input, pos);
        }

        let prefix_len = raw_prepared.as_inner().content().len();
        for pos in [
            203usize, 275, 1400, 1426, 1454, 1558, 1578, 2371, 2406, 2510,
        ] {
            let sht = hash_short_cache_src_at_mls(
                &case.input,
                pos,
                src_finder.short_hash_bits,
                src_finder.min_match,
            );
            let long_hash = hash_long_at(&case.input, pos, src_finder.long_hash_bits);
            let src_short_entry = src_finder.short_heads[tagged_index(sht)];
            let src_short = if src_short_entry != NO_TAGGED_ENTRY {
                tagged_pos(src_short_entry) as u32
            } else {
                NO_POS
            };
            let src_long = long_entry_pos(src_finder.long_entries[tagged_index(long_hash)]);
            let prefix_short_entry = prefix_finder.short_heads[tagged_index(sht)];
            let prefix_short = if prefix_short_entry != NO_TAGGED_ENTRY {
                tagged_pos(prefix_short_entry) as u32
            } else {
                NO_POS
            };
            let prefix_long = long_entry_pos(prefix_finder.long_entries[tagged_index(long_hash)]);
            let prefix_short_logical = (prefix_short != NO_POS).then_some(prefix_short);
            let prefix_long_logical = (prefix_long != NO_POS).then_some(prefix_long);
            eprintln!(
                "raw-dictionary L3 extdict pos={pos} src_short={src_short} src_long={src_long} prefix_short={prefix_short_logical:?} prefix_long={prefix_long_logical:?} prefix_offset_short={:?} prefix_offset_long={:?}",
                prefix_short_logical.map(|candidate| prefix_len + pos - candidate as usize),
                prefix_long_logical.map(|candidate| prefix_len + pos - candidate as usize),
            );
        }
    }

    #[test]
    fn print_raw_dictionary_l3_upstream_extdict_double_fast_probes() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
        let raw_prepared =
            EncoderDictionary::new(&raw_dictionary).expect("raw dictionary must parse");
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "raw-dictionary")
            .expect("raw-dictionary benchmark case should exist");
        let params = compression_parameters_for_input(
            CompressionLevel::try_new(3).unwrap(),
            Some(case.input.len()),
            Some(raw_prepared.as_inner()),
        );
        eprintln!(
            "raw-dictionary L3 rust applied cparams window={} chain={} hash={} search={} min_match={} strategy={:?}",
            params.upstream_cparams.window_log,
            params.upstream_cparams.chain_log,
            params.upstream_cparams.hash_log,
            params.upstream_cparams.search_log,
            params.upstream_cparams.min_match,
            params.upstream_cparams.strategy,
        );

        for pos in [
            1398usize, 1399, 1400, 1401, 1402, 1454, 1578, 2369, 2370, 2371, 2372, 2373, 2406, 2510,
        ] {
            match std::panic::catch_unwind(|| {
                upstream_trace_helper::trace_raw_dict_extdict_double_fast_probe(
                    helper,
                    3,
                    false,
                    pos,
                    &case.input,
                )
            }) {
                Ok(output) => {
                    eprintln!(
                        "raw-dictionary L3 upstream extdict double-fast probe @ {pos}\n{output}"
                    );
                }
                Err(_) => {
                    eprintln!(
                        "raw-dictionary L3 upstream extdict double-fast probe @ {pos} failed"
                    );
                }
            }
        }
    }

    #[test]
    fn print_raw_dictionary_l3_dense_extdict_double_fast_match_lengths() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
        let raw_prepared =
            EncoderDictionary::new(&raw_dictionary).expect("raw dictionary must parse");
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "raw-dictionary")
            .expect("raw-dictionary benchmark case should exist");
        let cparams = compression_parameters_for_input(
            CompressionLevel::try_new(3).unwrap(),
            Some(case.input.len()),
            Some(raw_prepared.as_inner()),
        );
        let params = cparams.match_finder;
        let prefix = raw_prepared.as_inner().content();
        let prefix_len = prefix.len();
        let mut prefix_finder = DoubleFastFinder::new(
            params.hash_bits,
            params.secondary_hash_bits,
            params.min_match,
        );
        if prefix.len() >= 8 {
            let end = prefix.len().saturating_sub(8);
            let mut pos = 0usize;
            while pos + 2 <= end {
                for extra in 0..3 {
                    let extra_pos = pos + extra;
                    if extra_pos + 8 > prefix.len() {
                        break;
                    }
                    let sht = hash_short_cache_src_at_mls(
                        prefix,
                        extra_pos,
                        prefix_finder.short_hash_bits,
                        prefix_finder.min_match,
                    );
                    if extra == 0 {
                        prefix_finder.short_heads[tagged_index(sht)] = tagged_entry(extra_pos, sht);
                    }
                    let long_hash = hash_long_at(prefix, extra_pos, prefix_finder.long_hash_bits);
                    if extra == 0
                        || long_entry_pos(prefix_finder.long_entries[tagged_index(long_hash)])
                            == NO_POS
                    {
                        prefix_finder.file_long_entry(long_hash, extra_pos as u32);
                    }
                }
                pos += 3;
            }
        }
        for pos in [203usize, 1400, 1578, 2371, 2406, 2510] {
            let mut src_finder = DoubleFastFinder::new(
                params.hash_bits,
                params.secondary_hash_bits,
                params.min_match,
            );
            for insert_pos in 0..pos {
                src_finder.insert_src_position(&case.input, insert_pos);
            }
            let sht = hash_short_cache_src_at_mls(
                &case.input,
                pos,
                src_finder.short_hash_bits,
                src_finder.min_match,
            );
            let long_hash = hash_long_at(&case.input, pos, src_finder.long_hash_bits);
            let src_short_raw = src_finder.short_heads[tagged_index(sht)];
            let src_long_raw = long_entry_pos(src_finder.long_entries[tagged_index(long_hash)]);
            let prefix_short_raw = prefix_finder.short_heads[tagged_index(sht)];
            let prefix_long_raw =
                long_entry_pos(prefix_finder.long_entries[tagged_index(long_hash)]);
            let src_short = if src_short_raw != NO_TAGGED_ENTRY {
                tagged_pos(src_short_raw)
            } else {
                NO_POS as usize
            };
            let src_long = src_long_raw as usize;
            let prefix_short = if prefix_short_raw != NO_TAGGED_ENTRY {
                tagged_pos(prefix_short_raw)
            } else {
                NO_POS as usize
            };
            let prefix_long = prefix_long_raw as usize;

            let src_short_len = (src_short_raw != NO_TAGGED_ENTRY
                && src_short < pos
                && case.input[src_short..].len() >= MIN_MATCH
                && logical_match_has_length(
                    prefix,
                    &case.input,
                    prefix_len + src_short,
                    prefix_len + pos,
                    MIN_MATCH,
                ))
            .then(|| {
                count_match_length_with_prefix(
                    prefix,
                    &case.input,
                    prefix_len + src_short,
                    prefix_len + pos,
                )
            });
            let src_long_len = (src_long_raw != NO_POS
                && src_long < pos
                && case.input[src_long..].len() >= 8
                && logical_match_has_length(
                    prefix,
                    &case.input,
                    prefix_len + src_long,
                    prefix_len + pos,
                    8,
                ))
            .then(|| {
                count_match_length_with_prefix(
                    prefix,
                    &case.input,
                    prefix_len + src_long,
                    prefix_len + pos,
                )
            });
            let prefix_short_len = (prefix_short_raw != NO_TAGGED_ENTRY
                && logical_match_has_length(
                    prefix,
                    &case.input,
                    prefix_short,
                    prefix_len + pos,
                    MIN_MATCH,
                ))
            .then(|| {
                count_match_length_with_prefix(prefix, &case.input, prefix_short, prefix_len + pos)
            });
            let prefix_long_len = (prefix_long_raw != NO_POS
                && logical_match_has_length(prefix, &case.input, prefix_long, prefix_len + pos, 8))
            .then(|| {
                count_match_length_with_prefix(prefix, &case.input, prefix_long, prefix_len + pos)
            });

            eprintln!(
                "dense raw-dictionary L3 pos={pos} src_short={src_short} src_short_len={src_short_len:?} src_long={src_long} src_long_len={src_long_len:?} prefix_short={:?} prefix_short_len={prefix_short_len:?} prefix_long={:?} prefix_long_len={prefix_long_len:?}",
                (prefix_short_raw != NO_TAGGED_ENTRY).then_some(prefix_short),
                (prefix_long_raw != NO_POS).then_some(prefix_long),
            );
        }
    }

    #[test]
    fn print_raw_dictionary_l3_extdict_long_hash_writes_for_2371() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
        let raw_prepared =
            EncoderDictionary::new(&raw_dictionary).expect("raw dictionary must parse");
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "raw-dictionary")
            .expect("raw-dictionary benchmark case should exist");
        let cparams = compression_parameters_for_input(
            CompressionLevel::try_new(3).unwrap(),
            Some(case.input.len()),
            Some(raw_prepared.as_inner()),
        );
        assert_eq!(
            cparams.match_finder.parser_strategy,
            ParserStrategy::DoubleFast
        );
        let params = cparams.match_finder;
        let prefix = raw_prepared.as_inner().content();
        let prefix_len = prefix.len();
        let mut prefix_finder = DoubleFastFinder::new(
            params.hash_bits,
            params.secondary_hash_bits,
            params.min_match,
        );
        prefix_finder.insert_prefix_ext_dict(prefix);
        let src_finder = DoubleFastFinder::new(
            params.hash_bits,
            params.secondary_hash_bits,
            params.min_match,
        );
        let mut combined_finder = prefix_finder.clone();
        for (dst, src_entry) in combined_finder
            .short_heads
            .iter_mut()
            .zip(src_finder.short_heads.iter().copied())
        {
            if src_entry != NO_TAGGED_ENTRY {
                let pos = tagged_pos(src_entry) as u32;
                *dst = tagged_entry(prefix_len + pos as usize, src_entry as usize);
            }
        }
        // Only the position moves; the slot keeps the tag it already had. That
        // is what the untagged form of this loop did, and the library's own
        // merge (`double_fast.rs`) takes the source side's tag instead -- a
        // divergence that predates the entries being one value and is left
        // alone here rather than folded into a refactor.
        for (dst, src_entry) in combined_finder
            .long_entries
            .iter_mut()
            .zip(src_finder.long_entries.iter().copied())
        {
            let src_pos = long_entry_pos(src_entry);
            if src_pos != NO_POS {
                *dst = long_entry_with_pos(*dst, (prefix_len + src_pos as usize) as u32);
            }
        }

        let mut plan = SequencePlan::default();
        let mut repeat_offsets = RepeatOffsets::default();
        let mut rep_offsets = repeat_offsets12(repeat_offsets);
        let mut anchor = 0usize;
        let mut ip = 0usize;
        let search_limit = case.input.len().saturating_sub(8);
        let target_pos = 2371usize;
        let target_hash = hash_long_at(&case.input, target_pos, combined_finder.long_hash_bits);
        eprintln!("target_pos={target_pos} target_hash={target_hash}");

        while ip < search_limit && ip <= target_pos {
            let sht = hash_short_cache_src_at_mls(
                &case.input,
                ip,
                combined_finder.short_hash_bits,
                combined_finder.min_match,
            );
            let match_index_raw = combined_finder.short_heads[tagged_index(sht)];
            let long_hash = hash_long_at(&case.input, ip, combined_finder.long_hash_bits);
            let match_long_index_raw =
                long_entry_pos(combined_finder.long_entries[tagged_index(long_hash)]);
            let match_index = if match_index_raw != NO_TAGGED_ENTRY {
                tagged_pos(match_index_raw)
            } else {
                NO_POS as usize
            };
            let match_long_index = match_long_index_raw as usize;

            let current = ip;
            let current_logical = prefix_len + current;

            if long_hash == target_hash || ip == target_pos {
                eprintln!(
                    "search ip={ip} current_logical={current_logical} long_hash={long_hash} prev_long={match_long_index}"
                );
            }

            combined_finder.short_heads[tagged_index(sht)] = tagged_entry(current_logical, sht);
            combined_finder.file_long_entry(long_hash, current_logical as u32);

            if long_hash == target_hash || ip == target_pos {
                eprintln!("  write search curr={current_logical}");
            }

            if let Some(rep_match_start) =
                prefixed_offset_match_start(prefix_len, ip + 1, rep_offsets.0, 0, 0).filter(
                    |match_start| {
                        logical_match_has_length(
                            prefix,
                            &case.input,
                            *match_start,
                            prefix_len + ip + 1,
                            MIN_MATCH,
                        )
                    },
                )
            {
                let rep_length = count_match_length_with_prefix(
                    prefix,
                    &case.input,
                    rep_match_start,
                    prefix_len + ip + 1,
                );
                eprintln!("  rep1 ip={} len={rep_length}", ip + 1);
                store_lazy_sequence(
                    &mut plan,
                    &case.input,
                    &mut anchor,
                    &mut repeat_offsets,
                    ip + 1,
                    rep_offsets.0,
                    rep_length,
                )
                .unwrap();
                ip = anchor;
            } else if match_long_index_raw != NO_POS
                && logical_match_has_length(
                    prefix,
                    &case.input,
                    match_long_index,
                    current_logical,
                    8,
                )
            {
                let match_min_start = if match_long_index < prefix_len {
                    0
                } else {
                    prefix_len
                };
                let found = extend_back_logical_match_with_min_start(
                    prefix,
                    &case.input,
                    anchor,
                    DoubleFastMatch {
                        start: ip,
                        offset: current_logical - match_long_index,
                        length: count_match_length_with_prefix(
                            prefix,
                            &case.input,
                            match_long_index,
                            current_logical,
                        ),
                    },
                    match_min_start,
                );
                eprintln!(
                    "  long ip={} candidate={} off={} len={}",
                    found.start, match_long_index, found.offset, found.length
                );
                store_lazy_regular_sequence_with_source(
                    &mut plan,
                    &case.input,
                    &mut anchor,
                    &mut repeat_offsets,
                    found.start,
                    found.offset,
                    found.length,
                    SequenceTraceMatchSource::Unknown,
                )
                .unwrap();
                ip = anchor;
            } else if match_index_raw != NO_TAGGED_ENTRY
                && logical_match_has_length(
                    prefix,
                    &case.input,
                    match_index,
                    current_logical,
                    MIN_MATCH,
                )
            {
                let next_long_hash =
                    hash_long_at(&case.input, ip + 1, combined_finder.long_hash_bits);
                let next_match_long_index_raw =
                    long_entry_pos(combined_finder.long_entries[tagged_index(next_long_hash)]);
                let next_match_long_index = next_match_long_index_raw as usize;
                combined_finder.file_long_entry(next_long_hash, (current_logical + 1) as u32);
                if next_long_hash == target_hash {
                    eprintln!("  write next_long curr={}", current_logical + 1);
                }

                let mut start = ip;
                let mut chosen_candidate_index = match_index;
                let mut offset = current_logical - match_index;
                let mut length = count_match_length_with_prefix(
                    prefix,
                    &case.input,
                    match_index,
                    current_logical,
                );

                if next_match_long_index_raw != NO_POS
                    && logical_match_has_length(
                        prefix,
                        &case.input,
                        next_match_long_index,
                        current_logical + 1,
                        8,
                    )
                {
                    let next_length = count_match_length_with_prefix(
                        prefix,
                        &case.input,
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

                let match_min_start = if chosen_candidate_index < prefix_len {
                    0
                } else {
                    prefix_len
                };
                let found = extend_back_logical_match_with_min_start(
                    prefix,
                    &case.input,
                    anchor,
                    DoubleFastMatch {
                        start,
                        offset,
                        length,
                    },
                    match_min_start,
                );
                eprintln!(
                    "  short ip={} candidate={} off={} len={}",
                    found.start, chosen_candidate_index, found.offset, found.length
                );
                store_lazy_regular_sequence_with_source(
                    &mut plan,
                    &case.input,
                    &mut anchor,
                    &mut repeat_offsets,
                    found.start,
                    found.offset,
                    found.length,
                    SequenceTraceMatchSource::Unknown,
                )
                .unwrap();
                ip = anchor;
            } else {
                ip = ip.saturating_add(
                    ((ip.saturating_sub(anchor)) >> params.skip_search_strength) + 1,
                );
                continue;
            }

            rep_offsets = repeat_offsets12(repeat_offsets);
            if ip <= search_limit {
                let index_to_insert = current.saturating_add(2);
                if index_to_insert + 8 <= case.input.len() {
                    let hash =
                        hash_long_at(&case.input, index_to_insert, combined_finder.long_hash_bits);
                    combined_finder.file_long_entry(hash, (prefix_len + index_to_insert) as u32);
                    if hash == target_hash {
                        eprintln!(
                            "  write post-match index_to_insert={}",
                            prefix_len + index_to_insert
                        );
                    }
                }
                if ip >= 2 && ip - 2 + 8 <= case.input.len() {
                    let hash = hash_long_at(&case.input, ip - 2, combined_finder.long_hash_bits);
                    combined_finder.file_long_entry(hash, (prefix_len + (ip - 2)) as u32);
                    if hash == target_hash {
                        eprintln!("  write post-match ip-2={}", prefix_len + (ip - 2));
                    }
                }
                if index_to_insert + 8 <= case.input.len() {
                    let sht = hash_short_cache_src_at_mls(
                        &case.input,
                        index_to_insert,
                        combined_finder.short_hash_bits,
                        combined_finder.min_match,
                    );
                    combined_finder.short_heads[tagged_index(sht)] =
                        tagged_entry(prefix_len + index_to_insert, sht);
                }
                if ip >= 1 && ip - 1 + 8 <= case.input.len() {
                    let sht = hash_short_cache_src_at_mls(
                        &case.input,
                        ip - 1,
                        combined_finder.short_hash_bits,
                        combined_finder.min_match,
                    );
                    combined_finder.short_heads[tagged_index(sht)] =
                        tagged_entry(prefix_len + (ip - 1), sht);
                }

                while let Some(rep_match_start) =
                    prefixed_offset_match_start(prefix_len, ip, rep_offsets.1, 0, 0)
                {
                    if !logical_match_has_length(
                        prefix,
                        &case.input,
                        rep_match_start,
                        prefix_len + ip,
                        MIN_MATCH,
                    ) {
                        break;
                    }
                    if ip + 8 <= case.input.len() {
                        let hash = hash_long_at(&case.input, ip, combined_finder.long_hash_bits);
                        combined_finder.file_long_entry(hash, (prefix_len + ip) as u32);
                        if hash == target_hash {
                            eprintln!("  write repchain ip={}", prefix_len + ip);
                        }
                    }
                    if ip + 8 <= case.input.len() {
                        let sht = hash_short_cache_src_at_mls(
                            &case.input,
                            ip,
                            combined_finder.short_hash_bits,
                            combined_finder.min_match,
                        );
                        combined_finder.short_heads[tagged_index(sht)] =
                            tagged_entry(prefix_len + ip, sht);
                    }
                    let rep_ip = ip;
                    let rep_length = count_match_length_with_prefix(
                        prefix,
                        &case.input,
                        rep_match_start,
                        prefix_len + rep_ip,
                    );
                    eprintln!("  rep2 ip={rep_ip} len={rep_length}");
                    store_lazy_sequence(
                        &mut plan,
                        &case.input,
                        &mut anchor,
                        &mut repeat_offsets,
                        rep_ip,
                        rep_offsets.1,
                        rep_length,
                    )
                    .unwrap();
                    ip = anchor;
                    rep_offsets = repeat_offsets12(repeat_offsets);
                    if ip > search_limit {
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn print_raw_dictionary_l3_extdict_long_hash_writes_for_1400() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
        let raw_prepared =
            EncoderDictionary::new(&raw_dictionary).expect("raw dictionary must parse");
        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "raw-dictionary")
            .expect("raw-dictionary benchmark case should exist");
        let cparams = compression_parameters_for_input(
            CompressionLevel::try_new(3).unwrap(),
            Some(case.input.len()),
            Some(raw_prepared.as_inner()),
        );
        assert_eq!(
            cparams.match_finder.parser_strategy,
            ParserStrategy::DoubleFast
        );
        let params = cparams.match_finder;
        let prefix = raw_prepared.as_inner().content();
        let prefix_len = prefix.len();
        let mut prefix_finder = DoubleFastFinder::new(
            params.hash_bits,
            params.secondary_hash_bits,
            params.min_match,
        );
        prefix_finder.insert_prefix_ext_dict(prefix);
        let src_finder = DoubleFastFinder::new(
            params.hash_bits,
            params.secondary_hash_bits,
            params.min_match,
        );
        let mut combined_finder = prefix_finder.clone();
        for (dst, src_entry) in combined_finder
            .short_heads
            .iter_mut()
            .zip(src_finder.short_heads.iter().copied())
        {
            if src_entry != NO_TAGGED_ENTRY {
                let pos = tagged_pos(src_entry) as u32;
                *dst = tagged_entry(prefix_len + pos as usize, src_entry as usize);
            }
        }
        // Only the position moves; the slot keeps the tag it already had. That
        // is what the untagged form of this loop did, and the library's own
        // merge (`double_fast.rs`) takes the source side's tag instead -- a
        // divergence that predates the entries being one value and is left
        // alone here rather than folded into a refactor.
        for (dst, src_entry) in combined_finder
            .long_entries
            .iter_mut()
            .zip(src_finder.long_entries.iter().copied())
        {
            let src_pos = long_entry_pos(src_entry);
            if src_pos != NO_POS {
                *dst = long_entry_with_pos(*dst, (prefix_len + src_pos as usize) as u32);
            }
        }

        let mut plan = SequencePlan::default();
        let mut repeat_offsets = RepeatOffsets::default();
        let mut rep_offsets = repeat_offsets12(repeat_offsets);
        let mut anchor = 0usize;
        let mut ip = 0usize;
        let search_limit = case.input.len().saturating_sub(8);
        let target_pos = 1400usize;
        let target_hash = hash_long_at(&case.input, target_pos, combined_finder.long_hash_bits);
        eprintln!("target_pos={target_pos} target_hash={target_hash}");

        while ip < search_limit && ip <= target_pos + 4 {
            let sht = hash_short_cache_src_at_mls(
                &case.input,
                ip,
                combined_finder.short_hash_bits,
                combined_finder.min_match,
            );
            let match_index_raw = combined_finder.short_heads[tagged_index(sht)];
            let long_hash = hash_long_at(&case.input, ip, combined_finder.long_hash_bits);
            let match_long_index_raw =
                long_entry_pos(combined_finder.long_entries[tagged_index(long_hash)]);
            let match_index = if match_index_raw != NO_TAGGED_ENTRY {
                tagged_pos(match_index_raw)
            } else {
                NO_POS as usize
            };
            let match_long_index = match_long_index_raw as usize;

            let current = ip;
            let current_logical = prefix_len + current;

            if long_hash == target_hash || (1400..=1403).contains(&ip) {
                eprintln!(
                    "search ip={ip} current_logical={current_logical} long_hash={long_hash} prev_long={match_long_index}"
                );
            }

            combined_finder.short_heads[tagged_index(sht)] = tagged_entry(current_logical, sht);
            combined_finder.file_long_entry(long_hash, current_logical as u32);

            if long_hash == target_hash || (1400..=1403).contains(&ip) {
                eprintln!("  write search curr={current_logical}");
            }

            if let Some(rep_match_start) =
                prefixed_offset_match_start(prefix_len, ip + 1, rep_offsets.0, 0, 0).filter(
                    |match_start| {
                        logical_match_has_length(
                            prefix,
                            &case.input,
                            *match_start,
                            prefix_len + ip + 1,
                            MIN_MATCH,
                        )
                    },
                )
            {
                let rep_length = count_match_length_with_prefix(
                    prefix,
                    &case.input,
                    rep_match_start,
                    prefix_len + ip + 1,
                );
                eprintln!("  rep1 ip={} len={rep_length}", ip + 1);
                store_lazy_sequence(
                    &mut plan,
                    &case.input,
                    &mut anchor,
                    &mut repeat_offsets,
                    ip + 1,
                    rep_offsets.0,
                    rep_length,
                )
                .unwrap();
                ip = anchor;
            } else if match_long_index_raw != NO_POS
                && logical_match_has_length(
                    prefix,
                    &case.input,
                    match_long_index,
                    current_logical,
                    8,
                )
            {
                let match_min_start = if match_long_index < prefix_len {
                    0
                } else {
                    prefix_len
                };
                let found = extend_back_logical_match_with_min_start(
                    prefix,
                    &case.input,
                    anchor,
                    DoubleFastMatch {
                        start: ip,
                        offset: current_logical - match_long_index,
                        length: count_match_length_with_prefix(
                            prefix,
                            &case.input,
                            match_long_index,
                            current_logical,
                        ),
                    },
                    match_min_start,
                );
                eprintln!(
                    "  long ip={} candidate={} off={} len={}",
                    found.start, match_long_index, found.offset, found.length
                );
                store_lazy_regular_sequence_with_source(
                    &mut plan,
                    &case.input,
                    &mut anchor,
                    &mut repeat_offsets,
                    found.start,
                    found.offset,
                    found.length,
                    SequenceTraceMatchSource::Unknown,
                )
                .unwrap();
                ip = anchor;
            } else if match_index_raw != NO_TAGGED_ENTRY
                && logical_match_has_length(
                    prefix,
                    &case.input,
                    match_index,
                    current_logical,
                    MIN_MATCH,
                )
            {
                let next_long_hash =
                    hash_long_at(&case.input, ip + 1, combined_finder.long_hash_bits);
                let next_match_long_index_raw =
                    long_entry_pos(combined_finder.long_entries[tagged_index(next_long_hash)]);
                let next_match_long_index = next_match_long_index_raw as usize;
                combined_finder.file_long_entry(next_long_hash, (current_logical + 1) as u32);
                if next_long_hash == target_hash || (1400..=1403).contains(&(ip + 1)) {
                    eprintln!("  write next_long curr={}", current_logical + 1);
                }

                let mut start = ip;
                let mut chosen_candidate_index = match_index;
                let mut offset = current_logical - match_index;
                let mut length = count_match_length_with_prefix(
                    prefix,
                    &case.input,
                    match_index,
                    current_logical,
                );

                if next_match_long_index_raw != NO_POS
                    && logical_match_has_length(
                        prefix,
                        &case.input,
                        next_match_long_index,
                        current_logical + 1,
                        8,
                    )
                {
                    let next_length = count_match_length_with_prefix(
                        prefix,
                        &case.input,
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

                let match_min_start = if chosen_candidate_index < prefix_len {
                    0
                } else {
                    prefix_len
                };
                let found = extend_back_logical_match_with_min_start(
                    prefix,
                    &case.input,
                    anchor,
                    DoubleFastMatch {
                        start,
                        offset,
                        length,
                    },
                    match_min_start,
                );
                eprintln!(
                    "  short ip={} candidate={} off={} len={}",
                    found.start, chosen_candidate_index, found.offset, found.length
                );
                store_lazy_regular_sequence_with_source(
                    &mut plan,
                    &case.input,
                    &mut anchor,
                    &mut repeat_offsets,
                    found.start,
                    found.offset,
                    found.length,
                    SequenceTraceMatchSource::Unknown,
                )
                .unwrap();
                ip = anchor;
            } else {
                ip = ip.saturating_add(
                    ((ip.saturating_sub(anchor)) >> params.skip_search_strength) + 1,
                );
                continue;
            }

            rep_offsets = repeat_offsets12(repeat_offsets);
            if ip <= search_limit {
                let index_to_insert = current.saturating_add(2);
                if index_to_insert + 8 <= case.input.len() {
                    let hash =
                        hash_long_at(&case.input, index_to_insert, combined_finder.long_hash_bits);
                    combined_finder.file_long_entry(hash, (prefix_len + index_to_insert) as u32);
                    if hash == target_hash || (1400..=1403).contains(&index_to_insert) {
                        eprintln!(
                            "  write post-match index_to_insert={}",
                            prefix_len + index_to_insert
                        );
                    }
                }
                if ip >= 2 && ip - 2 + 8 <= case.input.len() {
                    let hash = hash_long_at(&case.input, ip - 2, combined_finder.long_hash_bits);
                    combined_finder.file_long_entry(hash, (prefix_len + (ip - 2)) as u32);
                    if hash == target_hash || (1400..=1403).contains(&(ip - 2)) {
                        eprintln!("  write post-match ip-2={}", prefix_len + (ip - 2));
                    }
                }
                if index_to_insert + 8 <= case.input.len() {
                    let sht = hash_short_cache_src_at_mls(
                        &case.input,
                        index_to_insert,
                        combined_finder.short_hash_bits,
                        combined_finder.min_match,
                    );
                    combined_finder.short_heads[tagged_index(sht)] =
                        tagged_entry(prefix_len + index_to_insert, sht);
                }
                if ip >= 1 && ip - 1 + 8 <= case.input.len() {
                    let sht = hash_short_cache_src_at_mls(
                        &case.input,
                        ip - 1,
                        combined_finder.short_hash_bits,
                        combined_finder.min_match,
                    );
                    combined_finder.short_heads[tagged_index(sht)] =
                        tagged_entry(prefix_len + (ip - 1), sht);
                }

                while let Some(rep_match_start) =
                    prefixed_offset_match_start(prefix_len, ip, rep_offsets.1, 0, 0)
                {
                    if !logical_match_has_length(
                        prefix,
                        &case.input,
                        rep_match_start,
                        prefix_len + ip,
                        MIN_MATCH,
                    ) {
                        break;
                    }
                    if ip + 8 <= case.input.len() {
                        let hash = hash_long_at(&case.input, ip, combined_finder.long_hash_bits);
                        combined_finder.file_long_entry(hash, (prefix_len + ip) as u32);
                        if hash == target_hash || (1400..=1403).contains(&ip) {
                            eprintln!("  write repchain ip={}", prefix_len + ip);
                        }
                    }
                    if ip + 8 <= case.input.len() {
                        let sht = hash_short_cache_src_at_mls(
                            &case.input,
                            ip,
                            combined_finder.short_hash_bits,
                            combined_finder.min_match,
                        );
                        combined_finder.short_heads[tagged_index(sht)] =
                            tagged_entry(prefix_len + ip, sht);
                    }
                    let rep_ip = ip;
                    let rep_length = count_match_length_with_prefix(
                        prefix,
                        &case.input,
                        rep_match_start,
                        prefix_len + rep_ip,
                    );
                    eprintln!("  rep2 ip={rep_ip} len={rep_length}");
                    store_lazy_sequence(
                        &mut plan,
                        &case.input,
                        &mut anchor,
                        &mut repeat_offsets,
                        rep_ip,
                        rep_offsets.1,
                        rep_length,
                    )
                    .unwrap();
                    ip = anchor;
                    rep_offsets = repeat_offsets12(repeat_offsets);
                    if ip > search_limit {
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn no_dict_benchmark_first_block_sequences_match_upstream() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        for (case_name, level) in [
            ("log-lines", 1u8),
            ("log-lines", 2u8),
            ("log-lines", 3u8),
            ("json-records", 5u8),
            ("json-records", 6u8),
            ("json-records", 7u8),
            ("log-lines", 5u8),
            ("log-lines", 6u8),
            ("log-lines", 7u8),
            ("log-lines", 8u8),
        ] {
            let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
                .into_iter()
                .find(|case| case.name == case_name)
                .unwrap_or_else(|| panic!("{case_name} benchmark case should exist"));
            let input = &case.input;
            let params = compression_parameters_for_input(
                CompressionLevel::try_new(i32::from(level)).unwrap(),
                Some(input.len()),
                None,
            );
            let upstream_cparams = upstream_trace_helper::trace_regular_applied_cparams(
                helper,
                i32::from(level),
                false,
                input,
            );
            let expected_cparams = upstream_cparams_from_helper(upstream_cparams);
            assert_eq!(
                params.upstream_cparams, expected_cparams,
                "{case_name} L{level} should use upstream applied cparams for no-dict benchmark input",
            );
            let (rust_trace, upstream_sequences) =
                regular_first_block_sequence_traces(helper, level, input);
            let rust_sequences = &rust_trace.emitted_matches;
            let first_mismatch =
                regular_first_block_sequence_first_mismatch(rust_sequences, &upstream_sequences);
            let mismatch_window = first_mismatch.map(|index| {
                mismatch_window(
                    rust_sequences,
                    &upstream_sequences,
                    &rust_trace.trace_emissions,
                    &rust_trace.trace_row_searches,
                    &rust_trace.trace_chain_searches,
                    index,
                )
            });

            if std::env::var_os("ZSTANDARD_PRINT_NO_DICT_BENCHMARK_MISMATCH").is_some() {
                let upstream_row_lazy_probes = mismatch_window
                    .as_ref()
                    .map(|window| {
                        let local_mismatch_index = window.mismatch_index.min(3);
                        let mut probe_positions = window
                            .row_searches
                            .iter()
                            .map(|search| search.pos)
                            .chain(
                                window
                                    .rust_sequences
                                    .get(local_mismatch_index)
                                    .into_iter()
                                    .flatten()
                                    .map(|sequence| sequence.start),
                            )
                            .chain(
                                window
                                    .upstream_sequences
                                    .get(local_mismatch_index)
                                    .into_iter()
                                    .flatten()
                                    .map(|sequence| sequence.start),
                            )
                            .collect::<Vec<_>>();
                        probe_positions.sort_unstable();
                        probe_positions.dedup();
                        probe_positions
                            .into_iter()
                            .filter(|&pos| pos + 4 <= input.len())
                            .map(|pos| {
                                upstream_trace_helper::trace_no_dict_row_lazy_probe(
                                    helper,
                                    i32::from(level),
                                    false,
                                    pos,
                                    input,
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let upstream_row_search_probes = mismatch_window
                    .as_ref()
                    .map(|window| {
                        window
                            .row_searches
                            .iter()
                            .filter(|search| search.pos > 0)
                            .map(|search| {
                                upstream_trace_helper::trace_no_dict_row_search_probe(
                                    helper,
                                    i32::from(level),
                                    false,
                                    search.pos - 1,
                                    search.pos,
                                    input,
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                eprintln!(
                    "{case_name} L{level} mismatch_window={mismatch_window:?} upstream_row_lazy_probes={upstream_row_lazy_probes:?} upstream_row_search_probes={upstream_row_search_probes:?}",
                );
            }

            assert!(
                first_mismatch.is_none(),
                "{case_name} L{level} first mismatch at sequence {}: rust={:?} upstream={:?} \
                 rust_prefix={:?} upstream_prefix={:?} emission_trace_prefix={:?} row_search_prefix={:?} mismatch_window={:?}",
                first_mismatch.map(|index| index + 1).unwrap_or_default(),
                first_mismatch.and_then(|index| rust_sequences.get(index).copied()),
                first_mismatch.and_then(|index| upstream_sequences.get(index).copied()),
                &rust_sequences[..rust_sequences.len().min(14)],
                &upstream_sequences[..upstream_sequences.len().min(14)],
                &rust_trace.trace_emissions[..rust_trace.trace_emissions.len().min(14)],
                &rust_trace.trace_row_searches[..rust_trace.trace_row_searches.len().min(16)],
                mismatch_window,
            );
        }
    }

    #[test]
    fn no_dict_benchmark_row_lazy_probes_match_upstream() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        for (case_name, level) in [
            ("json-records", 5u8),
            ("json-records", 6u8),
            ("json-records", 7u8),
            ("log-lines", 5u8),
            ("log-lines", 6u8),
            ("log-lines", 7u8),
        ] {
            let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
                .into_iter()
                .find(|case| case.name == case_name)
                .unwrap_or_else(|| panic!("{case_name} benchmark case should exist"));
            let input = &case.input;
            let params = compression_parameters_for_input(
                CompressionLevel::try_new(i32::from(level)).unwrap(),
                Some(input.len()),
                None,
            );
            let rust_trace = planned_first_block_trace_without_dict(
                input,
                EncoderOptions {
                    block_size: 128 * 1024,
                    checksum: false,
                    write_dict_id: true,
                    compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
                    ..Default::default()
                },
            )
            .unwrap();
            let probe_positions = hot_no_dict_row_probe_positions(
                &rust_trace.trace_row_lazy_probes,
                &rust_trace.emitted_matches,
                input.len(),
            );

            for pos in probe_positions {
                let rust = rust_no_dict_row_lazy_probe(
                    &rust_trace.trace_row_lazy_probes,
                    &rust_trace.trace_emissions,
                    pos,
                )
                .unwrap_or_else(|| panic!("missing rust no-dict row lazy probe at position {pos}"));
                let upstream = upstream_trace_helper::trace_no_dict_row_lazy_probe(
                    helper,
                    i32::from(level),
                    false,
                    pos,
                    input,
                );
                if std::env::var_os("ZSTANDARD_PRINT_NO_DICT_ROW_LAZY_PROBES").is_some() {
                    eprintln!(
                        "{case_name} L{level} probe {pos} rust={rust:?} upstream={upstream:?}"
                    );
                }

                assert_eq!(
                    upstream.backend,
                    upstream_trace_helper::UpstreamChainProbeBackend::NoDict,
                    "{case_name} L{level} probe {pos} should use no-dict backend",
                );
                assert_eq!(
                    upstream.depth, params.match_finder.lazy_search_depth,
                    "{case_name} L{level} probe {pos} lazy depth",
                );
                assert_eq!(
                    rust.anchor, upstream.anchor,
                    "{case_name} L{level} probe {pos} anchor"
                );
                assert_eq!(
                    rust.offset_1, upstream.offset_1,
                    "{case_name} L{level} probe {pos} offset_1"
                );
                assert_eq!(
                    rust.offset_2, upstream.offset_2,
                    "{case_name} L{level} probe {pos} offset_2"
                );
                assert_eq!(
                    rust.baseline_rep_length, upstream.baseline_rep_length,
                    "{case_name} L{level} probe {pos} baseline rep",
                );
                assert_eq!(
                    rust.baseline_regular_next_to_update, upstream.baseline_regular_next_to_update,
                    "{case_name} L{level} probe {pos} baseline regular next_to_update",
                );
                assert_eq!(
                    rust.baseline_regular_hash, upstream.baseline_regular_hash,
                    "{case_name} L{level} probe {pos} baseline regular hash",
                );
                assert_eq!(
                    rust.baseline_regular_rel_row, upstream.baseline_regular_rel_row,
                    "{case_name} L{level} probe {pos} baseline regular rel_row",
                );
                assert_eq!(
                    rust.baseline_regular_tag, upstream.baseline_regular_tag,
                    "{case_name} L{level} probe {pos} baseline regular tag",
                );
                assert_eq!(
                    rust.baseline_regular_low_limit, upstream.baseline_regular_low_limit,
                    "{case_name} L{level} probe {pos} baseline regular low_limit",
                );
                assert_eq!(
                    rust.baseline_regular_attempt_budget, upstream.baseline_regular_attempt_budget,
                    "{case_name} L{level} probe {pos} baseline regular attempt budget",
                );
                assert_eq!(
                    rust.baseline_regular_head_index, upstream.baseline_regular_head_index,
                    "{case_name} L{level} probe {pos} baseline regular head index",
                );
                assert_eq!(
                    rust.baseline_regular_insert_index, upstream.baseline_regular_insert_index,
                    "{case_name} L{level} probe {pos} baseline regular insert index",
                );
                // group_width intentionally differs: we always use 1 (SWAR path)
                // regardless of platform, while upstream uses platform-specific values.
                assert_eq!(
                    rust.baseline_regular_match_count, upstream.baseline_regular_match_count,
                    "{case_name} L{level} probe {pos} baseline regular match count",
                );
                assert_eq!(
                    rust.baseline_regular_match_positions,
                    upstream.baseline_regular_match_positions,
                    "{case_name} L{level} probe {pos} baseline regular match positions",
                );
                assert_eq!(
                    rust.baseline_regular_match_indices, upstream.baseline_regular_match_indices,
                    "{case_name} L{level} probe {pos} baseline regular match indices",
                );
                assert_eq!(
                    rust.baseline_regular_visit_count, upstream.baseline_regular_visit_count,
                    "{case_name} L{level} probe {pos} baseline regular visit count",
                );
                assert_eq!(
                    rust.baseline_regular_visit_positions,
                    upstream.baseline_regular_visit_positions,
                    "{case_name} L{level} probe {pos} baseline regular visit positions",
                );
                assert_eq!(
                    rust.baseline_regular_visit_indices, upstream.baseline_regular_visit_indices,
                    "{case_name} L{level} probe {pos} baseline regular visit indices",
                );
                assert_eq!(
                    rust.baseline_regular_visit_lengths, upstream.baseline_regular_visit_lengths,
                    "{case_name} L{level} probe {pos} baseline regular visit lengths",
                );
                assert_eq!(
                    rust.baseline_regular_length, upstream.baseline_regular_length,
                    "{case_name} L{level} probe {pos} baseline regular length",
                );
                assert_eq!(
                    rust.baseline_regular_off_base, upstream.baseline_regular_off_base,
                    "{case_name} L{level} probe {pos} baseline regular offbase",
                );
                assert_eq!(
                    rust.depth1_rep_length, upstream.depth1_rep_length,
                    "{case_name} L{level} probe {pos} depth1 rep"
                );
                assert_eq!(
                    rust.depth1_regular_length, upstream.depth1_regular_length,
                    "{case_name} L{level} probe {pos} depth1 regular length",
                );
                assert_eq!(
                    rust.depth1_regular_off_base, upstream.depth1_regular_off_base,
                    "{case_name} L{level} probe {pos} depth1 regular offbase",
                );
                assert_eq!(
                    rust.depth2_rep_length, upstream.depth2_rep_length,
                    "{case_name} L{level} probe {pos} depth2 rep"
                );
                assert_eq!(
                    rust.depth2_regular_length, upstream.depth2_regular_length,
                    "{case_name} L{level} probe {pos} depth2 regular length",
                );
                assert_eq!(
                    rust.depth2_regular_off_base, upstream.depth2_regular_off_base,
                    "{case_name} L{level} probe {pos} depth2 regular offbase",
                );
                assert_eq!(
                    rust.chosen_kind, upstream.chosen_kind,
                    "{case_name} L{level} probe {pos} chosen kind"
                );
                assert_eq!(
                    rust.chosen_start, upstream.chosen_start,
                    "{case_name} L{level} probe {pos} chosen start"
                );
                assert_eq!(
                    rust.chosen_length, upstream.chosen_length,
                    "{case_name} L{level} probe {pos} chosen length"
                );
                assert_eq!(
                    rust.chosen_off_base, upstream.chosen_off_base,
                    "{case_name} L{level} probe {pos} chosen offbase"
                );
                assert_eq!(
                    rust.literal_length, upstream.literal_length,
                    "{case_name} L{level} probe {pos} literal length"
                );
                assert_eq!(
                    rust.immediate_rep2_length, upstream.immediate_rep2_length,
                    "{case_name} L{level} probe {pos} immediate rep2",
                );
                assert_eq!(
                    rust.continue_step_count, upstream.continue_step_count,
                    "{case_name} L{level} probe {pos} continue step count",
                );
                assert_eq!(
                    rust.continue_positions, upstream.continue_positions,
                    "{case_name} L{level} probe {pos} continue positions",
                );
                assert_eq!(
                    rust.continue_rep_lengths, upstream.continue_rep_lengths,
                    "{case_name} L{level} probe {pos} continue rep lengths",
                );
                assert_eq!(
                    rust.continue_rep_improved, upstream.continue_rep_improved,
                    "{case_name} L{level} probe {pos} continue rep improved",
                );
                assert_eq!(
                    rust.continue_regular_lengths, upstream.continue_regular_lengths,
                    "{case_name} L{level} probe {pos} continue regular lengths",
                );
                assert_eq!(
                    rust.continue_regular_off_bases, upstream.continue_regular_off_bases,
                    "{case_name} L{level} probe {pos} continue regular offbases",
                );
                assert_eq!(
                    rust.continue_regular_improved, upstream.continue_regular_improved,
                    "{case_name} L{level} probe {pos} continue regular improved",
                );
                assert_eq!(
                    rust.continue_current_kinds, upstream.continue_current_kinds,
                    "{case_name} L{level} probe {pos} continue current kinds",
                );
                assert_eq!(
                    rust.continue_current_starts, upstream.continue_current_starts,
                    "{case_name} L{level} probe {pos} continue current starts",
                );
                assert_eq!(
                    rust.continue_current_lengths, upstream.continue_current_lengths,
                    "{case_name} L{level} probe {pos} continue current lengths",
                );
                assert_eq!(
                    rust.continue_current_off_bases, upstream.continue_current_off_bases,
                    "{case_name} L{level} probe {pos} continue current offbases",
                );
                assert_eq!(
                    rust.stop_reason, upstream.stop_reason,
                    "{case_name} L{level} probe {pos} stop reason",
                );
                assert_eq!(
                    no_dict_row_lazy_decisions(
                        params.match_finder.lazy_search_depth,
                        rust.baseline_rep_length,
                        rust.baseline_regular_length,
                        rust.baseline_regular_off_base,
                        rust.depth1_rep_length,
                        rust.depth1_regular_length,
                        rust.depth1_regular_off_base,
                        rust.depth2_rep_length,
                        rust.depth2_regular_length,
                        rust.depth2_regular_off_base,
                    ),
                    no_dict_row_lazy_decisions(
                        upstream.depth,
                        upstream.baseline_rep_length,
                        upstream.baseline_regular_length,
                        upstream.baseline_regular_off_base,
                        upstream.depth1_rep_length,
                        upstream.depth1_regular_length,
                        upstream.depth1_regular_off_base,
                        upstream.depth2_rep_length,
                        upstream.depth2_regular_length,
                        upstream.depth2_regular_off_base,
                    ),
                    "{case_name} L{level} probe {pos} decision state",
                );
                assert_eq!(
                    no_dict_row_lazy_continue_decision_state(
                        rust,
                        params.match_finder.lazy_search_depth,
                    ),
                    upstream_no_dict_row_lazy_continue_decision_state(upstream),
                    "{case_name} L{level} probe {pos} continue decision state",
                );
            }
        }
    }

    #[test]
    fn no_dict_benchmark_row_late_continue_window_match_upstream() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "json-records")
            .expect("json-records benchmark case should exist");
        let input = &case.input;
        let rust_trace = planned_first_block_trace_without_dict(
            input,
            EncoderOptions {
                block_size: 128 * 1024,
                checksum: false,
                write_dict_id: true,
                compression_level: CompressionLevel::try_new(6).unwrap(),
                ..Default::default()
            },
        )
        .unwrap();
        let mut probe_positions = rust_trace
            .trace_row_lazy_probes
            .iter()
            .filter(|probe| (49351..=49359).contains(&probe.pos))
            .map(|probe| probe.pos)
            .collect::<Vec<_>>();
        probe_positions.sort_unstable();
        probe_positions.dedup();
        assert!(
            !probe_positions.is_empty(),
            "expected row lazy probes in the json-records L6 late-continue window",
        );

        for pos in probe_positions {
            let rust = rust_no_dict_row_lazy_probe(
                &rust_trace.trace_row_lazy_probes,
                &rust_trace.trace_emissions,
                pos,
            )
            .unwrap_or_else(|| panic!("missing rust no-dict row lazy probe at position {pos}"));
            let upstream =
                upstream_trace_helper::trace_no_dict_row_lazy_probe(helper, 6, false, pos, input);
            let rust_decisions = no_dict_row_lazy_continue_decision_state(rust, 1);
            let upstream_decisions = upstream_no_dict_row_lazy_continue_decision_state(upstream);

            assert_eq!(
                rust.continue_step_count, upstream.continue_step_count,
                "json-records L6 probe {pos} continue step count",
            );
            assert_eq!(
                rust.continue_positions, upstream.continue_positions,
                "json-records L6 probe {pos} continue positions",
            );
            assert_eq!(
                rust.continue_rep_lengths, upstream.continue_rep_lengths,
                "json-records L6 probe {pos} continue rep lengths",
            );
            assert_eq!(
                rust.continue_rep_improved, upstream.continue_rep_improved,
                "json-records L6 probe {pos} continue rep improved",
            );
            assert_eq!(
                rust.continue_regular_lengths, upstream.continue_regular_lengths,
                "json-records L6 probe {pos} continue regular lengths",
            );
            assert_eq!(
                rust.continue_regular_off_bases, upstream.continue_regular_off_bases,
                "json-records L6 probe {pos} continue regular offbases",
            );
            assert_eq!(
                rust.continue_regular_improved, upstream.continue_regular_improved,
                "json-records L6 probe {pos} continue regular improved",
            );
            assert_eq!(
                rust.continue_current_kinds, upstream.continue_current_kinds,
                "json-records L6 probe {pos} continue current kinds",
            );
            assert_eq!(
                rust.continue_current_starts, upstream.continue_current_starts,
                "json-records L6 probe {pos} continue current starts",
            );
            assert_eq!(
                rust.continue_current_lengths, upstream.continue_current_lengths,
                "json-records L6 probe {pos} continue current lengths",
            );
            assert_eq!(
                rust.continue_current_off_bases, upstream.continue_current_off_bases,
                "json-records L6 probe {pos} continue current offbases",
            );
            assert_eq!(
                rust.stop_reason, upstream.stop_reason,
                "json-records L6 probe {pos} stop reason",
            );
            assert_eq!(
                rust_decisions, upstream_decisions,
                "json-records L6 probe {pos} continue decision state",
            );
        }
    }

    #[test]
    fn no_dict_benchmark_row_search_probes_match_upstream() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        for (case_name, level) in [
            ("json-records", 5u8),
            ("json-records", 6u8),
            ("json-records", 7u8),
            ("log-lines", 5u8),
            ("log-lines", 6u8),
            ("log-lines", 7u8),
        ] {
            let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
                .into_iter()
                .find(|case| case.name == case_name)
                .unwrap_or_else(|| panic!("{case_name} benchmark case should exist"));
            let input = &case.input;
            let rust_trace = planned_first_block_trace_without_dict(
                input,
                EncoderOptions {
                    block_size: 128 * 1024,
                    checksum: false,
                    write_dict_id: true,
                    compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
                    ..Default::default()
                },
            )
            .unwrap();
            let probe_positions = hot_no_dict_row_search_probe_positions(
                &rust_trace.trace_row_searches,
                &rust_trace.trace_emissions,
                input.len(),
            );

            for pos in probe_positions {
                let rust = rust_no_dict_row_search_probe(&rust_trace.trace_row_searches, pos)
                    .unwrap_or_else(|| {
                        panic!("missing rust no-dict row search probe at position {pos}")
                    });
                let upstream = upstream_trace_helper::trace_no_dict_row_search_probe(
                    helper,
                    i32::from(level),
                    false,
                    pos - 1,
                    pos,
                    input,
                );
                if std::env::var_os("ZSTANDARD_PRINT_NO_DICT_ROW_SEARCH_PROBES").is_some() {
                    eprintln!(
                        "{case_name} L{level} probe {pos} rust={rust:?} upstream={upstream:?}"
                    );
                }

                assert!(
                    upstream.visited,
                    "{case_name} L{level} probe {pos} should be visited"
                );
                assert_eq!(
                    rust.state_pos, upstream.state_pos,
                    "{case_name} L{level} probe {pos} state pos",
                );
                assert_eq!(
                    rust.probe_pos, upstream.probe_pos,
                    "{case_name} L{level} probe {pos} probe pos",
                );
                assert_eq!(
                    rust.next_to_update_before_search, upstream.next_to_update_before_search,
                    "{case_name} L{level} probe {pos} next_to_update",
                );
                assert_eq!(
                    rust.hash, upstream.hash,
                    "{case_name} L{level} probe {pos} hash",
                );
                assert_eq!(
                    rust.rel_row, upstream.rel_row,
                    "{case_name} L{level} probe {pos} rel_row",
                );
                assert_eq!(
                    rust.tag, upstream.tag,
                    "{case_name} L{level} probe {pos} tag",
                );
                assert_eq!(
                    rust.low_limit, upstream.low_limit,
                    "{case_name} L{level} probe {pos} low_limit",
                );
                assert_eq!(
                    rust.attempt_budget, upstream.attempt_budget,
                    "{case_name} L{level} probe {pos} attempt budget",
                );
                assert_eq!(
                    rust.head_index, upstream.head_index,
                    "{case_name} L{level} probe {pos} head index",
                );
                assert_eq!(
                    rust.insert_index, upstream.insert_index,
                    "{case_name} L{level} probe {pos} insert index",
                );
                // group_width intentionally differs: we always use 1 (SWAR path)
                // regardless of platform, while upstream uses platform-specific values.
                assert_eq!(
                    rust.match_count, upstream.match_count,
                    "{case_name} L{level} probe {pos} match count",
                );
                assert_eq!(
                    rust.match_positions, upstream.match_positions,
                    "{case_name} L{level} probe {pos} match positions",
                );
                assert_eq!(
                    rust.match_indices, upstream.match_indices,
                    "{case_name} L{level} probe {pos} match indices",
                );
                assert_eq!(
                    rust.visit_count, upstream.visit_count,
                    "{case_name} L{level} probe {pos} visit count",
                );
                assert_eq!(
                    rust.visit_positions, upstream.visit_positions,
                    "{case_name} L{level} probe {pos} visit positions",
                );
                assert_eq!(
                    rust.visit_indices, upstream.visit_indices,
                    "{case_name} L{level} probe {pos} visit indices",
                );
                assert_eq!(
                    rust.visit_lengths, upstream.visit_lengths,
                    "{case_name} L{level} probe {pos} visit lengths",
                );
                assert_eq!(
                    rust.visit_gate_passes, upstream.visit_gate_passes,
                    "{case_name} L{level} probe {pos} visit gate passes",
                );
                assert_eq!(
                    rust.visit_winner_lengths, upstream.visit_winner_lengths,
                    "{case_name} L{level} probe {pos} visit winner lengths",
                );
                assert_eq!(
                    rust.visit_winner_off_bases, upstream.visit_winner_off_bases,
                    "{case_name} L{level} probe {pos} visit winner offbases",
                );
                assert_eq!(
                    rust.match_length, upstream.match_length,
                    "{case_name} L{level} probe {pos} match length",
                );
                assert_eq!(
                    rust.off_base, upstream.off_base,
                    "{case_name} L{level} probe {pos} offbase",
                );
                assert_eq!(
                    no_dict_row_search_decision_state(rust),
                    upstream_no_dict_row_search_decision_state(upstream),
                    "{case_name} L{level} probe {pos} decision state",
                );
            }
        }
    }

    #[test]
    fn json_records_bad_block_first_block_sequences_match_upstream() {
        let Some(helper) = upstream_trace_helper::helper_path() else {
            return;
        };

        let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
            .into_iter()
            .find(|case| case.name == "json-records")
            .expect("json-records benchmark case should exist");
        let input = &case.input[..128 * 1024];
        let options = EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::try_new(5).unwrap(),
            ..Default::default()
        };
        let params = compression_parameters_for_options(options, Some(input.len()), None);
        let rust_trace = planned_first_block_trace_without_dict(input, options).unwrap();
        let rust_sequences = &rust_trace.emitted_matches;
        let upstream_sequences =
            upstream_trace_helper::trace_regular_sequences(helper, 5, false, input.len(), input);

        let first_mismatch = rust_sequences
            .iter()
            .zip(upstream_sequences.iter())
            .enumerate()
            // The shared comparator, not a copy of it: this predicate used to be
            // open-coded here, which is exactly why it kept its own idea of what
            // counts as a mismatch after the repcode substitution landed.
            .find(|(_, (rust, upstream))| !regular_sequence_matches_upstream(**rust, **upstream));
        let mismatch_window = first_mismatch.map(|(index, _)| {
            mismatch_window(
                rust_sequences,
                &upstream_sequences,
                &rust_trace.trace_emissions,
                &rust_trace.trace_row_searches,
                &rust_trace.trace_chain_searches,
                index,
            )
        });
        if std::env::var_os("ZSTANDARD_PRINT_JSON_RECORDS_ROW_MISMATCH").is_some() {
            let upstream_row_lazy_probes = mismatch_window
                .as_ref()
                .map(|window| {
                    let local_mismatch_index = window.mismatch_index.min(3);
                    let mut probe_positions = window
                        .row_searches
                        .iter()
                        .map(|search| search.pos)
                        .chain(
                            window
                                .rust_sequences
                                .get(local_mismatch_index)
                                .into_iter()
                                .flatten()
                                .map(|sequence| sequence.start),
                        )
                        .chain(
                            window
                                .upstream_sequences
                                .get(local_mismatch_index)
                                .into_iter()
                                .flatten()
                                .map(|sequence| sequence.start),
                        )
                        .collect::<Vec<_>>();
                    probe_positions.sort_unstable();
                    probe_positions.dedup();
                    probe_positions
                        .into_iter()
                        .filter(|&pos| pos + 4 <= input.len())
                        .map(|pos| {
                            upstream_trace_helper::trace_no_dict_row_lazy_probe(
                                helper, 5, false, pos, input,
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            eprintln!(
                "json-records mismatch_window={mismatch_window:?} upstream_row_lazy_probes={upstream_row_lazy_probes:?}"
            );
        }
        assert!(
            first_mismatch.is_none(),
            "json-records L5 first mismatch at sequence {} (expected full first-block parity): rust={:?} upstream={:?} \
             rust_prefix={:?} upstream_prefix={:?} emission_trace_prefix={:?} row_search_prefix={:?} mismatch_window={:?} \
             hash38={} hash83={} match_finder={:?}",
            first_mismatch
                .map(|(index, _)| index + 1)
                .unwrap_or_default(),
            first_mismatch.map(|(_, (rust, _))| *rust),
            first_mismatch.map(|(_, (_, upstream))| *upstream),
            &rust_sequences[..rust_sequences.len().min(14)],
            &upstream_sequences[..upstream_sequences.len().min(14)],
            &rust_trace.trace_emissions[..rust_trace.trace_emissions.len().min(14)],
            &rust_trace.trace_row_searches[..rust_trace.trace_row_searches.len().min(16)],
            mismatch_window,
            debug_row_hash_for_params(input, 38, params.match_finder, 0),
            debug_row_hash_for_params(input, 83, params.match_finder, 0),
            params.match_finder,
        );
    }

    #[test]
    fn raw_literals_estimate_rejects_small_sequence_blocks_early() {
        let mut plan = SequencePlan {
            literals: b"status=ok service=edge".to_vec(),
            sequences: vec![SequenceCommand {
                literal_length: 22,
                offset_value: 4,
                match_length: 4,
            }],
            repeat_offsets: RepeatOffsets::default(),
            ..Default::default()
        };
        let src = vec![b'x'; 26];
        let mut huffman_dst = Vec::new();
        let mut sequence_bitstream = Vec::new();
        let mut sequence_workspace = SequenceEncodeScratch::default();
        let mut huf_workspace = huff0::CompressWorkspace::default();
        let mut out = Vec::new();

        let result = encode_compressed_block_direct(
            &mut OutBuf::growable(&mut out),
            &src,
            &mut plan,
            &LiteralsEncodingState::default(),
            &SequenceEncodingState::default(),
            &mut huffman_dst,
            &mut sequence_bitstream,
            &mut sequence_workspace,
            ParserStrategy::Fast,
            &mut huf_workspace,
            false,
        )
        .unwrap();

        assert!(result.is_none());
        assert!(out.is_empty());
    }

    #[test]
    fn actual_literals_size_can_reject_sequence_block_before_sequence_entropy() {
        let mut literals = build_high_entropy_literals(96);
        for index in (0..literals.len()).step_by(10) {
            literals[index] = b'a';
        }
        let mut plan = SequencePlan {
            literals,
            sequences: vec![SequenceCommand {
                literal_length: 96,
                offset_value: 4,
                match_length: 4,
            }],
            repeat_offsets: RepeatOffsets::default(),
            ..Default::default()
        };
        let src = vec![b'x'; 100];
        let mut huffman_dst = Vec::new();
        let mut sequence_bitstream = Vec::new();
        let mut sequence_workspace = SequenceEncodeScratch::default();
        let mut huf_workspace = huff0::CompressWorkspace::default();
        let mut out = Vec::new();

        let result = encode_compressed_block_direct(
            &mut OutBuf::growable(&mut out),
            &src,
            &mut plan,
            &LiteralsEncodingState::default(),
            &SequenceEncodingState::default(),
            &mut huffman_dst,
            &mut sequence_bitstream,
            &mut sequence_workspace,
            ParserStrategy::Fast,
            &mut huf_workspace,
            false,
        )
        .unwrap();

        assert!(result.is_none());
        assert!(out.is_empty());
    }
}

/// Encoder configuration. Use [`EncoderOptions::default`] for sensible
/// defaults, or override individual fields. A common pattern is
/// `EncoderOptions { compression_level: CompressionLevel::BETTER, ..Default::default() }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderOptions {
    /// Maximum bytes per block. Defaults to [`crate::BLOCK_SIZE_MAX`] (128 KiB);
    /// smaller values produce more block headers but can improve streaming latency.
    pub block_size: usize,
    /// If `true`, append a four-byte XXH64-truncated content checksum to each frame.
    pub checksum: bool,
    /// If `true`, embed the dictionary id in the frame header when a formatted dictionary is in use.
    pub write_dict_id: bool,
    /// Target [`CompressionLevel`]; controls the parser strategy and table sizes.
    pub compression_level: CompressionLevel,
    /// Compression parameters to use instead of the ones
    /// [`Self::compression_level`] would choose. Defaults to none overridden.
    pub parameters: ParameterOverrides,
    /// Total bytes the frame will carry, if known ahead of time; upstream's
    /// `ZSTD_CCtx_setPledgedSrcSize`. Defaults to `None`.
    ///
    /// This is a streaming setting. A stream that says how much it will carry
    /// gets parameters sized for that much, rather than the largest tier, and
    /// its frame header declares a content size instead of only a window. The
    /// pledge is checked: [`crate::StreamingEncoder::finish`] reports
    /// [`Error::InvalidParameter`] if the stream carried a different number of
    /// bytes, because a frame header that lies about its length is worse than
    /// one that says nothing.
    ///
    /// The one-shot entry points already know the exact length and use it. If
    /// this disagrees with the input they are given, they report it rather
    /// than quietly preferring one of the two.
    pub pledged_src_size: Option<u64>,
    /// If `true`, record the decompressed size in the frame header; upstream's
    /// `ZSTD_c_contentSizeFlag`. Defaults to `true`.
    ///
    /// Turning this off costs a decoder the ability to allocate the output in
    /// one go — `ZSTD_getFrameContentSize` and this crate's
    /// [`FrameHeader`](crate::FrameHeader) both report "unknown" — in exchange
    /// for up to eight bytes per frame. The size is not written for a stream
    /// with no [`Self::pledged_src_size`] either way, since there is nothing
    /// truthful to write.
    pub write_content_size: bool,
    /// Frame envelope; see [`Format`]. Defaults to [`Format::Zstd1`].
    pub format: Format,
}

impl Default for EncoderOptions {
    fn default() -> Self {
        Self {
            block_size: BLOCK_SIZE_MAX,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::default(),
            parameters: ParameterOverrides::default(),
            pledged_src_size: None,
            write_content_size: true,
            format: Format::Zstd1,
        }
    }
}

impl EncoderOptions {
    /// Builder-style override for [`Self::compression_level`].
    pub fn with_compression_level(mut self, compression_level: CompressionLevel) -> Self {
        self.compression_level = compression_level;
        self
    }

    /// Builder-style override for [`Self::write_dict_id`].
    pub fn with_write_dict_id(mut self, write_dict_id: bool) -> Self {
        self.write_dict_id = write_dict_id;
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CompressionParameters {
    pub(crate) match_finder: MatchFinderParameters,
    pub(crate) max_history_bytes: usize,
    pub(crate) upstream_cparams: UpstreamCompressionParameters,
    pub(crate) use_row_match_finder: bool,
    /// The long-distance matcher's table parameters, or `None` when it will not
    /// run. Resolving to `None` rather than carrying a separate flag keeps the
    /// two from disagreeing: there are no parameters to read when it is off.
    pub(crate) ldm: Option<LdmParameters>,
    /// What the caller asked for, unresolved. The two questions C asks of this
    /// are not the same question, so the mode is carried rather than a decision
    /// made from it; see [`Self::literals_compression_disabled`].
    pub(crate) literal_compression: LiteralCompressionMode,
}

impl CompressionParameters {
    /// Whether this configuration forbids Huffman-coding the literals section,
    /// upstream's `ZSTD_literalsCompressionIsDisabled`.
    ///
    /// The `Auto` arm reads the *resolved* parameters rather than the level, so
    /// it stays correct when an explicit strategy or target length is asked for
    /// directly. Among the levels only the negative ones satisfy it, because
    /// every `Fast` row at a positive level carries `target_length == 0`.
    ///
    /// This is not the predicate the optimal parser's cost model uses. C asks
    /// two different questions of the same parameter: this one, which decides
    /// how the block is *written* and resolves `auto` against the strategy, and
    /// `ZSTD_compressedLiterals`, which decides how a literal is *priced* and
    /// treats `auto` as coded without consulting anything. They differ under
    /// `auto` with an accelerated `Fast`, which no optimal parse can be, so the
    /// distinction is invisible today — but conflating them would make the
    /// parser's prices depend on the strategy in a way C's never do.
    pub(crate) fn literals_compression_disabled(&self) -> bool {
        match self.literal_compression {
            LiteralCompressionMode::Enabled => false,
            LiteralCompressionMode::Disabled => true,
            LiteralCompressionMode::Auto => {
                self.upstream_cparams.strategy == UpstreamStrategy::Fast
                    && self.upstream_cparams.target_length > 0
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum BlockTraceDecision {
    Raw,
    Rle,
    Compressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum BlockTraceMode {
    Predefined,
    Rle,
    FseCompressed,
    Repeat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub struct BlockTraceSequenceModes {
    pub literal_lengths: BlockTraceMode,
    pub offsets: BlockTraceMode,
    pub match_lengths: BlockTraceMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum BlockTraceParserStrategy {
    Fast,
    DoubleFast,
    Greedy,
    Lazy,
    Lazy2,
    GreedyRow,
    LazyRow,
    Lazy2Row,
    BinaryTreeLazy2,
    BinaryTreeOpt,
    BinaryTreeUltra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum BlockTraceUpstreamStrategy {
    Fast,
    DoubleFast,
    Greedy,
    Lazy,
    Lazy2,
    BinaryTreeLazy2,
    BinaryTreeOpt,
    BinaryTreeUltra,
    BinaryTreeUltra2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum BlockTraceDictionaryMode {
    None,
    ExtDict,
    DictMatchState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum BlockTraceDictionaryTableSource {
    None,
    Prefix,
    Prepared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub struct BlockTraceCompressionParameters {
    pub window_log: u32,
    pub chain_log: u32,
    pub hash_log: u32,
    pub search_log: u32,
    pub min_match: u32,
    pub target_length: u32,
    pub strategy: BlockTraceUpstreamStrategy,
    pub use_row_match_finder: bool,
    pub dictionary_mode: BlockTraceDictionaryMode,
    pub prepared_match_state: bool,
    pub chain_table_allocated: bool,
    pub row_hash_log: Option<u32>,
    pub dict_table_source: BlockTraceDictionaryTableSource,
    pub parser_strategy: BlockTraceParserStrategy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[doc(hidden)]
pub struct BlockTraceRepcodeStats {
    pub rep1: u32,
    pub rep2: u32,
    pub rep3: u32,
    pub rep1_minus1: u32,
    pub explicit_offsets: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[doc(hidden)]
pub enum BlockTraceMatchSource {
    #[default]
    Unknown,
    Dict,
    Prefix,
    Source,
    Rep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[doc(hidden)]
pub struct BlockTraceRegularMatchSourceCounts {
    pub dict: u32,
    pub prefix: u32,
    pub source: u32,
    pub unknown: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[doc(hidden)]
pub struct BlockTraceRepMatchSourceCounts {
    pub dict: u32,
    pub prefix: u32,
    pub source: u32,
    pub unknown: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[doc(hidden)]
pub struct BlockTraceRowSearchContest {
    pub winner: BlockTraceMatchSource,
    pub source_length: usize,
    pub dict_length: usize,
    pub attempts_left_before_dict: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[doc(hidden)]
pub struct BlockTraceAcceptedRegularMatch {
    pub source: BlockTraceMatchSource,
    pub start: usize,
    pub length: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[doc(hidden)]
pub enum BlockTraceEmittedMatchKind {
    #[default]
    Unknown,
    Regular,
    Rep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[doc(hidden)]
pub struct BlockTraceEmittedMatch {
    pub kind: BlockTraceEmittedMatchKind,
    pub source: BlockTraceMatchSource,
    pub start: usize,
    pub literal_length: usize,
    pub length: usize,
    pub off_base: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[doc(hidden)]
pub struct BlockTraceParserStats {
    pub literal_bytes: usize,
    pub matched_bytes: usize,
    pub repcodes: BlockTraceRepcodeStats,
    pub regular_match_sources: BlockTraceRegularMatchSourceCounts,
    pub rep_match_sources: BlockTraceRepMatchSourceCounts,
    pub explicit_offset_sum: u64,
    pub explicit_offset_count: u32,
    pub first_match_source: Option<BlockTraceMatchSource>,
    pub offset_code_counts: [u32; 32],
    pub first_row_search_contest: Option<BlockTraceRowSearchContest>,
    pub first_emitted_match: Option<BlockTraceEmittedMatch>,
    pub second_emitted_match: Option<BlockTraceEmittedMatch>,
    pub third_emitted_match: Option<BlockTraceEmittedMatch>,
    pub fourth_emitted_match: Option<BlockTraceEmittedMatch>,
    pub first_accepted_regular_match: Option<BlockTraceAcceptedRegularMatch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub struct BlockTrace {
    pub raw_size: usize,
    pub sequence_count: usize,
    pub parser_stats: BlockTraceParserStats,
    pub compression_parameters: BlockTraceCompressionParameters,
    pub literal_section_size: usize,
    pub sequence_header_size: usize,
    pub last_count_size: usize,
    pub sequence_bitstream_size: usize,
    pub sequence_modes: Option<BlockTraceSequenceModes>,
    pub long_offsets: bool,
    pub candidate_compressed_size: Option<usize>,
    pub decision: BlockTraceDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[doc(hidden)]
pub struct EncodeStageProfile {
    pub total: Duration,
    pub block_split: Duration,
    pub planning: Duration,
    pub planning_row_search: Duration,
    pub planning_chain_search: Duration,
    pub planning_rep_check: Duration,
    pub planning_match_count: Duration,
    pub planning_insert_update: Duration,
    pub planning_parser: Duration,
    pub planning_row_parser_baseline_rep: Duration,
    pub planning_row_parser_baseline_regular: Duration,
    pub planning_row_parser_continue: Duration,
    pub planning_row_parser_store: Duration,
    pub planning_row_parser_rep2: Duration,
    pub literals: Duration,
    pub sequences: Duration,
    /// `sequences`, split the way upstream splits its own: generating the
    /// symbol codes (`ZSTD_seqToCodes`), building the three FSE tables
    /// (`ZSTD_buildSequencesStatistics`), writing the bitstream
    /// (`ZSTD_encodeSequences`), and assembling the section. Only the last has
    /// no upstream counterpart: C writes the section into the output buffer
    /// where the profiling path stages it in scratch and copies. The four sum
    /// to `sequences`.
    pub sequence_codes: Duration,
    pub sequence_statistics: Duration,
    pub sequence_bitstream: Duration,
    pub sequence_assembly: Duration,
    pub blocks: usize,
    pub compressed_blocks: usize,
    pub raw_blocks: usize,
    pub rle_blocks: usize,
}

/// Whether a stage profile also measures the planner's phase sub-breakdown.
///
/// The two cannot be measured at once. The sub-breakdown is instrumented with
/// a timer taken per lazy parser step, which is lost in the noise next to a
/// double-fast parse and dominates a lazy one: `binary-structured`'s first
/// block profiles at 17.91 ms at level 5 against a real 1.02 ms, and the
/// profile then attributes 99% of the frame to planning because 99% of what it
/// measured *was* the timers.
///
/// So a caller has to say which of the two it wants. [`Self::Off`] leaves the
/// stage totals and their shares true, and reports every `planning_*` field as
/// zero. [`Self::On`] fills those fields in, at the cost of a total that is
/// inflated by an amount that grows with how much work the parser does per
/// byte -- read its percentages, never its milliseconds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[doc(hidden)]
pub enum PlannerPhases {
    /// Time the named stages only. The default, because a profile whose total
    /// is wrong is worse than one that answers fewer questions.
    #[default]
    Off,
    /// Also time each phase of the lazy parser, inflating the total.
    On,
}

impl PlannerPhases {
    fn is_on(self) -> bool {
        matches!(self, Self::On)
    }
}

impl From<SequenceCompressionMode> for BlockTraceMode {
    fn from(value: SequenceCompressionMode) -> Self {
        match value {
            SequenceCompressionMode::Predefined => Self::Predefined,
            SequenceCompressionMode::Rle => Self::Rle,
            SequenceCompressionMode::FseCompressed => Self::FseCompressed,
            SequenceCompressionMode::Repeat => Self::Repeat,
        }
    }
}

impl From<ParserStrategy> for BlockTraceParserStrategy {
    fn from(value: ParserStrategy) -> Self {
        match value {
            ParserStrategy::Fast => Self::Fast,
            ParserStrategy::DoubleFast => Self::DoubleFast,
            ParserStrategy::Greedy => Self::Greedy,
            ParserStrategy::Lazy => Self::Lazy,
            ParserStrategy::Lazy2 => Self::Lazy2,
            ParserStrategy::GreedyRow => Self::GreedyRow,
            ParserStrategy::LazyRow => Self::LazyRow,
            ParserStrategy::Lazy2Row => Self::Lazy2Row,
            ParserStrategy::BinaryTreeLazy2 => Self::BinaryTreeLazy2,
            ParserStrategy::BinaryTreeOpt => Self::BinaryTreeOpt,
            ParserStrategy::BinaryTreeUltra => Self::BinaryTreeUltra,
        }
    }
}

impl From<UpstreamStrategy> for BlockTraceUpstreamStrategy {
    fn from(value: UpstreamStrategy) -> Self {
        match value {
            UpstreamStrategy::Fast => Self::Fast,
            UpstreamStrategy::DoubleFast => Self::DoubleFast,
            UpstreamStrategy::Greedy => Self::Greedy,
            UpstreamStrategy::Lazy => Self::Lazy,
            UpstreamStrategy::Lazy2 => Self::Lazy2,
            UpstreamStrategy::BinaryTreeLazy2 => Self::BinaryTreeLazy2,
            UpstreamStrategy::BinaryTreeOpt => Self::BinaryTreeOpt,
            UpstreamStrategy::BinaryTreeUltra => Self::BinaryTreeUltra,
            UpstreamStrategy::BinaryTreeUltra2 => Self::BinaryTreeUltra2,
        }
    }
}

impl From<PrefixMatchMode> for BlockTraceDictionaryMode {
    fn from(value: PrefixMatchMode) -> Self {
        match value {
            PrefixMatchMode::ExtDict => Self::ExtDict,
            PrefixMatchMode::DictMatchState => Self::DictMatchState,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LiteralsRepeatMode {
    #[default]
    None,
    Check,
    Valid,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct LiteralsEncodingState {
    huffman_table: Option<huff0::CTableX1>,
    repeat_mode: LiteralsRepeatMode,
    /// Whether this frame may Huffman-code its literals at all.
    ///
    /// Upstream's `ZSTD_literalsCompressionIsDisabled`, which under the default
    /// `ZSTD_ps_auto` reads `strategy == ZSTD_fast && targetLength > 0`. Only
    /// the negative levels satisfy both: every `ZSTD_fast` row at a positive
    /// level has `targetLength == 0`.
    ///
    /// This lives on the literals state rather than beside it because the state
    /// already reaches every literals encoder, and because it configures the
    /// same thing the rest of the struct carries.
    compression_disabled: bool,
}

impl LiteralsEncodingState {
    /// Frame-initial literals state for `dictionary` under `params`.
    ///
    /// `params` is required rather than applied afterwards because
    /// `compression_disabled` cannot be recovered from anything else on this
    /// struct: a state that has silently lost it still encodes valid frames,
    /// still decodes, and merely stops matching upstream. Making the policy an
    /// argument means a new construction site cannot forget it.
    pub(crate) fn new(dictionary: Option<&Dictionary<'_>>, params: CompressionParameters) -> Self {
        let compression_disabled = params.literals_compression_disabled();
        let Some(dictionary) = dictionary else {
            return Self {
                compression_disabled,
                ..Self::default()
            };
        };
        let repeat_mode = if dictionary.huffman_repeat_valid() {
            LiteralsRepeatMode::Valid
        } else {
            LiteralsRepeatMode::Check
        };
        Self::with_table_and_repeat_mode(
            dictionary.huffman_encoding_table(),
            repeat_mode,
            compression_disabled,
        )
    }

    fn with_table_and_repeat_mode(
        huffman_table: Option<&huff0::CTableX1>,
        repeat_mode: LiteralsRepeatMode,
        compression_disabled: bool,
    ) -> Self {
        Self {
            huffman_table: huffman_table.copied(),
            repeat_mode: if huffman_table.is_some() {
                repeat_mode
            } else {
                LiteralsRepeatMode::None
            },
            compression_disabled,
        }
    }

    fn huffman_table(&self) -> Option<&huff0::CTableX1> {
        self.huffman_table.as_ref()
    }

    fn repeat_mode(self) -> LiteralsRepeatMode {
        self.repeat_mode
    }
}

#[derive(Default)]
pub(crate) struct EntropyEncodeScratch {
    huffman_dst: Vec<u8>,
    sequence_bitstream: Vec<u8>,
    sequence_workspace: SequenceEncodeScratch,
    planned_sequences: SequencePlan,
    literals_section: Vec<u8>,
    literals_candidate: Vec<u8>,
    sequence_section: Vec<u8>,
    block_payload: Vec<u8>,
    /// Cached contiguous match state, reused across encode calls to avoid
    /// per-frame hash table re-allocation. The parameters it was built for
    /// travel inside it, so a state built for different ones is refused.
    cached_match_state: Option<ContiguousBlockMatchState>,
    /// Reusable output buffer, swapped in/out across encode calls to avoid
    /// per-frame heap allocation.
    output_buf: Vec<u8>,
    /// Huffman compression workspace (~13KB), reused across blocks to avoid
    /// repeated stack allocation + zeroing. Matches C's approach of passing
    /// workspace from the caller.
    pub(crate) huf_workspace: huff0::CompressWorkspace,
}

/// Every buffer in [`EntropyEncodeScratch`], borrowed at once.
///
/// The point of handing them back as one tuple is that the borrow checker sees
/// a single split of `&mut self` into disjoint fields; taking them through
/// separate accessors would conflict.
type EntropyEncodeScratchSplit<'a> = (
    &'a mut Vec<u8>,
    &'a mut Vec<u8>,
    &'a mut SequenceEncodeScratch,
    &'a mut SequencePlan,
    &'a mut Vec<u8>,
    &'a mut Vec<u8>,
    &'a mut Vec<u8>,
    &'a mut Vec<u8>,
    &'a mut huff0::CompressWorkspace,
);

impl EntropyEncodeScratch {
    /// Drop the parser state that belongs to a single frame: the optimal
    /// parser's price model and the search structures keyed to that frame's
    /// bytes. Everything else here is a buffer, and buffers are what this type
    /// exists to carry between frames.
    ///
    /// A reused encoder that skipped this would open a frame with the previous
    /// frame's statistics, and `btultra2` would take that for a price model it
    /// had already seeded and decline to seed a new one.
    pub(crate) fn clear_frame_parser_state(&mut self) {
        self.planned_sequences.opt_price_state = None;
        self.planned_sequences.opt_hash3 = None;
        self.planned_sequences.opt_source_bt = None;
        self.planned_sequences.opt_dict_bt = None;
    }

    /// Re-key the parser state held here to a frame buffer whose first `delta`
    /// bytes have been dropped, as a streaming compaction does.
    ///
    /// Most of what this type carries is a buffer, and a buffer has no opinion
    /// about which bytes it is next filled from. The optimal parser's
    /// three-byte table is the exception: it is a search structure that happens
    /// to be reused rather than a scratch buffer, and it addresses the frame by
    /// absolute index. It lives here rather than on the match state, which is
    /// how it came to be the one table a compaction never re-keyed.
    ///
    /// The two binary trees are not in the same position. They are rebuilt
    /// whenever the prefix they embed changes length, and a compaction retires
    /// the prefixed state altogether, so no drop ever reaches them.
    pub(crate) fn shift_frame_positions(&mut self, delta: usize) {
        if let Some(hash3) = self.planned_sequences.opt_hash3.as_mut() {
            hash3.shift_positions(delta);
        }
    }

    fn split(&mut self) -> EntropyEncodeScratchSplit<'_> {
        (
            &mut self.huffman_dst,
            &mut self.sequence_bitstream,
            &mut self.sequence_workspace,
            &mut self.planned_sequences,
            &mut self.literals_section,
            &mut self.literals_candidate,
            &mut self.sequence_section,
            &mut self.block_payload,
            &mut self.huf_workspace,
        )
    }
}

#[derive(Default)]
struct EncodeStageProfiler {
    total: Duration,
    block_split: Duration,
    planning: Duration,
    planning_row_search: Duration,
    planning_chain_search: Duration,
    planning_rep_check: Duration,
    planning_match_count: Duration,
    planning_insert_update: Duration,
    planning_parser: Duration,
    planning_row_parser_baseline_rep: Duration,
    planning_row_parser_baseline_regular: Duration,
    planning_row_parser_continue: Duration,
    planning_row_parser_store: Duration,
    planning_row_parser_rep2: Duration,
    literals: Duration,
    sequences: Duration,
    sequence_codes: Duration,
    sequence_statistics: Duration,
    sequence_bitstream: Duration,
    sequence_assembly: Duration,
    blocks: usize,
    compressed_blocks: usize,
    raw_blocks: usize,
    rle_blocks: usize,
}

impl EncodeStageProfiler {
    fn finish(self) -> EncodeStageProfile {
        EncodeStageProfile {
            total: self.total,
            block_split: self.block_split,
            planning: self.planning,
            planning_row_search: self.planning_row_search,
            planning_chain_search: self.planning_chain_search,
            planning_rep_check: self.planning_rep_check,
            planning_match_count: self.planning_match_count,
            planning_insert_update: self.planning_insert_update,
            planning_parser: self.planning_parser,
            planning_row_parser_baseline_rep: self.planning_row_parser_baseline_rep,
            planning_row_parser_baseline_regular: self.planning_row_parser_baseline_regular,
            planning_row_parser_continue: self.planning_row_parser_continue,
            planning_row_parser_store: self.planning_row_parser_store,
            planning_row_parser_rep2: self.planning_row_parser_rep2,
            literals: self.literals,
            sequences: self.sequences,
            sequence_codes: self.sequence_codes,
            sequence_statistics: self.sequence_statistics,
            sequence_bitstream: self.sequence_bitstream,
            sequence_assembly: self.sequence_assembly,
            blocks: self.blocks,
            compressed_blocks: self.compressed_blocks,
            raw_blocks: self.raw_blocks,
            rle_blocks: self.rle_blocks,
        }
    }

    fn record_raw_block(&mut self) {
        self.blocks += 1;
        self.raw_blocks += 1;
    }

    fn record_rle_block(&mut self) {
        self.blocks += 1;
        self.rle_blocks += 1;
    }

    fn record_compressed_block(&mut self) {
        self.blocks += 1;
        self.compressed_blocks += 1;
    }

    fn record_planning_profile(&mut self, plan: &SequencePlan) {
        let profile = plan.planning_profile();
        self.planning_row_search += profile.row_search;
        self.planning_chain_search += profile.chain_search;
        self.planning_rep_check += profile.rep_check;
        self.planning_match_count += profile.match_count;
        self.planning_insert_update += profile.insert_update;
        self.planning_parser += profile.parser;
        self.planning_row_parser_baseline_rep += profile.row_parser_baseline_rep;
        self.planning_row_parser_baseline_regular += profile.row_parser_baseline_regular;
        self.planning_row_parser_continue += profile.row_parser_continue;
        self.planning_row_parser_store += profile.row_parser_store;
        self.planning_row_parser_rep2 += profile.row_parser_rep2;
    }
}

#[derive(Debug, Clone, Copy)]
struct PlannedBlockSummary {
    sequence_count: usize,
}

impl PlannedBlockSummary {
    /// Whether to try encoding this block as literals with no sequences.
    ///
    /// The attempt used to be gated on a guess about whether the literals would
    /// compress, so that a hopeless block did not pay a histogram over every
    /// byte. `huff0::Compressibility` now carries C's own version of that
    /// judgement into the one place that can act on it cheaply, so the attempt
    /// is always worth making.
    fn should_try_zero_sequence_block(self) -> bool {
        self.sequence_count == 0
    }
}

/// Largest frame the encoder can produce from `src_len` bytes under `options`.
///
/// This is the counterpart of upstream's `ZSTD_compressBound`, and it exists
/// for the same reason: a caller who must not allocate during the encode needs
/// to know, before starting, how much room to hand over. Size a buffer with
/// this and [`encode_into_slice`] cannot report
/// [`Error::DstSizeTooSmall`]; size a `Vec` with it and
/// [`Encoder::encode_into`] will not reallocate.
///
/// # What bounds it
///
/// Compression can enlarge input, so the bound is built from the worst case
/// the encoder will actually emit rather than from a ratio:
///
/// - **Frame header, 18 bytes.** Magic (4), frame header descriptor (1),
///   window descriptor (1), dictionary id (up to 4), frame content size
///   (up to 8). Not every frame carries every field, but one can.
/// - **Payload, `src_len` bytes.** A block is emitted compressed only when it
///   came out strictly smaller than its input (`compression_wins`), and an
///   RLE block is one byte, so no block's payload ever exceeds the bytes it
///   covers.
/// - **Three bytes per block.** Block count is bounded by the *smallest* block
///   the encoder can emit, which is not always `options.block_size`: at the
///   maximum block size the splitter may end a block early, down to
///   `UPSTREAM_SPLIT_CHUNK_SIZE`. Below the maximum the splitter does not run
///   and blocks are exactly `options.block_size`.
/// - **Four bytes** for the content checksum, when enabled.
///
/// # How much of this is measured
///
/// `compress_bound_holds_for_every_level_and_shape` encodes real inputs and
/// checks the frames against this, because a bound derived by reading the
/// encoder is a claim about the encoder and not a measurement of it. On the
/// shape that binds it — incompressible bytes, where every block goes raw —
/// a 1 MB frame at 1 KiB blocks lands **9 bytes** under at the default level,
/// 8 at level 1. That pins the payload
/// and per-block terms exactly, and dropping the per-block term fails the test
/// by 870 bytes.
///
/// Two parts are deliberately slack and no test here makes them bind:
///
/// - **The last 8 or 9 bytes of the header allowance.** Reaching 18 needs a
///   four-byte dictionary id *and* an eight-byte content size, and the latter
///   is only emitted above 4 GiB. Frames a test can afford use 9 or 10.
/// - **The splitter's minimum block.** Splitting requires the frame to have
///   banked savings, savings require compression to have worked, and a frame
///   where compression worked has far more payload slack than the extra
///   3-byte headers cost. The two conditions fight each other: on mixed
///   content built specifically to split every 8 KiB, the frame still came in
///   16 KB to 76 KB under. The term costs 345 bytes of over-allocation on a
///   1 MB buffer at the maximum block size and is kept because being wrong
///   here is unrecoverable while being generous is 0.03%.
pub fn compress_bound(src_len: usize, options: EncoderOptions) -> usize {
    let block_size = options.block_size.clamp(1, BLOCK_SIZE_MAX);
    let smallest_block = if block_size < UPSTREAM_SPLIT_FULL_BLOCK_SIZE {
        block_size
    } else {
        UPSTREAM_SPLIT_CHUNK_SIZE
    };
    // Empty input still costs one block: an empty last block terminates the
    // frame.
    let block_count = src_len.div_ceil(smallest_block).max(1);

    FRAME_HEADER_MAX
        + block_count * BLOCK_HEADER_SIZE
        + src_len
        + if options.checksum { CHECKSUM_SIZE } else { 0 }
}

/// Encode `src` as a single Zstandard frame using default [`EncoderOptions`]
/// and return the compressed bytes.
pub fn encode_all(src: &[u8]) -> Result<Vec<u8>> {
    encode_all_with_options(src, EncoderOptions::default())
}

/// Encode `src` into a caller-owned slice, returning the frame's length.
///
/// The destination is yours: `dst` is written in place, and no part of the
/// frame is staged through a buffer this crate owns. Use it when the output
/// belongs to an arena, an FFI caller, or anything else that will not accept a
/// `Vec`.
///
/// This form builds a fresh [`Encoder`] per call, so it also pays for that
/// encoder's scratch. Hold an [`Encoder`] and call
/// [`Encoder::encode_into_slice`] to keep the scratch across calls; see there
/// for what a warm encoder does and does not still allocate.
///
/// `dst` does not have to be [`compress_bound`] bytes, and a smaller buffer
/// often succeeds — but only sizing it to the bound guarantees success. When
/// the frame does not fit, the result is [`Error::DstSizeTooSmall`] and `dst`
/// holds an arbitrary prefix that must not be treated as a frame.
///
/// ```
/// use zstandard::{EncoderOptions, compress_bound, decode_all, encode_into_slice};
///
/// let payload = b"the same sentence, the same sentence, the same sentence";
/// let options = EncoderOptions::default();
///
/// let mut buffer = vec![0u8; compress_bound(payload.len(), options)];
/// let written = encode_into_slice(payload, &mut buffer, options)?;
///
/// assert_eq!(decode_all(&buffer[..written])?, payload);
/// # Ok::<(), zstandard::Error>(())
/// ```
pub fn encode_into_slice(src: &[u8], dst: &mut [u8], options: EncoderOptions) -> Result<usize> {
    Encoder::new().encode_into_slice(src, dst, options)
}

/// Like [`encode_all`] but uses caller-supplied [`EncoderOptions`].
pub fn encode_all_with_options(src: &[u8], options: EncoderOptions) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_all_into_scratch(
        src,
        options,
        None,
        &mut EntropyEncodeScratch::default(),
        &mut OutBuf::growable(&mut out),
    )?;
    Ok(out)
}

/// Encode `src` using a pre-parsed dictionary. Prefer this over
/// [`encode_all_with_dict`] when reusing the same dictionary across many calls.
pub fn encode_all_with_prepared_dict(src: &[u8], dict: &EncoderDictionary<'_>) -> Result<Vec<u8>> {
    encode_all_with_prepared_dict_and_options(src, dict, EncoderOptions::default())
}

/// Like [`encode_all_with_prepared_dict`] but uses caller-supplied [`EncoderOptions`].
pub fn encode_all_with_prepared_dict_and_options(
    src: &[u8],
    dict: &EncoderDictionary<'_>,
    options: EncoderOptions,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_all_into_scratch(
        src,
        options,
        Some(dict),
        &mut EntropyEncodeScratch::default(),
        &mut OutBuf::growable(&mut out),
    )?;
    Ok(out)
}

/// Encode `src` using `dict` as a dictionary. The slice is parsed on every
/// call; use [`encode_all_with_prepared_dict`] for repeated use.
pub fn encode_all_with_dict(src: &[u8], dict: &[u8]) -> Result<Vec<u8>> {
    encode_all_with_dict_and_options(src, dict, EncoderOptions::default())
}

/// Like [`encode_all_with_dict`] but uses caller-supplied [`EncoderOptions`].
pub fn encode_all_with_dict_and_options(
    src: &[u8],
    dict: &[u8],
    options: EncoderOptions,
) -> Result<Vec<u8>> {
    let dictionary = EncoderDictionary::new(dict)?;
    encode_all_with_prepared_dict_and_options(src, &dictionary, options)
}

#[cfg(any(feature = "internal-trace", test))]
// Compiled into test builds so unit tests can reach the trace API without
// turning the feature on. Not every entry point has a default-feature test,
// so under `cfg(test)` alone some are legitimately unused.
#[cfg_attr(not(feature = "internal-trace"), allow(dead_code))]
#[doc(hidden)]
pub fn trace_first_block_with_options(src: &[u8], options: EncoderOptions) -> Result<BlockTrace> {
    trace_first_block_inner(src, options, None, &mut EntropyEncodeScratch::default())
}

#[cfg(any(feature = "internal-trace", test))]
// Compiled into test builds so unit tests can reach the trace API without
// turning the feature on. Not every entry point has a default-feature test,
// so under `cfg(test)` alone some are legitimately unused.
#[cfg_attr(not(feature = "internal-trace"), allow(dead_code))]
#[doc(hidden)]
pub fn trace_first_block_with_prepared_dict_and_options(
    src: &[u8],
    dict: &EncoderDictionary<'_>,
    options: EncoderOptions,
) -> Result<BlockTrace> {
    trace_first_block_inner(
        src,
        options,
        Some(dict),
        &mut EntropyEncodeScratch::default(),
    )
}

#[cfg(any(feature = "internal-trace", test))]
// Compiled into test builds so unit tests can reach the trace API without
// turning the feature on. Not every entry point has a default-feature test,
// so under `cfg(test)` alone some are legitimately unused.
#[cfg_attr(not(feature = "internal-trace"), allow(dead_code))]
#[doc(hidden)]
pub fn profile_first_block_with_options(
    src: &[u8],
    options: EncoderOptions,
    phases: PlannerPhases,
) -> Result<EncodeStageProfile> {
    profile_encode_inner(
        &src[..src.len().min(options.block_size)],
        options,
        None,
        phases,
        &mut EntropyEncodeScratch::default(),
    )
}

#[cfg(any(feature = "internal-trace", test))]
// Compiled into test builds so unit tests can reach the trace API without
// turning the feature on. Not every entry point has a default-feature test,
// so under `cfg(test)` alone some are legitimately unused.
#[cfg_attr(not(feature = "internal-trace"), allow(dead_code))]
#[doc(hidden)]
pub fn profile_first_block_with_prepared_dict_and_options(
    src: &[u8],
    dict: &EncoderDictionary<'_>,
    options: EncoderOptions,
    phases: PlannerPhases,
) -> Result<EncodeStageProfile> {
    profile_encode_inner(
        &src[..src.len().min(options.block_size)],
        options,
        Some(dict),
        phases,
        &mut EntropyEncodeScratch::default(),
    )
}

/// Reusable one-shot encoder that amortizes buffer allocation across calls.
///
/// # Footprint
///
/// Roughly 19 KB by value: the scratch is mostly the Huffman compression
/// workspace, held as inline arrays rather than behind a pointer. Being reusable
/// this is a type callers tend to store, so box it where it would otherwise sit
/// in an enum, in a future held across an `await`, or on a small stack.
/// [`Decoder`](crate::Decoder) is tens of bytes and needs no such care.
pub struct Encoder {
    scratch: EntropyEncodeScratch,
}

impl Encoder {
    /// Construct an `Encoder` with empty scratch buffers.
    pub fn new() -> Self {
        Self {
            scratch: EntropyEncodeScratch::default(),
        }
    }

    /// See [`encode_all`] for the equivalent free-function form.
    pub fn encode_all(&mut self, src: &[u8]) -> Result<Vec<u8>> {
        self.encode_all_with_options(src, EncoderOptions::default())
    }

    /// See [`encode_all_with_options`] for the equivalent free-function form.
    pub fn encode_all_with_options(
        &mut self,
        src: &[u8],
        options: EncoderOptions,
    ) -> Result<Vec<u8>> {
        let mut out = core::mem::take(&mut self.scratch.output_buf);
        out.clear();
        encode_all_into_scratch(
            src,
            options,
            None,
            &mut self.scratch,
            &mut OutBuf::growable(&mut out),
        )?;
        // Swap a zero-length Vec back so the *capacity* stays with `out`.
        // On the next call we will `take` from output_buf again.  If the
        // caller drops the returned Vec the allocator reclaims it; if they
        // keep reusing the Encoder we just re-allocate once.
        Ok(out)
    }

    /// Encode into a caller-provided buffer, avoiding per-call output
    /// allocation entirely.
    pub fn encode_into(&mut self, src: &[u8], dst: &mut Vec<u8>) -> Result<()> {
        self.encode_into_with_options(src, dst, EncoderOptions::default())
    }

    /// Encode into a caller-provided buffer with options.
    pub fn encode_into_with_options(
        &mut self,
        src: &[u8],
        dst: &mut Vec<u8>,
        options: EncoderOptions,
    ) -> Result<()> {
        dst.clear();
        encode_all_into_scratch(
            src,
            options,
            None,
            &mut self.scratch,
            &mut OutBuf::growable(dst),
        )
    }

    /// Encode into a caller-provided buffer with a prepared dictionary.
    pub fn encode_into_with_prepared_dict_and_options(
        &mut self,
        src: &[u8],
        dst: &mut Vec<u8>,
        dict: &EncoderDictionary<'_>,
        options: EncoderOptions,
    ) -> Result<()> {
        dst.clear();
        encode_all_into_scratch(
            src,
            options,
            Some(dict),
            &mut self.scratch,
            &mut OutBuf::growable(dst),
        )
    }

    /// Encode into a caller-owned slice, returning the frame's length.
    ///
    /// The destination is never allocated, grown, or replaced: output goes
    /// straight into `dst`. The entropy scratch lives on `self`, so a warm
    /// encoder does not rebuild it either — "warm" meaning it has already
    /// grown to what the work needs, which is after one call for a loop over
    /// similar frames, and again whenever an input exceeds anything this
    /// encoder has seen.
    ///
    /// It is not, however, allocation-*free*. A warm encode still makes about
    /// ten small allocations per frame, none of them larger than a few dozen
    /// bytes and none of them scaling with the input: the `Vec`s the sequence
    /// table choices carry, and the frame header's content-size and
    /// dictionary-id fields. `tests/allocation.rs` measures this and pins the
    /// "does not scale with input" half of it, which is the part a caller
    /// sizing a memory budget depends on.
    ///
    /// See [`encode_into_slice`] for the free-function form and for what
    /// happens when `dst` is too small.
    pub fn encode_into_slice(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        options: EncoderOptions,
    ) -> Result<usize> {
        self.encode_into_slice_inner(src, dst, None, options)
    }

    /// Encode into a caller-owned slice with a prepared dictionary.
    pub fn encode_into_slice_with_prepared_dict_and_options(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        dict: &EncoderDictionary<'_>,
        options: EncoderOptions,
    ) -> Result<usize> {
        self.encode_into_slice_inner(src, dst, Some(dict), options)
    }

    fn encode_into_slice_inner(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        dict: Option<&EncoderDictionary<'_>>,
        options: EncoderOptions,
    ) -> Result<usize> {
        let mut out = OutBuf::fixed(dst);
        encode_all_into_scratch(src, options, dict, &mut self.scratch, &mut out)?;
        // Checked before the length is returned, never after a partial write is
        // handed out: an overflowed encode leaves a prefix in `dst` that is a
        // syntactically plausible frame, so reporting its length would be
        // silent truncation rather than a failure.
        if out.overflowed() {
            return Err(Error::DstSizeTooSmall);
        }
        Ok(out.len())
    }

    /// See [`encode_all_with_prepared_dict`] for the equivalent free-function form.
    pub fn encode_all_with_prepared_dict(
        &mut self,
        src: &[u8],
        dict: &EncoderDictionary<'_>,
    ) -> Result<Vec<u8>> {
        self.encode_all_with_prepared_dict_and_options(src, dict, EncoderOptions::default())
    }

    /// See [`encode_all_with_prepared_dict_and_options`] for the equivalent free-function form.
    pub fn encode_all_with_prepared_dict_and_options(
        &mut self,
        src: &[u8],
        dict: &EncoderDictionary<'_>,
        options: EncoderOptions,
    ) -> Result<Vec<u8>> {
        let mut out = core::mem::take(&mut self.scratch.output_buf);
        out.clear();
        encode_all_into_scratch(
            src,
            options,
            Some(dict),
            &mut self.scratch,
            &mut OutBuf::growable(&mut out),
        )?;
        Ok(out)
    }

    /// See [`encode_all_with_dict`] for the equivalent free-function form.
    pub fn encode_all_with_dict(&mut self, src: &[u8], dict: &[u8]) -> Result<Vec<u8>> {
        self.encode_all_with_dict_and_options(src, dict, EncoderOptions::default())
    }

    /// See [`encode_all_with_dict_and_options`] for the equivalent free-function form.
    pub fn encode_all_with_dict_and_options(
        &mut self,
        src: &[u8],
        dict: &[u8],
        options: EncoderOptions,
    ) -> Result<Vec<u8>> {
        let dictionary = EncoderDictionary::new(dict)?;
        self.encode_all_with_prepared_dict_and_options(src, &dictionary, options)
    }

    #[doc(hidden)]
    pub fn profile_first_block_with_options(
        &mut self,
        src: &[u8],
        options: EncoderOptions,
        phases: PlannerPhases,
    ) -> Result<EncodeStageProfile> {
        profile_encode_inner(
            &src[..src.len().min(options.block_size)],
            options,
            None,
            phases,
            &mut self.scratch,
        )
    }

    #[doc(hidden)]
    pub fn profile_first_block_with_prepared_dict_and_options(
        &mut self,
        src: &[u8],
        dict: &EncoderDictionary<'_>,
        options: EncoderOptions,
        phases: PlannerPhases,
    ) -> Result<EncodeStageProfile> {
        profile_encode_inner(
            &src[..src.len().min(options.block_size)],
            options,
            Some(dict),
            phases,
            &mut self.scratch,
        )
    }

    /// Stage timings for a whole frame rather than its first block.
    ///
    /// The first-block variants cannot see the block splitter at all. Its
    /// entry point returns early while `savings` is under 3, and `savings` is
    /// zero until a block has been encoded, so block 0 never reaches
    /// `upstream_split_block` however splittable the input is. A first-block
    /// profile therefore reports the splitter as free, which it is not: on the
    /// pinned upstream it is around 6% of a 4 MiB double-fast frame. Anything
    /// comparing stage shares against C wants this entry point.
    #[doc(hidden)]
    pub fn profile_encode_all_with_options(
        &mut self,
        src: &[u8],
        options: EncoderOptions,
        phases: PlannerPhases,
    ) -> Result<EncodeStageProfile> {
        profile_encode_inner(src, options, None, phases, &mut self.scratch)
    }

    /// [`Self::profile_encode_all_with_options`] for a frame that carries a
    /// dictionary.
    ///
    /// Without this the dictionary cases have no whole-frame profile at all:
    /// the no-dictionary entry point runs them down the contiguous path, which
    /// is a different parser, and `raw-dictionary` at level 4 then profiles at
    /// 1.52 ms against the real path's 6.42.
    #[doc(hidden)]
    pub fn profile_encode_all_with_prepared_dict_and_options(
        &mut self,
        src: &[u8],
        dict: &EncoderDictionary<'_>,
        options: EncoderOptions,
        phases: PlannerPhases,
    ) -> Result<EncodeStageProfile> {
        profile_encode_inner(src, options, Some(dict), phases, &mut self.scratch)
    }

    #[doc(hidden)]
    pub fn trace_first_block_with_options(
        &mut self,
        src: &[u8],
        options: EncoderOptions,
    ) -> Result<BlockTrace> {
        trace_first_block_inner(src, options, None, &mut self.scratch)
    }

    #[doc(hidden)]
    pub fn trace_first_block_with_prepared_dict_and_options(
        &mut self,
        src: &[u8],
        dict: &EncoderDictionary<'_>,
        options: EncoderOptions,
    ) -> Result<BlockTrace> {
        trace_first_block_inner(src, options, Some(dict), &mut self.scratch)
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

/// The window a block may reach back through, C's `windowSize = MAX(1,
/// MIN(1 << windowLog, pledgedSrcSize))` (`zstd_compress.c:2131`).
///
/// Bounded by the source because a frame cannot reach behind its own start, so
/// a window wider than the content is a window the parsers cannot fill.
fn upstream_window_size_for(params: CompressionParameters, src_len: usize) -> usize {
    params
        .max_history_bytes
        .min(MAX_DECLARABLE_WINDOW_SIZE)
        .min(src_len)
        .max(1)
}

/// Largest block the one-shot encoder will emit, C's `blockSize =
/// MIN(maxBlockSize, windowSize)` (`zstd_compress.c:2132`).
///
/// A block wider than its own window is what made the declared `Window_Size`
/// carry a block on top of the history: the fast and double-fast parsers keep
/// C's block-constant floor, which this crate clamps with `.min(block_start)`,
/// so such a block is floored at its own start rather than a window back from
/// its end. Capping here removes the cause instead of declaring around it, and
/// is what upstream does.
///
/// This binds only when the window is narrower than a block, which no
/// compression level produces — fitting the parameters to the source keeps the
/// window at least as wide as the frame, and above that the window starts at
/// 512 KiB against 128 KiB blocks. A `window_log` override reaches it.
fn block_size_max_for(
    params: CompressionParameters,
    options: EncoderOptions,
    src_len: usize,
) -> usize {
    options
        .block_size
        .min(upstream_window_size_for(params, src_len))
}

/// Largest offset the one-shot encoder can emit for `src_len`, and therefore
/// the smallest `Window_Size` a decoder needs to replay the frame.
///
/// The history alone, which is what upstream declares (`windowSize = (U32)1 <<
/// params->cParams.windowLog`, `zstd_compress.c:4703`).
///
/// This used to carry a block on top, for two reasons that are both now gone.
/// The fast pair could be floored at a wide block's start rather than a window
/// back from its end, which [`block_size_max_for`] removes by capping the block.
/// The prefixed parsers could emit a match below the source floor they were
/// handed, which was `BinaryTreeFinder` bounding combined prefix-and-source
/// indices by a source-space floor; see `BinaryTreeFinder::stored_index_floor`.
///
/// `every_parser_stays_inside_the_window_it_declares` and
/// `every_parser_stays_inside_its_window_with_a_dictionary` are what hold this
/// down: with the block at or below the window, `Window_Size` is the window and
/// a decoder rejecting the frame is the check.
fn frame_window_size_for(params: CompressionParameters) -> u64 {
    params.max_history_bytes.min(MAX_DECLARABLE_WINDOW_SIZE) as u64
}

fn encode_all_into_scratch(
    src: &[u8],
    options: EncoderOptions,
    prepared_dictionary: Option<&EncoderDictionary<'_>>,
    scratch: &mut EntropyEncodeScratch,
    out: &mut OutBuf<'_>,
) -> Result<()> {
    validate_options_for_one_shot(options, src.len())?;
    let dictionary = prepared_dictionary.map(EncoderDictionary::as_inner);
    let params = compression_parameters_for_options(options, Some(src.len()), dictionary);
    let block_size_max = block_size_max_for(params, options, src.len());
    let block_count = if src.is_empty() {
        1usize
    } else {
        src.len().div_ceil(block_size_max)
    };
    let required_capacity = 13 + (block_count * 4) + src.len() + usize::from(options.checksum) * 4;
    // Every caller hands over an empty buffer, so `reserve` counts from zero.
    out.reserve(required_capacity);

    let dictionary_id = options
        .write_dict_id
        .then(|| dictionary.and_then(Dictionary::frame_dictionary_id))
        .flatten();

    // A single-segment header sets `Window_Size == Frame_Content_Size`, which
    // forces every decoder to size its window to the whole payload. Upstream
    // rejects anything above `ZSTD_WINDOWLOG_MAX` (128 MiB) outright, so
    // unconditionally emitting one made large frames undecodable by the
    // reference implementation. Only take that path when the content actually
    // fits inside the window this level would use; otherwise declare the
    // window we really need.
    //
    // Upstream derives `Single_Segment_flag` the same way rather than taking
    // it as a setting: `singleSegment = contentSizeFlag && (windowSize >=
    // pledgedSrcSize)` (`zstd_compress.c:4704`). With the content size
    // suppressed there is nothing for that flag to agree with — the format
    // requires a `Frame_Content_Size` whenever it is set — so the frame
    // declares a window and stays silent about its length.
    let frame_window_size = frame_window_size_for(params);
    if !options.write_content_size {
        write_windowed_header_with_dict(
            out,
            frame_window_size,
            options.checksum,
            dictionary_id,
            options.format,
        )?;
    } else if src.len() as u64 <= frame_window_size {
        write_single_segment_header_with_dict(
            out,
            src.len() as u64,
            options.checksum,
            dictionary_id,
            options.format,
        );
    } else {
        write_windowed_header_with_content_size(
            out,
            frame_window_size,
            src.len() as u64,
            options.checksum,
            dictionary_id,
            options.format,
        )?;
    }

    if src.is_empty() {
        BlockHeader {
            last_block: true,
            block_type: BlockType::Raw,
            block_size: 0,
        }
        .write_to(out);
    } else {
        let mut repeat_offsets =
            dictionary.map_or_else(RepeatOffsets::default, Dictionary::repeat_offsets);
        let mut sequence_tables = dictionary.map_or_else(
            SequenceEncodingState::default,
            Dictionary::sequence_encoding_state,
        );
        let mut literals_state = LiteralsEncodingState::new(dictionary, params);
        let mut block_start = 0usize;
        let dictionary_content = dictionary.map_or(&[][..], Dictionary::matching_content);
        let prepared_match_state = prepared_dictionary
            .and_then(|dictionary| dictionary.prepared_match_state(params.match_finder));
        let mut savings = 0i64;
        scratch.clear_frame_parser_state();
        // What decides the parser is whether there is any history to match
        // against, which is the dictionary's *content*, not whether a
        // dictionary was supplied. A dictionary with no content contributes no
        // history, so the frame's own bytes are the whole window and this is
        // the contiguous case exactly as if none had been passed.
        //
        // Dispatching on `dictionary.is_some()` instead sent the empty
        // dictionary down a third path that re-derived each block's history as
        // a prefix slice. That path emitted matches which do not exist in the
        // source -- a 12-byte match whose bytes agree for 4 -- so the frame
        // failed its own checksum, and upstream rejected it too. Covered by
        // `an_empty_dictionary_encodes_the_same_frame_as_no_dictionary`.
        if dictionary_content.is_empty() {
            // Reuse cached match state when parameters are compatible, avoiding
            // per-frame hash table re-allocation.
            let mut match_state = scratch
                .cached_match_state
                .take()
                .and_then(|mut cached| {
                    cached
                        .reset_if_compatible(params.match_finder)
                        .then_some(cached)
                })
                .unwrap_or_else(|| ContiguousBlockMatchState::new(src.len(), params.match_finder));
            let mut ldm = params
                .ldm
                .map(|ldm| LdmFrameState::new(ldm, params.max_history_bytes));
            let first_block_size = upstream_optimal_block_size(
                src,
                0,
                block_size_max,
                params.upstream_cparams.strategy,
                0,
            );
            seed_optimal_prices_from_first_block(
                scratch,
                &src[..first_block_size],
                repeat_offsets,
                params,
                &mut match_state,
                ldm.as_mut(),
            )?;
            while block_start < src.len() {
                // C determines block size via ZSTD_findBlockSize: min(blockSizeMax,
                // remaining). For optimal strategies (btopt/btultra/btultra2), the
                // pre-compression block-split fingerprinting heuristic is expensive
                // (sampling_rate=1 scans every byte) and rarely beneficial — it adds
                // 30-40% overhead on highly-compressible data while finding no split
                // points. Use C's simple formula for these strategies. Lower strategies
                // keep the heuristic since their cheaper sampling_rate still pays off
                // for heterogeneous data.
                let block_size = if params.upstream_cparams.strategy.is_optimal() {
                    block_size_max.min(src.len() - block_start)
                } else {
                    upstream_optimal_block_size(
                        src,
                        block_start,
                        block_size_max,
                        params.upstream_cparams.strategy,
                        savings,
                    )
                };
                let block_end = block_start + block_size;
                let block_out_start = out.len();
                encode_block_into_contiguous(
                    out,
                    src,
                    block_start,
                    block_end,
                    &mut match_state,
                    &mut literals_state,
                    block_end == src.len(),
                    &mut repeat_offsets,
                    &mut sequence_tables,
                    scratch,
                    params,
                    ldm.as_mut(),
                )?;
                savings += block_size as i64 - (out.len() - block_out_start) as i64;
                block_start = block_end;
            }
            // Cache match state for reuse in the next call.
            scratch.cached_match_state = Some(match_state);
        } else {
            // `dictionary_content` is read off `dictionary` above, so content
            // this branch can see implies the dictionary it came from.
            let dict = dictionary.expect("dictionary content comes from a dictionary");
            // Seed the optimal parser's pricing model from the dictionary's
            // actual Huffman/FSE tables, matching C's ZSTD_rescaleFreqs.
            scratch.planned_sequences.opt_dict_price_seed = dict.optimal_price_seed();
            let mut match_state = PrefixedBlockMatchState::new_with_prepared_match_state(
                dictionary_content,
                src.len(),
                params.match_finder,
                prefix_match_mode_for_dictionary(
                    dict,
                    Some(src.len()),
                    params.upstream_cparams.strategy,
                ),
                prepared_match_state.as_deref(),
            );
            // The dictionary is hashed in before the first block, the way
            // `ZSTD_loadDictionaryContent` does it, so a match can reach into it
            // from the very first position of the frame.
            let mut ldm = params.ldm.map(|ldm| {
                let mut state = LdmFrameState::new(ldm, params.max_history_bytes);
                state.load_dictionary(dictionary_content);
                state
            });
            while block_start < src.len() {
                let block_size = if params.upstream_cparams.strategy.is_optimal() {
                    block_size_max.min(src.len() - block_start)
                } else {
                    upstream_optimal_block_size(
                        src,
                        block_start,
                        block_size_max,
                        params.upstream_cparams.strategy,
                        savings,
                    )
                };
                let block_end = block_start + block_size;
                let block_out_start = out.len();
                encode_block_into_prefixed_contiguous(
                    out,
                    dictionary_content,
                    src,
                    block_start,
                    block_end,
                    &mut match_state,
                    &mut literals_state,
                    block_end == src.len(),
                    &mut repeat_offsets,
                    &mut sequence_tables,
                    scratch,
                    params,
                    ldm.as_mut(),
                )?;
                savings += block_size as i64 - (out.len() - block_out_start) as i64;
                block_start = block_end;
            }
        }
    }

    if options.checksum {
        out.extend_from_slice(&(xxh64(src, 0) as u32).to_le_bytes());
    }

    Ok(())
}

fn trace_first_block_inner(
    src: &[u8],
    options: EncoderOptions,
    prepared_dictionary: Option<&EncoderDictionary<'_>>,
    scratch: &mut EntropyEncodeScratch,
) -> Result<BlockTrace> {
    validate_options(options)?;
    let dictionary = prepared_dictionary.map(EncoderDictionary::as_inner);
    let params = compression_parameters_for_options(options, Some(src.len()), dictionary);
    let dictionary_mode = dictionary.map_or(BlockTraceDictionaryMode::None, |dictionary| {
        prefix_match_mode_for_dictionary(
            dictionary,
            Some(src.len()),
            params.upstream_cparams.strategy,
        )
        .into()
    });
    let prepared_match_state = prepared_dictionary
        .and_then(|dictionary| dictionary.prepared_match_state(params.match_finder));
    let compression_parameters = block_trace_compression_parameters(
        params,
        dictionary_mode,
        prepared_match_state.as_deref(),
    );
    let raw_size = src.len().min(options.block_size);
    if raw_size == 0 {
        return Ok(BlockTrace {
            raw_size: 0,
            sequence_count: 0,
            parser_stats: BlockTraceParserStats {
                literal_bytes: 0,
                matched_bytes: 0,
                repcodes: BlockTraceRepcodeStats::default(),
                ..Default::default()
            },
            compression_parameters,
            literal_section_size: 0,
            sequence_header_size: 0,
            last_count_size: 0,
            sequence_bitstream_size: 0,
            sequence_modes: None,
            long_offsets: false,
            candidate_compressed_size: None,
            decision: BlockTraceDecision::Raw,
        });
    }

    let chunk = &src[..raw_size];
    if should_emit_raw_block_without_compression(chunk.len()) {
        return Ok(BlockTrace {
            raw_size,
            sequence_count: 0,
            parser_stats: BlockTraceParserStats {
                literal_bytes: raw_size,
                matched_bytes: 0,
                repcodes: BlockTraceRepcodeStats::default(),
                ..Default::default()
            },
            compression_parameters,
            literal_section_size: 0,
            sequence_header_size: 0,
            last_count_size: 0,
            sequence_bitstream_size: 0,
            sequence_modes: None,
            long_offsets: false,
            candidate_compressed_size: None,
            decision: BlockTraceDecision::Raw,
        });
    }
    if chunk.len() >= 2 && all_bytes_equal(chunk) {
        return Ok(BlockTrace {
            raw_size,
            sequence_count: 0,
            parser_stats: BlockTraceParserStats {
                literal_bytes: raw_size,
                matched_bytes: 0,
                repcodes: BlockTraceRepcodeStats::default(),
                ..Default::default()
            },
            compression_parameters,
            literal_section_size: 0,
            sequence_header_size: 0,
            last_count_size: 0,
            sequence_bitstream_size: 0,
            sequence_modes: None,
            long_offsets: false,
            candidate_compressed_size: None,
            decision: BlockTraceDecision::Rle,
        });
    }

    let repeat_offsets = dictionary.map_or_else(RepeatOffsets::default, Dictionary::repeat_offsets);
    let sequence_tables = dictionary.map_or_else(
        SequenceEncodingState::default,
        Dictionary::sequence_encoding_state,
    );
    let literals_state = LiteralsEncodingState::new(dictionary, params);
    let parser_strategy = params.match_finder.parser_strategy;
    let (
        huffman_dst,
        sequence_bitstream,
        sequence_workspace,
        sequence_plan,
        literals_section,
        literals_candidate,
        sequence_section,
        _block_payload,
        huf_workspace,
    ) = scratch.split();
    sequence_plan.enable_tracing();

    let dictionary_content = dictionary.map_or(&[][..], Dictionary::matching_content);
    // Content, not presence: a dictionary with none of it is no history, which
    // is the contiguous case. See the matching dispatch in
    // `encode_all_into_scratch`.
    if dictionary_content.is_empty() {
        let mut match_state = ContiguousBlockMatchState::new(src.len(), params.match_finder);
        // `chunk`, not `src`: every other caller of this planner passes a slice
        // that ends where the block ends (`&frame[..block_end]` in the frame
        // loop, `chunk` in the sibling tracer just above), because the planner
        // parses to the end of what it is given. Passing the whole input made
        // this trace parse the entire frame as one block while still reporting
        // `raw_size` as the block size, so every derived figure -- sequence
        // count, literal section size, offset code histogram -- described a
        // different span than the one it named. On `json-records` at 1 MiB
        // that read as 18065 sequences for a block that ships 2973. The
        // dictionary branch below always passed `chunk` and was never wrong.
        plan_sequences_for_contiguous_block_into(
            sequence_plan,
            chunk,
            0,
            repeat_offsets,
            params.match_finder,
            params.max_history_bytes,
            &mut match_state,
        )?;
    } else {
        let dict = dictionary.expect("dictionary content comes from a dictionary");
        sequence_plan.opt_dict_price_seed = dict.optimal_price_seed();
        let mut match_state = PrefixedBlockMatchState::new_with_prepared_match_state(
            dictionary_content,
            src.len(),
            params.match_finder,
            prefix_match_mode_for_dictionary(
                dict,
                Some(src.len()),
                params.upstream_cparams.strategy,
            ),
            prepared_match_state.as_deref(),
        );
        plan_sequences_for_prefixed_contiguous_block_into(
            sequence_plan,
            dictionary_content,
            chunk,
            0,
            repeat_offsets,
            params.match_finder,
            params.max_history_bytes,
            &mut match_state,
        )?;
    }

    let summary = summarize_planned_block(sequence_plan, sequence_workspace)?;
    let parser_stats = block_trace_parser_stats(
        sequence_plan,
        repeat_offsets,
        dictionary.map_or(0, |dictionary| dictionary.matching_content().len()),
        dictionary_mode,
    )?;
    if summary.should_try_zero_sequence_block() {
        let candidate = match encode_zero_sequence_compressed_block_owned(
            chunk,
            &literals_state,
            parser_strategy,
            huffman_dst,
            huf_workspace,
        ) {
            Ok(candidate) => candidate,
            Err(Error::SrcSizeWrong) => None,
            Err(error) => return Err(error),
        };
        let candidate_compressed_size = candidate.as_ref().map(|(payload, _)| payload.len());
        let decision = if candidate_compressed_size
            .is_some_and(|size| compression_wins(size, chunk.len(), parser_strategy))
        {
            BlockTraceDecision::Compressed
        } else {
            BlockTraceDecision::Raw
        };
        return Ok(BlockTrace {
            raw_size,
            sequence_count: 0,
            parser_stats,
            compression_parameters,
            literal_section_size: candidate_compressed_size
                .map_or(0, |size| size.saturating_sub(1)),
            sequence_header_size: usize::from(candidate_compressed_size.is_some()),
            last_count_size: 0,
            sequence_bitstream_size: 0,
            sequence_modes: None,
            long_offsets: false,
            candidate_compressed_size,
            decision,
        });
    }
    if summary.sequence_count == 0 {
        return Ok(BlockTrace {
            raw_size,
            sequence_count: 0,
            parser_stats,
            compression_parameters,
            literal_section_size: 0,
            sequence_header_size: 0,
            last_count_size: 0,
            sequence_bitstream_size: 0,
            sequence_modes: None,
            long_offsets: false,
            candidate_compressed_size: None,
            decision: BlockTraceDecision::Raw,
        });
    }

    let (literal_section_size, _) = match encode_literals_section_into(
        &sequence_plan.literals,
        &literals_state,
        parser_strategy,
        literals_compressibility(sequence_plan.literals.len(), summary.sequence_count),
        huffman_dst,
        literals_section,
        literals_candidate,
        huf_workspace,
    ) {
        Ok(result) => result,
        Err(Error::SrcSizeWrong) => {
            return Ok(BlockTrace {
                raw_size,
                sequence_count: summary.sequence_count,
                parser_stats,
                compression_parameters,
                literal_section_size: 0,
                sequence_header_size: 0,
                last_count_size: 0,
                sequence_bitstream_size: 0,
                sequence_modes: None,
                long_offsets: false,
                candidate_compressed_size: None,
                decision: BlockTraceDecision::Raw,
            });
        }
        Err(error) => return Err(error),
    };
    let (_, sequence_stats) =
        match encode_prepared_seq_store_section_with_strategy_and_scratch_into_stats(
            sequence_section,
            &sequence_tables,
            parser_strategy,
            sequence_bitstream,
            sequence_plan,
            sequence_workspace,
            &mut SequenceSectionTimings::default(),
        ) {
            Ok(result) => result,
            Err(Error::SrcSizeWrong) => {
                return Ok(BlockTrace {
                    raw_size,
                    sequence_count: summary.sequence_count,
                    parser_stats,
                    compression_parameters,
                    literal_section_size: 0,
                    sequence_header_size: 0,
                    last_count_size: 0,
                    sequence_bitstream_size: 0,
                    sequence_modes: None,
                    long_offsets: false,
                    candidate_compressed_size: None,
                    decision: BlockTraceDecision::Raw,
                });
            }
            Err(error) => return Err(error),
        };
    let candidate_compressed_size = (!sequence_section_triggers_legacy_decoder_bug(
        sequence_stats.last_count_size,
        sequence_stats.bitstream_size,
    ))
    .then_some(literal_section_size + sequence_section.len());
    let decision = if candidate_compressed_size
        .is_some_and(|size| compression_wins(size, chunk.len(), parser_strategy))
    {
        BlockTraceDecision::Compressed
    } else {
        BlockTraceDecision::Raw
    };

    Ok(BlockTrace {
        raw_size,
        sequence_count: summary.sequence_count,
        parser_stats,
        compression_parameters,
        literal_section_size,
        sequence_header_size: sequence_stats.header_size,
        last_count_size: sequence_stats.last_count_size,
        sequence_bitstream_size: sequence_stats.bitstream_size,
        sequence_modes: sequence_stats.modes.map(block_trace_modes),
        long_offsets: sequence_stats.long_offsets,
        candidate_compressed_size,
        decision,
    })
}

fn profile_encode_inner(
    src: &[u8],
    options: EncoderOptions,
    prepared_dictionary: Option<&EncoderDictionary<'_>>,
    phases: PlannerPhases,
    scratch: &mut EntropyEncodeScratch,
) -> Result<EncodeStageProfile> {
    let total_start = Instant::now();
    validate_options(options)?;
    let dictionary = prepared_dictionary.map(EncoderDictionary::as_inner);
    let params = compression_parameters_for_options(options, Some(src.len()), dictionary);
    let mut profiler = EncodeStageProfiler::default();

    if src.is_empty() {
        profiler.record_raw_block();
        profiler.total = total_start.elapsed();
        return Ok(profiler.finish());
    }

    let mut repeat_offsets =
        dictionary.map_or_else(RepeatOffsets::default, Dictionary::repeat_offsets);
    let mut sequence_tables = dictionary.map_or_else(
        SequenceEncodingState::default,
        Dictionary::sequence_encoding_state,
    );
    let mut literals_state = LiteralsEncodingState::new(dictionary, params);
    let mut block_start = 0usize;
    let dictionary_content = dictionary.map_or(&[][..], Dictionary::matching_content);
    let prepared_match_state = prepared_dictionary
        .and_then(|dictionary| dictionary.prepared_match_state(params.match_finder));
    let mut savings = 0i64;
    // Clear cross-block pricing state from any previous frame.
    scratch.planned_sequences.opt_price_state = None;
    scratch.planned_sequences.opt_source_bt = None;
    scratch.planned_sequences.opt_dict_bt = None;

    // Content, not presence, so this profiles the branch `encode_all_into_scratch`
    // actually takes. See the comment there.
    if dictionary_content.is_empty() {
        let mut match_state = ContiguousBlockMatchState::new(src.len(), params.match_finder);
        while block_start < src.len() {
            let split_start = Instant::now();
            let block_size = upstream_optimal_block_size(
                src,
                block_start,
                options.block_size,
                params.upstream_cparams.strategy,
                savings,
            );
            profiler.block_split += split_start.elapsed();
            let block_end = block_start + block_size;
            let encoded_size = profile_block_into_contiguous(
                src,
                block_start,
                block_end,
                &mut match_state,
                &mut literals_state,
                &mut repeat_offsets,
                &mut sequence_tables,
                scratch,
                params,
                phases,
                &mut profiler,
            )?;
            savings += block_size as i64 - encoded_size as i64;
            block_start = block_end;
        }
    } else {
        let dict = dictionary.expect("dictionary content comes from a dictionary");
        scratch.planned_sequences.opt_dict_price_seed = dict.optimal_price_seed();
        let mut match_state = PrefixedBlockMatchState::new_with_prepared_match_state(
            dictionary_content,
            src.len(),
            params.match_finder,
            prefix_match_mode_for_dictionary(
                dict,
                Some(src.len()),
                params.upstream_cparams.strategy,
            ),
            prepared_match_state.as_deref(),
        );
        while block_start < src.len() {
            let split_start = Instant::now();
            let block_size = upstream_optimal_block_size(
                src,
                block_start,
                options.block_size,
                params.upstream_cparams.strategy,
                savings,
            );
            profiler.block_split += split_start.elapsed();
            let block_end = block_start + block_size;
            let encoded_size = profile_block_into_prefixed_contiguous(
                dictionary_content,
                src,
                block_start,
                block_end,
                &mut match_state,
                &mut literals_state,
                &mut repeat_offsets,
                &mut sequence_tables,
                scratch,
                params,
                phases,
                &mut profiler,
            )?;
            savings += block_size as i64 - encoded_size as i64;
            block_start = block_end;
        }
    }

    profiler.total = total_start.elapsed();
    Ok(profiler.finish())
}

fn block_trace_modes(modes: SequenceCompressionModes) -> BlockTraceSequenceModes {
    BlockTraceSequenceModes {
        literal_lengths: modes.literal_lengths.into(),
        offsets: modes.offsets.into(),
        match_lengths: modes.match_lengths.into(),
    }
}

fn block_trace_compression_parameters(
    params: CompressionParameters,
    dictionary_mode: BlockTraceDictionaryMode,
    prepared_match_state: Option<&PreparedDictionaryMatchState>,
) -> BlockTraceCompressionParameters {
    let prepared_match_state_used = prepared_match_state.is_some();
    BlockTraceCompressionParameters {
        window_log: params.upstream_cparams.window_log,
        chain_log: params.upstream_cparams.chain_log,
        hash_log: params.upstream_cparams.hash_log,
        search_log: params.upstream_cparams.search_log,
        min_match: params.upstream_cparams.min_match,
        target_length: params.upstream_cparams.target_length,
        strategy: params.upstream_cparams.strategy.into(),
        use_row_match_finder: params.use_row_match_finder,
        dictionary_mode,
        prepared_match_state: prepared_match_state_used,
        chain_table_allocated: block_trace_chain_table_allocated(params, prepared_match_state),
        row_hash_log: block_trace_row_hash_log(params, prepared_match_state),
        dict_table_source: block_trace_dictionary_table_source(
            dictionary_mode,
            prepared_match_state_used,
        ),
        parser_strategy: params.match_finder.parser_strategy.into(),
    }
}

fn block_trace_chain_table_allocated(
    params: CompressionParameters,
    prepared_match_state: Option<&PreparedDictionaryMatchState>,
) -> bool {
    if let Some(prepared_match_state) = prepared_match_state {
        return prepared_match_state.chain_table_allocated();
    }
    matches!(
        params.match_finder.parser_strategy,
        ParserStrategy::DoubleFast
    ) || params.match_finder.parser_strategy.is_hash_chain()
        || params.match_finder.parser_strategy.is_binary_tree()
}

fn block_trace_row_hash_log(
    params: CompressionParameters,
    prepared_match_state: Option<&PreparedDictionaryMatchState>,
) -> Option<u32> {
    if let Some(row_hash_log) =
        prepared_match_state.and_then(PreparedDictionaryMatchState::row_hash_log)
    {
        return Some(row_hash_log);
    }
    params.match_finder.parser_strategy.is_row_hash().then_some(
        params
            .match_finder
            .hash_bits
            .saturating_sub(params.match_finder.search_log.clamp(4, 6)),
    )
}

fn block_trace_dictionary_table_source(
    dictionary_mode: BlockTraceDictionaryMode,
    prepared_match_state: bool,
) -> BlockTraceDictionaryTableSource {
    match dictionary_mode {
        BlockTraceDictionaryMode::None => BlockTraceDictionaryTableSource::None,
        _ if prepared_match_state => BlockTraceDictionaryTableSource::Prepared,
        _ => BlockTraceDictionaryTableSource::Prefix,
    }
}

fn block_trace_parser_stats(
    plan: &SequencePlan,
    initial_repeat_offsets: RepeatOffsets,
    prefix_len: usize,
    dictionary_mode: BlockTraceDictionaryMode,
) -> Result<BlockTraceParserStats> {
    let mut repcodes = BlockTraceRepcodeStats::default();
    let mut regular_match_sources = BlockTraceRegularMatchSourceCounts::default();
    let mut rep_match_sources = BlockTraceRepMatchSourceCounts::default();
    let mut offset_code_counts = [0u32; 32];
    let mut matched_bytes = 0usize;
    let mut explicit_offset_sum = 0u64;
    let mut explicit_offset_count = 0u32;
    let mut first_match_source = None;
    let mut first_emitted_match = None;
    let mut second_emitted_match = None;
    let mut third_emitted_match = None;
    let mut fourth_emitted_match = None;
    let mut first_accepted_regular_match = None;
    let mut repeat_offsets = initial_repeat_offsets;
    let mut current_pos = 0usize;

    for (index, sequence) in plan.sequences.iter().enumerate() {
        matched_bytes = matched_bytes
            .checked_add(sequence.match_length as usize)
            .ok_or(Error::OutputSizeOverflow)?;
        offset_code_counts[offset_code(sequence.offset_value)? as usize] += 1;
        let repcode_kind = classify_trace_repcode(sequence);
        match repcode_kind {
            TraceRepcodeKind::Rep1 => repcodes.rep1 += 1,
            TraceRepcodeKind::Rep2 => repcodes.rep2 += 1,
            TraceRepcodeKind::Rep3 => repcodes.rep3 += 1,
            TraceRepcodeKind::Rep1Minus1 => repcodes.rep1_minus1 += 1,
            TraceRepcodeKind::Explicit => repcodes.explicit_offsets += 1,
        }
        let raw_offset = repeat_offsets.resolve(sequence)?;
        let trace_source = plan
            .trace_match_sources
            .get(index)
            .copied()
            .unwrap_or(SequenceTraceMatchSource::Unknown);
        let trace_emission = plan.trace_emissions.get(index).copied().unwrap_or_default();
        let match_start = current_pos
            .checked_add(sequence.literal_length as usize)
            .ok_or(Error::OutputSizeOverflow)?;
        if first_match_source.is_none() {
            first_match_source = Some(match block_trace_first_match_source(trace_source) {
                BlockTraceMatchSource::Unknown => match trace_emission.kind {
                    SequenceTraceEmissionKind::Rep => BlockTraceMatchSource::Rep,
                    SequenceTraceEmissionKind::Regular
                        if dictionary_mode == BlockTraceDictionaryMode::None =>
                    {
                        BlockTraceMatchSource::Source
                    }
                    SequenceTraceEmissionKind::Regular => BlockTraceMatchSource::Unknown,
                },
                known => known,
            });
        }
        let emitted_kind = match block_trace_emitted_match_kind(trace_source) {
            BlockTraceEmittedMatchKind::Unknown => match trace_emission.kind {
                SequenceTraceEmissionKind::Regular => BlockTraceEmittedMatchKind::Regular,
                SequenceTraceEmissionKind::Rep => BlockTraceEmittedMatchKind::Rep,
            },
            known => known,
        };
        let emitted_source = match block_trace_emitted_match_source(
            trace_source,
            match_start,
            raw_offset as usize,
            prefix_len,
            dictionary_mode,
        ) {
            BlockTraceMatchSource::Unknown if dictionary_mode == BlockTraceDictionaryMode::None => {
                BlockTraceMatchSource::Source
            }
            known => known,
        };
        let emitted_match = BlockTraceEmittedMatch {
            kind: emitted_kind,
            source: emitted_source,
            start: match_start,
            literal_length: sequence.literal_length as usize,
            length: sequence.match_length as usize,
            off_base: sequence.offset_value as usize,
            offset: raw_offset as usize,
        };
        if first_emitted_match.is_none() {
            first_emitted_match = Some(emitted_match);
        } else if second_emitted_match.is_none() {
            second_emitted_match = Some(emitted_match);
        } else if third_emitted_match.is_none() {
            third_emitted_match = Some(emitted_match);
        } else if fourth_emitted_match.is_none() {
            fourth_emitted_match = Some(emitted_match);
        }
        if first_accepted_regular_match.is_none()
            && !matches!(
                trace_source,
                SequenceTraceMatchSource::Rep | SequenceTraceMatchSource::Unknown
            )
        {
            first_accepted_regular_match = Some(BlockTraceAcceptedRegularMatch {
                source: block_trace_first_match_source(trace_source),
                start: match_start,
                length: sequence.match_length as usize,
                offset: raw_offset as usize,
            });
        }
        match trace_source {
            SequenceTraceMatchSource::Dict => regular_match_sources.dict += 1,
            SequenceTraceMatchSource::Prefix => regular_match_sources.prefix += 1,
            SequenceTraceMatchSource::Source | SequenceTraceMatchSource::LongDistance => {
                regular_match_sources.source += 1
            }
            SequenceTraceMatchSource::Rep => match block_trace_rep_match_source(
                match_start,
                raw_offset as usize,
                prefix_len,
                dictionary_mode,
            ) {
                BlockTraceMatchSource::Dict => rep_match_sources.dict += 1,
                BlockTraceMatchSource::Prefix => rep_match_sources.prefix += 1,
                BlockTraceMatchSource::Source => rep_match_sources.source += 1,
                _ => rep_match_sources.unknown += 1,
            },
            SequenceTraceMatchSource::Unknown => regular_match_sources.unknown += 1,
        }
        if matches!(repcode_kind, TraceRepcodeKind::Explicit) {
            explicit_offset_sum = explicit_offset_sum
                .checked_add(raw_offset as u64)
                .ok_or(Error::OutputSizeOverflow)?;
            explicit_offset_count = explicit_offset_count
                .checked_add(1)
                .ok_or(Error::OutputSizeOverflow)?;
        }
        current_pos = current_pos
            .checked_add(sequence.literal_length as usize)
            .and_then(|value| value.checked_add(sequence.match_length as usize))
            .ok_or(Error::OutputSizeOverflow)?;
    }

    Ok(BlockTraceParserStats {
        literal_bytes: plan.literals.len(),
        matched_bytes,
        repcodes,
        regular_match_sources,
        rep_match_sources,
        explicit_offset_sum,
        explicit_offset_count,
        first_match_source,
        offset_code_counts,
        first_row_search_contest: plan
            .trace_first_row_contest
            .map(block_trace_row_search_contest),
        first_emitted_match,
        second_emitted_match,
        third_emitted_match,
        fourth_emitted_match,
        first_accepted_regular_match,
    })
}

fn block_trace_first_match_source(trace_source: SequenceTraceMatchSource) -> BlockTraceMatchSource {
    match trace_source {
        SequenceTraceMatchSource::Dict => BlockTraceMatchSource::Dict,
        SequenceTraceMatchSource::Prefix => BlockTraceMatchSource::Prefix,
        SequenceTraceMatchSource::Source | SequenceTraceMatchSource::LongDistance => {
            BlockTraceMatchSource::Source
        }
        SequenceTraceMatchSource::Rep => BlockTraceMatchSource::Rep,
        SequenceTraceMatchSource::Unknown => BlockTraceMatchSource::Unknown,
    }
}

fn block_trace_emitted_match_kind(
    trace_source: SequenceTraceMatchSource,
) -> BlockTraceEmittedMatchKind {
    match trace_source {
        SequenceTraceMatchSource::Dict
        | SequenceTraceMatchSource::Prefix
        | SequenceTraceMatchSource::Source
        | SequenceTraceMatchSource::LongDistance => BlockTraceEmittedMatchKind::Regular,
        SequenceTraceMatchSource::Rep => BlockTraceEmittedMatchKind::Rep,
        SequenceTraceMatchSource::Unknown => BlockTraceEmittedMatchKind::Unknown,
    }
}

fn block_trace_emitted_match_source(
    trace_source: SequenceTraceMatchSource,
    match_start: usize,
    raw_offset: usize,
    prefix_len: usize,
    dictionary_mode: BlockTraceDictionaryMode,
) -> BlockTraceMatchSource {
    match trace_source {
        SequenceTraceMatchSource::Rep => {
            block_trace_rep_match_source(match_start, raw_offset, prefix_len, dictionary_mode)
        }
        _ => block_trace_first_match_source(trace_source),
    }
}

fn block_trace_row_search_contest(
    contest: SequenceTraceRowSearchContest,
) -> BlockTraceRowSearchContest {
    BlockTraceRowSearchContest {
        winner: block_trace_first_match_source(contest.winner),
        source_length: contest.source_length,
        dict_length: contest.dict_length,
        attempts_left_before_dict: contest.attempts_left_before_dict,
    }
}

fn block_trace_rep_match_source(
    match_pos: usize,
    raw_offset: usize,
    prefix_len: usize,
    dictionary_mode: BlockTraceDictionaryMode,
) -> BlockTraceMatchSource {
    let history_len = prefix_len.saturating_add(match_pos);
    if raw_offset == 0 || raw_offset > history_len {
        return BlockTraceMatchSource::Unknown;
    }
    let match_start = history_len - raw_offset;
    if match_start < prefix_len {
        return match dictionary_mode {
            BlockTraceDictionaryMode::None => BlockTraceMatchSource::Unknown,
            BlockTraceDictionaryMode::ExtDict => BlockTraceMatchSource::Prefix,
            BlockTraceDictionaryMode::DictMatchState => BlockTraceMatchSource::Dict,
        };
    }
    BlockTraceMatchSource::Source
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TraceRepcodeKind {
    Rep1,
    Rep2,
    Rep3,
    Rep1Minus1,
    Explicit,
}

fn classify_trace_repcode(sequence: &SequenceCommand) -> TraceRepcodeKind {
    let literal_length_zero = sequence.literal_length == 0;
    match (sequence.offset_value, literal_length_zero) {
        (1, false) => TraceRepcodeKind::Rep1,
        (1, true) => TraceRepcodeKind::Rep2,
        (2, false) => TraceRepcodeKind::Rep2,
        (2, true) => TraceRepcodeKind::Rep3,
        (3, false) => TraceRepcodeKind::Rep3,
        (3, true) => TraceRepcodeKind::Rep1Minus1,
        _ => TraceRepcodeKind::Explicit,
    }
}

pub(crate) fn validate_options(options: EncoderOptions) -> Result<()> {
    if options.block_size == 0 || options.block_size > BLOCK_SIZE_MAX {
        return Err(Error::InvalidParameter("block_size must be in 1..=128 KiB"));
    }
    options.parameters.validate()?;
    Ok(())
}

/// [`validate_options`], plus the checks only a one-shot encode can make.
///
/// A pledge is a promise about the whole frame, so the one-shot path is the
/// one place it can be settled immediately rather than at `finish`.
fn validate_options_for_one_shot(options: EncoderOptions, src_len: usize) -> Result<()> {
    validate_options(options)?;
    if let Some(pledged) = options.pledged_src_size {
        if pledged != src_len as u64 {
            return Err(Error::InvalidParameter(
                "pledged_src_size does not match the input length",
            ));
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn encode_block_into(
    out: &mut OutBuf<'_>,
    chunk: &[u8],
    last_block: bool,
    literals_state: &mut LiteralsEncodingState,
    repeat_offsets: &mut RepeatOffsets,
    sequence_tables: &mut SequenceEncodingState,
    scratch: &mut EntropyEncodeScratch,
    params: CompressionParameters,
) -> Result<()> {
    encode_block_into_with_prefix(
        out,
        chunk,
        &[],
        literals_state,
        last_block,
        repeat_offsets,
        sequence_tables,
        scratch,
        params,
    )
}

pub(crate) fn encode_block_into_with_prefix(
    out: &mut OutBuf<'_>,
    chunk: &[u8],
    prefix: &[u8],
    literals_state: &mut LiteralsEncodingState,
    last_block: bool,
    repeat_offsets: &mut RepeatOffsets,
    sequence_tables: &mut SequenceEncodingState,
    scratch: &mut EntropyEncodeScratch,
    params: CompressionParameters,
) -> Result<()> {
    let prefixes = [prefix];
    encode_block_into_with_prefixes(
        out,
        chunk,
        &prefixes,
        literals_state,
        last_block,
        repeat_offsets,
        sequence_tables,
        scratch,
        params,
    )
}

pub(crate) fn encode_block_into_with_prefixes(
    out: &mut OutBuf<'_>,
    chunk: &[u8],
    prefixes: &[&[u8]],
    literals_state: &mut LiteralsEncodingState,
    last_block: bool,
    repeat_offsets: &mut RepeatOffsets,
    sequence_tables: &mut SequenceEncodingState,
    scratch: &mut EntropyEncodeScratch,
    params: CompressionParameters,
) -> Result<()> {
    if chunk.is_empty() {
        BlockHeader {
            last_block,
            block_type: BlockType::Raw,
            block_size: 0,
        }
        .write_to(out);
        return Ok(());
    }

    if should_emit_raw_block_without_compression(chunk.len()) {
        BlockHeader {
            last_block,
            block_type: BlockType::Raw,
            block_size: chunk.len() as u32,
        }
        .write_to(out);
        out.extend_from_slice(chunk);
        return Ok(());
    }

    if chunk.len() >= 2 && all_bytes_equal(chunk) {
        BlockHeader {
            last_block,
            block_type: BlockType::Rle,
            block_size: chunk.len() as u32,
        }
        .write_to(out);
        out.push(chunk[0]);
        return Ok(());
    }

    if let Some((next_repeat_offsets, next_sequence_tables, next_literals_state)) =
        encode_best_compressed_block_direct(
            out,
            chunk,
            prefixes,
            literals_state,
            *repeat_offsets,
            sequence_tables,
            scratch,
            params,
            last_block,
        )?
    {
        *repeat_offsets = next_repeat_offsets;
        *sequence_tables = next_sequence_tables;
        *literals_state = next_literals_state;
        return Ok(());
    }

    BlockHeader {
        last_block,
        block_type: BlockType::Raw,
        block_size: chunk.len() as u32,
    }
    .write_to(out);
    out.extend_from_slice(chunk);
    Ok(())
}

/// Seed the optimal parser's price model by parsing the block that opens the
/// frame twice: once to accumulate symbol statistics, then again with those
/// statistics already in hand. Does nothing unless the strategy is `btultra2`
/// and the block is long enough to say anything about its own distribution.
///
/// `block` must be the first block of a dictionary-less frame, starting at the
/// frame's own origin, and `match_state` must hold nothing yet. C asserts all
/// three in `ZSTD_initStats_ultra` (`zstd_opt.c`) and tests them in
/// `ZSTD_compressBlock_btultra2` before calling it, where they read as
/// "`ms->opt.litLengthSum == 0`, no dictionary, `curr == window.dictLimit`".
/// The pass costs twice the CPU of one block and buys around half a percent
/// on it.
///
/// C can gate this inside the block compressor because every caller reaches
/// the parser through it. Here the one-shot and streaming encoders each drive
/// their own block loop, so each has to ask; leaving the streaming one out is
/// worth 4.49% at level 21 on `wikipedia`, and the whole of that gap lands in
/// the first block.
pub(crate) fn seed_optimal_prices_from_first_block(
    scratch: &mut EntropyEncodeScratch,
    block: &[u8],
    repeat_offsets: RepeatOffsets,
    params: CompressionParameters,
    match_state: &mut ContiguousBlockMatchState,
    ldm: Option<&mut LdmFrameState>,
) -> Result<()> {
    if params.upstream_cparams.strategy != UpstreamStrategy::BinaryTreeUltra2
        || block.len() <= OPT_PREDEFINED_THRESHOLD
    {
        return Ok(());
    }

    // Pass 1 exists only for what it leaves in the price model, so its
    // sequences and its repeat offsets are both dropped: `repeat_offsets`
    // arrives by value and the plan is overwritten by pass 2. C spells the
    // same thing `U32 tmpRep[ZSTD_REP_NUM]`.
    //
    // It does see the long-distance matches, though. C generates them in
    // `ZSTD_buildSeqStore`, one level above the block compressor that runs
    // both passes, so both price the same candidates. Running the matcher
    // here rather than in pass 2 is also what keeps it running once per
    // block: `sequences_for_block` answers pass 2 from what this left.
    match ldm {
        Some(ldm) => plan_sequences_for_contiguous_block_with_ldm_into(
            &mut scratch.planned_sequences,
            block,
            0,
            repeat_offsets,
            params.match_finder,
            params.max_history_bytes,
            match_state,
            ldm,
        )?,
        None => plan_sequences_for_contiguous_block_into(
            &mut scratch.planned_sequences,
            block,
            0,
            repeat_offsets,
            params.match_finder,
            params.max_history_bytes,
            match_state,
        )?,
    }

    let price_state = scratch.planned_sequences.opt_price_state.take();
    scratch.planned_sequences.opt_hash3 = None;
    // Pass 2 must see the block as unvisited, or it would match against
    // positions pass 1 filed for bytes the frame has not emitted yet. C
    // achieves this by sliding the window forward over the block rather than
    // by clearing the tables, which comes to the same thing for a match state
    // that holds nothing else.
    match_state.reset();
    scratch.planned_sequences.opt_price_state = price_state;
    Ok(())
}

// ── Post-sequence block splitting ───────────────────────────────────────────
// Matches C zstd's postBlockSplitter: after sequence generation, recursively
// split the sequence store into sub-blocks when independent entropy tables
// compress better. Enabled for btopt+ strategies with windowLog >= 17.

const POST_SPLIT_MIN_SEQUENCES: usize = 300;
const POST_SPLIT_MAX_SPLITS: usize = 196;

fn should_try_post_sequence_split(params: &CompressionParameters, seq_count: usize) -> bool {
    let btopt_or_higher = matches!(
        params.upstream_cparams.strategy,
        UpstreamStrategy::BinaryTreeOpt
            | UpstreamStrategy::BinaryTreeUltra
            | UpstreamStrategy::BinaryTreeUltra2
    );
    btopt_or_higher && params.upstream_cparams.window_log >= 17 && seq_count > 4
}

/// Recursive binary search for sequence-store split points.
/// Returns a list of partition boundaries: [0, split1, ..., seq_count].
fn find_post_sequence_splits(
    plan: &SequencePlan,
    lit_offsets: &[usize],
    total_lit_len: usize,
    prev_seq_tables: Option<&SequenceEncodingState>,
    prev_huf_table: Option<&huff0::CTableX1>,
    literals_compression_disabled: bool,
) -> Vec<usize> {
    let seq_count = plan.sequences.len();
    let mut splits = Vec::with_capacity(POST_SPLIT_MAX_SPLITS + 2);
    derive_splits_recursive(
        &mut splits,
        0,
        seq_count,
        plan,
        lit_offsets,
        total_lit_len,
        prev_seq_tables,
        prev_huf_table,
        literals_compression_disabled,
    );
    // Build final partition table: [0, split1, ..., seq_count]
    splits.sort_unstable();
    let mut result = Vec::with_capacity(splits.len() + 2);
    result.push(0);
    result.extend_from_slice(&splits);
    result.push(seq_count);
    result
}

fn derive_splits_recursive(
    splits: &mut Vec<usize>,
    start: usize,
    end: usize,
    plan: &SequencePlan,
    lit_offsets: &[usize],
    total_lit_len: usize,
    prev_seq_tables: Option<&SequenceEncodingState>,
    prev_huf_table: Option<&huff0::CTableX1>,
    literals_compression_disabled: bool,
) {
    if end - start < POST_SPLIT_MIN_SEQUENCES || splits.len() >= POST_SPLIT_MAX_SPLITS {
        return;
    }
    let mid = (start + end) / 2;

    let whole_cost = estimate_range_cost(
        plan,
        lit_offsets,
        total_lit_len,
        start,
        end,
        prev_seq_tables,
        prev_huf_table,
        literals_compression_disabled,
    );
    let left_cost = estimate_range_cost(
        plan,
        lit_offsets,
        total_lit_len,
        start,
        mid,
        prev_seq_tables,
        prev_huf_table,
        literals_compression_disabled,
    );
    let right_cost = estimate_range_cost(
        plan,
        lit_offsets,
        total_lit_len,
        mid,
        end,
        prev_seq_tables,
        prev_huf_table,
        literals_compression_disabled,
    );

    if left_cost + right_cost < whole_cost {
        derive_splits_recursive(
            splits,
            start,
            mid,
            plan,
            lit_offsets,
            total_lit_len,
            prev_seq_tables,
            prev_huf_table,
            literals_compression_disabled,
        );
        splits.push(mid);
        derive_splits_recursive(
            splits,
            mid,
            end,
            plan,
            lit_offsets,
            total_lit_len,
            prev_seq_tables,
            prev_huf_table,
            literals_compression_disabled,
        );
    }
}

fn estimate_range_cost(
    plan: &SequencePlan,
    lit_offsets: &[usize],
    total_lit_len: usize,
    start: usize,
    end: usize,
    prev_seq_tables: Option<&SequenceEncodingState>,
    prev_huf_table: Option<&huff0::CTableX1>,
    literals_compression_disabled: bool,
) -> u64 {
    let ll_codes = &plan.literal_codes[start..end];
    let of_codes = &plan.offset_codes[start..end];
    let ml_codes = &plan.match_codes[start..end];
    let lit_start = lit_offsets[start];
    let lit_end = if end == plan.sequences.len() {
        total_lit_len
    } else {
        lit_offsets[end]
    };
    let literals = &plan.literals[lit_start..lit_end];
    estimate_subblock_cost_bits(
        ll_codes,
        of_codes,
        ml_codes,
        literals,
        prev_seq_tables,
        prev_huf_table,
        literals_compression_disabled,
    )
}

/// Encode a sequence plan as multiple split sub-blocks.
/// Each sub-block gets its own block header and entropy tables.
/// The plan data is read from `scratch.planned_sequences`.
fn encode_split_subblocks(
    out: &mut OutBuf<'_>,
    chunk: &[u8],
    split_points: &[usize],
    lit_offsets: &[usize],
    _total_lit_len: usize,
    initial_repeat_offsets: RepeatOffsets,
    literals_state: &mut LiteralsEncodingState,
    sequence_tables: &mut SequenceEncodingState,
    scratch: &mut EntropyEncodeScratch,
    parser_strategy: ParserStrategy,
    last_frame_block: bool,
) -> Result<Option<(RepeatOffsets, SequenceEncodingState, LiteralsEncodingState)>> {
    let num_partitions = split_points.len() - 1;
    let mut sub_plan = SequencePlan::default();
    let mut src_pos = 0usize;
    let mut compressor_reps = initial_repeat_offsets;
    let mut decoder_reps = initial_repeat_offsets;
    let mut prev_was_uncompressed = false;
    let mut any_compressed = false;

    for p in 0..num_partitions {
        let seq_start = split_points[p];
        let seq_end = split_points[p + 1];
        let is_last_partition = p == num_partitions - 1;
        let last_block = last_frame_block && is_last_partition;

        // Extract sub-block data from the main plan in scratch
        let plan = &scratch.planned_sequences;
        let total_seqs = plan.sequences.len();
        let lit_start = lit_offsets[seq_start];
        let lit_end = if seq_end == total_seqs {
            plan.literals.len()
        } else {
            lit_offsets[seq_end]
        };

        let mut sub_sequences: Vec<SequenceCommand> = plan.sequences[seq_start..seq_end].to_vec();
        let mut sub_offset_codes: Vec<u8> = plan.offset_codes[seq_start..seq_end].to_vec();
        let sub_ll_codes: Vec<u8> = plan.literal_codes[seq_start..seq_end].to_vec();
        let sub_ml_codes: Vec<u8> = plan.match_codes[seq_start..seq_end].to_vec();
        let sub_literals: Vec<u8> = plan.literals[lit_start..lit_end].to_vec();

        // Reconcile repcodes if previous sub-block was uncompressed
        if prev_was_uncompressed {
            reconcile_subblock_repcodes(
                &mut sub_sequences,
                &mut sub_offset_codes,
                &mut decoder_reps,
                &mut compressor_reps,
            );
        }

        // Compute source bytes for this sub-block
        let match_bytes: usize = sub_sequences.iter().map(|s| s.match_length as usize).sum();
        let src_bytes = sub_literals.len() + match_bytes;
        let sub_chunk = &chunk[src_pos..src_pos + src_bytes];
        src_pos += src_bytes;

        // Populate sub-plan.  After populating, resolve the actual final
        // repeat offsets by walking through all sequences — the initial
        // `repeat_offsets` set by `populate_from_subblock` is just the
        // starting state.  `encode_compressed_block_direct` returns
        // `sequence_plan.repeat_offsets`, so it must reflect the END state.
        sub_plan.populate_from_subblock(
            &sub_sequences,
            &sub_literals,
            &sub_ll_codes,
            &sub_offset_codes,
            &sub_ml_codes,
            decoder_reps,
        );
        {
            let mut reps = decoder_reps;
            for seq in &sub_sequences {
                reps.resolve_values(seq.literal_length, seq.offset_value)
                    .expect("resolve_values failed in sub-block repeat offset computation");
            }
            sub_plan.repeat_offsets = reps;
        }

        // Try to encode this sub-block directly into out
        let (
            huffman_dst,
            sequence_bitstream,
            sequence_workspace,
            _main_plan,
            _,
            _,
            _,
            _,
            huf_workspace,
        ) = scratch.split();

        if let Some((next_reps, next_seq_tables, next_lit_state)) = encode_compressed_block_direct(
            out,
            sub_chunk,
            &mut sub_plan,
            literals_state,
            sequence_tables,
            huffman_dst,
            sequence_bitstream,
            sequence_workspace,
            parser_strategy,
            huf_workspace,
            last_block,
        )? {
            decoder_reps = next_reps;
            compressor_reps = next_reps;
            *sequence_tables = next_seq_tables;
            *literals_state = next_lit_state;
            prev_was_uncompressed = false;
            any_compressed = true;
        } else {
            BlockHeader {
                last_block,
                block_type: BlockType::Raw,
                block_size: sub_chunk.len() as u32,
            }
            .write_to(out);
            out.extend_from_slice(sub_chunk);
            for seq in &sub_sequences {
                let ll0 = seq.literal_length == 0;
                advance_rep_state_encode(&mut compressor_reps, seq.offset_value, ll0);
            }
            prev_was_uncompressed = true;
        }
    }

    if any_compressed {
        Ok(Some((
            decoder_reps,
            sequence_tables.clone(),
            *literals_state,
        )))
    } else {
        Ok(None)
    }
}

/// Advance repeat offset state for a single sequence (encoder side).
fn advance_rep_state_encode(reps: &mut RepeatOffsets, offset_value: u32, ll0: bool) {
    let vals = reps.values();
    if offset_value >= 4 {
        *reps = RepeatOffsets::from_values([offset_value - 3, vals[0], vals[1]]);
    } else {
        let adj = if ll0 { offset_value } else { offset_value - 1 };
        match adj {
            0 => {}
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_block_into_contiguous(
    out: &mut OutBuf<'_>,
    frame: &[u8],
    block_start: usize,
    block_end: usize,
    match_state: &mut ContiguousBlockMatchState,
    literals_state: &mut LiteralsEncodingState,
    last_block: bool,
    repeat_offsets: &mut RepeatOffsets,
    sequence_tables: &mut SequenceEncodingState,
    scratch: &mut EntropyEncodeScratch,
    params: CompressionParameters,
    mut ldm: Option<&mut LdmFrameState>,
) -> Result<()> {
    let chunk = &frame[block_start..block_end];
    if chunk.is_empty() {
        BlockHeader {
            last_block,
            block_type: BlockType::Raw,
            block_size: 0,
        }
        .write_to(out);
        return Ok(());
    }

    if should_emit_raw_block_without_compression(chunk.len()) {
        if let Some(ldm) = ldm.as_deref_mut() {
            // Fed to the table, and the matches then dropped: an uncompressed
            // block still has to be reachable from a later one.
            ldm.sequences_for_block(&[], &frame[..block_end], block_start);
        }
        BlockHeader {
            last_block,
            block_type: BlockType::Raw,
            block_size: chunk.len() as u32,
        }
        .write_to(out);
        out.extend_from_slice(chunk);
        match_state.insert_range_for_uncompressed_block(frame, block_start, block_end);
        return Ok(());
    }

    if chunk.len() >= 2 && all_bytes_equal(chunk) {
        if let Some(ldm) = ldm.as_deref_mut() {
            // Fed to the table, and the matches then dropped: an uncompressed
            // block still has to be reachable from a later one.
            ldm.sequences_for_block(&[], &frame[..block_end], block_start);
        }
        BlockHeader {
            last_block,
            block_type: BlockType::Rle,
            block_size: chunk.len() as u32,
        }
        .write_to(out);
        out.push(chunk[0]);
        match_state.insert_rle_block(frame, block_start, block_end);
        return Ok(());
    }

    // Plan sequences — we need the plan to decide whether to split.
    {
        let (_, _, _, sequence_plan, _, _, _, _, _) = scratch.split();
        match ldm {
            Some(ldm) => plan_sequences_for_contiguous_block_with_ldm_into(
                sequence_plan,
                &frame[..block_end],
                block_start,
                *repeat_offsets,
                params.match_finder,
                params.max_history_bytes,
                match_state,
                ldm,
            )?,
            None => plan_sequences_for_contiguous_block_into(
                sequence_plan,
                &frame[..block_end],
                block_start,
                *repeat_offsets,
                params.match_finder,
                params.max_history_bytes,
                match_state,
            )?,
        }
    }

    // Check if post-sequence block splitting would help.
    let seq_count = scratch.planned_sequences.sequences.len();
    if should_try_post_sequence_split(&params, seq_count) {
        scratch.planned_sequences.ensure_codes_populated();
        let lit_offsets = build_literal_offset_table(&scratch.planned_sequences.sequences);
        let total_lit_len = scratch.planned_sequences.literals.len();
        let split_points = find_post_sequence_splits(
            &scratch.planned_sequences,
            &lit_offsets,
            total_lit_len,
            Some(sequence_tables),
            literals_state.huffman_table(),
            literals_state.compression_disabled,
        );
        if split_points.len() > 2 {
            // Multiple sub-blocks — encode via split path
            if let Some((next_reps, next_seq_tables, next_lit_state)) = encode_split_subblocks(
                out,
                chunk,
                &split_points,
                &lit_offsets,
                total_lit_len,
                *repeat_offsets,
                literals_state,
                sequence_tables,
                scratch,
                params.match_finder.parser_strategy,
                last_block,
            )? {
                *repeat_offsets = next_reps;
                *sequence_tables = next_seq_tables;
                *literals_state = next_lit_state;
                return Ok(());
            }
            // All sub-blocks were raw — fall through to single-block raw
            BlockHeader {
                last_block,
                block_type: BlockType::Raw,
                block_size: chunk.len() as u32,
            }
            .write_to(out);
            out.extend_from_slice(chunk);
            return Ok(());
        }
    }

    // Single-block path (no split, or not eligible)
    let (
        huffman_dst,
        sequence_bitstream,
        sequence_workspace,
        sequence_plan,
        _,
        _,
        _,
        _,
        huf_workspace,
    ) = scratch.split();
    if let Some((next_repeat_offsets, next_sequence_tables, next_literals_state)) =
        encode_compressed_block_direct(
            out,
            chunk,
            sequence_plan,
            literals_state,
            sequence_tables,
            huffman_dst,
            sequence_bitstream,
            sequence_workspace,
            params.match_finder.parser_strategy,
            huf_workspace,
            last_block,
        )?
    {
        *repeat_offsets = next_repeat_offsets;
        *sequence_tables = next_sequence_tables;
        *literals_state = next_literals_state;
        return Ok(());
    }

    BlockHeader {
        last_block,
        block_type: BlockType::Raw,
        block_size: chunk.len() as u32,
    }
    .write_to(out);
    out.extend_from_slice(chunk);
    Ok(())
}

pub(crate) fn encode_block_into_prefixed_contiguous(
    out: &mut OutBuf<'_>,
    prefix: &[u8],
    frame: &[u8],
    block_start: usize,
    block_end: usize,
    match_state: &mut PrefixedBlockMatchState,
    literals_state: &mut LiteralsEncodingState,
    last_block: bool,
    repeat_offsets: &mut RepeatOffsets,
    sequence_tables: &mut SequenceEncodingState,
    scratch: &mut EntropyEncodeScratch,
    params: CompressionParameters,
    mut ldm: Option<&mut LdmFrameState>,
) -> Result<()> {
    let chunk = &frame[block_start..block_end];
    if chunk.is_empty() {
        BlockHeader {
            last_block,
            block_type: BlockType::Raw,
            block_size: 0,
        }
        .write_to(out);
        return Ok(());
    }

    if should_emit_raw_block_without_compression(chunk.len()) {
        if let Some(ldm) = ldm.as_deref_mut() {
            // Fed to the table, and the matches then dropped: an uncompressed
            // block still has to be reachable from a later one.
            ldm.sequences_for_block(prefix, &frame[..block_end], block_start);
        }
        BlockHeader {
            last_block,
            block_type: BlockType::Raw,
            block_size: chunk.len() as u32,
        }
        .write_to(out);
        out.extend_from_slice(chunk);
        match_state.insert_range_for_uncompressed_block(frame, block_start, block_end);
        return Ok(());
    }

    if chunk.len() >= 2 && all_bytes_equal(chunk) {
        if let Some(ldm) = ldm.as_deref_mut() {
            // Fed to the table, and the matches then dropped: an uncompressed
            // block still has to be reachable from a later one.
            ldm.sequences_for_block(prefix, &frame[..block_end], block_start);
        }
        BlockHeader {
            last_block,
            block_type: BlockType::Rle,
            block_size: chunk.len() as u32,
        }
        .write_to(out);
        out.push(chunk[0]);
        match_state.insert_range(frame, block_start, block_end);
        return Ok(());
    }

    // Plan sequences — we need the plan to decide whether to split.
    {
        let (_, _, _, sequence_plan, _, _, _, _, _) = scratch.split();
        match ldm {
            Some(ldm) => plan_sequences_for_prefixed_contiguous_block_with_ldm_into(
                sequence_plan,
                prefix,
                &frame[..block_end],
                block_start,
                *repeat_offsets,
                params.match_finder,
                params.max_history_bytes,
                match_state,
                ldm,
            )?,
            None => plan_sequences_for_prefixed_contiguous_block_into(
                sequence_plan,
                prefix,
                &frame[..block_end],
                block_start,
                *repeat_offsets,
                params.match_finder,
                params.max_history_bytes,
                match_state,
            )?,
        }
    }

    // Check if post-sequence block splitting would help.
    let seq_count = scratch.planned_sequences.sequences.len();
    if should_try_post_sequence_split(&params, seq_count) {
        scratch.planned_sequences.ensure_codes_populated();
        let lit_offsets = build_literal_offset_table(&scratch.planned_sequences.sequences);
        let total_lit_len = scratch.planned_sequences.literals.len();
        let split_points = find_post_sequence_splits(
            &scratch.planned_sequences,
            &lit_offsets,
            total_lit_len,
            Some(sequence_tables),
            literals_state.huffman_table(),
            literals_state.compression_disabled,
        );
        if split_points.len() > 2 {
            // Multiple sub-blocks — encode via split path
            if let Some((next_reps, next_seq_tables, next_lit_state)) = encode_split_subblocks(
                out,
                chunk,
                &split_points,
                &lit_offsets,
                total_lit_len,
                *repeat_offsets,
                literals_state,
                sequence_tables,
                scratch,
                params.match_finder.parser_strategy,
                last_block,
            )? {
                *repeat_offsets = next_reps;
                *sequence_tables = next_seq_tables;
                *literals_state = next_lit_state;
                return Ok(());
            }
            // All sub-blocks were raw — fall through to single-block raw
            BlockHeader {
                last_block,
                block_type: BlockType::Raw,
                block_size: chunk.len() as u32,
            }
            .write_to(out);
            out.extend_from_slice(chunk);
            return Ok(());
        }
    }

    // Single-block path (no split, or not eligible)
    let (
        huffman_dst,
        sequence_bitstream,
        sequence_workspace,
        sequence_plan,
        _,
        _,
        _,
        _,
        huf_workspace,
    ) = scratch.split();
    if let Some((next_repeat_offsets, next_sequence_tables, next_literals_state)) =
        encode_compressed_block_direct(
            out,
            chunk,
            sequence_plan,
            literals_state,
            sequence_tables,
            huffman_dst,
            sequence_bitstream,
            sequence_workspace,
            params.match_finder.parser_strategy,
            huf_workspace,
            last_block,
        )?
    {
        *repeat_offsets = next_repeat_offsets;
        *sequence_tables = next_sequence_tables;
        *literals_state = next_literals_state;
        return Ok(());
    }

    BlockHeader {
        last_block,
        block_type: BlockType::Raw,
        block_size: chunk.len() as u32,
    }
    .write_to(out);
    out.extend_from_slice(chunk);
    Ok(())
}

fn profile_block_into_contiguous(
    frame: &[u8],
    block_start: usize,
    block_end: usize,
    match_state: &mut ContiguousBlockMatchState,
    literals_state: &mut LiteralsEncodingState,
    repeat_offsets: &mut RepeatOffsets,
    sequence_tables: &mut SequenceEncodingState,
    scratch: &mut EntropyEncodeScratch,
    params: CompressionParameters,
    phases: PlannerPhases,
    profiler: &mut EncodeStageProfiler,
) -> Result<usize> {
    let chunk = &frame[block_start..block_end];
    if chunk.is_empty() {
        profiler.record_raw_block();
        return Ok(BlockHeader::SIZE);
    }

    if should_emit_raw_block_without_compression(chunk.len()) {
        match_state.insert_range_for_uncompressed_block(frame, block_start, block_end);
        profiler.record_raw_block();
        return Ok(BlockHeader::SIZE + chunk.len());
    }

    if chunk.len() >= 2 && all_bytes_equal(chunk) {
        match_state.insert_rle_block(frame, block_start, block_end);
        profiler.record_rle_block();
        return Ok(BlockHeader::SIZE + 1);
    }

    let (
        huffman_dst,
        sequence_bitstream,
        sequence_workspace,
        sequence_plan,
        literals_section,
        literals_candidate,
        sequence_section,
        block_payload,
        huf_workspace,
    ) = scratch.split();
    sequence_plan.set_planning_profile(phases.is_on());
    let planning_start = Instant::now();
    // `&frame[..block_end]`, as `encode_block_into_contiguous` passes: the
    // parser runs to the end of the slice it is given, so handing it the whole
    // frame re-parses the entire tail once per block. Every shipped profiling
    // entry point truncates to the first block, where the two are the same
    // slice, which is why that never showed.
    plan_sequences_for_contiguous_block_into(
        sequence_plan,
        &frame[..block_end],
        block_start,
        *repeat_offsets,
        params.match_finder,
        params.max_history_bytes,
        match_state,
    )?;
    profiler.planning += planning_start.elapsed();
    profiler.record_planning_profile(sequence_plan);
    if let Some((payload_len, next_repeat_offsets, next_sequence_tables, next_literals_state)) =
        profile_planned_compressed_block(
            chunk,
            sequence_plan,
            literals_state,
            sequence_tables,
            huffman_dst,
            sequence_bitstream,
            sequence_workspace,
            literals_section,
            literals_candidate,
            sequence_section,
            block_payload,
            params.match_finder.parser_strategy,
            profiler,
            huf_workspace,
        )?
    {
        *repeat_offsets = next_repeat_offsets;
        *sequence_tables = next_sequence_tables;
        *literals_state = next_literals_state;
        profiler.record_compressed_block();
        return Ok(BlockHeader::SIZE + payload_len);
    }

    profiler.record_raw_block();
    Ok(BlockHeader::SIZE + chunk.len())
}

fn profile_block_into_prefixed_contiguous(
    prefix: &[u8],
    frame: &[u8],
    block_start: usize,
    block_end: usize,
    match_state: &mut PrefixedBlockMatchState,
    literals_state: &mut LiteralsEncodingState,
    repeat_offsets: &mut RepeatOffsets,
    sequence_tables: &mut SequenceEncodingState,
    scratch: &mut EntropyEncodeScratch,
    params: CompressionParameters,
    phases: PlannerPhases,
    profiler: &mut EncodeStageProfiler,
) -> Result<usize> {
    let chunk = &frame[block_start..block_end];
    if chunk.is_empty() {
        profiler.record_raw_block();
        return Ok(BlockHeader::SIZE);
    }

    if should_emit_raw_block_without_compression(chunk.len()) {
        match_state.insert_range_for_uncompressed_block(frame, block_start, block_end);
        profiler.record_raw_block();
        return Ok(BlockHeader::SIZE + chunk.len());
    }

    if chunk.len() >= 2 && all_bytes_equal(chunk) {
        match_state.insert_range(frame, block_start, block_end);
        profiler.record_rle_block();
        return Ok(BlockHeader::SIZE + 1);
    }

    let (
        huffman_dst,
        sequence_bitstream,
        sequence_workspace,
        sequence_plan,
        literals_section,
        literals_candidate,
        sequence_section,
        block_payload,
        huf_workspace,
    ) = scratch.split();
    sequence_plan.set_planning_profile(phases.is_on());
    let planning_start = Instant::now();
    // Truncated for the same reason as the contiguous path above.
    plan_sequences_for_prefixed_contiguous_block_into(
        sequence_plan,
        prefix,
        &frame[..block_end],
        block_start,
        *repeat_offsets,
        params.match_finder,
        params.max_history_bytes,
        match_state,
    )?;
    profiler.planning += planning_start.elapsed();
    profiler.record_planning_profile(sequence_plan);
    if let Some((payload_len, next_repeat_offsets, next_sequence_tables, next_literals_state)) =
        profile_planned_compressed_block(
            chunk,
            sequence_plan,
            literals_state,
            sequence_tables,
            huffman_dst,
            sequence_bitstream,
            sequence_workspace,
            literals_section,
            literals_candidate,
            sequence_section,
            block_payload,
            params.match_finder.parser_strategy,
            profiler,
            huf_workspace,
        )?
    {
        *repeat_offsets = next_repeat_offsets;
        *sequence_tables = next_sequence_tables;
        *literals_state = next_literals_state;
        profiler.record_compressed_block();
        return Ok(BlockHeader::SIZE + payload_len);
    }

    profiler.record_raw_block();
    Ok(BlockHeader::SIZE + chunk.len())
}

fn profile_planned_compressed_block(
    src: &[u8],
    sequence_plan: &mut SequencePlan,
    literals_state: &LiteralsEncodingState,
    sequence_tables: &SequenceEncodingState,
    huffman_dst: &mut Vec<u8>,
    sequence_bitstream: &mut Vec<u8>,
    sequence_workspace: &mut SequenceEncodeScratch,
    literals_section: &mut Vec<u8>,
    literals_candidate: &mut Vec<u8>,
    sequence_section: &mut Vec<u8>,
    block_payload: &mut Vec<u8>,
    parser_strategy: ParserStrategy,
    profiler: &mut EncodeStageProfiler,
    huf_workspace: &mut huff0::CompressWorkspace,
) -> Result<
    Option<(
        usize,
        RepeatOffsets,
        SequenceEncodingState,
        LiteralsEncodingState,
    )>,
> {
    let sequence_prepare_start = Instant::now();
    let summary = summarize_planned_block(sequence_plan, sequence_workspace)?;
    // Upstream's `ZSTD_seqToCodes`, which it runs inside
    // `ZSTD_buildSequencesStatistics` rather than before it. Counted into
    // `sequences` as a whole and reported separately as well.
    let codes = sequence_prepare_start.elapsed();
    profiler.sequences += codes;
    profiler.sequence_codes += codes;

    if summary.should_try_zero_sequence_block() {
        let literals_start = Instant::now();
        // `SrcSizeWrong` here is huff0 declining a literal run it cannot code,
        // not a failure: the block is emitted raw instead. `trace_first_block_inner`
        // resolves it the same way, and a profile that propagated it would abort
        // on any frame holding such a block rather than time it.
        let candidate = match encode_zero_sequence_compressed_block(
            src,
            literals_state,
            parser_strategy,
            huffman_dst,
            block_payload,
            literals_candidate,
            huf_workspace,
        ) {
            Ok(candidate) => candidate,
            Err(Error::SrcSizeWrong) => None,
            Err(error) => return Err(error),
        };
        profiler.literals += literals_start.elapsed();
        return Ok(candidate
            .filter(|(payload_len, _)| compression_wins(*payload_len, src.len(), parser_strategy))
            .map(|(payload_len, next_literals_state)| {
                (
                    payload_len,
                    sequence_plan.repeat_offsets,
                    sequence_tables.clone(),
                    next_literals_state,
                )
            }));
    }

    if summary.sequence_count == 0 {
        return Ok(None);
    }

    let literals_start = Instant::now();
    let (literals_len, next_literals_state) = match encode_literals_section_into(
        &sequence_plan.literals,
        literals_state,
        parser_strategy,
        literals_compressibility(sequence_plan.literals.len(), summary.sequence_count),
        huffman_dst,
        literals_section,
        literals_candidate,
        huf_workspace,
    ) {
        Ok(result) => result,
        Err(Error::SrcSizeWrong) => return Ok(None),
        Err(error) => return Err(error),
    };
    profiler.literals += literals_start.elapsed();

    let sequence_start = Instant::now();
    let mut sequence_timings = SequenceSectionTimings::default();
    let (next_sequence_tables, sequence_stats) =
        match encode_prepared_seq_store_section_with_strategy_and_scratch_into_stats(
            sequence_section,
            sequence_tables,
            parser_strategy,
            sequence_bitstream,
            sequence_plan,
            sequence_workspace,
            &mut sequence_timings,
        ) {
            Ok(result) => result,
            Err(Error::SrcSizeWrong) => return Ok(None),
            Err(error) => return Err(error),
        };
    profiler.sequence_statistics += sequence_timings.statistics;
    profiler.sequence_bitstream += sequence_timings.bitstream;
    profiler.sequence_assembly += sequence_timings.assembly;
    if sequence_section_triggers_legacy_decoder_bug(
        sequence_stats.last_count_size,
        sequence_stats.bitstream_size,
    ) {
        profiler.sequences += sequence_start.elapsed();
        return Ok(None);
    }
    block_payload.clear();
    block_payload.reserve(literals_len + sequence_section.len());
    block_payload.extend_from_slice(&literals_section[..literals_len]);
    block_payload.extend_from_slice(sequence_section);
    profiler.sequences += sequence_start.elapsed();

    Ok(
        compression_wins(block_payload.len(), src.len(), parser_strategy).then_some({
            (
                block_payload.len(),
                sequence_plan.repeat_offsets,
                next_sequence_tables,
                next_literals_state,
            )
        }),
    )
}

fn all_bytes_equal(src: &[u8]) -> bool {
    let first = src[0];
    src[1..].iter().all(|&byte| byte == first)
}

/// Writes a compressed block (header + payload) directly into `out`,
/// eliminating intermediate buffer copies.
fn encode_best_compressed_block_direct(
    out: &mut OutBuf<'_>,
    src: &[u8],
    prefixes: &[&[u8]],
    literals_state: &LiteralsEncodingState,
    repeat_offsets: RepeatOffsets,
    sequence_tables: &SequenceEncodingState,
    scratch: &mut EntropyEncodeScratch,
    params: CompressionParameters,
    last_block: bool,
) -> Result<Option<(RepeatOffsets, SequenceEncodingState, LiteralsEncodingState)>> {
    let (
        huffman_dst,
        sequence_bitstream,
        sequence_workspace,
        sequence_plan,
        _,
        _,
        _,
        _,
        huf_workspace,
    ) = scratch.split();
    plan_sequences_for_block_into(
        sequence_plan,
        src,
        prefixes,
        repeat_offsets,
        params.match_finder,
    )?;
    encode_compressed_block_direct(
        out,
        src,
        sequence_plan,
        literals_state,
        sequence_tables,
        huffman_dst,
        sequence_bitstream,
        sequence_workspace,
        params.match_finder.parser_strategy,
        huf_workspace,
        last_block,
    )
}

fn should_emit_raw_block_without_compression(raw_size: usize) -> bool {
    raw_size < MIN_COMPRESSIBLE_BLOCK_SIZE
}

fn summarize_planned_block(
    plan: &mut SequencePlan,
    sequence_scratch: &mut SequenceEncodeScratch,
) -> Result<PlannedBlockSummary> {
    if !plan.sequences.is_empty() {
        prepare_seq_store_encode_scratch(plan, sequence_scratch)?;
    }
    Ok(PlannedBlockSummary {
        sequence_count: plan.sequences.len(),
    })
}

fn compression_wins(
    compressed_size: usize,
    raw_size: usize,
    parser_strategy: ParserStrategy,
) -> bool {
    if compressed_size >= raw_size {
        return false;
    }
    raw_size - compressed_size >= minimum_compressed_block_gain(raw_size, parser_strategy)
}

fn minimum_compressed_block_gain(raw_size: usize, parser_strategy: ParserStrategy) -> usize {
    let minlog = if parser_strategy.zstd_rank() >= ParserStrategy::BinaryTreeUltra.zstd_rank() {
        parser_strategy.zstd_rank() - 1
    } else {
        6
    };
    (raw_size >> minlog) + 2
}

fn compressed_literals_clear_minimum_gain(
    raw_size: usize,
    compressed_size: usize,
    parser_strategy: ParserStrategy,
) -> bool {
    compressed_size
        < raw_size.saturating_sub(minimum_compressed_block_gain(raw_size, parser_strategy))
}

fn encode_zero_sequence_compressed_block(
    src: &[u8],
    literals_state: &LiteralsEncodingState,
    parser_strategy: ParserStrategy,
    huffman_dst: &mut Vec<u8>,
    block_payload: &mut Vec<u8>,
    literals_candidate: &mut Vec<u8>,
    huf_workspace: &mut huff0::CompressWorkspace,
) -> Result<Option<(usize, LiteralsEncodingState)>> {
    let Some(next_literals_state) = encode_compressed_literals_section_into(
        src,
        literals_state,
        parser_strategy,
        literals_compressibility(src.len(), 0),
        huffman_dst,
        block_payload,
        literals_candidate,
        huf_workspace,
    )?
    else {
        return Ok(None);
    };
    block_payload.push(0);
    if block_payload.len() >= src.len() {
        return Ok(None);
    }
    Ok(Some((block_payload.len(), next_literals_state)))
}

fn encode_zero_sequence_compressed_block_owned(
    src: &[u8],
    literals_state: &LiteralsEncodingState,
    parser_strategy: ParserStrategy,
    huffman_dst: &mut Vec<u8>,
    huf_workspace: &mut huff0::CompressWorkspace,
) -> Result<Option<(Vec<u8>, LiteralsEncodingState)>> {
    let mut payload = Vec::new();
    let mut literals_candidate = Vec::new();
    let Some(next_literals_state) = encode_compressed_literals_section_into(
        src,
        literals_state,
        parser_strategy,
        literals_compressibility(src.len(), 0),
        huffman_dst,
        &mut payload,
        &mut literals_candidate,
        huf_workspace,
    )?
    else {
        return Ok(None);
    };
    payload.push(0);
    if payload.len() >= src.len() {
        return Ok(None);
    }
    Ok(Some((payload, next_literals_state)))
}

fn sequence_section_triggers_legacy_decoder_bug(
    last_count_size: usize,
    bitstream_size: usize,
) -> bool {
    last_count_size != 0 && last_count_size + bitstream_size < 4
}

fn encode_literals_section_into(
    src: &[u8],
    literals_state: &LiteralsEncodingState,
    parser_strategy: ParserStrategy,
    compressibility: huff0::Compressibility,
    huffman_dst: &mut Vec<u8>,
    literals_section: &mut Vec<u8>,
    literals_candidate: &mut Vec<u8>,
    huf_workspace: &mut huff0::CompressWorkspace,
) -> Result<(usize, LiteralsEncodingState)> {
    let mut next_state = *literals_state;
    let raw_size = raw_or_rle_literals_header_len(src.len()) + src.len();

    // `ZSTD_compressLiterals` returns `ZSTD_noCompressLiterals` the moment
    // literal compression is disabled, ahead of both the Huffman attempt and
    // the RLE check. Raw is therefore the answer even for an all-one-byte
    // section that RLE would encode in a single byte.
    if literals_state.compression_disabled {
        encode_raw_literals_section_into(src, literals_section);
        return Ok((literals_section.len(), next_state));
    }

    // Try Huffman compression first (matching C's ZSTD_compressLiterals which
    // attempts compression before checking for RLE).
    if let Some(compressed_state) = encode_compressed_literals_section_into(
        src,
        literals_state,
        parser_strategy,
        compressibility,
        huffman_dst,
        literals_section,
        literals_candidate,
        huf_workspace,
    )? {
        if literals_section.len() < raw_size {
            next_state = compressed_state;
            return Ok((literals_section.len(), next_state));
        }
    }

    // Huffman didn't help — check RLE as fallback. C only checks
    // all_bytes_equal after Huff0 returns cLitSize==1, avoiding the full
    // scan on non-RLE blocks. Our Huff0 path already handles the
    // cLitSize==1 case, so this catch is for the rare scenario where
    // Huff0 was skipped (too small, incompressible heuristic) but
    // the data is still RLE.
    if !src.is_empty() && src.len() >= 8 && all_bytes_equal(src) {
        let rle_size = raw_or_rle_literals_header_len(src.len()) + 1;
        if rle_size < raw_size {
            encode_rle_literals_section_into(src, literals_section);
            return Ok((literals_section.len(), next_state));
        }
    }

    encode_raw_literals_section_into(src, literals_section);
    Ok((literals_section.len(), next_state))
}

fn encode_raw_literals_section_into(src: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(raw_or_rle_literals_header_len(src.len()) + src.len());
    append_raw_or_rle_literals_header(&mut OutBuf::growable(out), 0, src.len());
    out.extend_from_slice(src);
}

fn encode_rle_literals_section_into(src: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(raw_or_rle_literals_header_len(src.len()) + usize::from(!src.is_empty()));
    append_raw_or_rle_literals_header(&mut OutBuf::growable(out), 1, src.len());
    if let Some(&byte) = src.first() {
        out.push(byte);
    }
}

fn append_raw_or_rle_literals_header(
    dst: &mut OutBuf<'_>,
    block_type: u8,
    regenerated_size: usize,
) {
    let header_len = raw_or_rle_literals_header_len(regenerated_size);
    if header_len == 1 {
        dst.push(block_type | ((regenerated_size as u8) << 3));
    } else if header_len == 2 {
        let value = u32::from(block_type) | (1u32 << 2) | ((regenerated_size as u32) << 4);
        dst.push((value & 0xff) as u8);
        dst.push(((value >> 8) & 0xff) as u8);
    } else {
        let value = u32::from(block_type) | (3u32 << 2) | ((regenerated_size as u32) << 4);
        dst.push((value & 0xff) as u8);
        dst.push(((value >> 8) & 0xff) as u8);
        dst.push(((value >> 16) & 0xff) as u8);
    }
}

fn raw_or_rle_literals_header_len(regenerated_size: usize) -> usize {
    if regenerated_size <= 31 {
        1
    } else if regenerated_size <= 0x0fff {
        2
    } else {
        3
    }
}

/// How the literal compressor should pick the Huffman table depth at this
/// strategy.
///
/// The depth search is a real win where upstream runs it, but running it
/// everywhere is a fidelity loss rather than a free byte: it produces a
/// different table than upstream writes for the same literals, so the two
/// frames diverge inside the literals section even when the parse agrees
/// exactly. That is what kept the trained-dictionary parity tests failing at
/// levels 5 through 7 — the sequences already matched.
fn huffman_table_depth(parser_strategy: ParserStrategy) -> huff0::TableDepth {
    if parser_strategy.searches_huffman_table_depth() {
        huff0::TableDepth::Searched
    } else {
        huff0::TableDepth::Estimated
    }
}

/// Literal bytes per sequence above which C stops expecting the literals to
/// compress. `SUSPECT_UNCOMPRESSIBLE_LITERAL_RATIO` in `zstd_compress.c`.
const SUSPECT_UNCOMPRESSIBLE_LITERAL_RATIO: usize = 20;

/// What the parse implies about whether these literals will compress.
///
/// A block the match finder could find nothing in, or one where each sequence
/// covers a long run of literals, is the shape incompressible input takes. C
/// forms the same judgement in `ZSTD_entropyCompressSeqStore_internal` and uses
/// it only to choose between counting a sample and counting everything, never
/// to skip compression outright.
fn literals_compressibility(literals_len: usize, sequence_count: usize) -> huff0::Compressibility {
    if sequence_count == 0 || literals_len / sequence_count >= SUSPECT_UNCOMPRESSIBLE_LITERAL_RATIO
    {
        huff0::Compressibility::Suspect
    } else {
        huff0::Compressibility::Unknown
    }
}

fn encode_compressed_literals_section_into(
    src: &[u8],
    literals_state: &LiteralsEncodingState,
    parser_strategy: ParserStrategy,
    compressibility: huff0::Compressibility,
    huffman_dst: &mut Vec<u8>,
    literals_section: &mut Vec<u8>,
    _literals_candidate: &mut Vec<u8>,
    huf_workspace: &mut huff0::CompressWorkspace,
) -> Result<Option<LiteralsEncodingState>> {
    // Upstream's `disableLiteralCompression` short-circuit, which sits ahead of
    // every other reason `ZSTD_compressLiterals` declines to Huffman-code.
    if literals_state.compression_disabled {
        return Ok(None);
    }
    let layout = compressed_literals_layout(src.len(), literals_state.repeat_mode());
    if src.len() < minimum_literals_to_compress(parser_strategy, literals_state.repeat_mode()) {
        return Ok(None);
    }

    let compressed_bound = huff0::compress_bound(src.len());
    if huffman_dst.len() < compressed_bound {
        huffman_dst.resize(compressed_bound, 0);
    }
    let Some(choice) = huff0::compress_prefer_existing_table_into_mode(
        huffman_dst,
        src,
        literals_state.huffman_table(),
        layout.stream_mode(),
        huffman_table_depth(parser_strategy),
        compressibility,
        huf_workspace,
    )?
    else {
        return Ok(None);
    };
    let compressed_literals = &huffman_dst[..choice.written];
    if !compressed_literals_clear_minimum_gain(
        src.len(),
        compressed_literals.len(),
        parser_strategy,
    ) {
        return Ok(None);
    }
    if compressed_literals.len() == 1 && (src.len() >= 8 || all_bytes_equal(src)) {
        return Ok(None);
    }
    if compressed_literals.len() > layout.max_compressed_size() {
        return Ok(None);
    }

    let block_type = if choice.reused_table { 3 } else { 2 };
    literals_section.clear();
    literals_section.reserve(layout.header_size + compressed_literals.len());
    append_compressed_literals_header(
        &mut OutBuf::growable(literals_section),
        block_type,
        layout.size_format,
        src.len(),
        compressed_literals.len(),
    );
    literals_section.extend_from_slice(compressed_literals);
    Ok(Some(LiteralsEncodingState::with_table_and_repeat_mode(
        Some(&choice.table),
        if choice.reused_table {
            // `set_repeat` preserves the prior HUF repeat state; it does not upgrade
            // `check` tables to `valid`.
            literals_state.repeat_mode()
        } else {
            LiteralsRepeatMode::Check
        },
        literals_state.compression_disabled,
    )))
}

#[derive(Clone, Copy)]
struct CompressedLiteralsLayout {
    header_size: usize,
    size_format: u8,
    single_stream: bool,
}

impl CompressedLiteralsLayout {
    fn max_compressed_size(self) -> usize {
        match self.size_format {
            0 | 1 => 0x03ff,
            2 => 0x3fff,
            3 => 0x3ffff,
            _ => unreachable!(),
        }
    }

    fn stream_mode(self) -> huff0::StreamMode {
        if self.single_stream {
            huff0::StreamMode::Single
        } else {
            huff0::StreamMode::Four
        }
    }
}

fn compressed_literals_layout(
    regenerated_size: usize,
    repeat_mode: LiteralsRepeatMode,
) -> CompressedLiteralsLayout {
    let header_size =
        3 + usize::from(regenerated_size >= 1024) + usize::from(regenerated_size >= 16 * 1024);
    let single_stream =
        regenerated_size < 256 || (repeat_mode == LiteralsRepeatMode::Valid && header_size == 3);
    let size_format = match header_size {
        3 => u8::from(!single_stream),
        4 => 2,
        5 => 3,
        _ => unreachable!(),
    };
    CompressedLiteralsLayout {
        header_size,
        size_format,
        single_stream,
    }
}

fn minimum_literals_to_compress(
    parser_strategy: ParserStrategy,
    repeat_mode: LiteralsRepeatMode,
) -> usize {
    if repeat_mode == LiteralsRepeatMode::Valid {
        return 6;
    }

    let shift = (9 - parser_strategy.zstd_rank()).min(3);
    8usize << shift
}

fn append_compressed_literals_header(
    out: &mut OutBuf<'_>,
    block_type: u8,
    size_format: u8,
    regenerated_size: usize,
    compressed_size: usize,
) {
    let low_bits = u64::from(block_type) | (u64::from(size_format) << 2);
    let (header_size, value) = match size_format {
        0 | 1 => (
            3,
            low_bits | ((regenerated_size as u64) << 4) | ((compressed_size as u64) << 14),
        ),
        2 => (
            4,
            low_bits | ((regenerated_size as u64) << 4) | ((compressed_size as u64) << 18),
        ),
        3 => (
            5,
            low_bits | ((regenerated_size as u64) << 4) | ((compressed_size as u64) << 22),
        ),
        _ => unreachable!(),
    };
    let start = out.len();
    out.resize(start + header_size, 0);
    out.write_at(start, &value.to_le_bytes()[..header_size]);
}

// ── Direct-to-output encoding ───────────────────────────────────────────────
// These functions append encoded data directly to the output Vec, eliminating
// intermediate buffer copies (literals_section, sequence_section, block_payload).

/// Append compressed literals (header + Huffman data) directly to `out`.
/// Returns `Some(next_state)` on success, `None` if compression wasn't beneficial.
fn encode_compressed_literals_direct(
    src: &[u8],
    literals_state: &LiteralsEncodingState,
    parser_strategy: ParserStrategy,
    compressibility: huff0::Compressibility,
    huffman_dst: &mut Vec<u8>,
    out: &mut OutBuf<'_>,
    huf_workspace: &mut huff0::CompressWorkspace,
) -> Result<Option<LiteralsEncodingState>> {
    // Upstream's `disableLiteralCompression` short-circuit, which sits ahead of
    // every other reason `ZSTD_compressLiterals` declines to Huffman-code.
    if literals_state.compression_disabled {
        return Ok(None);
    }
    let layout = compressed_literals_layout(src.len(), literals_state.repeat_mode());
    if src.len() < minimum_literals_to_compress(parser_strategy, literals_state.repeat_mode()) {
        return Ok(None);
    }

    let compressed_bound = huff0::compress_bound(src.len());
    if huffman_dst.len() < compressed_bound {
        huffman_dst.resize(compressed_bound, 0);
    }
    let Some(choice) = huff0::compress_prefer_existing_table_into_mode(
        huffman_dst,
        src,
        literals_state.huffman_table(),
        layout.stream_mode(),
        huffman_table_depth(parser_strategy),
        compressibility,
        huf_workspace,
    )?
    else {
        return Ok(None);
    };
    let compressed_literals = &huffman_dst[..choice.written];
    if !compressed_literals_clear_minimum_gain(
        src.len(),
        compressed_literals.len(),
        parser_strategy,
    ) {
        return Ok(None);
    }
    if compressed_literals.len() == 1 && (src.len() >= 8 || all_bytes_equal(src)) {
        return Ok(None);
    }
    if compressed_literals.len() > layout.max_compressed_size() {
        return Ok(None);
    }

    let block_type = if choice.reused_table { 3 } else { 2 };
    out.reserve(layout.header_size + compressed_literals.len());
    append_compressed_literals_header(
        out,
        block_type,
        layout.size_format,
        src.len(),
        compressed_literals.len(),
    );
    out.extend_from_slice(compressed_literals);
    Ok(Some(LiteralsEncodingState::with_table_and_repeat_mode(
        Some(&choice.table),
        if choice.reused_table {
            literals_state.repeat_mode()
        } else {
            LiteralsRepeatMode::Check
        },
        literals_state.compression_disabled,
    )))
}

/// Append the literals section (compressed, RLE, or raw) directly to `out`.
/// Returns `(bytes_written, next_state)`.
fn encode_literals_section_direct(
    src: &[u8],
    literals_state: &LiteralsEncodingState,
    parser_strategy: ParserStrategy,
    compressibility: huff0::Compressibility,
    huffman_dst: &mut Vec<u8>,
    out: &mut OutBuf<'_>,
    huf_workspace: &mut huff0::CompressWorkspace,
) -> Result<(usize, LiteralsEncodingState)> {
    let mut next_state = *literals_state;
    let raw_size = raw_or_rle_literals_header_len(src.len()) + src.len();
    let start = out.len();

    // See `encode_literals_section_into`: disabled means raw, ahead of the RLE
    // check as well as the Huffman attempt.
    if literals_state.compression_disabled {
        let literals_start = out.len();
        append_raw_or_rle_literals_header(out, 0, src.len());
        out.extend_from_slice(src);
        return Ok((out.len() - literals_start, next_state));
    }

    // Try Huffman compression first (matching C's ZSTD_compressLiterals).
    if let Some(compressed_state) = encode_compressed_literals_direct(
        src,
        literals_state,
        parser_strategy,
        compressibility,
        huffman_dst,
        out,
        huf_workspace,
    )? {
        if out.len() - start < raw_size {
            next_state = compressed_state;
            return Ok((out.len() - start, next_state));
        }
        // Compression didn't beat raw — undo what was appended
        out.truncate(start);
    }

    // RLE check after Huff0 (matching C).
    if !src.is_empty() && src.len() >= 8 && all_bytes_equal(src) {
        let rle_size = raw_or_rle_literals_header_len(src.len()) + 1;
        if rle_size < raw_size {
            append_raw_or_rle_literals_header(out, 1, src.len());
            out.push(src[0]);
            return Ok((out.len() - start, next_state));
        }
    }

    // Raw fallback
    out.reserve(raw_or_rle_literals_header_len(src.len()) + src.len());
    append_raw_or_rle_literals_header(out, 0, src.len());
    out.extend_from_slice(src);
    Ok((out.len() - start, next_state))
}

/// Fold one training sample's entropy statistics into `stats`, for
/// [`crate::dict_builder`].
///
/// C's `ZDICT_countEStats`. The sample's first block is compressed against the
/// candidate content as a raw-content dictionary; if it compressed at all, its
/// literals and its three sequence code streams are counted.
///
/// A block that did not compress contributes nothing. C skips it because
/// `ZSTD_compressBlock` returns zero there, and the distinction matters: an
/// incompressible sample has no sequences worth modelling, and counting its
/// literals would pull the tables toward a distribution no sample encodes with.
///
/// `params` is computed once by the caller from the *average* sample size, not
/// per sample, because that is what C derives its block size bound and its
/// parser configuration from.
pub(crate) fn count_dictionary_entropy_stats(
    sample: &[u8],
    dictionary: &EncoderDictionary<'_>,
    params: CompressionParameters,
    scratch: &mut EntropyEncodeScratch,
    block_buffer: &mut Vec<u8>,
    stats: &mut crate::dict_builder::EntropyStats,
) -> Result<()> {
    let block_size_max = BLOCK_SIZE_MAX.min(1usize << params.upstream_cparams.window_log);
    let sample = &sample[..sample.len().min(block_size_max)];
    if sample.is_empty() {
        return Ok(());
    }

    let inner = dictionary.as_inner();
    let content = inner.matching_content();
    let literals_state = LiteralsEncodingState::new(Some(inner), params);
    let sequence_tables = SequenceEncodingState::default();
    let parser_strategy = params.match_finder.parser_strategy;
    let prepared_match_state = dictionary.prepared_match_state(params.match_finder);

    let (
        huffman_dst,
        sequence_bitstream,
        sequence_workspace,
        sequence_plan,
        _literals_section,
        _literals_candidate,
        _sequence_section,
        _block_payload,
        huf_workspace,
    ) = scratch.split();

    let mut match_state = PrefixedBlockMatchState::new_with_prepared_match_state(
        content,
        sample.len(),
        params.match_finder,
        prefix_match_mode_for_dictionary(
            inner,
            Some(sample.len()),
            params.upstream_cparams.strategy,
        ),
        prepared_match_state.as_deref(),
    );
    sequence_plan.opt_dict_price_seed = inner.optimal_price_seed();
    plan_sequences_for_prefixed_contiguous_block_into(
        sequence_plan,
        content,
        sample,
        0,
        inner.repeat_offsets(),
        params.match_finder,
        params.max_history_bytes,
        &mut match_state,
    )?;

    block_buffer.clear();
    let compressed = {
        let mut out = OutBuf::growable(block_buffer);
        encode_compressed_block_direct(
            &mut out,
            sample,
            sequence_plan,
            &literals_state,
            &sequence_tables,
            huffman_dst,
            sequence_bitstream,
            sequence_workspace,
            parser_strategy,
            huf_workspace,
            true,
        )?
    };
    if compressed.is_none() {
        return Ok(());
    }

    for &literal in &sequence_plan.literals {
        stats.literals[literal as usize] += 1;
    }
    sequence_plan.ensure_codes_populated();
    for &code in &sequence_plan.offset_codes {
        if let Some(slot) = stats.offset_codes.get_mut(code as usize) {
            *slot += 1;
        }
    }
    for &code in &sequence_plan.match_codes {
        stats.match_lengths[code as usize] += 1;
    }
    for &code in &sequence_plan.literal_codes {
        stats.literal_lengths[code as usize] += 1;
    }
    Ok(())
}

/// Encode a compressed block directly into `out` (header + payload).
/// Returns `Some(...)` if compression was worthwhile, `None` if the caller
/// should emit a raw block instead.  On `None`, `out` is unchanged.
fn encode_compressed_block_direct(
    out: &mut OutBuf<'_>,
    src: &[u8],
    sequence_plan: &mut SequencePlan,
    literals_state: &LiteralsEncodingState,
    sequence_tables: &SequenceEncodingState,
    huffman_dst: &mut Vec<u8>,
    sequence_bitstream: &mut Vec<u8>,
    sequence_workspace: &mut SequenceEncodeScratch,
    parser_strategy: ParserStrategy,
    huf_workspace: &mut huff0::CompressWorkspace,
    last_block: bool,
) -> Result<Option<(RepeatOffsets, SequenceEncodingState, LiteralsEncodingState)>> {
    let summary = summarize_planned_block(sequence_plan, sequence_workspace)?;

    if summary.should_try_zero_sequence_block() {
        // Zero-sequence path: literals + 0x00 sequence count
        let header_pos = out.len();
        out.extend_from_slice(&[0, 0, 0]); // placeholder block header
        let payload_start = out.len();

        if let Some(next_literals_state) = encode_compressed_literals_direct(
            src,
            literals_state,
            parser_strategy,
            literals_compressibility(src.len(), 0),
            huffman_dst,
            out,
            huf_workspace,
        )? {
            out.push(0); // zero sequence count
            let payload_len = out.len() - payload_start;
            if compression_wins(payload_len, src.len(), parser_strategy) {
                BlockHeader {
                    last_block,
                    block_type: BlockType::Compressed,
                    block_size: payload_len as u32,
                }
                .write_at(out, header_pos);
                return Ok(Some((
                    sequence_plan.repeat_offsets,
                    sequence_tables.clone(),
                    next_literals_state,
                )));
            }
        }
        // Compression didn't win — undo
        out.truncate(header_pos);
        return Ok(None);
    }

    if summary.sequence_count == 0 {
        return Ok(None);
    }

    // Sequence path: literals section + sequence section
    let header_pos = out.len();
    out.extend_from_slice(&[0, 0, 0]); // placeholder block header
    let payload_start = out.len();

    let (_literals_len, next_literals_state) = encode_literals_section_direct(
        &sequence_plan.literals,
        literals_state,
        parser_strategy,
        literals_compressibility(sequence_plan.literals.len(), summary.sequence_count),
        huffman_dst,
        out,
        huf_workspace,
    )?;

    let (next_sequence_tables, sequence_stats) = encode_sequence_section_direct(
        out,
        sequence_tables,
        parser_strategy,
        sequence_bitstream,
        sequence_plan,
        sequence_workspace,
    )?;

    // Check legacy decoder bug
    if sequence_section_triggers_legacy_decoder_bug(
        sequence_stats.last_count_size,
        sequence_stats.bitstream_size,
    ) {
        out.truncate(header_pos);
        return Ok(None);
    }

    let payload_len = out.len() - payload_start;
    if !compression_wins(payload_len, src.len(), parser_strategy) {
        out.truncate(header_pos);
        return Ok(None);
    }

    // Backfill block header
    BlockHeader {
        last_block,
        block_type: BlockType::Compressed,
        block_size: payload_len as u32,
    }
    .write_at(out, header_pos);

    Ok(Some((
        sequence_plan.repeat_offsets,
        next_sequence_tables,
        next_literals_state,
    )))
}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Scratch contents are an implementation detail and change every
        // block; expose only that the handle exists and holds reusable state.
        f.debug_struct("Encoder").finish_non_exhaustive()
    }
}
