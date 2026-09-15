//! Long-distance matching: the matcher, its table, and its parameters.
//!
//! LDM is a coarse matcher that finds very long matches at distances the
//! per-block parsers cannot reach, because their tables are sized to the search
//! window rather than to the whole history. What happens to its matches
//! depends on the parser: below `btopt` they are *taken* and the parser runs
//! on the gaps between them, while `btopt` and above search as they always
//! would and price each long-distance match as one more candidate. Both live
//! in [`plan_sequences_for_contiguous_block_with_ldm_into`](super::plan_sequences_for_contiguous_block_with_ldm_into);
//! this module produces the matches and the cursor over them, and nothing
//! else.
//!
//! Three parameter rules here are transcribed from the pinned checkout, and
//! each decides what the table looks like rather than being visible on its own:
//!
//! - the **window force**, `zstd_compress.c:1646`;
//! - the **auto rule**, `zstd_compress.c:272`;
//! - the **default derivation**, `ZSTD_ldm_adjustParameters`, `zstd_ldm.c:135`.
//!
//! Two of the three were first written straight from the C and were still
//! wrong; `oracles/ldm/compare.sh` is what found them, by compiling a harness
//! against the pinned checkout and diffing both the 5184-row parameter grid and
//! the generated sequences. Change anything here and run it.
//!
//! A dictionary is supported throughout. [`LdmSource`] puts one in front of the
//! frame in a single index space, and [`LdmState::fill_from_dictionary`] hashes
//! it in the way `ZSTD_loadDictionaryContent` does -- a different walk from the
//! one generation makes, which is why it is a different function.
//!
//! [`resolve_enable_ldm`] is implemented and tested but still not consulted by
//! the encoder. That is no longer about a dictionary: honouring the rule would
//! change default output at level 22 above 64 MiB and nowhere else, and nothing
//! in the suite encodes a body that large. See the note in
//! `compression_parameters_with_overrides` and Phase 3 in
//! `docs/PARITY_PLAN.md`.

// `resolve_enable_ldm` has only tests for callers, and a handful of constants
// are named for the reader rather than used twice.
#![allow(dead_code)]

use crate::encode::UpstreamStrategy;
use crate::window::max_table_log;

/// `ZSTD_LDM_DEFAULT_WINDOW_LOG` (`zstd_ldm.h:21`), which is
/// `ZSTD_WINDOWLOG_LIMIT_DEFAULT`.
pub(crate) const LDM_DEFAULT_WINDOW_LOG: u32 = 27;

/// `LDM_MIN_MATCH_LENGTH` (`zstd_ldm.c:20`).
const LDM_MIN_MATCH_LENGTH: u32 = 64;

/// `LDM_BUCKET_SIZE_LOG` (`zstd_ldm.c:19`), the *lower* clamp on the bucket
/// size, not a default.
const LDM_BUCKET_SIZE_LOG: u32 = 4;

/// `ZSTD_LDM_BUCKETSIZELOG_MAX` (`zstd.h:1300`).
const LDM_BUCKET_SIZE_LOG_MAX: u32 = 8;

/// `ZSTD_HASHLOG_MIN` and `ZSTD_HASHLOG_MAX`, the bounds `ldm_hash_log` is
/// clamped into.
const HASH_LOG_MIN: u32 = 6;
const HASH_LOG_MAX: u32 = 30;

/// Whether long-distance matching runs, upstream's `ZSTD_c_enableLongDistanceMatching`.
///
/// Three states rather than a `bool` because [`Self::Auto`] is upstream's
/// default and is not either boolean: it enables LDM for some parameter sets
/// and not others, and which it picks is decided after the compression
/// parameters have been fitted to the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LdmMode {
    /// Decide from the resolved compression parameters, upstream's
    /// `ZSTD_ps_auto`. See `resolve_enable_ldm`.
    #[default]
    Auto,
    /// Always on, upstream's `ZSTD_ps_enable`. This also *pins* the window; see
    /// `LDM_DEFAULT_WINDOW_LOG` and the note on `force_window_log`.
    Enabled,
    /// Always off, upstream's `ZSTD_ps_disable`.
    Disabled,
}

/// The four LDM table parameters, after [`LdmParameters::resolve`].
///
/// Each corresponds to a `ZSTD_c_ldm*` parameter and each is `None` until
/// resolved, because "unset" is what the derivation branches on — a caller who
/// supplies one changes how the others are derived, not just its own value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct LdmParameterOverrides {
    /// `ZSTD_c_ldmHashLog`.
    pub(crate) hash_log: Option<u32>,
    /// `ZSTD_c_ldmMinMatch`.
    pub(crate) min_match_length: Option<u32>,
    /// `ZSTD_c_ldmBucketSizeLog`.
    pub(crate) bucket_size_log: Option<u32>,
    /// `ZSTD_c_ldmHashRateLog`.
    pub(crate) hash_rate_log: Option<u32>,
}

/// Resolved LDM parameters: what the matcher would build its table from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LdmParameters {
    pub(crate) window_log: u32,
    pub(crate) hash_log: u32,
    pub(crate) min_match_length: u32,
    pub(crate) bucket_size_log: u32,
    pub(crate) hash_rate_log: u32,
}

/// C's `ZSTD_resolveEnableLdm` (`zstd_compress.c:269-274`).
///
/// The rule reads as "levels 16 and up" and is not. In tier 0 of the level
/// table the window logs at levels 16 through 22 are 22, 23, 23, 23, 25, 26,
/// 27, so **only level 22 reaches 27**. It is also evaluated on *adjusted*
/// parameters (`zstd_compress.c:6378`, after `ZSTD_getCParamsFromCCtxParams`),
/// which fits the window to the source, so it additionally needs a source above
/// `2^26` before level 22's window survives adjustment at 27.
pub(crate) fn resolve_enable_ldm(
    mode: LdmMode,
    strategy: UpstreamStrategy,
    window_log: u32,
) -> bool {
    match mode {
        LdmMode::Enabled => true,
        LdmMode::Disabled => false,
        LdmMode::Auto => strategy >= UpstreamStrategy::BinaryTreeOpt && window_log >= 27,
    }
}

/// The window LDM pins when it is *explicitly* enabled, C's
/// `zstd_compress.c:1646`.
///
/// LDM does not permit a larger window, it forces one: enabling it on a level
/// whose window is smaller raises that window to 128 MiB, and enabling it on
/// level 22 changes nothing.
///
/// Two things about the position of this in C decide the observable behaviour,
/// and both are easy to get backwards:
///
/// - it fires on [`LdmMode::Enabled`] only, never on [`LdmMode::Auto`], because
///   the auto rule is resolved *after* `ZSTD_getCParamsFromCCtxParams` returns
///   (`:6378`) while this sits inside it (`:1646`);
/// - it sits *before* `ZSTD_overrideCParams`, so an explicit `window_log`
///   override beats it.
pub(crate) fn force_window_log(mode: LdmMode, window_log: u32) -> u32 {
    if matches!(mode, LdmMode::Enabled) {
        LDM_DEFAULT_WINDOW_LOG
    } else {
        window_log
    }
}

impl LdmParameters {
    /// C's `ZSTD_ldm_adjustParameters` (`zstd_ldm.c:135-167`).
    ///
    /// The derivation is ordered and interdependent: `hash_rate_log` feeds
    /// `hash_log`, which caps `bucket_size_log`. Any other order yields a
    /// different table shape, so this follows C statement by statement rather
    /// than resolving each field independently.
    ///
    /// The last line is deliberately outside the "unset" checks, so it applies
    /// to a caller-supplied `bucket_size_log` as well as a derived one.
    pub(crate) fn resolve(
        overrides: LdmParameterOverrides,
        strategy: UpstreamStrategy,
        window_log: u32,
    ) -> Self {
        // `params->windowLog = cParams->windowLog` first: everything below
        // reads it, including the caller-supplied branches.
        // C spells "unset" as zero for all four, so a supplied zero takes the
        // derived value rather than sticking. The public surface rejects zero
        // for three of these on bounds anyway; folding it here keeps the two
        // spellings from disagreeing where it does not.
        let overrides = LdmParameterOverrides {
            hash_log: overrides.hash_log.filter(|&value| value != 0),
            min_match_length: overrides.min_match_length.filter(|&value| value != 0),
            bucket_size_log: overrides.bucket_size_log.filter(|&value| value != 0),
            hash_rate_log: overrides.hash_rate_log.filter(|&value| value != 0),
        };

        let mut hash_rate_log = overrides.hash_rate_log.unwrap_or(0);
        if hash_rate_log == 0 {
            hash_rate_log = match overrides.hash_log {
                // Derived from a supplied `hash_log`, but only when the window
                // is wider than it. Otherwise it stays 0, which C leaves as-is
                // rather than clamping.
                Some(hash_log) if window_log > hash_log => window_log - hash_log,
                Some(_) => 0,
                // "mapping from [fast, rate7] to [btultra2, rate4]"
                None => 7 - (strategy.as_upstream_code() / 3),
            };
        }

        let hash_log = overrides.hash_log.unwrap_or_else(|| {
            window_log
                .saturating_sub(hash_rate_log)
                .clamp(HASH_LOG_MIN, HASH_LOG_MAX)
        });

        let min_match_length = overrides.min_match_length.unwrap_or({
            if strategy >= UpstreamStrategy::BinaryTreeUltra {
                LDM_MIN_MATCH_LENGTH / 2
            } else {
                LDM_MIN_MATCH_LENGTH
            }
        });

        let bucket_size_log = overrides.bucket_size_log.unwrap_or_else(|| {
            strategy
                .as_upstream_code()
                .clamp(LDM_BUCKET_SIZE_LOG, LDM_BUCKET_SIZE_LOG_MAX)
        });

        Self {
            window_log,
            hash_log,
            min_match_length,
            // Unconditional, and after `hash_log` is known.
            bucket_size_log: bucket_size_log.min(hash_log),
            hash_rate_log,
        }
    }
}

/// C's `ZSTD_ldm_gearTab` (`zstd_ldm_geartab.h:17`), transcribed verbatim.
///
/// A gear hash multiplies nothing: each byte selects one of these 64-bit
/// constants and the accumulator shifts left by one before adding it, so bit
/// `n` of the accumulator depends on the last `n` bytes. That property is what
/// makes the high bits usable as a content-defined split criterion.
#[rustfmt::skip]
const GEAR_TAB: [u64; 256] = [
    0xf5b8f72c5f77775c, 0x84935f266b7ac412, 0xb647ada9ca730ccc, 0xb065bb4b114fb1de,
    0x34584e7e8c3a9fd0, 0x4e97e17c6ae26b05, 0x3a03d743bc99a604, 0xcecd042422c4044f,
    0x76de76c58524259e, 0x9c8528f65badeaca, 0x86563706e2097529, 0x2902475fa375d889,
    0xafb32a9739a5ebe6, 0xce2714da3883e639, 0x21eaf821722e69e, 0x37b628620b628,
    0x49a8d455d88caf5, 0x8556d711e6958140, 0x4f7ae74fc605c1f, 0x829f0c3468bd3a20,
    0x4ffdc885c625179e, 0x8473de048a3daf1b, 0x51008822b05646b2, 0x69d75d12b2d1cc5f,
    0x8c9d4a19159154bc, 0xc3cc10f4abbd4003, 0xd06ddc1cecb97391, 0xbe48e6e7ed80302e,
    0x3481db31cee03547, 0xacc3f67cdaa1d210, 0x65cb771d8c7f96cc, 0x8eb27177055723dd,
    0xc789950d44cd94be, 0x934feadc3700b12b, 0x5e485f11edbdf182, 0x1e2e2a46fd64767a,
    0x2969ca71d82efa7c, 0x9d46e9935ebbba2e, 0xe056b67e05e6822b, 0x94d73f55739d03a0,
    0xcd7010bdb69b5a03, 0x455ef9fcd79b82f4, 0x869cb54a8749c161, 0x38d1a4fa6185d225,
    0xb475166f94bbe9bb, 0xa4143548720959f1, 0x7aed4780ba6b26ba, 0xd0ce264439e02312,
    0x84366d746078d508, 0xa8ce973c72ed17be, 0x21c323a29a430b01, 0x9962d617e3af80ee,
    0xab0ce91d9c8cf75b, 0x530e8ee6d19a4dbc, 0x2ef68c0cf53f5d72, 0xc03a681640a85506,
    0x496e4e9f9c310967, 0x78580472b59b14a0, 0x273824c23b388577, 0x66bf923ad45cb553,
    0x47ae1a5a2492ba86, 0x35e304569e229659, 0x4765182a46870b6f, 0x6cbab625e9099412,
    0xddac9a2e598522c1, 0x7172086e666624f2, 0xdf5003ca503b7837, 0x88c0c1db78563d09,
    0x58d51865acfc289d, 0x177671aec65224f1, 0xfb79d8a241e967d7, 0x2be1e101cad9a49a,
    0x6625682f6e29186b, 0x399553457ac06e50, 0x35dffb4c23abb74, 0x429db2591f54aade,
    0xc52802a8037d1009, 0x6acb27381f0b25f3, 0xf45e2551ee4f823b, 0x8b0ea2d99580c2f7,
    0x3bed519cbcb4e1e1, 0xff452823dbb010a, 0x9d42ed614f3dd267, 0x5b9313c06257c57b,
    0xa114b8008b5e1442, 0xc1fe311c11c13d4b, 0x66e8763ea34c5568, 0x8b982af1c262f05d,
    0xee8876faaa75fbb7, 0x8a62a4d0d172bb2a, 0xc13d94a3b7449a97, 0x6dbbba9dc15d037c,
    0xc786101f1d92e0f1, 0xd78681a907a0b79b, 0xf61aaf2962c9abb9, 0x2cfd16fcd3cb7ad9,
    0x868c5b6744624d21, 0x25e650899c74ddd7, 0xba042af4a7c37463, 0x4eb1a539465a3eca,
    0xbe09dbf03b05d5ca, 0x774e5a362b5472ba, 0x47a1221229d183cd, 0x504b0ca18ef5a2df,
    0xdffbdfbde2456eb9, 0x46cd2b2fbee34634, 0xf2aef8fe819d98c3, 0x357f5276d4599d61,
    0x24a5483879c453e3, 0x88026889192b4b9, 0x28da96671782dbec, 0x4ef37c40588e9aaa,
    0x8837b90651bc9fb3, 0xc164f741d3f0e5d6, 0xbc135a0a704b70ba, 0x69cd868f7622ada,
    0xbc37ba89e0b9c0ab, 0x47c14a01323552f6, 0x4f00794bacee98bb, 0x7107de7d637a69d5,
    0x88af793bb6f2255e, 0xf3c6466b8799b598, 0xc288c616aa7f3b59, 0x81ca63cf42fca3fd,
    0x88d85ace36a2674b, 0xd056bd3792389e7, 0xe55c396c4e9dd32d, 0xbefb504571e6c0a6,
    0x96ab32115e91e8cc, 0xbf8acb18de8f38d1, 0x66dae58801672606, 0x833b6017872317fb,
    0xb87c16f2d1c92864, 0xdb766a74e58b669c, 0x89659f85c61417be, 0xc8daad856011ea0c,
    0x76a4b565b6fe7eae, 0xa469d085f6237312, 0xaaf0365683a3e96c, 0x4dbb746f8424f7b8,
    0x638755af4e4acc1, 0x3d7807f5bde64486, 0x17be6d8f5bbb7639, 0x903f0cd44dc35dc,
    0x67b672eafdf1196c, 0xa676ff93ed4c82f1, 0x521d1004c5053d9d, 0x37ba9ad09ccc9202,
    0x84e54d297aacfb51, 0xa0b4b776a143445, 0x820d471e20b348e, 0x1874383cb83d46dc,
    0x97edeec7a1efe11c, 0xb330e50b1bdc42aa, 0x1dd91955ce70e032, 0xa514cdb88f2939d5,
    0x2791233fd90db9d3, 0x7b670a4cc50f7a9b, 0x77c07d2a05c6dfa5, 0xe3778b6646d0a6fa,
    0xb39c8eda47b56749, 0x933ed448addbef28, 0xaf846af6ab7d0bf4, 0xe5af208eb666e49,
    0x5e6622f73534cd6a, 0x297daeca42ef5b6e, 0x862daef3d35539a6, 0xe68722498f8e1ea9,
    0x981c53093dc0d572, 0xfa09b0bfbf86fbf5, 0x30b1e96166219f15, 0x70e7d466bdc4fb83,
    0x5a66736e35f2a8e9, 0xcddb59d2b7c1baef, 0xd6c7d247d26d8996, 0xea4e39eac8de1ba3,
    0x539c8bb19fa3aff2, 0x9f90e4c5fd508d8, 0xa34e5956fbaf3385, 0x2e2f8e151d3ef375,
    0x173691e9b83faec1, 0xb85a8d56bf016379, 0x8382381267408ae3, 0xb90f901bbdc0096d,
    0x7c6ad32933bcec65, 0x76bb5e2f2c8ad595, 0x390f851a6cf46d28, 0xc3e6064da1c2da72,
    0xc52a0c101cfa5389, 0xd78eaf84a3fbc530, 0x3781b9e2288b997e, 0x73c2f6dea83d05c4,
    0x4228e364c5b5ed7, 0x9d7a3edf0da43911, 0x8edcfeda24686756, 0x5e7667a7b7a9b3a1,
    0x4c4f389fa143791d, 0xb08bc1023da7cddc, 0x7ab4be3ae529b1cc, 0x754e6132dbe74ff9,
    0x71635442a839df45, 0x2f6fb1643fbe52de, 0x961e0a42cf7a8177, 0xf3b45d83d89ef2ea,
    0xee3de4cf4a6e3e9b, 0xcd6848542c3295e7, 0xe4cee1664c78662f, 0x9947548b474c68c4,
    0x25d73777a5ed8b0b, 0xc915b1d636b7fc, 0x21c2ba75d9b0d2da, 0x5f6b5dcf608a64a1,
    0xdcf333255ff9570c, 0x633b922418ced4ee, 0xc136dde0b004b34a, 0x58cc83b05d4b2f5a,
    0x5eb424dda28e42d2, 0x62df47369739cd98, 0xb4e0b42485e4ce17, 0x16e1f0c1f9a8d1e7,
    0x8ec3916707560ebf, 0x62ba6e2df2cc9db3, 0xcbf9f4ff77d83a16, 0x78d9d7d07d2bbcc4,
    0xef554ce1e02c41f4, 0x8d7581127eccf94d, 0xa9b53336cb3c8a05, 0x38c42c0bf45c4f91,
    0x640893cdf4488863, 0x80ec34bc575ea568, 0x39f324f5b48eaa40, 0xe9d9ed1f8eff527f,
    0x9224fc058cc5a214, 0xbaba00b04cfe7741, 0x309a9f120fcf52af, 0xa558f3ec65626212,
    0x424bec8b7adabe2f, 0x41622513a6aea433, 0xb88da2d5324ca798, 0xd287733b245528a4,
    0x9a44697e6d68aec3, 0x7b1093be2f49bb28, 0x50bbec632e3d8aad, 0x6cd90723e1ea8283,
    0x897b9e7431b02bf3, 0x219efdcb338a7047, 0x3b0311f0a27c0656, 0xdb17bf91c0db96e7,
    0x8cd4fd6b4e85a5b2, 0xfab071054ba6409d, 0x40d6fe831fa9dfd9, 0xaf358debad7d791e,
    0xeb8d0e25a65e3e58, 0xbbcbd3df14e08580, 0xcf751f27ecdab2b, 0x2b4da14f2613d8f4,
];

/// C's `LDM_BATCH_SIZE` (`zstd_ldm.c`): how many split points one pass of the
/// rolling hash may record before the caller must drain them.
const LDM_BATCH_SIZE: usize = 64;

/// C's `kMaxChunkSize` in `ZSTD_ldm_generateSequences` (`zstd_ldm.c:533`). The
/// input is walked in chunks this size so the maximum offset is enforced, and
/// so overflow correction has somewhere to happen.
const LDM_MAX_CHUNK_SIZE: usize = 1 << 20;

/// C's `HASH_READ_SIZE`: the tail the search stops short of, so a match check
/// can always read a whole word.
const HASH_READ_SIZE: usize = 8;

/// One entry of the long-distance hash table.
///
/// The checksum is the top half of the same `XXH64` whose bottom half chose the
/// bucket, so a bucket scan rejects almost every non-match without touching the
/// content at all. That is what makes a bucket of 2^`bucket_size_log` entries
/// affordable to scan linearly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LdmEntry {
    /// Position of the match candidate, as an index into the frame.
    offset: u32,
    checksum: u32,
}

/// A match the matcher found, in the shape C stores in its `RawSeqStore_t`.
///
/// This is not a `SequenceCommand`: the offset is a raw distance rather than an
/// offset code, and the literal length counts bytes since the previous LDM
/// match rather than since the previous sequence of any kind. The block
/// compressor turns runs of these into real sequences, parsing the gaps
/// between them with the ordinary match finder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawSequence {
    pub(crate) literal_length: u32,
    pub(crate) match_length: u32,
    pub(crate) offset: u32,
}

/// A cursor over one block's long-distance matches, measured in bytes of that
/// block.
///
/// C keeps this next to the matches, as `RawSeqStore_t`'s `pos` (which match)
/// and `posInSequence` (how far into it, counting its literals first and then
/// its match). It is separate here because the parsers that *price* long-
/// distance matches rather than taking them advance a cursor of their own
/// without consuming anything, and C models that by copying the whole store.
#[derive(Debug, Clone)]
pub(crate) struct RawSequenceCursor<'a> {
    sequences: &'a [RawSequence],
    index: usize,
    offset_in_sequence: u32,
}

impl<'a> RawSequenceCursor<'a> {
    pub(crate) fn new(sequences: &'a [RawSequence]) -> Self {
        Self {
            sequences,
            index: 0,
            offset_in_sequence: 0,
        }
    }

    /// The match the cursor sits in, or `None` once it has passed every one.
    pub(crate) fn current(&self) -> Option<RawSequence> {
        self.sequences.get(self.index).copied()
    }

    /// How far into [`Self::current`] the cursor sits, counting that match's
    /// literals first and then the match itself.
    pub(crate) fn offset_in_sequence(&self) -> u32 {
        self.offset_in_sequence
    }

    /// C's `ZSTD_ldm_skipRawSeqStoreBytes` (`zstd_ldm.c:664`): move forward
    /// over `bytes` bytes of the block, across as many matches as that covers.
    ///
    /// C spells this body twice, once there and once as the static
    /// `ZSTD_optLdm_skipRawSeqStoreBytes` (`zstd_opt.c:918`). The two are
    /// identical; the duplication is a linkage convenience, not two behaviours.
    ///
    /// Landing exactly on a boundary leaves the cursor at the *start* of the
    /// next match rather than at the end of this one, which is what makes
    /// "which match is current" answerable without also asking how far into it
    /// the cursor sits.
    pub(crate) fn skip_bytes(&mut self, bytes: usize) {
        let mut remaining = self.offset_in_sequence as usize + bytes;
        while remaining > 0 {
            let Some(sequence) = self.sequences.get(self.index) else {
                break;
            };
            let span = (sequence.literal_length + sequence.match_length) as usize;
            if remaining < span {
                self.offset_in_sequence = remaining as u32;
                return;
            }
            remaining -= span;
            self.index += 1;
        }
        self.offset_in_sequence = 0;
    }
}

/// The best candidate a bucket scan found, with the lengths that made it best.
///
/// C carries a `bestEntry` pointer alongside the two lengths; keeping the
/// position here rather than recovering it afterwards matters because two
/// candidates can match to the same length at different distances, and the one
/// the scan chose is the one whose offset must be emitted.
#[derive(Debug, Clone, Copy)]
struct BucketMatch {
    match_pos: usize,
    forward: usize,
    backward: usize,
}

impl BucketMatch {
    fn total(self) -> usize {
        self.forward + self.backward
    }
}

/// The rolling hash and its split criterion, C's `ldmRollingHashState_t`.
#[derive(Debug, Clone, Copy)]
struct GearHash {
    rolling: u64,
    stop_mask: u64,
}

impl GearHash {
    /// C's `ZSTD_ldm_gear_init` (`zstd_ldm.c:32`).
    ///
    /// The mask has `hash_rate_log` bits set, so it triggers on average every
    /// `2^hash_rate_log` bytes. Those bits are placed as high as
    /// `min_match_length` allows rather than at the bottom, because bit `n` of
    /// a gear hash depends on the last `n` bytes: a low bit would split on a
    /// window of a byte or two and find the same split in unrelated content.
    fn new(params: LdmParameters) -> Self {
        let max_bits_in_mask = params.min_match_length.min(64);
        let rate = params.hash_rate_log;
        let stop_mask = if rate > 0 && rate <= max_bits_in_mask {
            ((1u64 << rate) - 1) << (max_bits_in_mask - rate)
        } else {
            // Degenerate: honour the rate and give up on the window.
            (1u64 << rate) - 1
        };
        Self {
            rolling: u64::from(u32::MAX),
            stop_mask,
        }
    }

    /// C's `ZSTD_ldm_gear_reset` (`zstd_ldm.c:65`), which **does not reset
    /// anything**, and so is deliberately not called here.
    ///
    /// Its comment says it "feeds [data, data + minMatchLength) into the hash
    /// without registering any splits. This effectively resets the hash
    /// state." The body reads `state->rolling` into a local, rolls the local
    /// over the bytes, and returns without ever assigning it back. The computed
    /// hash is dead, so the call is a no-op on everything except the time it
    /// takes.
    ///
    /// That looks like a missing store rather than a decision, but the split
    /// points it produces are observable in the compressed output, so
    /// reproducing them is what byte parity means here. Priming the hash the
    /// way the comment describes moves the first two split points of a frame
    /// (14 and 27 became 12 and 31 on the corpus in `oracles/ldm/`), and they
    /// re-converge only once ~31 bytes have rolled through and the initial
    /// value has shifted past the bits the split mask tests.
    ///
    /// If upstream ever adds the store, this becomes a real reset and the call
    /// sites below come back.
    const fn upstream_reset_is_a_noop() {}

    /// C's `ZSTD_ldm_gear_feed` (`zstd_ldm.c:96`).
    ///
    /// Records into `splits` every position in `data` whose hash meets the
    /// criterion, stopping early once `LDM_BATCH_SIZE` of them are found.
    /// Returns how many bytes were consumed, which is *not* `data.len()` when
    /// it stopped early — the caller resumes from there.
    fn feed(&mut self, data: &[u8], splits: &mut Vec<usize>) -> usize {
        let mut hash = self.rolling;
        let mask = self.stop_mask;
        let mut n = 0usize;
        while n < data.len() {
            hash = (hash << 1).wrapping_add(GEAR_TAB[usize::from(data[n])]);
            n += 1;
            if hash & mask == 0 {
                splits.push(n);
                if splits.len() == LDM_BATCH_SIZE {
                    break;
                }
            }
        }
        self.rolling = hash;
        n
    }
}

/// The long-distance match table: buckets of candidates, plus the per-bucket
/// insertion cursor.
///
/// C keeps these as two allocations in its workspace (`hashTable` and
/// `bucketOffsets`) and indexes both by the same hash; the shape is kept here
/// because the insertion cursor wrapping within a bucket is what decides which
/// candidate a bucket forgets, and therefore what matches are found.
#[derive(Debug, Clone)]
pub(crate) struct LdmTable {
    entries: Vec<LdmEntry>,
    /// Next slot to write in each bucket, wrapping at the bucket size.
    bucket_cursors: Vec<u8>,
    bucket_size_log: u32,
    /// `hash_log - bucket_size_log`: how many bits of the hash select a bucket.
    hash_bits: u32,
}

/// Widest `hash_log` this target can hold a table for; see [`max_table_log`].
const MAX_HASH_LOG: u32 = max_table_log(size_of::<LdmEntry>());

impl LdmTable {
    /// The `(hash_log, bucket_size_log)` a table is actually built with.
    ///
    /// `ldm_hash_log` reaches 30, whose table a 32-bit target cannot address,
    /// so it narrows to [`MAX_HASH_LOG`]. That happens here rather than in
    /// [`LdmParameters::resolve`], which stays a statement-by-statement port of
    /// C: the matcher reads the table's own `hash_bits` and `bucket_size_log`,
    /// never `params.hash_log`.
    fn geometry(params: LdmParameters) -> (u32, u32) {
        let hash_log = params.hash_log.min(MAX_HASH_LOG);
        (hash_log, params.bucket_size_log.min(hash_log))
    }

    pub(crate) fn new(params: LdmParameters) -> Self {
        let (hash_log, bucket_size_log) = Self::geometry(params);
        let hash_bits = hash_log - bucket_size_log;
        Self {
            entries: vec![LdmEntry::default(); 1usize << hash_log],
            bucket_cursors: vec![0u8; 1usize << hash_bits],
            bucket_size_log,
            hash_bits,
        }
    }

    fn bucket(&self, hash: u32) -> &[LdmEntry] {
        let start = (hash as usize) << self.bucket_size_log;
        &self.entries[start..start + (1usize << self.bucket_size_log)]
    }

    /// C's `ZSTD_ldm_reduceTable` (`zstd_ldm.c:516`): subtract `reducer` from
    /// every position, and forget the ones that fall below it.
    ///
    /// Position zero doubles as "empty" -- a fresh table is all zeros, and the
    /// candidate loop rejects anything at or below the window floor, which is
    /// never negative. So a forgotten entry and a never-written one are the same
    /// value, which is why C can write `0` here rather than a sentinel.
    fn reduce(&mut self, reducer: u32) {
        for entry in &mut self.entries {
            entry.offset = entry.offset.saturating_sub(reducer);
        }
    }

    /// C's `ZSTD_ldm_insertEntry` (`zstd_ldm.c:194`).
    fn insert(&mut self, hash: u32, entry: LdmEntry) {
        let cursor = &mut self.bucket_cursors[hash as usize];
        let slot = ((hash as usize) << self.bucket_size_log) + usize::from(*cursor);
        self.entries[slot] = entry;
        *cursor = ((usize::from(*cursor) + 1) & ((1usize << self.bucket_size_log) - 1)) as u8;
    }
}

/// The bytes the matcher searches, in one index space: the dictionary first,
/// then the frame.
///
/// C keeps these in two allocations and addresses them through `base` and
/// `dictBase`, which is why `ZSTD_ldm_generateSequences_internal` branches on
/// `extDict` and reaches for `ZSTD_count_2segments` and
/// `ZSTD_ldm_countBackwardsMatch_2segments` (`zstd_ldm.c:430-444`). Both of
/// those helpers exist to *reconstruct* one property: on running off the end of
/// the dictionary, counting continues at the first byte of the prefix. That is
/// the definition of a contiguous buffer, so naming a single index space here
/// gets the same answers with no branch on `extDict` at all -- what C calls
/// `dictEnd` and what it calls `lowPrefixPtr` are one index, [`frame_start`].
///
/// The two slices stay separate rather than being concatenated because a
/// concatenation would copy the dictionary once per frame, and the streaming
/// encoder would copy it again after every compaction.
///
/// One consequence of C's own coordinates is worth stating, because this crate
/// would otherwise have to work for it. C biases every real position by
/// `ZSTD_WINDOW_START_INDEX` (2) and starts `lowLimit` there too
/// (`zstd_compress_internal.h:266,1340`), so its candidate test
/// `cur->offset <= lowestIndex` rejects the dictionary's *first* byte. Here
/// position zero is that same byte, the test is the same `<=`, and the window
/// floor starts at zero -- so it is rejected here for the same reason. That is
/// also what lets a zeroed table read as empty: the one position a cleared
/// entry could be confused with is a position no search may select.
///
/// [`frame_start`]: LdmSource::frame_start
#[derive(Debug, Clone, Copy)]
pub(crate) struct LdmSource<'a> {
    dictionary: &'a [u8],
    frame: &'a [u8],
}

impl<'a> LdmSource<'a> {
    /// A frame with no dictionary before it.
    pub(crate) fn contiguous(frame: &'a [u8]) -> Self {
        Self {
            dictionary: &[],
            frame,
        }
    }

    pub(crate) fn with_dictionary(dictionary: &'a [u8], frame: &'a [u8]) -> Self {
        Self { dictionary, frame }
    }

    /// Where the frame begins, which is both C's `dictEnd` and its
    /// `lowPrefixPtr`.
    pub(crate) fn frame_start(&self) -> usize {
        self.dictionary.len()
    }

    pub(crate) fn len(&self) -> usize {
        self.dictionary.len() + self.frame.len()
    }

    fn byte(&self, pos: usize) -> u8 {
        match pos.checked_sub(self.dictionary.len()) {
            Some(in_frame) => self.frame[in_frame],
            None => self.dictionary[pos],
        }
    }

    /// The frame bytes in `[from, to)`, for the reads that never cross into the
    /// dictionary: the rolling hash and the checksum of a split point, both of
    /// which run over the range being searched.
    fn frame_range(&self, from: usize, to: usize) -> &'a [u8] {
        &self.frame[from - self.dictionary.len()..to - self.dictionary.len()]
    }

    /// Bytes matching forwards from two positions, stopping at `end`.
    ///
    /// `end` is the end of the range being searched, not the end of the buffer.
    /// C passes its `iend`, which is the end of the current chunk
    /// (`ZSTD_ldm_countMatch`'s caller at `zstd_ldm.c:449`), so a match is cut
    /// at the chunk boundary and the next call re-finds the rest. Running to the
    /// end of the buffer instead pushes the anchor past the range and reports a
    /// match the caller has no room to lay down.
    fn count_forwards(&self, pos: usize, match_pos: usize, end: usize) -> usize {
        let frame_start = self.frame_start();
        // `pos` is a split point and so is always inside the frame. When the
        // candidate is too, which is every comparison in a frame with no
        // dictionary, both sides index one slice directly.
        if match_pos >= frame_start {
            let (pos, match_pos, end) = (
                pos - frame_start,
                match_pos - frame_start,
                end - frame_start,
            );
            let mut count = 0usize;
            while pos + count < end && self.frame[pos + count] == self.frame[match_pos + count] {
                count += 1;
            }
            return count;
        }
        let mut count = 0usize;
        while pos + count < end && self.byte(pos + count) == self.byte(match_pos + count) {
            count += 1;
        }
        count
    }

    /// Bytes matching backwards from `pos` and `match_pos`, bounded by `anchor`
    /// and by the window floor.
    ///
    /// C's `ZSTD_ldm_countBackwardsMatch` (`zstd_ldm.c:210`), whose `pMatchBase`
    /// is `base + dictLimit` and so is the same floor the forward search rejects
    /// candidates against: `ZSTD_window_enforceMaxDist` raises `dictLimit` to
    /// `lowLimit` on every chunk. Extending backwards is what lets a split point
    /// in the middle of a repeat still produce a match that starts where the
    /// repeat does.
    ///
    /// `window_low` is zero for any frame shorter than the window, which is
    /// every frame the byte-parity sweep can afford to run; it starts mattering
    /// once the frame outgrows its window, and the streaming encoder's buffer is
    /// where that first happens.
    fn count_backwards(
        &self,
        pos: usize,
        anchor: usize,
        match_pos: usize,
        window_low: usize,
    ) -> usize {
        let mut count = 0usize;
        // The `pos` side stops at `anchor`, which is never below the frame, and
        // the candidate side stops at `window_low`. So a floor at or above the
        // frame's start keeps the whole walk inside one slice.
        if window_low >= self.frame_start() {
            let frame_start = self.frame_start();
            let (pos, anchor, match_pos, window_low) = (
                pos - frame_start,
                anchor - frame_start,
                match_pos - frame_start,
                window_low - frame_start,
            );
            while pos - count > anchor
                && match_pos - count > window_low
                && self.frame[pos - count - 1] == self.frame[match_pos - count - 1]
            {
                count += 1;
            }
            return count;
        }
        while pos - count > anchor
            && match_pos - count > window_low
            && self.byte(pos - count - 1) == self.byte(match_pos - count - 1)
        {
            count += 1;
        }
        count
    }
}

/// Long-distance matcher state that lives for the frame.
///
/// The table outlives any one block: LDM exists to find matches further back
/// than a block, so a table rebuilt per block would defeat it. `next_position`
/// is where hashing left off, which is how a call resumes without rehashing
/// what it already saw.
#[derive(Debug, Clone)]
pub(crate) struct LdmState {
    table: LdmTable,
    params: LdmParameters,
    /// C's `ldmState->loadedDictEnd`: where the dictionary ends, while it is
    /// still reachable, and zero once it is not.
    ///
    /// This is a credit against the window rather than a bound on it. A
    /// dictionary is allowed to be referenced *in full* for as long as its last
    /// byte is inside the window, so the floor is only raised once the frame has
    /// grown past `max_distance + loaded_dict_end`
    /// (`ZSTD_window_enforceMaxDist`, `zstd_compress_internal.h:1280`). At that
    /// point C invalidates the dictionary outright, which is why this drops to
    /// zero and stays there rather than tracking the dictionary's position.
    loaded_dict_end: usize,
}

impl LdmState {
    pub(crate) fn new(params: LdmParameters) -> Self {
        Self {
            table: LdmTable::new(params),
            params,
            loaded_dict_end: 0,
        }
    }

    /// Hash a dictionary into the table, C's `ZSTD_ldm_fillHashTable`
    /// (`zstd_ldm.c:285`), called from `ZSTD_loadDictionaryContent`
    /// (`zstd_compress.c:4958`) once per frame before any block is compressed.
    ///
    /// This is *not* the same walk [`generate_into`](Self::generate_into) would
    /// perform over the same bytes, and the difference decides which positions
    /// end up in the table. Generation primes the rolling hash over the first
    /// `min_match_length` bytes and starts hashing after them; this feeds from
    /// the first byte with the hash in its initial state and instead discards
    /// the splits that would name a position before the start. Same bytes,
    /// different split points, so the dictionary cannot be handled by simply
    /// extending the range that generation is asked to search.
    pub(crate) fn fill_from_dictionary(&mut self, dictionary: &[u8]) {
        let min_match = self.params.min_match_length as usize;
        let hash_mask = (1u32 << self.table.hash_bits).wrapping_sub(1);

        let mut hash = GearHash::new(self.params);
        let mut ip = 0usize;
        let mut splits: Vec<usize> = Vec::with_capacity(LDM_BATCH_SIZE);

        while ip < dictionary.len() {
            splits.clear();
            let hashed = hash.feed(&dictionary[ip..], &mut splits);
            for &split_offset in &splits {
                // A split naming a position before the dictionary starts has no
                // `min_match_length` bytes behind it to checksum.
                if ip + split_offset < min_match {
                    continue;
                }
                let split = ip + split_offset - min_match;
                let xxhash = crate::xxhash::xxh64(&dictionary[split..split + min_match], 0);
                self.table.insert(
                    (xxhash as u32) & hash_mask,
                    LdmEntry {
                        offset: split as u32,
                        checksum: (xxhash >> 32) as u32,
                    },
                );
            }
            ip += hashed;
        }

        self.loaded_dict_end = dictionary.len();
    }

    pub(crate) fn parameters(&self) -> LdmParameters {
        self.params
    }

    /// Re-key the table to a buffer whose first `dropped` bytes have been
    /// discarded, C's overflow correction (`zstd_ldm.c:562-566`).
    ///
    /// Every entry is a position in the buffer the matcher was last run over,
    /// so moving those bytes invalidates all of them at once. C reaches this
    /// only when its virtual indices approach `2^31`; the streaming encoder
    /// here reaches it every time it compacts, which is once per window of
    /// input. The correction is the same either way.
    ///
    /// Rebasing is not an optimization here the way it is for the match
    /// finders, which can be rebuilt over the retained bytes at a cost. This
    /// table has no rebuild: the matcher only ever hashes forward over the
    /// range it is handed, so a cleared table is one that has forgotten the
    /// whole frame and will only learn the blocks still to come.
    pub(crate) fn shift_positions(&mut self, dropped: usize) {
        self.table.reduce(
            u32::try_from(dropped).expect("a buffer this encoder can hold is addressable in u32"),
        );
        // C invalidates the dictionary on overflow correction rather than
        // rebasing it (`zstd_ldm.c:565`), and so must this: the dictionary sits
        // below everything the buffer still holds, so the bytes its entries
        // named are exactly the ones compaction dropped.
        self.loaded_dict_end = 0;
    }

    /// C's `ZSTD_ldm_generateSequences_internal` (`zstd_ldm.c:342`), for a
    /// contiguous frame with no external dictionary.
    ///
    /// Appends to `out` and returns the number of trailing bytes not covered by
    /// any match, which is the caller's next `leftover_literals`.
    ///
    /// `src` is the whole frame so far and `[start, end)` the range to search;
    /// matches may reach anywhere in `src` down to `window_low`, which is what
    /// makes this a *long*-distance matcher rather than a per-block one.
    fn generate_into(
        &mut self,
        src: LdmSource<'_>,
        start: usize,
        end: usize,
        window_low: usize,
        out: &mut Vec<RawSequence>,
    ) -> usize {
        let min_match = self.params.min_match_length as usize;
        let entries_per_bucket = 1usize << self.table.bucket_size_log;
        let hash_mask = (1u32 << self.table.hash_bits).wrapping_sub(1);

        if end.saturating_sub(start) < min_match {
            return end.saturating_sub(start);
        }

        let mut anchor = start;
        let mut ip = start;
        let mut hash = GearHash::new(self.params);
        // C primes over the first `min_match` bytes here, except that its
        // `ZSTD_ldm_gear_reset` never stores the hash it computes -- see
        // `GearHash::upstream_reset_is_a_noop`. The position still advances.
        GearHash::upstream_reset_is_a_noop();
        ip += min_match;

        // C stops `HASH_READ_SIZE` short so its match check can read a word.
        let limit = end.saturating_sub(HASH_READ_SIZE);
        let mut splits: Vec<usize> = Vec::with_capacity(LDM_BATCH_SIZE);

        while ip < limit {
            splits.clear();
            let hashed = hash.feed(src.frame_range(ip, limit), &mut splits);

            let mut restart = None;
            for &split_offset in &splits {
                // The split names the *end* of the hashed window; the candidate
                // starts `min_match` before it.
                let split = ip + split_offset - min_match;
                let xxhash = crate::xxhash::xxh64(src.frame_range(split, split + min_match), 0);
                let bucket_hash = (xxhash as u32) & hash_mask;
                let checksum = (xxhash >> 32) as u32;
                let new_entry = LdmEntry {
                    offset: split as u32,
                    checksum,
                };

                // A split inside the previous match would produce an
                // overlapping sequence. Register it and move on, so the table
                // still learns the position.
                if split < anchor {
                    self.table.insert(bucket_hash, new_entry);
                    continue;
                }

                // Strictly greater, so among equal totals the earliest entry
                // in the bucket wins -- which is C's tie-break, and the bucket
                // order is what the insertion cursor decides.
                let mut best: Option<BucketMatch> = None;
                for index in 0..entries_per_bucket {
                    let candidate = self.table.bucket(bucket_hash)[index];
                    if candidate.checksum != checksum || (candidate.offset as usize) <= window_low {
                        continue;
                    }
                    let match_pos = candidate.offset as usize;
                    // Every entry names a position in the buffer this call was
                    // handed. A caller that moved those bytes without calling
                    // `shift_positions` breaks that, and the first symptom is
                    // an index a long way past the end rather than a wrong
                    // match: say so here instead.
                    debug_assert!(
                        match_pos < split,
                        "a long-distance table entry outlived the buffer it names"
                    );
                    let forward = src.count_forwards(split, match_pos, end);
                    if forward < min_match {
                        continue;
                    }
                    let backward = src.count_backwards(split, anchor, match_pos, window_low);
                    if best.is_none_or(|best| forward + backward > best.total()) {
                        best = Some(BucketMatch {
                            match_pos,
                            forward,
                            backward,
                        });
                    }
                }

                let Some(BucketMatch {
                    match_pos,
                    forward,
                    backward,
                }) = best
                else {
                    self.table.insert(bucket_hash, new_entry);
                    continue;
                };

                out.push(RawSequence {
                    literal_length: (split - backward - anchor) as u32,
                    match_length: (forward + backward) as u32,
                    offset: (split - match_pos) as u32,
                });

                // After the sequence, so the entry cannot clobber the winner.
                self.table.insert(bucket_hash, new_entry);
                anchor = split + forward;

                // A match running past what we hashed means a repeating,
                // overlapping pattern -- all zeros, say. One repetition meeting
                // the split criterion means every repetition does, and inserting
                // them all is what made this 20x slower on such input. Re-prime
                // at the anchor and skip the rest.
                if anchor > ip + hashed {
                    // C re-primes at the anchor here, and again the reset does
                    // not store. The skip itself is what matters: it is worth
                    // 20x on input that is a single byte repeated.
                    GearHash::upstream_reset_is_a_noop();
                    restart = Some(anchor - hashed);
                    break;
                }
            }

            ip = restart.unwrap_or(ip) + hashed;
        }

        end - anchor
    }

    /// C's `ZSTD_ldm_generateSequences` (`zstd_ldm.c:526`): the chunked outer
    /// loop, which is what bounds the offsets any one pass can emit.
    ///
    /// Called once per block rather than once per frame. C does the same, from
    /// inside its block compressor, and the shape matters: a whole-input
    /// pre-pass is not something a streaming encoder can do, and it would not
    /// reproduce the per-chunk truncation of the window either.
    ///
    /// The trailing literals a call does not cover are *dropped*, not carried:
    /// C's `leftoverSize` is a local to `ZSTD_ldm_generateSequences`, so it
    /// spans the 1 MiB chunks within one call and nothing more. The caller
    /// compresses the tail after the last sequence itself, so carrying the
    /// count into the next block would double-count those literals -- and the
    /// first sequence of every block but the first would claim literals from
    /// outside the block. Only a per-block comparison against C shows this;
    /// a single whole-input call cannot.
    ///
    /// `max_distance` is C's `1U << params->windowLog`, taken from the caller
    /// rather than from [`LdmParameters::window_log`] because the two can
    /// disagree here and cannot in C. `ZSTD_ldm_adjustParameters` copies
    /// `cParams.windowLog` into the LDM parameters, so C's matcher reaches
    /// exactly as far as the frame declares; this crate additionally caps a
    /// frame's history at [`MAX_DECLARABLE_WINDOW_SIZE`], and a matcher reaching
    /// past that cap would emit an offset the frame's own header forbids.
    ///
    /// [`MAX_DECLARABLE_WINDOW_SIZE`]: crate::frame::MAX_DECLARABLE_WINDOW_SIZE
    pub(crate) fn generate_sequences(
        &mut self,
        src: LdmSource<'_>,
        start: usize,
        end: usize,
        max_distance: usize,
        out: &mut Vec<RawSequence>,
    ) {
        let mut leftover_literals = 0u32;
        let mut chunk_start = start;
        while chunk_start < end {
            let chunk_end = (chunk_start + LDM_MAX_CHUNK_SIZE).min(end);
            // The offset has to be valid at the *end* of the sequence, because
            // a sequence may later be split across a block boundary. Bounding
            // against the chunk's end rather than its start is what keeps that
            // true.
            //
            // C's `ZSTD_window_enforceMaxDist`: a dictionary is reachable in
            // full for as long as its last byte is inside the window, so it is
            // credited against the distance rather than counted within it. The
            // floor stays on the floor until the frame outgrows that credit,
            // and the dictionary is invalidated at the moment it does.
            let window_low = if chunk_end > max_distance + self.loaded_dict_end {
                self.loaded_dict_end = 0;
                chunk_end - max_distance
            } else {
                0
            };
            let before = out.len();
            let leftover = self.generate_into(src, chunk_start, chunk_end, window_low, out);

            if out.len() > before {
                // The literals since the last match belong to the first
                // sequence this chunk produced.
                out[before].literal_length += leftover_literals;
                leftover_literals = leftover as u32;
            } else {
                leftover_literals += (chunk_end - chunk_start) as u32;
            }
            chunk_start = chunk_end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `entries` past `isize::MAX` bytes is a `capacity overflow` panic
    /// rather than a refused allocation, and `panic = "abort"` makes that
    /// unrecoverable. At eight bytes an entry, `ldm_hash_log` 28 and up reach
    /// it on a 32-bit target, one log earlier than the binary tree does.
    ///
    /// `bucket_size_log` has to follow the narrowed log rather than the
    /// requested one: [`LdmTable::bucket`] indexes `entries` by
    /// `hash << bucket_size_log` over `hash_bits` bits of hash.
    #[test]
    fn no_ldm_hash_log_in_range_asks_for_a_table_past_the_address_space() {
        // `ParameterOverrides::LDM_HASH_LOG`, which `validate` accepts in full
        // on every target.
        for hash_log in 6..=30u32 {
            let (built, bucket_size_log) = LdmTable::geometry(LdmParameters {
                window_log: 27,
                hash_log,
                min_match_length: 64,
                bucket_size_log: 3,
                hash_rate_log: 0,
            });
            assert_eq!(built, hash_log.min(MAX_HASH_LOG));
            assert!(bucket_size_log <= built);
            let bytes = (1usize << built).checked_mul(size_of::<LdmEntry>());
            assert!(
                bytes.is_some_and(|bytes| bytes <= isize::MAX as usize),
                "ldm_hash_log {hash_log} sizes the table past isize::MAX"
            );
        }
    }

    /// `ldm_hash_log` 28 used to abort the process on a 32-bit target. Nothing
    /// narrows it on the way in -- `adjust_upstream_cparams` reaches the
    /// parser's chain log, not this one -- so even one-shot reached it.
    ///
    /// 32-bit only, because that is where the abort was: [`MAX_HASH_LOG`] is 59
    /// where `usize` is 64 bits, so `ldm_hash_log` 28 is an ordinary parameter
    /// there and the encode proves nothing about the narrowing while still
    /// building two gibibytes of `entries`. The `wasm32-wasip1` leg of CI runs
    /// the suite on a genuine 32-bit `usize`, which is where this one has to
    /// pass.
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn a_wide_ldm_hash_log_encodes_instead_of_aborting() {
        let options = crate::EncoderOptions {
            compression_level: crate::CompressionLevel::try_new(3).unwrap(),
            parameters: crate::ParameterOverrides {
                long_distance_matching: LdmMode::Enabled,
                ldm_hash_log: Some(28),
                ..Default::default()
            },
            ..Default::default()
        };
        let input = b"a wide ldm hash log encodes a wide ldm hash log encodes";
        let encoded = crate::encode_all_with_options(input, options).unwrap();
        assert_eq!(crate::decode_all(&encoded).unwrap(), input);
    }

    /// `7 - strategy / 3` over C's 1-based codes, which is not the same curve
    /// as `7 - ordinal / 3` over 0-based ones: at `Greedy` (code 3) C gives 6
    /// and a 0-based reading gives 7.
    #[test]
    fn the_hash_rate_falls_in_steps_of_three_strategies() {
        let rate = |strategy| {
            LdmParameters::resolve(LdmParameterOverrides::default(), strategy, 27).hash_rate_log
        };
        assert_eq!(rate(UpstreamStrategy::Fast), 7);
        assert_eq!(rate(UpstreamStrategy::DoubleFast), 7);
        assert_eq!(rate(UpstreamStrategy::Greedy), 6);
        assert_eq!(rate(UpstreamStrategy::Lazy2), 6);
        assert_eq!(rate(UpstreamStrategy::BinaryTreeLazy2), 5);
        assert_eq!(rate(UpstreamStrategy::BinaryTreeUltra), 5);
        assert_eq!(rate(UpstreamStrategy::BinaryTreeUltra2), 4);
    }

    /// A supplied `hash_log` redirects where `hash_rate_log` comes from, rather
    /// than only replacing its own field.
    #[test]
    fn a_supplied_hash_log_derives_the_rate_from_the_window() {
        let resolved = LdmParameters::resolve(
            LdmParameterOverrides {
                hash_log: Some(20),
                ..LdmParameterOverrides::default()
            },
            UpstreamStrategy::Fast,
            27,
        );
        assert_eq!(resolved.hash_log, 20);
        // The window minus the supplied hash log, not the strategy mapping's 7.
        assert_eq!(resolved.hash_rate_log, 27 - 20);
    }

    /// C guards that subtraction with `windowLog > hashLog` and leaves the rate
    /// at zero otherwise, rather than clamping or wrapping.
    #[test]
    fn a_hash_log_wider_than_the_window_leaves_the_rate_at_zero() {
        let resolved = LdmParameters::resolve(
            LdmParameterOverrides {
                hash_log: Some(27),
                ..LdmParameterOverrides::default()
            },
            UpstreamStrategy::Fast,
            27,
        );
        assert_eq!(resolved.hash_rate_log, 0);
    }

    /// The minimum match halves at `btultra`, not at `btopt`.
    #[test]
    fn the_minimum_match_halves_from_btultra_up() {
        let min_match = |strategy| {
            LdmParameters::resolve(LdmParameterOverrides::default(), strategy, 27).min_match_length
        };
        assert_eq!(min_match(UpstreamStrategy::BinaryTreeOpt), 64);
        assert_eq!(min_match(UpstreamStrategy::BinaryTreeUltra), 32);
        assert_eq!(min_match(UpstreamStrategy::BinaryTreeUltra2), 32);
    }

    /// The bucket clamp's lower bound is 4, so the fast strategies do not get
    /// buckets of their own code.
    #[test]
    fn the_bucket_size_is_clamped_into_four_to_eight() {
        let bucket = |strategy| {
            LdmParameters::resolve(LdmParameterOverrides::default(), strategy, 27).bucket_size_log
        };
        assert_eq!(bucket(UpstreamStrategy::Fast), 4);
        assert_eq!(bucket(UpstreamStrategy::Lazy), 4);
        assert_eq!(bucket(UpstreamStrategy::Lazy2), 5);
        assert_eq!(bucket(UpstreamStrategy::BinaryTreeUltra2), 8);
    }

    /// The final `min(bucket_size_log, hash_log)` sits outside the unset check,
    /// so it binds a value the caller supplied as well as a derived one. This
    /// is the one line of the derivation that a field-by-field resolution would
    /// miss entirely.
    #[test]
    fn the_hash_log_caps_even_a_supplied_bucket_size() {
        let resolved = LdmParameters::resolve(
            LdmParameterOverrides {
                hash_log: Some(6),
                bucket_size_log: Some(8),
                ..LdmParameterOverrides::default()
            },
            UpstreamStrategy::BinaryTreeUltra2,
            27,
        );
        assert_eq!(resolved.hash_log, 6);
        assert_eq!(
            resolved.bucket_size_log, 6,
            "the cap applies to a supplied value"
        );
    }

    /// Only level 22's window reaches 27, so the auto rule is far narrower than
    /// "levels 16 and up".
    #[test]
    fn the_auto_rule_needs_both_a_wide_window_and_an_optimal_strategy() {
        use UpstreamStrategy::{BinaryTreeLazy2, BinaryTreeOpt, BinaryTreeUltra2};
        assert!(resolve_enable_ldm(LdmMode::Auto, BinaryTreeOpt, 27));
        assert!(resolve_enable_ldm(LdmMode::Auto, BinaryTreeUltra2, 27));
        // Level 21's window is 26: optimal, and still off.
        assert!(!resolve_enable_ldm(LdmMode::Auto, BinaryTreeUltra2, 26));
        // btlazy2 at a forced 27: wide enough, and still off.
        assert!(!resolve_enable_ldm(LdmMode::Auto, BinaryTreeLazy2, 27));
    }

    #[test]
    fn the_explicit_modes_ignore_the_parameters() {
        for window_log in [10u32, 27] {
            for strategy in [UpstreamStrategy::Fast, UpstreamStrategy::BinaryTreeUltra2] {
                assert!(resolve_enable_ldm(LdmMode::Enabled, strategy, window_log));
                assert!(!resolve_enable_ldm(LdmMode::Disabled, strategy, window_log));
            }
        }
    }

    /// The force applies to `Enabled` only. `Auto` is resolved after the
    /// parameters are fitted, so it never reaches the assignment that would
    /// widen them.
    #[test]
    fn only_an_explicit_enable_pins_the_window() {
        assert_eq!(force_window_log(LdmMode::Enabled, 10), 27);
        assert_eq!(force_window_log(LdmMode::Enabled, 27), 27);
        assert_eq!(force_window_log(LdmMode::Auto, 10), 10);
        assert_eq!(force_window_log(LdmMode::Disabled, 10), 10);
    }
}

/// Emits the grid `oracles/ldm/ldm_oracle.c` prints from C's own
/// `ZSTD_ldm_adjustParameters`, so the transcription above can be diffed
/// against the pinned checkout rather than against a reading of it.
///
/// `./oracles/ldm/compare.sh` runs both sides and diffs them. Reading was not
/// enough: the first version of `resolve` was written straight from the C and
/// still differed on 3261 of 5184 rows, because C spells "unset" as zero
/// throughout and an `Option` spells it as `None`.
#[cfg(test)]
#[test]
#[ignore]
fn print_resolution_grid() {
    const WINDOWS: [u32; 4] = [10, 15, 20, 27];
    const HASH_LOGS: [u32; 4] = [0, 6, 20, 27];
    const BUCKETS: [u32; 4] = [0, 1, 4, 8];
    const RATES: [u32; 3] = [0, 3, 7];
    const MIN_MATCH: [u32; 3] = [0, 4, 4096];
    const STRATEGIES: [UpstreamStrategy; 9] = [
        UpstreamStrategy::Fast,
        UpstreamStrategy::DoubleFast,
        UpstreamStrategy::Greedy,
        UpstreamStrategy::Lazy,
        UpstreamStrategy::Lazy2,
        UpstreamStrategy::BinaryTreeLazy2,
        UpstreamStrategy::BinaryTreeOpt,
        UpstreamStrategy::BinaryTreeUltra,
        UpstreamStrategy::BinaryTreeUltra2,
    ];
    for strategy in STRATEGIES {
        for window_log in WINDOWS {
            for hash_log in HASH_LOGS {
                for bucket_size_log in BUCKETS {
                    for hash_rate_log in RATES {
                        for min_match_length in MIN_MATCH {
                            let resolved = LdmParameters::resolve(
                                LdmParameterOverrides {
                                    hash_log: Some(hash_log),
                                    min_match_length: Some(min_match_length),
                                    bucket_size_log: Some(bucket_size_log),
                                    hash_rate_log: Some(hash_rate_log),
                                },
                                strategy,
                                window_log,
                            );
                            println!(
                                "{},{window_log},{hash_log},{bucket_size_log},{hash_rate_log},\
                                 {min_match_length},{},{},{},{}",
                                strategy.as_upstream_code(),
                                resolved.hash_log,
                                resolved.min_match_length,
                                resolved.bucket_size_log,
                                resolved.hash_rate_log,
                            );
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod matcher_tests {
    use super::*;

    fn params_for(strategy: UpstreamStrategy, window_log: u32) -> LdmParameters {
        LdmParameters::resolve(LdmParameterOverrides::default(), strategy, window_log)
    }

    /// Bytes with no long repeat of its own, so any match the matcher reports
    /// is one this test planted rather than an accident of the generator.
    fn filler(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut state = seed | 1;
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Every sequence a run reports must describe bytes that are actually
    /// equal, at a distance the frame could replay. Checking the *content*
    /// rather than the count is the difference between "it found something"
    /// and "it found a match".
    fn assert_sequences_are_real(src: &[u8], sequences: &[RawSequence], window_log: u32) {
        let mut pos = 0usize;
        for (index, seq) in sequences.iter().enumerate() {
            pos += seq.literal_length as usize;
            let offset = seq.offset as usize;
            let length = seq.match_length as usize;
            assert!(offset > 0, "sequence {index} has a zero offset");
            assert!(
                offset <= pos,
                "sequence {index}: offset {offset} reaches before the frame at {pos}"
            );
            assert!(
                offset <= 1usize << window_log,
                "sequence {index}: offset {offset} exceeds the {} window",
                1usize << window_log
            );
            assert!(
                pos + length <= src.len(),
                "sequence {index} runs past the input"
            );
            for k in 0..length {
                assert_eq!(
                    src[pos + k],
                    src[pos - offset + k],
                    "sequence {index} byte {k}: not a real match"
                );
            }
            pos += length;
        }
    }

    /// A repeat further apart than any per-block parser's window, which is the
    /// whole reason LDM exists.
    #[test]
    fn a_repeat_far_past_a_block_is_found() {
        const GAP: usize = 3 << 20;
        let planted = filler(1 << 16, 0x5EED);
        let mut src = planted.clone();
        src.extend_from_slice(&filler(GAP, 0xA11CE));
        src.extend_from_slice(&planted);

        let params = params_for(UpstreamStrategy::BinaryTreeUltra2, 27);
        let mut state = LdmState::new(params);
        let mut sequences = Vec::new();
        state.generate_sequences(
            LdmSource::contiguous(&src),
            0,
            src.len(),
            1 << params.window_log,
            &mut sequences,
        );

        assert!(
            !sequences.is_empty(),
            "no long-distance match found across a {GAP}-byte gap"
        );
        assert_sequences_are_real(&src, &sequences, params.window_log);

        // The planted repeat is what should be covered -- the gap between the
        // two copies is unrepeating filler and there is nothing there to match.
        let longest = sequences.iter().map(|s| s.match_length).max().unwrap() as usize;
        assert!(
            longest >= planted.len(),
            "longest match {longest} does not cover the {}-byte planted repeat",
            planted.len()
        );
        let far = sequences
            .iter()
            .filter(|s| s.offset as usize > 1 << 20)
            .count();
        assert!(far > 0, "no sequence reached past a megabyte");
    }

    /// Run the matcher over `frame` with `dictionary` in front of it, returning
    /// the sequences and the joint buffer they are expressed against.
    ///
    /// The joint buffer is materialised here only so the assertions can index
    /// it; the matcher itself never sees the two slices joined.
    fn generate_with_dictionary(
        dictionary: &[u8],
        frame: &[u8],
        strategy: UpstreamStrategy,
        window_log: u32,
    ) -> (Vec<RawSequence>, Vec<u8>) {
        let params = params_for(strategy, window_log);
        let mut state = LdmState::new(params);
        state.fill_from_dictionary(dictionary);
        let mut sequences = Vec::new();
        state.generate_sequences(
            LdmSource::with_dictionary(dictionary, frame),
            dictionary.len(),
            dictionary.len() + frame.len(),
            1 << params.window_log,
            &mut sequences,
        );
        let mut joint = dictionary.to_vec();
        joint.extend_from_slice(frame);
        (sequences, joint)
    }

    /// Every reported match is a real one in the joint index space, and the
    /// literal runs tile the frame exactly.
    fn assert_sequences_are_real_across(joint: &[u8], from: usize, sequences: &[RawSequence]) {
        let mut pos = from;
        for (index, seq) in sequences.iter().enumerate() {
            pos += seq.literal_length as usize;
            let (offset, length) = (seq.offset as usize, seq.match_length as usize);
            assert!(
                offset <= pos,
                "sequence {index}: offset {offset} reaches before the dictionary at {pos}"
            );
            assert!(
                pos + length <= joint.len(),
                "sequence {index} runs past the joint buffer"
            );
            for k in 0..length {
                assert_eq!(
                    joint[pos + k],
                    joint[pos - offset + k],
                    "sequence {index} byte {k}: not a real match"
                );
            }
            pos += length;
        }
    }

    /// The point of the whole feature: a repeat whose only earlier copy is in
    /// the dictionary.
    #[test]
    fn a_match_is_found_in_the_dictionary() {
        let planted = filler(1 << 16, 0x5EED);
        let mut dictionary = planted.clone();
        dictionary.extend_from_slice(&filler(1 << 16, 0x0DDBA11));
        let mut frame = filler(1 << 19, 0xA11CE);
        frame.extend_from_slice(&planted);

        let (sequences, joint) =
            generate_with_dictionary(&dictionary, &frame, UpstreamStrategy::BinaryTreeUltra2, 27);

        assert!(!sequences.is_empty(), "no match found in the dictionary");
        assert_sequences_are_real_across(&joint, dictionary.len(), &sequences);

        // The match has to reach back past the whole frame, which is the only
        // way to land in the dictionary at all.
        let into_dictionary = sequences
            .iter()
            .filter(|seq| seq.offset as usize > frame.len() - planted.len())
            .count();
        assert!(
            into_dictionary > 0,
            "every match stayed inside the frame; the dictionary was never searched"
        );
    }

    /// A match that starts inside the dictionary and runs off its end into the
    /// frame.
    ///
    /// This is the case C needs `ZSTD_count_2segments` and
    /// `ZSTD_ldm_countBackwardsMatch_2segments` for, because its dictionary and
    /// its frame are two allocations. Here they are one index space and the
    /// crossing is ordinary counting -- which is worth a test precisely because
    /// nothing in the code marks the boundary.
    #[test]
    fn a_match_runs_from_the_dictionary_into_the_frame() {
        // The dictionary ends with `planted`, and the frame opens with it, so
        // the repeat later in the frame can match a run that begins in the
        // dictionary and continues past its last byte.
        let planted = filler(1 << 16, 0xC0FFEE);
        let mut dictionary = filler(1 << 16, 0xBEEF);
        dictionary.extend_from_slice(&planted);
        let mut frame = planted.clone();
        frame.extend_from_slice(&filler(1 << 19, 0xA11CE));
        frame.extend_from_slice(&planted);
        frame.extend_from_slice(&planted);

        let (sequences, joint) =
            generate_with_dictionary(&dictionary, &frame, UpstreamStrategy::BinaryTreeUltra2, 27);

        assert!(!sequences.is_empty(), "no match found");
        assert_sequences_are_real_across(&joint, dictionary.len(), &sequences);

        // Walk the tiling once, asking of each match where it sits in the joint
        // space: one that begins below the boundary and ends above it is a match
        // read out of both slices.
        let mut position = dictionary.len();
        let mut crossing = false;
        for seq in &sequences {
            position += seq.literal_length as usize;
            let match_start = position - seq.offset as usize;
            crossing |= match_start < dictionary.len()
                && match_start + seq.match_length as usize > dictionary.len();
            position += seq.match_length as usize;
        }
        assert!(
            crossing,
            "no match crossed the dictionary boundary, so the joining is untested"
        );
    }

    /// C credits the dictionary against the window rather than counting it
    /// within one, and then invalidates it outright the moment the frame
    /// outgrows that credit (`ZSTD_window_enforceMaxDist`).
    ///
    /// So the same dictionary match is reachable early in a frame and
    /// unreachable later in it, at a boundary the window decides.
    #[test]
    fn the_dictionary_stops_being_reachable_once_the_frame_outgrows_the_window() {
        const WINDOW_LOG: u32 = 18;
        let planted = filler(1 << 16, 0x5EED);
        let dictionary = planted.clone();

        // Early: the frame is still well inside the window, so the dictionary
        // keeps its credit and the repeat is found.
        let mut early = filler(1 << 15, 0xA11CE);
        early.extend_from_slice(&planted);
        let (found_early, joint) = generate_with_dictionary(
            &dictionary,
            &early,
            UpstreamStrategy::BinaryTreeUltra2,
            WINDOW_LOG,
        );
        assert_sequences_are_real_across(&joint, dictionary.len(), &found_early);
        assert!(
            !found_early.is_empty(),
            "the dictionary was unreachable while it still held its credit"
        );

        // Late: the same repeat, placed past the window. The dictionary has
        // been invalidated by then and there is nothing else to match.
        let mut late = filler(3 << WINDOW_LOG, 0xA11CE);
        late.extend_from_slice(&planted);
        let (found_late, joint) = generate_with_dictionary(
            &dictionary,
            &late,
            UpstreamStrategy::BinaryTreeUltra2,
            WINDOW_LOG,
        );
        assert_sequences_are_real_across(&joint, dictionary.len(), &found_late);
        assert!(
            found_late.is_empty(),
            "the dictionary was still reachable {} bytes into a {} window",
            late.len(),
            1usize << WINDOW_LOG
        );
    }

    /// Filling from a dictionary is a different walk from generating over the
    /// same bytes, so the two do not file the same positions.
    ///
    /// This is why a dictionary cannot be handled by widening the range
    /// generation is asked to search. C spells the difference out as two
    /// functions -- `ZSTD_ldm_fillHashTable` primes nothing and discards the
    /// splits that fall too near the start, while
    /// `ZSTD_ldm_generateSequences_internal` primes over the first
    /// `minMatchLength` bytes and begins after them.
    #[test]
    fn filling_from_a_dictionary_is_not_the_walk_generation_makes() {
        let bytes = filler(1 << 18, 0x1234);
        let params = params_for(UpstreamStrategy::BinaryTreeUltra2, 27);

        let mut filled = LdmState::new(params);
        filled.fill_from_dictionary(&bytes);

        let mut generated = LdmState::new(params);
        let mut sequences = Vec::new();
        generated.generate_sequences(
            LdmSource::contiguous(&bytes),
            0,
            bytes.len(),
            1 << params.window_log,
            &mut sequences,
        );

        assert_ne!(
            filled.table.entries, generated.table.entries,
            "the two walks filed the same positions, so one of them is not what it claims"
        );
    }

    /// Nothing to find means nothing reported: a matcher that emits sequences
    /// on incompressible input would corrupt every frame it touched.
    #[test]
    fn unrepeating_input_yields_no_sequences() {
        let src = filler(1 << 20, 0xD1CE);
        let params = params_for(UpstreamStrategy::BinaryTreeUltra2, 27);
        let mut state = LdmState::new(params);
        let mut sequences = Vec::new();
        state.generate_sequences(
            LdmSource::contiguous(&src),
            0,
            src.len(),
            1 << params.window_log,
            &mut sequences,
        );
        assert!(
            sequences.is_empty(),
            "found {} matches in unrepeating input",
            sequences.len()
        );
    }

    /// The window bounds what the matcher may emit, exactly as it bounds the
    /// per-block parsers. A repeat further apart than the window has to go
    /// unreported rather than be reported at an offset the decoder cannot use.
    #[test]
    fn a_repeat_further_than_the_window_is_not_reported() {
        const WINDOW_LOG: u32 = 20;
        let planted = filler(1 << 16, 0x5EED);
        let mut src = planted.clone();
        src.extend_from_slice(&filler(3 << 20, 0xA11CE));
        src.extend_from_slice(&planted);

        let params = params_for(UpstreamStrategy::BinaryTreeUltra2, WINDOW_LOG);
        let mut state = LdmState::new(params);
        let mut sequences = Vec::new();
        state.generate_sequences(
            LdmSource::contiguous(&src),
            0,
            src.len(),
            1 << params.window_log,
            &mut sequences,
        );
        assert_sequences_are_real(&src, &sequences, WINDOW_LOG);
    }

    /// All-zero input is the pathological case the skip-ahead exists for: every
    /// repetition meets the split criterion, so inserting them all is what made
    /// C 20x slower before it skipped. The matcher must still terminate and
    /// still report real matches.
    #[test]
    fn a_single_repeated_byte_terminates() {
        let src = vec![0u8; 1 << 20];
        let params = params_for(UpstreamStrategy::BinaryTreeUltra2, 27);
        let mut state = LdmState::new(params);
        let mut sequences = Vec::new();
        state.generate_sequences(
            LdmSource::contiguous(&src),
            0,
            src.len(),
            1 << params.window_log,
            &mut sequences,
        );
        assert_sequences_are_real(&src, &sequences, params.window_log);
    }

    /// Feeding one block at a time must find the same matches, at the same
    /// places, as feeding the whole input: the table lives for the frame, so if
    /// it did not, a streaming encoder could never reproduce a one-shot frame.
    ///
    /// The comparison is on absolute positions rather than on the raw
    /// sequences, because the literal lengths legitimately differ. Each call
    /// measures literals from its own start and drops the ones after its last
    /// match, so a block boundary between two matches shortens the second
    /// match's literal run by everything before the boundary. Asserting the raw
    /// sequences equal is the mistake this test used to make, and it enshrined
    /// a leftover count that carried across blocks -- which is not what C does
    /// and would have double-counted those literals downstream.
    #[test]
    fn per_block_calls_find_the_same_matches_as_one_call() {
        let planted = filler(1 << 15, 0x5EED);
        let mut src = planted.clone();
        src.extend_from_slice(&filler(1 << 19, 0xA11CE));
        src.extend_from_slice(&planted);

        let params = params_for(UpstreamStrategy::BinaryTreeUltra2, 27);
        const BLOCK: usize = 128 << 10;

        let mut whole = Vec::new();
        LdmState::new(params).generate_sequences(
            LdmSource::contiguous(&src),
            0,
            src.len(),
            1 << params.window_log,
            &mut whole,
        );
        assert!(!whole.is_empty(), "the planted repeat produced no match");

        let mut blocked = Vec::new();
        let mut state = LdmState::new(params);
        let mut start = 0usize;
        while start < src.len() {
            let end = (start + BLOCK).min(src.len());
            let mut block = Vec::new();
            state.generate_sequences(
                LdmSource::contiguous(&src),
                start,
                end,
                1 << params.window_log,
                &mut block,
            );

            // Every sequence a call produces has to fit inside the range it was
            // given, or the caller cannot lay it down.
            let covered: usize = block
                .iter()
                .map(|seq: &RawSequence| (seq.literal_length + seq.match_length) as usize)
                .sum();
            assert!(
                covered <= end - start,
                "block [{start}, {end}) produced {covered} bytes of sequences"
            );

            // Absolute start of each match, rebuilt the way the block
            // compressor walks the store.
            let mut position = start;
            for seq in &block {
                position += seq.literal_length as usize;
                blocked.push((position, seq.match_length, seq.offset));
                position += seq.match_length as usize;
            }
            start = end;
        }

        let mut position = 0usize;
        let expected: Vec<_> = whole
            .iter()
            .map(|seq| {
                position += seq.literal_length as usize;
                let at = position;
                position += seq.match_length as usize;
                (at, seq.match_length, seq.offset)
            })
            .collect();

        assert_eq!(
            expected, blocked,
            "block-at-a-time generation found different matches"
        );
    }
}

/// Writes the corpus `oracles/ldm/compare.sh` feeds to both matchers, then
/// prints this crate's sequences for it in the same CSV shape
/// `oracles/ldm/ldm_seq_oracle.c` prints C's.
#[cfg(test)]
#[test]
#[ignore]
fn print_sequences_for_corpus() {
    let path = std::env::var("ZSTANDARD_LDM_CORPUS").expect("ZSTANDARD_LDM_CORPUS");
    let window_log: u32 = std::env::var("ZSTANDARD_LDM_WINDOW_LOG")
        .expect("ZSTANDARD_LDM_WINDOW_LOG")
        .parse()
        .expect("a window log");
    let strategy_code: u32 = std::env::var("ZSTANDARD_LDM_STRATEGY")
        .expect("ZSTANDARD_LDM_STRATEGY")
        .parse()
        .expect("a strategy code");
    const STRATEGIES: [UpstreamStrategy; 9] = [
        UpstreamStrategy::Fast,
        UpstreamStrategy::DoubleFast,
        UpstreamStrategy::Greedy,
        UpstreamStrategy::Lazy,
        UpstreamStrategy::Lazy2,
        UpstreamStrategy::BinaryTreeLazy2,
        UpstreamStrategy::BinaryTreeOpt,
        UpstreamStrategy::BinaryTreeUltra,
        UpstreamStrategy::BinaryTreeUltra2,
    ];
    let strategy = STRATEGIES[(strategy_code - 1) as usize];

    let src = std::fs::read(&path).expect("corpus");
    let block_size: usize = std::env::var("ZSTANDARD_LDM_BLOCK_SIZE")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&value| value != 0)
        .unwrap_or(src.len());

    // Zero and unset are the same thing here, as they are in C, so an absent
    // variable and a variable set to `0` both leave the field to derive.
    //
    // A value that will not parse is a typo, not a request to derive. This
    // oracle exists to prove both sides resolved the *same* parameters, so
    // quietly dropping one would turn a mistyped run into a false "identical".
    let supplied = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|value| {
                value
                    .parse::<u32>()
                    .unwrap_or_else(|_| panic!("{name} must be an integer, got {value:?}"))
            })
            .filter(|&value| value != 0)
    };
    let overrides = LdmParameterOverrides {
        hash_log: supplied("ZSTANDARD_LDM_HASH_LOG"),
        min_match_length: supplied("ZSTANDARD_LDM_MIN_MATCH"),
        bucket_size_log: supplied("ZSTANDARD_LDM_BUCKET_SIZE_LOG"),
        hash_rate_log: supplied("ZSTANDARD_LDM_HASH_RATE_LOG"),
    };

    // The first `dict_size` bytes stand in for a dictionary, exactly as they do
    // in `ldm_seq_oracle.c`: hashed in up front by a different walk, then
    // searchable but never searched *over*.
    let dict_size: usize = std::env::var("ZSTANDARD_LDM_DICT_SIZE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
        .min(src.len());
    let (dictionary, frame) = src.split_at(dict_size);
    let source = LdmSource::with_dictionary(dictionary, frame);

    let params = LdmParameters::resolve(overrides, strategy, window_log);
    let mut state = LdmState::new(params);
    if dict_size > 0 {
        state.fill_from_dictionary(dictionary);
    }
    let mut sequences = Vec::new();
    let mut start = dict_size;
    while start < src.len() {
        let end = (start + block_size).min(src.len());
        sequences.clear();
        state.generate_sequences(source, start, end, 1 << params.window_log, &mut sequences);
        for seq in &sequences {
            println!(
                "{},{},{},{}",
                start, seq.literal_length, seq.match_length, seq.offset
            );
        }
        start = end;
    }
}

/// Writes one of the benchmark corpora to `ZSTANDARD_LDM_CORPUS_OUT`, so the
/// matcher can be diffed against C on the same bytes the encoder diverges on
/// rather than only on a corpus built to make it easy.
#[cfg(test)]
#[test]
#[ignore]
fn write_benchmark_corpus() {
    #[path = "../support/corpora.rs"]
    mod corpora;

    let name = std::env::var("ZSTANDARD_LDM_CORPUS_NAME").expect("ZSTANDARD_LDM_CORPUS_NAME");
    let out = std::env::var("ZSTANDARD_LDM_CORPUS_OUT").expect("ZSTANDARD_LDM_CORPUS_OUT");
    let size: usize = std::env::var("ZSTANDARD_LDM_CORPUS_SIZE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1 << 20);
    // Generated at a fixed size and then truncated, so a prefix of the corpus
    // is the same bytes at every length. Regenerating at each length would not
    // be: the generators shape their output to the size they are given.
    let case = corpora::benchmark_report_cases(1 << 20)
        .into_iter()
        .find(|case| case.name == name)
        .expect("a corpus by that name");
    std::fs::write(&out, &case.input[..size.min(case.input.len())]).expect("write the corpus");
}
