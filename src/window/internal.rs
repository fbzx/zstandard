use super::*;

pub(crate) const MIN_MATCH: usize = 4;
pub(crate) const MAX_MATCH_HASH_BITS: u32 = 25;
/// Widest hash the tagged (short-cache) tables can take.
///
/// Those lookups compute `hash_bits + SHORT_CACHE_TAG_BITS` bits out of a
/// 32-bit hash and split them into a table index and an 8-bit tag, so anything
/// above this leaves no room for the tag: the shift underflows, which panics
/// in debug and silently produces an out-of-range index in release.
///
/// Upstream's no-dictionary fast parser has no such bound — it hashes with
/// `hlog` alone (`zstd_fast.c:261`) and carries the tag only in the CDict
/// tables, whose `hashLog` its `ZSTD_CDictIndicesAreTagged` clamp already
/// holds at 24. Nothing a compression level selects comes close: the widest
/// `hash_log` on a `Fast` or `DoubleFast` row is 17. This binds only for a
/// caller who overrides `hash_log` or `chain_log` above 24 and lands on one of
/// those two parsers, and there it costs table size, not correctness. The
/// bound is kept for the 32-bit hash; entry positions are independent of it.
pub(crate) const MAX_TAGGED_MATCH_HASH_BITS: u32 = 32 - SHORT_CACHE_TAG_BITS;

/// The hash width a finder will actually build for a requested `hash_log`.
pub(crate) const fn match_hash_bits(requested: u32) -> u32 {
    if requested < 10 {
        10
    } else if requested > MAX_MATCH_HASH_BITS {
        MAX_MATCH_HASH_BITS
    } else {
        requested
    }
}

/// [`match_hash_bits`] bounded by the 32-bit hash's room for a short-cache tag.
///
/// Kept as a function rather than inlined at each site so the construction of
/// a finder and the test for whether a cached one can be reused cannot drift
/// apart: they disagreed once, and the reuse test silently stopped matching.
pub(crate) const fn tagged_match_hash_bits(requested: u32) -> u32 {
    let bits = match_hash_bits(requested);
    if bits > MAX_TAGGED_MATCH_HASH_BITS {
        MAX_TAGGED_MATCH_HASH_BITS
    } else {
        bits
    }
}
/// The lowest source index a match may start at, as seen from a position.
///
/// C's lazy, greedy, binary-tree and optimal parsers do not share one floor
/// across a block. They call `ZSTD_getLowestMatchIndex(ms, curr, windowLog)`
/// at the position doing the looking (`zstd_lazy.c:257`, `zstd_opt.c:619`), so
/// a match anywhere in the block may reach a full window back from *itself*.
/// Only the fast and double-fast parsers take a single floor for the whole
/// block, and theirs is measured from the block's end — `zstd_fast.c:204` and
/// `zstd_double_fast.c:119` for the prefix, `zstd_fast.c:723` and
/// `zstd_double_fast.c:627` for their ext-dict variants — which is why those
/// two keep a plain `usize`.
///
/// Both of those files *also* call a helper at `curr`, at `zstd_fast.c:240`
/// and `zstd_double_fast.c:160`. Those are not match floors: they are the
/// one-time `maxRep` clamp on the repeat offsets carried into the block, which
/// this crate models separately as `rep_window_low`.
///
/// `base` is C's `window.lowLimit`, which `ZSTD_window_enforceMaxDist` sets
/// from the block's *start* (`zstd_compress.c:4630` passes `ip`) and which no
/// position may reach below. `reach` is `1 << windowLog`, how far back a single
/// position may look. When a dictionary is loaded C drops the per-position term
/// and returns the block-constant limit alone — the `isDictionary` branch of
/// both helpers, on the grounds that a dictionary is either wholly valid or
/// already invalidated — and that is [`MatchFloor::fixed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MatchFloor {
    base: usize,
    reach: usize,
}

impl MatchFloor {
    /// One floor for every position in the block.
    pub(crate) const fn fixed(base: usize) -> Self {
        Self {
            base,
            reach: usize::MAX,
        }
    }

    /// C's per-position floor: never below `base`, and never further than
    /// `reach` behind the position doing the looking.
    pub(crate) const fn reaching(base: usize, reach: usize) -> Self {
        Self { base, reach }
    }

    /// The floor as seen from `pos`.
    #[inline(always)]
    pub(crate) const fn at(self, pos: usize) -> usize {
        let reachable = pos.saturating_sub(self.reach);
        if reachable > self.base {
            reachable
        } else {
            self.base
        }
    }
}

/// [`MatchFloor`] for a block that sits behind a prefix.
///
/// The prefixed parsers take two floors, one into the prefix and one into the
/// source, because they address those as two buffers.
///
/// C has exactly the two shapes below and picks between them per block. While
/// any of the dictionary is still inside the window both helpers drop their
/// per-position term and return their block-constant limit unchanged — their
/// `isDictionary` branch, on the grounds that a dictionary is either wholly
/// valid or already invalidated — so every position in the block shares one
/// floor. Once `ZSTD_checkDictValidity` has retired it (`zstd_compress.c:4629`,
/// called with the block's *end*), the floor is `curr - maxDist` at the
/// position doing the looking.
///
/// The two limits are not the same field: `ZSTD_getLowestMatchIndex` returns
/// `window.lowLimit` and `ZSTD_getLowestPrefixIndex` returns
/// `window.dictLimit`. They coincide only in `dictMatchState` mode, which is
/// the mode this crate builds; in C's normal dict mode the dictionary lies
/// between the two (`zstd_compress_internal.h:1250-1252`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrefixedMatchFloor {
    /// One pair for the whole block.
    Fixed {
        prefix_low: usize,
        source_low: usize,
    },
    /// Resolved per position, in the virtual coordinate space where the prefix
    /// runs from `0` to `prefix_len` and source position `p` sits at
    /// `prefix_len + p`.
    Reaching {
        prefix_len: usize,
        floor: MatchFloor,
    },
}

impl PrefixedMatchFloor {
    pub(crate) const fn fixed(prefix_low: usize, source_low: usize) -> Self {
        Self::Fixed {
            prefix_low,
            source_low,
        }
    }

    pub(crate) const fn reaching(prefix_len: usize, floor: MatchFloor) -> Self {
        Self::Reaching { prefix_len, floor }
    }

    /// The `(prefix_low, source_low)` pair as seen from source position `pos`.
    #[inline(always)]
    pub(crate) const fn at(self, pos: usize) -> (usize, usize) {
        match self {
            Self::Fixed {
                prefix_low,
                source_low,
            } => (prefix_low, source_low),
            Self::Reaching { prefix_len, floor } => {
                let virtual_floor = floor.at(prefix_len + pos);
                if virtual_floor >= prefix_len {
                    (prefix_len, virtual_floor - prefix_len)
                } else {
                    (virtual_floor, 0)
                }
            }
        }
    }
}

pub(crate) const NO_POS: u32 = u32::MAX;
/// How far a lazily-filled match finder may fall behind the start of a block
/// before the catch-up is abandoned rather than walked. C's threshold in the
/// "limited update after a very long match" clause of `ZSTD_buildSeqStore`.
pub(crate) const LIMITED_UPDATE_LAG: usize = 384;
/// The most positions the catch-up bridges once the lag passes
/// `LIMITED_UPDATE_LAG`. C's `MIN(192, ...)` in the same clause.
pub(crate) const LIMITED_UPDATE_SPAN: usize = 192;
/// The same pair again, for the clamp long-distance matching applies before
/// each segment of a block. C spells these `1024` and `MIN(512, ...)` in
/// `ZSTD_ldm_limitTableUpdate`, and applies them on top of the block-level
/// clamp above rather than instead of it.
pub(crate) const LDM_LIMITED_UPDATE_LAG: usize = 1024;
pub(crate) const LDM_LIMITED_UPDATE_SPAN: usize = 512;
/// The stride C fills the fast tables at, and the tail it stops short of, in
/// `ZSTD_fillHashTableForCCtx`. The tail is `HASH_READ_SIZE`: the fill hashes
/// eight bytes at each position, so it cannot start one within eight of the
/// end.
pub(crate) const FAST_FILL_STEP: usize = 3;
pub(crate) const FAST_FILL_HASH_READ_SIZE: usize = 8;
pub(crate) const ROW_HASH_TAG_BITS: u32 = 8;
pub(crate) const SHORT_CACHE_TAG_BITS: u32 = 8;
/// An empty slot in a tagged table: [`NO_POS`] widened to the entry type.
pub(crate) const NO_TAGGED_ENTRY: u64 = NO_POS as u64;
pub(crate) const SHORT_CACHE_TAG_MASK: u32 = (1 << SHORT_CACHE_TAG_BITS) - 1;
pub(crate) const SHORT_CACHE_TAG_MASK_USIZE: usize = SHORT_CACHE_TAG_MASK as usize;
pub(crate) const ROW_HASH_CACHE_SIZE: usize = 8;
pub(crate) const ROW_HASH_MAX_ENTRIES: usize = 64;
pub(crate) const ROW_LAZY_TRACE_MAX_STEPS: usize = 8;
pub(crate) const HASH_READ_SIZE: usize = 8;
pub(crate) const OPT_NUM: usize = 4096;
pub(crate) const OPT_PRICE_ACCURACY_LOG: u32 = 8;
pub(crate) const OPT_PRICE_UNIT: u32 = 1 << OPT_PRICE_ACCURACY_LOG;
pub(crate) const OPT_LITERAL_FREQ_ADD: u32 = 2;
pub(crate) const OPT_PREDEFINED_THRESHOLD: usize = 8;
pub(crate) const LAZY_SKIPPING_STEP: usize = 8;
/// The row-hash salt every frame is encoded under: `ZSTD_bitmix(0, 8) ^
/// ZSTD_bitmix(0, 4)`, what C's `ZSTD_advanceHashSalt()` yields on a zeroed
/// context, so this crate salts every frame as C salts its first.
///
/// C advances from here on each reset; this does not, and
/// [`super::lazy::RowHashFinder::reset`] says why.
pub(crate) const DEFAULT_ROW_HASH_SALT: u64 = 0x8358_92B4_BB2C_AE74;

/// One past the last position in a `len`-byte buffer whose `MIN_MATCH`-byte
/// hash key still lies inside it.
///
/// Zero when the buffer is shorter than the key. That is the case the obvious
/// `len.saturating_sub(MIN_MATCH) + 1` gets wrong: the subtraction floors at
/// zero and the `+ 1` then admits position 0, whose key runs off the end of the
/// buffer. Reachable from `encode_all_with_dict` with a body of one to three
/// bytes, because a block that short is emitted raw and the encoder still
/// indexes its positions for the blocks that follow.
///
/// `row_insert_end` is the same shape for the row finder's wider key.
#[inline]
pub(crate) fn hash_insert_end(len: usize) -> usize {
    (len + 1).saturating_sub(MIN_MATCH)
}

/// Pre-computed frequency arrays derived from a trained dictionary's
/// Huffman and FSE encoding tables.  Matches C's `ZSTD_rescaleFreqs`
/// dictionary initialization path (zstd_opt.c lines 158-210).
#[derive(Debug, Clone)]
pub(crate) struct DictionaryPriceSeed {
    pub(crate) lit_freq: [u32; 256],
    pub(crate) lit_sum: u32,
    pub(crate) ll_freq: [u32; 36],
    pub(crate) ll_sum: u32,
    pub(crate) ml_freq: [u32; 53],
    pub(crate) ml_sum: u32,
    pub(crate) of_freq: [u32; 32],
    pub(crate) of_sum: u32,
}

/// Accumulated pricing state persisted across blocks within a frame.
/// Matches C's `optState_t` cross-block frequency persistence.
#[derive(Debug, Clone)]
pub(crate) struct OptimalPriceState {
    pub(crate) lit_freq: [u32; 256],
    pub(crate) lit_sum: u32,
    pub(crate) ll_freq: [u32; 36],
    pub(crate) ll_sum: u32,
    pub(crate) ml_freq: [u32; 53],
    pub(crate) ml_sum: u32,
    pub(crate) of_freq: [u32; 32],
    pub(crate) of_sum: u32,
}

impl OptimalPriceState {
    /// Rescale accumulated frequencies for a new block, matching C's
    /// `ZSTD_scaleStats` (zstd_opt.c).  Target sums: literals ~4096,
    /// LL/ML/OF ~2048.
    pub(crate) fn rescale(&mut self) {
        self.lit_sum = Self::scale_stats(&mut self.lit_freq, 12);
        self.ll_sum = Self::scale_stats(&mut self.ll_freq, 11);
        self.ml_sum = Self::scale_stats(&mut self.ml_freq, 11);
        self.of_sum = Self::scale_stats(&mut self.of_freq, 11);
    }

    /// Matches C's `ZSTD_scaleStats` (zstd_opt.c:123-131): compute sum,
    /// derive shift from `highbit32(sum >> logTarget)`, then single-pass
    /// downscale with base-1 guarantee.
    fn scale_stats(freq: &mut [u32], log_target: u32) -> u32 {
        let prev_sum: u32 = freq.iter().sum();
        let factor = prev_sum >> log_target;
        // C: `if (factor <= 1) return prevsum;` (ZSTD_scaleStats, zstd_opt.c).
        if factor <= 1 {
            return prev_sum;
        }
        let shift = highbit32(factor);
        let mut sum = 0u32;
        for f in freq.iter_mut() {
            *f = 1 + (*f >> shift);
            sum += *f;
        }
        sum
    }
}

/// HC3: Direct-mapped hash table for 3-byte matches, matching C's hashTable3
/// (zstd_opt.c). Used by the optimal parser when min_match == 3 to find
/// short matches that the binary tree (which uses mls-byte keys) would miss.
/// C only allocates this for the CCtx (source), not for the CDict (dictionary).
#[derive(Debug, Clone)]
pub(crate) struct Hash3Table {
    pub(crate) heads: Vec<u32>,
    pub(crate) hash_log: u32,
    pub(crate) next_to_update: usize,
}

/// Maximum HC3 hash log, matching C's ZSTD_HASHLOG3_MAX = 17.
const HASH3_LOG_MAX: u32 = 17;

/// Prime multiplier for 3-byte hashing, matching C's prime3bytes.
const HASH3_PRIME: u32 = 506_832_829;

/// Maximum distance for HC3 matches (256 KB), matching C's `(1<<18)` heuristic.
pub(crate) const HASH3_MAX_DISTANCE: usize = 1 << 18;

impl Hash3Table {
    pub(crate) fn new(window_log: u32) -> Self {
        let hash_log = HASH3_LOG_MAX.min(window_log);
        let size = 1usize << hash_log;
        Self {
            heads: vec![0; size],
            hash_log,
            next_to_update: 0,
        }
    }

    /// Re-key every position to a buffer whose first `delta` bytes have been
    /// dropped, as C's `ZSTD_reduceIndex` does to `hashTable3`
    /// (`zstd_compress.c:2680`) alongside the hash and chain tables.
    ///
    /// This table is direct-mapped and refilled only forward, from
    /// `next_to_update` to the position being searched. That is what makes the
    /// rebase necessary rather than merely tidy: a bucket the parse does not
    /// revisit keeps whatever it last held, so a stale entry is not corrected
    /// by the next block, or by any block. It names a byte `delta` further
    /// along than the one it stands for, the candidate fails either the
    /// `match_index < pos` test or the three-byte length check, and the match
    /// is silently lost for as long as the frame runs.
    ///
    /// `next_to_update` moves too. Both callers currently overwrite it from the
    /// tree's own frontier before using the table, so today this line changes
    /// nothing; leaving it stale would still be wrong, and a caller that
    /// trusted the field would inherit a bug rather than find one.
    pub(crate) fn shift_positions(&mut self, delta: usize) {
        shift_raw_positions(&mut self.heads, delta, 0);
        self.next_to_update = self.next_to_update.saturating_sub(delta);
    }

    /// 3-byte hash matching C's ZSTD_hash3Ptr: read 4 bytes as LE u32,
    /// shift left 8 (use bottom 24 bits), multiply by prime, take top bits.
    #[inline(always)]
    fn hash3(src: &[u8], pos: usize, hash_log: u32) -> usize {
        debug_assert!(pos + 4 <= src.len());
        let value = u32::from_le_bytes([src[pos], src[pos + 1], src[pos + 2], src[pos + 3]]);
        ((value << 8).wrapping_mul(HASH3_PRIME) >> (32 - hash_log)) as usize
    }

    /// Lazily insert all positions from `next_to_update` to `pos - 1`,
    /// then return the current occupant of `pos`'s hash bucket (the match
    /// candidate). Matches C's `ZSTD_insertAndFindFirstIndexHash3`.
    /// Does NOT insert `pos` itself — that happens on the next call.
    #[inline]
    pub(crate) fn insert_and_find(&mut self, src: &[u8], pos: usize) -> u32 {
        let mut idx = self.next_to_update;
        let target = pos;
        let hash_log = self.hash_log;
        while idx < target {
            let h = Self::hash3(src, idx, hash_log);
            self.heads[h] = idx as u32;
            idx += 1;
        }
        self.next_to_update = target;
        let h = Self::hash3(src, pos, hash_log);
        self.heads[h]
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ParserStrategy {
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

impl ParserStrategy {
    pub(crate) fn zstd_rank(self) -> u32 {
        match self {
            Self::Fast => 1,
            Self::DoubleFast => 2,
            Self::Greedy => 3,
            Self::Lazy => 4,
            Self::Lazy2 => 5,
            Self::GreedyRow => 3,
            Self::LazyRow => 4,
            Self::Lazy2Row => 5,
            Self::BinaryTreeLazy2 => 6,
            Self::BinaryTreeOpt => 7,
            Self::BinaryTreeUltra => 8,
        }
    }

    pub(crate) fn is_hash_chain(self) -> bool {
        matches!(self, Self::Greedy | Self::Lazy | Self::Lazy2)
    }

    pub(crate) fn is_row_hash(self) -> bool {
        matches!(self, Self::GreedyRow | Self::LazyRow | Self::Lazy2Row)
    }

    pub(crate) fn is_binary_tree(self) -> bool {
        matches!(
            self,
            Self::BinaryTreeLazy2 | Self::BinaryTreeOpt | Self::BinaryTreeUltra
        )
    }

    /// Whether this parser prices long-distance matches against the ones it
    /// finds itself, rather than having them laid down for it.
    ///
    /// C's threshold is `cParams->strategy >= ZSTD_btopt` in
    /// `ZSTD_ldm_blockCompress` (`zstd_ldm.c:698`), which splits that function
    /// in two: below it, the matcher's output is taken and the parser runs on
    /// the gaps; at and above it, the store is handed to the parser and the
    /// parser decides.
    pub(crate) fn prices_long_distance_matches(self) -> bool {
        self.zstd_rank() >= ParserStrategy::BinaryTreeOpt.zstd_rank()
    }

    /// Whether literal compression should search for the Huffman table depth
    /// rather than estimate it.
    ///
    /// C's `HUF_OPTIMAL_DEPTH_THRESHOLD` is `ZSTD_btultra`, and both
    /// `ZSTD_compressLiterals` and `ZSTD_buildBlockEntropyStats` compare
    /// `cParams.strategy` against it. `BinaryTreeUltra` covers `ZSTD_btultra`
    /// and `ZSTD_btultra2`, which are ranks 8 and 9.
    pub(crate) fn searches_huffman_table_depth(self) -> bool {
        self.zstd_rank() >= ParserStrategy::BinaryTreeUltra.zstd_rank()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrefixMatchMode {
    ExtDict,
    DictMatchState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MatchFinderParameters {
    pub(crate) parser_strategy: ParserStrategy,
    pub(crate) hash_bits: u32,
    pub(crate) chain_log: u32,
    pub(crate) secondary_hash_bits: u32,
    pub(crate) search_log: u32,
    pub(crate) min_match: u32,
    pub(crate) fast_search_step: usize,
    pub(crate) search_depth: usize,
    pub(crate) dictionary_search_depth: usize,
    pub(crate) source_score_penalty_with_prefix: i32,
    pub(crate) lazy_search_depth: usize,
    pub(crate) skip_search_strength: u32,
    pub(crate) min_match_length_zero_literals: usize,
    pub(crate) min_match_length_after_literals: usize,
    pub(crate) good_enough_match_length: usize,
    pub(crate) target_length: usize,
    pub(crate) ext_dict_index_bias: usize,
    /// Apply extra cost for long offsets (off_code >= 20).
    /// True for btopt and btultra (C optLevel < 2), false for btultra2.
    pub(crate) long_offset_penalty: bool,
    /// Whether the optimal parser prices a literal by the block's statistics or
    /// at the eight bits an uncompressed one occupies. C's
    /// `ZSTD_compressedLiterals`, read from the copy of
    /// `ZSTD_c_literalCompressionMode` that `zstd_compress.c:3285` puts in the
    /// match state.
    ///
    /// Only the optimal parsers consult it; the others do not price anything.
    pub(crate) compressed_literals: bool,
    /// Window log for this compression level. Used for HC3 hash table sizing.
    pub(crate) window_log: u32,
    /// Whether the dictionary is *attached* rather than copied — C's
    /// `ZSTD_shouldAttachDict`, which decides between
    /// `ZSTD_resetCCtx_byAttachingCDict` and `..._byCopyingCDict`.
    ///
    /// Attaching gives the dictionary its own match state, so the optimal
    /// parser runs a source tree and a dictionary tree and searches both.
    /// Copying folds the dictionary into one unified tree in extDict
    /// coordinates and skips the second phase entirely.
    ///
    /// This used to be read off `source_hash_bits.is_some()`, which happened to
    /// be set on exactly the attaching path. That made a parameter and a
    /// dispatch decision the same field: correcting the parameters would have
    /// silently changed which parser ran.
    pub(crate) dictionary_attaches: bool,
    /// The dictionary match state's own table geometry — C's
    /// `cdict->matchState.cParams`, which attaching points `dictMatchState`
    /// at (`zstd_compress.c:2229`) and never rewrites.
    ///
    /// These are *not* `appliedParams.cParams`. Applied is the CDict's
    /// parameters re-adjusted against the source, so on a source smaller than
    /// the dictionary it shrinks: a 16 KiB dictionary against a 256-byte
    /// source resolves `chain_log` 16 for the dictionary and 9 for applied.
    /// Since the tree's roll buffer holds `1 << (chain_log - 1)` positions,
    /// sizing the dictionary's tables from applied put 255 positions of a
    /// 16 KiB dictionary within reach and made the rest unreachable.
    ///
    /// Only these three fields can ever differ — `search_log`, `min_match`,
    /// `target_length` and `strategy` are identical on all 247 attached rows
    /// of `oracles/cparams`, because applied is *derived* from the CDict's
    /// parameters and `ZSTD_adjustCParams_internal` touches only the window
    /// and the two table logs.
    ///
    /// `None` outside the dictionary paths, where the accessors below fall
    /// back to the main fields.
    pub(crate) dictionary_hash_bits: Option<u32>,
    pub(crate) dictionary_chain_log: Option<u32>,
    pub(crate) dictionary_window_log: Option<u32>,
    /// The lowest position in the prefix at which a match may start — C's
    /// `dms->window.lowLimit`, which is where `ZSTD_DUBT_findBetterDictMatch`
    /// stops (`zstd_lazy.c:199`).
    ///
    /// Zero for a raw-content dictionary and for streaming history, where the
    /// prefix is exactly the referenceable bytes. Two for a formatted
    /// dictionary, because [`Dictionary::matching_content`] hands the encoder a
    /// prefix that starts two bytes *before* the parsed content in order to
    /// keep virtual indices aligned with the prepared tables — the same
    /// `ZSTD_WINDOW_START_INDEX` bias C carries. Those two bytes are alignment,
    /// not dictionary content: the decoder is given `content()` and cannot
    /// resolve an offset that reaches them.
    ///
    /// It went unnoticed for as long as the floor was trimmed to the window,
    /// which on any dictionary bigger than the window kept position 0 out of
    /// reach anyway. Widening the floor to the whole live dictionary is what
    /// exposed it, as a frame this crate could not read back.
    pub(crate) prefix_low_limit: usize,
}

/// The log of the cycle a hash chain's `previous` table wraps at, which is the
/// clamped chain log and so the size of that table.
pub(crate) fn chain_cycle_log(chain_log: u32) -> u32 {
    chain_log.clamp(10, 20)
}

/// The log of the cycle a binary tree's `children` table wraps at.
///
/// One below the chain log, which is C's `ZSTD_cycleLog` (`zstd_compress.c:1441`)
/// for every strategy from `btlazy2` up: the tree stores two children per
/// position, so it holds half as many positions as the chain would.
pub(crate) fn binary_tree_cycle_log(chain_log: u32) -> u32 {
    chain_log.saturating_sub(1).max(1)
}

impl MatchFinderParameters {
    /// The cycle length the strategy's position-indexed table wraps at, and so
    /// the alignment any rebase of it has to respect. One where the strategy
    /// has no such table.
    ///
    /// Derived from the parameters rather than read off a built finder, because
    /// the streaming encoder has to size its buffer around this before it has a
    /// finder to ask. The two are kept in step by both going through
    /// [`chain_cycle_log`] and [`binary_tree_cycle_log`].
    pub(crate) fn rebase_period(&self) -> usize {
        if self.parser_strategy.is_binary_tree() {
            1usize << binary_tree_cycle_log(self.chain_log)
        } else if self.parser_strategy.is_hash_chain() {
            1usize << chain_cycle_log(self.chain_log)
        } else {
            1
        }
    }

    /// Hash bits for the dictionary's own tables — C's `dmsCParams->hashLog`,
    /// which `ZSTD_DUBT_findBetterDictMatch` hashes with (`zstd_lazy.c:176`).
    pub(crate) fn dictionary_hash_bits(&self) -> u32 {
        self.dictionary_hash_bits.unwrap_or(self.hash_bits)
    }
    /// Chain log for the dictionary's own tables — C's `dmsCParams->chainLog`,
    /// which the dictionary tree's `btMask` comes from (`zstd_lazy.c:190`).
    pub(crate) fn dictionary_chain_log(&self) -> u32 {
        self.dictionary_chain_log.unwrap_or(self.chain_log)
    }
    /// Window log for the dictionary's own tables.
    pub(crate) fn dictionary_window_log(&self) -> u32 {
        self.dictionary_window_log.unwrap_or(self.window_log)
    }
}

impl Default for MatchFinderParameters {
    fn default() -> Self {
        Self {
            parser_strategy: ParserStrategy::Lazy,
            hash_bits: 16,
            chain_log: 16,
            secondary_hash_bits: 15,
            search_log: 4,
            min_match: MIN_MATCH as u32,
            fast_search_step: 1,
            search_depth: 4,
            dictionary_search_depth: 12,
            source_score_penalty_with_prefix: 0,
            lazy_search_depth: 1,
            skip_search_strength: 8,
            min_match_length_zero_literals: MIN_MATCH,
            min_match_length_after_literals: 6,
            good_enough_match_length: 32,
            target_length: 0,
            ext_dict_index_bias: 0,
            long_offset_penalty: false,
            compressed_literals: true,
            window_log: 22,
            dictionary_attaches: false,
            dictionary_hash_bits: None,
            dictionary_chain_log: None,
            dictionary_window_log: None,
            prefix_low_limit: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SequencePlanningProfile {
    pub(crate) enabled: bool,
    pub(crate) row_search: Duration,
    pub(crate) chain_search: Duration,
    pub(crate) rep_check: Duration,
    pub(crate) match_count: Duration,
    pub(crate) insert_update: Duration,
    pub(crate) parser: Duration,
    pub(crate) row_parser_baseline_rep: Duration,
    pub(crate) row_parser_baseline_regular: Duration,
    pub(crate) row_parser_continue: Duration,
    pub(crate) row_parser_store: Duration,
    pub(crate) row_parser_rep2: Duration,
}

impl SequencePlanningProfile {
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub(crate) fn is_enabled(self) -> bool {
        self.enabled
    }

    pub(crate) fn reset(&mut self) {
        let enabled = self.enabled;
        *self = Self {
            enabled,
            ..Self::default()
        };
    }

    pub(crate) fn record_iteration(&mut self, total: Duration, timings: PlanningIterationTimings) {
        if !self.enabled {
            return;
        }
        self.row_search += timings.row_search;
        self.chain_search += timings.chain_search;
        self.rep_check += timings.rep_check;
        self.match_count += timings.match_count;
        self.insert_update += timings.insert_update;
        self.row_parser_baseline_rep += timings.row_parser_baseline_rep;
        self.row_parser_baseline_regular += timings.row_parser_baseline_regular;
        self.row_parser_continue += timings.row_parser_continue;
        self.row_parser_store += timings.row_parser_store;
        self.row_parser_rep2 += timings.row_parser_rep2;
        self.parser += total
            .saturating_sub(timings.row_search)
            .saturating_sub(timings.chain_search)
            .saturating_sub(timings.rep_check)
            .saturating_sub(timings.match_count)
            .saturating_sub(timings.insert_update);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PlanningIterationTimings {
    pub(crate) row_search: Duration,
    pub(crate) chain_search: Duration,
    pub(crate) rep_check: Duration,
    pub(crate) match_count: Duration,
    pub(crate) insert_update: Duration,
    pub(crate) row_parser_baseline_rep: Duration,
    pub(crate) row_parser_baseline_regular: Duration,
    pub(crate) row_parser_continue: Duration,
    pub(crate) row_parser_store: Duration,
    pub(crate) row_parser_rep2: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PlanningIterationCategorySnapshot {
    pub(crate) row_search: Duration,
    pub(crate) chain_search: Duration,
    pub(crate) rep_check: Duration,
    pub(crate) match_count: Duration,
    pub(crate) insert_update: Duration,
}

impl PlanningIterationCategorySnapshot {
    #[inline(always)]
    pub(crate) fn capture(timings: Option<&PlanningIterationTimings>) -> Option<Self> {
        timings.map(|timings| Self {
            row_search: timings.row_search,
            chain_search: timings.chain_search,
            rep_check: timings.rep_check,
            match_count: timings.match_count,
            insert_update: timings.insert_update,
        })
    }

    #[inline(always)]
    pub(crate) fn delta(self, timings: &PlanningIterationTimings) -> Duration {
        timings
            .row_search
            .saturating_sub(self.row_search)
            .saturating_add(timings.chain_search.saturating_sub(self.chain_search))
            .saturating_add(timings.rep_check.saturating_sub(self.rep_check))
            .saturating_add(timings.match_count.saturating_sub(self.match_count))
            .saturating_add(timings.insert_update.saturating_sub(self.insert_update))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LazyParserPhase {
    BaselineRep,
    BaselineRegular,
    Continue,
    Store,
    Rep2,
}

#[inline(always)]
pub(crate) fn record_lazy_parser_phase(
    timings: Option<&mut PlanningIterationTimings>,
    phase: LazyParserPhase,
    phase_start: Option<Instant>,
    snapshot: Option<PlanningIterationCategorySnapshot>,
) {
    let (Some(timings), Some(phase_start), Some(snapshot)) = (timings, phase_start, snapshot)
    else {
        return;
    };
    let duration = phase_start
        .elapsed()
        .saturating_sub(snapshot.delta(timings));
    match phase {
        LazyParserPhase::BaselineRep => timings.row_parser_baseline_rep += duration,
        LazyParserPhase::BaselineRegular => timings.row_parser_baseline_regular += duration,
        LazyParserPhase::Continue => timings.row_parser_continue += duration,
        LazyParserPhase::Store => timings.row_parser_store += duration,
        LazyParserPhase::Rep2 => timings.row_parser_rep2 += duration,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SeqStore {
    pub(crate) literals: Vec<u8>,
    pub(crate) sequences: Vec<SequenceCommand>,
    pub(crate) literal_codes: Vec<u8>,
    pub(crate) offset_codes: Vec<u8>,
    pub(crate) match_codes: Vec<u8>,
    pub(crate) trace_match_sources: Vec<SequenceTraceMatchSource>,
    pub(crate) trace_first_row_contest: Option<SequenceTraceRowSearchContest>,
    pub(crate) trace_emissions: Vec<SequenceTraceEmission>,
    pub(crate) trace_row_searches: Vec<SequenceTraceRowSearch>,
    pub(crate) trace_row_lazy_probes: Vec<SequenceTraceRowLazyProbe>,
    #[cfg(test)]
    pub(crate) trace_chain_searches: Vec<SequenceTraceChainSearch>,
    pub(crate) trace_enabled: bool,
    pub(crate) planning_profile: SequencePlanningProfile,
    pub(crate) repeat_offsets: RepeatOffsets,
    /// Cached optimal-parser DP nodes, reused across blocks to avoid
    /// per-block allocation of the ~115 KB nodes array.
    pub(crate) opt_nodes: Vec<OptimalPathNode>,
    /// Cached tree-match results, reused across positions.
    pub(crate) opt_tree_matches: Vec<MatchCandidate>,
    /// Cached optimal-match candidates, reused across positions.
    pub(crate) opt_candidates: Vec<OptimalMatchCandidate>,
    /// Cached backtrace sequence buffer, reused across DP windows.
    pub(crate) opt_backtrace: Vec<(u32, u32, u32)>,
    /// Dictionary-derived pricing seed for the optimal parser.
    pub(crate) opt_dict_price_seed: Option<DictionaryPriceSeed>,
    /// Accumulated pricing state from the previous block, for cross-block
    /// frequency persistence (matching C's optState_t).
    pub(crate) opt_price_state: Option<OptimalPriceState>,
    /// HC3 hash table for 3-byte match finding in the optimal parser.
    /// Persists across blocks within a frame (matching C's hashTable3).
    pub(crate) opt_hash3: Option<Hash3Table>,
    /// Source-only binary tree for the optimal parser. Contains only source
    /// positions (no dictionary entries). Persists across blocks within a frame.
    pub(crate) opt_source_bt: Option<BinaryTreeFinder>,
    /// Pre-built dictionary binary tree for the optimal parser. Read-only
    /// after initial construction. Shared via Arc for cheap cloning.
    pub(crate) opt_dict_bt: Option<Arc<BinaryTreeFinder>>,
    /// Prefix length `opt_source_bt` and `opt_dict_bt` were built from.
    ///
    /// Reusing them across blocks is only sound while the prefix is fixed,
    /// which holds for a dictionary but not for streaming, where the prefix is
    /// the frame history and grows with every block. Rebuilding when this
    /// changes is what keeps streaming from parsing later blocks against a
    /// stale tree.
    pub(crate) opt_prefix_len: Option<usize>,
    /// Scratch buffer for dictionary match candidates from Phase 2 search.
    pub(crate) opt_dict_matches: Vec<MatchCandidate>,
}

pub(crate) type SequencePlan = SeqStore;

// Not derivable, however much it looks it: `trace_enabled` defaults to
// `cfg!(test)`, not to `false`. Clippy's `derivable_impls` sees the non-test
// build, where `cfg!(test)` folds to `false` and the impl really does match
// the derive, and suggests replacing it. Taking that suggestion turns tracing
// off in test builds, where every parity assertion depends on it, and the
// failure is silent: the planner still runs, it just stops recording what it
// did. This crate has been bitten by the `cfg!(test)`/tracing relationship
// before; the allow is here so the next reader knows the impl is deliberate.
#[allow(clippy::derivable_impls)]
impl Default for SeqStore {
    fn default() -> Self {
        Self {
            literals: Vec::new(),
            sequences: Vec::new(),
            literal_codes: Vec::new(),
            offset_codes: Vec::new(),
            match_codes: Vec::new(),
            trace_match_sources: Vec::new(),
            trace_first_row_contest: None,
            trace_emissions: Vec::new(),
            trace_row_searches: Vec::new(),
            trace_row_lazy_probes: Vec::new(),
            #[cfg(test)]
            trace_chain_searches: Vec::new(),
            trace_enabled: cfg!(test),
            planning_profile: SequencePlanningProfile::default(),
            repeat_offsets: RepeatOffsets::default(),
            opt_nodes: Vec::new(),
            opt_tree_matches: Vec::new(),
            opt_candidates: Vec::new(),
            opt_backtrace: Vec::new(),
            opt_dict_price_seed: None,
            opt_price_state: None,
            opt_hash3: None,
            opt_source_bt: None,
            opt_dict_bt: None,
            opt_prefix_len: None,
            opt_dict_matches: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SequenceTraceMatchSource {
    #[default]
    Unknown,
    Dict,
    Prefix,
    Source,
    Rep,
    /// A match the long-distance matcher found, which the parser never saw.
    LongDistance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SequenceTraceRowSearchContest {
    pub(crate) winner: SequenceTraceMatchSource,
    pub(crate) source_length: usize,
    pub(crate) dict_length: usize,
    pub(crate) attempts_left_before_dict: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SequenceTraceEmissionKind {
    #[default]
    Regular,
    Rep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SequenceTraceRowLazyStopReason {
    #[default]
    None,
    NoBaseline,
    Depth0,
    Limit,
    NoRegularImprove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SequenceTraceEmission {
    pub(crate) kind: SequenceTraceEmissionKind,
    pub(crate) source: SequenceTraceMatchSource,
    pub(crate) anchor_before: usize,
    pub(crate) start: usize,
    pub(crate) literal_length: usize,
    pub(crate) match_length: usize,
    pub(crate) off_base: u32,
    pub(crate) raw_offset: usize,
    pub(crate) offset_1_before: usize,
    pub(crate) offset_2_before: usize,
    pub(crate) offset_1_after: usize,
    pub(crate) offset_2_after: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SequenceTraceRowSearch {
    pub(crate) pos: usize,
    pub(crate) next_to_update_before_search: usize,
    pub(crate) hash: usize,
    pub(crate) rel_row: usize,
    pub(crate) tag: u8,
    pub(crate) low_limit: usize,
    pub(crate) attempt_budget: usize,
    pub(crate) head_index: usize,
    pub(crate) insert_index: usize,
    pub(crate) group_width: usize,
    pub(crate) source_match_count: usize,
    pub(crate) source_match_positions: [usize; 4],
    pub(crate) source_match_indices: [usize; 4],
    pub(crate) source_visit_count: usize,
    pub(crate) source_visit_positions: [usize; 4],
    pub(crate) source_visit_indices: [usize; 4],
    pub(crate) source_visit_lengths: [usize; 4],
    pub(crate) source_visit_gate_passes: [bool; 4],
    pub(crate) source_visit_winner_lengths: [usize; 4],
    pub(crate) source_visit_winner_off_bases: [usize; 4],
    pub(crate) source_length: usize,
    pub(crate) source_offset: usize,
    pub(crate) dict_length: usize,
    pub(crate) dict_offset: usize,
    pub(crate) attempts_left_before_dict: usize,
    pub(crate) winner: SequenceTraceMatchSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SequenceTraceRowLazyProbe {
    pub(crate) pos: usize,
    pub(crate) anchor: usize,
    pub(crate) offset_1: usize,
    pub(crate) offset_2: usize,
    pub(crate) baseline_rep_length: usize,
    pub(crate) baseline_regular: SequenceTraceRowSearch,
    pub(crate) depth1_rep_length: usize,
    pub(crate) depth1_regular_length: usize,
    pub(crate) depth1_regular_off_base: usize,
    pub(crate) depth2_rep_length: usize,
    pub(crate) depth2_regular_length: usize,
    pub(crate) depth2_regular_off_base: usize,
    pub(crate) chosen_kind: SequenceTraceEmissionKind,
    pub(crate) chosen_start: usize,
    pub(crate) chosen_length: usize,
    pub(crate) chosen_off_base: usize,
    pub(crate) literal_length: usize,
    pub(crate) continue_step_count: usize,
    pub(crate) continue_positions: [usize; ROW_LAZY_TRACE_MAX_STEPS],
    pub(crate) continue_rep_lengths: [usize; ROW_LAZY_TRACE_MAX_STEPS],
    pub(crate) continue_rep_improved: [bool; ROW_LAZY_TRACE_MAX_STEPS],
    pub(crate) continue_regular_lengths: [usize; ROW_LAZY_TRACE_MAX_STEPS],
    pub(crate) continue_regular_off_bases: [usize; ROW_LAZY_TRACE_MAX_STEPS],
    pub(crate) continue_regular_improved: [bool; ROW_LAZY_TRACE_MAX_STEPS],
    pub(crate) continue_current_kinds: [SequenceTraceEmissionKind; ROW_LAZY_TRACE_MAX_STEPS],
    pub(crate) continue_current_starts: [usize; ROW_LAZY_TRACE_MAX_STEPS],
    pub(crate) continue_current_lengths: [usize; ROW_LAZY_TRACE_MAX_STEPS],
    pub(crate) continue_current_off_bases: [usize; ROW_LAZY_TRACE_MAX_STEPS],
    pub(crate) stop_reason: SequenceTraceRowLazyStopReason,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SequenceTraceChainSearch {
    pub(crate) anchor: usize,
    pub(crate) pos: usize,
    pub(crate) probe_depth: u8,
    pub(crate) offset_1: usize,
    pub(crate) offset_2: usize,
    pub(crate) current_kind: SequenceTraceEmissionKind,
    pub(crate) current_source: SequenceTraceMatchSource,
    pub(crate) current_length: usize,
    pub(crate) current_offbase: u32,
    pub(crate) rep_length: usize,
    pub(crate) source_length: usize,
    pub(crate) source_offset: usize,
    pub(crate) dict_length: usize,
    pub(crate) dict_offset: usize,
    pub(crate) hash_slot: usize,
    pub(crate) head_index: usize,
    pub(crate) next_to_update: usize,
    pub(crate) low_limit: usize,
    pub(crate) min_chain: usize,
    pub(crate) chain_link_count: usize,
    pub(crate) chain_link_indices: [usize; 4],
    pub(crate) chain_link_sources: [SequenceTraceMatchSource; 4],
    pub(crate) visit_count: usize,
    pub(crate) visit_indices: [usize; 4],
    pub(crate) visit_lengths: [usize; 4],
    pub(crate) regular_winner: SequenceTraceMatchSource,
    pub(crate) chosen_kind: SequenceTraceEmissionKind,
    pub(crate) chosen_source: SequenceTraceMatchSource,
    pub(crate) chosen_start: usize,
    pub(crate) chosen_length: usize,
    pub(crate) chosen_offbase: u32,
    pub(crate) attempts_left_before_dict: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RowMatchBufferTrace {
    pub(crate) next_to_update_before_search: usize,
    pub(crate) hash: usize,
    pub(crate) rel_row: usize,
    pub(crate) tag: u8,
    pub(crate) low_limit: usize,
    pub(crate) attempt_budget: usize,
    pub(crate) head_index: usize,
    pub(crate) insert_index: usize,
    pub(crate) group_width: usize,
    pub(crate) num_matches: usize,
    pub(crate) match_positions: [usize; 4],
    pub(crate) match_indices: [usize; 4],
    pub(crate) visit_count: usize,
    pub(crate) visit_positions: [usize; 4],
    pub(crate) visit_indices: [usize; 4],
    pub(crate) visit_lengths: [usize; 4],
    pub(crate) visit_gate_passes: [bool; 4],
    pub(crate) visit_winner_lengths: [usize; 4],
    pub(crate) visit_winner_off_bases: [usize; 4],
}

impl SeqStore {
    pub(crate) fn enable_tracing(&mut self) {
        self.trace_enabled = true;
    }

    /// Turn tracing off, including the `cfg!(test)` default.
    ///
    /// Traces default to on under `cfg(test)`, which means a unit test reaches
    /// the traced copy of a planner and never the untraced copy that actually
    /// ships. Where the two are separate loops, that leaves the shipped one
    /// with no unit coverage at all, so a test that wants to compare them has
    /// to ask for the untraced path explicitly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn disable_tracing(&mut self) {
        self.trace_enabled = false;
    }

    pub(crate) fn tracing_enabled(&self) -> bool {
        self.trace_enabled
    }

    /// Set, rather than latch: [`SequencePlanningProfile::reset`] deliberately
    /// carries `enabled` across blocks, so a plan that once profiled would go
    /// on profiling for the life of the scratch it lives in. Every block says
    /// what it wants.
    pub(crate) fn set_planning_profile(&mut self, enabled: bool) {
        self.planning_profile.set_enabled(enabled);
    }

    pub(crate) fn planning_profile(&self) -> SequencePlanningProfile {
        self.planning_profile
    }

    pub(crate) fn reset_for_block(&mut self, block_len: usize) {
        self.literals.clear();
        self.sequences.clear();
        self.literal_codes.clear();
        self.offset_codes.clear();
        self.match_codes.clear();
        self.trace_match_sources.clear();
        self.trace_first_row_contest = None;
        self.trace_emissions.clear();
        self.trace_row_searches.clear();
        self.trace_row_lazy_probes.clear();
        #[cfg(test)]
        self.trace_chain_searches.clear();
        self.planning_profile.reset();

        // Every one of these was just cleared, so `len` is zero and
        // `reserve(n)` asks for exactly `n` of capacity. Passing a shortfall
        // computed against the *existing* capacity is what this used to do, and
        // on a vector that already held some capacity it asked for less than it
        // already had and grew nothing — leaving the guarded `ptr::write` calls
        // below writing past the end. The four code vectors grow independently
        // across blocks, so they drift to different capacities and only some of
        // them come up short: it took a frame of small flushed blocks after
        // larger ones to land on it.
        //
        // `reserve` is already a no-op when the capacity suffices, so there is
        // nothing for a conditional to save here.
        debug_assert!(self.literals.is_empty() && self.sequences.is_empty());

        // Literal copies write a fixed 16 bytes whatever the run length, so the
        // buffer needs room past the block for the last one.
        self.literals.reserve(block_len + WILDCOPY_OVERLENGTH);

        // A sequence covers at least `MIN_MATCH` bytes of the block, so this
        // many is the most a block can produce.
        let sequence_bound = block_len.div_ceil(MIN_MATCH);
        self.sequences.reserve(sequence_bound);
        self.literal_codes.reserve(sequence_bound);
        self.offset_codes.reserve(sequence_bound);
        self.match_codes.reserve(sequence_bound);
        if self.trace_enabled {
            self.trace_match_sources.reserve(sequence_bound);
            self.trace_emissions.reserve(sequence_bound);
            self.trace_row_searches.reserve(sequence_bound);
            self.trace_row_lazy_probes.reserve(sequence_bound);
            #[cfg(test)]
            self.trace_chain_searches.reserve(sequence_bound * 3);
        }
    }

    /// Populate this SeqStore from slices, for use as a sub-block plan during
    /// post-sequence block splitting. Reuses existing allocations.
    /// Append a sequence whose literals are already in [`Self::literals`].
    ///
    /// The parsers all copy their literals as they emit, so this exists for
    /// the one producer that does not: a long-distance match, whose literal
    /// run was laid down by the parser that ran on the gap in front of it.
    /// Safe pushes rather than the guarded `ptr::write` the parsers use,
    /// because the capacity `reset_for_block` reserved was computed for one
    /// parse of the block and a long-distance block is several.
    pub(crate) fn push_stored_sequence(&mut self, sequence: SequenceCommand) {
        self.literal_codes
            .push(literal_length_code_unchecked(sequence.literal_length));
        self.offset_codes
            .push(offset_code_unchecked(sequence.offset_value));
        self.match_codes
            .push(match_length_code_unchecked(sequence.match_length));
        self.sequences.push(sequence);
        if self.trace_enabled {
            self.trace_match_sources
                .push(SequenceTraceMatchSource::LongDistance);
        }
    }

    pub(crate) fn populate_from_subblock(
        &mut self,
        sequences: &[SequenceCommand],
        literals: &[u8],
        ll_codes: &[u8],
        of_codes: &[u8],
        ml_codes: &[u8],
        repeat_offsets: RepeatOffsets,
    ) {
        self.literals.clear();
        self.literals.extend_from_slice(literals);
        self.sequences.clear();
        self.sequences.extend_from_slice(sequences);
        self.literal_codes.clear();
        self.literal_codes.extend_from_slice(ll_codes);
        self.offset_codes.clear();
        self.offset_codes.extend_from_slice(of_codes);
        self.match_codes.clear();
        self.match_codes.extend_from_slice(ml_codes);
        self.trace_match_sources.clear();
        self.trace_first_row_contest = None;
        self.trace_emissions.clear();
        self.trace_row_searches.clear();
        self.trace_row_lazy_probes.clear();
        #[cfg(test)]
        self.trace_chain_searches.clear();
        self.trace_enabled = false;
        self.planning_profile.reset();
        self.repeat_offsets = repeat_offsets;
    }

    /// Ensure code vectors are populated from sequence commands.
    /// No-op if codes are already computed.
    pub(crate) fn ensure_codes_populated(&mut self) {
        if self.literal_codes.len() == self.sequences.len() {
            return;
        }
        self.literal_codes.clear();
        self.offset_codes.clear();
        self.match_codes.clear();
        self.literal_codes.reserve(self.sequences.len());
        self.offset_codes.reserve(self.sequences.len());
        self.match_codes.reserve(self.sequences.len());
        for seq in &self.sequences {
            self.literal_codes
                .push(literal_length_code_unchecked(seq.literal_length));
            self.offset_codes
                .push(offset_code_unchecked(seq.offset_value));
            self.match_codes
                .push(match_length_code_unchecked(seq.match_length));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MatchCandidate {
    pub(crate) offset: usize,
    pub(crate) length: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ExtendedMatch {
    pub(crate) start: usize,
    pub(crate) offset: usize,
    pub(crate) length: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LazyMatchDecision {
    pub(crate) skip: usize,
    pub(crate) inserted: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LazyMatchKind {
    Repeat1,
    Regular,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LazyParserMatch {
    pub(crate) start: usize,
    pub(crate) offset: usize,
    pub(crate) length: usize,
    pub(crate) kind: LazyMatchKind,
}

impl LazyParserMatch {
    pub(crate) fn offbase(self) -> u32 {
        match self.kind {
            LazyMatchKind::Repeat1 => 1,
            LazyMatchKind::Regular => (self.offset.min(u32::MAX as usize) as u32).saturating_add(3),
        }
    }

    pub(crate) fn candidate(self) -> MatchCandidate {
        MatchCandidate {
            offset: self.offset,
            length: self.length,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[inline(always)]
pub(crate) fn repeat_match_length_without_prefix(
    src: &[u8],
    pos: usize,
    raw_offset: usize,
    window_low: usize,
) -> Option<usize> {
    let start = pos + 1;
    if raw_offset == 0 || start < raw_offset || start + MIN_MATCH > src.len() {
        return None;
    }
    let match_start = start - raw_offset;
    if match_start < window_low {
        return None;
    }
    // SAFETY: match_start < start (raw_offset > 0), start + 4 <= src.len() checked above.
    #[allow(unsafe_code)]
    if !unsafe { match_prefix_4bytes(src, match_start, start) } {
        return None;
    }
    let length = if start + MIN_MATCH < src.len() {
        #[allow(unsafe_code)]
        // SAFETY: match_start + 4 < start + 4 < src.len()
        {
            MIN_MATCH
                + unsafe {
                    count_match_length_unchecked(src, match_start + MIN_MATCH, start + MIN_MATCH)
                }
        }
    } else {
        MIN_MATCH
    };
    Some(length)
}

#[allow(dead_code)]
pub(crate) fn repeat_match_length_with_prefix(
    prefix: &[u8],
    src: &[u8],
    pos: usize,
    raw_offset: usize,
    prefix_low: usize,
    source_low: usize,
) -> Option<usize> {
    let start = pos + 1;
    let current = prefix.len() + start;
    if raw_offset == 0 || raw_offset > current || start + MIN_MATCH > src.len() {
        return None;
    }
    let match_start = current - raw_offset;
    if !logical_match_start_is_valid(prefix.len(), match_start, prefix_low, source_low) {
        return None;
    }
    logical_match_has_length(prefix, src, match_start, current, MIN_MATCH)
        .then(|| count_match_length_with_prefix(prefix, src, match_start, current))
}

#[inline(always)]
pub(crate) fn repeat_ahead_match_without_prefix(
    src: &[u8],
    pos: usize,
    raw_offset: usize,
    window_low: usize,
) -> Option<MatchCandidate> {
    repeat_match_length_without_prefix(src, pos, raw_offset, window_low).map(|length| {
        MatchCandidate {
            offset: raw_offset,
            length,
        }
    })
}

pub(crate) fn repeat_ahead_match_with_prefix_chain(
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    pos: usize,
    raw_offset: usize,
    prefix_low: usize,
    source_low: usize,
) -> Option<MatchCandidate> {
    let start = pos + 1;
    let current = prefix_chain.len() + start;
    if raw_offset == 0 || raw_offset > current || start + MIN_MATCH > src.len() {
        return None;
    }

    let match_start = current - raw_offset;
    if !logical_match_start_is_valid(prefix_chain.len(), match_start, prefix_low, source_low) {
        return None;
    }
    virtual_match_has_length(prefix_chain, src, match_start, current, MIN_MATCH).then(|| {
        MatchCandidate {
            offset: raw_offset,
            length: count_match_length_virtual(prefix_chain, src, match_start, current),
        }
    })
}

#[inline(always)]
pub(crate) fn extend_back_source_match(
    src: &[u8],
    anchor: usize,
    mut found: DoubleFastMatch,
) -> DoubleFastMatch {
    while found.start > anchor
        && found.start > found.offset
        && src[found.start - 1] == src[found.start - 1 - found.offset]
    {
        found.start -= 1;
        found.length += 1;
    }
    found
}

pub(crate) fn extend_back_logical_match(
    prefix: &[u8],
    src: &[u8],
    anchor: usize,
    found: DoubleFastMatch,
) -> DoubleFastMatch {
    extend_back_logical_match_with_min_start(prefix, src, anchor, found, 0)
}

pub(crate) fn extend_back_logical_match_with_min_start(
    prefix: &[u8],
    src: &[u8],
    anchor: usize,
    mut found: DoubleFastMatch,
    min_match_start: usize,
) -> DoubleFastMatch {
    let mut current = prefix.len() + found.start;
    while found.start > anchor
        && current > found.offset
        && current - 1 - found.offset >= min_match_start
        && src[found.start - 1] == logical_byte(prefix, src, current - 1 - found.offset)
    {
        found.start -= 1;
        found.length += 1;
        current -= 1;
    }
    found
}

pub(crate) fn extend_back_logical_match_with_limits(
    prefix: &[u8],
    src: &[u8],
    anchor: usize,
    mut found: DoubleFastMatch,
    prefix_low: usize,
    source_low: usize,
) -> DoubleFastMatch {
    let mut current = prefix.len() + found.start;
    while found.start > anchor
        && current > found.offset
        && logical_match_start_is_valid(
            prefix.len(),
            current - 1 - found.offset,
            prefix_low,
            source_low,
        )
        && src[found.start - 1] == logical_byte(prefix, src, current - 1 - found.offset)
    {
        found.start -= 1;
        found.length += 1;
        current -= 1;
    }
    found
}

#[inline(always)]
#[allow(unsafe_code)]
pub(crate) fn src_match_has_length(src: &[u8], left: usize, right: usize, len: usize) -> bool {
    if left + len > src.len() || right + len > src.len() {
        return false;
    }
    // Explicit word-sized fast paths for the common short lengths (MIN_MATCH
    // and 8-byte). The generic slice-compare tail below compiles to a libc
    // memcmp call; these short paths avoid it.
    // SAFETY: bounds checked above.
    unsafe {
        let base = src.as_ptr();
        match len {
            4 => {
                return core::ptr::read_unaligned(base.add(left) as *const u32)
                    == core::ptr::read_unaligned(base.add(right) as *const u32);
            }
            8 => {
                return core::ptr::read_unaligned(base.add(left) as *const u64)
                    == core::ptr::read_unaligned(base.add(right) as *const u64);
            }
            _ => {}
        }
    }
    src[left..left + len] == src[right..right + len]
}

pub(crate) fn logical_match_has_length(
    prefix: &[u8],
    src: &[u8],
    left: usize,
    right: usize,
    len: usize,
) -> bool {
    let prefix_len = prefix.len();
    let virtual_len = prefix_len + src.len();
    if left + len > virtual_len || right + len > virtual_len {
        return false;
    }

    // Fast path: both positions are in the src segment — use direct comparison.
    if left >= prefix_len && right >= prefix_len {
        let left_src = left - prefix_len;
        let right_src = right - prefix_len;
        return src_match_has_length(src, left_src, right_src, len);
    }

    let mut matched = 0usize;
    while matched < len {
        let left_slice = logical_slice(prefix, src, left + matched);
        let right_slice = logical_slice(prefix, src, right + matched);
        let left_bytes = left_slice.as_slice();
        let right_bytes = right_slice.as_slice();
        let chunk_len = left_bytes.len().min(right_bytes.len()).min(len - matched);
        if left_bytes[..chunk_len] != right_bytes[..chunk_len] {
            return false;
        }
        matched += chunk_len;
    }
    true
}

pub(crate) fn count_match_length_with_prefix(
    prefix: &[u8],
    src: &[u8],
    left: usize,
    right: usize,
) -> usize {
    let prefix_len = prefix.len();

    // Fast path: both positions are in the src segment — use the fast
    // pointer-based counting, avoiding LogicalSlice enum dispatch entirely.
    if left >= prefix_len && right >= prefix_len {
        let left_src = left - prefix_len;
        let right_src = right - prefix_len;
        return count_match_length(src, left_src, right_src);
    }

    // 2-segment counting for cross-boundary matches: count in the first
    // segment (prefix or prefix→src boundary), then continue in src.
    let virtual_len = prefix_len + src.len();
    let max_len = virtual_len
        .saturating_sub(right)
        .min(virtual_len.saturating_sub(left));
    let mut matched = 0usize;
    while matched < max_len {
        let left_slice = logical_slice(prefix, src, left + matched);
        let right_slice = logical_slice(prefix, src, right + matched);
        let left_bytes = left_slice.as_slice();
        let right_bytes = right_slice.as_slice();
        let chunk_len = left_bytes
            .len()
            .min(right_bytes.len())
            .min(max_len - matched);
        let chunk_match =
            count_match_length_slices(&left_bytes[..chunk_len], &right_bytes[..chunk_len]);
        matched += chunk_match;
        if chunk_match != chunk_len {
            break;
        }
    }
    matched
}

pub(crate) fn logical_slice<'a, 'b>(
    prefix: &'a [u8],
    src: &'b [u8],
    pos: usize,
) -> LogicalSlice<'a, 'b> {
    if pos < prefix.len() {
        LogicalSlice::Prefix(&prefix[pos..])
    } else {
        LogicalSlice::Src(&src[pos - prefix.len()..])
    }
}

pub(crate) fn logical_byte(prefix: &[u8], src: &[u8], pos: usize) -> u8 {
    if pos < prefix.len() {
        prefix[pos]
    } else {
        src[pos - prefix.len()]
    }
}

pub(crate) fn logical_match_start_is_valid(
    prefix_len: usize,
    match_start: usize,
    prefix_low: usize,
    source_low: usize,
) -> bool {
    if match_start < prefix_len {
        match_start >= prefix_low
    } else {
        match_start - prefix_len >= source_low
    }
}

pub(crate) fn best_repeat_match(
    src: &[u8],
    pos: usize,
    repeat_offsets: [u32; 3],
    literal_length: usize,
    params: MatchFinderParameters,
    window_low: usize,
) -> Option<MatchCandidate> {
    let [rep1, rep2, rep3] = repeat_offsets;
    let mut best = None;
    let literal_length_zero = literal_length == 0;

    for raw_offset in [
        Some(rep1),
        Some(rep2),
        Some(rep3),
        literal_length_zero.then_some(rep1.saturating_sub(1)),
    ]
    .into_iter()
    .flatten()
    {
        let raw_offset = raw_offset as usize;
        if raw_offset == 0 || raw_offset > pos {
            continue;
        }

        let match_start = pos - raw_offset;
        if match_start < window_low {
            continue;
        }
        let length = count_match_length(src, match_start, pos);
        if length >= params.min_match_length_zero_literals {
            best = choose_better_match(
                best,
                Some(MatchCandidate {
                    offset: raw_offset,
                    length,
                }),
                repeat_offsets,
                literal_length,
            );
        }
    }

    best
}

pub(crate) fn best_match_without_prefix(
    src: &[u8],
    pos: usize,
    repeat_offsets: [u32; 3],
    literal_length: usize,
    params: MatchFinderParameters,
    finder: &mut impl LazySearchFinder,
    window_low: usize,
) -> Option<MatchCandidate> {
    let mut best = best_repeat_match(src, pos, repeat_offsets, literal_length, params, window_low);
    if let Some(candidate) = finder.find_match(src, pos, params, literal_length, window_low) {
        best = choose_better_match(best, Some(candidate), repeat_offsets, literal_length);
    }
    best
}

pub(crate) fn best_match_with_prefix_chain(
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    pos: usize,
    repeat_offsets: [u32; 3],
    literal_length: usize,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    mode: PrefixMatchMode,
    prefix_finder: &impl LazySearchFinder,
    src_finder: &mut impl LazySearchFinder,
) -> Option<MatchCandidate> {
    let mut best = best_repeat_match_with_prefix_chain(
        prefix_chain,
        src,
        pos,
        repeat_offsets,
        literal_length,
        params,
        prefix_low,
        source_low,
    );
    let prefix_candidate = prefix_finder.find_prefix_chain_match(
        prefix_chain,
        src,
        pos,
        params,
        literal_length,
        prefix_low,
    );
    let source_candidate = src_finder.find_match(src, pos, params, literal_length, source_low);
    match mode {
        PrefixMatchMode::DictMatchState => {
            if let Some(candidate) = source_candidate {
                best = choose_better_match(best, Some(candidate), repeat_offsets, literal_length);
            }
            if let Some(candidate) = prefix_candidate {
                best = choose_better_match(best, Some(candidate), repeat_offsets, literal_length);
            }
        }
        PrefixMatchMode::ExtDict => {
            if let Some(candidate) = prefix_candidate {
                best = choose_better_match(best, Some(candidate), repeat_offsets, literal_length);
            }
            if let Some(candidate) = source_candidate {
                best = choose_better_match_with_adjustment(
                    best,
                    Some(candidate),
                    repeat_offsets,
                    literal_length,
                    -params.source_score_penalty_with_prefix,
                );
            }
        }
    }
    best
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn best_repeat_match_with_prefix_chain(
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    pos: usize,
    repeat_offsets: [u32; 3],
    literal_length: usize,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
) -> Option<MatchCandidate> {
    let [rep1, rep2, rep3] = repeat_offsets;
    let mut best = None;
    let current = prefix_chain.len() + pos;
    let literal_length_zero = literal_length == 0;

    for raw_offset in [
        Some(rep1),
        Some(rep2),
        Some(rep3),
        literal_length_zero.then_some(rep1.saturating_sub(1)),
    ]
    .into_iter()
    .flatten()
    {
        let raw_offset = raw_offset as usize;
        if raw_offset == 0 || raw_offset > current {
            continue;
        }

        let match_start = current - raw_offset;
        if !logical_match_start_is_valid(prefix_chain.len(), match_start, prefix_low, source_low) {
            continue;
        }
        let length = count_match_length_virtual(prefix_chain, src, match_start, current);
        if length >= params.min_match_length_zero_literals {
            best = choose_better_match(
                best,
                Some(MatchCandidate {
                    offset: raw_offset,
                    length,
                }),
                repeat_offsets,
                literal_length,
            );
        }
    }

    best
}

pub(crate) fn choose_better_match(
    current: Option<MatchCandidate>,
    candidate: Option<MatchCandidate>,
    repeat_offsets: [u32; 3],
    literal_length: usize,
) -> Option<MatchCandidate> {
    choose_better_match_with_adjustment(current, candidate, repeat_offsets, literal_length, 0)
}

pub(crate) fn choose_better_match_with_adjustment(
    current: Option<MatchCandidate>,
    candidate: Option<MatchCandidate>,
    repeat_offsets: [u32; 3],
    literal_length: usize,
    candidate_score_adjustment: i32,
) -> Option<MatchCandidate> {
    match (current, candidate) {
        (None, next) | (next, None) => next,
        (Some(current), Some(candidate)) => {
            let current_score = estimated_match_score_bits(current, repeat_offsets, literal_length);
            let candidate_score =
                estimated_match_score_bits(candidate, repeat_offsets, literal_length)
                    + candidate_score_adjustment;
            if candidate_score > current_score
                || (candidate_score == current_score
                    && (candidate.length > current.length
                        || (candidate.length == current.length
                            && candidate.offset < current.offset)))
            {
                Some(candidate)
            } else {
                Some(current)
            }
        }
    }
}

pub(crate) fn choose_better_regular_match(
    current: Option<MatchCandidate>,
    candidate: Option<MatchCandidate>,
    literal_length: usize,
) -> Option<MatchCandidate> {
    choose_better_regular_match_with_adjustment(current, candidate, literal_length, 0)
}

pub(crate) fn choose_better_regular_match_with_adjustment(
    current: Option<MatchCandidate>,
    candidate: Option<MatchCandidate>,
    literal_length: usize,
    candidate_score_adjustment: i32,
) -> Option<MatchCandidate> {
    match (current, candidate) {
        (None, next) | (next, None) => next,
        (Some(current), Some(candidate)) => {
            let current_score = estimated_regular_match_score_bits(current, literal_length);
            let candidate_score = estimated_regular_match_score_bits(candidate, literal_length)
                + candidate_score_adjustment;
            if candidate_score > current_score
                || (candidate_score == current_score
                    && (candidate.length > current.length
                        || (candidate.length == current.length
                            && candidate.offset < current.offset)))
            {
                Some(candidate)
            } else {
                Some(current)
            }
        }
    }
}

pub(crate) fn extend_back_source_candidate(
    src: &[u8],
    anchor: usize,
    pos: usize,
    candidate: MatchCandidate,
    window_low: usize,
) -> ExtendedMatch {
    let mut found = ExtendedMatch {
        start: pos,
        offset: candidate.offset,
        length: candidate.length,
    };
    while found.start > anchor
        && found.start > found.offset
        && found.start - found.offset > window_low
        && src[found.start - 1] == src[found.start - 1 - found.offset]
    {
        found.start -= 1;
        found.length += 1;
    }
    found
}

pub(crate) fn extend_back_prefix_chain_match(
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    anchor: usize,
    pos: usize,
    candidate: MatchCandidate,
    prefix_low: usize,
    source_low: usize,
) -> ExtendedMatch {
    let mut found = ExtendedMatch {
        start: pos,
        offset: candidate.offset,
        length: candidate.length,
    };
    let prefix_len = prefix_chain.len();
    let mut current = prefix_len + found.start;
    // C fixes this floor once, from the region the match itself starts in,
    // and never revisits it (`ZSTD_compressBlock_lazy_generic`):
    //
    //     const BYTE* const mStart = (matchIndex < prefixLowestIndex)
    //                              ? dictStart : prefixLowest;
    //     while (((start > anchor) & (match > mStart)) && (start[-1] == match[-1]))
    //
    // So a match found in the source stops at the source's own start, even
    // though the dictionary sits immediately below it and the bytes there may
    // well continue to agree. C cannot express that walk: `match` and `mStart`
    // are pointers into one of two buffers, chosen before the loop.
    //
    // This crate addresses the dictionary and the source through a single
    // index space, which makes the boundary invisible — and re-deciding the
    // region at each step, as this did, let the walk cross out of the source
    // and carry on into the dictionary. It cost exactly the bytes that agreed
    // across the join: one match per block ran two bytes long, moving two
    // bytes out of the literals, on every corpus at greedy through lazy2.
    let floor = if current.saturating_sub(found.offset) < prefix_len {
        prefix_low
    } else {
        prefix_len + source_low
    };
    while found.start > anchor
        && current > found.offset
        && current - 1 - found.offset >= floor
        && src[found.start - 1] == virtual_byte(prefix_chain, src, current - 1 - found.offset)
    {
        found.start -= 1;
        found.length += 1;
        current -= 1;
    }
    found
}

pub(crate) fn regular_match_source_for_prefix_mode(
    mode: PrefixMatchMode,
    start: usize,
    offset: usize,
) -> SequenceTraceMatchSource {
    if offset <= start {
        return SequenceTraceMatchSource::Source;
    }

    match mode {
        PrefixMatchMode::ExtDict => SequenceTraceMatchSource::Prefix,
        PrefixMatchMode::DictMatchState => SequenceTraceMatchSource::Dict,
    }
}

pub(crate) fn estimated_match_score_bits(
    candidate: MatchCandidate,
    repeat_offsets: [u32; 3],
    literal_length: usize,
) -> i32 {
    let literal_length_u32 = literal_length.min(u32::MAX as usize) as u32;
    let repeat_offsets = RepeatOffsets::from_values(repeat_offsets);
    let sequence_cost = estimate_sequence_bit_cost(
        candidate.offset as u32,
        literal_length_u32,
        candidate.length as u32,
        repeat_offsets,
    )
    .unwrap_or(32);
    let literal_cost = literal_length_u32.saturating_mul(5);
    candidate.length as i32 * 8 - sequence_cost as i32 - literal_cost as i32
}

pub(crate) fn estimated_regular_match_score_bits(
    candidate: MatchCandidate,
    literal_length: usize,
) -> i32 {
    let literal_length_u32 = literal_length.min(u32::MAX as usize) as u32;
    let offset_bits = ((candidate.offset as u32).saturating_add(3)).ilog2();
    let literal_cost = literal_length_u32.saturating_mul(5);
    let sequence_cost = offset_bits + 22;
    candidate.length as i32 * 8 - sequence_cost as i32 - literal_cost as i32
}

#[inline(always)]
pub(crate) fn repeat_match_without_prefix_at(
    src: &[u8],
    pos: usize,
    raw_offset: usize,
    window_low: usize,
) -> Option<MatchCandidate> {
    if raw_offset == 0 || raw_offset > pos || pos + MIN_MATCH > src.len() {
        return None;
    }

    let match_start = pos - raw_offset;
    if match_start < window_low {
        return None;
    }
    // SAFETY: match_start < pos (raw_offset > 0), pos + 4 <= src.len() checked above.
    #[allow(unsafe_code)]
    if !unsafe { match_prefix_4bytes(src, match_start, pos) } {
        return None;
    }
    let length = if pos + MIN_MATCH < src.len() {
        #[allow(unsafe_code)]
        // SAFETY: match_start + 4 < pos + 4 < src.len()
        {
            MIN_MATCH
                + unsafe {
                    count_match_length_unchecked(src, match_start + MIN_MATCH, pos + MIN_MATCH)
                }
        }
    } else {
        MIN_MATCH
    };

    Some(MatchCandidate {
        offset: raw_offset,
        length,
    })
}

pub(crate) fn repeat_match_with_prefix_chain_at(
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    pos: usize,
    raw_offset: usize,
    prefix_low: usize,
    source_low: usize,
) -> Option<MatchCandidate> {
    let current = prefix_chain.len() + pos;
    if raw_offset == 0 || raw_offset > current || pos + MIN_MATCH > src.len() {
        return None;
    }

    let match_start = current - raw_offset;
    if !logical_match_start_is_valid(prefix_chain.len(), match_start, prefix_low, source_low)
        || !virtual_match_has_length(prefix_chain, src, match_start, current, MIN_MATCH)
    {
        return None;
    }

    Some(MatchCandidate {
        offset: raw_offset,
        length: count_match_length_virtual(prefix_chain, src, match_start, current),
    })
}

pub(crate) fn best_regular_match_with_prefix_chain(
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    pos: usize,
    literal_length: usize,
    params: MatchFinderParameters,
    prefix_low: usize,
    source_low: usize,
    mode: PrefixMatchMode,
    prefix_finder: &impl LazySearchFinder,
    src_finder: &mut impl LazySearchFinder,
) -> Option<MatchCandidate> {
    // In C, ZSTD_BtFindBestMatch gates BOTH source and dictionary searches
    // behind the nextToUpdate skip check.  When the source finder would skip
    // (DUBT optimization), skip the dictionary search as well.
    if src_finder.would_skip(pos) {
        return src_finder.find_match_with_prefix(
            prefix_chain,
            src,
            pos,
            params,
            literal_length,
            prefix_low,
            source_low,
        );
    }
    // In C's ExtDict BinaryTree mode (ZSTD_DUBT_findBestMatch with
    // dictMode=extDict), the DUBT only searches the source tree.  There is
    // no ZSTD_DUBT_findBetterDictMatch call for extDict — only for
    // dictMatchState.  Skip the prefix search when the source finder
    // indicates it does not include prefix entries (BinaryTree case).
    let prefix_candidate =
        if mode == PrefixMatchMode::ExtDict && src_finder.skips_prefix_regular_search() {
            None
        } else {
            prefix_finder.find_prefix_chain_match(
                prefix_chain,
                src,
                pos,
                params,
                literal_length,
                prefix_low,
            )
        };
    let source_candidate = src_finder.find_match_with_prefix(
        prefix_chain,
        src,
        pos,
        params,
        literal_length,
        prefix_low,
        source_low,
    );
    let mut best = match mode {
        PrefixMatchMode::DictMatchState => source_candidate,
        PrefixMatchMode::ExtDict => prefix_candidate,
    };
    match mode {
        PrefixMatchMode::DictMatchState => {
            // C's ZSTD_DUBT_findBetterDictMatch: the dictionary match must be
            // strictly longer than the source match AND pass the 4x DUBT cost
            // test.  This prevents shorter dictionary matches at better offsets
            // from replacing a longer source match.
            if let Some(dict_candidate) = prefix_candidate {
                let src_len = source_candidate.map_or(0, |c| c.length);
                if dict_candidate.length > src_len {
                    let src_offbase = source_candidate
                        .map_or(999_999_999u32, |c| (c.offset as u32).saturating_add(3));
                    let gain = 4 * (dict_candidate.length as i32 - src_len as i32);
                    let cost = highbit32((dict_candidate.offset as u32) + 1) as i32
                        - highbit32(src_offbase) as i32;
                    if gain > cost {
                        best = Some(dict_candidate);
                    }
                } else if source_candidate.is_none() {
                    best = Some(dict_candidate);
                }
            }
        }
        PrefixMatchMode::ExtDict => {
            if let Some(candidate) = source_candidate {
                best = choose_better_regular_match_with_adjustment(
                    best,
                    Some(candidate),
                    literal_length,
                    -params.source_score_penalty_with_prefix,
                );
            }
        }
    }
    best
}

pub(crate) fn lazy_repeat_match_improves(
    current: LazyParserMatch,
    candidate: MatchCandidate,
    multiplier: i32,
) -> bool {
    let gain2 = candidate.length as i32 * multiplier;
    let gain1 = current.length as i32 * multiplier - highbit32(current.offbase()) as i32 + 1;
    gain2 > gain1
}

pub(crate) fn lazy_regular_match_improves(
    current: LazyParserMatch,
    candidate: MatchCandidate,
    current_bias: i32,
) -> bool {
    let candidate_offbase = (candidate.offset.min(u32::MAX as usize) as u32).saturating_add(3);
    let gain2 = candidate.length as i32 * 4 - highbit32(candidate_offbase) as i32;
    let gain1 = current.length as i32 * 4 - highbit32(current.offbase()) as i32 + current_bias;
    gain2 > gain1
}

#[inline(always)]
pub(crate) fn store_lazy_sequence(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    repeat_offsets: &mut RepeatOffsets,
    start: usize,
    raw_offset: usize,
    match_length: usize,
) -> Result<()> {
    store_lazy_sequence_with_source(
        plan,
        src,
        anchor,
        repeat_offsets,
        start,
        raw_offset,
        match_length,
        SequenceTraceMatchSource::Unknown,
    )
}

/// Push a sequence without any tracing overhead. No `tracing_enabled()` check,
/// no `SequenceTraceMatchSource` parameter. Used by the Fast/DoubleFast
/// parsers where tracing is never enabled in the benchmark path.
///
/// Also computes and pushes FSE codes (ll_code, of_code, ml_code) inline,
/// matching C zstd's `ZSTD_storeSeq` pattern of populating code tables
/// during sequence planning. This eliminates the separate
/// `prepare_seq_store_encode_scratch` pass.
///
/// Fast literal copy: when `lit_len <= 16` AND sufficient capacity exists,
/// a single 16-byte copy (matching C's `ZSTD_copy16`) replaces the
/// exact-length `memcpy`. For longer literals, falls back to
/// `copy_nonoverlapping`.
/// How far past a literal run the fixed-width copy below may read and write.
/// Named after C's constant of the same value, which bounds the same overshoot.
pub(crate) const WILDCOPY_OVERLENGTH: usize = 32;

/// Whether a literal run can be copied with the fixed 16-byte `ZSTD_copy16`
/// rather than an exact-length `memcpy`.
///
/// Both ends have to have room. The destination is the easy one: the literals
/// buffer is allocated with headroom, so writing 16 bytes for a shorter run only
/// dirties bytes past `set_len`. The *source* is the caller's buffer and has no
/// headroom at all, so a run that ends within 16 bytes of it must not be read
/// this way — that was reading up to 16 bytes past the end of whatever slice the
/// caller handed to `encode_all`.
///
/// C draws the same line at `litLimit_w = litLimit - WILDCOPY_OVERLENGTH` in
/// `ZSTD_storeSeq` and falls back to `ZSTD_safecopyLiterals` past it. Using the
/// full overlength rather than the 16 bytes actually read keeps the two in step,
/// and it is the bound the wildcopy for longer runs would need anyway.
#[inline(always)]
pub(crate) fn wildcopy_literals_fits(
    src: &[u8],
    literals_end: usize,
    literal_length: usize,
    new_literals_len: usize,
    literals_capacity: usize,
) -> bool {
    // Only runs of at most 16 bytes are copied this way; longer ones take the
    // exact-length path rather than C's follow-on wildcopy.
    literal_length <= 16
        && literals_end + WILDCOPY_OVERLENGTH <= src.len()
        && new_literals_len + 16 <= literals_capacity
}

#[inline(always)]
#[allow(unsafe_code)]
pub(crate) fn push_lazy_sequence_no_trace(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    sequence: SequenceCommand,
) {
    let lit_len = sequence.literal_length as usize;
    let start = *anchor + lit_len;
    let new_len = plan.literals.len() + lit_len;
    if new_len <= plan.literals.capacity() {
        // SAFETY: capacity is sufficient (pre-allocated in reset_for_block
        // with WILDCOPY_OVERLENGTH headroom), and *anchor + lit_len <=
        // src.len() (guaranteed by caller).
        unsafe {
            let dst = plan.literals.as_mut_ptr().add(plan.literals.len());
            let s = src.as_ptr().add(*anchor);
            if wildcopy_literals_fits(src, start, lit_len, new_len, plan.literals.capacity()) {
                // Single 16-byte copy matching C's ZSTD_copy16 pattern.
                // Over-writes past logical end are safe because capacity
                // includes WILDCOPY_OVERLENGTH headroom. set_len only
                // covers the exact bytes.
                core::ptr::copy_nonoverlapping(s, dst, 16);
            } else {
                core::ptr::copy_nonoverlapping(s, dst, lit_len);
            }
            plan.literals.set_len(new_len);
        }
    } else {
        plan.literals.extend_from_slice(&src[*anchor..start]);
    }
    // Compute and push FSE codes + sequence command inline with direct pointer
    // writes.  Capacity is guaranteed by reset_for_block which reserves
    // block_len / MIN_MATCH for all four Vecs.
    unsafe {
        let idx = plan.sequences.len();
        debug_assert!(idx < plan.sequences.capacity());
        debug_assert!(idx < plan.literal_codes.capacity());
        debug_assert!(idx < plan.offset_codes.capacity());
        debug_assert!(idx < plan.match_codes.capacity());
        core::ptr::write(plan.sequences.as_mut_ptr().add(idx), sequence);
        core::ptr::write(
            plan.literal_codes.as_mut_ptr().add(idx),
            literal_length_code_unchecked(sequence.literal_length),
        );
        core::ptr::write(
            plan.offset_codes.as_mut_ptr().add(idx),
            offset_code_unchecked(sequence.offset_value),
        );
        core::ptr::write(
            plan.match_codes.as_mut_ptr().add(idx),
            match_length_code_unchecked(sequence.match_length),
        );
        plan.sequences.set_len(idx + 1);
        plan.literal_codes.set_len(idx + 1);
        plan.offset_codes.set_len(idx + 1);
        plan.match_codes.set_len(idx + 1);
    }
    *anchor = start + sequence.match_length as usize;
}

#[inline(always)]
#[allow(unsafe_code)]
pub(crate) fn push_lazy_sequence_with_source(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    sequence: SequenceCommand,
    match_source: SequenceTraceMatchSource,
) {
    let lit_len = sequence.literal_length as usize;
    let start = *anchor + lit_len;
    let new_len = plan.literals.len() + lit_len;
    if new_len <= plan.literals.capacity() {
        unsafe {
            let dst = plan.literals.as_mut_ptr().add(plan.literals.len());
            let s = src.as_ptr().add(*anchor);
            if wildcopy_literals_fits(src, start, lit_len, new_len, plan.literals.capacity()) {
                core::ptr::copy_nonoverlapping(s, dst, 16);
            } else {
                core::ptr::copy_nonoverlapping(s, dst, lit_len);
            }
            plan.literals.set_len(new_len);
        }
    } else {
        plan.literals.extend_from_slice(&src[*anchor..start]);
    }
    plan.sequences.push(sequence);
    plan.literal_codes
        .push(literal_length_code_unchecked(sequence.literal_length));
    plan.offset_codes
        .push(offset_code_unchecked(sequence.offset_value));
    plan.match_codes
        .push(match_length_code_unchecked(sequence.match_length));
    if plan.tracing_enabled() {
        plan.trace_match_sources.push(match_source);
    }
    *anchor = start + sequence.match_length as usize;
}

#[inline(always)]
pub(crate) fn store_lazy_sequence_with_source(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    repeat_offsets: &mut RepeatOffsets,
    start: usize,
    raw_offset: usize,
    match_length: usize,
    match_source: SequenceTraceMatchSource,
) -> Result<()> {
    if !plan.tracing_enabled() {
        let literal_length = start - *anchor;
        let offset_value = repeat_offsets.encode_offset_value_and_update(
            raw_offset.min(u32::MAX as usize) as u32,
            literal_length.min(u32::MAX as usize) as u32,
        );
        push_lazy_sequence_with_source(
            plan,
            src,
            anchor,
            SequenceCommand {
                literal_length: literal_length.min(u32::MAX as usize) as u32,
                offset_value,
                match_length: match_length.min(u32::MAX as usize) as u32,
            },
            match_source,
        );
        return Ok(());
    }

    let literal_length = start - *anchor;
    let anchor_before = *anchor;
    let [offset_1_before, offset_2_before, _] = repeat_offsets.values();
    let offset_value = repeat_offsets.encode_offset_value_and_update(
        raw_offset.min(u32::MAX as usize) as u32,
        literal_length.min(u32::MAX as usize) as u32,
    );
    let [offset_1_after, offset_2_after, _] = repeat_offsets.values();
    push_lazy_sequence_with_source(
        plan,
        src,
        anchor,
        SequenceCommand {
            literal_length: literal_length.min(u32::MAX as usize) as u32,
            offset_value,
            match_length: match_length.min(u32::MAX as usize) as u32,
        },
        match_source,
    );
    trace_lazy_emission(
        plan,
        if offset_value <= 3 {
            SequenceTraceEmissionKind::Rep
        } else {
            SequenceTraceEmissionKind::Regular
        },
        match_source,
        anchor_before,
        start,
        match_length,
        offset_value,
        raw_offset,
        offset_1_before as usize,
        offset_2_before as usize,
        offset_1_after as usize,
        offset_2_after as usize,
    );
    Ok(())
}

#[inline(always)]
pub(crate) fn store_lazy_sequence_with_offset_value_and_source(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    repeat_offsets: &mut RepeatOffsets,
    start: usize,
    offset_value: u32,
    match_length: usize,
    match_source: SequenceTraceMatchSource,
) -> Result<u32> {
    let literal_length = (start - *anchor).min(u32::MAX as usize) as u32;
    let sequence = SequenceCommand {
        literal_length,
        offset_value,
        match_length: match_length.min(u32::MAX as usize) as u32,
    };
    let raw_offset = repeat_offsets.resolve_encode(literal_length, offset_value);
    push_lazy_sequence_with_source(plan, src, anchor, sequence, match_source);
    Ok(raw_offset)
}

#[inline(always)]
pub(crate) fn store_lazy_regular_sequence_with_source(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    repeat_offsets: &mut RepeatOffsets,
    start: usize,
    raw_offset: usize,
    match_length: usize,
    match_source: SequenceTraceMatchSource,
) -> Result<u32> {
    if !plan.tracing_enabled() {
        let raw_offset_u32 = raw_offset.min(u32::MAX as usize) as u32;
        let off_base = repeat_offsets.update_explicit_offset(raw_offset_u32);
        let literal_length = start - *anchor;
        push_lazy_sequence_with_source(
            plan,
            src,
            anchor,
            SequenceCommand {
                literal_length: literal_length.min(u32::MAX as usize) as u32,
                offset_value: off_base,
                match_length: match_length.min(u32::MAX as usize) as u32,
            },
            match_source,
        );
        return Ok(raw_offset_u32);
    }

    let anchor_before = *anchor;
    let [offset_1_before, offset_2_before, _] = repeat_offsets.values();
    let off_base = explicit_offbase(raw_offset);
    let raw_offset = store_lazy_sequence_with_offset_value_and_source(
        plan,
        src,
        anchor,
        repeat_offsets,
        start,
        off_base,
        match_length,
        match_source,
    )?;
    let [offset_1_after, offset_2_after, _] = repeat_offsets.values();
    trace_lazy_emission(
        plan,
        SequenceTraceEmissionKind::Regular,
        match_source,
        anchor_before,
        start,
        match_length,
        off_base,
        raw_offset as usize,
        offset_1_before as usize,
        offset_2_before as usize,
        offset_1_after as usize,
        offset_2_after as usize,
    );
    Ok(raw_offset)
}

pub(crate) fn store_lazy_zero_literal_rep2_with_source(
    plan: &mut SequencePlan,
    src: &[u8],
    anchor: &mut usize,
    repeat_offsets: &mut RepeatOffsets,
    match_length: usize,
    match_source: SequenceTraceMatchSource,
) -> Result<u32> {
    if !plan.tracing_enabled() {
        let raw_offset = repeat_offsets.resolve_zero_literal_rep2_encode();
        push_lazy_sequence_with_source(
            plan,
            src,
            anchor,
            SequenceCommand {
                literal_length: 0,
                offset_value: 1,
                match_length: match_length.min(u32::MAX as usize) as u32,
            },
            match_source,
        );
        return Ok(raw_offset);
    }

    let anchor_before = *anchor;
    let [offset_1_before, offset_2_before, _] = repeat_offsets.values();
    let raw_offset = repeat_offsets.resolve_zero_literal_rep2_encode();
    let [offset_1_after, offset_2_after, _] = repeat_offsets.values();
    push_lazy_sequence_with_source(
        plan,
        src,
        anchor,
        SequenceCommand {
            literal_length: 0,
            offset_value: 1,
            match_length: match_length.min(u32::MAX as usize) as u32,
        },
        match_source,
    );
    trace_lazy_emission(
        plan,
        SequenceTraceEmissionKind::Rep,
        match_source,
        anchor_before,
        anchor_before,
        match_length,
        1,
        raw_offset as usize,
        offset_1_before as usize,
        offset_2_before as usize,
        offset_1_after as usize,
        offset_2_after as usize,
    );
    Ok(raw_offset)
}

pub(crate) fn explicit_offbase(raw_offset: usize) -> u32 {
    (raw_offset.min(u32::MAX as usize) as u32).saturating_add(3)
}

pub(crate) fn trace_lazy_emission(
    plan: &mut SequencePlan,
    kind: SequenceTraceEmissionKind,
    source: SequenceTraceMatchSource,
    anchor_before: usize,
    start: usize,
    match_length: usize,
    off_base: u32,
    raw_offset: usize,
    offset_1_before: usize,
    offset_2_before: usize,
    offset_1_after: usize,
    offset_2_after: usize,
) {
    if !plan.tracing_enabled() {
        return;
    }
    plan.trace_emissions.push(SequenceTraceEmission {
        kind,
        source,
        anchor_before,
        start,
        literal_length: start.saturating_sub(anchor_before),
        match_length,
        off_base,
        raw_offset,
        offset_1_before,
        offset_2_before,
        offset_1_after,
        offset_2_after,
    });
}

pub(crate) fn row_dict_match_state_offsets_synced(
    repeat_offsets: RepeatOffsets,
    offset_1: usize,
    offset_2: usize,
) -> bool {
    let [rep1, rep2, _] = repeat_offsets.values();
    rep1 as usize == offset_1 && rep2 as usize == offset_2
}

pub(crate) fn skip_after_no_match(
    anchor: usize,
    pos: usize,
    params: MatchFinderParameters,
) -> usize {
    ((pos.saturating_sub(anchor)) >> params.skip_search_strength) + 1
}

#[inline(always)]
pub(crate) fn fast_step_increment(params: MatchFinderParameters) -> usize {
    let shift = params
        .skip_search_strength
        .saturating_sub(1)
        .min(usize::BITS.saturating_sub(1));
    1usize << shift
}

pub(crate) fn regular_match_length_threshold(
    literal_length: usize,
    params: MatchFinderParameters,
) -> usize {
    let threshold = if literal_length == 0 {
        params.min_match_length_zero_literals
    } else {
        params.min_match_length_after_literals
    };
    threshold.max(MIN_MATCH)
}

/// Check rep offset validity and count match length in one pass.
/// Returns 0 if the rep offset is invalid, otherwise returns the match length.
/// Uses a cheap 4-byte prefix comparison before full counting, matching C
/// zstd's pattern: `MEM_read32(ip+1-offset_1) == MEM_read32(ip+1)` before
/// calling `ZSTD_count`.
#[inline(always)]
pub(crate) fn count_rep_match_length(
    src: &[u8],
    pos: usize,
    rep_offset: usize,
    window_low: usize,
) -> usize {
    if rep_offset == 0 || pos < rep_offset || pos - rep_offset < window_low || pos + 4 > src.len() {
        return 0;
    }
    let match_start = pos - rep_offset;
    // SAFETY: match_start = pos - rep_offset < pos, so match_start + 4 < pos + 4 <= src.len().
    #[allow(unsafe_code)]
    if !unsafe { match_prefix_4bytes(src, match_start, pos) } {
        return 0;
    }
    // 4-byte prefix matches — count remaining bytes from offset 4.
    if pos + 4 < src.len() {
        #[allow(unsafe_code)]
        // SAFETY: match_start + 4 < pos + 4 < src.len()
        return 4 + unsafe { count_match_length_unchecked(src, match_start + 4, pos + 4) };
    }
    4
}

#[inline(always)]
pub(crate) fn count_match_length(src: &[u8], left: usize, right: usize) -> usize {
    // Hot-path callers guarantee left < right and right < src.len(), so
    // max_len = src.len() - right. Use the unsafe pointer-based path when
    // these preconditions hold.
    if left < right && right < src.len() {
        #[allow(unsafe_code)]
        // SAFETY: left < right < src.len(), so both pointers are in bounds
        // and max_len = src.len() - right is correct.
        unsafe {
            return count_match_length_unsafe(src.as_ptr(), left, right, src.len() - right);
        }
    }
    let max_len = src
        .len()
        .saturating_sub(right)
        .min(src.len().saturating_sub(left));
    count_match_length_slices(&src[left..left + max_len], &src[right..right + max_len])
}

/// Check whether the first 4 bytes at `left` and `right` in `src` match.
/// Used as a cheap pre-filter before full match-length counting, matching
/// C zstd's `matchFound` pattern which does `MEM_read32` comparison before
/// calling `ZSTD_count`.
///
/// # Safety
///
/// `left + 4 <= src.len()` and `right + 4 <= src.len()` must both hold.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn match_prefix_4bytes(src: &[u8], left: usize, right: usize) -> bool {
    debug_assert!(left + 4 <= src.len() && right + 4 <= src.len());
    let base = src.as_ptr();
    // SAFETY: the caller guarantees four readable bytes at each of `left` and
    // `right`, so both offsets stay inside `src`. Unaligned reads, so `src`
    // carries no alignment requirement.
    let l = unsafe { core::ptr::read_unaligned(base.add(left) as *const u32) };
    let r = unsafe { core::ptr::read_unaligned(base.add(right) as *const u32) };
    l == r
}

/// Branchless match candidate validation + 8-byte comparison, matching C
/// zstd's `ZSTD_selectAddr` + `MEM_read64` pattern. When the candidate is
/// invalid, reads from a dummy address (index 0) instead of branching,
/// compiling the validity check to a CMOV. The combined boolean then uses
/// a single predictable branch (mostly-miss for the content comparison).
///
/// Returns (true, candidate) if both validity and 8-byte prefix match pass.
///
/// # Safety
///
/// `ip + 8 <= src.len()` must hold, and `src.len() >= 8` — the latter because
/// an invalid candidate is replaced by index 0, which is then read from.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn check_long_match_branchless(
    src: &[u8],
    candidate: usize,
    ip: usize,
    low: usize,
) -> bool {
    let valid = candidate.wrapping_sub(low) < ip.wrapping_sub(low);
    // Use index 0 as dummy when invalid. This is always a valid index
    // (search_limit check ensures src.len() >= 8). The read at index 0
    // will almost never match ip's data, so the combined check fails fast.
    let safe_candidate = if valid { candidate } else { 0 };
    // SAFETY: `ip + 8 <= src.len()` by this function's contract. For the left
    // side, `safe_candidate` is either a candidate the `valid` test just placed
    // below `ip`, or 0; both admit an 8-byte read given `src.len() >= 8`. The
    // substitution is what makes this branchless, and it is also what makes the
    // read unconditionally in bounds.
    let matches = unsafe { match_prefix_8bytes(src, safe_candidate, ip) };
    // Keep logical AND: for the 8-byte long match, LLVM's branch on `valid`
    // skips two 8-byte loads when the candidate is invalid — common in
    // incompressible regions. Measured better than branchless for mixed-entropy.
    //
    // Re-measured 2026-08-26 against both branchless forms, interleaved
    // best-of-5 on an idle machine over eight level 3-4 rows: bitwise `&`, and
    // bitwise `&` reading a hot 16-byte static in place of index 0 — which is
    // what C's `dummy[]` in `ZSTD_compressBlock_doubleFast_noDict_generic` is
    // for. Both lost by 1.9% mean, so the fallback's cache temperature is not
    // what decides this; the skipped loads are. Do not "fix" this to match C.
    matches && valid
}

/// Same pattern for 4-byte (short) matches.
///
/// # Safety
///
/// `ip + 4 <= src.len()` must hold, and `src.len() >= 4` — the latter because
/// an invalid candidate is replaced by index 0, which is then read from.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn check_short_match_branchless(
    src: &[u8],
    candidate: usize,
    ip: usize,
    low: usize,
) -> bool {
    let valid = candidate.wrapping_sub(low) < ip.wrapping_sub(low);
    let safe_candidate = if valid { candidate } else { 0 };
    // SAFETY: as in `check_long_match_branchless`, one size down.
    let matches = unsafe { match_prefix_4bytes(src, safe_candidate, ip) };
    // Use bitwise AND to prevent LLVM from short-circuiting into a branch.
    matches & valid
}

/// Check whether the first 8 bytes at `left` and `right` in `src` match.
/// Used for DoubleFast long-match validation (min match length 8).
///
/// # Safety
///
/// `left + 8 <= src.len()` and `right + 8 <= src.len()` must both hold.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn match_prefix_8bytes(src: &[u8], left: usize, right: usize) -> bool {
    debug_assert!(left + 8 <= src.len() && right + 8 <= src.len());
    let base = src.as_ptr();
    // SAFETY: the caller guarantees eight readable bytes at each of `left` and
    // `right`, so both offsets stay inside `src`. Unaligned reads, so `src`
    // carries no alignment requirement.
    let l = unsafe { core::ptr::read_unaligned(base.add(left) as *const u64) };
    let r = unsafe { core::ptr::read_unaligned(base.add(right) as *const u64) };
    l == r
}

/// Cheap 4-byte prefix match check across a logical prefix || src buffer.
/// `left` and `right` are logical positions in the virtual buffer.
/// `right` must be >= prefix.len() (i.e. in the src portion).
#[inline(always)]
pub(crate) fn match_prefix_4bytes_logical(
    prefix: &[u8],
    src: &[u8],
    left: usize,
    right: usize,
) -> bool {
    let prefix_len = prefix.len();
    let right_src = right - prefix_len;
    if right_src + 4 > src.len() {
        return false;
    }
    let right_word = crate::entropy::mem::read_u32(src, right_src);
    if left >= prefix_len {
        let left_src = left - prefix_len;
        left_src + 4 <= src.len() && crate::entropy::mem::read_u32(src, left_src) == right_word
    } else if left + 4 <= prefix_len {
        crate::entropy::mem::read_u32(prefix, left) == right_word
    } else {
        // Spans prefix/src boundary — byte-by-byte fallback (very rare).
        for i in 0..4 {
            let left_byte = if left + i < prefix_len {
                prefix[left + i]
            } else {
                src[left + i - prefix_len]
            };
            if left_byte != src[right_src + i] {
                return false;
            }
        }
        true
    }
}

/// Cheap 8-byte prefix match check across a logical prefix || src buffer.
/// Used for DoubleFast long-match validation in dictionary paths.
#[inline(always)]
pub(crate) fn match_prefix_8bytes_logical(
    prefix: &[u8],
    src: &[u8],
    left: usize,
    right: usize,
) -> bool {
    let prefix_len = prefix.len();
    let right_src = right - prefix_len;
    if right_src + 8 > src.len() {
        return false;
    }
    let right_word = crate::entropy::mem::read_u64(src, right_src);
    if left >= prefix_len {
        let left_src = left - prefix_len;
        left_src + 8 <= src.len() && crate::entropy::mem::read_u64(src, left_src) == right_word
    } else if left + 8 <= prefix_len {
        crate::entropy::mem::read_u64(prefix, left) == right_word
    } else {
        // Spans prefix/src boundary — byte-by-byte fallback (very rare).
        for i in 0..8 {
            let left_byte = if left + i < prefix_len {
                prefix[left + i]
            } else {
                src[left + i - prefix_len]
            };
            if left_byte != src[right_src + i] {
                return false;
            }
        }
        true
    }
}

/// Like `count_match_length` but skips the precondition checks.
///
/// # Safety
///
/// `left < right && right <= src.len()` must hold. `right == src.len()` is the
/// boundary case a parser reaches when its match runs to the last byte of the
/// buffer: the length passed on is then zero, so nothing is read and the answer
/// is zero. Upstream's `ZSTD_count` accepts `pIn == pInLimit` for the same
/// reason. The parsers hold `ip <= src.len() - 8` and then look eight bytes
/// ahead, so equality is reachable and the strict `<` this used to require was
/// one byte tighter than either the callers or the arithmetic below.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn count_match_length_unchecked(src: &[u8], left: usize, right: usize) -> usize {
    debug_assert!(
        left < right && right <= src.len(),
        "count_match_length_unchecked precondition: left={left} right={right} len={}",
        src.len()
    );
    // SAFETY: `right <= src.len()` makes `src.len() - right` the exact count of
    // bytes readable from `right`, and `left < right` makes that a lower bound
    // for `left` too, so the `max_len` handed on is readable from both.
    unsafe { count_match_length_unsafe(src.as_ptr(), left, right, src.len() - right) }
}

/// Pointer-based match length counting. Avoids slice creation and bounds
/// checks.
///
/// # Safety
///
/// `left` and `right` must be valid offsets into the allocation `base` points
/// into, and `max_len` bytes must be readable from both `base + left` and
/// `base + right`. Note that this is a plain readability requirement rather
/// than a match-length one: the function reads up to `max_len` bytes on each
/// side whatever the data says, stopping early only on a mismatch.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn count_match_length_unsafe(
    base: *const u8,
    left: usize,
    right: usize,
    max_len: usize,
) -> usize {
    // SAFETY: the caller guarantees `max_len` readable bytes from each of
    // `base + left` and `base + right`. Every read below is guarded by a
    // `pos + width <= max_len` test evaluated before the read, so no access
    // passes either end. The reads are unaligned, so `base` carries no
    // alignment requirement.
    unsafe {
        let left_ptr = base.add(left);
        let right_ptr = base.add(right);
        let mut pos = 0;
        // 16-byte main loop: compare two 8-byte words per iteration to reduce
        // loop overhead and improve throughput for long matches. Matches C zstd's
        // `ZSTD_count` pattern of maximising comparison bandwidth.
        while pos + 16 <= max_len {
            let l0 = core::ptr::read_unaligned(left_ptr.add(pos) as *const u64);
            let r0 = core::ptr::read_unaligned(right_ptr.add(pos) as *const u64);
            if l0 != r0 {
                let diff = l0 ^ r0;
                return pos + mismatch_byte_offset(diff);
            }
            let l1 = core::ptr::read_unaligned(left_ptr.add(pos + 8) as *const u64);
            let r1 = core::ptr::read_unaligned(right_ptr.add(pos + 8) as *const u64);
            if l1 != r1 {
                let diff = l1 ^ r1;
                return pos + 8 + mismatch_byte_offset(diff);
            }
            pos += 16;
        }
        // Handle the 8-byte remainder if present.
        if pos + 8 <= max_len {
            let l0 = core::ptr::read_unaligned(left_ptr.add(pos) as *const u64);
            let r0 = core::ptr::read_unaligned(right_ptr.add(pos) as *const u64);
            if l0 != r0 {
                let diff = l0 ^ r0;
                return pos + mismatch_byte_offset(diff);
            }
            pos += 8;
        }
        // Binary tail: 4-byte, 2-byte, 1-byte steps matching C's pattern.
        if pos + 4 <= max_len
            && core::ptr::read_unaligned(left_ptr.add(pos) as *const u32)
                == core::ptr::read_unaligned(right_ptr.add(pos) as *const u32)
        {
            pos += 4;
        }
        if pos + 2 <= max_len
            && core::ptr::read_unaligned(left_ptr.add(pos) as *const u16)
                == core::ptr::read_unaligned(right_ptr.add(pos) as *const u16)
        {
            pos += 2;
        }
        if pos < max_len && *left_ptr.add(pos) == *right_ptr.add(pos) {
            pos += 1;
        }
        pos
    }
}

/// Return the byte offset of the first differing byte in a non-zero XOR diff.
#[inline(always)]
const fn mismatch_byte_offset(diff: u64) -> usize {
    if cfg!(target_endian = "little") {
        diff.trailing_zeros() as usize / 8
    } else {
        diff.leading_zeros() as usize / 8
    }
}

/// Count matching bytes between `src[left..]` and `src[right..]`, starting the
/// comparison from byte offset `start` (skipping already-known-equal bytes).
/// Returns the total number of matching bytes (including `start`).
#[allow(dead_code)]
#[inline(always)]
pub(crate) fn count_match_length_from(
    src: &[u8],
    left: usize,
    right: usize,
    start: usize,
) -> usize {
    if left < right && right < src.len() {
        let max_len = src.len() - right;
        if start >= max_len {
            return max_len;
        }
        #[allow(unsafe_code)]
        // SAFETY: left < right < src.len(), left+start and right+start are
        // within bounds because start < max_len = src.len() - right.
        unsafe {
            return start
                + count_match_length_unsafe(
                    src.as_ptr(),
                    left + start,
                    right + start,
                    max_len - start,
                );
        }
    }
    let max_len = src
        .len()
        .saturating_sub(right)
        .min(src.len().saturating_sub(left));
    if start >= max_len {
        return max_len;
    }
    start
        + count_match_length_slices(
            &src[left + start..left + max_len],
            &src[right + start..right + max_len],
        )
}

#[inline(always)]
pub(crate) fn count_match_length_virtual(
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    left: usize,
    right: usize,
) -> usize {
    let prefix_len = prefix_chain.len();
    let virtual_len = prefix_len + src.len();
    let max_len = virtual_len
        .saturating_sub(right)
        .min(virtual_len.saturating_sub(left));

    // Fast path: both positions are in the source segment.
    if left >= prefix_len && right >= prefix_len {
        let left_src = left - prefix_len;
        let right_src = right - prefix_len;
        let left_remaining = &src[left_src..src.len().min(left_src + max_len)];
        let right_remaining = &src[right_src..src.len().min(right_src + max_len)];
        return count_match_length_slices(left_remaining, right_remaining);
    }

    let mut matched = 0usize;
    while matched < max_len {
        let left_slice = prefix_chain.slice(src, left + matched);
        let right_slice = prefix_chain.slice(src, right + matched);
        let left_bytes = left_slice.as_slice();
        let right_bytes = right_slice.as_slice();
        let chunk_len = left_bytes
            .len()
            .min(right_bytes.len())
            .min(max_len - matched);
        let chunk_match =
            count_match_length_slices(&left_bytes[..chunk_len], &right_bytes[..chunk_len]);
        matched += chunk_match;
        if chunk_match != chunk_len {
            break;
        }
    }
    matched
}

/// Count matching virtual bytes between positions `left` and `right`, starting
/// the comparison from byte offset `start`. Returns the total number of matching
/// bytes (including `start`).
#[allow(dead_code)]
#[inline(always)]
pub(crate) fn count_match_length_virtual_from(
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    left: usize,
    right: usize,
    start: usize,
) -> usize {
    let prefix_len = prefix_chain.len();
    let virtual_len = prefix_len + src.len();
    let max_len = virtual_len
        .saturating_sub(right)
        .min(virtual_len.saturating_sub(left));

    if start >= max_len {
        return max_len;
    }

    // Fast path: both positions (from start) are in the source segment.
    if left + start >= prefix_len && right + start >= prefix_len {
        let left_src = left + start - prefix_len;
        let right_src = right + start - prefix_len;
        let remaining = max_len - start;
        let left_remaining = &src[left_src..src.len().min(left_src + remaining)];
        let right_remaining = &src[right_src..src.len().min(right_src + remaining)];
        return start + count_match_length_slices(left_remaining, right_remaining);
    }

    let mut matched = start;
    while matched < max_len {
        let left_slice = prefix_chain.slice(src, left + matched);
        let right_slice = prefix_chain.slice(src, right + matched);
        let left_bytes = left_slice.as_slice();
        let right_bytes = right_slice.as_slice();
        let chunk_len = left_bytes
            .len()
            .min(right_bytes.len())
            .min(max_len - matched);
        let chunk_match =
            count_match_length_slices(&left_bytes[..chunk_len], &right_bytes[..chunk_len]);
        matched += chunk_match;
        if chunk_match != chunk_len {
            break;
        }
    }
    matched
}

#[inline(always)]
#[allow(unsafe_code)]
pub(crate) fn virtual_match_has_length(
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    left: usize,
    right: usize,
    len: usize,
) -> bool {
    let virtual_len = prefix_chain.len() + src.len();
    if left + len > virtual_len || right + len > virtual_len {
        return false;
    }

    // Fast path: both indices fully in source with a small fixed length (the
    // usual case from repeat-offset checks with len = MIN_MATCH = 4). LLVM
    // compiles the generic slice-compare below to a call to libc memcmp, which
    // shows up as a DYLD-STUB call in the profile — bypass it with explicit
    // unaligned word reads.
    let prefix_len = prefix_chain.len();
    if left >= prefix_len && right >= prefix_len {
        let left_src = left - prefix_len;
        let right_src = right - prefix_len;
        // SAFETY: left + len <= virtual_len = prefix_len + src.len(), and
        // left >= prefix_len, so left_src + len <= src.len(). Same for right.
        match len {
            4 => {
                return unsafe {
                    core::ptr::read_unaligned(src.as_ptr().add(left_src) as *const u32)
                        == core::ptr::read_unaligned(src.as_ptr().add(right_src) as *const u32)
                };
            }
            8 => {
                return unsafe {
                    core::ptr::read_unaligned(src.as_ptr().add(left_src) as *const u64)
                        == core::ptr::read_unaligned(src.as_ptr().add(right_src) as *const u64)
                };
            }
            _ => {}
        }
    }

    let mut matched = 0usize;
    while matched < len {
        let left_slice = prefix_chain.slice(src, left + matched);
        let right_slice = prefix_chain.slice(src, right + matched);
        let left_bytes = left_slice.as_slice();
        let right_bytes = right_slice.as_slice();
        let chunk_len = left_bytes.len().min(right_bytes.len()).min(len - matched);
        if left_bytes[..chunk_len] != right_bytes[..chunk_len] {
            return false;
        }
        matched += chunk_len;
    }
    true
}

pub(crate) fn virtual_match_can_reach_length(
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    left: usize,
    right: usize,
    required_length: usize,
) -> bool {
    if required_length < MIN_MATCH {
        return true;
    }

    let virtual_len = prefix_chain.len() + src.len();
    if left + required_length > virtual_len || right + required_length > virtual_len {
        return false;
    }

    virtual_match_has_length(
        prefix_chain,
        src,
        left + required_length - MIN_MATCH,
        right + required_length - MIN_MATCH,
        MIN_MATCH,
    )
}

pub(crate) fn minimum_regular_match_length_to_tie(
    current: MatchCandidate,
    candidate_offset: usize,
    literal_length: usize,
) -> usize {
    let current_score = estimated_regular_match_score_bits(current, literal_length);
    let literal_length_u32 = literal_length.min(u32::MAX as usize) as u32;
    let literal_cost = literal_length_u32.saturating_mul(5);
    let candidate_offset_bits = ((candidate_offset as u32).saturating_add(3)).ilog2();
    let candidate_sequence_cost = candidate_offset_bits + 22;
    let required_bits = current_score + candidate_sequence_cost as i32 + literal_cost as i32;
    if required_bits <= 0 {
        0
    } else {
        required_bits.saturating_add(7) as usize / 8
    }
}

#[inline(always)]
pub(crate) fn count_match_length_slices(left: &[u8], right: &[u8]) -> usize {
    let max_len = left.len().min(right.len());
    #[cfg(target_pointer_width = "64")]
    {
        let left = &left[..max_len];
        let right = &right[..max_len];
        let (left_words, left_tail) = left.as_chunks::<8>();
        let (right_words, right_tail) = right.as_chunks::<8>();
        for (index, (left_word, right_word)) in
            left_words.iter().zip(right_words.iter()).enumerate()
        {
            let left_word = u64::from_ne_bytes(*left_word);
            let right_word = u64::from_ne_bytes(*right_word);
            if left_word != right_word {
                let diff = left_word ^ right_word;
                let equal_prefix_bytes = if cfg!(target_endian = "little") {
                    diff.trailing_zeros() as usize / 8
                } else {
                    diff.leading_zeros() as usize / 8
                };
                return index * 8 + equal_prefix_bytes;
            }
        }
        let mut matched = left_words.len() * 8;
        while matched < max_len
            && left_tail[matched - left_words.len() * 8]
                == right_tail[matched - left_words.len() * 8]
        {
            matched += 1;
        }
        matched
    }

    #[cfg(target_pointer_width = "32")]
    {
        let left = &left[..max_len];
        let right = &right[..max_len];
        let (left_words, left_tail) = left.as_chunks::<4>();
        let (right_words, right_tail) = right.as_chunks::<4>();
        for (index, (left_word, right_word)) in
            left_words.iter().zip(right_words.iter()).enumerate()
        {
            let left_word = u32::from_ne_bytes(*left_word);
            let right_word = u32::from_ne_bytes(*right_word);
            if left_word != right_word {
                let diff = left_word ^ right_word;
                let equal_prefix_bytes = if cfg!(target_endian = "little") {
                    diff.trailing_zeros() as usize / 8
                } else {
                    diff.leading_zeros() as usize / 8
                };
                return index * 4 + equal_prefix_bytes;
            }
        }
        let mut matched = left_words.len() * 4;
        while matched < max_len
            && left_tail[matched - left_words.len() * 4]
                == right_tail[matched - right_words.len() * 4]
        {
            matched += 1;
        }
        matched
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PrefixChain<'a> {
    pub(crate) segments: &'a [&'a [u8]],
    pub(crate) len: usize,
}

impl<'a> PrefixChain<'a> {
    pub(crate) fn new(segments: &'a [&'a [u8]]) -> Result<Option<Self>> {
        let len = segments.iter().try_fold(0usize, |total, segment| {
            total
                .checked_add(segment.len())
                .ok_or(Error::OutputSizeOverflow)
        })?;
        if len == 0 {
            return Ok(None);
        }
        Ok(Some(Self { segments, len }))
    }

    #[inline(always)]
    pub(crate) fn len(self) -> usize {
        self.len
    }

    #[inline(always)]
    pub(crate) fn byte(self, pos: usize) -> u8 {
        if self.segments.len() == 1 {
            return self.segments[0][pos];
        }
        let mut base = 0usize;
        for segment in self.segments {
            let end = base + segment.len();
            if pos < end {
                return segment[pos - base];
            }
            base = end;
        }
        unreachable!("prefix chain lookup outside the provided segments");
    }

    #[inline(always)]
    pub(crate) fn slice<'b>(self, src: &'b [u8], pos: usize) -> VirtualSlice<'a, 'b> {
        if pos >= self.len {
            return VirtualSlice::Src(&src[pos - self.len..]);
        }
        if self.segments.len() == 1 {
            return VirtualSlice::Prefix(&self.segments[0][pos..]);
        }

        let mut base = 0usize;
        for segment in self.segments {
            let end = base + segment.len();
            if pos < end {
                return VirtualSlice::Prefix(&segment[pos - base..]);
            }
            base = end;
        }
        unreachable!("prefix chain lookup outside the provided segments");
    }
}

pub(crate) enum VirtualSlice<'a, 'b> {
    Prefix(&'a [u8]),
    Src(&'b [u8]),
}

impl<'a, 'b> VirtualSlice<'a, 'b> {
    #[inline(always)]
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::Prefix(slice) => slice,
            Self::Src(slice) => slice,
        }
    }
}

pub(crate) enum LogicalSlice<'a, 'b> {
    Prefix(&'a [u8]),
    Src(&'b [u8]),
}

impl<'a, 'b> LogicalSlice<'a, 'b> {
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::Prefix(slice) => slice,
            Self::Src(slice) => slice,
        }
    }
}

#[inline(always)]
pub(crate) fn hash_at(src: &[u8], pos: usize, hash_bits: u32) -> usize {
    let value = crate::entropy::mem::read_u32(src, pos).wrapping_mul(0x9E37_79B1);
    (value >> (32 - hash_bits)) as usize
}

/// Hash of the eight bytes at `pos`, with a tag, for the double-fast long table.
///
/// Returns `hash_bits + SHORT_CACHE_TAG_BITS` bits: [`tagged_index`] takes the
/// table slot from the top, the low eight are the tag [`long_entry`] files
/// under the position. Unlike the short table the entry is 64 bits wide, so the
/// tag costs the position none of its range.
#[inline(always)]
#[allow(unsafe_code)]
pub(crate) fn hash_long_at(src: &[u8], pos: usize, hash_bits: u32) -> usize {
    debug_assert!(pos + 8 <= src.len());
    let value = unsafe { crate::entropy::mem::read_u64_unchecked(src, pos) }
        .wrapping_mul(0xCF1B_BCDC_B7A5_6463);
    // hash_bits is pre-clamped to [10, MAX_MATCH_HASH_BITS] at finder
    // construction, and the tag is taken from below it, so the widest shift
    // here is 64 - (25 + 8).
    debug_assert!((10..=MAX_MATCH_HASH_BITS).contains(&hash_bits));
    (value >> (64 - (hash_bits + SHORT_CACHE_TAG_BITS))) as usize
}

#[inline(always)]
pub(crate) fn hash_short_cache_src_at_mls(
    src: &[u8],
    pos: usize,
    hash_bits: u32,
    min_match: u32,
) -> usize {
    hash_src_at_mls_bits(src, pos, hash_bits + SHORT_CACHE_TAG_BITS, min_match)
}

#[inline(always)]
pub(crate) fn hash_at_mls(src: &[u8], pos: usize, hash_bits: u32, min_match: u32) -> usize {
    hash_src_at_mls_bits(src, pos, hash_bits, min_match)
}

#[inline(always)]
#[allow(unsafe_code)]
pub(crate) fn hash_at_mls_const<const MLS: u32>(src: &[u8], pos: usize, hash_bits: u32) -> usize {
    debug_assert!(pos + 8 <= src.len());
    match MLS {
        4 => hash_at_mls_4(src, pos, hash_bits),
        5 => hash_at_mls_5(src, pos, hash_bits),
        6 => hash_at_mls_6(src, pos, hash_bits),
        _ => {
            let value = unsafe { crate::entropy::mem::read_u64_unchecked(src, pos) };
            ((((value << 8).wrapping_mul(58_295_818_150_454_627)) >> (64 - hash_bits)) as u32)
                as usize
        }
    }
}

#[allow(unsafe_code)]
#[inline(always)]
pub(crate) fn hash_at_mls_4(src: &[u8], pos: usize, hash_bits: u32) -> usize {
    debug_assert!(pos + 4 <= src.len());
    let value =
        unsafe { crate::entropy::mem::read_u32_unchecked(src, pos) }.wrapping_mul(0x9E37_79B1);
    (value >> (32 - hash_bits)) as usize
}

#[allow(unsafe_code)]
#[inline(always)]
pub(crate) fn hash_at_mls_5(src: &[u8], pos: usize, hash_bits: u32) -> usize {
    debug_assert!(pos + 8 <= src.len());
    let value = unsafe { crate::entropy::mem::read_u64_unchecked(src, pos) };
    ((((value << 24).wrapping_mul(889_523_592_379)) >> (64 - hash_bits)) as u32) as usize
}

#[allow(unsafe_code)]
#[inline(always)]
pub(crate) fn hash_at_mls_6(src: &[u8], pos: usize, hash_bits: u32) -> usize {
    debug_assert!(pos + 8 <= src.len());
    let value = unsafe { crate::entropy::mem::read_u64_unchecked(src, pos) };
    ((((value << 16).wrapping_mul(227_718_039_650_203)) >> (64 - hash_bits)) as u32) as usize
}

/// Like `hash_at_mls_const` but returns `hash_bits + SHORT_CACHE_TAG_BITS` bits:
/// the top `hash_bits` bits are the hash table index, the bottom 8 bits are
/// the short-cache tag.
#[inline(always)]
pub(crate) fn hash_at_mls_const_tagged<const MLS: u32>(
    src: &[u8],
    pos: usize,
    hash_bits: u32,
) -> usize {
    hash_at_mls_const::<MLS>(src, pos, hash_bits + SHORT_CACHE_TAG_BITS)
}

/// Build a tagged hash-table entry from a position and hash_and_tag value.
/// The 64-bit entry stores the position in bits [63:8] and the tag in bits
/// [7:0], so positions beyond 16 MiB survive intact.
#[inline(always)]
pub(crate) fn tagged_entry(pos: usize, hash_and_tag: usize) -> u64 {
    ((pos as u64) << SHORT_CACHE_TAG_BITS) | (hash_and_tag as u64 & SHORT_CACHE_TAG_MASK as u64)
}

/// Extract the position from a tagged hash-table entry.
#[inline(always)]
pub(crate) fn tagged_pos(entry: u64) -> usize {
    (entry >> SHORT_CACHE_TAG_BITS) as usize
}

/// Check whether the tag in a hash-table entry matches the tag from a hash_and_tag value.
#[inline(always)]
pub(crate) fn tag_matches(entry: u64, hash_and_tag: usize) -> bool {
    (entry as usize ^ hash_and_tag) & SHORT_CACHE_TAG_MASK_USIZE == 0
}

/// Extract the hash table index from a hash_and_tag value.
#[inline(always)]
pub(crate) fn tagged_index(hash_and_tag: usize) -> usize {
    hash_and_tag >> SHORT_CACHE_TAG_BITS
}

/// Rebase a table of raw positions by `delta`, so it keeps describing the same
/// bytes after the first `delta` of them are dropped from the buffer it indexes.
///
/// Entries that pointed into the dropped bytes become `empty`, which is that
/// table's own encoding for a slot that has never been filled.
///
/// This is what C does in `ZSTD_reduceTable` (`zstd_compress.c`). Subtracting
/// one value from every entry in place preserves the table the parser actually
/// built; clearing the table and re-inserting the retained bytes produces a
/// denser and measurably different one.
pub(crate) fn shift_raw_positions(table: &mut [u32], delta: usize, empty: u32) {
    let Ok(delta) = u32::try_from(delta) else {
        table.fill(empty);
        return;
    };
    for entry in table.iter_mut() {
        if *entry == empty {
            continue;
        }
        *entry = entry.checked_sub(delta).unwrap_or(empty);
    }
}

/// The long double-fast table's entry layout: a filed position with the slot's
/// tag byte below it.
///
/// Both tables need more than 24 position bits: the one-shot encoder hands
/// the parser the whole input, so its positions run to the frame length,
/// and a 24-bit field wraps every one past 16 MiB. A 64-bit entry keeps the
/// position at the full `u32` width the table has always filed and still reads
/// and writes as a single value, which is the reason for the width. The parser
/// touches this table once per inner iteration, and a tag kept in a second
/// array is a second cache line and a second store per iteration where
/// upstream's one `U32*` costs one of each.
pub(crate) const LONG_ENTRY_TAG_MASK: u64 = SHORT_CACHE_TAG_MASK as u64;

/// An entry naming no position. [`long_entry_pos`] decodes it to [`NO_POS`], so
/// the position tests that already rejected `NO_POS` need no special case.
pub(crate) const LONG_ENTRY_EMPTY: u64 = (NO_TAGGED_ENTRY) << SHORT_CACHE_TAG_BITS;

/// Build a long table entry from a position and the hash that chose its slot.
#[inline(always)]
pub(crate) fn long_entry(pos: u32, hash_and_tag: usize) -> u64 {
    ((pos as u64) << SHORT_CACHE_TAG_BITS) | (hash_and_tag as u64 & LONG_ENTRY_TAG_MASK)
}

/// The position a long table entry names.
#[inline(always)]
pub(crate) fn long_entry_pos(entry: u64) -> u32 {
    (entry >> SHORT_CACHE_TAG_BITS) as u32
}

/// Replace an entry's position, keeping its tag.
///
/// A rebase moves where the bytes a slot names live without changing which
/// bytes they are, so the tag survives it untouched.
#[inline(always)]
pub(crate) fn long_entry_with_pos(entry: u64, pos: u32) -> u64 {
    ((pos as u64) << SHORT_CACHE_TAG_BITS) | (entry & LONG_ENTRY_TAG_MASK)
}

/// Whether a long table entry was filed under a hash with this tag.
#[inline(always)]
pub(crate) fn long_entry_tag_matches(entry: u64, hash_and_tag: usize) -> bool {
    (entry ^ hash_and_tag as u64) & LONG_ENTRY_TAG_MASK == 0
}

/// [`shift_raw_positions`] for the long double-fast table's 64-bit entries.
///
/// Only the position moves; see [`long_entry_with_pos`]. A slot whose position
/// falls below the retained bytes is emptied outright rather than left with a
/// stale tag, which costs nothing -- a reader that gets past the tag is
/// rejected by the position either way -- and keeps "empty" to one bit pattern.
pub(crate) fn shift_long_entries(table: &mut [u64], delta: usize) {
    let Ok(delta) = u32::try_from(delta) else {
        table.fill(LONG_ENTRY_EMPTY);
        return;
    };
    for entry in table.iter_mut() {
        let pos = long_entry_pos(*entry);
        if pos == NO_POS {
            continue;
        }
        *entry = match pos.checked_sub(delta) {
            Some(shifted) => long_entry_with_pos(*entry, shifted),
            None => LONG_ENTRY_EMPTY,
        };
    }
}

/// [`shift_raw_positions`] for a table that also stores a non-position marker,
/// which names a state rather than a byte and so must not move with the buffer.
///
/// C spells the same exception as `preserveMark` in `ZSTD_reduceTable_internal`
/// (`zstd_compress.c:349`), where it has to add the reducer back to the mark
/// because the mark there is a small integer that the subtraction below would
/// otherwise reach. Ours sits at the top of the range, so skipping it is enough.
pub(crate) fn shift_raw_positions_preserving(
    table: &mut [u32],
    delta: usize,
    empty: u32,
    mark: u32,
) {
    let Ok(delta) = u32::try_from(delta) else {
        for entry in table.iter_mut() {
            if *entry != mark {
                *entry = empty;
            }
        }
        return;
    };
    for entry in table.iter_mut() {
        if *entry == empty || *entry == mark {
            continue;
        }
        *entry = entry.checked_sub(delta).unwrap_or(empty);
    }
}

/// [`shift_raw_positions`] for a table whose entries carry a hash tag in their
/// low [`SHORT_CACHE_TAG_BITS`] bits.
///
/// The tag identifies which bytes the entry was filed under and does not move
/// with the buffer, so it survives the shift untouched. Emptied slots take
/// [`NO_POS`], which is what [`FastFinder::reset`] fills a table with.
pub(crate) fn shift_tagged_positions(table: &mut [u64], delta: usize) {
    for entry in table.iter_mut() {
        if *entry == NO_TAGGED_ENTRY {
            continue;
        }
        *entry = match tagged_pos(*entry).checked_sub(delta) {
            Some(pos) => tagged_entry(pos, *entry as usize),
            None => NO_TAGGED_ENTRY,
        };
    }
}

#[inline(always)]
pub(crate) fn hash_short_cache_prefix_at_mls(
    prefix: &[u8],
    pos: usize,
    hash_bits: u32,
    min_match: u32,
) -> usize {
    hash_prefix_at_mls_bits(prefix, pos, hash_bits + SHORT_CACHE_TAG_BITS, min_match)
}

#[inline(always)]
#[allow(unsafe_code)]
fn hash_src_at_mls_bits(src: &[u8], pos: usize, hash_bits: u32, min_match: u32) -> usize {
    // min_match is pre-clamped at finder construction.
    match min_match {
        // C: ZSTD_hash3 — 3-byte hash using prime3bytes = 506832829
        3 => {
            debug_assert!(pos + 4 <= src.len());
            #[allow(unsafe_code)]
            let value = unsafe { crate::entropy::mem::read_u32_unchecked(src, pos) };
            (((value << 8).wrapping_mul(506_832_829)) >> (32 - hash_bits)) as usize
        }
        4 => hash_at_mls_4(src, pos, hash_bits),
        5 => hash_at_mls_5(src, pos, hash_bits),
        6 => hash_at_mls_6(src, pos, hash_bits),
        _ => {
            debug_assert!(pos + 8 <= src.len());
            let value = unsafe { crate::entropy::mem::read_u64_unchecked(src, pos) };
            ((((value << 8).wrapping_mul(58_295_818_150_454_627)) >> (64 - hash_bits)) as u32)
                as usize
        }
    }
}

#[inline(always)]
fn hash_prefix_at_mls_bits(prefix: &[u8], pos: usize, hash_bits: u32, min_match: u32) -> usize {
    match min_match {
        4 => {
            let value = crate::entropy::mem::read_u32(prefix, pos).wrapping_mul(0x9E37_79B1);
            (value >> (32 - hash_bits)) as usize
        }
        5 => {
            let value = crate::entropy::mem::read_u64(prefix, pos);
            ((((value << 24).wrapping_mul(889_523_592_379)) >> (64 - hash_bits)) as u32) as usize
        }
        6 => {
            let value = crate::entropy::mem::read_u64(prefix, pos);
            ((((value << 16).wrapping_mul(227_718_039_650_203)) >> (64 - hash_bits)) as u32)
                as usize
        }
        _ => {
            let value = crate::entropy::mem::read_u64(prefix, pos);
            ((((value << 8).wrapping_mul(58_295_818_150_454_627)) >> (64 - hash_bits)) as u32)
                as usize
        }
    }
}

/// [`hash_long_at`] over prefix bytes, tagged identically.
pub(crate) fn hash_long_prefix_at(prefix: &[u8], pos: usize, hash_bits: u32) -> usize {
    let bytes = [
        prefix[pos],
        prefix[pos + 1],
        prefix[pos + 2],
        prefix[pos + 3],
        prefix[pos + 4],
        prefix[pos + 5],
        prefix[pos + 6],
        prefix[pos + 7],
    ];
    hash_bytes_long(bytes, hash_bits + SHORT_CACHE_TAG_BITS)
}

/// Test-only: reached solely from the `*_candidate_at` probes.
#[cfg(test)]
pub(crate) fn hash_short_cache_long_src_at(src: &[u8], pos: usize, hash_bits: u32) -> usize {
    let bytes = [
        src[pos],
        src[pos + 1],
        src[pos + 2],
        src[pos + 3],
        src[pos + 4],
        src[pos + 5],
        src[pos + 6],
        src[pos + 7],
    ];
    hash_bytes_long_short_cache(bytes, hash_bits + SHORT_CACHE_TAG_BITS)
}

/// Test-only: reached solely from the `*_candidate_at` probes.
#[cfg(test)]
pub(crate) fn tagged_dict_candidate(entry: u32, hash_and_tag: usize) -> Option<usize> {
    if entry == 0 || (entry & SHORT_CACHE_TAG_MASK) != (hash_and_tag as u32 & SHORT_CACHE_TAG_MASK)
    {
        return None;
    }
    Some(((entry >> SHORT_CACHE_TAG_BITS).saturating_sub(1)) as usize)
}

pub(crate) fn hash_short_cache_long_prefix_at(prefix: &[u8], pos: usize, hash_bits: u32) -> usize {
    let bytes = [
        prefix[pos],
        prefix[pos + 1],
        prefix[pos + 2],
        prefix[pos + 3],
        prefix[pos + 4],
        prefix[pos + 5],
        prefix[pos + 6],
        prefix[pos + 7],
    ];
    hash_bytes_long_short_cache(bytes, hash_bits + SHORT_CACHE_TAG_BITS)
}

pub(crate) fn write_tagged_dict_index(hash_table: &mut [u32], hash_and_tag: usize, index: usize) {
    let hash = hash_and_tag >> SHORT_CACHE_TAG_BITS;
    let tag = (hash_and_tag as u32) & SHORT_CACHE_TAG_MASK;
    let encoded_index = (index as u32).saturating_add(1);
    hash_table[hash] = (encoded_index << SHORT_CACHE_TAG_BITS) | tag;
}

#[inline(always)]
pub(crate) fn hash_prefix_chain_at(
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    pos: usize,
    hash_bits: u32,
) -> usize {
    let bytes = [
        virtual_byte(prefix_chain, src, pos),
        virtual_byte(prefix_chain, src, pos + 1),
        virtual_byte(prefix_chain, src, pos + 2),
        virtual_byte(prefix_chain, src, pos + 3),
    ];
    hash_bytes(bytes, hash_bits)
}

#[inline(always)]
pub(crate) fn hash_prefix_chain_at_mls(
    prefix_chain: PrefixChain<'_>,
    src: &[u8],
    pos: usize,
    hash_bits: u32,
    min_match: u32,
) -> usize {
    let bytes = [
        virtual_byte(prefix_chain, src, pos),
        virtual_byte(prefix_chain, src, pos + 1),
        virtual_byte(prefix_chain, src, pos + 2),
        virtual_byte(prefix_chain, src, pos + 3),
        virtual_byte(prefix_chain, src, pos + 4),
        virtual_byte(prefix_chain, src, pos + 5),
        virtual_byte(prefix_chain, src, pos + 6),
        virtual_byte(prefix_chain, src, pos + 7),
    ];
    hash_bytes_for_min_match(bytes, hash_bits, min_match)
}

#[inline(always)]
pub(crate) fn virtual_byte(prefix_chain: PrefixChain<'_>, src: &[u8], pos: usize) -> u8 {
    if pos < prefix_chain.len() {
        prefix_chain.byte(pos)
    } else {
        src[pos - prefix_chain.len()]
    }
}

pub(crate) fn hash_bytes(bytes: [u8; 4], hash_bits: u32) -> usize {
    let value = u32::from_le_bytes(bytes).wrapping_mul(0x9E37_79B1);
    (value >> (32 - hash_bits)) as usize
}

pub(crate) fn hash_bytes_for_min_match(bytes: [u8; 8], hash_bits: u32, min_match: u32) -> usize {
    match min_match {
        // C: ZSTD_hash3 — `((u << 8) * prime3bytes) >> (32 - h)`
        // where prime3bytes = 506832829
        3 => {
            let value = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            (((value << 8).wrapping_mul(506_832_829)) >> (32 - hash_bits)) as usize
        }
        4 => hash_bytes([bytes[0], bytes[1], bytes[2], bytes[3]], hash_bits),
        5 => {
            let value = u64::from_le_bytes(bytes);
            ((((value << 24).wrapping_mul(889_523_592_379)) >> (64 - hash_bits)) as u32) as usize
        }
        6 => {
            let value = u64::from_le_bytes(bytes);
            ((((value << 16).wrapping_mul(227_718_039_650_203)) >> (64 - hash_bits)) as u32)
                as usize
        }
        _ => {
            let value = u64::from_le_bytes(bytes);
            ((((value << 8).wrapping_mul(58_295_818_150_454_627)) >> (64 - hash_bits)) as u32)
                as usize
        }
    }
}

pub(crate) fn hash_bytes_long(bytes: [u8; 8], hash_bits: u32) -> usize {
    let value = u64::from_le_bytes(bytes).wrapping_mul(0xCF1B_BCDC_B7A5_6463);
    (value >> (64 - hash_bits)) as usize
}

pub(crate) fn hash_bytes_long_short_cache(bytes: [u8; 8], hash_bits: u32) -> usize {
    let value = u64::from_le_bytes(bytes).wrapping_mul(0xCF1B_BCDC_B7A5_6463);
    (value >> (64 - hash_bits)) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_entries_preserve_positions_past_16_mib() {
        for pos in [1 << 24, (1 << 24) + 1] {
            for tag in 0..=SHORT_CACHE_TAG_MASK_USIZE {
                let hash_and_tag = (123 << SHORT_CACHE_TAG_BITS) | tag;
                let entry = tagged_entry(pos, hash_and_tag);
                assert_eq!(tagged_pos(entry), pos);
                assert_eq!(entry & SHORT_CACHE_TAG_MASK as u64, tag as u64);
                assert!(tag_matches(entry, hash_and_tag));
                assert!(!tag_matches(entry, hash_and_tag ^ 1));
            }
        }
    }
}
