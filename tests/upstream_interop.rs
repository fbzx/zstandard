use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
};

#[allow(dead_code)]
#[path = "../src/support/corpora.rs"]
mod benchmark_corpora;
#[allow(dead_code)]
#[path = "../src/support/upstream_zstd.rs"]
mod upstream_trace_helper;

use zstandard::{
    BLOCK_SIZE_MAX, BlockHeader, BlockTraceDictionaryMode, BlockTraceDictionaryTableSource,
    BlockTraceEmittedMatchKind, BlockTraceMatchSource, BlockTraceParserStrategy,
    BlockTraceUpstreamStrategy, BlockType, CompressionLevel, DecoderDictionary, DecoderOptions,
    Encoder, EncoderDictionary, EncoderOptions, Error, Format, FrameHeader, LiteralCompressionMode,
    ParameterOverrides, RowMatchFinderMode, Strategy, StreamingDecoder, StreamingEncoder,
    decode_all, decode_all_with_dict, decode_all_with_prepared_dict, encode_all_with_dict,
    encode_all_with_dict_and_options, encode_all_with_options, encode_all_with_prepared_dict,
    encode_all_with_prepared_dict_and_options, parse_block_header, parse_frame_header,
    trace_first_block_with_prepared_dict_and_options,
};

static HELPER: OnceLock<Option<PathBuf>> = OnceLock::new();

#[test]
fn upstream_decompresses_rust_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let cases = [
        Vec::new(),
        b"zstd-rs".to_vec(),
        vec![0x42; 16_384],
        build_pattern(220_000),
    ];

    for input in cases {
        let encoded = encode_all_with_options(
            &input,
            EncoderOptions {
                block_size: 64 * 1024,
                checksum: true,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = run_helper(helper, "decompress", &encoded);
        assert_eq!(decoded, input);
    }
}

#[test]
fn rust_and_upstream_decode_golden_decompression_fixtures() {
    let Some(helper) = helper_path() else {
        return;
    };

    for fixture in [
        "zeroSeq_2B.zst",
        "block-128k.zst",
        "empty-block.zst",
        "rle-first-block.zst",
    ] {
        let Some(frame) = golden_decompression_fixture(fixture) else {
            return;
        };
        let rust_decoded = decode_all(&frame).unwrap();
        let upstream_decoded = run_helper(helper, "decompress-stream", &frame);
        assert_eq!(rust_decoded, upstream_decoded, "fixture {fixture}");
    }
}

/// Everything upstream ships in `tests/golden-decompression-errors/` is there
/// because the reference decoder has to refuse it. Walking the directory rather
/// than naming the files means bumping the upstream pin picks new cases up
/// instead of silently skipping them, and running both decoders keeps the check
/// honest: what has to agree is the verdict, not the message.
///
/// `truncated_huff_state.zst` sat in that directory accepted for the life of the
/// project, decoding to seven bytes of invented output, because nothing under
/// `tests/` had ever read the corpus. The frames are also pinned inline in
/// `tests/codec.rs`, which is the copy that runs without an upstream checkout.
#[test]
fn rust_and_upstream_both_reject_the_golden_decompression_error_fixtures() {
    let Some(helper) = helper_path() else {
        return;
    };

    // Reaching here means `build_helper` already resolved and pin-checked the
    // upstream tree, so a missing or empty corpus is a broken checkout rather
    // than a reason to skip. Skipping quietly is how this corpus went unread in
    // the first place.
    let dir = upstream_trace_helper::upstream_dir().join("tests/golden-decompression-errors");
    let entries =
        fs::read_dir(&dir).unwrap_or_else(|err| panic!("cannot read {}: {err}", dir.display()));

    let mut fixtures: Vec<PathBuf> = entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "zst"))
        .collect();
    fixtures.sort();

    assert!(
        !fixtures.is_empty(),
        "no .zst fixtures under {}; the corpus moved and this test proves nothing",
        dir.display()
    );

    // Control. Every assertion below is that `helper_accepts` returned false, so
    // a harness fault that made it *always* return false would leave this test
    // green while checking nothing. A frame upstream does decode proves the
    // observation works before the rejections are read as evidence.
    let good = golden_decompression_fixture("rle-first-block.zst")
        .expect("golden-decompression corpus is part of the same checkout");
    assert!(
        helper_accepts(helper, "decompress-stream", &good),
        "the harness cannot observe an accepted frame, so it cannot observe a rejected one"
    );

    for path in &fixtures {
        let name = path.file_name().unwrap().to_string_lossy();
        let frame = fs::read(path).unwrap();

        assert!(
            !helper_accepts(helper, "decompress-stream", &frame),
            "{name} is in golden-decompression-errors, but upstream decoded it"
        );
        assert!(
            decode_all(&frame).is_err(),
            "upstream rejects {name}, but decode_all accepted it"
        );
    }
}

#[test]
fn upstream_decompresses_rust_compressed_literals_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let cases = [
        build_huff_friendly_pattern(900),
        build_huff_friendly_pattern(12_000),
        build_huff_friendly_pattern(180_000),
    ];

    for input in cases {
        let encoded = encode_all_with_options(
            &input,
            EncoderOptions {
                block_size: 64 * 1024,
                checksum: true,
                ..Default::default()
            },
        )
        .unwrap();
        let literal_types = literals_block_types(&encoded);

        assert!(
            literal_types.contains(&2),
            "Rust output did not contain a compressed literals block: {literal_types:?}"
        );

        let decoded = run_helper(helper, "decompress", &encoded);
        assert_eq!(decoded, input);
    }
}

#[test]
fn upstream_decompresses_rust_sequence_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let cases = [
        build_repeated_chunk_pattern(24_000),
        build_repeated_chunk_pattern(180_000),
    ];

    for input in cases {
        let encoded = encode_all_with_options(
            &input,
            EncoderOptions {
                block_size: 64 * 1024,
                checksum: true,
                ..Default::default()
            },
        )
        .unwrap();

        let sequence_counts = compressed_block_sequence_counts(&encoded);
        assert!(
            sequence_counts.iter().any(|&count| count > 0),
            "Rust output did not contain a compressed block with real sequences: {sequence_counts:?}"
        );

        let decoded = run_helper(helper, "decompress", &encoded);
        assert_eq!(decoded, input);
    }
}

#[test]
fn upstream_decompresses_rust_output_across_encoder_levels() {
    let Some(helper) = helper_path() else {
        return;
    };

    let input = build_short_match_pattern(48_000);
    for level in [
        CompressionLevel::FASTEST,
        CompressionLevel::DEFAULT,
        CompressionLevel::BETTER,
        CompressionLevel::BEST,
    ] {
        let encoded = encode_all_with_options(
            &input,
            EncoderOptions {
                block_size: 64 * 1024,
                checksum: true,
                write_dict_id: true,
                compression_level: level,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = run_helper(helper, "decompress", &encoded);
        assert_eq!(decoded, input, "failed at encoder level {}", level.as_i32());
    }
}

/// Upstream decodes the frames where this crate deliberately diverges from it.
///
/// Levels 13 to 15 are btlazy2, the only levels with no row match finder, and
/// their regular matches go through a store that encodes an offset as a
/// repcode whenever it happens to equal one. C's lazy family cannot do that --
/// `REPCODE2_TO_OFFBASE` and `REPCODE3_TO_OFFBASE` appear nowhere in
/// `zstd_lazy.c` -- so these frames are legal zstd that upstream would never
/// have produced, and they compress 3.5% to 4.5% better than its own output
/// for it. That divergence is deliberate; see "The repcode substitution" in
/// `docs/PARITY_PLAN.md`, and branch `btlazy2-no-repcodes` for the
/// parity-restoring change that was measured and declined.
///
/// Which makes *this* the guarantee the choice rests on: whatever we emit,
/// upstream must still read it. The two tests above cover 48 KB of synthetic
/// pattern at four levels, and neither reaches this path -- when the decision
/// was taken, no test had ever fed one of these frames to upstream's decoder.
/// Every benchmark corpus runs here, because the repcode substitution depends
/// on how often a match distance coincides with a live repeat offset, which is
/// a property of the data rather than of the parser.
#[test]
fn upstream_decompresses_frames_that_diverge_in_our_favour() {
    let Some(helper) = helper_path() else {
        return;
    };
    const SIZE: usize = 1 << 20;
    for corpus in benchmark_corpora::benchmark_report_cases(SIZE) {
        if corpus.dict_kind != benchmark_corpora::DictKind::None {
            continue;
        }
        for level in 13..=15i32 {
            let encoded = encode_all_with_options(
                &corpus.input,
                EncoderOptions {
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    ..Default::default()
                },
            )
            .unwrap();
            let decoded = run_helper(helper, "decompress", &encoded);
            assert_eq!(
                decoded, corpus.input,
                "{} L{level}: upstream could not decode this crate's frame",
                corpus.name
            );
        }
    }
}

/// One level per distinct parser strategy, plus the negative levels' parameter
/// regime, chosen so the sweep below samples *store paths* rather than level
/// numbers.
///
/// Taken from `supported_levels_follow_upstream_backend_selection`, which is
/// the test that pins the mapping; if it changes, this list is wrong and the
/// sweep quietly stops covering a parser.
///
/// **The mapping is a function of input size, not of the level alone**, which
/// is why the sweep runs two sizes rather than trusting one list. `clevels.h`
/// selects a different row per size class, and the shift is a whole strategy
/// wide: measured here, level 2 is `Fast` at 1 MiB and `DoubleFast` at 256 KiB,
/// and level 4 is `DoubleFast` at 1 MiB and `GreedyRow` at 256 KiB. Below a
/// window log of 14 the row variants disappear entirely in favour of the chain
/// planners, which store differently again. A list of levels can only ever
/// sample the strategies *at the sizes it is run against*.
///
/// **The negative levels are a fourth axis inside `Fast`, not a fourth
/// strategy.** They all share cparams row `0` and differ only in an
/// acceleration factor equal to the level's own magnitude, which widens the
/// fast parser's step between match attempts. Sampling them matters because the
/// step is what decides how far the parser advances without inserting into its
/// table, and a store that emits a repcode is reading repeat-offset state that
/// a wider step leaves further behind. `-131072` is the floor
/// (`ZSTD_minCLevel()`), where the step is large enough that a block can be
/// reached with no sequences at all, which is a different emission path again.
const DECODABILITY_LEVELS: [i32; 9] = [
    1,  // Fast
    3,  // DoubleFast
    5,  // GreedyRow, or Greedy on a small input
    6,  // LazyRow, or Lazy
    8,  // Lazy2Row, or Lazy2
    13, // BinaryTreeLazy2
    16, // BinaryTreeOpt
    18, // BinaryTreeUltra
    22, // BinaryTreeUltra, with btultra2's extra pass
];

/// **The guarantee that replaces byte parity.** Upstream decodes everything
/// this crate can emit, across every path it can emit it from.
///
/// This crate no longer aims at producing upstream's bytes. It aims at
/// producing *better* bytes where it can, and upstream's C will keep moving
/// besides, so a comparison against its output is a comparison against a
/// moving target that we have deliberately stepped off. What cannot move is
/// the format: a frame we emit has to be readable by any conforming decoder,
/// forever, and upstream's is the reference one.
///
/// So this is the load-bearing test, and it is deliberately broad where the
/// parity sweeps are deliberately narrow. It was not broad before. When the
/// btlazy2 divergence was accepted, the only corpus-scale evidence was
/// `upstream_decompresses_frames_that_diverge_in_our_favour` above -- three
/// levels, one-shot, no dictionary -- and the older round-trip tests cover 48
/// KB of synthetic pattern. Levels 1 to 12 and 16 to 22 had never had a
/// benchmark-corpus frame put through upstream's decoder at all, nor had any
/// streaming frame, nor any dictionary frame over real data.
///
/// The three axes are here because each one reaches a different store, and a
/// repcode written against the wrong repeat-offset state is exactly the kind
/// of defect that round-trips through our *own* decoder and fails on a
/// conforming one:
///
/// - **strategy**, via [`DECODABILITY_LEVELS`];
/// - **window size**, via two input sizes, because the row match finder is
///   gated on `windowLog > 14` and the chain planners below it store
///   differently;
/// - **framing**, via one-shot and streaming, which lay their blocks out
///   differently and carry repeat offsets across block boundaries differently;
/// - **the long-distance matcher**, which is off by default and so reached by
///   none of the above. Its store runs *after* whichever strategy the level
///   selected and inherits that parser's repeat offsets, which makes it the one
///   place in the crate where a store can be handed state it did not build.
///   Coding a repcode against the third of those offsets is a real defect that
///   fails only on a conforming decoder, because Fast and DoubleFast leave that
///   slot stale; this axis is what catches it.
///
/// Dictionary frames get the same treatment in
/// [`upstream_decodes_dictionary_frames_from_every_strategy`].
///
/// When a divergence widens, widen this. A deliberate divergence is only ever
/// as safe as the round trip that guards it.
#[test]
fn upstream_decodes_frames_from_every_strategy_and_framing() {
    let Some(helper) = helper_path() else {
        return;
    };
    // Above and below the window log of 14 that gates the row match finder.
    const SIZES: [usize; 2] = [1 << 20, 48 << 10];
    const PIECE: usize = 64 * 1024;
    let mut checked = 0usize;

    for size in SIZES {
        for corpus in benchmark_corpora::benchmark_report_cases(size) {
            if corpus.dict_kind != benchmark_corpora::DictKind::None {
                continue;
            }
            for level in DECODABILITY_LEVELS {
                let options = EncoderOptions {
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    ..Default::default()
                };
                let long_distance = EncoderOptions {
                    parameters: ParameterOverrides {
                        long_distance_matching: zstandard::LdmMode::Enabled,
                        ..Default::default()
                    },
                    ..options
                };
                let frames = [
                    (
                        "one-shot",
                        "decompress",
                        encode_all_with_options(&corpus.input, options).unwrap(),
                    ),
                    (
                        "one-shot with long-distance matching",
                        "decompress",
                        encode_all_with_options(&corpus.input, long_distance).unwrap(),
                    ),
                    (
                        "streaming",
                        "decompress-stream",
                        stream_encode(&corpus.input, options, PIECE),
                    ),
                ];
                for (framing, mode, frame) in frames {
                    assert_eq!(
                        run_helper(helper, mode, &frame),
                        corpus.input,
                        "{} L{level} at {size} bytes: upstream could not decode our {framing} frame",
                        corpus.name,
                    );
                    checked += 1;
                }
            }
        }
    }

    // Asserted rather than assumed: a corpus list that silently shrank, or a
    // `continue` that swallowed a whole axis, would otherwise leave this test
    // passing on a fraction of what it claims to cover.
    assert_eq!(
        checked,
        SIZES.len() * 9 * DECODABILITY_LEVELS.len() * 3,
        "the decodability sweep did not cover its grid",
    );
}

/// The same guarantee for dictionary frames, which reach stores that no
/// no-dictionary sweep touches.
///
/// Split from the sweep above rather than folded into it because the dictionary
/// planners are a genuinely separate family -- the prepared-table path, the
/// external-dictionary path and the prefixed path each have their own store --
/// and because these two corpora are the only ones the benchmark set builds a
/// dictionary for, so folding them in would have made the grid assertion above
/// lie about its shape.
#[test]
fn upstream_decodes_dictionary_frames_from_every_strategy() {
    let Some(helper) = helper_path() else {
        return;
    };
    const SIZE: usize = 1 << 20;
    let raw_bytes = run_helper(helper, "emit-raw-dict", &[]);
    let trained_bytes = run_helper(helper, "emit-trained-dict", &[]);
    let mut checked = 0usize;

    for corpus in benchmark_corpora::benchmark_report_cases(SIZE) {
        let (dictionary, mode) = match corpus.dict_kind {
            benchmark_corpora::DictKind::None => continue,
            benchmark_corpora::DictKind::Raw => (&raw_bytes, "decompress-stream-raw-dict"),
            benchmark_corpora::DictKind::Trained => {
                (&trained_bytes, "decompress-stream-trained-dict")
            }
        };
        let prepared = EncoderDictionary::new(dictionary).unwrap();
        for level in DECODABILITY_LEVELS {
            let encoded = encode_all_with_prepared_dict_and_options(
                &corpus.input,
                &prepared,
                EncoderOptions {
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                run_helper(helper, mode, &encoded),
                corpus.input,
                "{} L{level}: upstream could not decode our dictionary frame",
                corpus.name,
            );
            checked += 1;
        }
    }

    assert_eq!(
        checked,
        2 * DECODABILITY_LEVELS.len(),
        "the dictionary decodability sweep did not cover its grid",
    );
}

/// Input lengths clustered where the parsers' bounds live, so the sweep below
/// covers the sizes at which a store path is entered *partially* or not at all.
///
/// `MIN_MATCH` is 3 and the parsers guard their search on it, so 0 through 8
/// walks across "no match is representable", "exactly one is", and "the first
/// hash can be computed". 64 is past every such guard.
///
/// **What these lengths actually reach was measured, not assumed.** Below 7
/// bytes no planner is called at all; at 7 and 8 the fast planner is entered
/// and returns down its short-input branch; at 64 it runs its main loop. In
/// every one of those cases the plan is then discarded in favour of a raw
/// block, because nothing it found pays for a compressed one. So what this
/// sweep exercises is the raw and RLE block decision and the frame header's
/// small-content-size forms, *not* the parsers -- an injected defect in the
/// fast planner's short-input branch is invisible here and shows up two tests
/// up, on the corpus sweep's block tails.
const DEGENERATE_LENGTHS: [usize; 10] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 64];

/// Legality at the sizes where the parsers barely run, or do not run at all.
///
/// The corpus sweeps above all feed at least 48 KB, so every frame they produce
/// is a compressed block reached through a parser's main loop. The degenerate
/// band is a different artifact: an empty frame, a single-byte frame, a raw
/// block, a one-byte content-size field.
///
/// That band is not untested -- `tests/codec.rs` has a dozen cases across it --
/// but every one of those is a round trip through *our own* decoder, which
/// cannot catch an encoder and a decoder that agree on the same wrong reading.
/// This is the differential half, and it is the half that was missing. A frame
/// carrying no sequences is also the one case where the literals section is the
/// whole block, so its header is the only thing the two decoders have to agree
/// about.
#[test]
fn upstream_decodes_degenerate_inputs_from_every_strategy() {
    let Some(helper) = helper_path() else {
        return;
    };
    let raw_dict_bytes = run_helper(helper, "emit-raw-dict", &[]);
    let dictionary = EncoderDictionary::new(&raw_dict_bytes).unwrap();
    let mut checked = 0usize;

    for length in DEGENERATE_LENGTHS {
        // Not a constant pattern: a run of one byte is compressible by any
        // parser and would hide a length guard behind a match that any of them
        // finds. This repeats with period 7 so that lengths 0..8 straddle it.
        let input: Vec<u8> = (0..length).map(|i| b"degenrt"[i % 7]).collect();

        for level in DECODABILITY_LEVELS {
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                ..Default::default()
            };
            let long_distance = EncoderOptions {
                parameters: ParameterOverrides {
                    long_distance_matching: zstandard::LdmMode::Enabled,
                    ..Default::default()
                },
                ..options
            };
            let frames = [
                (
                    "one-shot",
                    "decompress",
                    encode_all_with_options(&input, options).unwrap(),
                ),
                (
                    "one-shot with long-distance matching",
                    "decompress",
                    encode_all_with_options(&input, long_distance).unwrap(),
                ),
                (
                    // A one-byte piece so that an input of length n is pushed
                    // as n separate blocks' worth of calls, which is the only
                    // way to reach a flush that has seen fewer bytes than the
                    // parser's guard.
                    "streaming",
                    "decompress-stream",
                    stream_encode(&input, options, 1),
                ),
                (
                    "with a raw dictionary",
                    "decompress-stream-raw-dict",
                    encode_all_with_prepared_dict_and_options(&input, &dictionary, options)
                        .unwrap(),
                ),
            ];
            for (framing, mode, frame) in frames {
                assert_eq!(
                    run_helper(helper, mode, &frame),
                    input,
                    "{length}-byte input at L{level}: upstream could not decode our {framing} frame",
                );
                checked += 1;
            }
        }
    }

    assert_eq!(
        checked,
        DEGENERATE_LENGTHS.len() * DECODABILITY_LEVELS.len() * 4,
        "the degenerate-input decodability sweep did not cover its grid",
    );
}

/// Legality of what this crate emits *between* frames, which no other sweep
/// looks at because every other one encodes exactly one frame and decodes it.
///
/// Two separate guarantees are folded together here because they only fail
/// together in practice. A concatenation is legal only if each frame terminates
/// exactly where it says it does: a frame that over-declares its content size,
/// or leaves a byte of padding after its last block, decodes fine on its own
/// and desynchronises everything after it. And a skippable frame is the one
/// thing this crate writes that is not a compressed frame at all.
///
/// `tests/codec.rs` covers both shapes already, and would catch a mis-sized
/// skippable header on its own -- but only through our decoder. The reverse
/// direction, upstream's concatenations through ours, is
/// `rust_decompresses_concatenated_upstream_frames`. Ours through upstream's
/// was the missing quadrant.
#[test]
fn upstream_decodes_our_concatenated_and_skippable_frames() {
    let Some(helper) = helper_path() else {
        return;
    };

    // Deliberately mixed: a level per strategy family, both framings, an empty
    // frame, and skippable frames in the three positions that can desynchronise
    // a reader -- leading, interior and trailing.
    let segments: [(Vec<u8>, i32, bool); 5] = [
        (build_pattern(70_000), 1, false),
        (Vec::new(), 9, true),
        (build_small_alphabet_pattern(120_000), -7, false),
        (build_repeated_chunk_pattern(90_000), 19, true),
        (build_pattern(3), 5, false),
    ];

    let mut encoded = zstandard::write_skippable_frame(0, b"leading").unwrap();
    let mut expected = Vec::new();
    for (index, (input, level, streamed)) in segments.into_iter().enumerate() {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };
        let frame = if streamed {
            stream_encode(&input, options, 4096)
        } else {
            encode_all_with_options(&input, options).unwrap()
        };
        encoded.extend_from_slice(&frame);
        expected.extend_from_slice(&input);
        // An interior skippable frame after every segment but the last, and a
        // zero-length payload among them: eight bytes of header and nothing
        // else is the shape most likely to be mis-sized.
        if index + 1 < 5 {
            let payload = vec![b'x'; index * 17];
            encoded.extend_from_slice(
                &zstandard::write_skippable_frame(u8::try_from(index).unwrap(), &payload).unwrap(),
            );
        }
    }
    encoded.extend_from_slice(&zstandard::write_skippable_frame(15, b"trailing").unwrap());

    assert_eq!(
        run_helper(helper, "decompress-stream", &encoded),
        expected,
        "upstream could not decode our concatenation of frames and skippable frames",
    );

    // Our own decoder has to agree, or the concatenation proves only that we
    // and upstream disagree about which of us is wrong.
    assert_eq!(decode_all(&encoded).unwrap(), expected);
}

#[test]
fn upstream_decompresses_streaming_encoder_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let input = build_repeated_chunk_pattern(128 * 1024);
    let mut encoder = StreamingEncoder::new(EncoderOptions {
        block_size: 64 * 1024,
        checksum: true,
        write_dict_id: true,
        compression_level: CompressionLevel::BETTER,
        ..Default::default()
    })
    .unwrap();

    let mut encoded = encoder.take_output();
    for chunk in input.chunks(9_001) {
        encoder.push(chunk).unwrap();
        encoded.extend_from_slice(&encoder.take_output());
    }
    encoder.finish().unwrap();
    encoded.extend_from_slice(&encoder.take_output());

    let header = match parse_frame_header(&encoded).unwrap() {
        FrameHeader::Zstandard(header) => header,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    assert!(!header.single_segment);
    assert_eq!(header.content_size, None);

    let decoded = run_helper(helper, "decompress-stream", &encoded);
    assert_eq!(decoded, input);
}

#[test]
fn upstream_decompresses_flushed_streaming_encoder_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let input = build_repeated_chunk_pattern(96 * 1024);
    let mut encoder = StreamingEncoder::new(EncoderOptions {
        block_size: 64 * 1024,
        checksum: true,
        write_dict_id: true,
        compression_level: CompressionLevel::BETTER,
        ..Default::default()
    })
    .unwrap();

    let mut encoded = encoder.take_output();
    encoder.push(&input[..23_000]).unwrap();
    encoder.flush().unwrap();
    encoded.extend_from_slice(&encoder.take_output());

    for chunk in input[23_000..].chunks(6_001) {
        encoder.push(chunk).unwrap();
        encoded.extend_from_slice(&encoder.take_output());
    }
    encoder.finish().unwrap();
    encoded.extend_from_slice(&encoder.take_output());

    let decoded = run_helper(helper, "decompress-stream", &encoded);
    assert_eq!(decoded, input);
}

#[test]
fn upstream_decompresses_reset_streaming_encoder_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let first = build_pattern(42_000);
    let second = build_small_alphabet_pattern(58_000);
    let mut encoder = StreamingEncoder::new(EncoderOptions {
        block_size: 64 * 1024,
        checksum: true,
        write_dict_id: true,
        compression_level: CompressionLevel::BETTER,
        ..Default::default()
    })
    .unwrap();

    let mut encoded = encoder.take_output();
    encoder.push(&first).unwrap();
    encoder.finish().unwrap();
    encoder.reset().unwrap();
    encoder.push(&second).unwrap();
    encoder.finish().unwrap();
    encoded.extend_from_slice(&encoder.take_output());

    let mut expected = first.clone();
    expected.extend_from_slice(&second);
    let decoded = run_helper(helper, "decompress-stream", &encoded);
    assert_eq!(decoded, expected);
}

#[test]
fn streaming_decoder_consumes_upstream_output_in_chunks() {
    let Some(helper) = helper_path() else {
        return;
    };

    let left = run_configured_compress(
        helper,
        "compress-regular-configured",
        5,
        true,
        &build_pattern(90_000),
    );
    let right = run_configured_compress(
        helper,
        "compress-regular-configured",
        9,
        false,
        &build_small_alphabet_pattern(140_000),
    );

    let mut encoded = Vec::new();
    encoded.extend_from_slice(&left);
    encoded.extend_from_slice(&right);

    let mut decoder = StreamingDecoder::new(DecoderOptions::default());
    let mut decoded = Vec::new();
    let mut scratch = [0u8; 8192];
    for chunk in encoded.chunks(113) {
        decoder.push(chunk).unwrap();
        drain_decoder(&mut decoder, &mut scratch, &mut decoded);
    }
    decoder.finish().unwrap();
    drain_decoder(&mut decoder, &mut scratch, &mut decoded);

    let mut expected = build_pattern(90_000);
    expected.extend_from_slice(&build_small_alphabet_pattern(140_000));
    assert_eq!(decoded, expected);
}

#[test]
fn rust_decompresses_upstream_no_sequence_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let cases = [
        b"small literals block".to_vec(),
        vec![0x11; 8192],
        build_pattern(180_000),
    ];

    for input in cases {
        let encoded = run_helper(helper, "compress-no-seqs", &input);
        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, input);
    }
}

#[test]
fn rust_decompresses_upstream_compressed_literals_no_sequence_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let cases = [
        build_small_alphabet_pattern(900),
        build_small_alphabet_pattern(12_000),
        build_small_alphabet_pattern(180_000),
    ];

    for input in cases {
        let encoded = run_helper(helper, "compress-literals-no-seqs", &input);
        let literal_types = literals_block_types(&encoded);

        assert!(
            literal_types.contains(&2),
            "upstream output did not contain a compressed literals block: {literal_types:?}"
        );

        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, input);
    }
}

#[test]
fn rust_decompresses_treeless_literals_after_a_previous_huffman_table() {
    let Some(helper) = helper_path() else {
        return;
    };

    let input = build_small_alphabet_pattern(4096);
    let encoded = run_helper(helper, "compress-literals-no-seqs", &input);
    let first_block = first_compressed_block_payload(&encoded);
    let literals = parse_literals_header(&first_block);
    assert_eq!(
        literals.block_type, 2,
        "expected a compressed literals block"
    );
    assert_eq!(&first_block[literals.payload_end()..], [0]);

    let literals_payload = &first_block[literals.header_size..literals.payload_end()];
    let tree_size = huffman_tree_description_size(literals_payload);
    assert!(
        tree_size < literals_payload.len(),
        "Huffman table consumed the entire literals payload"
    );
    let streams = &literals_payload[tree_size..];

    let mut treeless_block = Vec::new();
    treeless_block.extend_from_slice(&encode_compressed_literals_header(
        3,
        literals.size_format,
        literals.regenerated_size,
        streams.len(),
    ));
    treeless_block.extend_from_slice(streams);
    treeless_block.push(0);

    let mut frame = write_single_segment_header(input.len() * 2);
    append_block(&mut frame, BlockType::Compressed, &first_block, false);
    append_block(&mut frame, BlockType::Compressed, &treeless_block, true);

    let decoded = decode_all(&frame).unwrap();
    let mut expected = input.clone();
    expected.extend_from_slice(&input);
    assert_eq!(decoded, expected);
    assert_eq!(literals_block_types(&frame), vec![2, 3]);
}

#[test]
fn rust_decompresses_upstream_regular_sequence_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let cases = [
        build_pattern(180_000),
        build_small_alphabet_pattern(220_000),
        build_repeated_chunk_pattern(320_000),
    ];

    for input in cases {
        let encoded = run_helper(helper, "compress-regular", &input);
        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, input);
    }
}

#[test]
fn rust_decompresses_upstream_level_checksum_matrix() {
    let Some(helper) = helper_path() else {
        return;
    };

    let cases = [
        build_pattern(48_000),
        build_small_alphabet_pattern(220_000),
        build_repeated_chunk_pattern(320_000),
    ];
    let configs = [(1, false), (9, true), (15, false)];

    for input in cases {
        for (level, checksum) in configs {
            let encoded = run_configured_compress(
                helper,
                "compress-regular-configured",
                level,
                checksum,
                &input,
            );
            let header = match parse_frame_header(&encoded).unwrap() {
                FrameHeader::Zstandard(header) => header,
                FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
            };

            assert_eq!(header.checksum, checksum);

            let decoded = decode_all(&encoded).unwrap();
            assert_eq!(decoded, input);
        }
    }
}

#[test]
fn rust_decompresses_concatenated_upstream_frames() {
    let Some(helper) = helper_path() else {
        return;
    };

    let segments = [
        (build_pattern(70_000), 1, false),
        (build_small_alphabet_pattern(180_000), 9, true),
        (build_repeated_chunk_pattern(140_000), 15, false),
    ];

    let mut encoded = Vec::new();
    let mut expected = Vec::new();
    for (input, level, checksum) in segments {
        encoded.extend_from_slice(&run_configured_compress(
            helper,
            "compress-regular-configured",
            level,
            checksum,
            &input,
        ));
        expected.extend_from_slice(&input);
    }

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, expected);
}

/// The compressed size of every block in `frame`, in order.
fn block_sizes(frame: &[u8]) -> Vec<u32> {
    let header = match parse_frame_header(frame).unwrap() {
        FrameHeader::Zstandard(header) => header,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let mut pos = header.header_size;
    let mut sizes = Vec::new();
    loop {
        let block = parse_block_header(&frame[pos..]).unwrap();
        sizes.push(block.block_size);
        pos += 3 + block.block_size as usize;
        if block.last_block {
            break;
        }
    }
    sizes
}

fn stream_encode(input: &[u8], options: EncoderOptions, piece: usize) -> Vec<u8> {
    let mut encoder = StreamingEncoder::new(options).unwrap();
    let mut out = Vec::new();
    for chunk in input.chunks(piece) {
        encoder.push(chunk).unwrap();
        out.extend_from_slice(&encoder.take_output());
    }
    encoder.finish().unwrap();
    out.extend_from_slice(&encoder.take_output());
    out
}

/// Frames produced by upstream's *streaming* encoder decode correctly here, and
/// ours decode correctly there.
///
/// Every other upstream comparison in this file is one-shot on both sides, and
/// a streaming frame is not the same artifact: with no pledged source size it
/// declares a window instead of a content size, and upstream's buffered
/// streaming path lays its blocks out differently from its own one-shot path.
/// Until `compress-regular-streaming-configured` existed there was nothing in
/// the tree that had ever seen an upstream streaming frame.
#[test]
fn upstream_streaming_frames_round_trip_both_ways() {
    let Some(upstream_helper) = upstream_trace_helper::helper_path() else {
        return;
    };
    // Two different helper binaries with two different mode sets. Only the
    // shared one compresses through the streaming API, and only the local one
    // decompresses a frame whose size the header does not declare -- the shared
    // helper's `decompress` sizes its output from `ZSTD_getFrameContentSize`,
    // which is precisely what a streaming frame omits.
    let Some(local_helper) = helper_path() else {
        return;
    };

    // Several 128 KiB chunks, so the frame exercises upstream's chunk loop
    // rather than its single-chunk shortcut.
    let input_size = 1024 * 1024;
    let piece = 32 * 1024;
    let cases = ["json-records", "log-lines", "binary-structured"];

    for case in benchmark_corpora::benchmark_report_cases(input_size) {
        if !cases.contains(&case.name) {
            continue;
        }
        for level in [3, 9, 15] {
            for checksum in [false, true] {
                let theirs = upstream_trace_helper::compress_streaming_once(
                    upstream_helper,
                    level,
                    checksum,
                    piece,
                    &case.input,
                );
                let header = match parse_frame_header(&theirs).unwrap() {
                    FrameHeader::Zstandard(header) => header,
                    FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
                };
                // The absent pledge is what makes this a streaming frame rather
                // than a one-shot frame produced through a streaming call.
                assert_eq!(header.content_size, None, "{} level {level}", case.name);
                assert_eq!(header.checksum, checksum);
                assert_eq!(
                    decode_all(&theirs).unwrap(),
                    case.input,
                    "{} level {level} checksum {checksum}",
                    case.name
                );

                let options = EncoderOptions {
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    checksum,
                    ..Default::default()
                };
                let ours = stream_encode(&case.input, options, piece);
                assert_eq!(
                    run_helper(local_helper, "decompress-stream", &ours),
                    case.input,
                    "{} level {level} checksum {checksum}",
                    case.name
                );
            }
        }
    }
}

#[test]
fn rust_and_upstream_reject_malformed_sequence_blocks() {
    let Some(helper) = helper_path() else {
        return;
    };

    let cases = [
        malformed_reserved_sequence_mode_frame(),
        malformed_repeat_without_previous_table_frame(),
        malformed_truncated_sequence_fse_table_frame(),
        malformed_zero_sequence_trailing_payload_frame(),
        malformed_repeat_offset_underflow_frame(),
        malformed_offset_past_history_frame(),
    ];

    for frame in cases {
        assert!(decode_all(&frame).is_err());
        assert!(helper_rejects_frame(helper, &frame));
    }
}

#[test]
fn rust_and_upstream_reject_malformed_frame_headers_and_payloads() {
    let Some(helper) = helper_path() else {
        return;
    };

    let cases = [
        malformed_frame_header_reserved_bit_frame(),
        malformed_truncated_dictionary_id_frame(),
        malformed_reserved_block_type_frame(),
        malformed_block_too_large_for_frame_frame(),
        malformed_truncated_raw_block_payload_frame(),
        malformed_truncated_rle_block_payload_frame(),
        malformed_truncated_checksum_frame(),
    ];

    for frame in cases {
        assert!(decode_all(&frame).is_err());
        assert!(helper_rejects_frame(helper, &frame));
    }
}

#[test]
fn rust_decompresses_upstream_raw_content_dictionary_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-raw-dict", &[]);
    let cases = [
        build_raw_dictionary_input(8_000),
        build_raw_dictionary_input(96_000),
    ];

    for input in cases {
        let encoded = run_helper(helper, "compress-raw-dict", &input);
        let decoded = decode_all_with_dict(&encoded, &dictionary).unwrap();
        assert_eq!(decoded, input);
    }
}

#[test]
fn upstream_decompresses_rust_raw_dictionary_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-raw-dict", &[]);
    for input in [
        build_raw_dictionary_input(18_000),
        build_raw_dictionary_input(140_000),
    ] {
        let encoded = encode_all_with_dict(&input, &dictionary).unwrap();
        let decoded = run_helper(helper, "decompress-stream-raw-dict", &encoded);
        assert_eq!(decoded, input);
        assert_eq!(decode_all_with_dict(&encoded, &dictionary).unwrap(), input);
    }
}

#[test]
fn rust_decompresses_upstream_formatted_dictionary_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-trained-dict", &[]);
    let cases = [
        build_trained_dictionary_input(6_000),
        build_trained_dictionary_input(72_000),
    ];

    for input in cases {
        let encoded = run_helper(helper, "compress-trained-dict", &input);
        let header = match parse_frame_header(&encoded).unwrap() {
            FrameHeader::Zstandard(header) => header,
            FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
        };
        let dictionary_id = header.dictionary_id.unwrap();

        let decoded = decode_all_with_dict(&encoded, &dictionary).unwrap();
        assert_eq!(decoded, input);

        let err = decode_all(&encoded).unwrap_err();
        assert_eq!(err, Error::DictionaryRequired(Some(dictionary_id)));

        let err = decode_all_with_dict(&encoded, b"wrong raw dictionary").unwrap_err();
        assert_eq!(
            err,
            Error::DictionaryMismatch {
                expected: dictionary_id,
                actual: 0,
            }
        );
    }
}

#[test]
fn upstream_decompresses_rust_formatted_dictionary_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-trained-dict", &[]);
    for input in [
        build_trained_dictionary_input(18_000),
        build_trained_dictionary_input(140_000),
    ] {
        let encoded = encode_all_with_dict(&input, &dictionary).unwrap();
        let header = match parse_frame_header(&encoded).unwrap() {
            FrameHeader::Zstandard(header) => header,
            FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
        };
        assert!(header.dictionary_id.is_some());

        let decoded = run_helper(helper, "decompress-stream-trained-dict", &encoded);
        assert_eq!(decoded, input);
        assert_eq!(decode_all_with_dict(&encoded, &dictionary).unwrap(), input);
    }
}

#[test]
fn formatted_dictionary_frames_can_omit_dict_id() {
    let Some(helper) = helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-trained-dict", &[]);
    let input = build_trained_dictionary_input(64_000);
    let encoded = encode_all_with_dict_and_options(
        &input,
        &dictionary,
        EncoderOptions {
            block_size: 64 * 1024,
            checksum: true,
            write_dict_id: false,
            compression_level: CompressionLevel::DEFAULT,
            ..Default::default()
        },
    )
    .unwrap();

    let header = match parse_frame_header(&encoded).unwrap() {
        FrameHeader::Zstandard(header) => header,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    assert_eq!(header.dictionary_id, None);

    let upstream_decoded = run_helper(helper, "decompress-stream-trained-dict", &encoded);
    assert_eq!(upstream_decoded, input);
    assert_eq!(decode_all_with_dict(&encoded, &dictionary).unwrap(), input);
    assert!(
        !matches!(decode_all(&encoded), Err(Error::DictionaryRequired(_))),
        "hidden-dictionary frames should fail later than header validation",
    );
}

#[test]
fn prepared_dictionary_roundtrips_formatted_dictionary_frames() {
    let Some(helper) = helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-trained-dict", &[]);
    let prepared = EncoderDictionary::new(&dictionary).unwrap();
    let prepared_decoding = DecoderDictionary::new(&dictionary).unwrap();
    assert!(!prepared.is_raw_content());
    assert_ne!(prepared.id(), 0);
    assert_eq!(prepared.id(), prepared_decoding.id());

    for input in [
        build_trained_dictionary_input(18_000),
        build_trained_dictionary_input(72_000),
    ] {
        let encoded = encode_all_with_prepared_dict(&input, &prepared).unwrap();
        let upstream_decoded = run_helper(helper, "decompress-stream-trained-dict", &encoded);
        assert_eq!(upstream_decoded, input);
        assert_eq!(
            decode_all_with_prepared_dict(&encoded, &prepared_decoding).unwrap(),
            input
        );

        let upstream_encoded = run_helper(helper, "compress-trained-dict", &input);
        assert_eq!(
            decode_all_with_prepared_dict(&upstream_encoded, &prepared_decoding).unwrap(),
            input
        );
    }
}

#[test]
fn prepared_dictionary_can_reuse_sequence_tables_on_the_first_block() {
    let Some(helper) = helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-trained-dict", &[]);
    let prepared = EncoderDictionary::new(&dictionary).unwrap();
    let input = build_trained_dictionary_input(6_000);
    let encoded = encode_all_with_prepared_dict_and_options(
        &input,
        &prepared,
        EncoderOptions {
            block_size: 64 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::FASTEST,
            ..Default::default()
        },
    )
    .unwrap();
    let payload = first_compressed_block_payload(&encoded);

    assert!(
        compressed_block_sequence_count(&payload) > 0,
        "expected the first compressed block to contain sequences"
    );
    let (ll_mode, of_mode, ml_mode) = compressed_block_sequence_modes(&payload).unwrap();
    let modes = (ll_mode, of_mode, ml_mode);
    assert!(
        [ll_mode, of_mode, ml_mode]
            .into_iter()
            .any(|mode| mode == 0x3),
        "expected a prepared dictionary to enable repeat-mode sequence tables on block 1, got modes {modes:?}"
    );
}

#[test]
fn trained_dictionary_small_input_trace_uses_dict_match_state_mode() {
    let Some(helper) = helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-trained-dict", &[]);
    let prepared = EncoderDictionary::new(&dictionary).unwrap();
    let input = build_trained_dictionary_input(6_000);
    let trace = trace_first_block_with_prepared_dict_and_options(
        &input,
        &prepared,
        EncoderOptions {
            block_size: 64 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::FASTEST,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        trace.compression_parameters.dictionary_mode,
        BlockTraceDictionaryMode::DictMatchState
    );
    assert!(trace.compression_parameters.prepared_match_state);
    assert_eq!(
        trace.compression_parameters.dict_table_source,
        BlockTraceDictionaryTableSource::Prepared
    );
    assert_eq!(trace.parser_stats.regular_match_sources.prefix, 0);
    assert_eq!(trace.parser_stats.rep_match_sources.prefix, 0);
    assert!(
        trace.parser_stats.regular_match_sources.dict + trace.parser_stats.rep_match_sources.dict
            > 0,
        "expected trained-dictionary trace to exercise prepared dictionary matches"
    );
}

#[test]
fn trained_dictionary_large_input_trace_uses_extdict_prepared_tables() {
    let Some(helper) = helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-trained-dict", &[]);
    let prepared = EncoderDictionary::new(&dictionary).unwrap();
    let input = build_trained_dictionary_input(64 * 1024);
    let trace = trace_first_block_with_prepared_dict_and_options(
        &input,
        &prepared,
        EncoderOptions {
            block_size: 64 * 1024,
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::try_new(6).unwrap(),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        trace.compression_parameters.dictionary_mode,
        BlockTraceDictionaryMode::ExtDict
    );
    assert!(trace.compression_parameters.prepared_match_state);
    assert_eq!(
        trace.compression_parameters.dict_table_source,
        BlockTraceDictionaryTableSource::Prepared
    );
    assert_eq!(trace.parser_stats.regular_match_sources.dict, 0);
    assert_eq!(trace.parser_stats.rep_match_sources.dict, 0);
    assert!(
        trace.parser_stats.regular_match_sources.prefix
            + trace.parser_stats.rep_match_sources.prefix
            > 0,
        "expected prepared extdict matches on large trained-dictionary input"
    );
}

#[test]
fn trained_dictionary_large_levels_follow_upstream_attach_cutoff() {
    let Some(helper) = helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-trained-dict", &[]);
    let prepared = EncoderDictionary::new(&dictionary).unwrap();
    let input = build_trained_dictionary_input(64 * 1024);

    for level in [5, 6, 7] {
        let trace = trace_first_block_with_prepared_dict_and_options(
            &input,
            &prepared,
            EncoderOptions {
                block_size: 64 * 1024,
                checksum: false,
                write_dict_id: true,
                compression_level: CompressionLevel::try_new(level).unwrap(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            trace.compression_parameters.dictionary_mode,
            BlockTraceDictionaryMode::ExtDict,
            "level {level} should use extDict mode once the source is above the attach cutoff"
        );
        assert!(trace.compression_parameters.prepared_match_state);
        assert_eq!(
            trace.compression_parameters.dict_table_source,
            BlockTraceDictionaryTableSource::Prepared
        );
        assert_eq!(trace.parser_stats.regular_match_sources.dict, 0);
        assert_eq!(trace.parser_stats.rep_match_sources.dict, 0);
        assert!(
            trace.parser_stats.regular_match_sources.prefix
                + trace.parser_stats.rep_match_sources.prefix
                > 0,
            "level {level} should still exercise prepared dictionary matches"
        );
        if let Some(contest) = trace.parser_stats.first_row_search_contest {
            assert_eq!(contest.winner, BlockTraceMatchSource::Prefix);
            assert!(contest.source_length >= 4);
            assert!(contest.dict_length >= 4);
        }
        let emitted = trace
            .parser_stats
            .first_emitted_match
            .expect("row levels should emit a first traced sequence");
        assert_eq!(emitted.kind, BlockTraceEmittedMatchKind::Regular);
        assert_eq!(emitted.source, BlockTraceMatchSource::Prefix);
        let second_emitted = trace
            .parser_stats
            .second_emitted_match
            .expect("row levels should emit a second traced sequence");
        assert_eq!(second_emitted.kind, BlockTraceEmittedMatchKind::Regular);
        assert_eq!(second_emitted.source, BlockTraceMatchSource::Prefix);
    }
}

#[test]
fn trained_dictionary_bad_block_first_four_sequences_match_upstream() {
    let Some(helper) = helper_path() else {
        return;
    };
    let Some(upstream_helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-trained-dict", &[]);
    let prepared = EncoderDictionary::new(&dictionary).unwrap();
    let case = benchmark_corpora::benchmark_report_cases(512 * 1024)
        .into_iter()
        .find(|case| case.name == "trained-dictionary")
        .expect("trained-dictionary benchmark case should exist");
    let input = &case.input[..128 * 1024];

    for level in [5, 6, 7] {
        let trace = trace_first_block_with_prepared_dict_and_options(
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
        let upstream_sequences = upstream_trace_helper::trace_trained_dict_sequences(
            upstream_helper,
            level,
            false,
            4,
            input,
        );
        let rust_sequences = [
            trace.parser_stats.first_emitted_match,
            trace.parser_stats.second_emitted_match,
            trace.parser_stats.third_emitted_match,
            trace.parser_stats.fourth_emitted_match,
        ];

        assert_eq!(
            upstream_sequences.len(),
            4,
            "level {level} should produce four upstream trace sequences"
        );
        for (index, (upstream, rust)) in upstream_sequences.iter().zip(rust_sequences).enumerate() {
            let rust = rust.unwrap_or_else(|| {
                panic!("level {level} missing Rust emitted sequence {}", index + 1)
            });
            assert_eq!(
                rust.kind,
                block_trace_kind_from_upstream(*upstream),
                "level {level} sequence {} kind",
                index + 1
            );
            assert_eq!(
                rust.source,
                block_trace_source_from_upstream(*upstream),
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

#[test]
fn trained_dictionary_levels_use_upstream_applied_cparams() {
    let Some(helper) = helper_path() else {
        return;
    };
    let Some(upstream_helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-trained-dict", &[]);
    let prepared = EncoderDictionary::new(&dictionary).unwrap();
    let input = build_trained_dictionary_input(256 * 1024);
    let block_size = 128 * 1024;
    for level in [1, 3, 5, 6, 7] {
        let trace = trace_first_block_with_prepared_dict_and_options(
            &input,
            &prepared,
            EncoderOptions {
                block_size,
                checksum: false,
                write_dict_id: true,
                compression_level: CompressionLevel::try_new(level).unwrap(),
                ..Default::default()
            },
        )
        .unwrap();
        let upstream = upstream_trace_helper::trace_trained_dict_applied_cparams(
            upstream_helper,
            level,
            false,
            &input,
        );

        assert_eq!(trace.compression_parameters.window_log, upstream.window_log);
        assert_eq!(trace.compression_parameters.chain_log, upstream.chain_log);
        assert_eq!(trace.compression_parameters.hash_log, upstream.hash_log);
        assert_eq!(trace.compression_parameters.search_log, upstream.search_log);
        assert_eq!(trace.compression_parameters.min_match, upstream.min_match);
        assert_eq!(
            trace.compression_parameters.target_length,
            upstream.target_length
        );
        assert_eq!(
            trace.compression_parameters.strategy,
            block_trace_strategy_from_upstream(upstream.strategy),
            "level {level} upstream strategy",
        );
        assert_eq!(
            trace.compression_parameters.dictionary_mode,
            BlockTraceDictionaryMode::ExtDict,
            "level {level} should use extDict mode for large trained-dictionary inputs",
        );
        assert!(trace.compression_parameters.prepared_match_state);
        assert_eq!(
            trace.compression_parameters.dict_table_source,
            BlockTraceDictionaryTableSource::Prepared
        );
        assert_eq!(trace.parser_stats.regular_match_sources.dict, 0);
        assert_eq!(trace.parser_stats.rep_match_sources.dict, 0);
    }
}

fn block_trace_kind_from_upstream(
    sequence: upstream_trace_helper::UpstreamSequenceTrace,
) -> BlockTraceEmittedMatchKind {
    match sequence.kind {
        upstream_trace_helper::UpstreamSequenceKind::Regular => BlockTraceEmittedMatchKind::Regular,
        upstream_trace_helper::UpstreamSequenceKind::Rep => BlockTraceEmittedMatchKind::Rep,
    }
}

fn block_trace_source_from_upstream(
    sequence: upstream_trace_helper::UpstreamSequenceTrace,
) -> BlockTraceMatchSource {
    match sequence.source {
        upstream_trace_helper::UpstreamSequenceSource::Dict => BlockTraceMatchSource::Dict,
        upstream_trace_helper::UpstreamSequenceSource::Prefix => BlockTraceMatchSource::Prefix,
        upstream_trace_helper::UpstreamSequenceSource::Source => BlockTraceMatchSource::Source,
    }
}

fn block_trace_strategy_from_upstream(strategy: u32) -> BlockTraceUpstreamStrategy {
    match strategy {
        1 => BlockTraceUpstreamStrategy::Fast,
        2 => BlockTraceUpstreamStrategy::DoubleFast,
        3 => BlockTraceUpstreamStrategy::Greedy,
        4 => BlockTraceUpstreamStrategy::Lazy,
        5 => BlockTraceUpstreamStrategy::Lazy2,
        6 => BlockTraceUpstreamStrategy::BinaryTreeLazy2,
        7 => BlockTraceUpstreamStrategy::BinaryTreeOpt,
        8 => BlockTraceUpstreamStrategy::BinaryTreeUltra,
        9 => BlockTraceUpstreamStrategy::BinaryTreeUltra2,
        other => panic!("unexpected upstream strategy {other}"),
    }
}

#[test]
fn rust_decompresses_upstream_dictionary_matrix_output() {
    let Some(helper) = helper_path() else {
        return;
    };

    let raw_dictionary = run_helper(helper, "emit-raw-dict", &[]);
    let trained_dictionary = run_helper(helper, "emit-trained-dict", &[]);
    let configs = [(3, false), (9, true)];

    for (input, level, checksum) in [
        (build_raw_dictionary_input(24_000), 3, false),
        (build_raw_dictionary_input(180_000), 9, true),
    ] {
        let encoded = run_configured_compress(
            helper,
            "compress-raw-dict-configured",
            level,
            checksum,
            &input,
        );
        let header = match parse_frame_header(&encoded).unwrap() {
            FrameHeader::Zstandard(header) => header,
            FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
        };

        assert_eq!(header.checksum, checksum);
        let decoded = decode_all_with_dict(&encoded, &raw_dictionary).unwrap();
        assert_eq!(decoded, input);
    }

    for input in [
        build_trained_dictionary_input(24_000),
        build_trained_dictionary_input(200_000),
    ] {
        for (level, checksum) in configs {
            let encoded = run_configured_compress(
                helper,
                "compress-trained-dict-configured",
                level,
                checksum,
                &input,
            );
            let header = match parse_frame_header(&encoded).unwrap() {
                FrameHeader::Zstandard(header) => header,
                FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
            };

            assert_eq!(header.checksum, checksum);
            assert!(header.dictionary_id.is_some());

            let decoded = decode_all_with_dict(&encoded, &trained_dictionary).unwrap();
            assert_eq!(decoded, input);
        }
    }
}

fn helper_path() -> Option<&'static PathBuf> {
    HELPER.get_or_init(build_helper).as_ref()
}

fn golden_decompression_fixture(name: &str) -> Option<Vec<u8>> {
    let path = upstream_trace_helper::upstream_dir()
        .join("tests/golden-decompression")
        .join(name);
    fs::read(path).ok()
}

fn build_helper() -> Option<PathBuf> {
    let upstream = upstream_trace_helper::upstream_dir_or_skip("upstream interop tests")?;
    if Command::new("cc")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("skipping upstream interop tests: cc is not available");
        return None;
    }

    let temp_dir =
        env::temp_dir().join(format!("zstandard-upstream-interop-{}", std::process::id()));
    if let Err(err) = fs::create_dir_all(&temp_dir) {
        panic!("failed to create temporary helper directory {temp_dir:?}: {err}");
    }

    let source_path = temp_dir.join("helper.c");
    let binary_path = temp_dir.join("helper");
    let source = r#"
#define ZSTD_STATIC_LINKING_ONLY
#include "zstd.h"
#include "zdict.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static unsigned char* read_all_stdin(size_t* size_out) {
    size_t cap = 4096;
    size_t size = 0;
    unsigned char* buf = (unsigned char*)malloc(cap ? cap : 1);
    if (buf == NULL) {
        fprintf(stderr, "malloc failed\n");
        exit(1);
    }

    for (;;) {
        size_t remaining = cap - size;
        size_t n = fread(buf + size, 1, remaining, stdin);
        size += n;
        if (n < remaining) {
            if (feof(stdin)) {
                *size_out = size;
                return buf;
            }
            fprintf(stderr, "stdin read failed\n");
            exit(1);
        }
        cap *= 2;
        unsigned char* next = (unsigned char*)realloc(buf, cap);
        if (next == NULL) {
            fprintf(stderr, "realloc failed\n");
            exit(1);
        }
        buf = next;
    }
}

static void write_all_stdout(const void* data, size_t size) {
    if (size == 0) {
        return;
    }
    if (fwrite(data, 1, size, stdout) != size) {
        fprintf(stderr, "stdout write failed\n");
        exit(1);
    }
}

static const unsigned char RAW_DICT_BYTES[] =
    "GET /api/v1/users?id=123&status=active HTTP/1.1\r\n"
    "Host: example.internal\r\n"
    "Accept: application/json\r\n"
    "{\"status\":\"active\",\"role\":\"admin\",\"region\":\"us-central\"}\n";

static void build_trained_dict(unsigned char** dict_out, size_t* dict_size_out) {
    static const char* ORDER_STATUSES[] = { "open", "closed", "pending" };
    static const char* INVOICE_STATUSES[] = { "draft", "final", "paid" };
    static const char* BUILD_STATES[] = { "running", "passed", "failed" };
    static const char* BRANCHES[] = { "main", "release", "hotfix" };
    static const char* REGIONS[] = { "us-east", "eu-west", "ap-south" };
    enum { TRAIN_SAMPLE_COUNT = 64, SAMPLE_CAPACITY = 320, DICT_CAPACITY = 512 };
    size_t sample_sizes[TRAIN_SAMPLE_COUNT];
    unsigned char* samples;
    unsigned char* dict;
    unsigned char* cursor;
    size_t dict_size;
    size_t i;
 
    samples = (unsigned char*)malloc(TRAIN_SAMPLE_COUNT * SAMPLE_CAPACITY);
    dict = (unsigned char*)malloc(DICT_CAPACITY);
    if (samples == NULL || dict == NULL) {
        fprintf(stderr, "allocation failed\n");
        exit(1);
    }

    cursor = samples;
    for (i = 0; i < TRAIN_SAMPLE_COUNT; ++i) {
        char sample[SAMPLE_CAPACITY];
        unsigned customer_id = 10000u + (unsigned)i * 7u;
        unsigned project_id = 4000u + (unsigned)i * 3u;
        unsigned build_id = 9000u + (unsigned)i * 5u;
        int written;

        if ((i % 3) == 0) {
            written = snprintf(
                sample,
                sizeof(sample),
                "GET /v2/customers/%u/orders?status=%s&limit=50\n"
                "{\"customer_id\":%u,\"status\":\"%s\",\"region\":\"%s\","
                "\"items\":[{\"sku\":\"A-%u\",\"qty\":%u}]}\n",
                customer_id,
                ORDER_STATUSES[i % 3],
                customer_id,
                ORDER_STATUSES[i % 3],
                REGIONS[i % 3],
                100u + (unsigned)(i % 17),
                1u + (unsigned)(i % 4)
            );
        } else if ((i % 3) == 1) {
            written = snprintf(
                sample,
                sizeof(sample),
                "POST /v2/customers/%u/invoices\n"
                "{\"customer_id\":%u,\"currency\":\"USD\",\"total\":%u,"
                "\"status\":\"%s\",\"region\":\"%s\"}\n",
                customer_id,
                customer_id,
                1500u + (unsigned)i * 11u,
                INVOICE_STATUSES[i % 3],
                REGIONS[i % 3]
            );
        } else {
            written = snprintf(
                sample,
                sizeof(sample),
                "PATCH /v2/projects/%u/builds/%u\n"
                "{\"project\":%u,\"build\":%u,\"state\":\"%s\","
                "\"branch\":\"%s\",\"artifact\":\"bundle.tar\"}\n",
                project_id,
                build_id,
                project_id,
                build_id,
                BUILD_STATES[i % 3],
                BRANCHES[i % 3]
            );
        }

        if (written < 0 || written >= SAMPLE_CAPACITY) {
            fprintf(stderr, "failed to generate training sample\n");
            exit(1);
        }

        sample_sizes[i] = (size_t)written;
        memcpy(cursor, sample, sample_sizes[i]);
        cursor += sample_sizes[i];
    }

    dict_size = ZDICT_trainFromBuffer(dict, DICT_CAPACITY, samples, sample_sizes, TRAIN_SAMPLE_COUNT);
    free(samples);
    if (ZDICT_isError(dict_size)) {
        fprintf(stderr, "train-dict failed: %s\n", ZDICT_getErrorName(dict_size));
        exit(1);
    }

    *dict_out = dict;
    *dict_size_out = dict_size;
}

static int parse_int_arg(const char* value, const char* name) {
    char* end = NULL;
    long parsed = strtol(value, &end, 10);
    if (value[0] == '\0' || end == value || *end != '\0') {
        fprintf(stderr, "invalid %s: %s\n", name, value);
        exit(2);
    }
    return (int)parsed;
}

static int parse_bool_arg(const char* value, const char* name) {
    int parsed = parse_int_arg(value, name);
    if (parsed != 0 && parsed != 1) {
        fprintf(stderr, "%s must be 0 or 1\n", name);
        exit(2);
    }
    return parsed;
}

static void write_compressed_output(
    const unsigned char* src,
    size_t src_size,
    int level,
    int checksum,
    const void* dict,
    size_t dict_size
) {
    ZSTD_CCtx* cctx = ZSTD_createCCtx();
    size_t dst_capacity = ZSTD_compressBound(src_size);
    void* dst = malloc(dst_capacity ? dst_capacity : 1);
    size_t compressed_size;

    if (cctx == NULL || dst == NULL) {
        fprintf(stderr, "allocation failed\n");
        exit(2);
    }
    if (ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_compressionLevel, level)) ||
        ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_contentSizeFlag, 1)) ||
        ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_checksumFlag, checksum))) {
        fprintf(stderr, "parameter setup failed\n");
        exit(2);
    }

    if (dict != NULL && dict_size != 0) {
        if (ZSTD_isError(ZSTD_CCtx_loadDictionary(cctx, dict, dict_size))) {
            fprintf(stderr, "dictionary setup failed\n");
            exit(2);
        }
    }
    compressed_size = ZSTD_compress2(cctx, dst, dst_capacity, src, src_size);

    if (ZSTD_isError(compressed_size)) {
        fprintf(stderr, "compress failed: %s\n", ZSTD_getErrorName(compressed_size));
        exit(2);
    }

    write_all_stdout(dst, compressed_size);
    free(dst);
    ZSTD_freeCCtx(cctx);
}

static void stream_decompress_to_stdout(const unsigned char* src, size_t src_size) {
    ZSTD_DCtx* dctx = ZSTD_createDCtx();
    ZSTD_inBuffer input = { src, src_size, 0 };
    unsigned char buffer[1 << 15];

    if (dctx == NULL) {
        fprintf(stderr, "allocation failed\n");
        exit(2);
    }

    for (;;) {
        ZSTD_outBuffer output = { buffer, sizeof(buffer), 0 };
        size_t result = ZSTD_decompressStream(dctx, &output, &input);
        if (ZSTD_isError(result)) {
            fprintf(stderr, "decompress-stream failed: %s\n", ZSTD_getErrorName(result));
            exit(2);
        }
        write_all_stdout(buffer, output.pos);
        if (result == 0 && input.pos == input.size) {
            break;
        }
    }

    ZSTD_freeDCtx(dctx);
}

static void stream_decompress_with_dict_to_stdout(
    const unsigned char* src,
    size_t src_size,
    const void* dict,
    size_t dict_size
) {
    ZSTD_DCtx* dctx = ZSTD_createDCtx();
    ZSTD_inBuffer input = { src, src_size, 0 };
    unsigned char buffer[1 << 15];

    if (dctx == NULL) {
        fprintf(stderr, "allocation failed\n");
        exit(2);
    }
    if (ZSTD_isError(ZSTD_DCtx_loadDictionary(dctx, dict, dict_size))) {
        fprintf(stderr, "dictionary setup failed\n");
        exit(2);
    }

    for (;;) {
        ZSTD_outBuffer output = { buffer, sizeof(buffer), 0 };
        size_t result = ZSTD_decompressStream(dctx, &output, &input);
        if (ZSTD_isError(result)) {
            fprintf(stderr, "decompress-stream failed: %s\n", ZSTD_getErrorName(result));
            exit(2);
        }
        write_all_stdout(buffer, output.pos);
        if (result == 0 && input.pos == input.size) {
            break;
        }
    }

    ZSTD_freeDCtx(dctx);
}

int main(int argc, char** argv) {
    size_t src_size = 0;
    unsigned char* src = read_all_stdin(&src_size);

    if (argc < 2) {
        fprintf(stderr, "usage: helper <mode> [args]\n");
        return 2;
    }

    if (strcmp(argv[1], "emit-raw-dict") == 0) {
        write_all_stdout(RAW_DICT_BYTES, sizeof(RAW_DICT_BYTES) - 1);
        return 0;
    }

    if (strcmp(argv[1], "compress-no-seqs") == 0) {
        ZSTD_CCtx* cctx = ZSTD_createCCtx();
        size_t dst_capacity = ZSTD_compressBound(src_size);
        void* dst = malloc(dst_capacity ? dst_capacity : 1);
        size_t compressed_size;

        if (cctx == NULL || dst == NULL) {
            fprintf(stderr, "allocation failed\n");
            return 2;
        }
        if (ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_compressionLevel, 1)) ||
            ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_contentSizeFlag, 1)) ||
            ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_checksumFlag, 0)) ||
            ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_literalCompressionMode, ZSTD_ps_disable))) {
            fprintf(stderr, "parameter setup failed\n");
            return 2;
        }

        compressed_size = ZSTD_compressSequences(cctx, dst, dst_capacity, NULL, 0, src, src_size);
        if (ZSTD_isError(compressed_size)) {
            fprintf(stderr, "compress-no-seqs failed: %s\n", ZSTD_getErrorName(compressed_size));
            return 2;
        }
        write_all_stdout(dst, compressed_size);
        return 0;
    }

    if (strcmp(argv[1], "compress-literals-no-seqs") == 0) {
        ZSTD_CCtx* cctx = ZSTD_createCCtx();
        size_t dst_capacity = ZSTD_compressBound(src_size);
        void* dst = malloc(dst_capacity ? dst_capacity : 1);
        size_t compressed_size;

        if (cctx == NULL || dst == NULL) {
            fprintf(stderr, "allocation failed\n");
            return 2;
        }
        if (ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_compressionLevel, 5)) ||
            ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_contentSizeFlag, 1)) ||
            ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_checksumFlag, 0)) ||
            ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_literalCompressionMode, ZSTD_ps_enable))) {
            fprintf(stderr, "parameter setup failed\n");
            return 2;
        }

        compressed_size = ZSTD_compressSequences(cctx, dst, dst_capacity, NULL, 0, src, src_size);
        if (ZSTD_isError(compressed_size)) {
            fprintf(stderr, "compress-literals-no-seqs failed: %s\n", ZSTD_getErrorName(compressed_size));
            return 2;
        }
        write_all_stdout(dst, compressed_size);
        return 0;
    }

    if (strcmp(argv[1], "compress-regular") == 0) {
        write_compressed_output(src, src_size, 5, 0, NULL, 0);
        return 0;
    }

    if (strcmp(argv[1], "compress-raw-dict") == 0) {
        write_compressed_output(
            src,
            src_size,
            5,
            0,
            RAW_DICT_BYTES,
            sizeof(RAW_DICT_BYTES) - 1
        );
        return 0;
    }

    if (strcmp(argv[1], "emit-trained-dict") == 0) {
        unsigned char* dict;
        size_t dict_size;
        build_trained_dict(&dict, &dict_size);
        write_all_stdout(dict, dict_size);
        free(dict);
        return 0;
    }

    if (strcmp(argv[1], "compress-trained-dict") == 0) {
        unsigned char* dict;
        size_t dict_size;
        build_trained_dict(&dict, &dict_size);
        write_compressed_output(src, src_size, 5, 0, dict, dict_size);
        free(dict);
        return 0;
    }

    if (strcmp(argv[1], "compress-regular-configured") == 0) {
        int level;
        int checksum;
        if (argc != 4) {
            fprintf(stderr, "usage: helper compress-regular-configured <level> <checksum>\n");
            return 2;
        }
        level = parse_int_arg(argv[2], "level");
        checksum = parse_bool_arg(argv[3], "checksum");
        write_compressed_output(src, src_size, level, checksum, NULL, 0);
        return 0;
    }

    if (strcmp(argv[1], "compress-raw-dict-configured") == 0) {
        int level;
        int checksum;
        if (argc != 4) {
            fprintf(stderr, "usage: helper compress-raw-dict-configured <level> <checksum>\n");
            return 2;
        }
        level = parse_int_arg(argv[2], "level");
        checksum = parse_bool_arg(argv[3], "checksum");
        write_compressed_output(
            src,
            src_size,
            level,
            checksum,
            RAW_DICT_BYTES,
            sizeof(RAW_DICT_BYTES) - 1
        );
        return 0;
    }

    if (strcmp(argv[1], "compress-trained-dict-configured") == 0) {
        unsigned char* dict;
        size_t dict_size;
        int level;
        int checksum;
        if (argc != 4) {
            fprintf(stderr, "usage: helper compress-trained-dict-configured <level> <checksum>\n");
            return 2;
        }
        level = parse_int_arg(argv[2], "level");
        checksum = parse_bool_arg(argv[3], "checksum");
        build_trained_dict(&dict, &dict_size);
        write_compressed_output(src, src_size, level, checksum, dict, dict_size);
        free(dict);
        return 0;
    }

    if (strcmp(argv[1], "decompress") == 0) {
        unsigned long long dst_size_ull = ZSTD_findDecompressedSize(src, src_size);
        size_t dst_size;
        void* dst;
        size_t actual_size;

        if (dst_size_ull == ZSTD_CONTENTSIZE_ERROR || dst_size_ull == ZSTD_CONTENTSIZE_UNKNOWN) {
            fprintf(stderr, "decompressed size unavailable\n");
            return 2;
        }

        dst_size = (size_t)dst_size_ull;
        dst = malloc(dst_size ? dst_size : 1);
        if (dst == NULL) {
            fprintf(stderr, "allocation failed\n");
            return 2;
        }

        actual_size = ZSTD_decompress(dst, dst_size, src, src_size);
        if (ZSTD_isError(actual_size)) {
            fprintf(stderr, "decompress failed: %s\n", ZSTD_getErrorName(actual_size));
            return 2;
        }
        write_all_stdout(dst, actual_size);
        return 0;
    }

    if (strcmp(argv[1], "decompress-stream") == 0) {
        stream_decompress_to_stdout(src, src_size);
        return 0;
    }

    if (strcmp(argv[1], "decompress-stream-raw-dict") == 0) {
        stream_decompress_with_dict_to_stdout(
            src,
            src_size,
            RAW_DICT_BYTES,
            sizeof(RAW_DICT_BYTES) - 1
        );
        return 0;
    }

    if (strcmp(argv[1], "decompress-stream-trained-dict") == 0) {
        unsigned char* dict;
        size_t dict_size;
        build_trained_dict(&dict, &dict_size);
        stream_decompress_with_dict_to_stdout(src, src_size, dict, dict_size);
        free(dict);
        return 0;
    }

    if (strcmp(argv[1], "decompress-must-fail") == 0) {
        size_t dst_capacity = src_size * 64 + (1U << 20);
        void* dst = malloc(dst_capacity ? dst_capacity : 1);
        size_t result;

        if (dst == NULL) {
            fprintf(stderr, "allocation failed\n");
            return 2;
        }

        result = ZSTD_decompress(dst, dst_capacity, src, src_size);
        if (ZSTD_isError(result)) {
            return 0;
        }

        fprintf(stderr, "decompress unexpectedly succeeded\n");
        return 3;
    }

    fprintf(stderr, "unknown mode: %s\n", argv[1]);
    return 2;
}
"#;

    fs::write(&source_path, source).unwrap();

    let mut sources = Vec::new();
    sources.push(source_path.clone());
    sources.extend(collect_c_sources(&upstream.join("lib/common"), &[]));
    sources.extend(collect_c_sources(
        &upstream.join("lib/compress"),
        &["zstdmt_compress.c"],
    ));
    sources.extend(collect_c_sources(&upstream.join("lib/decompress"), &[]));
    sources.extend(collect_c_sources(&upstream.join("lib/dictBuilder"), &[]));

    let mut command = Command::new("cc");
    command
        .arg("-O2")
        .arg("-std=c99")
        .arg("-DZSTD_DISABLE_ASM")
        .arg(format!("-I{}", upstream.join("lib").display()))
        .arg(format!("-I{}", upstream.join("lib/common").display()))
        .arg(format!("-I{}", upstream.join("lib/compress").display()))
        .arg(format!("-I{}", upstream.join("lib/decompress").display()))
        .arg(format!("-I{}", upstream.join("lib/dictBuilder").display()));
    for source in &sources {
        command.arg(source);
    }
    let status = command.arg("-o").arg(&binary_path).status().unwrap();
    assert!(status.success(), "failed to build upstream zstd helper");

    Some(binary_path)
}

fn run_helper(helper: &Path, mode: &str, input: &[u8]) -> Vec<u8> {
    run_helper_args(helper, &[mode], input)
}

/// Whether the reference decoder accepted `input`, rather than what it produced.
/// The helper's decode modes exit 2 when `libzstd` reports an error, so the exit
/// status is the verdict. `run_helper` asserts success and so cannot express a
/// rejection at all.
fn helper_accepts(helper: &Path, mode: &str, input: &[u8]) -> bool {
    let mut child = Command::new(helper)
        .arg(mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // The helper reads stdin to EOF before decoding, so the pipe has to be
    // closed or it blocks forever. A short write means it exited early, which is
    // a rejection and not a harness failure.
    {
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(input);
    }

    child.wait().unwrap().success()
}

fn run_helper_args(helper: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let command_name = args.join(" ");
    let mut child = Command::new(helper)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "helper {command_name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn run_configured_compress(
    helper: &Path,
    mode: &str,
    level: i32,
    checksum: bool,
    input: &[u8],
) -> Vec<u8> {
    let level_arg = level.to_string();
    let checksum_arg = if checksum { "1" } else { "0" };
    run_helper_args(helper, &[mode, &level_arg, checksum_arg], input)
}

fn helper_rejects_frame(helper: &Path, input: &[u8]) -> bool {
    Command::new(helper)
        .arg("decompress-must-fail")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| {
            child.stdin.as_mut().unwrap().write_all(input)?;
            child.wait()
        })
        .map(|status| status.success())
        .unwrap()
}

fn drain_decoder(decoder: &mut StreamingDecoder<'_>, scratch: &mut [u8], output: &mut Vec<u8>) {
    while decoder.pending_output_len() != 0 {
        let count = decoder.read(scratch);
        assert!(
            count != 0,
            "decoder reported pending output but returned zero bytes"
        );
        output.extend_from_slice(&scratch[..count]);
    }
}

fn build_pattern(size: usize) -> Vec<u8> {
    (0..size)
        .map(|index| ((index * 17) as u8).wrapping_add((index >> 5) as u8))
        .collect()
}

fn build_small_alphabet_pattern(size: usize) -> Vec<u8> {
    (0..size)
        .map(|index| match index & 0x0f {
            0..=8 => b'A',
            9..=12 => b'B',
            13..=14 => b'C',
            _ => b'D',
        })
        .collect()
}

fn build_huff_friendly_pattern(size: usize) -> Vec<u8> {
    let mut state = 0x1234_5678u32;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            b'A' + ((state & 0x0f) as u8)
        })
        .collect()
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

fn build_short_match_pattern(size: usize) -> Vec<u8> {
    let mut state = 0x1234_5678u32;
    let mut out = Vec::with_capacity(size);
    while out.len() + 8 <= size {
        out.extend_from_slice(b"ABCDE");
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        out.extend_from_slice(&[state as u8, (state >> 8) as u8, (state >> 16) as u8]);
    }
    while out.len() < size {
        state = state.rotate_left(5) ^ 0x9E37_79B9;
        out.push(state as u8);
    }
    out
}

fn build_raw_dictionary_input(size: usize) -> Vec<u8> {
    let statuses = ["active", "pending", "disabled"];
    let roles = ["admin", "analyst", "operator"];
    let regions = ["us-central", "us-east", "eu-west"];

    let mut out = Vec::with_capacity(size);
    let mut user_id = 1_000u32;
    while out.len() < size {
        let status = statuses[user_id as usize % statuses.len()];
        let role = roles[(user_id as usize / 2) % roles.len()];
        let region = regions[(user_id as usize / 3) % regions.len()];
        let record = format!(
            "GET /api/v1/users?id={user_id}&status={status} HTTP/1.1\r\n\
Host: example.internal\r\n\
Accept: application/json\r\n\
{{\"status\":\"{status}\",\"role\":\"{role}\",\"region\":\"{region}\"}}\n"
        );
        let remaining = size - out.len();
        out.extend_from_slice(&record.as_bytes()[..remaining.min(record.len())]);
        user_id += 1;
    }
    out
}

fn build_trained_dictionary_input(size: usize) -> Vec<u8> {
    let order_statuses = ["open", "closed", "pending"];
    let invoice_statuses = ["draft", "final", "paid"];
    let build_states = ["running", "passed", "failed"];
    let branches = ["main", "release", "hotfix"];
    let regions = ["us-east", "eu-west", "ap-south"];

    let mut out = Vec::with_capacity(size);
    let mut index = 0u32;
    while out.len() < size {
        let customer_id = 10_000 + index * 7;
        let project_id = 4_000 + index * 3;
        let build_id = 9_000 + index * 5;
        let status = order_statuses[index as usize % order_statuses.len()];
        let invoice_status = invoice_statuses[index as usize % invoice_statuses.len()];
        let build_state = build_states[index as usize % build_states.len()];
        let branch = branches[index as usize % branches.len()];
        let region = regions[index as usize % regions.len()];
        let record = match index % 3 {
            0 => format!(
                "GET /v2/customers/{customer_id}/orders?status={status}&limit=50\n\
{{\"customer_id\":{customer_id},\"status\":\"{status}\",\"region\":\"{region}\",\"items\":[{{\"sku\":\"A-{sku}\",\"qty\":{qty}}}]}}\n",
                sku = 100 + (index % 17),
                qty = 1 + (index % 4),
            ),
            1 => format!(
                "POST /v2/customers/{customer_id}/invoices\n\
{{\"customer_id\":{customer_id},\"currency\":\"USD\",\"total\":{total},\"status\":\"{invoice_status}\",\"region\":\"{region}\"}}\n",
                total = 1_500 + index * 11,
            ),
            _ => format!(
                "PATCH /v2/projects/{project_id}/builds/{build_id}\n\
{{\"project\":{project_id},\"build\":{build_id},\"state\":\"{build_state}\",\"branch\":\"{branch}\",\"artifact\":\"bundle.tar\"}}\n",
            ),
        };

        let remaining = size - out.len();
        out.extend_from_slice(&record.as_bytes()[..remaining.min(record.len())]);
        index += 1;
    }
    out
}

fn malformed_reserved_sequence_mode_frame() -> Vec<u8> {
    malformed_sequence_frame(0, &[1, 0x01])
}

fn malformed_frame_header_reserved_bit_frame() -> Vec<u8> {
    vec![0x28, 0xB5, 0x2F, 0xFD, (1 << 5) | (1 << 3), 0]
}

fn malformed_truncated_dictionary_id_frame() -> Vec<u8> {
    vec![0x28, 0xB5, 0x2F, 0xFD, (1 << 5) | 1]
}

fn malformed_reserved_block_type_frame() -> Vec<u8> {
    let mut frame = write_single_segment_header(0);
    append_custom_block_header(&mut frame, 3, 0, true);
    frame
}

fn malformed_block_too_large_for_frame_frame() -> Vec<u8> {
    let mut frame = write_single_segment_header(1);
    append_custom_block_header(&mut frame, 0, 2, true);
    frame.extend_from_slice(b"ab");
    frame
}

fn malformed_truncated_raw_block_payload_frame() -> Vec<u8> {
    let mut frame = write_single_segment_header(4);
    append_custom_block_header(&mut frame, 0, 4, true);
    frame.extend_from_slice(b"ab");
    frame
}

fn malformed_truncated_rle_block_payload_frame() -> Vec<u8> {
    let mut frame = write_single_segment_header(4);
    append_custom_block_header(&mut frame, 1, 4, true);
    frame
}

fn malformed_truncated_checksum_frame() -> Vec<u8> {
    let mut frame = encode_all_with_options(
        b"checksummed data",
        EncoderOptions {
            block_size: 128 * 1024,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();
    frame.pop();
    frame
}

fn malformed_repeat_without_previous_table_frame() -> Vec<u8> {
    malformed_sequence_frame(0, &[1, 0b1111_1100])
}

fn malformed_truncated_sequence_fse_table_frame() -> Vec<u8> {
    malformed_sequence_frame(0, &[1, 0b1000_0000])
}

fn malformed_zero_sequence_trailing_payload_frame() -> Vec<u8> {
    malformed_sequence_frame(0, &[0, 0xAA])
}

fn malformed_repeat_offset_underflow_frame() -> Vec<u8> {
    malformed_sequence_frame(3, &[1, 0b0101_0100, 0, 1, 0, 0b0000_0011])
}

fn malformed_offset_past_history_frame() -> Vec<u8> {
    malformed_sequence_frame(3, &[1, 0b0101_0100, 0, 3, 0, 0b0000_1010])
}

fn malformed_sequence_frame(content_size: usize, sequence_section: &[u8]) -> Vec<u8> {
    let mut frame = write_single_segment_header(content_size.max(32));
    let mut payload = vec![0];
    payload.extend_from_slice(sequence_section);
    append_block(&mut frame, BlockType::Compressed, &payload, true);
    frame
}

fn literals_block_types(frame: &[u8]) -> Vec<u8> {
    let header = match parse_frame_header(frame).unwrap() {
        FrameHeader::Zstandard(header) => header,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };

    let mut cursor = header.header_size;
    let mut block_types = Vec::new();
    loop {
        let block = parse_block_header(&frame[cursor..]).unwrap();
        cursor += 3;
        let payload_end = cursor + block_payload_size(block.block_type, block.block_size);
        if block.block_type == BlockType::Compressed {
            block_types.push(frame[cursor] & 0x3);
        }
        cursor = payload_end;
        if block.last_block {
            break;
        }
    }
    block_types
}

fn compressed_block_sequence_counts(frame: &[u8]) -> Vec<usize> {
    let header = match parse_frame_header(frame).unwrap() {
        FrameHeader::Zstandard(header) => header,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };

    let mut cursor = header.header_size;
    let mut counts = Vec::new();
    loop {
        let block = parse_block_header(&frame[cursor..]).unwrap();
        cursor += 3;
        let payload_end = cursor + block_payload_size(block.block_type, block.block_size);
        if block.block_type == BlockType::Compressed {
            let payload = &frame[cursor..payload_end];
            let literals = parse_literals_header(payload);
            counts.push(decode_sequence_count(&payload[literals.payload_end()..]));
        }
        cursor = payload_end;
        if block.last_block {
            break;
        }
    }

    counts
}

fn compressed_block_sequence_count(payload: &[u8]) -> usize {
    let literals = parse_literals_header(payload);
    decode_sequence_count(&payload[literals.payload_end()..])
}

fn compressed_block_sequence_modes(payload: &[u8]) -> Option<(u8, u8, u8)> {
    let literals = parse_literals_header(payload);
    let sequence_section = &payload[literals.payload_end()..];
    let sequence_count = decode_sequence_count(sequence_section);
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
    let modes = sequence_section[mode_index];
    Some(((modes >> 6) & 0x3, (modes >> 4) & 0x3, (modes >> 2) & 0x3))
}

fn first_compressed_block_payload(frame: &[u8]) -> Vec<u8> {
    let header = match parse_frame_header(frame).unwrap() {
        FrameHeader::Zstandard(header) => header,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let block = parse_block_header(&frame[header.header_size..]).unwrap();
    assert_eq!(block.block_type, BlockType::Compressed);

    let payload_start = header.header_size + 3;
    let payload_end = payload_start + block_payload_size(block.block_type, block.block_size);
    frame[payload_start..payload_end].to_vec()
}

#[derive(Debug, Clone, Copy)]
struct LiteralsHeader {
    block_type: u8,
    size_format: u8,
    header_size: usize,
    regenerated_size: usize,
    compressed_size: usize,
}

impl LiteralsHeader {
    fn payload_end(self) -> usize {
        self.header_size + self.compressed_size
    }
}

fn parse_literals_header(src: &[u8]) -> LiteralsHeader {
    let header0 = src[0];
    let block_type = header0 & 0x3;
    let size_format = (header0 >> 2) & 0x3;

    let (header_size, regenerated_size, compressed_size) = match block_type {
        0 | 1 => match size_format {
            0 | 2 => {
                let size = (header0 >> 3) as usize;
                (1, size, size)
            }
            1 => {
                let size = ((src[0] as usize) >> 4) | ((src[1] as usize) << 4);
                (2, size, size)
            }
            3 => {
                let value =
                    (src[0] as usize) | ((src[1] as usize) << 8) | ((src[2] as usize) << 16);
                let size = value >> 4;
                (3, size, size)
            }
            _ => unreachable!(),
        },
        2 | 3 => match size_format {
            0 | 1 => {
                let value =
                    (src[0] as usize) | ((src[1] as usize) << 8) | ((src[2] as usize) << 16);
                (3, (value >> 4) & 0x03ff, (value >> 14) & 0x03ff)
            }
            2 => {
                let value = u32::from_le_bytes([src[0], src[1], src[2], src[3]]) as usize;
                (4, (value >> 4) & 0x3fff, (value >> 18) & 0x3fff)
            }
            3 => {
                let value = (src[0] as u64)
                    | ((src[1] as u64) << 8)
                    | ((src[2] as u64) << 16)
                    | ((src[3] as u64) << 24)
                    | ((src[4] as u64) << 32);
                (
                    5,
                    ((value >> 4) & 0x3ffff) as usize,
                    ((value >> 22) & 0x3ffff) as usize,
                )
            }
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };

    LiteralsHeader {
        block_type,
        size_format,
        header_size,
        regenerated_size,
        compressed_size,
    }
}

fn decode_sequence_count(src: &[u8]) -> usize {
    let byte0 = src[0] as usize;
    if byte0 < 128 {
        byte0
    } else if byte0 < 255 {
        ((byte0 - 128) << 8) + src[1] as usize
    } else {
        0x7F00 + src[1] as usize + ((src[2] as usize) << 8)
    }
}

fn huffman_tree_description_size(src: &[u8]) -> usize {
    let header = src[0] as usize;
    if header >= 128 {
        1 + ((header - 126) / 2)
    } else {
        1 + header
    }
}

fn encode_compressed_literals_header(
    block_type: u8,
    size_format: u8,
    regenerated_size: usize,
    compressed_size: usize,
) -> Vec<u8> {
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
    let mut out = vec![0u8; header_size];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = ((value >> (index * 8)) & 0xff) as u8;
    }
    out
}

fn write_single_segment_header(content_size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());

    let (fcs_flag, encoded_fcs) = if content_size <= 255 {
        (0u8, vec![content_size as u8])
    } else if content_size <= 65_791 {
        (1u8, ((content_size - 256) as u16).to_le_bytes().to_vec())
    } else if u32::try_from(content_size).is_ok() {
        (2u8, (content_size as u32).to_le_bytes().to_vec())
    } else {
        panic!("content size too large for test frame");
    };

    out.push((fcs_flag << 6) | (1 << 5));
    out.extend_from_slice(&encoded_fcs);
    out
}

fn append_block(frame: &mut Vec<u8>, block_type: BlockType, payload: &[u8], last_block: bool) {
    let block_type_bits = match block_type {
        BlockType::Raw => 0u32,
        BlockType::Rle => 1u32,
        BlockType::Compressed => 2u32,
    };
    append_custom_block_header(frame, block_type_bits, payload.len() as u32, last_block);
    frame.extend_from_slice(payload);
}

fn append_custom_block_header(
    frame: &mut Vec<u8>,
    block_type_bits: u32,
    block_size: u32,
    last_block: bool,
) {
    let value = u32::from(last_block) | (block_type_bits << 1) | (block_size << 3);
    frame.push((value & 0xff) as u8);
    frame.push(((value >> 8) & 0xff) as u8);
    frame.push(((value >> 16) & 0xff) as u8);
}

fn block_payload_size(block_type: BlockType, block_size: u32) -> usize {
    match block_type {
        BlockType::Raw | BlockType::Compressed => block_size as usize,
        BlockType::Rle => 1,
    }
}

fn collect_c_sources(dir: &Path, exclude: &[&str]) -> Vec<PathBuf> {
    let mut entries = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("c"))
        .filter(|path| {
            !exclude
                .iter()
                .any(|name| path.file_name().and_then(|file| file.to_str()) == Some(*name))
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

/// Bytes with the statistical shape of a serialized numeric-array payload:
/// short repeated record headers, then little-endian IEEE-754 arrays whose
/// exponent bytes barely move while their mantissa bytes are close to random,
/// interleaved with monotonic integer columns.
///
/// The shape matters because the two halves land on different parts of the
/// encoder. Headers, exponent bytes and the high bytes of a monotonic counter
/// are what the match finder and the offset coder see; mantissas are
/// incompressible and go through the literals path. A corpus of text or of
/// pure noise exercises one and not the other, and the ratio assertions below
/// exist so that a change which makes this generator degenerate is a test
/// failure rather than a silently easier test.
fn numeric_array_payload(bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes + 4096);
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state
    };

    let mut record = 0u64;
    let mut clock = 1_700_000_000_000_000u64;
    while out.len() < bytes {
        // Record header: a tag, a name, a counter. Repetitive across records
        // but not identical, so the match finder has something to find and the
        // repeat offsets are not trivially correct.
        out.push(0x01);
        out.extend_from_slice(b"samples");
        out.push(0x00);
        out.extend_from_slice(&record.to_le_bytes());
        let count = 64 + (next() >> 40) as usize % 192;
        out.extend_from_slice(&(count as u32).to_le_bytes());

        // A smooth signal: consecutive values share an exponent, so the top
        // two bytes of each f64 repeat while the low six do not.
        let phase = record as f64 * 0.37;
        for index in 0..count {
            let t = index as f64 / 32.0 + phase;
            // The perturbation is deliberate. A pure sine repeats exactly
            // every period, and a body that repeats verbatim lets a broken
            // match finder look correct.
            let jitter = (next() >> 11) as f64 / (1u64 << 53) as f64;
            let value = 20.0 + 4.0 * t.sin() + jitter * 1e-6;
            out.extend_from_slice(&value.to_le_bytes());
        }

        // A monotonic microsecond column: high bytes constant, low bytes not.
        out.push(0x02);
        out.extend_from_slice(b"t");
        out.push(0x00);
        for _ in 0..count {
            clock += 950 + (next() >> 52) % 200;
            out.extend_from_slice(&clock.to_le_bytes());
        }

        // A narrow float column, to cover the 4-byte stride as well as the 8.
        out.push(0x03);
        out.extend_from_slice(b"gain");
        out.push(0x00);
        for index in 0..count / 2 {
            let value = 0.5f32 + (index as f32 / 128.0).cos() * 0.25;
            out.extend_from_slice(&value.to_le_bytes());
        }

        record += 1;
    }

    out.truncate(bytes);
    out
}

/// Level 3 is the only level many callers ever use, and it was covered here
/// only incidentally, on corpora that look nothing like serialized numbers.
///
/// Both directions, because they fail differently: a decode regression makes
/// existing archives unreadable, which no amount of re-running recovers, while
/// an encode regression produces files the rest of the world cannot open.
#[test]
fn level_three_numeric_payloads_interoperate_in_both_directions() {
    let Some(helper) = helper_path() else {
        return;
    };

    // Below one block, several blocks, and across the 128 KiB default boundary
    // in a way that leaves a short final block.
    for size in [9_000usize, 200_000, 1_500_000, 3_000_000] {
        let payload = numeric_array_payload(size);

        // Upstream writes, we read. This is the archive-compatibility
        // direction.
        let upstream_bytes =
            run_configured_compress(helper, "compress-regular-configured", 3, true, &payload);
        assert_eq!(
            decode_all(&upstream_bytes).unwrap(),
            payload,
            "failed to decode upstream level-3 output at {size} bytes"
        );

        // The corpus has to stay in a band where both halves of the encoder do
        // work. Far above this and it is repetitive enough that the literals
        // path is untested; far below and there are no matches to find.
        let ratio = payload.len() as f64 / upstream_bytes.len() as f64;
        // Measured 1.54 at 9 KB and 1.72-1.74 above it. The band is tight
        // deliberately: a loose one would let the generator drift most of the
        // way to degenerate while still passing, which is the failure mode the
        // assertion exists to prevent.
        assert!(
            (1.3..2.5).contains(&ratio),
            "generator has drifted: upstream compresses it {ratio:.2}:1 at {size} bytes"
        );

        // We write, upstream reads. One-shot first.
        let ours = encode_all_with_options(
            &payload,
            EncoderOptions {
                compression_level: CompressionLevel::try_new(3).unwrap(),
                checksum: true,
                ..Default::default()
            },
        )
        .unwrap();
        // Size parity, not just readability. A ratio regression at level 3 on
        // this shape does not break anything visibly; it quietly grows every
        // file written from then on, and nobody looks until a disk fills. As
        // measured against the pinned upstream this is exact at 9 KB and
        // 1.5 MB and within four bytes at the others, so the tolerance below
        // is for parse ties rather than for drift.
        let slack = (upstream_bytes.len() / 1000).max(64);
        assert!(
            ours.len() <= upstream_bytes.len() + slack,
            "level 3 at {size} bytes: {} vs upstream {} ({:+.3}%)",
            ours.len(),
            upstream_bytes.len(),
            (ours.len() as f64 - upstream_bytes.len() as f64) / upstream_bytes.len() as f64 * 100.0
        );

        assert_eq!(
            run_helper(helper, "decompress", &ours),
            payload,
            "upstream rejected our one-shot level-3 output at {size} bytes"
        );

        // Then streaming at the default block size, which is the shape a write
        // path that hands over data as it arrives actually produces. Chunks are
        // deliberately not a divisor of the block size, so block boundaries and
        // push boundaries do not coincide.
        let mut encoder = StreamingEncoder::new(EncoderOptions {
            compression_level: CompressionLevel::try_new(3).unwrap(),
            checksum: true,
            ..Default::default()
        })
        .unwrap();
        for chunk in payload.chunks(37_000) {
            encoder.push(chunk).unwrap();
        }
        encoder.finish().unwrap();
        let streamed = encoder.take_output();
        // `decompress-stream`, because a streaming frame declares no content
        // size and the one-shot helper mode needs one. That the one-shot check
        // above *can* use `decompress` is itself the assertion that our
        // one-shot frames still declare it, which any consumer calling
        // `ZSTD_getFrameContentSize` before allocating depends on.
        assert_eq!(
            run_helper(helper, "decompress-stream", &streamed),
            payload,
            "upstream rejected our streaming level-3 output at {size} bytes"
        );

        // Our own decoder has to agree with upstream's about our streaming
        // bytes, or one of the two is wrong and the round-trip above would not
        // say which.
        assert_eq!(decode_all(&streamed).unwrap(), payload);

        // Size parity for the streaming path too, against the same upstream
        // bytes the one-shot check used. Without this the streaming half of
        // this test asserts only that the output is readable, and readable is
        // the one thing a ratio regression never breaks: the encoder that
        // rebuilt its match finder every block emitted 4x one-shot and every
        // byte of it round-tripped.
        //
        // Upstream here is one-shot, so this compares across a block-boundary
        // difference as well as a parser one, and gets a looser tolerance than
        // the 0.1% above for that reason. Measured against the pinned upstream:
        // -1.74% at 9 KB, +0.112% at 200 KB, and within 8 bytes at 1.5 MB and
        // 3 MB. Only the 200 KB case is over, and it is the one that crosses
        // the 128 KiB boundary with a short block left over.
        //
        // 0.2%, which is where this was calibrated rather than where it was
        // first guessed. What it rejects: the same payload flushed every 5 KB
        // costs +10.8% at 200 KB and fails. What it accepts besides the
        // measurements above: flushing every 37 KB, worth at most +0.078%,
        // which is a legitimate write pattern and must not fail. The gate sits
        // between the two by nearly two orders of magnitude, so it is not
        // balanced on either.
        let streaming_slack = (upstream_bytes.len() / 500).max(256);
        assert!(
            streamed.len() <= upstream_bytes.len() + streaming_slack,
            "level 3 streaming at {size} bytes: {} vs upstream {} ({:+.3}%)",
            streamed.len(),
            upstream_bytes.len(),
            (streamed.len() as f64 - upstream_bytes.len() as f64) / upstream_bytes.len() as f64
                * 100.0
        );
    }
}

/// The same payload shape through a mid-stream flush, which is what a capture
/// writer does when a consumer needs to see data before the frame ends.
///
/// A flush closes a block early, so this is the case where our block sizes
/// stop matching anything upstream would choose on its own. Both what that
/// costs and whether the result is still readable, because a caller who
/// flushes on every record is paying this repeatedly and has no way to see it.
#[test]
fn level_three_numeric_payloads_survive_flushes_and_stay_readable_upstream() {
    let Some(helper) = helper_path() else {
        return;
    };

    let payload = numeric_array_payload(900_000);
    let mut encoder = StreamingEncoder::new(EncoderOptions {
        compression_level: CompressionLevel::try_new(3).unwrap(),
        checksum: true,
        ..Default::default()
    })
    .unwrap();

    // Flush at an interval that is not a divisor of the block size, so the
    // flushes land in every position relative to a boundary: inside the first
    // block, just short of one, just past one.
    //
    // Every chunk, not every third. Flush cost is paid per flush, so how
    // sharply this test reacts to a per-flush regression scales with how many
    // it performs. On this payload: 6 flushes cost 0.04%, 90 cost 2.60%, 450
    // cost 12.24%. At six the signal is a rounding error, and a defect that
    // doubled the price of a flush would have moved the total by 40 bytes.
    const FLUSH_INTERVAL: usize = 10_000;
    for chunk in payload.chunks(FLUSH_INTERVAL) {
        encoder.push(chunk).unwrap();
        encoder.flush().unwrap();
    }
    encoder.finish().unwrap();
    let framed = encoder.take_output();

    assert_eq!(run_helper(helper, "decompress-stream", &framed), payload);
    assert_eq!(decode_all(&framed).unwrap(), payload);

    // What the flushes cost, measured against the same payload streamed
    // without them rather than against upstream. Upstream is only reachable
    // here one-shot, so comparing to it would fold the price of flushing in
    // with any parser divergence and bound the sum, when the two move for
    // unrelated reasons and only one of them is this test's subject. The
    // unflushed comparison has no such confound: identical encoder, identical
    // pushes, one call different.
    let unflushed = {
        let mut encoder = StreamingEncoder::new(EncoderOptions {
            compression_level: CompressionLevel::try_new(3).unwrap(),
            checksum: true,
            ..Default::default()
        })
        .unwrap();
        for chunk in payload.chunks(FLUSH_INTERVAL) {
            encoder.push(chunk).unwrap();
        }
        encoder.finish().unwrap();
        encoder.take_output()
    };

    // 90 flushes over 900 KB of serialized f64 columns costs 2.60%. That is
    // the format's price, not a defect: a flush ends the block, and upstream
    // pays the same for the same request. What the bound is here to catch is
    // that price changing, which happens if a flush stops carrying match
    // history across the block it closes — a defect every assertion above
    // still passes, because the output stays perfectly readable.
    //
    // 3.5% against a measured 2.60%, so the margin is about a third. That is
    // deliberately narrow: the measurement is fully deterministic, and the
    // whole point is to notice movement. It is verified to reject rather than
    // merely to hold — flushing five times as often costs 12.24% and fails.
    // If a legitimate parser change moves this, re-measure and say so here
    // rather than widening it to whatever passes.
    let ceiling = unflushed.len() + unflushed.len() * 35 / 1000;
    assert!(
        framed.len() <= ceiling,
        "flushing every {FLUSH_INTERVAL} bytes cost {:+.3}%: {} bytes against {} unflushed",
        (framed.len() as f64 - unflushed.len() as f64) / unflushed.len() as f64 * 100.0,
        framed.len(),
        unflushed.len()
    );
}

/// Our streaming encoder must split blocks the way upstream's *streaming*
/// encoder does, which is not the way either encoder's one-shot path does.
///
/// Upstream runs its pre-split heuristic inside `ZSTD_compress_frameChunk`,
/// which declines outright once fewer than 128 KiB remain in the chunk it was
/// handed:
///
/// ```c
/// if (srcSize < 128 KB || blockSizeMax < 128 KB)
///     return MIN(srcSize, blockSizeMax);
/// ```
///
/// Its one-shot path hands that loop the whole input, so the tail stays above
/// the floor and a large input is split many times over; its buffered streaming
/// path hands over exactly one 128 KiB block at a time, so a chunk yields at
/// most two blocks. On `binary-structured` at 1 MiB that is 99 blocks against
/// 16, and 1.84% of compressed size.
///
/// Nothing else in the tree can catch this. Every other upstream comparison
/// here was one-shot on both sides, and the streaming property tests in
/// `tests/property.rs` gate our streaming output against our *own* one-shot
/// output, which is exactly the layout that was wrong.
///
/// Restricted to the levels whose strategy is not one of the optimal parsers.
/// `StreamingEncoder::next_block_size` deliberately takes a whole block at
/// those levels and so never splits, while upstream does; that is a separate,
/// pre-existing deviation, and on `tabular-csv` at level 19 it happens to be
/// worth 18.56% in our favour. Fold those levels in here and this test would be
/// asserting a behaviour we have not chosen.
#[test]
fn streaming_block_layout_matches_upstream_streaming() {
    let Some(upstream_helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    // Large enough that the frame spans several 128 KiB chunks -- with one
    // chunk there is no carried-forward tail to get wrong and the test would
    // pass either way.
    let input_size = 1024 * 1024;
    let piece = 32 * 1024;
    let cases = [
        "json-records",
        "log-lines",
        "mixed-entropy",
        "binary-structured",
    ];

    for case in benchmark_corpora::benchmark_report_cases(input_size) {
        if !cases.contains(&case.name) {
            continue;
        }
        for level in [3, 9, 15] {
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                ..Default::default()
            };
            let ours = stream_encode(&case.input, options, piece);
            let theirs = upstream_trace_helper::compress_streaming_once(
                upstream_helper,
                level,
                false,
                piece,
                &case.input,
            );

            assert_eq!(decode_all(&ours).unwrap(), case.input);
            assert_eq!(
                block_sizes(&ours).len(),
                block_sizes(&theirs).len(),
                "{} level {level}: block layout differs from upstream streaming.\n  ours   {:?}\n  theirs {:?}",
                case.name,
                block_sizes(&ours),
                block_sizes(&theirs),
            );

            // The layout assertion above is the sharp one. This bounds the cost
            // of the layout being right but the contents diverging. The worst
            // measured row across all eleven corpora at levels 1..=15 is
            // +0.03%; before the frame-chunk fix `binary-structured` sat at
            // +1.84% here.
            let delta = (ours.len() as f64 - theirs.len() as f64) / theirs.len() as f64;
            assert!(
                delta <= 0.001,
                "{} level {level}: emitted {} bytes against upstream streaming's {} ({:+.2}%)",
                case.name,
                ours.len(),
                theirs.len(),
                delta * 100.0,
            );
        }
    }
}

/// One-shot positions keep growing beyond 16 MiB, where a 24-bit entry wraps.
#[test]
fn fast_one_shot_stays_at_upstream_size_past_16_mib() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    let mut input = benchmark_corpora::build_json_records_pattern(8 * 1024 * 1024);
    input.extend(benchmark_corpora::build_tabular_csv_pattern(
        9 * 1024 * 1024,
    ));
    let mut sizes = Vec::new();
    for level in [1, 3] {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };
        let mut encoder = Encoder::new();
        encoder
            .encode_all_with_options(&input[..8 * 1024 * 1024], options)
            .unwrap();
        let ours = encoder.encode_all_with_options(&input, options).unwrap();
        let theirs = upstream_trace_helper::compress_advanced(
            helper,
            upstream_trace_helper::DICT_NONE,
            &upstream_settings_for(level, &[]),
            &input,
        );
        assert_eq!(decode_all(&ours).unwrap(), input);
        assert_eq!(
            upstream_trace_helper::decompress_once(helper, "decompress", &ours),
            input
        );
        eprintln!(
            "17 MiB level {level}: Rust {} bytes, upstream {} bytes",
            ours.len(),
            theirs.len()
        );
        sizes.push((level, ours.len(), theirs.len()));
    }
    for (level, ours, theirs) in sizes {
        assert!(
            ours <= theirs,
            "level {level}: Rust {ours} bytes, upstream {theirs} bytes"
        );
    }
}

/// A stream that pledges its size produces the frame upstream's streaming
/// encoder produces under the same pledge, byte for byte, down to a frame
/// whose content is exactly one block, and wherever the two one-shot encoders
/// already agree.
///
/// The single-block case is the one that had drifted. Our buffer filled on the
/// last pledged byte and the block went out unflagged, followed by an empty
/// last block from `finish`. Upstream sets its buffer target one byte past the
/// block size when the pledge equals it (`ZSTD_CCtx_init_compressStream2`,
/// `zstd_compress.c`), so its only block waits for `ZSTD_e_end` and carries
/// the flag itself. `compress_streaming_once` cannot observe this because it
/// never pledges; the advanced streaming mode can.
///
/// Three claims, checked separately so a failure names its side: our pledged
/// stream is our one-shot frame; upstream's pledged stream is upstream's
/// one-shot frame; and where the one-shot frames agree, so do the streams.
/// They do not agree on the 539-byte input at levels 9 and 19, where our
/// one-shot parser is one byte over upstream's, which is a parse tie of the
/// kind `KNOWN_UPSTREAM_SIZE_GAPS` records and nothing the stream adds.
///
/// Level 19 is left out above one block for the reason
/// `streaming_block_layout_matches_upstream_streaming` gives: the optimal
/// parsers split a block where upstream does not, a separate and chosen
/// deviation. A single block has nothing to split.
#[test]
fn a_pledged_stream_matches_upstream_streaming_under_the_same_pledge() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };
    const PIECE: usize = 32 * 1024;
    let single_block: &[usize] = &[1, 539, 4096, 36_827, 128 * 1024];
    let multi_block: &[usize] = &[128 * 1024 + 1, 300 * 1024];

    for (sizes, levels) in [
        (single_block, &[1, 3, 9, 19][..]),
        (multi_block, &[1, 3, 9][..]),
    ] {
        for &size in sizes {
            let input = build_pattern(size);
            for &level in levels {
                let options = EncoderOptions {
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    pledged_src_size: Some(size as u64),
                    ..Default::default()
                };
                let settings = [
                    format!("compressionLevel={level}"),
                    format!("pledgedSrcSize={size}"),
                ];
                let ours = stream_encode(&input, options, PIECE);
                let ours_one_shot = encode_all_with_options(&input, options).unwrap();
                let theirs = upstream_trace_helper::compress_advanced_streaming(
                    helper,
                    upstream_trace_helper::DICT_NONE,
                    PIECE,
                    &settings,
                    &input,
                );
                let theirs_one_shot = upstream_trace_helper::compress_advanced(
                    helper,
                    upstream_trace_helper::DICT_NONE,
                    &settings[..1],
                    &input,
                );

                assert_eq!(decode_all(&ours).unwrap(), input);
                assert_eq!(
                    ours, ours_one_shot,
                    "{size} bytes at level {level}: our pledged stream differs from our one-shot frame"
                );
                assert_eq!(
                    theirs, theirs_one_shot,
                    "{size} bytes at level {level}: upstream's pledged stream differs from its one-shot frame"
                );
                if ours_one_shot == theirs_one_shot {
                    assert_eq!(
                        ours, theirs,
                        "{size} bytes at level {level}: pledged streaming frame differs from upstream's"
                    );
                } else {
                    assert!(
                        size == 539 && (level == 9 || level == 19),
                        "{size} bytes at level {level}: one-shot frames differ from upstream's, {} against {} bytes",
                        ours_one_shot.len(),
                        theirs_one_shot.len(),
                    );
                }
            }
        }
    }
}

/// A pledged stream whose content is an exact multiple of the block size and
/// longer than one block still closes its frame with the epilogue's empty last
/// block -- and so does upstream, which is why the frames still match.
///
/// This is the boundary of what `push_target` fixes, and it is worth pinning
/// from both sides. Upstream's bump is `blockSizeMax + (blockSizeMax ==
/// pledgedSrcSize)`, an equality and not a divisibility test, so at two or
/// three whole blocks it does not fire: the last full block goes out from the
/// streaming loop unflagged and `ZSTD_writeEpilogue` spends three bytes on an
/// empty block to carry the flag. Widening our own bump to every multiple
/// would save those three bytes and would diverge from upstream on every
/// block-aligned frame, which is the trade this test refuses on the record.
///
/// The assertion is structural rather than a byte count: both streams end in a
/// last block that is raw and empty. The size gap that follows from it is three
/// bytes against each side's own one-shot frame, which is what the comment on
/// `StreamingEncoder::finish` describes.
#[test]
fn a_pledged_stream_of_whole_blocks_carries_the_epilogue_like_upstream() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };
    const PIECE: usize = 32 * 1024;

    for size in [2 * BLOCK_SIZE_MAX, 3 * BLOCK_SIZE_MAX] {
        let input = build_pattern(size);
        for level in [1, 3, 9] {
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                pledged_src_size: Some(size as u64),
                ..Default::default()
            };
            let settings = [
                format!("compressionLevel={level}"),
                format!("pledgedSrcSize={size}"),
            ];
            let ours = stream_encode(&input, options, PIECE);
            let theirs = upstream_trace_helper::compress_advanced_streaming(
                helper,
                upstream_trace_helper::DICT_NONE,
                PIECE,
                &settings,
                &input,
            );

            assert_eq!(decode_all(&ours).unwrap(), input);
            for (whose, frame) in [("ours", &ours), ("upstream's", &theirs)] {
                let epilogue = parse_block_header(&frame[frame.len() - BlockHeader::SIZE..])
                    .expect("a frame ends on a block header");
                assert!(
                    epilogue.last_block
                        && epilogue.block_type == BlockType::Raw
                        && epilogue.block_size == 0,
                    "{size} bytes at level {level}: {whose} stream did not end on the \
                     epilogue's empty last block, but on {epilogue:?}"
                );
            }
            assert_eq!(
                ours, theirs,
                "{size} bytes at level {level}: pledged streaming frame differs from upstream's"
            );
        }
    }
}

/// Streaming a frame long enough to compact its buffer several times must stay
/// at upstream's size, which is the case a shorter frame cannot cover: below
/// twice the window the encoder never compacts at all and this whole path is
/// dead.
///
/// What it rejects is the encoder clearing its match tables at each compaction
/// and re-filling them over the retained bytes, which is what it did until the
/// tables learned to rebase in place. That dense re-fill is not the table the
/// parser had built. Measured here against the pinned upstream with the rebuild
/// forced back on: `json-records` +7.96% and `tabular-csv` +1.37% at level 1,
/// against -2.68% and -0.00% as it stands. The bound is 0.25%, roughly a
/// thirtieth of the smaller failure and five times the larger margin, so it is
/// balanced on neither side.
///
/// Level 1 is what discriminates. Level 2 is here because its output is
/// byte-identical to upstream's on both corpora, and a level that agrees
/// exactly is worth keeping under a gate even though the rebuild also passed
/// it.
#[test]
fn streaming_stays_at_upstream_size_across_repeated_compactions() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    // Six MiB against a 512 KiB window at level 1: the buffer fills to twice
    // the window and compacts roughly ten times over the frame. Four MiB would
    // still compact, but only after level 2's larger window has already used
    // most of the input.
    let input_size = 6 * 1024 * 1024;
    let piece = 32 * 1024;
    let cases = ["json-records", "tabular-csv"];

    for case in benchmark_corpora::benchmark_report_cases(input_size) {
        if !cases.contains(&case.name) {
            continue;
        }
        for level in [1i32, 2] {
            let theirs = upstream_trace_helper::compress_streaming_once(
                helper,
                level,
                false,
                piece,
                &case.input,
            );
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                ..Default::default()
            };
            let ours = stream_encode(&case.input, options, piece);

            assert_eq!(
                decode_all(&ours).unwrap(),
                case.input,
                "{} level {level}",
                case.name
            );

            let slack = (theirs.len() / 400).max(256);
            assert!(
                ours.len() <= theirs.len() + slack,
                "{} level {level}: {} bytes vs upstream streaming {} ({:+.2}%)",
                case.name,
                ours.len(),
                theirs.len(),
                (ours.len() as f64 - theirs.len() as f64) / theirs.len() as f64 * 100.0
            );
        }
    }
}

/// `btultra2` parses the block that opens a frame twice, seeding its price
/// model from the first pass, and the streaming encoder has to ask for that
/// explicitly because it drives its own block loop. See
/// `seed_optimal_prices_from_first_block`.
///
/// What it rejects is that seeding pass going missing from the streaming path,
/// which is where it was missing until this test existed. Measured against the
/// pinned upstream with the call removed: `wikipedia` L21 +4.49% and
/// `tabular-csv` L19 -18.56%, against +0.01% for both as it stands. The bound
/// is two-sided at 0.1% because the failure runs both ways -- on `tabular-csv`
/// the unseeded parser emits *less* than upstream, and a one-sided gate would
/// read that as healthy.
///
/// Level 17 is the control: `btopt` does not seed, so it must be unaffected,
/// and it was already byte-identical before the fix.
#[test]
fn streaming_seeds_the_optimal_price_model_like_upstream() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    let piece = 32 * 1024;
    let cases = [("wikipedia", 21i32), ("tabular-csv", 19), ("wikipedia", 17)];

    for case in benchmark_corpora::benchmark_report_cases(4 * 1024 * 1024) {
        for &(name, level) in &cases {
            if case.name != name {
                continue;
            }
            let theirs = upstream_trace_helper::compress_streaming_once(
                helper,
                level,
                false,
                piece,
                &case.input,
            );
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                ..Default::default()
            };
            let ours = stream_encode(&case.input, options, piece);

            assert_eq!(
                decode_all(&ours).unwrap(),
                case.input,
                "{name} level {level}"
            );

            let delta = (ours.len() as f64 - theirs.len() as f64) / theirs.len() as f64;
            assert!(
                delta.abs() <= 0.001,
                "{name} level {level}: {} bytes against upstream streaming {} ({:+.2}%)",
                ours.len(),
                theirs.len(),
                delta * 100.0
            );
        }
    }
}

/// Structured records that share a skeleton without repeating verbatim, the
/// shape a dictionary is actually trained for.
fn training_samples(count: usize) -> Vec<Vec<u8>> {
    let hosts = ["alpha", "beta", "gamma-node", "delta", "epsilon-7"];
    let statuses = ["open", "closed", "pending", "escalated"];
    (0..count)
        .map(|index| {
            let mut record = Vec::new();
            for line in 0..10 {
                let seed = index * 31 + line * 7;
                record.extend_from_slice(
                    format!(
                        "{{\"id\":{},\"host\":\"{}\",\"status\":\"{}\",\
                         \"latency_ms\":{},\"path\":\"/v2/tenants/{}/objects\"}}\n",
                        1_000_000 + seed * 991,
                        hosts[seed % hosts.len()],
                        statuses[(seed / 3) % statuses.len()],
                        (seed * 7919) % 4096,
                        seed % 97,
                    )
                    .as_bytes(),
                );
            }
            record
        })
        .collect()
}

/// The content-selection half of training must be byte-identical to upstream.
///
/// This is the fastCover algorithm proper: hashing, epochs, segment scoring and
/// placement. It is pure integer work over the sample bytes with no dependence
/// on our encoder, so anything less than an exact match is a porting error.
///
/// Both sides are pinned to a single `(k, d)`. Left to search, upstream tries
/// several segment sizes and keeps whichever compressed the held-out samples
/// smallest — a decision made by *its* compressor and by ours respectively, so
/// the two can settle on different candidates and select different content
/// while both are behaving correctly. Pinning removes that choice and leaves
/// only the algorithm under test. Whether the search then lands somewhere as
/// good is what `trained_dictionary_is_as_effective_as_upstream` measures.
#[test]
fn trained_dictionary_content_matches_upstream() {
    // The support helper, not this file's: they are separate binaries built
    // from separate C sources, and `train-dict` is a mode only the support one
    // carries.
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    for (count, capacity, k, d) in [
        (64usize, 512usize, 50u32, 8u32),
        (64, 4096, 200, 8),
        (64, 4096, 1024, 6),
        (120, 8192, 537, 8),
    ] {
        let owned = training_samples(count);
        let samples: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();

        let theirs =
            upstream_trace_helper::train_dictionary_fixed(helper, capacity, &samples, k, d);
        let ours = zstandard::train_dictionary_with_parameters(
            &samples,
            capacity,
            zstandard::DictionaryTrainingParameters {
                k,
                d,
                steps: 1,
                ..Default::default()
            },
        )
        .unwrap()
        .into_bytes();

        // Locate the content rather than assuming where it starts. Both sides
        // fill the capacity, but the content is a *prefix* of the selected
        // material trimmed to whatever the entropy header left over, so headers
        // of different sizes leave different-length content and the tails stop
        // lining up. Comparing a fixed-size tail would then fail and blame
        // content selection for an entropy-header difference the module docs
        // expressly permit.
        let our_header = upstream_trace_helper::dictionary_header_size(helper, &ours);
        let their_header = upstream_trace_helper::dictionary_header_size(helper, &theirs);
        let our_content = &ours[our_header..];
        let their_content = &theirs[their_header..];
        let shared = our_content.len().min(their_content.len());
        assert!(
            shared > capacity / 2,
            "not enough content to compare at k={k} d={d} capacity={capacity}: \
             {} and {} bytes",
            our_content.len(),
            their_content.len()
        );
        assert_eq!(
            &our_content[..shared],
            &their_content[..shared],
            "selected content differs at {count} samples / {capacity} bytes, k={k} d={d} \
             (headers {our_header} and {their_header} bytes)"
        );
    }
}

/// Compare the histograms training accumulates, rather than the tables built
/// from them.
///
/// A finished entropy table is a lossy function of its histogram, so comparing
/// dictionaries cannot say whether a divergence came from the statistics or
/// from the table construction. Comparing the counts can, and it is how the
/// parameter-derivation bug behind this test was found: training was deriving
/// its compression parameters the way compression *with* a dictionary does,
/// which shrinks the hash and chain tables, so it trained on a parse upstream
/// never runs.
///
/// The totals are asserted rather than the individual bins. Our parser and
/// upstream's still disagree over which matches to take in the raw-dictionary
/// path, so the counts land in slightly different bins; what must hold is that
/// they are describing the same amount of work. A bin-exact assertion here
/// would be an encoder-parity test wearing a dictionary-training costume.
#[test]
fn dictionary_training_statistics_match_upstream() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    let owned = training_samples(64);
    let samples: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    // Any content will do here; what is under test is the statistics gathered
    // against it, not how it was chosen.
    let content = &owned[0][..400];

    let (their_lit, their_off, their_ml, their_ll) =
        upstream_trace_helper::dictionary_entropy_stats(helper, 3, content, &samples);
    let (our_lit, our_off, our_ml, our_ll) =
        zstandard::trace_dictionary_entropy_stats(content, &samples, CompressionLevel::DEFAULT)
            .unwrap();

    let total = |counts: &[u32]| -> f64 { counts.iter().map(|&c| f64::from(c)).sum() };
    for (name, ours, theirs, tolerance) in [
        ("literal bytes", &our_lit, &their_lit, 0.01),
        ("offset codes", &our_off, &their_off, 0.02),
        ("match lengths", &our_ml, &their_ml, 0.02),
        ("literal lengths", &our_ll, &their_ll, 0.02),
    ] {
        let (ours, theirs) = (total(ours), total(theirs));
        let delta = (ours - theirs) / theirs;
        assert!(
            delta.abs() <= tolerance,
            "{name} total differs by {:+.2}%: {ours} against upstream {theirs}",
            delta * 100.0
        );
    }
}

/// How much a dictionary we trained gives up against one upstream trained from
/// the same samples.
///
/// Both dictionaries are measured with *our* encoder, so the comparison is of
/// the dictionaries and nothing else. Compressing with each side's own encoder
/// would fold the encoder gap into the answer and stop measuring training.
#[test]
fn trained_dictionary_is_as_effective_as_upstream() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    for (count, capacity) in [(64usize, 512usize), (64, 4096), (120, 8192)] {
        let owned = training_samples(count);
        let samples: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();

        let theirs = upstream_trace_helper::train_dictionary(helper, capacity, &samples);
        let ours = zstandard::train_dictionary(&samples, capacity).unwrap();

        // Held-out records, one at a time: the case a dictionary exists for.
        let held_out = training_samples(count + 40);
        let held_out = &held_out[count..];
        let mut with_ours = 0usize;
        let mut with_theirs = 0usize;
        for (dictionary, total) in [(&ours, &mut with_ours), (&theirs, &mut with_theirs)] {
            let prepared = EncoderDictionary::new(dictionary).unwrap();
            for sample in held_out {
                for record in sample.split(|&b| b == b'\n').filter(|r| !r.is_empty()) {
                    *total += encode_all_with_prepared_dict(record, &prepared)
                        .unwrap()
                        .len();
                }
            }
        }

        let delta = (with_ours as f64 - with_theirs as f64) / with_theirs as f64;
        eprintln!(
            "dict-effectiveness {count} samples / {capacity} bytes: \
             ours {with_ours} vs upstream {with_theirs} ({:+.3}%)",
            delta * 100.0
        );
        assert!(
            delta <= 0.01,
            "our trained dictionary is {:+.2}% worse than upstream's at \
             {count} samples / {capacity} bytes",
            delta * 100.0
        );
    }
}

/// Negative levels are byte-identical to upstream's, bar one recorded gap.
///
/// They all share one row of the level table and differ only in the
/// acceleration factor written over its `target_length`, so a single wrong
/// value in either place moves every one of these rows at once. Byte equality
/// is what distinguishes "we implemented the acceleration" from "we picked a
/// fast parser and got a plausible size".
///
/// It is also what caught the behaviour that makes these levels what they are:
/// upstream's `ZSTD_literalsCompressionIsDisabled` turns Huffman coding of the
/// literals section off for exactly this configuration (`ZSTD_fast` with a
/// non-zero `targetLength`), and nothing about the parse hints at it. Without
/// that, every row here came out around 0.64x of upstream's size, which reads
/// like a win rather than like a defect.
#[test]
fn negative_levels_are_byte_identical_to_upstream() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    // Spans the range: the shallow end where acceleration barely bites, a
    // middle that skips several bytes per attempt, and the floor.
    let levels = [
        -1,
        -2,
        -3,
        -5,
        -7,
        -10,
        -50,
        -100,
        -1000,
        CompressionLevel::MIN.as_i32(),
    ];
    // Above the 256 KiB tier boundary, so the row-0 window log is the one that
    // survives adjustment rather than being clamped to the source.
    let input_size = 1024 * 1024;

    // Rows where a block still encodes differently from upstream, as
    // `(case, level)`. The divergence is one block's payload in an otherwise
    // identical frame: the block count, every other block's size, and the
    // parse are all upstream's, and ours is the smaller of the two.
    //
    // A sweep of every corpus against levels -40..=-1 plus -50, -100, -1000
    // and the floor put 480 of 484 rows byte-identical. The four that were not
    // are `wikipedia` at -10 (4 bytes), -27 (83) and -34 (2), and
    // `raw-dictionary` at -2 (1). Only the first and last fall in the level
    // set below.
    //
    // It is not a tie that happens to land the wrong way: on `wikipedia` at
    // -10 the gap is absent at 512 KiB and 640 KiB and present from 768 KiB
    // up, and at 1.5 MiB two further levels join it. That is a real difference
    // in a per-block encoding decision under acceleration, and it is
    // unexplained. Recorded rather than asserted away, because the size bound
    // below still fails if any of it ever turns into a regression.
    let known_gaps: &[(&str, i32)] = &[("wikipedia", -10), ("raw-dictionary", -2)];

    for case in benchmark_corpora::benchmark_report_cases(input_size) {
        for level in levels {
            for checksum in [false, true] {
                let theirs = upstream_trace_helper::compress_once(
                    helper,
                    "compress-regular-configured",
                    level,
                    checksum,
                    &case.input,
                );
                let ours = encode_all_with_options(
                    &case.input,
                    EncoderOptions {
                        checksum,
                        compression_level: CompressionLevel::try_new(level).unwrap(),
                        ..Default::default()
                    },
                )
                .unwrap();
                assert_eq!(decode_all(&ours).unwrap(), case.input, "{}", case.name);

                if known_gaps.contains(&(case.name, level)) {
                    assert!(
                        ours.len() <= theirs.len(),
                        "{} level {level} is a recorded gap, but ours grew to {} bytes \
                         against upstream's {}",
                        case.name,
                        ours.len(),
                        theirs.len()
                    );
                    continue;
                }

                assert_eq!(
                    ours.len(),
                    theirs.len(),
                    "{} level {level} checksum {checksum}: {} bytes vs upstream {}",
                    case.name,
                    ours.len(),
                    theirs.len()
                );
                assert_eq!(
                    ours, theirs,
                    "{} level {level} checksum {checksum}",
                    case.name
                );
            }
        }
    }
}

/// Level `0` means the default level, not "no compression" and not a negative
/// level. Upstream maps it to `ZSTD_CLEVEL_DEFAULT` before anything else looks
/// at it, and `0` is the value a caller reaches by accident most easily.
#[test]
fn level_zero_is_the_default_level() {
    let zero = CompressionLevel::try_new(0).unwrap();
    assert_eq!(zero.as_i32(), 0, "the level keeps the value it was given");

    let input_size = 256 * 1024;
    for case in benchmark_corpora::benchmark_report_cases(input_size) {
        let at_zero = encode_all_with_options(
            &case.input,
            EncoderOptions {
                compression_level: zero,
                ..Default::default()
            },
        )
        .unwrap();
        let at_default = encode_all_with_options(
            &case.input,
            EncoderOptions {
                compression_level: CompressionLevel::DEFAULT,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(at_zero, at_default, "{}", case.name);
    }
}

/// Negative levels select the compression-parameter tier by source size, the
/// same as positive ones.
///
/// Row 0 differs per tier, so a negative level that ignored the tier would
/// still produce valid output and would still look accelerated. What separates
/// the rows is `min_match` and `hash_log`, *not* the window: at these sizes
/// `ZSTD_adjustCParams_internal` clamps every tier's window down to the source
/// anyway, so a corpus whose parse is insensitive to `min_match` cannot tell
/// the tiers apart at all. An earlier version of this test used a periodic
/// `(i * 7) % 251` ramp and would have passed unchanged had tier selection been
/// hardcoded to 0; a text-like corpus separates them by several percent.
#[test]
fn negative_levels_select_the_source_size_tier() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    // Both sides of every tier boundary, which `upstream_cparams_tier` puts at
    // 16 KiB, 128 KiB and 256 KiB with each bound inclusive, plus one size well
    // inside the top tier.
    let sizes = [
        8 * 1024,
        16 * 1024,
        16 * 1024 + 1,
        128 * 1024,
        128 * 1024 + 1,
        256 * 1024,
        256 * 1024 + 1,
        1024 * 1024,
    ];

    for size in sizes {
        // A generated corpus rather than a ramp: this one has a literal
        // alphabet wide enough that `min_match` changes the parse.
        let case = benchmark_corpora::benchmark_report_cases(size)
            .into_iter()
            .find(|case| case.name == "log-lines")
            .expect("log-lines is a benchmark corpus");
        for level in [-1, -5, -20] {
            let theirs = upstream_trace_helper::compress_once(
                helper,
                "compress-regular-configured",
                level,
                false,
                &case.input,
            );
            let ours = encode_all_with_options(
                &case.input,
                EncoderOptions {
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(ours, theirs, "{} bytes at level {level}", case.input.len());
        }
    }
}

/// A dictionary at a negative level routes through the CDict parameter path
/// with row 0, which is a different derivation from the dictionary-free one.
#[test]
fn negative_levels_with_a_dictionary_match_upstream() {
    let Some(helper) = helper_path() else {
        return;
    };

    let dictionary = run_helper(helper, "emit-raw-dict", &[]);

    for size in [24_000, 180_000] {
        let input = build_raw_dictionary_input(size);
        for level in [-1, -3, -9] {
            let theirs = run_configured_compress(
                helper,
                "compress-raw-dict-configured",
                level,
                false,
                &input,
            );
            let ours = encode_all_with_dict_and_options(
                &input,
                &dictionary,
                EncoderOptions {
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                ours, theirs,
                "raw dictionary at level {level}, {size} bytes"
            );
            assert_eq!(
                decode_all_with_dict(&ours, &dictionary).unwrap(),
                input,
                "level {level}"
            );
        }
    }
}

/// One compression-parameter override, expressed both ways: as the
/// [`ParameterOverrides`] this crate takes and as the `ZSTD_c_*` setting that
/// asks upstream for the same thing.
struct OverrideCase {
    label: String,
    overrides: ParameterOverrides,
    settings: Vec<String>,
}

impl OverrideCase {
    fn parameter(&self) -> &str {
        self.label.split('=').next().unwrap()
    }
}

/// Every parameter, at values no level would pick on its own.
///
/// Values matter here. Overriding `search_log` to whatever the level already
/// chose would produce a test that passes against any implementation that
/// applies overrides anywhere at all — or that ignores them entirely — because
/// the value is already the table's. Each of these is off the table for at
/// least some of the levels swept.
///
/// Two absences are deliberate. `target_length: Some(0)` has no upstream
/// equivalent to compare against: `ZSTD_overrideCParams` reads `0` as "unset"
/// (`zstd_compress.c:1633`). And every `window_log` here is at least as wide
/// as the corpus, for the reason given on
/// [`overriding_the_window_below_the_frame_loses_history`].
fn parameter_override_cases() -> Vec<OverrideCase> {
    fn case(label: String, overrides: ParameterOverrides, setting: String) -> OverrideCase {
        OverrideCase {
            label,
            overrides,
            settings: vec![setting],
        }
    }

    let mut cases = Vec::new();
    for value in [20u32, 23] {
        cases.push(case(
            format!("window_log={value}"),
            ParameterOverrides {
                window_log: Some(value),
                ..Default::default()
            },
            format!("windowLog={value}"),
        ));
    }
    for value in [12u32, 20] {
        cases.push(case(
            format!("hash_log={value}"),
            ParameterOverrides {
                hash_log: Some(value),
                ..Default::default()
            },
            format!("hashLog={value}"),
        ));
        cases.push(case(
            format!("chain_log={value}"),
            ParameterOverrides {
                chain_log: Some(value),
                ..Default::default()
            },
            format!("chainLog={value}"),
        ));
    }
    for value in [2u32, 6] {
        cases.push(case(
            format!("search_log={value}"),
            ParameterOverrides {
                search_log: Some(value),
                ..Default::default()
            },
            format!("searchLog={value}"),
        ));
    }
    for value in [3u32, 4, 5, 6, 7] {
        cases.push(case(
            format!("min_match={value}"),
            ParameterOverrides {
                min_match: Some(value),
                ..Default::default()
            },
            format!("minMatch={value}"),
        ));
    }
    for value in [8u32, 64, 999] {
        cases.push(case(
            format!("target_length={value}"),
            ParameterOverrides {
                target_length: Some(value),
                ..Default::default()
            },
            format!("targetLength={value}"),
        ));
    }
    for strategy in [
        Strategy::Fast,
        Strategy::DoubleFast,
        Strategy::Greedy,
        Strategy::Lazy,
        Strategy::Lazy2,
        Strategy::BinaryTreeLazy2,
        Strategy::BinaryTreeOpt,
        Strategy::BinaryTreeUltra,
        Strategy::BinaryTreeUltra2,
    ] {
        cases.push(case(
            format!("strategy={strategy:?}"),
            ParameterOverrides {
                strategy: Some(strategy),
                ..Default::default()
            },
            format!("strategy={}", strategy.as_u32()),
        ));
    }
    for (mode, switch) in [
        (RowMatchFinderMode::Auto, 0u32),
        (RowMatchFinderMode::Enabled, 1),
        (RowMatchFinderMode::Disabled, 2),
    ] {
        cases.push(case(
            format!("use_row_match_finder={mode:?}"),
            ParameterOverrides {
                use_row_match_finder: mode,
                ..Default::default()
            },
            format!("useRowMatchFinder={switch}"),
        ));
    }
    // Both arms that differ from `auto` are reached by the sweep's own levels,
    // so neither needs a forced strategy to go with it. `Enabled` only changes
    // a frame where `auto` would have switched coding off, which is the
    // accelerated `Fast` that level -3 resolves to; `Disabled` only reaches the
    // optimal price model under a strategy that prices anything, which is what
    // level 19 selects. Measured, not assumed: `Enabled` moves eight of the
    // eleven corpora at -3 and `Disabled` seven of them at 19.
    for (mode, switch) in [
        (LiteralCompressionMode::Auto, 0u32),
        (LiteralCompressionMode::Enabled, 1),
        (LiteralCompressionMode::Disabled, 2),
    ] {
        cases.push(case(
            format!("literal_compression={mode:?}"),
            ParameterOverrides {
                literal_compression: mode,
                ..Default::default()
            },
            format!("literalCompressionMode={switch}"),
        ));
    }
    // A few combinations, because parameters interact through adjustment: a
    // window override changes what `hash_log` and `chain_log` are clamped to,
    // and a strategy override changes how `chain_log` is read.
    cases.push(OverrideCase {
        label: "combined=deep search in a narrow window".to_string(),
        overrides: ParameterOverrides {
            window_log: Some(20),
            search_log: Some(6),
            hash_log: Some(18),
            ..Default::default()
        },
        settings: vec![
            "windowLog=20".to_string(),
            "searchLog=6".to_string(),
            "hashLog=18".to_string(),
        ],
    });
    cases.push(OverrideCase {
        label: "combined=every parameter but strategy".to_string(),
        overrides: ParameterOverrides {
            window_log: Some(21),
            hash_log: Some(19),
            chain_log: Some(18),
            search_log: Some(4),
            min_match: Some(4),
            target_length: Some(32),
            strategy: None,
            ..Default::default()
        },
        settings: vec![
            "windowLog=21".to_string(),
            "hashLog=19".to_string(),
            "chainLog=18".to_string(),
            "searchLog=4".to_string(),
            "minMatch=4".to_string(),
            "targetLength=32".to_string(),
        ],
    });
    cases
}

fn upstream_settings_for(level: i32, extra: &[String]) -> Vec<String> {
    let mut settings = vec![format!("compressionLevel={level}")];
    settings.extend_from_slice(extra);
    settings
}

/// Levels spanning every parser family, so an override lands on a table row
/// that already disagrees with it: fast, double-fast, greedy with the row match
/// finder, lazy2, and optimal. The negative level is here because it reaches
/// its parameters by a different route — row 0 plus an acceleration factor
/// written over `target_length` — and an override has to survive that.
const OVERRIDE_SWEEP_LEVELS: [i32; 5] = [-3, 1, 5, 12, 19];
const OVERRIDE_SWEEP_SIZE: usize = 128 * 1024;

/// `(corpus, with a dictionary, level, parameter)` rows where an override
/// still parses differently from upstream.
///
/// 849 swept rows are upstream's exact bytes; these hold the rest, and the
/// counts are pinned at the end of the sweep so they cannot drift silently.
/// Each gap is recorded by parameter rather than by value, because the values
/// within a parameter that diverge are not a stable set — the `strategy` rows,
/// which are the bulk of them, are excluded wholesale rather than listed.
///
/// What is *absent* is as informative as what is here: `window_log` and
/// `chain_log` are exact on every row, and `target_length` on all but one. The
/// residue is concentrated in `min_match`, and most of these rows differ by
/// six bytes or fewer with several differing by none at all — parse ties
/// rather than a systematic difference.
///
/// The one family that is not a tie is `min_match: Some(7)` against a
/// dictionary, where this crate comes out 170 to 222 bytes *smaller*.
/// [`a_min_match_of_seven_is_only_upstreams_cliff`] pins the shape of it, which
/// is sharper than the sizes suggest: upstream's 6 and 7 are byte identical
/// with no dictionary loaded and diverge by 149 to 196 bytes with one, and 7 is
/// the only value that does this — its 3, 4, 5 and 6 are all the same frame
/// once a dictionary is present. This crate's 7 costs it nothing either way.
///
/// Why 7 is upstream's cliff is not settled here. See that test for the reading
/// that fits half the levels and the measurement that rules it out for the
/// other half.
///
/// Rows here still have to round-trip and still have to come out no larger
/// than upstream by more than a small margin, so a regression is caught.
/// Rows where an override moves upstream's frame and leaves ours unchanged, or
/// the reverse.
///
/// Three rows stood here when the sweep stopped skipping rows whose baseline
/// already differed. Two were `search_log=2` on the raw dictionary, and their
/// cause was a floor: `compression_parameters_for_input` used to raise
/// `search_depth` to at least 8 whenever the dictionary was raw content, so an
/// override asking for `1 << 2` compares silently got 8 and the parse could not
/// move. C floors nothing — `ZSTD_insertBt1` and the lazy searches take
/// `1U << cParams->searchLog` as given, and raw content reaches
/// `ZSTD_loadDictionaryContent` by the same route a full dictionary does. With
/// the floor gone both rows engage and land within three bytes of upstream.
///
/// The note that used to stand here read the gap as an applied-cparams
/// difference on the dictionary path. That was wrong: the applied parameters
/// were identical on both sides, override included, which is what sent the
/// search past them and into the match finder.
///
/// The one row left is not the same shape. Both sides apply `min_match:
/// Some(7)` — the parameter is not being dropped — and upstream's frame moves
/// only because 7 is a cliff for it once a dictionary is loaded, while ours is
/// flat. It is a divergence in this crate's favour, and it is left alone.
/// `a_min_match_of_seven_is_only_upstreams_cliff` holds the measurements and
/// fails if either side stops behaving this way.
///
/// The round trip still runs on this row, and so does the size bound below.
const OVERRIDE_ENGAGEMENT_GAPS: &[(&str, bool, i32, &str)] =
    &[("raw-dictionary", true, 12, "min_match=7")];

/// Differential rows that exceed upstream's size by more than the margin.
///
/// Empty, and the assertion below is what keeps it that way. It held one row
/// -- `search_log=2` on the raw dictionary at level 19, 2490 bytes against
/// upstream's 1922 and the largest known override gap in the tree -- which was
/// the `search_depth` floor described on [`OVERRIDE_ENGAGEMENT_GAPS`] seen from
/// the other side: our frame stayed at its unoverridden size because the
/// override never reached the search.
///
/// Sizes are recorded alongside each row, so closing a gap fails here and
/// forces the entry out, and so does a gap widening while staying a gap.
///
/// The one row here is the crate's plain, non-row lazy family measured against
/// C's. Nothing could reach it before `use_row_match_finder` existed: `auto`
/// selects the row finder for every greedy/lazy row this sweep runs, because it
/// turns it on whenever the window exceeds `1 << 14` and no case here asks for
/// a window that small, so the parsers behind `ParserStrategy::Greedy`, `Lazy`
/// and `Lazy2` had never been compared with upstream at all.
///
/// Opening that door found a defect worth 2x, since fixed: the chain walk broke
/// off at a 64-byte match, which is the opt parsers' `sufficient_len` and has no
/// counterpart in `ZSTD_HcFindBestMatch`. Forced `Lazy` and `Lazy2` on 256 KiB
/// of `raw-dictionary` ran 1.99x upstream and now land on its bytes exactly;
/// forced `Greedy` ran 1.67x and now comes in 29% under. What is left is this
/// row, which is `Greedy` at 1.8% over on one corpus, and the three in
/// [`OVERRIDE_PARSE_GAPS`] below, which are all in this crate's favour.
const OVERRIDE_DIFFERENTIAL_SIZE_GAPS: &[(&str, bool, i32, &str, usize, usize)] = &[(
    "log-lines",
    false,
    5,
    "use_row_match_finder=Disabled",
    23692,
    23268,
)];

const OVERRIDE_PARSE_GAPS: &[(&str, bool, i32, &str)] = &[
    ("json-records", false, 1, "min_match"),
    ("log-lines", false, 1, "hash_log"),
    ("log-lines", false, 5, "hash_log"),
    ("mixed-entropy", false, 19, "min_match"),
    // Same two-byte divergence as the row above: this combination names
    // `min_match: Some(4)` among its six settings.
    ("mixed-entropy", false, 19, "combined"),
    ("wikipedia", false, 1, "hash_log"),
    // Same divergence as the row above: this combination names
    // `hash_log: Some(18)`, and `hash_log` alone diverges here too.
    ("wikipedia", false, 1, "combined"),
    // The row-lazy repcode substitution, in the one place in this sweep it is
    // visible: `search_log: Some(2)` at level 5 selects `GreedyRow`, and coding
    // a live distance as a repcode brings the frame in 16 bytes *under*
    // upstream's 41105. A divergence in this crate's favour, recorded here
    // because this sweep's default is still byte parity.
    ("mixed-entropy", false, 5, "search_log"),
    ("wikipedia", false, 5, "search_log"),
    ("wikipedia", false, 5, "min_match"),
    ("wikipedia", false, 19, "search_log"),
    ("tabular-csv", false, -3, "min_match"),
    ("tabular-csv", false, 1, "min_match"),
    ("raw-dictionary", false, 12, "search_log"),
    ("raw-dictionary", false, 19, "target_length"),
    ("trained-dictionary", false, 1, "hash_log"),
    ("wikipedia", false, 19, "combined"),
    ("raw-dictionary", false, 19, "combined"),
    ("trained-dictionary", false, 1, "combined"),
    ("raw-dictionary", true, 5, "min_match"),
    ("trained-dictionary", true, 19, "combined"),
    ("trained-dictionary", true, 5, "min_match"),
    ("trained-dictionary", true, 19, "min_match"),
    // `use_row_match_finder` reaches two parsers nothing else in this sweep
    // does, and both diverge slightly.
    //
    // `Disabled` reaches the plain greedy/lazy family. `auto` turns the row
    // finder on for every greedy/lazy row here -- it needs only a window above
    // `1 << 14`, and the smallest this sweep asks for is 20 -- so those parsers
    // had never been compared with upstream at all.
    //
    // All three are now in this crate's favour: `binary-structured` by 1319
    // bytes on 18971, `mixed-entropy` by 8 on 41114, `small-alphabet` by 4 on
    // 34. They survived the fix to the chain walk's bogus early exit --
    // `binary-structured` by the same 1319 it diverged by before it -- because
    // what that restored is the *deeper* part of the walk, and at level 5 these
    // rows carry a search depth of 8 that rarely reaches it.
    ("small-alphabet", false, 5, "use_row_match_finder"),
    ("mixed-entropy", false, 5, "use_row_match_finder"),
    ("binary-structured", false, 5, "use_row_match_finder"),
    // `Enabled` reaches the *prefixed* row parsers, for the same reason from
    // the other side: these two dictionaries resolve to a CDict window of 14 or
    // less, so `auto` leaves the row finder off at every level and forcing it
    // on is the only way in. Nine bytes under upstream on the raw dictionary
    // and five over on the trained one.
    ("raw-dictionary", true, 5, "use_row_match_finder"),
    ("trained-dictionary", true, 5, "use_row_match_finder"),
    // One borderline post-sequence split, at the same total size.
    //
    // Uncoded literals cost the same whichever side of a split they fall on, so
    // the literal term cancels out of `left + right < whole` and the partition
    // is decided by the sequence estimate alone. That estimate carries a little
    // error, and with the literal term gone there is nothing left to swamp it.
    //
    // Bisected rather than assumed: the same corpus at this level and mode is
    // byte-identical at 12, 24, 32, 48 and 64 KiB, where nothing splits, and
    // again at 96 KiB, where both sides split into the same three blocks. Only
    // the full 128 KiB block splits four ways with one boundary a few sequences
    // off upstream's, and it costs nothing -- 4597 bytes either way.
    ("wikipedia", false, 19, "literal_compression"),
];

/// Overriding a compression parameter produces upstream's bytes for the same
/// `ZSTD_c_*` setting.
///
/// `strategy` is swept but not asserted byte-for-byte; see
/// [`overriding_the_strategy_leaves_the_levels_parameter_space`]. Everything
/// else is, except the rows in [`OVERRIDE_PARSE_GAPS`].
///
/// The baseline is measured rather than assumed. Six of this crate's corpora
/// already differ from upstream at level 12 with no override at all, and one
/// dictionary corpus differs at three levels; asserting override parity on
/// those pairs would fail for a reason that has nothing to do with overrides.
/// They are skipped here and belong to whatever eventually closes them.
#[test]
fn parameter_overrides_are_byte_identical_to_upstream() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
    let trained_dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
    let raw_prepared = EncoderDictionary::new(&raw_dictionary).unwrap();
    let trained_prepared = EncoderDictionary::new(&trained_dictionary).unwrap();
    let cases = parameter_override_cases();
    let mut asserted = 0usize;
    let mut recorded_rows = 0usize;
    let mut differential_rows = 0usize;
    let mut engagement_gaps = Vec::new();
    let mut size_gaps = Vec::new();
    let mut mismatches: Vec<String> = Vec::new();

    for use_dictionary in [false, true] {
        for corpus in benchmark_corpora::benchmark_report_cases(OVERRIDE_SWEEP_SIZE) {
            let dictionary = match (use_dictionary, corpus.dict_kind) {
                (true, benchmark_corpora::DictKind::Raw) => Some((
                    upstream_trace_helper::DICT_RAW,
                    &raw_prepared,
                    &raw_dictionary,
                )),
                (true, benchmark_corpora::DictKind::Trained) => Some((
                    upstream_trace_helper::DICT_TRAINED,
                    &trained_prepared,
                    &trained_dictionary,
                )),
                (true, benchmark_corpora::DictKind::None) => continue,
                (false, _) => None,
            };
            let dict_mode = dictionary.map_or(upstream_trace_helper::DICT_NONE, |(mode, ..)| mode);
            let encode = |options: EncoderOptions| match dictionary {
                Some((_, prepared, _)) => {
                    encode_all_with_prepared_dict_and_options(&corpus.input, prepared, options)
                        .unwrap()
                }
                None => encode_all_with_options(&corpus.input, options).unwrap(),
            };

            for level in OVERRIDE_SWEEP_LEVELS {
                let plain = EncoderOptions {
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    ..Default::default()
                };
                let our_plain = encode(plain);
                let their_plain = upstream_trace_helper::compress_advanced(
                    helper,
                    dict_mode,
                    &upstream_settings_for(level, &[]),
                    &corpus.input,
                );
                let baseline_matches = our_plain == their_plain;

                for case in &cases {
                    let options = EncoderOptions {
                        parameters: case.overrides,
                        ..plain
                    };
                    let ours = encode(options);
                    let theirs = upstream_trace_helper::compress_advanced(
                        helper,
                        dict_mode,
                        &upstream_settings_for(level, &case.settings),
                        &corpus.input,
                    );

                    let restored = match dictionary {
                        Some((_, _, bytes)) => decode_all_with_dict(&ours, bytes).unwrap(),
                        None => decode_all(&ours).unwrap(),
                    };
                    assert_eq!(
                        restored, corpus.input,
                        "{} L{level} {} did not round-trip",
                        corpus.name, case.label
                    );

                    // Checked before the baseline, because a `strategy` row is
                    // recorded on purpose whether or not its baseline agrees: a
                    // forced strategy at a fixed level builds configurations no
                    // level ever selects, and asserting anything about how
                    // upstream *responds* to one is asserting about a
                    // configuration neither side ships.
                    let recorded = case.parameter() == "strategy"
                        || OVERRIDE_PARSE_GAPS.contains(&(
                            corpus.name,
                            use_dictionary,
                            level,
                            case.parameter(),
                        ));

                    if !recorded && !baseline_matches {
                        // This pair already differs from upstream with no
                        // override at all, so its *bytes* say nothing about the
                        // override. They used to be skipped outright, which is
                        // how a sweep bleeds coverage as this crate diverges on
                        // purpose: the row-lazy repcode substitution alone took
                        // 101 rows out of comparison here, and a vanishing row
                        // reads exactly like a passing one.
                        //
                        // Compare each side against itself instead, which needs
                        // no baseline agreement. See item 2 of
                        // `docs/ORACLE_PLAN.md`.
                        //
                        // Unless the two sides land on the same bytes, which is
                        // the thing engagement is a proxy for and beats it: an
                        // override that produced upstream's exact output is
                        // wired to something whether or not it had anything to
                        // change. `literal_compression=Disabled` does this on
                        // the smallest corpus, where our default already emits
                        // raw literals and upstream's default does not, so
                        // switching coding off is inert for us, moves upstream,
                        // and leaves both on identical frames.
                        if ours != theirs && (ours != our_plain) != (theirs != their_plain) {
                            engagement_gaps.push((
                                corpus.name,
                                use_dictionary,
                                level,
                                case.label.clone(),
                                ours != our_plain,
                            ));
                        }
                        if ours.len() > theirs.len() + theirs.len() / 100 + 64 {
                            size_gaps.push((
                                corpus.name,
                                use_dictionary,
                                level,
                                case.label.clone(),
                                ours.len(),
                                theirs.len(),
                            ));
                        }
                        differential_rows += 1;
                        continue;
                    }

                    if recorded {
                        recorded_rows += 1;
                        // Not byte parity, but still a bound: nothing may
                        // balloon. The margin covers the recorded rows above,
                        // whose worst overshoot is six bytes.
                        assert!(
                            ours.len() <= theirs.len() + theirs.len() / 100 + 64,
                            "{}{} L{level} {}: {} bytes against upstream's {}",
                            corpus.name,
                            if use_dictionary { "+dict" } else { "" },
                            case.label,
                            ours.len(),
                            theirs.len()
                        );
                        continue;
                    }

                    if ours != theirs {
                        // Collected rather than raised on the spot: one run
                        // should report every row that drifted, not the first.
                        mismatches.push(format!(
                            "{}{} L{level} {} {:?}: {} bytes against upstream's {}",
                            corpus.name,
                            if use_dictionary { "+dict" } else { "" },
                            case.label,
                            case.settings,
                            ours.len(),
                            theirs.len()
                        ));
                    }
                    asserted += 1;
                }
            }
        }
    }

    // Asserted as sets, so closing one of these forces its entry out rather
    // than leaving a stale exemption behind.
    engagement_gaps.sort();
    assert_eq!(
        engagement_gaps
            .iter()
            .map(|(corpus, dict, level, label, _)| (*corpus, *dict, *level, label.as_str()))
            .collect::<Vec<_>>(),
        OVERRIDE_ENGAGEMENT_GAPS.to_vec(),
        "the set of rows where an override moves one side and not the other changed",
    );
    size_gaps.sort();
    assert_eq!(
        size_gaps
            .iter()
            .map(|(corpus, dict, level, label, ours, theirs)| {
                (*corpus, *dict, *level, label.as_str(), *ours, *theirs)
            })
            .collect::<Vec<_>>(),
        OVERRIDE_DIFFERENTIAL_SIZE_GAPS.to_vec(),
        "the set of differential rows larger than upstream changed",
    );
    assert!(
        mismatches.is_empty(),
        "{} of {asserted} override rows are not upstream's bytes:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
    // The sweep is meant to compare hundreds of rows. A refactor that made
    // `baseline_matches` false everywhere would otherwise leave a green test
    // that compared nothing.
    assert!(
        asserted > 400,
        "only {asserted} rows were compared byte for byte"
    );
    // Pinned so the counts quoted in `docs/PARITY_PLAN.md` and the CHANGELOG
    // are measured rather than remembered. All three may move as corpora or
    // levels change; update them together.
    //
    // The sum is the invariant that matters: every row lands in exactly one of
    // the three, so a row that stops being byte-identical has to show up as
    // differential or recorded rather than disappearing. It did not before:
    // this sweep used to pin (849, 528) and skip the remaining 378 of its 1755
    // rows outright, and the row-lazy repcode substitution moved 99 more into
    // that invisible set. The three now sum to 1755, and the substitution shows
    // up as 847 -> 748 byte-identical against 252 -> 351 differential.
    //
    // `use_row_match_finder` added three cases and took the total to 1950.
    // `literal_compression` added three more, taking it to 2145, and 132 of
    // those 195 rows are byte-identical -- the highest share any single
    // parameter has contributed.
    assert_eq!(
        (asserted, recorded_rows, differential_rows),
        (1000, 674, 471),
        "the sweep's shape changed: {asserted} byte-identical rows, \
         {recorded_rows} recorded ones and {differential_rows} differential ones"
    );
}

/// Overrides land on parameters that have *already* been adjusted once, and
/// are then adjusted a second time.
///
/// This is the test the identity case cannot stand in for. Comparing
/// "overrides set to the level's own values" against "no overrides" is
/// satisfied by any implementation that applies overrides anywhere in the
/// pipeline, because the values are the table's either way. What separates the
/// orders is an override of `strategy`: upstream's first adjustment pass sizes
/// `chain_log` using the *table's* strategy and only the second pass sees the
/// caller's, so applying the override before a single pass produces a
/// different `chain_log` — a frame that is valid, decodes correctly, and is
/// not upstream's.
///
/// The parameters are read back from upstream directly rather than inferred
/// from the compressed size, so a failure names the parameter that drifted.
#[test]
fn overrides_are_applied_between_the_two_adjustment_passes() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    // Small enough that adjustment binds: at 6 KiB every tier's window is
    // clamped to the source and `chain_log` is pulled down with it, which is
    // the reduction the strategy override changes the size of.
    let input = build_pattern(6 * 1024);
    let mut compared = 0usize;

    for level in [1, 3, 5, 9, 15, 19, 22] {
        for strategy in [
            Strategy::Fast,
            Strategy::DoubleFast,
            Strategy::Greedy,
            Strategy::Lazy2,
            Strategy::BinaryTreeLazy2,
            Strategy::BinaryTreeOpt,
            Strategy::BinaryTreeUltra2,
        ] {
            for window_log in [10u32, 13, 16] {
                let settings = vec![
                    format!("strategy={}", strategy.as_u32()),
                    format!("windowLog={window_log}"),
                ];
                let theirs = upstream_trace_helper::trace_advanced_applied_cparams(
                    helper,
                    upstream_trace_helper::DICT_NONE,
                    &upstream_settings_for(level, &settings),
                    &input,
                );
                let ours = zstandard::trace_first_block_with_options(
                    &input,
                    EncoderOptions {
                        compression_level: CompressionLevel::try_new(level).unwrap(),
                        parameters: ParameterOverrides {
                            window_log: Some(window_log),
                            strategy: Some(strategy),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
                .unwrap()
                .compression_parameters;

                assert_eq!(
                    (
                        ours.window_log,
                        ours.chain_log,
                        ours.hash_log,
                        ours.search_log,
                        ours.min_match,
                        ours.target_length,
                    ),
                    (
                        theirs.window_log,
                        theirs.chain_log,
                        theirs.hash_log,
                        theirs.search_log,
                        theirs.min_match,
                        theirs.target_length,
                    ),
                    "level {level} strategy {strategy:?} window_log {window_log}"
                );
                compared += 1;
            }
        }
    }

    assert_eq!(
        compared,
        7 * 7 * 3,
        "the sweep did not cover its own matrix"
    );
}

/// Overrides reach the CDict's parameters as well as the source's.
///
/// `ZSTD_CCtx_loadDictionary` builds its CDict through
/// `ZSTD_createCDict_advanced2`, which resolves parameters through the same
/// `ZSTD_getCParamsFromCCtxParams` the source uses and so applies the
/// overrides a second time. Threading them into only one of the two calls
/// still round-trips and still decodes under upstream, so only a comparison of
/// the parameters themselves catches it.
///
/// `hash_log` and `chain_log` are what this reads, because they are what the
/// CDict path shrinks: the applied parameters take the CDict's table sizes and
/// the source's window.
#[test]
fn parameter_overrides_reach_the_dictionary_side_too() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
    let trained_dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
    let raw_prepared = EncoderDictionary::new(&raw_dictionary).unwrap();
    let trained_prepared = EncoderDictionary::new(&trained_dictionary).unwrap();
    let input = build_pattern(64 * 1024);
    let mut compared = 0usize;

    for (dict_mode, prepared) in [
        (upstream_trace_helper::DICT_RAW, &raw_prepared),
        (upstream_trace_helper::DICT_TRAINED, &trained_prepared),
    ] {
        for level in [1, 5, 12, 19] {
            for (hash_log, chain_log) in [(12u32, 12u32), (20, 20), (18, 16)] {
                let settings = vec![
                    format!("hashLog={hash_log}"),
                    format!("chainLog={chain_log}"),
                ];
                let theirs = upstream_trace_helper::trace_advanced_applied_cparams(
                    helper,
                    dict_mode,
                    &upstream_settings_for(level, &settings),
                    &input,
                );
                let ours = zstandard::trace_first_block_with_prepared_dict_and_options(
                    &input,
                    prepared,
                    EncoderOptions {
                        compression_level: CompressionLevel::try_new(level).unwrap(),
                        parameters: ParameterOverrides {
                            hash_log: Some(hash_log),
                            chain_log: Some(chain_log),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
                .unwrap()
                .compression_parameters;

                assert_eq!(
                    (ours.window_log, ours.hash_log, ours.chain_log),
                    (theirs.window_log, theirs.hash_log, theirs.chain_log),
                    "{dict_mode} dictionary, level {level}, hash_log {hash_log} chain_log {chain_log}"
                );
                compared += 1;
            }
        }
    }

    assert_eq!(
        compared,
        2 * 4 * 3,
        "the sweep did not cover its own matrix"
    );
}

/// `min_match: Some(7)` is a cliff for upstream and flat for this crate, and it
/// takes a dictionary to become one.
///
/// This is the last row of [`OVERRIDE_ENGAGEMENT_GAPS`], recorded as a
/// measurement because the sizes alone read like a parse tie and the shape does
/// not:
///
/// - With no dictionary, upstream's 6 and 7 are the *same frame*, byte for byte.
/// - With a raw dictionary, they differ by 149 to 196 bytes, and 7 is the only
///   value that moves: upstream's 3, 4, 5 and 6 all produce one frame.
/// - This crate is flat across 3..=7 with the dictionary and never pays for 7
///   without it.
///
/// **What is not established is why 7 is upstream's cliff.** The reading that
/// suggests itself is a clamp asymmetry: C clamps `minMatch` into a hashable
/// range at every search (`BOUNDED(3, .., 6)` at `zstd_opt.c:896`, `BOUNDED(4,
/// .., 6)` at `zstd_lazy.c:1531`) while `ZSTD_updateTree` (`zstd_opt.c:584`)
/// and `ZSTD_insertAndFindFirstIndex` (`zstd_lazy.c:661`) pass
/// `ms->cParams.minMatch` straight into `ZSTD_hashPtr`, so a preload at 7 would
/// file the dictionary under a hash no search ever asks for.
///
/// That reading is refuted for the lazy levels and only survives for the
/// binary-tree ones. `ZSTD_resolveRowMatchFinderMode` enables row hashing
/// whenever `windowLog > 14` (`zstd_compress.c:243`), which holds at every
/// level here, so level 6's preload runs `ZSTD_row_update` — and that one
/// *does* clamp, `MIN(minMatch, 6)` at `zstd_lazy.c:952`, agreeing with its
/// search. Level 6 degrades by as much as level 12 with no asymmetry to blame.
/// Clamping this crate's own preload to 6 to reproduce the effect changed no
/// byte anywhere in the suite, which is a third strike against it.
///
/// So the assertions below pin the *measurements* and not a mechanism. The
/// upstream half is deliberate: it holds a pinned checkout to behaviour that is
/// arguably a bug, and a version that changed it would fail here rather than
/// silently leave the recorded row unexplained.
#[test]
fn a_min_match_of_seven_is_only_upstreams_cliff() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
    let raw_prepared = EncoderDictionary::new(&raw_dictionary).unwrap();
    let corpus = benchmark_corpora::benchmark_report_cases(OVERRIDE_SWEEP_SIZE)
        .into_iter()
        .find(|case| case.name == "raw-dictionary")
        .expect("raw-dictionary is a benchmark corpus");

    // A lazy level and two binary-tree levels. The lazy one is the one that
    // refutes the clamp reading, so dropping it would leave the story looking
    // settled.
    for level in [6, 12, 16] {
        let ours = |min_match: u32, dictionary: bool| {
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                parameters: ParameterOverrides {
                    min_match: Some(min_match),
                    ..Default::default()
                },
                ..Default::default()
            };
            if dictionary {
                encode_all_with_prepared_dict_and_options(&corpus.input, &raw_prepared, options)
                    .unwrap()
            } else {
                encode_all_with_options(&corpus.input, options).unwrap()
            }
        };
        let theirs = |min_match: u32, dictionary: bool| {
            upstream_trace_helper::compress_advanced(
                helper,
                if dictionary {
                    upstream_trace_helper::DICT_RAW
                } else {
                    upstream_trace_helper::DICT_NONE
                },
                &upstream_settings_for(level, &[format!("minMatch={min_match}")]),
                &corpus.input,
            )
        };

        // Ours: 7 never costs more than 6, with a dictionary or without one.
        assert!(
            ours(7, true).len() <= ours(6, true).len(),
            "level {level}: min_match 7 costs us {} bytes over 6 with a dictionary",
            ours(7, true).len() - ours(6, true).len()
        );
        assert!(
            ours(7, false).len() <= ours(6, false).len(),
            "level {level}: min_match 7 costs us bytes over 6 without a dictionary"
        );

        // Upstream, with a dictionary: 3 through 6 are one frame and 7 alone
        // breaks away. Asserting the flat run matters as much as asserting the
        // cliff — without it, "7 is worse than 6" would also pass on an upstream
        // where every value moved, which is a different world entirely.
        for min_match in [3u32, 4, 5] {
            assert_eq!(
                theirs(min_match, true),
                theirs(6, true),
                "level {level}: upstream's min_match {min_match} and 6 no longer agree \
                 with a dictionary, so 7 is not the lone cliff this records"
            );
        }
        assert!(
            theirs(7, true).len() > theirs(6, true).len(),
            "level {level}: upstream's min_match 7 is no longer a cliff with a \
             dictionary. If upstream changed here, this divergence is gone and \
             OVERRIDE_ENGAGEMENT_GAPS should lose its remaining row."
        );

        // And without a dictionary the cliff is not there at all. This is the
        // half that makes the row a dictionary story rather than a min_match one.
        assert_eq!(
            theirs(7, false),
            theirs(6, false),
            "level {level}: upstream's min_match 6 and 7 differ without a dictionary, \
             so the cliff is no longer dictionary-dependent"
        );
    }
}

/// A `window_log` below the size of the frame is upstream's frame, byte for
/// byte — now without the caller having to hold `block_size` at the window.
///
/// **The parse.** Every parser but the fast pair takes its floor at the
/// position doing the looking — C's `ZSTD_getLowestMatchIndex(ms, curr,
/// windowLog)` — instead of once per block from the block's end. With one floor
/// per block and a block as wide as its window, the floor lands exactly on the
/// block's start and the parser finds *no* history at all: on this case that
/// was about 900 extra literal bytes in every block after the first, the same
/// count it emitted when a block's bytes were handed to it alone.
///
/// **The blocks.** Upstream shrinks a block to the window and declares the
/// window alone, and this encoder now does the same: `block_size_max_for` caps
/// the block and `frame_window_size_for` declares the history by itself. This
/// test used to hold `block_size` at the window by hand, because the encoder
/// would otherwise keep the caller's block size and declare a window wide
/// enough for what such a block could reach. The loop below still holds it, so
/// the two halves stay separable; the tail no longer has to, and asserts that
/// leaving `block_size` alone gives the same frame.
#[test]
fn overriding_the_window_below_the_frame_matches_upstream() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    let input = build_pattern(128 * 1024);
    for window_log in [15u32, 17] {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(5).unwrap(),
            block_size: 1 << window_log,
            parameters: ParameterOverrides {
                window_log: Some(window_log),
                ..Default::default()
            },
            ..Default::default()
        };
        let ours = encode_all_with_options(&input, options).unwrap();
        let theirs = upstream_trace_helper::compress_advanced(
            helper,
            upstream_trace_helper::DICT_NONE,
            &upstream_settings_for(5, &[format!("windowLog={window_log}")]),
            &input,
        );

        assert_eq!(decode_all(&ours).unwrap(), input);
        assert_eq!(
            upstream_trace_helper::decompress_once(helper, "decompress", &ours),
            input
        );

        let FrameHeader::Zstandard(ours_header) = parse_frame_header(&ours).unwrap() else {
            panic!("expected a Zstandard frame");
        };
        let FrameHeader::Zstandard(their_header) = parse_frame_header(&theirs).unwrap() else {
            panic!("expected a Zstandard frame");
        };
        assert_eq!(ours_header.window_size, 1 << window_log);
        assert_eq!(ours_header.window_size, their_header.window_size);
        assert_eq!(block_sizes(&ours), block_sizes(&theirs));
        assert_eq!(
            ours,
            theirs,
            "window_log={window_log}: {} bytes against upstream's {}",
            ours.len(),
            theirs.len()
        );
    }

    // And now without holding `block_size` at all: the encoder caps the block
    // itself, so leaving the caller's 128 KiB default in place gives the same
    // frame the held loop above produced. This assertion is the inverse of the
    // one it replaces, which recorded that the declared window came out wider
    // than upstream's whenever the caller did not hold the block down.
    let unheld = encode_all_with_options(
        &input,
        EncoderOptions {
            compression_level: CompressionLevel::try_new(5).unwrap(),
            parameters: ParameterOverrides {
                window_log: Some(15),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(decode_all(&unheld).unwrap(), input);
    let FrameHeader::Zstandard(unheld_header) = parse_frame_header(&unheld).unwrap() else {
        panic!("expected a Zstandard frame");
    };
    assert_eq!(
        unheld_header.window_size,
        1 << 15,
        "an uncapped block should no longer widen the declared window"
    );

    let theirs = upstream_trace_helper::compress_advanced(
        helper,
        upstream_trace_helper::DICT_NONE,
        &upstream_settings_for(5, &["windowLog=15".to_string()]),
        &input,
    );
    assert_eq!(block_sizes(&unheld), block_sizes(&theirs));
    assert_eq!(
        unheld,
        theirs,
        "unheld block_size: {} bytes against upstream's {}",
        unheld.len(),
        theirs.len()
    );
}

/// The binary tree stays upstream's once the body outgrows its window.
///
/// The tree's *inserts* have to stop where C's stop. `ZSTD_insertBt1` bounds
/// its traversal by `ZSTD_getLowestMatchIndex(ms, target, cParams->windowLog)`,
/// which for a source with no dictionary is `target - (1 << windowLog)` as soon
/// as the history outgrows the window and `lowLimit` only until then. This
/// crate's inserter used `ZSTD_WINDOW_START_INDEX` as the whole floor, on a
/// comment asserting that C's stayed there too on the contiguous path. It does
/// not.
///
/// The insert records no matches, so the mistake could not emit an illegal
/// offset and nothing rejected it. What it did instead was thread the tree
/// through positions the window had already dropped; a later search follows
/// those links, meets its own floor, and stops before candidates that were
/// still in range. The frames stayed valid and got bigger.
///
/// **Three things had to line up for it to be visible, and the sweeps that
/// should have caught it each missed one.** The body has to outgrow the window
/// -- under about 320 KiB at these windows the floor never rises and the two
/// agree exactly. The window has to be tight enough that the lost links matter,
/// which is why 17 shows it and 20 does not. And the corpus has to have repeats
/// out near the window distance to lose, which of the benchmark corpora is only
/// `wikipedia`. Every other corpus here agrees to the byte at every window,
/// before the fix as well as after.
///
/// It was found through a long-distance-matching parameter sweep, which is not
/// where it lives: two recorded gaps there were `wikipedia` at btopt with a
/// window of 17, and turning the matcher off left them at 7.81%.
///
/// The levels are natural ones, so a failure here is a shipped regression and
/// not a strategy override's off-manifold configuration.
#[test]
fn a_window_the_body_outgrows_keeps_the_tree_upstreams() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    let mut compared = 0usize;
    let mut identical = 0usize;
    // 1 MiB against windows of 128 KiB and 256 KiB: eight and four times the
    // window, so the floor is well clear of `lowLimit` for most of the frame.
    // 256 KiB against a window of 128 KiB is the control -- two windows is not
    // enough for the divergence to appear, and it must still agree.
    for size in [256 * 1024usize, 1024 * 1024] {
        for corpus in benchmark_corpora::benchmark_report_cases(size) {
            // btopt, btultra and btultra2, by the levels that select them.
            for level in [16, 19, 22] {
                for window_log in [17u32, 18] {
                    let settings = vec![format!("windowLog={window_log}")];
                    let ours = encode_all_with_options(
                        &corpus.input,
                        EncoderOptions {
                            compression_level: CompressionLevel::try_new(level).unwrap(),
                            parameters: ParameterOverrides {
                                window_log: Some(window_log),
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                    )
                    .unwrap();
                    let theirs = upstream_trace_helper::compress_advanced(
                        helper,
                        upstream_trace_helper::DICT_NONE,
                        &upstream_settings_for(level, &settings),
                        &corpus.input,
                    );
                    assert!(
                        ours.len() <= theirs.len() + theirs.len() / 100 + 64,
                        "{} at {} KiB, level {level}, window_log {window_log}: \
                         {} bytes against upstream's {}",
                        corpus.name,
                        size / 1024,
                        ours.len(),
                        theirs.len()
                    );
                    identical += usize::from(ours == theirs);
                    compared += 1;
                }
            }
        }
    }

    // Eleven corpora, two sizes, three levels, two windows.
    assert_eq!(
        compared,
        11 * 2 * 3 * 2,
        "the sweep did not cover its matrix"
    );

    // The bound above is one-directional, so it cannot notice the parse
    // drifting as long as the frames stay small. This is the other half: 91 of
    // the 132 rows are upstream's exact bytes, and losing them would mean
    // something moved even though nothing got bigger.
    //
    // The 41 that differ are almost all in this crate's favour and none is
    // large -- the worst overshoot in the whole grid is 210 bytes on 161 KB,
    // 0.13%, against a bound of 1%. Before the insert floor was fixed the same
    // grid ran to 10.93%.
    assert_eq!(
        identical, 91,
        "the number of rows matching upstream byte for byte changed"
    );
}

/// Overriding `strategy` moves the parsers into parameter combinations no
/// level produces, and this crate does not match upstream throughout them.
///
/// 85 of 459 swept rows differ. The output is always valid and always
/// round-trips, and is more often smaller than larger, but it is not
/// upstream's. The clearest family is `BinaryTreeLazy2` driven by a level
/// whose own row is `Fast` — `search_log` of 1 and a `chain_log` of 12 give a
/// binary tree two probes deep over a roll buffer of 2048 positions, which no
/// level asks for.
///
/// One cause was found and fixed while measuring this: the optimal parsers
/// substituted a per-strategy constant for `sufficient_len` when
/// `target_length` was zero, which upstream never does. Every optimal row in
/// the level table carries a non-zero `target_length`, so nothing but an
/// override could reach it; it was worth up to 6 KB on a 128 KiB frame. What
/// remains after that is smaller and has not been diagnosed.
#[test]
fn overriding_the_strategy_leaves_the_levels_parameter_space() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    let strategies = [
        Strategy::Fast,
        Strategy::DoubleFast,
        Strategy::Greedy,
        Strategy::Lazy,
        Strategy::Lazy2,
        Strategy::BinaryTreeLazy2,
        Strategy::BinaryTreeOpt,
        Strategy::BinaryTreeUltra,
        Strategy::BinaryTreeUltra2,
    ];
    let mut compared = 0usize;

    for corpus in benchmark_corpora::benchmark_report_cases(OVERRIDE_SWEEP_SIZE) {
        for level in OVERRIDE_SWEEP_LEVELS {
            for strategy in strategies {
                let ours = encode_all_with_options(
                    &corpus.input,
                    EncoderOptions {
                        compression_level: CompressionLevel::try_new(level).unwrap(),
                        parameters: ParameterOverrides {
                            strategy: Some(strategy),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
                .unwrap();
                let theirs = upstream_trace_helper::compress_advanced(
                    helper,
                    upstream_trace_helper::DICT_NONE,
                    &upstream_settings_for(level, &[format!("strategy={}", strategy.as_u32())]),
                    &corpus.input,
                );

                assert_eq!(
                    decode_all(&ours).unwrap(),
                    corpus.input,
                    "{} L{level} {strategy:?} did not round-trip",
                    corpus.name
                );
                assert_eq!(
                    upstream_trace_helper::decompress_once(helper, "decompress", &ours),
                    corpus.input,
                    "{} L{level} {strategy:?} was not readable by upstream",
                    corpus.name
                );
                // 0.2% and 64 bytes, ten times tighter than the 2% this
                // carried until 2026-08-06. Sized against what the sweep
                // actually does: 36 of its 495 rows come out larger than
                // upstream and the worst of them is 48 bytes on 41 KB, so every
                // one is a parse tie and none needs anything like 2%. The old
                // slack would have absorbed a real 1.9% regression on every row
                // at once without a word.
                assert!(
                    ours.len() <= theirs.len() + theirs.len() / 500 + 64,
                    "{} L{level} {strategy:?}: {} bytes against upstream's {}",
                    corpus.name,
                    ours.len(),
                    theirs.len()
                );

                compared += 1;
            }
        }
    }

    // A byte-identity floor of 82% stood here until 2026-08-06 -- 410 of 495
    // rows matching upstream exactly. It went for the same reason the
    // long-distance parity sets did: it counts a quantity this crate has stopped
    // aiming at, so it falls as the encoder deliberately improves. The
    // row-lazy repcode substitution alone took it from 410 to 323 without a
    // single row getting worse. What the floor was really guarding -- that a
    // change has not broken strategy overrides outright -- is covered above,
    // per row rather than in aggregate: the frame round-trips, upstream reads
    // it, and it stays within 0.2% of upstream's size.
    //
    // The row count stays, because it is the one thing the per-row assertions
    // cannot say: that every row ran.
    assert_eq!(
        compared,
        OVERRIDE_SWEEP_LEVELS.len() * 11 * 9,
        "the sweep did not cover its grid",
    );
}

/// `write_content_size: false` emits a windowed header with no
/// `Frame_Content_Size`, and upstream reads it.
#[test]
fn suppressing_the_content_size_matches_upstream() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    for size in [0usize, 1, 255, 4096, 200_000] {
        let input = build_pattern(size);
        for level in [1, 5, 19] {
            let ours = encode_all_with_options(
                &input,
                EncoderOptions {
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    write_content_size: false,
                    ..Default::default()
                },
            )
            .unwrap();

            let FrameHeader::Zstandard(header) = parse_frame_header(&ours).unwrap() else {
                panic!("expected a Zstandard frame");
            };
            assert!(
                !header.single_segment,
                "{size} bytes at level {level} still set Single_Segment_flag"
            );
            assert_eq!(
                header.content_size, None,
                "{size} bytes at level {level} still declared a content size"
            );

            assert_eq!(decode_all(&ours).unwrap(), input);
            let theirs = upstream_trace_helper::compress_advanced(
                helper,
                upstream_trace_helper::DICT_NONE,
                &upstream_settings_for(level, &["contentSizeFlag=0".to_string()]),
                &input,
            );
            assert_eq!(
                ours,
                theirs,
                "{size} bytes at level {level}: {} bytes against upstream's {}",
                ours.len(),
                theirs.len()
            );
        }
    }
}

/// A magicless frame is the same frame with its first four bytes gone.
#[test]
fn magicless_frames_match_upstream_and_round_trip() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    for size in [0usize, 1, 4096, 200_000] {
        let input = build_pattern(size);
        for level in [1, 5, 19] {
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                format: Format::Zstd1Magicless,
                ..Default::default()
            };
            let magicless = encode_all_with_options(&input, options).unwrap();
            let standard = encode_all_with_options(
                &input,
                EncoderOptions {
                    format: Format::Zstd1,
                    ..options
                },
            )
            .unwrap();

            assert_eq!(
                magicless,
                standard[4..],
                "{size} bytes at level {level}: the two formats differ by more than the magic"
            );

            let theirs = upstream_trace_helper::compress_advanced(
                helper,
                upstream_trace_helper::DICT_NONE,
                // ZSTD_f_zstd1_magicless is 1.
                &upstream_settings_for(level, &["format=1".to_string()]),
                &input,
            );
            assert_eq!(magicless, theirs, "{size} bytes at level {level}");

            let decoder_options = DecoderOptions {
                format: Format::Zstd1Magicless,
                ..Default::default()
            };
            assert_eq!(
                zstandard::decode_all_with_options(&magicless, decoder_options).unwrap(),
                input,
                "{size} bytes at level {level}"
            );
        }
    }
}

/// A pledged size makes a stream pick its parameters the way a one-shot call
/// does, instead of always taking the largest tier.
#[test]
fn a_pledged_size_selects_the_source_size_tier() {
    // Both sides of every tier boundary, which `upstream_cparams_tier` puts at
    // 16 KiB, 128 KiB and 256 KiB with each bound inclusive.
    let sizes = [
        8 * 1024usize,
        16 * 1024,
        16 * 1024 + 1,
        128 * 1024,
        128 * 1024 + 1,
        256 * 1024,
        256 * 1024 + 1,
    ];

    for size in sizes {
        let input = build_pattern(size);
        // Level 12's rows differ between tiers in strategy as well as table
        // size, so a stream that ignored the pledge would not merely be
        // slightly larger.
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(12).unwrap(),
            pledged_src_size: Some(size as u64),
            ..Default::default()
        };

        let mut encoder = StreamingEncoder::new(options).unwrap();
        encoder.push(&input).unwrap();
        encoder.finish().unwrap();
        let pledged = encoder.take_output();

        let one_shot = encode_all_with_options(&input, options).unwrap();
        assert_eq!(
            decode_all(&pledged).unwrap(),
            input,
            "{size} bytes did not round-trip"
        );

        let FrameHeader::Zstandard(header) = parse_frame_header(&pledged).unwrap() else {
            panic!("expected a Zstandard frame");
        };
        assert_eq!(
            header.content_size,
            Some(size as u64),
            "{size} bytes: the pledge did not reach the frame header"
        );

        // The one-shot encoder resolves parameters from the real length, so
        // its window is the one the pledge should have produced. Blocks may
        // still be laid out differently — streaming splits its input as it
        // arrives — so the window is what is compared, not the bytes.
        let FrameHeader::Zstandard(one_shot_header) = parse_frame_header(&one_shot).unwrap() else {
            panic!("expected a Zstandard frame");
        };
        assert_eq!(
            header.window_size, one_shot_header.window_size,
            "{size} bytes: the pledged stream declared a different window than the one-shot encode"
        );

        // Without the pledge the stream has to assume the largest tier, so
        // this is also a check that the pledge changed anything at all.
        let mut unpledged_encoder = StreamingEncoder::new(EncoderOptions {
            pledged_src_size: None,
            ..options
        })
        .unwrap();
        unpledged_encoder.push(&input).unwrap();
        unpledged_encoder.finish().unwrap();
        let unpledged = unpledged_encoder.take_output();
        let FrameHeader::Zstandard(unpledged_header) = parse_frame_header(&unpledged).unwrap()
        else {
            panic!("expected a Zstandard frame");
        };
        assert!(
            unpledged_header.window_size >= header.window_size,
            "{size} bytes: the unpledged stream declared a narrower window"
        );
        assert_eq!(unpledged_header.content_size, None);
    }
}

/// A stream that carries a different number of bytes than it pledged is
/// rejected, rather than completing a frame whose header states a length the
/// payload does not have.
#[test]
fn a_broken_pledge_is_reported_at_finish() {
    let input = build_pattern(4096);

    for pledged in [0u64, 4095, 4097, 1 << 20] {
        let mut encoder = StreamingEncoder::new(EncoderOptions {
            pledged_src_size: Some(pledged),
            ..Default::default()
        })
        .unwrap();
        encoder.push(&input).unwrap();
        assert!(
            matches!(encoder.finish(), Err(Error::InvalidParameter(_))),
            "a pledge of {pledged} against {} bytes was accepted",
            input.len()
        );
    }

    let mut encoder = StreamingEncoder::new(EncoderOptions {
        pledged_src_size: Some(input.len() as u64),
        ..Default::default()
    })
    .unwrap();
    encoder.push(&input).unwrap();
    encoder.finish().unwrap();
    assert_eq!(decode_all(&encoder.take_output()).unwrap(), input);
}

/// One level per parser family, which is every strategy upstream has. The
/// first six take the matcher's output; the last three price it as a candidate
/// against their own, and the sweep covers both halves of
/// `ZSTD_ldm_blockCompress` because the two share nothing but the matcher.
const LDM_SWEEP_STRATEGIES: [(Strategy, &str); 9] = [
    (Strategy::Fast, "1"),
    (Strategy::DoubleFast, "2"),
    (Strategy::Greedy, "3"),
    (Strategy::Lazy, "4"),
    (Strategy::Lazy2, "5"),
    (Strategy::BinaryTreeLazy2, "6"),
    (Strategy::BinaryTreeOpt, "7"),
    (Strategy::BinaryTreeUltra, "8"),
    (Strategy::BinaryTreeUltra2, "9"),
];

// `LDM_KNOWN_DIVERGENCES` stood here until 2026-08-06, recording one row --
// `wikipedia` at greedy, 18545 bytes against upstream's 18732, this crate
// finding a materially smaller parse. It went when the sweep stopped asserting
// byte parity: under a size bound that row is not a divergence to record, it is
// the sweep passing. Its real lesson is kept in that test's doc comment, which
// is that the row vanished from the set when the *baseline* stopped matching
// and read exactly like a fix.

/// The corpora the dictionary sweep runs, chosen for where the matcher engages
/// rather than for coverage of the corpus generator.
const LDM_DICTIONARY_CORPORA: [&str; 3] = ["wikipedia", "json-records", "log-lines"];

/// Rows where enabling the matcher reproduces the no-matcher frame exactly, as
/// `(corpus, strategy, dictionary)`.
///
/// Recorded rather than tolerated. These rows still prove the matcher does no
/// *harm* under a dictionary, but they cannot say it does any good, and without
/// the set below the sweep would read as if every row were testing something.
const LDM_DICTIONARY_INERT_ROWS: &[(&str, &str, &str)] = &[];

// `LDM_DICTIONARY_KNOWN_DIVERGENCES` stood here until 2026-08-06 with four
// `wikipedia` rows, none over 7 bytes on a 12-18 KB frame. They were ties
// rather than divergences with a direction -- at other corpus sizes the set
// moved and the sign moved with it -- which is precisely what a byte-parity
// assertion cannot express and a one-directional size bound does not care
// about. The four rows are all still compared; they just no longer need to be
// enumerated to pass.

/// Dictionary long-distance rows where this crate is *larger* than upstream, as
/// `(corpus, strategy code, dictionary, ours, theirs)`.
///
/// **Twenty-one of the fifty-four rows, and not one of them had ever been
/// compared.** The sweep used to check sizes only where the *no-matcher* frame
/// already matched upstream byte for byte, and these are rows where it does
/// not: this crate's dictionary parsers differ from upstream at several
/// strategies for reasons that predate long-distance matching, so every one of
/// these rows was excluded before it reached any comparison at all. Dropping
/// that gate is what made them visible.
///
/// Read as attribution, this list would be misleading, and it is worth being
/// plain about why: once the two baselines differ, no comparison of *sizes*
/// can separate the matcher's contribution from the inherited gap. Every row
/// here has a no-matcher baseline that is already larger than upstream's. What
/// attributes the matcher is the engagement assertion, which needs no baseline
/// and covers the whole grid.
///
/// Read as competitiveness, which is what it is for, the shape is: sixteen of
/// the twenty-one are within 10 bytes on frames of 12 KB to 340 KB -- parse
/// ties, the same signature as `KNOWN_UPSTREAM_SIZE_GAPS`'s tail. Five are
/// larger and are the ones to look at first: `json-records` Fast raw at +358,
/// `log-lines` Fast trained at +349, `wikipedia` Fast at +79 and +49, and
/// `log-lines` btultra trained at +27. Four of those five are Fast, which is
/// where `docs/PARITY_PLAN.md`'s dictionary band already has open items.
///
/// Asserted with both sizes, as `KNOWN_UPSTREAM_SIZE_GAPS` is: a row that
/// closes fails here and has to be removed, a row that opens fails, and a row
/// that widens fails too.
const LDM_DICTIONARY_SIZE_GAPS: &[(&str, &str, &str, usize, usize)] = &[
    ("json-records", "1", "raw", 153582, 153224),
    ("json-records", "7", "raw", 58550, 58548),
    ("json-records", "8", "raw", 58696, 58694),
    ("json-records", "9", "raw", 58740, 58737),
    ("json-records", "9", "trained", 61445, 61443),
    ("log-lines", "1", "trained", 343988, 343639),
    ("log-lines", "7", "raw", 213779, 213769),
    ("log-lines", "7", "trained", 209783, 209781),
    ("log-lines", "8", "raw", 151453, 151452),
    ("log-lines", "8", "trained", 152669, 152642),
    ("log-lines", "9", "raw", 141895, 141893),
    ("log-lines", "9", "trained", 144217, 144216),
    ("wikipedia", "1", "raw", 32731, 32652),
    ("wikipedia", "1", "trained", 32033, 31984),
    ("wikipedia", "6", "raw", 14608, 14603),
    ("wikipedia", "7", "raw", 12377, 12373),
    ("wikipedia", "7", "trained", 12143, 12139),
    ("wikipedia", "8", "raw", 12209, 12207),
    ("wikipedia", "8", "trained", 12049, 12047),
    ("wikipedia", "9", "raw", 12578, 12576),
    ("wikipedia", "9", "trained", 12389, 12387),
];

/// Long-distance matching with a dictionary engages where upstream's does, and
/// is measured against upstream's size on every row.
///
/// Both dictionary kinds run, because they select different parser plumbing:
/// a raw dictionary and a trained one resolve to different `PrefixMatchMode`s
/// at some strategies, and the long-distance path reaches the parser through
/// that mode.
///
/// **This asserted byte parity on 15 of its 54 rows until 2026-08-06**, the
/// other 39 excluded because this crate's dictionary output already differs
/// from upstream with no matcher involved. The exclusion was sound attribution
/// and an expanding blind spot: it grew with every deliberate divergence, and
/// nothing in it was checked for *size* either. Dropping the gate found 21 rows
/// where we are larger than upstream, none of which had ever been compared.
/// They are in [`LDM_DICTIONARY_SIZE_GAPS`].
///
/// What attributes the matcher now is engagement rather than size: enabling it
/// changes our frame if and only if it changes upstream's. That needs no
/// baseline to match, so it covers all 54 rows, and it is the property a size
/// bound cannot supply -- a matcher that had silently stopped running would
/// satisfy any bound that only asks us not to grow.
///
/// A dictionary the matcher never reaches into would satisfy *both* by doing
/// nothing on both sides, which is why [`LDM_DICTIONARY_INERT_ROWS`] records
/// every row where enabling it reproduces the no-matcher frame exactly, and
/// asserts that set too.
#[test]
fn long_distance_matching_with_a_dictionary_engages_and_is_measured_against_upstream() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };
    const SIZE: usize = 1 << 20;
    let dictionaries = [
        (
            "raw",
            upstream_trace_helper::DICT_RAW,
            upstream_trace_helper::emit_raw_dictionary(helper),
        ),
        (
            "trained",
            upstream_trace_helper::DICT_TRAINED,
            upstream_trace_helper::emit_trained_dictionary(helper),
        ),
    ];
    let mut compared = 0usize;
    let mut inert = Vec::new();
    let mut larger = Vec::new();

    for corpus in benchmark_corpora::benchmark_report_cases(SIZE) {
        if corpus.dict_kind != benchmark_corpora::DictKind::None {
            continue;
        }
        if !LDM_DICTIONARY_CORPORA.contains(&corpus.name) {
            continue;
        }
        for (strategy, code) in LDM_SWEEP_STRATEGIES {
            for (dict_name, dict_mode, dict_bytes) in &dictionaries {
                let plain = EncoderOptions {
                    compression_level: CompressionLevel::try_new(5).unwrap(),
                    parameters: ParameterOverrides {
                        strategy: Some(strategy),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let settings = |ldm: bool| {
                    let mut settings =
                        vec!["compressionLevel=5".to_string(), format!("strategy={code}")];
                    if ldm {
                        settings.push("enableLongDistanceMatching=1".to_string());
                    }
                    settings
                };
                let ours_off =
                    encode_all_with_dict_and_options(&corpus.input, dict_bytes, plain).unwrap();
                let options = EncoderOptions {
                    parameters: ParameterOverrides {
                        long_distance_matching: zstandard::LdmMode::Enabled,
                        ..plain.parameters
                    },
                    ..plain
                };
                let ours_on =
                    encode_all_with_dict_and_options(&corpus.input, dict_bytes, options).unwrap();
                assert_eq!(
                    decode_all_with_dict(&ours_on, dict_bytes).unwrap(),
                    corpus.input,
                    "{} {strategy:?} {dict_name} did not round-trip",
                    corpus.name
                );
                if ours_on == ours_off {
                    inert.push((corpus.name, code, *dict_name));
                }

                let theirs_of = |ldm: bool| {
                    upstream_trace_helper::compress_advanced(
                        helper,
                        dict_mode,
                        &settings(ldm),
                        &corpus.input,
                    )
                };
                let (theirs_off, theirs_on) = (theirs_of(false), theirs_of(true));

                assert_eq!(
                    ours_on != ours_off,
                    theirs_on != theirs_off,
                    "{} {strategy:?} {dict_name}: the matcher changed our frame \
                     ({} against {} bytes) where upstream's went {} against {}",
                    corpus.name,
                    ours_on.len(),
                    ours_off.len(),
                    theirs_on.len(),
                    theirs_off.len(),
                );
                if ours_on.len() > theirs_on.len() {
                    larger.push((
                        corpus.name,
                        code,
                        *dict_name,
                        ours_on.len(),
                        theirs_on.len(),
                    ));
                }
                compared += 1;
            }
        }
    }
    assert_eq!(
        larger, LDM_DICTIONARY_SIZE_GAPS,
        "the set of dictionary long-distance rows larger than upstream changed"
    );
    assert_eq!(
        inert, LDM_DICTIONARY_INERT_ROWS,
        "the set of rows where the matcher changes nothing changed; a row that moved \
         out of this set is now under test, and one that moved in has stopped being"
    );
    // Every row in the grid is compared now. It used to be fifteen of
    // fifty-four: the other thirty-nine were excluded because this crate's
    // *dictionary* output already differs from upstream with no matcher
    // involved, a gap older and wider than anything here, and that exclusion
    // grew with every deliberate divergence. The two properties this asserts
    // instead need no baseline to match, so the count is the grid.
    assert_eq!(
        compared,
        LDM_DICTIONARY_CORPORA.len() * LDM_SWEEP_STRATEGIES.len() * 2,
        "the sweep did not cover its grid",
    );
}

/// Long-distance matching engages exactly where upstream's does, and never
/// costs us more bytes than upstream's costs it.
///
/// The corpus is a megabyte so the frame spans several blocks: the table lives
/// for the frame, and a single-block frame would exercise none of what carries
/// across one. The strategy is pinned rather than left to the level, because
/// enabling long-distance matching also forces the window to `1 << 27` and the
/// level's own strategy would then be chosen from a different table row on both
/// sides at once, which proves less than it looks.
///
/// **This asserted byte parity until 2026-08-06, and did so on a shrinking
/// fraction of its own grid.** It compared the matcher's frame only on rows
/// whose *no-matcher* frame already matched upstream byte for byte, so that a
/// difference was attributable to the matcher rather than inherited. Right
/// discipline, and it assumes the baseline can match. As this crate diverges
/// from upstream on purpose the baseline stops matching and rows drop out
/// silently: with the row-lazy repcode substitution applied, 64 of 81 compared
/// rows became 51. Worse, the loss is disguised as success -- the single entry
/// in the old `LDM_KNOWN_DIVERGENCES` disappeared from the actual set, which
/// reads exactly like a divergence being fixed. It was not fixed; its baseline
/// stopped matching, so the row was skipped before the comparison it was named
/// for ever ran.
///
/// What replaces it compares each side against *itself*, so no row is ever
/// excluded. Both properties below hold on all 81 rows, measured with and
/// without the divergence:
///
/// 1. **Engagement.** Enabling the matcher changes our frame if and only if it
///    changes upstream's. This is the sharp one and the reason a size bound
///    alone will not do: at this window long-distance matching is often a
///    *loss*, and on a corpus where it finds nothing it must leave the frame
///    alone. A matcher that had quietly stopped running would satisfy any
///    bound that only asks us not to grow.
/// 2. **Never larger.** Our frame with the matcher is no bigger than upstream's
///    with the matcher. This is the objective, stated one-directionally for the
///    same reason `KNOWN_UPSTREAM_SIZE_GAPS` is.
///
/// A third was tried and dropped: that the matcher should cost us no more
/// *relatively* than it costs upstream, `(ours_on - ours_off) / ours_off <=
/// (theirs_on - theirs_off) / theirs_off`. Rearranged, that is algebraically
/// the old parity-gated bound, and it fails on four rows -- `json-records` at
/// strategies 3, 4 and 5 and `wikipedia` at 6 -- in every one of which we are
/// *smaller* than upstream with the matcher on. It measures our baseline being
/// better, not our matcher being worse, so it decays exactly as the byte gate
/// did.
///
/// That leaves one hole worth naming: our matcher could get materially worse
/// against our own no-matcher frame and still satisfy both properties, so long
/// as it stayed under upstream's. That question belongs to a different layer.
/// `tests/baseline.rs` records this crate's own long-distance output per row
/// and fails when it moves, which is the check that closes it. See
/// `docs/ORACLE_PLAN.md`.
#[test]
fn long_distance_matching_engages_where_upstreams_does_and_is_never_larger() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };
    const SIZE: usize = 1 << 20;
    let mut compared = 0usize;

    for corpus in benchmark_corpora::benchmark_report_cases(SIZE) {
        if corpus.dict_kind != benchmark_corpora::DictKind::None {
            continue;
        }
        for (strategy, code) in LDM_SWEEP_STRATEGIES {
            let plain = EncoderOptions {
                compression_level: CompressionLevel::try_new(5).unwrap(),
                parameters: ParameterOverrides {
                    strategy: Some(strategy),
                    ..Default::default()
                },
                ..Default::default()
            };
            let settings = |ldm: bool| {
                let mut settings =
                    vec!["compressionLevel=5".to_string(), format!("strategy={code}")];
                if ldm {
                    settings.push("enableLongDistanceMatching=1".to_string());
                }
                settings
            };
            let theirs_of = |ldm: bool| {
                upstream_trace_helper::compress_advanced(
                    helper,
                    upstream_trace_helper::DICT_NONE,
                    &settings(ldm),
                    &corpus.input,
                )
            };

            let options = EncoderOptions {
                parameters: ParameterOverrides {
                    long_distance_matching: zstandard::LdmMode::Enabled,
                    ..plain.parameters
                },
                ..plain
            };
            let ours_off = encode_all_with_options(&corpus.input, plain).unwrap();
            let ours_on = encode_all_with_options(&corpus.input, options).unwrap();
            assert_eq!(
                decode_all(&ours_on).unwrap(),
                corpus.input,
                "{} {strategy:?} did not round-trip",
                corpus.name
            );
            let (theirs_off, theirs_on) = (theirs_of(false), theirs_of(true));

            assert_eq!(
                ours_on != ours_off,
                theirs_on != theirs_off,
                "{} {strategy:?}: long-distance matching changed our frame \
                 ({} against {} bytes) where upstream's went {} against {}",
                corpus.name,
                ours_on.len(),
                ours_off.len(),
                theirs_on.len(),
                theirs_off.len(),
            );
            assert!(
                ours_on.len() <= theirs_on.len(),
                "{} {strategy:?}: {} bytes with long-distance matching against \
                 upstream's {}",
                corpus.name,
                ours_on.len(),
                theirs_on.len(),
            );
            compared += 1;
        }
    }
    assert_eq!(
        compared,
        9 * LDM_SWEEP_STRATEGIES.len(),
        "the sweep did not cover its grid",
    );
}

/// The window a case in the sweep below resolves to when it does not pin one.
///
/// Enabling long-distance matching forces 27, and then fitting the window to a
/// known source size brings it back down: a megabyte of corpus needs 20. Every
/// case's arithmetic is written against this number rather than against the 27,
/// and `long_distance_parameters_resolve_the_windows_their_cases_assume` is
/// what keeps that from going quietly stale.
const LDM_DERIVED_WINDOW_LOG: u32 = 20;

/// The window the sweep pins where a branch is only reachable below the
/// derived one.
///
/// Also the widest that keeps the sweep honest: at 20 a megabyte of corpus sits
/// entirely inside the window, so the matcher's floor is zero on every block
/// and any bound measured against it goes untested. At 17 the same corpus spans
/// eight windows.
const LDM_PARAMETER_WINDOW_LOG: u32 = 17;

/// `ZSTD_ps_enable`, the value `enableLongDistanceMatching=1` resolves to.
const UPSTREAM_PARAM_SWITCH_ENABLE: u32 = 1;

/// One long-distance parameter configuration, and the branch of
/// `LdmParameters::resolve` it exists to reach.
struct LdmParameterCase {
    name: &'static str,
    /// What the case's name claims about the parameters this configuration
    /// *resolves to*, asserted by
    /// [`long_distance_parameters_resolve_what_their_cases_assume`].
    ///
    /// Every field here is either supplied or derived from one that is, so the
    /// values below say nothing on their own about which branch they reach.
    /// Raising `bucket-capped-by-hash-log`'s hash log from 6 to 9 stops the cap
    /// firing and the case stops covering the only line it exists for, and
    /// neither the window check nor the parity sweep notices: upstream is
    /// handed the same numbers, so both sides agree for the wrong reason. This
    /// is the assertion that notices.
    claim: fn(&upstream_trace_helper::UpstreamAppliedLdmParams) -> bool,
    window_log: Option<u32>,
    hash_log: Option<u32>,
    min_match: Option<u32>,
    bucket_size_log: Option<u32>,
    hash_rate_log: Option<u32>,
}

impl LdmParameterCase {
    const fn named(
        name: &'static str,
        claim: fn(&upstream_trace_helper::UpstreamAppliedLdmParams) -> bool,
    ) -> Self {
        Self {
            name,
            claim,
            window_log: None,
            hash_log: None,
            min_match: None,
            bucket_size_log: None,
            hash_rate_log: None,
        }
    }

    fn overrides(&self, strategy: Strategy) -> ParameterOverrides {
        ParameterOverrides {
            window_log: self.window_log,
            strategy: Some(strategy),
            long_distance_matching: zstandard::LdmMode::Enabled,
            ldm_hash_log: self.hash_log,
            ldm_min_match: self.min_match,
            ldm_bucket_size_log: self.bucket_size_log,
            ldm_hash_rate_log: self.hash_rate_log,
            ..Default::default()
        }
    }

    /// The same configuration as upstream settings.
    ///
    /// A field this case leaves unset is one upstream is never told about,
    /// which is not the same as telling it zero: three of the four reject zero
    /// on bounds, and the fourth — `ldmHashRateLog`, whose bounds start there —
    /// is the one case that deliberately does pass it.
    fn settings(&self, code: &str) -> Vec<String> {
        let mut settings = vec![
            "compressionLevel=5".to_string(),
            format!("strategy={code}"),
            "enableLongDistanceMatching=1".to_string(),
        ];
        for (name, value) in [
            ("windowLog", self.window_log),
            ("ldmHashLog", self.hash_log),
            ("ldmMinMatch", self.min_match),
            ("ldmBucketSizeLog", self.bucket_size_log),
            ("ldmHashRateLog", self.hash_rate_log),
        ] {
            if let Some(value) = value {
                settings.push(format!("{name}={value}"));
            }
        }
        settings
    }
}

/// Every branch of the derivation, one case each.
///
/// Two things constrain the values. Each has to differ from what the three
/// strategies *derive* — greedy at a window of 20 resolves to a hash log of 14
/// and a rate of 6, so a case spelling those out would prove nothing — and each
/// has to insert often enough to find something: a rate of 15 reaches the same
/// clamp as `derived-hash-log-clamped` but inserts once every 32 kilobytes, and
/// would leave a megabyte of corpus compressed to exactly the bytes it had
/// without the matcher.
static LDM_PARAMETER_CASES: &[LdmParameterCase] = &[
    // A supplied rate is what `hash_log` is then derived *from*, so this is the
    // first half of the ordering. 20 - 9 leaves a table of 2^11 and an
    // insertion every 512 bytes.
    LdmParameterCase {
        hash_rate_log: Some(9),
        ..LdmParameterCase::named("rate-supplied", |p| {
            p.hash_rate_log == 9 && p.hash_log == p.window_log - 9
        })
    },
    // And the other direction: a supplied `hash_log` below the window derives
    // the rate as the difference, so 20 - 16 inserts every 16 bytes.
    LdmParameterCase {
        hash_log: Some(16),
        ..LdmParameterCase::named("hash-log-supplied", |p| {
            p.hash_log == 16 && p.hash_rate_log == p.window_log - 16
        })
    },
    // The same configuration spelled with an explicit zero, which C reads as
    // unset rather than as a rate of one insertion per byte. It has to produce
    // the case above byte for byte, which the sweep asserts directly as well as
    // against upstream.
    LdmParameterCase {
        hash_log: Some(16),
        hash_rate_log: Some(0),
        ..LdmParameterCase::named("zero-rate-means-unset", |p| {
            p.hash_log == 16 && p.hash_rate_log == p.window_log - 16
        })
    },
    // A supplied `hash_log` that the window does *not* exceed leaves the rate
    // at zero, which C neither clamps nor derives around: the split mask ends
    // up empty and every position is an insertion.
    LdmParameterCase {
        window_log: Some(LDM_PARAMETER_WINDOW_LOG),
        hash_log: Some(18),
        ..LdmParameterCase::named("hash-log-not-below-window", |p| {
            p.hash_log == 18 && p.window_log <= 18 && p.hash_rate_log == 0
        })
    },
    // The floor on the derived `hash_log`: 17 - 12 is 5, below `ZSTD_HASHLOG_MIN`.
    LdmParameterCase {
        window_log: Some(LDM_PARAMETER_WINDOW_LOG),
        hash_rate_log: Some(12),
        ..LdmParameterCase::named("derived-hash-log-clamped", |p| {
            p.hash_rate_log == 12 && p.window_log - p.hash_rate_log < 6 && p.hash_log == 6
        })
    },
    // The line outside the unset check: a supplied bucket size is capped by
    // `hash_log` exactly as a derived one is, so 8 against 6 resolves to 6 and
    // the whole table becomes a single bucket.
    LdmParameterCase {
        hash_log: Some(6),
        bucket_size_log: Some(8),
        hash_rate_log: Some(4),
        ..LdmParameterCase::named("bucket-capped-by-hash-log", |p| {
            p.hash_log == 6 && p.bucket_size_log == 6
        })
    },
    // `ZSTD_ldm_gear_init` places the split mask's bits as high as the minimum
    // match allows. A rate wider than that minimum cannot, and falls back to a
    // mask at the bottom of the hash.
    LdmParameterCase {
        hash_log: Some(18),
        min_match: Some(4),
        hash_rate_log: Some(6),
        ..LdmParameterCase::named("rate-wider-than-min-match", |p| {
            p.min_match_length == 4 && p.hash_rate_log == 6 && p.hash_rate_log > p.min_match_length
        })
    },
    // Nothing derived at all.
    LdmParameterCase {
        hash_log: Some(18),
        min_match: Some(32),
        bucket_size_log: Some(3),
        hash_rate_log: Some(6),
        ..LdmParameterCase::named("all-four-supplied", |p| {
            (
                p.hash_log,
                p.min_match_length,
                p.bucket_size_log,
                p.hash_rate_log,
            ) == (18, 32, 3, 6)
        })
    },
];

/// The matcher's output is consumed three ways, and each is a separate body of
/// code that the parameters reach: [`Strategy::Fast`] refills its own tables
/// over the span the matcher skipped, [`Strategy::Greedy`] runs the bounded
/// table update instead, and [`Strategy::BinaryTreeOpt`] prices the matcher's
/// sequences as candidates rather than laying them down.
const LDM_PARAMETER_STRATEGIES: [(Strategy, &str); 3] = [
    (Strategy::Fast, "1"),
    (Strategy::Greedy, "3"),
    (Strategy::BinaryTreeOpt, "7"),
];

/// Parameter-sweep rows where this crate is *larger* than upstream, as
/// `(corpus, strategy code, case, our size, upstream's)`.
///
/// This replaced a byte-parity divergence set on 2026-08-06, and the swap is
/// instructive. That set had six entries, and in every one of them **this
/// crate's frame was the smaller of the two** -- `json-records` at btopt by 18
/// bytes, `wikipedia` at greedy by 117 to 155. Under a policy that forgoes
/// parity where diverging compresses better, those six were the sweep passing,
/// and enumerating them as divergences to be explained had it backwards.
///
/// Two further rows stood here and are gone, and what they turned out to be is
/// worth keeping. Both were excluded by the old no-matcher baseline gate, so
/// neither had ever been compared in any direction; both were `wikipedia` at
/// btopt with a hash-log clamping case, 6.3% and 7.7% over upstream against a
/// divergence set whose worst entry was 155 bytes.
///
/// The shared hash-log clamping looked like the lead and was not. What the two
/// cases actually shared is that they are the only ones that set `window_log`,
/// and the gap reproduced with **no matcher at all**: btopt at a window of 17
/// was 7.8% over upstream before long-distance matching was even switched on.
/// The cause was the binary tree's insert floor, and it is fixed --
/// `BinaryTreeFinder::insert_range` now bounds its traversal by
/// `ZSTD_getLowestMatchIndex(ms, target, windowLog)` as `ZSTD_insertBt1` does.
/// The whole family is byte-identical to upstream now, including at natural
/// levels 16 through 22, where it had been up to 10.93%.
///
/// The lesson to keep is the one about where to look: a gap that only shows
/// with a feature enabled is not thereby a gap *in* that feature. Turning the
/// feature off was one measurement and it moved the search to another file.
///
/// Asserted with both sizes, so closing a gap fails here and forces the entry
/// out, and so does a gap widening while staying a gap. Compared in corpus,
/// then strategy, then case order.
const LDM_PARAMETER_SIZE_GAPS: &[(&str, &str, &str, usize, usize)] = &[
    // Three bytes on 28 KB, 0.01%, and the price of the row-lazy repcode
    // substitution rather than a defect: strategy 3 is greedy, which is in the
    // band the substitution applies to. The substitution is worth 0.648% across
    // levels 5 to 12 and is not free everywhere; this is the one place in the
    // long-distance surface where it costs anything at all.
    ("wikipedia", "3", "derived-hash-log-clamped", 28524, 28521),
];

/// Rows of the parameter sweep where enabling the matcher changes nothing, so
/// all eight cases collapse onto the frame the parser produces on its own.
///
/// Recorded rather than forbidden, because a corpus that engages the matcher at
/// one window need not engage it at another and that is a fact about the corpus,
/// not a defect. What must not happen is a row *silently* becoming inert: the
/// sweep would still be green while testing nothing.
///
/// The three here are `log-lines` at the pinned window of 17, where the corpus
/// holds nothing at a range the parser's own window does not already reach. The
/// two cases that pin that window are therefore carried by `wikipedia` and
/// `json-records` alone. `repeated-chunk` used to sit in this sweep and was
/// inert at *every* window and strategy -- twenty-four rows of nothing -- which
/// is why it is now `wikipedia`.
const LDM_PARAMETER_INERT_ROWS: &[(&str, &str, Option<u32>)] = &[
    ("log-lines", "1", Some(LDM_PARAMETER_WINDOW_LOG)),
    ("log-lines", "3", Some(LDM_PARAMETER_WINDOW_LOG)),
    ("log-lines", "7", Some(LDM_PARAMETER_WINDOW_LOG)),
];

/// [`LDM_PARAMETER_CASES`] resolves to what it says it resolves to.
///
/// Every case's arithmetic — which branch of the derivation it reaches, and how
/// often the matcher inserts — is a subtraction from the window, and the window
/// is resolved rather than stated: enabling long-distance matching forces 27
/// and then fitting it to a megabyte of source brings it to 20. Two of these
/// cases were originally written against 27 and silently landed on the wrong
/// branch, passing all the while, because the parity sweep hands upstream the
/// same numbers and both sides then agree for the wrong reason.
///
/// So this asserts three things, none of which the parity sweep can:
///
/// 1. the window each case assumes is the window it gets;
/// 2. each case's `claim` about the *resolved* four holds, at every strategy
///    the sweep runs — the claim is where "this reaches the cap" stops being a
///    comment and becomes an assertion;
/// 3. the eight resolved tuples are pairwise distinct, bar the one deliberate
///    pair, so no case can quietly become a duplicate of another.
///
/// All of it is read off upstream, which is the side whose numbers the cases
/// are matched against.
#[test]
fn long_distance_parameters_resolve_what_their_cases_assume() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };
    let corpus = benchmark_corpora::benchmark_report_cases(1 << 20)
        .into_iter()
        .find(|corpus| corpus.name == "json-records")
        .expect("the sweep's first corpus");
    for (_, code) in LDM_PARAMETER_STRATEGIES {
        let mut resolved = Vec::with_capacity(LDM_PARAMETER_CASES.len());
        for case in LDM_PARAMETER_CASES {
            let applied = upstream_trace_helper::trace_advanced_applied_ldm_params(
                helper,
                upstream_trace_helper::DICT_NONE,
                &case.settings(code),
                &corpus.input,
            );
            assert_eq!(
                applied.enabled, UPSTREAM_PARAM_SWITCH_ENABLE,
                "{} at strategy {code} did not enable the matcher at all",
                case.name,
            );
            assert_eq!(
                applied.window_log,
                case.window_log.unwrap_or(LDM_DERIVED_WINDOW_LOG),
                "{} at strategy {code} resolved to a different window than its values assume",
                case.name,
            );
            assert!(
                (case.claim)(&applied),
                "{} at strategy {code} no longer reaches the branch it is named for: {applied:?}",
                case.name,
            );
            resolved.push((case.name, applied));
        }
        for (index, (name, applied)) in resolved.iter().enumerate() {
            for (other_name, other) in &resolved[index + 1..] {
                // The one deliberate pair: two spellings of one configuration,
                // which the sweep asserts agree byte for byte.
                if [*name, *other_name] == ["hash-log-supplied", "zero-rate-means-unset"] {
                    assert_eq!(applied, other, "the two spellings stopped agreeing");
                    continue;
                }
                assert_ne!(
                    applied, other,
                    "{name} and {other_name} resolve identically at strategy {code}, \
                     so one of them tests nothing",
                );
            }
        }
    }
}

/// Each of the four long-distance parameters produces upstream's exact bytes.
///
/// [`long_distance_matching_is_byte_identical_to_upstream`] leaves all four
/// unset, so it proves the *defaults* and nothing about the derivation that
/// arrives at them. That derivation is ordered and interdependent —
/// `hash_rate_log` feeds `hash_log`, which caps `bucket_size_log`, and the cap
/// is applied outside the "unset" check so that it lands on a supplied value
/// too — and every way of getting the order wrong yields a different table
/// shape, a different set of insertion points, and output that is valid,
/// decodable, and silently not upstream's.
///
/// The corpora are the three where enabling the matcher measurably changes the
/// frame at all three strategies, which was measured rather than assumed and is
/// re-measured on every run by [`LDM_PARAMETER_INERT_ROWS`]. `pseudorandom` and
/// `mixed-entropy` would agree with upstream by both finding nothing; so, less
/// obviously, did `repeated-chunk`, which sat here until that check was added.
///
/// Two assertions guard the sweep itself. The baseline is measured rather than
/// assumed, as in the default-shape sweep above, so that a row this crate
/// already differs on cannot be read as a long-distance defect. And each case
/// has to move the frame off the *default* long-distance shape somewhere in the
/// grid: a parameter that never reached the matcher would agree with upstream
/// on every row here, since upstream would be running the defaults too.
#[test]
fn long_distance_parameters_engage_and_are_measured_against_upstream() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };
    const SIZE: usize = 1 << 20;
    const CORPORA: [&str; 3] = ["wikipedia", "json-records", "log-lines"];
    let mut compared = 0usize;
    let mut larger = Vec::new();
    let mut moved_off_the_defaults = vec![false; LDM_PARAMETER_CASES.len()];
    let mut inert = Vec::new();

    for corpus in benchmark_corpora::benchmark_report_cases(SIZE) {
        if !CORPORA.contains(&corpus.name) {
            continue;
        }
        for (strategy, code) in LDM_PARAMETER_STRATEGIES {
            // Both of these are per window, because two cases pin one: a
            // baseline measured at a different window would answer a different
            // question, and so would a default shape.
            let mut baselines: Vec<(Option<u32>, bool, Vec<u8>)> = Vec::new();
            for case in LDM_PARAMETER_CASES {
                if baselines
                    .iter()
                    .any(|(window, ..)| *window == case.window_log)
                {
                    continue;
                }
                let plain = EncoderOptions {
                    compression_level: CompressionLevel::try_new(5).unwrap(),
                    parameters: ParameterOverrides {
                        window_log: case.window_log,
                        strategy: Some(strategy),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut settings =
                    vec!["compressionLevel=5".to_string(), format!("strategy={code}")];
                if let Some(window_log) = case.window_log {
                    settings.push(format!("windowLog={window_log}"));
                }
                let without_ldm = encode_all_with_options(&corpus.input, plain).unwrap();
                let exact = without_ldm
                    == upstream_trace_helper::compress_advanced(
                        helper,
                        upstream_trace_helper::DICT_NONE,
                        &settings,
                        &corpus.input,
                    );

                // The same window with all four parameters left to derive, so
                // that "moved off the defaults" below compares like with like.
                // Never passed to the claim check, so its claim is vacuous.
                let defaults = LdmParameterCase {
                    window_log: case.window_log,
                    ..LdmParameterCase::named("defaults", |_| true)
                };
                let shape = encode_all_with_options(
                    &corpus.input,
                    EncoderOptions {
                        compression_level: CompressionLevel::try_new(5).unwrap(),
                        parameters: defaults.overrides(strategy),
                        ..Default::default()
                    },
                )
                .unwrap();

                // A corpus, strategy and window where enabling the matcher
                // changes nothing is a row that cannot fail: all eight cases
                // reproduce the no-matcher frame, agree with upstream doing the
                // same, and prove nothing about any parameter. `repeated-chunk`
                // was exactly this -- a megabyte of a 46-byte chunk leaves the
                // matcher nothing the parser has not already taken -- and it sat
                // here as eight inert rows per strategy until this caught it.
                if shape == without_ldm {
                    inert.push((corpus.name, code, case.window_log));
                }
                baselines.push((case.window_log, exact, shape));
            }

            let mut spelled_with_a_zero_rate: Option<Vec<u8>> = None;
            for (index, case) in LDM_PARAMETER_CASES.iter().enumerate() {
                let (_, _baseline_exact, default_shape) = baselines
                    .iter()
                    .find(|(window, ..)| *window == case.window_log)
                    .expect("every case's window was measured above");
                let options = EncoderOptions {
                    compression_level: CompressionLevel::try_new(5).unwrap(),
                    parameters: case.overrides(strategy),
                    ..Default::default()
                };
                let ours = encode_all_with_options(&corpus.input, options).unwrap();
                assert_eq!(
                    decode_all(&ours).unwrap(),
                    corpus.input,
                    "{} {strategy:?} {} did not round-trip",
                    corpus.name,
                    case.name,
                );
                // The two spellings of one configuration, which have to agree
                // with each other and not only with upstream.
                match case.name {
                    "hash-log-supplied" => spelled_with_a_zero_rate = Some(ours.clone()),
                    "zero-rate-means-unset" => assert_eq!(
                        Some(&ours),
                        spelled_with_a_zero_rate.as_ref(),
                        "{} {strategy:?}: a rate of zero was not read as unset",
                        corpus.name,
                    ),
                    _ => {}
                }

                // No longer below a baseline gate: a row excluded for failing to
                // match upstream could not previously be the evidence that a
                // parameter had an effect, and that exclusion grew with every
                // deliberate divergence. Every row carries the claim now.
                moved_off_the_defaults[index] |= &ours != default_shape;

                let theirs = upstream_trace_helper::compress_advanced(
                    helper,
                    upstream_trace_helper::DICT_NONE,
                    &case.settings(code),
                    &corpus.input,
                );
                if ours.len() > theirs.len() {
                    larger.push((corpus.name, code, case.name, ours.len(), theirs.len()));
                }
                compared += 1;
            }
        }
    }

    for (index, moved) in moved_off_the_defaults.iter().enumerate() {
        assert!(
            moved,
            "{} matched the default long-distance shape on every row, so the \
             sweep would pass with the parameter ignored",
            LDM_PARAMETER_CASES[index].name,
        );
    }
    assert_eq!(
        inert, LDM_PARAMETER_INERT_ROWS,
        "the set of rows where enabling the matcher changes nothing moved; a \
         row that gained one is now testable, a row that lost one is a corpus \
         that stopped exercising the matcher and needs replacing"
    );
    assert_eq!(
        larger, LDM_PARAMETER_SIZE_GAPS,
        "the set of supplied-parameter rows larger than upstream changed"
    );
    assert_eq!(
        compared,
        CORPORA.len() * LDM_PARAMETER_STRATEGIES.len() * LDM_PARAMETER_CASES.len(),
        "the sweep did not cover its grid",
    );
}

/// Long-distance matching in a *stream* costs no more against upstream's own
/// streaming encoder than the same stream without it does.
///
/// The window is pinned narrow so the frame outgrows it: at 512 KiB of history
/// this encoder compacts its buffer three times over a two-megabyte corpus,
/// every position the long-distance table holds is an index into that buffer,
/// and none of that happens in a one-shot frame however large. Left at the
/// window long-distance matching forces, a corpus would have to be a quarter of
/// a gigabyte to reach the first compaction.
///
/// Size rather than bytes, and relative to a measured baseline rather than
/// absolute. Two streaming frames of the same input are not the same artifact
/// as two one-shot frames: block layout, the pre-split heuristic and this
/// encoder's own match-state handling across a compaction all differ from
/// upstream's, and every one of those differences is already present with the
/// matcher switched off. Dividing the two ratios is what leaves only what
/// long-distance matching itself contributed. The worst row of the thirty-six
/// gives up 0.41% of its baseline, against a bound of 1%.
///
/// Rows streamed larger than upstream's own streamed frame with the matcher on,
/// as `(corpus, strategy code, ours, theirs)`.
///
/// Two, and they are different in kind. `json-records` at fast is five bytes on
/// 231 KB, a tie. `binary-structured` at fast is 27.8% -- and is *not* a
/// long-distance finding: our frame is 1.2778 of upstream's with the matcher
/// and 1.2778 of it without, the same figure to four places, because what
/// diverges there is upstream's own streaming path against its own one-shot
/// path rather than ours against either. Comparing each side against itself is
/// what showed that, and it is why this list records the matcher-on sizes only
/// as a tripwire, not as an attribution.
const LDM_STREAMING_SIZE_GAPS: &[(&str, &str, usize, usize)] = &[
    ("json-records", "1", 231253, 231248),
    ("binary-structured", "1", 407788, 319137),
];

/// Several rows land within a handful of bytes of upstream's streaming frame:
/// `json-records` at greedy is one byte apart, and `mixed-entropy` at fast is
/// exact. Byte parity is not asserted because the streaming paths already
/// diverge without the matcher.
#[test]
fn long_distance_matching_streams_at_upstreams_size() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };
    const SIZE: usize = 2 << 20;
    const PIECE: usize = 32 << 10;
    const WINDOW_LOG: u32 = 19;
    let cases = [
        "json-records",
        "log-lines",
        "mixed-entropy",
        "binary-structured",
    ];
    let mut compared = 0usize;
    let mut larger = Vec::new();

    for corpus in benchmark_corpora::benchmark_report_cases(SIZE) {
        if !cases.contains(&corpus.name) {
            continue;
        }
        for (strategy, code) in LDM_SWEEP_STRATEGIES {
            let ours = |ldm| {
                stream_encode(
                    &corpus.input,
                    EncoderOptions {
                        compression_level: CompressionLevel::try_new(5).unwrap(),
                        parameters: ParameterOverrides {
                            strategy: Some(strategy),
                            window_log: Some(WINDOW_LOG),
                            long_distance_matching: ldm,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    PIECE,
                )
            };
            let theirs = |ldm: bool| {
                let mut settings = vec![
                    "compressionLevel=5".to_string(),
                    format!("strategy={code}"),
                    format!("windowLog={WINDOW_LOG}"),
                ];
                if ldm {
                    settings.push("enableLongDistanceMatching=1".to_string());
                }
                upstream_trace_helper::compress_advanced_streaming(
                    helper,
                    upstream_trace_helper::DICT_NONE,
                    PIECE,
                    &settings,
                    &corpus.input,
                )
            };

            let with_ldm = ours(zstandard::LdmMode::Enabled);
            assert_eq!(
                decode_all(&with_ldm).unwrap(),
                corpus.input,
                "{} {strategy:?} did not round-trip",
                corpus.name
            );

            let without_ldm = ours(zstandard::LdmMode::Disabled);
            let (their_ldm, their_plain) = (theirs(true), theirs(false));

            // The sharp half, and the one a ratio cannot express: at this
            // window long-distance matching is often a *loss*, and on a corpus
            // where it finds nothing at all it must leave the frame alone. So
            // the frame has to change exactly where upstream's does. A matcher
            // that had quietly stopped running would pass any bound that only
            // asks us not to grow.
            assert_eq!(
                with_ldm != without_ldm,
                their_ldm != their_plain,
                "{} {strategy:?}: long-distance matching changed our stream \
                 ({} against {} bytes) where upstream's went {} against {}",
                corpus.name,
                with_ldm.len(),
                without_ldm.len(),
                their_ldm.len(),
                their_plain.len(),
            );

            // This was `ratio <= baseline * 1.01`, comparing how far under
            // upstream we sit with the matcher against how far under we sit
            // without it. It reads as a fair relative bound and is not one: as
            // this crate improves on upstream's *baseline* parse, the two ratios
            // separate for reasons that have nothing to do with the matcher.
            // With the row-lazy repcode substitution applied it fails on
            // `json-records` at greedy -- 0.9520 of upstream's size with the
            // matcher against 0.8406 without -- while our frame is smaller than
            // upstream's on both sides of that comparison. Rearranged it is the
            // parity gate again, and it decays the same way.
            if with_ldm.len() > their_ldm.len() {
                larger.push((corpus.name, code, with_ldm.len(), their_ldm.len()));
            }
            compared += 1;
        }
    }
    assert_eq!(
        larger, LDM_STREAMING_SIZE_GAPS,
        "the set of streamed long-distance rows larger than upstream changed"
    );
    assert_eq!(
        compared,
        cases.len() * 9,
        "the sweep did not cover its grid"
    );
}

/// Every shape a greedy parse can take in front of a dictionary, held to
/// upstream's compressed size.
///
/// Greedy is the one depth where a repeat match is not weighed against the
/// search result. `ZSTD_compressBlock_lazy_generic` checks the repeat at `ip+1`
/// before it searches and leaves the loop the moment it hits -- `if (depth==0)
/// goto _storeSequence;` at `zstd_lazy.c:1597`, and again at `:1995` for the
/// ext-dict variant -- so the search never runs and cannot overrule it. Three
/// of this crate's five prefixed parse paths were searching anyway and taking
/// whichever match was longer, which is not the same choice: a repeat spends
/// one offset code where an explicit offset spends its own bits, so the longer
/// match is often the more expensive sequence.
///
/// Held as a ratio and not as byte equality because these paths carry
/// divergences that predate this and are not all in our favour. What the bound
/// has to be is *non-vacuous*, and it is: with the three paths comparing rather
/// than short-circuiting, the worst row here was 1.1631 and eight others sat
/// above the ceiling; with them fixed the worst is 1.0048, on a 414-byte frame
/// two bytes larger than upstream's, which is the same row and the same two
/// bytes either way.
///
/// The shape assertion at the end is the other half. A dictionary parse varies
/// along three axes that are not independent of the level -- chain against row
/// finder, ext-dict against attached dict-match-state, and a prefix chain
/// against prepared tables -- and which one a case lands on moves with the
/// corpus size and the dictionary. Naming the sizes rather than the shapes
/// means a change to either resolution could quietly empty this out, so the
/// shapes it reached when it was written are pinned.
#[test]
fn greedy_in_front_of_a_dictionary_takes_the_depth_zero_repeat() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    // Level 4 resolves to greedy under every dictionary here, which the loop
    // asserts rather than assumes.
    const LEVEL: i32 = 4;
    const CEILING: f64 = 1.01;
    // 1 KiB attaches the dictionary, 64 KiB is past every attach cutoff, and
    // 8 KiB straddles: the trained dictionary still attaches there and the raw
    // one does not.
    const SIZES: [usize; 3] = [1024, 8 * 1024, 64 * 1024];
    const CORPORA: [&str; 4] = ["json-records", "log-lines", "tabular-csv", "wikipedia"];

    let raw_dictionary = upstream_trace_helper::emit_raw_dictionary(helper);
    let trained_dictionary = upstream_trace_helper::emit_trained_dictionary(helper);
    let raw_prepared = EncoderDictionary::new(&raw_dictionary).unwrap();
    let trained_prepared = EncoderDictionary::new(&trained_dictionary).unwrap();

    let mut shapes = std::collections::BTreeSet::new();
    let mut over_ceiling = Vec::new();
    let mut worst = (0.0f64, String::new());
    let mut compared = 0usize;

    for size in SIZES {
        for corpus in benchmark_corpora::benchmark_report_cases(size) {
            if !CORPORA.contains(&corpus.name) {
                continue;
            }
            for (dict_mode, prepared) in [
                (upstream_trace_helper::DICT_RAW, &raw_prepared),
                (upstream_trace_helper::DICT_TRAINED, &trained_prepared),
            ] {
                for (mode, switch) in [
                    (RowMatchFinderMode::Enabled, 1u32),
                    (RowMatchFinderMode::Disabled, 2u32),
                ] {
                    let theirs = upstream_trace_helper::compress_advanced(
                        helper,
                        dict_mode,
                        &upstream_settings_for(LEVEL, &[format!("useRowMatchFinder={switch}")]),
                        &corpus.input,
                    );
                    let options = EncoderOptions {
                        compression_level: CompressionLevel::try_new(LEVEL).unwrap(),
                        parameters: ParameterOverrides {
                            use_row_match_finder: mode,
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    let ours =
                        encode_all_with_prepared_dict_and_options(&corpus.input, prepared, options)
                            .unwrap();
                    let parameters = trace_first_block_with_prepared_dict_and_options(
                        &corpus.input,
                        prepared,
                        options,
                    )
                    .unwrap()
                    .compression_parameters;

                    let row = format!(
                        "{} {size} {dict_mode} {mode:?} {:?}/{:?}/{:?}",
                        corpus.name,
                        parameters.parser_strategy,
                        parameters.dictionary_mode,
                        parameters.dict_table_source,
                    );
                    assert!(
                        matches!(
                            parameters.parser_strategy,
                            BlockTraceParserStrategy::Greedy | BlockTraceParserStrategy::GreedyRow
                        ),
                        "{row} is not a greedy parse, so it no longer tests what this is for"
                    );
                    shapes.insert(format!(
                        "{:?}/{:?}/{:?}",
                        parameters.parser_strategy,
                        parameters.dictionary_mode,
                        parameters.dict_table_source,
                    ));

                    let ratio = ours.len() as f64 / theirs.len() as f64;
                    if ratio > worst.0 {
                        worst = (ratio, row.clone());
                    }
                    if ratio > CEILING {
                        over_ceiling.push(format!(
                            "{row}: {} vs {} ({ratio:.4})",
                            ours.len(),
                            theirs.len()
                        ));
                    }
                    compared += 1;
                }
            }
        }
    }

    assert!(
        over_ceiling.is_empty(),
        "greedy in front of a dictionary compressed worse than {CEILING:.2}x upstream:\n  {}",
        over_ceiling.join("\n  ")
    );
    assert_eq!(
        compared,
        SIZES.len() * CORPORA.len() * 4,
        "the grid is short"
    );
    let expected: std::collections::BTreeSet<String> = [
        "Greedy/DictMatchState/Prepared",
        "Greedy/ExtDict/Prefix",
        "Greedy/ExtDict/Prepared",
        "GreedyRow/DictMatchState/Prepared",
        "GreedyRow/ExtDict/Prefix",
        "GreedyRow/ExtDict/Prepared",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(
        shapes, expected,
        "the grid stopped reaching the dictionary shapes it was written for; worst row was {} at {:.4}",
        worst.1, worst.0
    );
}

/// The dictionary rows of `tests/baseline.rs`, held against upstream.
///
/// That grid builds its own dictionaries so it can run with no helper present,
/// and records only our own sizes. The gap that leaves is not small: its
/// dictionaries are far larger than this file's -- 112 KiB of raw content
/// against 156 bytes -- and a CDict resolves its own cparams, so the same level
/// lands on a different strategy and a different window. Nothing here could
/// reach those rows, and a movement in them could only ever be read as "ours
/// changed", never as "ours moved towards upstream" or away from it.
///
/// That mattered the first time a change moved them. Making greedy take its
/// depth-0 repeat gave up a 13% margin over upstream on `wikipedia` while
/// winning more than that back elsewhere, and deciding whether the trade was
/// worth taking needed upstream's own numbers for the affected rows. This holds
/// what that measurement found, so the margin cannot quietly erode later.
///
/// One-shot and streamed both, because the baseline records both and they are
/// not the same frame: upstream's `ZSTD_compress_frameChunk` declines to
/// pre-split a chunk below 128 KiB, so a streamed frame carries a different
/// block layout from a one-shot one over the same bytes.
#[test]
fn the_baseline_dictionary_rows_stay_at_or_under_upstream() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    // `tests/baseline.rs`'s own constants: its dictionary, its corpus size, its
    // streaming piece and the narrow window its streamed rows declare.
    const SIZE: usize = 256 * 1024;
    const PIECE: usize = 48 * 1024;
    const NARROW_WINDOW_LOG: u32 = 16;
    // The levels a dictionary puts on the greedy and lazy families here. Below
    // this is the fast pair and above it the binary trees, neither of which has
    // the depth-0 choice this is watching.
    const LEVELS: [i32; 3] = [4, 5, 6];
    const CORPORA: [&str; 4] = ["json-records", "log-lines", "tabular-csv", "wikipedia"];
    // Every row is now within 0.03% of upstream either way, the worst being a
    // streamed `tabular-csv` row at 1.0003, so the ceiling is close enough to
    // that to be worth tripping over. It was 1.005 while the `DoubleFast`
    // attach gap below was open.
    const CEILING: f64 = 1.001;
    /// Rows allowed above the ceiling, recorded rather than dissolved into a
    /// looser bound.
    ///
    /// Empty, and the assertion below keeps it that way: a row named here that
    /// comes in under the ceiling fails just as loudly as one that goes over.
    /// It last held `tabular-csv L4 one-shot`, the only row here a dictionary
    /// puts on `DoubleFast` rather than the greedy family, which measured 51814
    /// against upstream's 50406 until the `ip+2` rung came out of the
    /// double-fast attach path.
    const KNOWN_GAPS: [&str; 0] = [];

    let dictionary = benchmark_corpora::build_raw_dictionary_input(112 * 1024);
    let prepared = EncoderDictionary::new(&dictionary).unwrap();

    let mut over_ceiling = Vec::new();
    let mut closed_gaps = Vec::new();
    let mut compared = 0usize;
    // `KNOWN_GAPS` names rows, so the ceiling test is a closure over one. It
    // borrows both lists, so it and the loop live in a block that ends before
    // they are read.
    {
        let mut judge = |row: String, ours: usize, theirs: usize| {
            let ratio = ours as f64 / theirs as f64;
            match (KNOWN_GAPS.contains(&row.as_str()), ratio > CEILING) {
                (false, true) => {
                    over_ceiling.push(format!("{row}: {ours} vs {theirs} ({ratio:.4})"))
                }
                (true, false) => {
                    closed_gaps.push(format!("{row}: {ours} vs {theirs} ({ratio:.4})"))
                }
                _ => {}
            }
        };

        for corpus in benchmark_corpora::benchmark_report_cases(SIZE) {
            if !CORPORA.contains(&corpus.name) {
                continue;
            }
            for level in LEVELS {
                let one_shot_options = EncoderOptions {
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    ..Default::default()
                };
                let ours = encode_all_with_prepared_dict_and_options(
                    &corpus.input,
                    &prepared,
                    one_shot_options,
                )
                .unwrap();
                let theirs = upstream_trace_helper::compress_advanced_with_dict(
                    helper,
                    &dictionary,
                    &upstream_settings_for(level, &[]),
                    &corpus.input,
                );
                judge(
                    format!("{} L{level} one-shot", corpus.name),
                    ours.len(),
                    theirs.len(),
                );
                compared += 1;

                let streamed_options = EncoderOptions {
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    parameters: ParameterOverrides {
                        window_log: Some(NARROW_WINDOW_LOG),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut encoder =
                    StreamingEncoder::with_prepared_dict(&prepared, streamed_options).unwrap();
                let mut streamed = Vec::new();
                for chunk in corpus.input.chunks(PIECE) {
                    encoder.push(chunk).unwrap();
                    streamed.extend_from_slice(&encoder.take_output());
                }
                encoder.finish().unwrap();
                streamed.extend_from_slice(&encoder.take_output());
                let their_stream = upstream_trace_helper::compress_advanced_streaming_with_dict(
                    helper,
                    &dictionary,
                    PIECE,
                    &upstream_settings_for(level, &[format!("windowLog={NARROW_WINDOW_LOG}")]),
                    &corpus.input,
                );
                judge(
                    format!("{} L{level} streamed", corpus.name),
                    streamed.len(),
                    their_stream.len(),
                );
                compared += 1;
            }
        }
    }
    assert!(
        over_ceiling.is_empty(),
        "baseline dictionary rows compressed worse than {CEILING:.3}x upstream:\n  {}",
        over_ceiling.join("\n  ")
    );
    assert!(
        closed_gaps.is_empty(),
        "these rows are recorded in KNOWN_GAPS but now come in under {CEILING:.3}x; \
         drop them from the list:\n  {}",
        closed_gaps.join("\n  ")
    );
    assert_eq!(
        compared,
        CORPORA.len() * LEVELS.len() * 2,
        "the grid is short"
    );
}

/// Every `DoubleFast` parse a dictionary reaches, held against upstream.
///
/// `the_baseline_dictionary_rows_stay_at_or_under_upstream` above found one of
/// these by accident and could say nothing about the rest: it runs one corpus
/// set at three levels because that is what the baseline grid runs. Sweeping
/// the parser instead of the grid turned one 1.03x row into eleven, the worst
/// at 1.14x, and every one of them on the attach path -- `DictMatchState`, the
/// mode where the dictionary is searched alongside the source rather than
/// copied in front of it. Not one `ExtDict` row was over.
///
/// The cause was a rung of the match ladder that upstream does not have; see
/// `plan_sequences_double_fast_with_prepared_dict_inner`. This is the sweep
/// that found it, kept so the shape cannot come back.
///
/// Levels 1 to 5 rather than the double-fast levels of a plain compression,
/// because a CDict resolves its own parameters: the same level lands on a
/// different strategy depending on how large the dictionary is, so which rows
/// are `DoubleFast` is not knowable up front. Each row is traced and skipped
/// unless it really is one, and the shapes are counted at the end so a
/// resolution change cannot quietly empty the grid.
#[test]
fn every_double_fast_row_a_dictionary_reaches_stays_at_or_under_upstream() {
    let Some(helper) = upstream_trace_helper::helper_path() else {
        return;
    };

    const SIZES: [usize; 2] = [64 * 1024, 256 * 1024];
    const LEVELS: [i32; 5] = [1, 2, 3, 4, 5];
    /// Worst measured is 1.0040, on a 64 KiB `wikipedia` row against the small
    /// dictionary. Two rows sit between 1.0035 and that; everything else is at
    /// or under upstream.
    const CEILING: f64 = 1.005;
    /// The parse shapes these rows reach, as `(dictionary mode, table source)`
    /// counts. A dictionary can put `DoubleFast` on either of two, and both are
    /// here: the raw dictionaries take the attach path with tables built for
    /// the compression, the trained one is copied in front of the source with
    /// tables cached on the dictionary. Pinned because a row that stopped being
    /// `DoubleFast` would leave the grid silently rather than fail.
    const SHAPES: [(&str, usize); 2] = [("DictMatchState/Prefix", 66), ("ExtDict/Prepared", 44)];

    // Two raw dictionaries three orders of magnitude apart and one trained on
    // the larger of them. Size is what decides the parse here -- it moves the
    // window and the strategy the level resolves to -- so a single dictionary
    // would sweep one column of the grid and call it the grid.
    let big_raw = benchmark_corpora::build_raw_dictionary_input(112 * 1024);
    let small_raw = benchmark_corpora::build_raw_dictionary_input(2 * 1024);
    let big_trained = {
        let samples: Vec<&[u8]> = big_raw.chunks(4096).collect();
        zstandard::train_dictionary(&samples, 16 * 1024).unwrap()
    };
    let dictionaries: [(&str, &[u8]); 3] = [
        ("big-raw", &big_raw),
        ("big-trained", &big_trained),
        ("small-raw", &small_raw),
    ];

    let mut over_ceiling = Vec::new();
    let mut shapes: BTreeMap<String, usize> = BTreeMap::new();
    let mut total_ours = 0usize;
    let mut total_theirs = 0usize;
    for size in SIZES {
        for corpus in benchmark_corpora::benchmark_report_cases(size) {
            for (dictionary_name, dictionary) in dictionaries {
                let prepared = EncoderDictionary::new(dictionary).unwrap();
                for level in LEVELS {
                    let options = EncoderOptions {
                        compression_level: CompressionLevel::try_new(level).unwrap(),
                        ..Default::default()
                    };
                    let trace = trace_first_block_with_prepared_dict_and_options(
                        &corpus.input,
                        &prepared,
                        options,
                    )
                    .unwrap();
                    if trace.compression_parameters.parser_strategy
                        != BlockTraceParserStrategy::DoubleFast
                    {
                        continue;
                    }
                    *shapes
                        .entry(format!(
                            "{:?}/{:?}",
                            trace.compression_parameters.dictionary_mode,
                            trace.compression_parameters.dict_table_source
                        ))
                        .or_default() += 1;

                    let ours = encode_all_with_prepared_dict_and_options(
                        &corpus.input,
                        &prepared,
                        options,
                    )
                    .unwrap();
                    let theirs = upstream_trace_helper::compress_advanced_with_dict(
                        helper,
                        dictionary,
                        &upstream_settings_for(level, &[]),
                        &corpus.input,
                    );
                    total_ours += ours.len();
                    total_theirs += theirs.len();
                    let ratio = ours.len() as f64 / theirs.len() as f64;
                    if ratio > CEILING {
                        over_ceiling.push(format!(
                            "{} {size} {dictionary_name} L{level}: {} vs {} ({ratio:.4})",
                            corpus.name,
                            ours.len(),
                            theirs.len()
                        ));
                    }
                }
            }
        }
    }

    assert!(
        over_ceiling.is_empty(),
        "double-fast dictionary rows compressed worse than {CEILING:.3}x upstream:\n  {}",
        over_ceiling.join("\n  ")
    );
    // Row by row the grid is a bound; in total it is a claim, and the claim is
    // that this parser is not paying for its dictionary. The margin was 0.12%
    // the wrong way before the `ip+2` rung came out.
    assert!(
        total_ours <= total_theirs,
        "double-fast dictionary rows total {total_ours} against upstream's {total_theirs}"
    );
    let found: Vec<(&str, usize)> = shapes
        .iter()
        .map(|(shape, count)| (shape.as_str(), *count))
        .collect();
    assert_eq!(found, SHAPES, "the parse shapes this grid reaches moved");
}
