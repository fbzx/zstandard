//! Property tests covering invariants that hold across all inputs and
//! all configurations the public API exposes.
//!
//! These are deliberately small in case count (default 64 each, configurable
//! via `PROPTEST_CASES`) so they stay fast in CI. The fuzz targets in
//! `fuzz/fuzz_targets/` carry the heavier coverage.

use proptest::collection::vec;
use proptest::prelude::*;

// Aliased because proptest's own `Strategy` trait is in scope throughout this
// file, and the two are unrelated.
use zstandard::Strategy as MatchStrategy;
use zstandard::{
    CompressionLevel, DecoderDictionary, DecoderOptions, EncoderDictionary, EncoderOptions, Format,
    FrameHeader, ParameterOverrides, StreamingDecoder, StreamingEncoder, decode_all,
    decode_all_with_dict, decode_all_with_options, encode_all_with_dict_and_options,
    encode_all_with_options, parse_block_header, parse_frame_header,
};

const MAX_INPUT_BYTES: usize = 16 * 1024;
const MAX_CHUNK_BYTES: usize = 4 * 1024;
const MAX_DICTIONARY_BYTES: usize = 4 * 1024;

/// Large-input properties run past the point where the encoder switches
/// behavior: above one block (128 KiB), and above the ~1 MiB threshold where
/// cross-block state starts to matter. Byte-level generation at this size is
/// too slow for proptest, so these inputs are built from a small generated
/// seed instead — cheap to produce, cheap to shrink, and compressible enough
/// to actually exercise the match finder.
const LARGE_INPUT_MIN_BYTES: usize = 1_500_000;

/// Streaming properties compare streaming against one-shot, so they encode the
/// same body twice at every level, including the optimal ones. They use a
/// smaller input that still spans several blocks and exceeds the retained-
/// history threshold, so they keep their coverage without dominating the run.
const STREAMING_INPUT_MIN_BYTES: usize = 400 * 1024;

fn arb_input() -> impl Strategy<Value = Vec<u8>> {
    vec(any::<u8>(), 0..=MAX_INPUT_BYTES)
}

/// Every ordinary level, not just the fast families. Levels 16 and up select
/// the optimal parsers, which are the newest and most intricate encoder paths
/// in the crate.
///
/// Negative levels are deliberately excluded: this feeds the properties that
/// assert a compression *ratio*, and fast mode emits raw literals, so its
/// ratio is a different question with a different bound.
fn arb_level() -> impl Strategy<Value = i32> {
    1i32..=CompressionLevel::MAX.as_i32()
}

/// Every level the public API accepts, including the negative "fast mode"
/// levels, for the properties that assert only round-trip fidelity.
///
/// Fast mode is a distinct encoder configuration rather than just a faster
/// one: it disables Huffman coding of the literals section, so the whole
/// literals path runs differently from every positive level. Weighted toward
/// the ordinary levels, which still cover eight more parser strategies.
fn arb_any_level() -> impl Strategy<Value = i32> {
    prop_oneof![
        3 => 1i32..=CompressionLevel::MAX.as_i32(),
        1 => -64i32..=0,
        1 => Just(CompressionLevel::MIN.as_i32()),
    ]
}

/// Any override set the encoder accepts, drawn from the whole of each
/// parameter's public bounds rather than from values a level would pick.
///
/// The bounds come from the public constants, so a change to either end is
/// covered here without this file being edited — and a bound that was widened
/// past what the encoder can actually take fails these properties rather than
/// waiting for a caller to find it.
fn arb_overrides() -> impl Strategy<Value = ParameterOverrides> {
    /// A hash or chain table is `1 << log` 32-bit entries, and adjustment only
    /// shrinks it when the source size is known — which the streaming half of
    /// the round-trip property never gives it. Drawn from the full published
    /// range these properties would spend their time in four-gibibyte
    /// allocations; 22 is 16 MiB and still far wider than any level's row. The
    /// ends of the published range are walked exactly, and cheaply, by
    /// `parameter_overrides_reject_values_outside_their_bounds`.
    const MAX_TABLE_LOG: u32 = 22;

    fn field(bounds: zstandard::ParameterBounds) -> impl Strategy<Value = Option<u32>> {
        prop_oneof![2 => Just(None), 1 => (bounds.min..=bounds.max).prop_map(Some)]
    }
    fn table_field(bounds: zstandard::ParameterBounds) -> impl Strategy<Value = Option<u32>> {
        field(bounds).prop_map(|value| value.map(|log| log.min(MAX_TABLE_LOG)))
    }
    (
        field(ParameterOverrides::WINDOW_LOG),
        table_field(ParameterOverrides::HASH_LOG),
        table_field(ParameterOverrides::CHAIN_LOG),
        field(ParameterOverrides::SEARCH_LOG),
        field(ParameterOverrides::MIN_MATCH),
        field(ParameterOverrides::TARGET_LENGTH),
        prop_oneof![2 => Just(None), 1 => arb_strategy().prop_map(Some)],
    )
        .prop_map(
            |(window_log, hash_log, chain_log, search_log, min_match, target_length, strategy)| {
                ParameterOverrides {
                    window_log,
                    hash_log,
                    chain_log,
                    search_log,
                    min_match,
                    target_length,
                    strategy,
                    ..ParameterOverrides::default()
                }
            },
        )
}

fn arb_strategy() -> impl Strategy<Value = MatchStrategy> {
    prop_oneof![
        Just(MatchStrategy::Fast),
        Just(MatchStrategy::DoubleFast),
        Just(MatchStrategy::Greedy),
        Just(MatchStrategy::Lazy),
        Just(MatchStrategy::Lazy2),
        Just(MatchStrategy::BinaryTreeLazy2),
        Just(MatchStrategy::BinaryTreeOpt),
        Just(MatchStrategy::BinaryTreeUltra),
        Just(MatchStrategy::BinaryTreeUltra2),
    ]
}

fn arb_chunks() -> impl Strategy<Value = Vec<usize>> {
    vec(1usize..=MAX_CHUNK_BYTES, 1..=8)
}

/// A shape of large input, described compactly so proptest can generate and
/// shrink it cheaply.
#[derive(Debug, Clone)]
struct LargeInput {
    /// Distinct line templates cycled through; more templates means more
    /// distinct matches and longer search chains.
    templates: usize,
    /// Period, in bytes, at which the whole body repeats verbatim. Matches at
    /// this distance are only findable if history reaches back that far.
    repeat_period: usize,
    /// Bytes of incompressible filler injected between repeats.
    noise: usize,
}

fn arb_large_input() -> impl Strategy<Value = LargeInput> {
    (1usize..=16, 0usize..=3, 0usize..=64 * 1024).prop_map(|(templates, period_shift, noise)| {
        LargeInput {
            templates,
            repeat_period: (128 * 1024) << period_shift,
            noise,
        }
    })
}

impl LargeInput {
    fn build(&self) -> Vec<u8> {
        self.build_at_least(LARGE_INPUT_MIN_BYTES)
    }

    fn build_streaming(&self) -> Vec<u8> {
        self.build_at_least(STREAMING_INPUT_MIN_BYTES)
    }

    fn build_at_least(&self, min_len: usize) -> Vec<u8> {
        let mut body = Vec::with_capacity(self.repeat_period);
        let mut i = 0u64;
        while body.len() < self.repeat_period {
            let template = i % self.templates as u64;
            body.extend_from_slice(
                format!(
                    "2026-07-27T12:00:{:02}Z tmpl={template} seq={i} path=/api/v{}/items status=200\n",
                    i % 60,
                    template % 3
                )
                .as_bytes(),
            );
            i += 1;
        }
        body.truncate(self.repeat_period);

        let mut noise = Vec::with_capacity(self.noise);
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..self.noise {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            noise.push((state >> 24) as u8);
        }

        let mut out = Vec::with_capacity(min_len + self.repeat_period);
        while out.len() < min_len {
            out.extend_from_slice(&body);
            out.extend_from_slice(&noise);
        }
        out
    }
}

fn stream_encode(input: &[u8], options: EncoderOptions, chunk: usize) -> Vec<u8> {
    let mut encoder = StreamingEncoder::new(options).unwrap();
    let mut compressed = encoder.take_output();
    for piece in input.chunks(chunk) {
        encoder.push(piece).unwrap();
        compressed.extend_from_slice(&encoder.take_output());
    }
    encoder.finish().unwrap();
    compressed.extend_from_slice(&encoder.take_output());
    compressed
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64),
    ))]

    /// One-shot encode then decode reproduces the input exactly, at every
    /// public compression level.
    #[test]
    fn one_shot_roundtrip_all_levels(input in arb_input(), level in arb_any_level()) {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };
        let compressed = encode_all_with_options(&input, options).unwrap();
        let restored = decode_all(&compressed).unwrap();
        prop_assert_eq!(restored, input);
    }

    /// Any override set inside the published bounds still round-trips, at any
    /// level, through both the one-shot and streaming encoders.
    ///
    /// Overrides reach parameter combinations no level produces, which is
    /// where the encoder has the least prior coverage: a `window_log` of 10
    /// against a 16 KiB body, a binary tree two probes deep, a hash wider than
    /// the tagged tables were built for. The output need not be upstream's
    /// here — `parameter_overrides_are_byte_identical_to_upstream` is where
    /// that is measured — but it must be a frame this crate can read back.
    #[test]
    fn overrides_roundtrip_within_their_bounds(
        input in arb_input(),
        level in arb_any_level(),
        parameters in arb_overrides(),
    ) {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            parameters,
            ..Default::default()
        };
        let compressed = encode_all_with_options(&input, options).unwrap();
        prop_assert_eq!(decode_all(&compressed).unwrap(), input.clone());

        let mut encoder = StreamingEncoder::new(options).unwrap();
        let mut streamed = Vec::new();
        for chunk in input.chunks(MAX_CHUNK_BYTES.max(1)) {
            encoder.push(chunk).unwrap();
            streamed.extend_from_slice(&encoder.take_output());
        }
        encoder.finish().unwrap();
        streamed.extend_from_slice(&encoder.take_output());
        prop_assert_eq!(decode_all(&streamed).unwrap(), input);
    }

    /// A magicless frame is the standard frame minus its first four bytes, and
    /// a decoder told to expect one reads it back.
    #[test]
    fn magicless_frames_are_the_standard_frame_without_its_magic(
        input in arb_input(),
        level in arb_any_level(),
    ) {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };
        let standard = encode_all_with_options(&input, options).unwrap();
        let magicless = encode_all_with_options(
            &input,
            EncoderOptions { format: Format::Zstd1Magicless, ..options },
        )
        .unwrap();
        prop_assert_eq!(&magicless[..], &standard[4..]);

        let decoder_options = DecoderOptions {
            format: Format::Zstd1Magicless,
            ..Default::default()
        };
        prop_assert_eq!(
            decode_all_with_options(&magicless, decoder_options).unwrap(),
            input
        );
    }

    /// A pledged size that matches the stream produces a frame declaring that
    /// exact content size; one that does not is reported at `finish`.
    #[test]
    fn a_pledged_size_is_declared_and_checked(
        input in arb_input(),
        level in arb_any_level(),
        skew in 0usize..=3,
    ) {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            pledged_src_size: Some(input.len() as u64),
            ..Default::default()
        };
        let mut encoder = StreamingEncoder::new(options).unwrap();
        encoder.push(&input).unwrap();
        encoder.finish().unwrap();
        let frame = encoder.take_output();
        prop_assert_eq!(decode_all(&frame).unwrap(), input.clone());
        match parse_frame_header(&frame).unwrap() {
            FrameHeader::Zstandard(header) => {
                prop_assert_eq!(header.content_size, Some(input.len() as u64));
            }
            FrameHeader::Skippable(_) => prop_assert!(false, "expected a Zstandard frame"),
        }

        if skew > 0 {
            let mut encoder = StreamingEncoder::new(EncoderOptions {
                pledged_src_size: Some((input.len() + skew) as u64),
                ..options
            })
            .unwrap();
            encoder.push(&input).unwrap();
            prop_assert!(encoder.finish().is_err());
        }
    }

    /// Streaming encode and streaming decode produce the input regardless of
    /// where the chunk boundaries fall on either side. Adversarial chunking
    /// must not change semantics.
    #[test]
    fn streaming_roundtrip_arbitrary_chunking(
        input in arb_input(),
        encode_chunks in arb_chunks(),
        decode_chunks in arb_chunks(),
    ) {
        let mut encoder = StreamingEncoder::new(EncoderOptions::default()).unwrap();
        let mut compressed = encoder.take_output();

        let mut offset = 0;
        let mut chunk_idx = 0;
        while offset < input.len() {
            let chunk_size = encode_chunks[chunk_idx % encode_chunks.len()].min(input.len() - offset);
            encoder.push(&input[offset..offset + chunk_size]).unwrap();
            compressed.extend_from_slice(&encoder.take_output());
            offset += chunk_size;
            chunk_idx += 1;
        }
        encoder.finish().unwrap();
        compressed.extend_from_slice(&encoder.take_output());

        let mut decoder = StreamingDecoder::new(DecoderOptions::default());
        let mut restored = Vec::new();

        let mut offset = 0;
        let mut chunk_idx = 0;
        while offset < compressed.len() {
            let chunk_size =
                decode_chunks[chunk_idx % decode_chunks.len()].min(compressed.len() - offset);
            decoder.push(&compressed[offset..offset + chunk_size]).unwrap();
            restored.extend_from_slice(&decoder.take_output());
            offset += chunk_size;
            chunk_idx += 1;
        }
        decoder.finish().unwrap();
        restored.extend_from_slice(&decoder.take_output());

        prop_assert_eq!(restored, input);
    }

    /// Frames produced with a dictionary must round-trip when decoded with
    /// the same dictionary, for arbitrary input and dictionary bytes.
    #[test]
    fn dictionary_roundtrip(
        dictionary in vec(any::<u8>(), 0..=MAX_DICTIONARY_BYTES),
        input in arb_input(),
        level in arb_any_level(),
    ) {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };
        let compressed = encode_all_with_dict_and_options(&input, &dictionary, options).unwrap();
        let restored = decode_all_with_dict(&compressed, &dictionary).unwrap();
        prop_assert_eq!(restored, input);
    }

    /// The decoder must not panic on arbitrary byte sequences. Either it
    /// produces an `Error` or it returns successfully — but never unwinds,
    /// indexes out of bounds, or hangs.
    #[test]
    fn decoder_does_not_panic_on_random_input(blob in vec(any::<u8>(), 0..=MAX_INPUT_BYTES)) {
        let _ = decode_all(&blob);
    }
}

// Properties over inputs large enough to cross the thresholds where the
// encoder changes behavior. These are far more expensive than the small
// properties above, so they run a smaller case count by default.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_LARGE_CASES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8),
    ))]

    /// Redundant input must actually compress, at every level.
    ///
    /// The bodies these inputs are built from repeat verbatim every
    /// `repeat_period` bytes, so even a parser that finds nothing but the
    /// period should land far below the input size. This is the invariant
    /// that the block splitter violated: it left levels 5 through 15 emitting
    /// roughly 20% more than upstream on inputs above ~1 MB, while every
    /// roundtrip test kept passing.
    #[test]
    fn large_redundant_input_compresses(shape in arb_large_input(), level in arb_level()) {
        let input = shape.build();
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };
        let compressed = encode_all_with_options(&input, options).unwrap();
        prop_assert_eq!(decode_all(&compressed).unwrap(), input.clone());

        // The incompressible filler is reproduced once per period and cannot
        // be compressed, so it sets the floor on what is achievable.
        let periods = input.len().div_ceil(shape.repeat_period + shape.noise);
        let floor = shape.noise * periods;
        let budget = floor + input.len() / 4;
        prop_assert!(
            compressed.len() <= budget,
            "level {} emitted {} bytes for {} bytes of {}-byte-periodic input (budget {})",
            level,
            compressed.len(),
            input.len(),
            shape.repeat_period,
            budget
        );
    }

    /// Streaming must not be dramatically worse than one-shot on the same
    /// input. Both see the same bytes; only the delivery differs.
    ///
    /// Streaming previously clamped its match prefix to a single block, so
    /// everything beyond 128 KiB was invisible to it.
    #[test]
    fn streaming_ratio_tracks_one_shot(shape in arb_large_input(), level in arb_level()) {
        let input = shape.build_streaming();
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };

        let one_shot = encode_all_with_options(&input, options).unwrap();
        let streamed = stream_encode(&input, options, 32 * 1024);
        prop_assert_eq!(decode_all(&streamed).unwrap(), input.clone());

        // Measured over the whole domain `arb_large_input` generates — every
        // template count, every period, six noise values, all 22 levels, 8448
        // cases — the largest excess is 38.69%. The bound is 50%.
        //
        // It is not tighter because of what that 38.69% is, which is not a
        // streaming defect. It appears only at 16 templates and a 1 MiB period,
        // only at levels 19-22, and it is a fixed 8812 bytes regardless of
        // noise, input length, or push size — the same excess when the entire
        // input is handed over in a single push. Upstream v1.5.7 on those exact
        // bytes emits 31549 at level 19; our streaming emits 31589, within
        // 0.13% of it. Our *one-shot* emits 22777, beating upstream by 28%,
        // because knowing the source size clamps the window log from 23 to 20
        // and our btultra2 parse does markedly better there. Upstream's own
        // streaming and one-shot agree at 31549, so it has no such gap to
        // measure. Ours is one-shot being unusually good, not streaming being
        // bad, and it is verified: upstream decodes the 22777-byte frame back
        // to all 1048576 original bytes.
        //
        // So this comparison has a floor on its sensitivity that no choice of
        // constant removes: one-shot is the reference, and one-shot can
        // legitimately be 39% better. What it still catches is the defect class
        // it was written for — streaming rebuilding its match finder every
        // block emitted roughly 4x one-shot. The tight gate on streaming output
        // is in `tests/upstream_interop.rs`, where upstream is the reference
        // instead and the tolerance is 0.2%.
        const ALLOWANCE_PERCENT: usize = 150;

        prop_assert!(
            streamed.len() <= one_shot.len() * ALLOWANCE_PERCENT / 100,
            "level {}: streaming emitted {} bytes vs {} one-shot on {} bytes ({:+.2}%, allowance {}%)",
            level,
            streamed.len(),
            one_shot.len(),
            input.len(),
            (streamed.len() as f64 - one_shot.len() as f64) / one_shot.len() as f64 * 100.0,
            ALLOWANCE_PERCENT
        );
    }

    /// Every frame the encoder produces must decode under a window limit set
    /// to exactly the window the frame itself declares. A frame that needs
    /// more history than it advertises is malformed even when a permissive
    /// decoder happens to accept it.
    #[test]
    fn emitted_offsets_fit_the_declared_window(
        shape in arb_large_input(),
        level in arb_level(),
    ) {
        let input = shape.build_streaming();
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };

        for frame in [
            encode_all_with_options(&input, options).unwrap(),
            stream_encode(&input, options, 32 * 1024),
        ] {
            let FrameHeader::Zstandard(header) = parse_frame_header(&frame).unwrap() else {
                unreachable!("encoder emits zstandard frames");
            };
            let strict = DecoderOptions {
                max_window_size: Some(header.window_size),
                ..Default::default()
            };
            let restored = decode_all_with_options(&frame, strict).unwrap_or_else(|err| {
                panic!(
                    "level {level} frame declares a {}-byte window but needs more: {err:?}",
                    header.window_size
                )
            });
            prop_assert_eq!(restored, input.clone());
        }
    }
}

/// Records whose fields all cycle on a period that fits inside the window,
/// except two that never repeat.
///
/// The near-miss is the point, and it is easy to lose. A body that repeats
/// verbatim gets locked onto by the first repeat offset that lands and never
/// lets go, which hides everything this test is about; an earlier draft of this
/// generator without the always-growing `user` field still matched far enough
/// through each record to stay in that groove, and passed against the defect.
/// With it, every match breaks a few times per record, the parser goes back to
/// the hash table, and which candidate it picks decides the output.
fn near_periodic_records(len: usize, period_records: u64) -> Vec<u8> {
    let services = ["api", "billing", "search", "worker"];
    let regions = ["us-east-1", "us-west-2", "eu-west-1"];
    let statuses = ["ok", "degraded", "failed"];
    let mut out = Vec::with_capacity(len);
    let mut index = 0u64;
    while out.len() < len {
        let cycle = index % period_records;
        let record = format!(
            "{{\"ts\":\"2026-07-27T12:{:02}:{:02}Z\",\"service\":\"{}\",\"region\":\"{}\",\
             \"status\":\"{}\",\"latency_ms\":{},\"req_id\":\"req-{:08x}\",\"user\":{}}}\n",
            cycle % 60,
            (cycle * 7) % 60,
            services[(cycle as usize) % services.len()],
            regions[(cycle as usize / 2) % regions.len()],
            statuses[(cycle as usize / 5) % statuses.len()],
            20 + (cycle * 13) % 700,
            index.wrapping_mul(2_654_435_761) as u32,
            10_000 + index * 3,
        );
        let take = record.len().min(len - out.len());
        out.extend_from_slice(&record.as_bytes()[..take]);
        index += 1;
    }
    out
}

/// A body whose period sits inside the level's window, run out to many times
/// that window.
///
/// Level 1's window is 512 KiB and the period here is roughly 300 KiB, so every
/// repeat is reachable and the encoder should ride them for the whole frame. It
/// did not: the parsers took their floor from the *start* of the block rather
/// than its end, so they could reach a block further back than the window and
/// preferred those over-wide matches. That cost offset bits and displaced the
/// repeat offset the periodic body would otherwise have kept hitting, and the
/// second harmonic of the period — just outside the window — is what kept
/// offering them. Whole-frame output was 1.26x upstream's on the same bytes.
///
/// The bound compares the frame against its own first window rather than a
/// pinned byte count. With the period inside the window, every later window is
/// no harder to code than the first, so the frame must get *cheaper* per byte
/// as it goes on. The repaired encoder reaches 0.79x its opening rate; the
/// broken one flatlines at 1.00x, never improving on its first window at all.
#[test]
fn a_period_inside_the_window_stays_findable_for_the_whole_frame() {
    const WINDOW: usize = 512 * 1024;
    const FRAME: usize = 8 * WINDOW;

    let input = near_periodic_records(FRAME, 2100);
    let options = EncoderOptions {
        compression_level: CompressionLevel::try_new(1).unwrap(),
        ..Default::default()
    };

    let opening = encode_all_with_options(&input[..WINDOW], options).unwrap();
    let whole = encode_all_with_options(&input, options).unwrap();
    assert_eq!(decode_all(&whole).unwrap(), input);

    let opening_rate = opening.len() as f64 / WINDOW as f64;
    let whole_rate = whole.len() as f64 / input.len() as f64;
    assert!(
        whole_rate < opening_rate * 0.9,
        "level 1 spent {whole_rate:.4} bytes per byte over {} bytes against {opening_rate:.4} \
         for the first {WINDOW}, a factor of {:.3}; the body's period sits inside the window, \
         so the frame should only get cheaper to code as it goes on",
        input.len(),
        whole_rate / opening_rate,
    );
}

/// The window a frame declares is the window the level uses, and nothing more.
///
/// Every parser bounds its matches by the block's end, so a block cannot reach
/// further back than the window. This used to declare `window + block_size`,
/// which made every decoder reserve 128 KiB it never needed and put these
/// frames above upstream's memory budget one level sooner than upstream's own.
#[test]
fn declared_window_matches_the_level_window() {
    // Level 1 uses windowLog 19 and level 2 windowLog 20 for inputs this size.
    for (level, window) in [(1, 512 * 1024u64), (2, 1024 * 1024)] {
        let input = LargeInput {
            templates: 4,
            repeat_period: 128 * 1024,
            noise: 2048,
        }
        .build_at_least(4 * window as usize);
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };

        for (label, frame) in [
            (
                "one-shot",
                encode_all_with_options(&input, options).unwrap(),
            ),
            ("streaming", stream_encode(&input, options, 32 * 1024)),
        ] {
            let FrameHeader::Zstandard(header) = parse_frame_header(&frame).unwrap() else {
                unreachable!("encoder emits zstandard frames");
            };
            assert_eq!(
                header.window_size, window,
                "level {level} {label} declared {} where the level's window is {window}",
                header.window_size,
            );
        }
    }
}

/// The shape that caught the streaming encoder picking its block boundaries by
/// size alone.
///
/// Its period is a little over one block, so a fixed cut lands in a different
/// place each time round and never on the seam between the repeating body and
/// the incompressible filler. The one-shot encoder ends blocks where the
/// content's statistics change, which puts the filler in a block of its own and
/// leaves the body matchable in one piece. Streaming did not, and at level 1 it
/// paid 92747 bytes against 33741 for the same input.
///
/// Pinned as a plain test rather than left to the generator: proptest found it
/// once, but a seed file only re-runs while the seed keeps shrinking to the
/// same case.
#[test]
fn streaming_matches_one_shot_on_a_period_that_straddles_the_block_size() {
    let shape = LargeInput {
        templates: 1,
        repeat_period: 128 * 1024,
        noise: 16589,
    };
    let input = shape.build_streaming();

    // Levels 1-4 are the fast and double-fast strategies, where the block
    // boundary decides whether the long-range match is found at all.
    for level in [1, 2, 3, 4, 5, 9, 13, 17] {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };
        let one_shot = encode_all_with_options(&input, options).unwrap();
        let streamed = stream_encode(&input, options, 32 * 1024);

        assert_eq!(decode_all(&streamed).unwrap(), input);
        // Not 10%. This shape is pinned and every number here is
        // deterministic, and on it streaming comes out 3 bytes *under*
        // one-shot at all eight levels — a constant that does not move with
        // the level or with output sizes that range from 19559 to 33966, so it
        // is frame header and not parse. A 10% tolerance on a fixed input with
        // a fixed answer was leaving 3374 bytes of room at level 1 to catch a
        // defect that was worth 59006.
        assert!(
            streamed.len() <= one_shot.len() + 128,
            "level {level}: streaming emitted {} bytes against {} one-shot ({:+})",
            streamed.len(),
            one_shot.len(),
            streamed.len() as i64 - one_shot.len() as i64,
        );
    }
}

/// A stream that pledges exactly one block of content produces the frame the
/// one-shot encoder produces for the same bytes and options, however the
/// content is pushed, and that frame's only block carries the last-block flag.
///
/// Before `push_target` a full buffer was encoded as soon as it filled, and a
/// pledge equal to the block size made the only block fill on the last pledged
/// byte: it went out unflagged and `finish` closed the frame with an empty last
/// block, three bytes the one-shot encoder never spends on the same input.
/// Upstream sidesteps the same trap by setting its buffer target one byte past
/// the block size when the pledge equals it (`ZSTD_CCtx_init_compressStream2`,
/// `zstd_compress.c`), so its only block waits for `ZSTD_e_end` and carries
/// the flag itself.
///
/// 128 KiB is the largest single block, and at that size the window is clamped
/// to the content, so block, window and pledge all coincide there. A pledge
/// that is exceeded still fills the held-back buffer and is still refused.
#[test]
fn a_pledged_single_block_stream_matches_one_shot() {
    for size in [1usize, 539, 4096, 36_827, 128 * 1024] {
        let input: Vec<u8> = (0..size)
            .map(|index| b"the quick brown fox "[index % 20].wrapping_add((index / 4096) as u8))
            .collect();
        for level in [1, 3, 9, 19] {
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                pledged_src_size: Some(size as u64),
                ..Default::default()
            };
            let one_shot = encode_all_with_options(&input, options).unwrap();
            let FrameHeader::Zstandard(header) = parse_frame_header(&one_shot).unwrap() else {
                panic!("expected a Zstandard frame");
            };
            assert!(
                parse_block_header(&one_shot[header.header_size..])
                    .unwrap()
                    .last_block,
                "{size} bytes at level {level}: one-shot did not flag its only block"
            );
            // Whole, in pieces that never fill the buffer, and byte by byte for
            // the sizes where that stays quick.
            for piece in [size, 1000, 1] {
                if piece == 1 && size > 4096 {
                    continue;
                }
                let streamed = stream_encode(&input, options, piece);
                assert_eq!(
                    streamed, one_shot,
                    "{size} bytes at level {level}, pushed {piece} at a time: streamed frame differs from one-shot"
                );
            }
        }
    }

    let mut encoder = StreamingEncoder::new(EncoderOptions {
        pledged_src_size: Some(4096),
        ..Default::default()
    })
    .unwrap();
    encoder.push(&[0x5a; 4097]).unwrap();
    assert!(encoder.finish().is_err());
}

/// A real trained dictionary, trained once and shared by every case.
///
/// Training per case would dominate the runtime, and the bytes do not need to
/// vary: what varies is the damage done to them below.
fn valid_dictionary() -> &'static [u8] {
    static DICTIONARY: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    DICTIONARY.get_or_init(|| {
        let records: Vec<Vec<u8>> = (0..256)
            .map(|i| {
                format!(
                    "{{\"id\":{i},\"region\":\"us-east-{}\",\"status\":\"open\",\"path\":\"/v2/objects/{i}\"}}",
                    i % 4
                )
                .into_bytes()
            })
            .collect();
        let samples: Vec<&[u8]> = records.iter().map(Vec::as_slice).collect();
        zstandard::train_dictionary(&samples, 4096).expect("training a dictionary must succeed")
    })
}

/// A real dictionary with bytes flipped in its entropy-table region, and
/// sometimes truncated.
///
/// **Damaging a valid dictionary rather than generating one is the whole point,
/// and the numbers say why.** Random bytes behind the dictionary magic parse
/// successfully about 1% of the time, so at proptest's default 64 cases such a
/// generator would produce well under one accepted dictionary per run and
/// prove nothing about two parsers agreeing. Mutating a real one lands at
/// roughly 50% acceptance, which is the regime where the two directions are
/// both doing real work and can disagree.
///
/// Mutations are aimed at the first 400 bytes because that is where the
/// entropy tables live; the header is 8 bytes and everything past the tables
/// is content, which neither direction interprets.
fn damaged_dictionary_bytes() -> impl Strategy<Value = Vec<u8>> {
    let valid = valid_dictionary();
    let table_region = valid.len().min(400);
    (
        vec((8..table_region, any::<u8>()), 1..5),
        proptest::option::of(8..valid.len()),
    )
        .prop_map(move |(edits, truncate_at)| {
            let mut bytes = valid.to_vec();
            for (at, mask) in edits {
                bytes[at] ^= mask;
            }
            if let Some(cut) = truncate_at {
                bytes.truncate(cut);
            }
            bytes
        })
}

proptest! {
    /// The two dictionary directions accept and reject the same bytes.
    ///
    /// They build different tables from one description, and the builders do
    /// not have matching error paths: `build_ctable` rejects a normalized count
    /// below `-1` and `build_dtable` has no equivalent check. So "the encoding
    /// direction refuses what the decoding direction accepts" is a reachable
    /// shape in the code, whatever `fse::read_ncount` happens to emit in
    /// practice, and a caller holding one of each over the same bytes would
    /// watch one succeed and the other fail.
    ///
    /// Nothing has been found that reaches it. This is what keeps looking.
    #[test]
    fn the_dictionary_directions_agree_on_what_parses(bytes in damaged_dictionary_bytes()) {
        let encoding = EncoderDictionary::new(&bytes).is_ok();
        let decoding = DecoderDictionary::new(&bytes).is_ok();

        prop_assert_eq!(
            encoding,
            decoding,
            "encoding accepted={}, decoding accepted={}, for {} bytes",
            encoding,
            decoding,
            bytes.len(),
        );
    }
}

/// The generator above must produce dictionaries that actually parse.
///
/// `the_dictionary_directions_agree_on_what_parses` compares two booleans, so
/// it passes trivially on any input both directions reject -- and a generator
/// that only produced rubbish would look identical to one doing real work.
/// Random bytes behind the dictionary magic parse about 1% of the time, which
/// at proptest's default case count is under one accepted dictionary per run.
/// This measures the generator that replaced it.
///
/// The bound is deliberately loose. What it is defending against is a
/// generator that drifts to near-zero acceptance after some later edit, not a
/// particular rate.
#[test]
fn the_damaged_dictionary_generator_still_produces_parseable_dictionaries() {
    let valid = valid_dictionary();
    let table_region = valid.len().min(400);

    // A fixed xorshift rather than proptest's runner, so the figure this
    // asserts on is the same on every machine and every run.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let total = 4_000;
    let mut accepted = 0;
    for _ in 0..total {
        let mut bytes = valid.to_vec();
        for _ in 0..1 + (next() % 4) as usize {
            let at = 8 + (next() % (table_region - 8) as u64) as usize;
            bytes[at] ^= (next() & 0xff) as u8;
        }
        if next() % 2 == 0 {
            let cut = 8 + (next() % (valid.len() - 8) as u64) as usize;
            bytes.truncate(cut);
        }
        if EncoderDictionary::new(&bytes).is_ok() {
            accepted += 1;
        }
    }

    let rate = accepted as f64 / total as f64;
    assert!(
        rate > 0.05,
        "the damaged-dictionary generator accepted {accepted} of {total} ({:.1}%); \
         below this the agreement property is comparing two rejections and proving nothing",
        rate * 100.0,
    );
}
