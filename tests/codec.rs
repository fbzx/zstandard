use zstandard::{
    BlockType, CompressionLevel, Decoder, DecoderDictionary, DecoderOptions, Encoder,
    EncoderDictionary, EncoderOptions, Error, Format, FrameHeader, LdmMode, ParameterBounds,
    ParameterOverrides, RowMatchFinderMode, Strategy, StreamingDecoder, StreamingEncoder,
    decode_all, decode_all_with_dict, decode_all_with_options, decode_all_with_prepared_dict,
    encode_all, encode_all_with_dict, encode_all_with_dict_and_options, encode_all_with_options,
    encode_all_with_prepared_dict, encode_all_with_prepared_dict_and_options, parse_block_header,
    parse_frame_header, parse_frame_header_with_format, write_skippable_frame,
};

#[allow(dead_code)]
#[path = "../src/support/corpora.rs"]
mod benchmark_corpora;

#[test]
fn roundtrips_raw_frames_across_multiple_blocks() {
    let data = build_pattern(160_000);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            block_size: 64 * 1024,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();
    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

#[test]
fn emits_rle_blocks_for_repeated_chunks() {
    let data = vec![0xAB; 4096];
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            block_size: 4096,
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();

    let header = parse_frame_header(&encoded).unwrap();
    let header_size = match header {
        FrameHeader::Zstandard(header) => header.header_size,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let block = parse_block_header(&encoded[header_size..]).unwrap();
    assert_eq!(block.block_type, BlockType::Rle);

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

#[test]
fn tiny_repeated_blocks_below_upstream_min_cblock_size_stay_raw() {
    let raw_data = vec![0xAB; 6];
    let raw_encoded = encode_all_with_options(
        &raw_data,
        EncoderOptions {
            block_size: raw_data.len(),
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();

    let raw_header = parse_frame_header(&raw_encoded).unwrap();
    let raw_header_size = match raw_header {
        FrameHeader::Zstandard(header) => header.header_size,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let raw_block = parse_block_header(&raw_encoded[raw_header_size..]).unwrap();
    assert_eq!(raw_block.block_type, BlockType::Raw);
    assert_eq!(decode_all(&raw_encoded).unwrap(), raw_data);

    let rle_data = vec![0xAB; 7];
    let rle_encoded = encode_all_with_options(
        &rle_data,
        EncoderOptions {
            block_size: rle_data.len(),
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();

    let rle_header = parse_frame_header(&rle_encoded).unwrap();
    let rle_header_size = match rle_header {
        FrameHeader::Zstandard(header) => header.header_size,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let rle_block = parse_block_header(&rle_encoded[rle_header_size..]).unwrap();
    assert_eq!(rle_block.block_type, BlockType::Rle);
    assert_eq!(decode_all(&rle_encoded).unwrap(), rle_data);
}

#[test]
fn keeps_raw_blocks_for_incompressible_chunks() {
    let data = build_incompressible_pattern(4096);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            block_size: 4096,
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();

    let header = parse_frame_header(&encoded).unwrap();
    let header_size = match header {
        FrameHeader::Zstandard(header) => header.header_size,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let block = parse_block_header(&encoded[header_size..]).unwrap();
    assert_eq!(block.block_type, BlockType::Raw);

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

#[test]
fn decodes_errata_empty_compressed_block_example() {
    let frame = decode_hex_bytes("28b52ffd20001500000000");
    assert_eq!(decode_all(&frame).unwrap(), Vec::<u8>::new());
}

#[test]
fn emits_compressed_literals_blocks_for_huff_friendly_chunks() {
    let data = build_huff_friendly_pattern(12_000);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            block_size: data.len(),
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();

    let header = parse_frame_header(&encoded).unwrap();
    let header_size = match header {
        FrameHeader::Zstandard(header) => header.header_size,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let block = parse_block_header(&encoded[header_size..]).unwrap();
    assert_eq!(block.block_type, BlockType::Compressed);

    let payload_start = header_size + 3;
    let payload_end = payload_start + block.block_size as usize;
    let payload = &encoded[payload_start..payload_end];
    assert_eq!(
        payload[0] & 0x3,
        2,
        "expected a compressed literals section"
    );

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

#[test]
fn later_blocks_reuse_prior_huffman_tables_for_treeless_literals() {
    let block_size = 8 * 1024;
    let data = build_huff_friendly_pattern(block_size * 2);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            block_size,
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();

    let blocks = parse_frame_blocks(&encoded);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].block_type, BlockType::Compressed);
    assert_eq!(blocks[1].block_type, BlockType::Compressed);

    let second_payload = &encoded[blocks[1].payload_start..blocks[1].payload_end];
    assert_eq!(
        second_payload[0] & 0x3,
        3,
        "expected the second block to emit treeless literals"
    );

    assert_eq!(decode_all(&encoded).unwrap(), data);
}

#[test]
fn emits_sequence_compressed_blocks_for_repeated_chunks() {
    let data = build_repeated_chunk_pattern(24_000);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            block_size: data.len(),
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();

    let header = parse_frame_header(&encoded).unwrap();
    let header_size = match header {
        FrameHeader::Zstandard(header) => header.header_size,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let block = parse_block_header(&encoded[header_size..]).unwrap();
    assert_eq!(block.block_type, BlockType::Compressed);

    let payload_start = header_size + 3;
    let payload_end = payload_start + block.block_size as usize;
    let payload = &encoded[payload_start..payload_end];
    assert!(
        compressed_block_sequence_count(payload) > 0,
        "expected the block to contain real sequences"
    );

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

#[test]
fn later_blocks_use_prior_frame_history_for_matches() {
    let block = build_incompressible_pattern(64 * 1024);
    let mut data = block.clone();
    data.extend_from_slice(&block);

    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            block_size: block.len(),
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();

    let blocks = parse_frame_blocks(&encoded);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[1].block_type, BlockType::Compressed);

    let payload = &encoded[blocks[1].payload_start..blocks[1].payload_end];
    assert!(
        compressed_block_sequence_count(payload) > 0,
        "expected the second block to reuse prior frame history"
    );

    assert_eq!(decode_all(&encoded).unwrap(), data);
}

#[test]
fn dictionary_frames_keep_prior_block_history_after_the_first_block() {
    let dictionary = raw_test_dictionary();
    let block = build_incompressible_pattern(64 * 1024);
    let mut data = block.clone();
    data.extend_from_slice(&block);

    let encoded_with_dict = zstandard::encode_all_with_dict_and_options(
        &data,
        dictionary,
        EncoderOptions {
            block_size: block.len(),
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();

    let dict_blocks = parse_frame_blocks(&encoded_with_dict);
    assert_eq!(dict_blocks.len(), 2);
    assert_eq!(dict_blocks[1].block_type, BlockType::Compressed);

    let dict_payload = &encoded_with_dict[dict_blocks[1].payload_start..dict_blocks[1].payload_end];
    assert!(
        compressed_block_sequence_count(dict_payload) > 0,
        "expected the dictionary path to keep prior block history"
    );

    assert_eq!(
        decode_all_with_dict(&encoded_with_dict, dictionary).unwrap(),
        data
    );
}

#[test]
fn dictionary_remains_available_to_later_blocks() {
    let dictionary = raw_test_dictionary();
    let block1 = build_incompressible_pattern(64 * 1024);
    let block2 = build_dictionary_echo_pattern(dictionary, 48 * 1024);
    let mut data = block1.clone();
    data.extend_from_slice(&block2);

    let encoded = zstandard::encode_all_with_dict_and_options(
        &data,
        dictionary,
        EncoderOptions {
            block_size: block1.len(),
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();

    let blocks = parse_frame_blocks(&encoded);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[1].block_type, BlockType::Compressed);

    let payload = &encoded[blocks[1].payload_start..blocks[1].payload_end];
    assert!(
        compressed_block_sequence_count(payload) > 0,
        "expected the second block to keep matching against the dictionary",
    );

    assert_eq!(decode_all_with_dict(&encoded, dictionary).unwrap(), data);
}

#[test]
fn rejects_out_of_range_compression_levels() {
    let message = Error::InvalidParameter("compression_level must be in -131072..=22");
    assert_eq!(CompressionLevel::try_from(23u8).unwrap_err(), message);
    assert_eq!(CompressionLevel::try_new(23).unwrap_err(), message);
    assert_eq!(CompressionLevel::try_new(i32::MAX).unwrap_err(), message);

    // Upstream clamps below its floor rather than reporting; this rejects, so
    // that a caller who names a level nobody implements hears about it instead
    // of silently getting a different one.
    assert_eq!(
        CompressionLevel::try_new(CompressionLevel::MIN.as_i32() - 1).unwrap_err(),
        message
    );
    assert_eq!(CompressionLevel::try_new(i32::MIN).unwrap_err(), message);

    // `0` is in range: upstream maps it to the default level, and
    // `level_zero_is_the_default_level` in the interop suite pins that it
    // encodes identically to level 3.
    assert_eq!(CompressionLevel::try_new(0).unwrap().as_i32(), 0);
    assert_eq!(
        CompressionLevel::try_new(CompressionLevel::MIN.as_i32())
            .unwrap()
            .as_i32(),
        -131_072
    );
}

#[test]
fn higher_compression_levels_improve_structured_text_ratio() {
    let data = build_structured_log_pattern(24_000);

    let fast = encode_all_with_options(
        &data,
        EncoderOptions {
            block_size: data.len(),
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::FASTEST,
            ..Default::default()
        },
    )
    .unwrap();
    let strong = encode_all_with_options(
        &data,
        EncoderOptions {
            block_size: data.len(),
            checksum: false,
            write_dict_id: true,
            compression_level: CompressionLevel::BEST,
            ..Default::default()
        },
    )
    .unwrap();

    let fast_counts = compressed_block_sequence_counts(&fast);
    let strong_counts = compressed_block_sequence_counts(&strong);
    assert!(
        fast_counts.iter().any(|&count| count > 0),
        "expected the fast level to still emit sequences: {fast_counts:?}"
    );
    assert!(
        strong_counts.iter().any(|&count| count > 0),
        "expected the strong level to emit sequences: {strong_counts:?}"
    );
    assert!(
        strong.len() < fast.len(),
        "expected stronger level to compress this structured-text corpus better: fast={} strong={}",
        fast.len(),
        strong.len()
    );

    assert_eq!(decode_all(&fast).unwrap(), data);
    assert_eq!(decode_all(&strong).unwrap(), data);
}

#[test]
fn roundtrips_raw_dictionary_compressed_frames() {
    let dictionary = raw_test_dictionary();
    let data = build_dictionary_echo_pattern(dictionary, 24_000);

    let encoded = encode_all_with_dict(&data, dictionary).unwrap();

    let decoded = decode_all_with_dict(&encoded, dictionary).unwrap();
    assert_eq!(decoded, data);
    assert!(
        decode_all(&encoded).is_err(),
        "raw-dictionary-compressed output unexpectedly decoded without the dictionary",
    );
}

/// Bodies and dictionaries shorter than the match finder's hash key.
///
/// A block of one to three bytes cannot hold a match, so it is emitted raw — but
/// the encoder still indexes its positions so later blocks can match against
/// them, and the bound on that loop read `len - MIN_MATCH + 1` through a
/// saturating subtraction. For a body shorter than the key that floors at zero
/// and the `+ 1` admits position 0, whose four-byte key runs off the end of the
/// caller's slice. It panicked at levels 4 through 8, which are the ones that
/// index a dictionary this way.
///
/// Sweeping both sizes from zero rather than testing the one failing pair keeps
/// this honest about every other degenerate combination.
#[test]
fn roundtrips_dictionaries_and_bodies_shorter_than_a_hash_key() {
    for level in 1..=22i32 {
        let level = CompressionLevel::try_new(level).unwrap();
        for dict_len in 0..24usize {
            for body_len in 0..24usize {
                let dictionary: Vec<u8> = (0..dict_len).map(|i| (i * 7 % 251) as u8).collect();
                let body: Vec<u8> = (0..body_len).map(|i| (i * 13 % 241) as u8).collect();

                let encoded = encode_all_with_dict_and_options(
                    &body,
                    &dictionary,
                    EncoderOptions {
                        block_size: 4096,
                        compression_level: level,
                        ..Default::default()
                    },
                )
                .unwrap();

                assert_eq!(
                    decode_all_with_dict(&encoded, &dictionary).unwrap(),
                    body,
                    "level {} with a {dict_len}-byte dictionary and a {body_len}-byte body",
                    level.as_i32(),
                );
            }
        }
    }
}

#[test]
fn prepared_dictionary_reuses_parsed_state_for_one_shot_calls() {
    let dictionary = raw_test_dictionary();
    let prepared = EncoderDictionary::new(dictionary).unwrap();
    let prepared_decoding = DecoderDictionary::new(dictionary).unwrap();
    assert_eq!(prepared.id(), 0);
    assert!(prepared.is_raw_content());

    for data in [
        build_dictionary_echo_pattern(dictionary, 24_000),
        build_dictionary_echo_pattern(dictionary, 48_000),
    ] {
        let encoded = encode_all_with_prepared_dict(&data, &prepared).unwrap();
        let decoded = decode_all_with_prepared_dict(&encoded, &prepared_decoding).unwrap();

        assert_eq!(decoded, data);
        assert_eq!(decode_all_with_dict(&encoded, dictionary).unwrap(), data);
    }
}

/// A reusable encoder exists to amortize allocation, and nothing else. The same
/// input at the same level must come out byte for byte the same on the tenth
/// call as on the first, and the same as from a context that never encoded
/// anything.
///
/// The optimal parser's node array was kept across calls for its allocation but
/// grown rather than reset, so a later parse read the previous one's prices at
/// positions it never wrote. Output stayed valid, so only a comparison catches
/// it: levels 13 through 15 alternated between 45 and 48 bytes on this body,
/// call after call.
///
/// The interleaved shorter encode is here because that is how a real caller
/// reuses a context, and it resizes the state the next call inherits.
#[test]
fn a_reused_encoder_produces_identical_output_every_call() {
    // Straight from the input that found this. Long runs of one byte with a few
    // markers is what it takes: the parse reaches few positions, so most of the
    // node array is never written this time round and whatever the last parse
    // left there is what gets read. Bodies with matches everywhere overwrite the
    // array as they go and hide it.
    const BODY: &[u8] = &[
        0, 0, 0, 0, 0, 0, 0, 0, 26, 26, 26, 26, 26, 0, 0, 0, 0, 0, 0, 0, 82, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 246, 255, 0, 0, 0, 0, 0, 0, 0, 0, 41, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 41, 0, 0, 0, 0, 0, 0, 0, 0, 0, 44, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 221, 194,
    ];
    let body = BODY.to_vec();
    for level in 1..=22i32 {
        let level = CompressionLevel::try_new(level).unwrap();
        let options = EncoderOptions {
            block_size: 1024,
            compression_level: level,
            ..Default::default()
        };
        let expected = Encoder::new()
            .encode_all_with_options(&body, options)
            .unwrap();

        let mut encoder = Encoder::new();
        for call in 0..4 {
            assert_eq!(
                encoder.encode_all_with_options(&body, options).unwrap(),
                expected,
                "level {} differed on call {call}",
                level.as_i32(),
            );
            let _ = encoder
                .encode_all_with_options(&body[..body.len() / 4], options)
                .unwrap();
        }
    }
}

#[test]
fn streaming_encoder_roundtrips_chunked_input_with_windowed_headers() {
    let data = build_repeated_chunk_pattern(220_000);
    let mut encoder = StreamingEncoder::new(EncoderOptions {
        block_size: 64 * 1024,
        checksum: true,
        write_dict_id: true,
        compression_level: CompressionLevel::BETTER,
        ..Default::default()
    })
    .unwrap();

    assert!(encoder.pending_output_len() > 0);
    let mut encoded = encoder.take_output();
    let header = match parse_frame_header(&encoded).unwrap() {
        FrameHeader::Zstandard(header) => header,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    assert!(!header.single_segment);
    assert_eq!(header.content_size, None);
    assert!(header.checksum);

    for chunk in data.chunks(7_777) {
        encoder.push(chunk).unwrap();
        encoded.extend_from_slice(&encoder.take_output());
    }
    encoder.finish().unwrap();
    assert!(encoder.is_finished());
    encoded.extend_from_slice(&encoder.take_output());

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

#[test]
fn streaming_encoder_roundtrips_raw_dictionary_frames() {
    let dictionary = raw_test_dictionary();
    let data = build_dictionary_echo_pattern(dictionary, 80_000);
    let mut encoder = StreamingEncoder::with_dict(
        dictionary,
        EncoderOptions {
            block_size: 64 * 1024,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let mut encoded = encoder.take_output();
    for chunk in data.chunks(4_321) {
        encoder.push(chunk).unwrap();
        encoded.extend_from_slice(&encoder.take_output());
    }
    encoder.finish().unwrap();
    encoded.extend_from_slice(&encoder.take_output());

    let decoded = decode_all_with_dict(&encoded, dictionary).unwrap();
    assert_eq!(decoded, data);
}

#[test]
fn prepared_dictionary_reuses_parsed_state_for_streaming_calls() {
    let dictionary = raw_test_dictionary();
    let prepared = EncoderDictionary::new(dictionary).unwrap();
    let prepared_decoding = DecoderDictionary::new(dictionary).unwrap();
    let data = build_dictionary_echo_pattern(dictionary, 96_000);

    let mut encoder = StreamingEncoder::with_prepared_dict(
        &prepared,
        EncoderOptions {
            block_size: 64 * 1024,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let mut encoded = encoder.take_output();
    for chunk in data.chunks(5_123) {
        encoder.push(chunk).unwrap();
        encoded.extend_from_slice(&encoder.take_output());
    }
    encoder.finish().unwrap();
    encoded.extend_from_slice(&encoder.take_output());

    let mut decoder =
        StreamingDecoder::with_prepared_dict(&prepared_decoding, DecoderOptions::default());
    let mut decoded = Vec::new();
    let mut scratch = [0u8; 4096];
    for chunk in encoded.chunks(211) {
        decoder.push(chunk).unwrap();
        drain_decoder(&mut decoder, &mut scratch, &mut decoded);
    }
    decoder.finish().unwrap();
    drain_decoder(&mut decoder, &mut scratch, &mut decoded);

    assert_eq!(decoded, data);
}

#[test]
fn streaming_encoder_flushes_partial_blocks_and_keeps_history() {
    let block = build_incompressible_pattern(32 * 1024);
    let mut encoder = StreamingEncoder::new(EncoderOptions {
        block_size: 64 * 1024,
        checksum: false,
        ..Default::default()
    })
    .unwrap();

    let mut encoded = encoder.take_output();
    encoder.push(&block).unwrap();
    encoder.flush().unwrap();
    assert!(!encoder.is_finished());
    encoded.extend_from_slice(&encoder.take_output());

    encoder.push(&block).unwrap();
    encoder.finish().unwrap();
    encoded.extend_from_slice(&encoder.take_output());

    let blocks = parse_frame_blocks(&encoded);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[1].block_type, BlockType::Compressed);

    let payload = &encoded[blocks[1].payload_start..blocks[1].payload_end];
    assert!(
        compressed_block_sequence_count(payload) > 0,
        "expected the post-flush block to match against flushed history"
    );

    let mut expected = block.clone();
    expected.extend_from_slice(&block);
    assert_eq!(decode_all(&encoded).unwrap(), expected);
}

/// A flush is the only way a caller can end a block below the size the split
/// heuristic would ever choose, and the optimal parser prices a block that short
/// from the predefined tables rather than from its own statistics.
///
/// It still has to hand those statistics to the block after it. It did not: the
/// literal histogram was filled and accumulated only when the block was priced
/// dynamically, so a first block of `OPT_PREDEFINED_THRESHOLD` bytes or fewer
/// left the next block's model with a zero literal sum and a literal price that
/// underflowed. C keeps the two apart — whether literals are compressed decides
/// whether the histogram is maintained, not which prices this block used.
///
/// Eight bytes is the threshold exactly; nine already took the dynamic path and
/// never showed the defect.
///
/// The block *after* the flush has to be short too. A long one reaches its first
/// literal by a route that never consults the per-byte literal price, so it rode
/// the same zero straight past the defect — which is why this test carries a
/// tail of tens of bytes rather than the tens of thousands that read more
/// naturally.
#[test]
fn streaming_encoder_survives_a_flush_below_the_predefined_price_threshold() {
    let body = build_repeated_chunk_pattern(64);
    for level in [
        CompressionLevel::try_new(16).unwrap(),
        CompressionLevel::try_new(17).unwrap(),
        CompressionLevel::BEST,
    ] {
        for opening in 1..=8usize {
            let mut encoder = StreamingEncoder::new(EncoderOptions {
                block_size: 4096,
                compression_level: level,
                ..Default::default()
            })
            .unwrap();

            let mut encoded = encoder.take_output();
            encoder.push(&body[..opening]).unwrap();
            encoder.flush().unwrap();
            encoded.extend_from_slice(&encoder.take_output());

            encoder.push(&body[opening..]).unwrap();
            encoder.finish().unwrap();
            encoded.extend_from_slice(&encoder.take_output());

            assert_eq!(
                decode_all(&encoded).unwrap(),
                body,
                "level {} with a {opening}-byte opening block",
                level.as_i32(),
            );
        }
    }
}

/// Small flushed blocks after larger ones, which is what it takes to catch a
/// sequence plan whose per-block capacity never grew.
///
/// The plan's four code vectors are cleared and re-reserved for every block, but
/// the reserve asked for the shortfall against the existing capacity while
/// `Vec::reserve` measures from the length — and the length is zero right after
/// the clear. On a vector that already held capacity the request came out below
/// what it had, so nothing grew, and the plan wrote sequence codes straight past
/// the end of the allocation. The vectors grow independently, so they drift to
/// different capacities and only some of them come up short.
///
/// Blocks shrink here because a flush ends one wherever the buffer stands, and
/// the body has a short period broken every few bytes so the parser keeps
/// emitting sequences instead of settling on one repeat offset. A body that
/// repeats exactly produces so few sequences per block that the capacity never
/// binds, and this test passes against the defect.
#[test]
fn streaming_encoder_survives_blocks_that_shrink_across_flushes() {
    let body = build_dense_sequence_pattern(1 << 16);
    for level in 1..=22i32 {
        let level = CompressionLevel::try_new(level).unwrap();
        let mut encoder = StreamingEncoder::new(EncoderOptions {
            block_size: 1 << 14,
            compression_level: level,
            ..Default::default()
        })
        .unwrap();

        let mut encoded = encoder.take_output();
        for (index, piece) in body.chunks(16).enumerate() {
            encoder.push(piece).unwrap();
            if index % 4 == 0 {
                encoder.flush().unwrap();
            }
            encoded.extend_from_slice(&encoder.take_output());
        }
        encoder.finish().unwrap();
        encoded.extend_from_slice(&encoder.take_output());

        assert_eq!(
            decode_all(&encoded).unwrap(),
            body,
            "level {}",
            level.as_i32(),
        );
    }
}

/// A decoder that recycles its frame allocation still decodes every frame right.
///
/// `StreamingDecoder` keeps the previous frame's `FrameDecodeState` allocation
/// and assigns the next frame into it whole. This feeds one decoder a run of
/// deliberately unalike frames -- different corpora, dictionaries on some and
/// not others, checksums on and off, dictionary frames either side of a
/// dictionary-less one -- because a run of identical frames would agree even if
/// state were being shared outright.
///
/// **How much of this is measured.** Carrying `repeat_offsets` across frames is
/// caught here. Carrying `literals_state` or `sequence_tables` is *not*, by this
/// test or by any other in the suite: both were injected deliberately and
/// everything still passed. A stale entropy table is invisible until a frame
/// opens a section in `Repeat` mode against it, and frames from this crate's own
/// encoder do not, so no test built on encoder output can reach it. What rules
/// those two out is structural rather than empirical -- the decoder assigns the
/// whole struct and the compiler requires every field -- which is why the note
/// at that assignment says not to unpick it into per-field stores.
#[test]
fn a_recycled_decoder_decodes_every_frame_like_a_fresh_one() {
    let dictionary = build_structured_log_pattern(64 * 1024);
    let encoder_dict = EncoderDictionary::new(&dictionary).unwrap();
    let decoder_dict = DecoderDictionary::new(&dictionary).unwrap();

    let bodies = [
        build_repeated_chunk_pattern(40_000),
        build_huff_friendly_pattern(36_000),
        build_dictionary_echo_pattern(&dictionary, 44_000),
        build_incompressible_pattern(12_000),
        build_dense_sequence_pattern(52_000),
        build_dictionary_echo_pattern(&dictionary, 28_000),
        build_structured_log_pattern(48_000),
    ];

    // Every fourth frame carries a dictionary, so a dictionary frame follows a
    // dictionary-less one and vice versa.
    let frames: Vec<(Vec<u8>, bool)> = bodies
        .iter()
        .enumerate()
        .map(|(index, body)| {
            let with_dict = index % 3 == 2;
            let options = EncoderOptions {
                checksum: index % 2 == 0,
                compression_level: CompressionLevel::BETTER,
                ..Default::default()
            };
            let frame = if with_dict {
                encode_all_with_prepared_dict_and_options(body, &encoder_dict, options).unwrap()
            } else {
                encode_all_with_options(body, options).unwrap()
            };
            (frame, with_dict)
        })
        .collect();

    let mut recycled = StreamingDecoder::with_prepared_dict(
        &decoder_dict,
        DecoderOptions {
            single_frame: true,
            ..Default::default()
        },
    );

    for (index, ((frame, _), body)) in frames.iter().zip(bodies.iter()).enumerate() {
        if index > 0 {
            recycled.reset().unwrap();
        }
        recycled.push(frame).unwrap();
        recycled.finish().unwrap();
        let from_recycled = recycled.take_output();

        assert_eq!(
            from_recycled.len(),
            body.len(),
            "frame {index} decoded to the wrong length on a recycled decoder"
        );
        assert_eq!(
            from_recycled, *body,
            "frame {index} decoded differently on a recycled decoder than it was encoded from"
        );
    }
}

/// A reset context still produces frames that decode to what went in.
///
/// This is the round-trip half of the question and the weaker half: state that
/// leaks across a reset changes the *parse* while leaving the frame perfectly
/// decodable, so nothing here would notice it. See
/// `a_reset_frame_is_byte_identical_to_a_fresh_one` for the half that would.
#[test]
fn streaming_encoder_reset_reuses_context_for_concatenated_frames() {
    let first = build_repeated_chunk_pattern(48_000);
    let second = build_huff_friendly_pattern(52_000);
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
    assert_eq!(decode_all(&encoded).unwrap(), expected);
}

/// A frame produced after a reset must be byte-identical to the same frame from
/// an encoder that has done nothing else.
///
/// `reset_frame_state` clears a dozen fields, and the property that matters is
/// not that each line is present but that nothing was forgotten. Byte equality
/// is the only bound that can say so: anything a stale field changes shows up
/// in the parse, and a parse difference is a byte difference, while the frame
/// stays perfectly decodable. That is why the round-trip test above cannot
/// answer this, and why the leak this guards against is invisible to a ratio
/// bound too -- carrying the previous frame's price model over makes the next
/// frame *smaller*, not larger.
///
/// The first frame is deliberately long enough to compact several times, so the
/// context the second frame inherits is one that has done real work rather than
/// one that merely allocated. The second reset is not redundant: the first
/// starts from a context that was itself fresh, so state that only survives a
/// second cycle would pass with one.
///
/// Measured rather than assumed: dropping `clear_frame_parser_state` from
/// `reset_frame_state` fails 36 of the 108 comparisons below, from -1.91% (the
/// previous frame's price model carried over, so the frame comes out *smaller*)
/// to +516% (a dictionary row that collapses to near-raw). All 36 are the three
/// optimal parsers, twelve each, and none of the other six move at all. That is
/// the shape to expect: the price model is the state here with the longest
/// memory, and it is the only piece of it a non-optimal parser never reads.
#[test]
fn a_reset_frame_is_byte_identical_to_a_fresh_one() {
    const WINDOW_LOG: u32 = 15;
    // Six windows, so the first frame compacts several times before the reset.
    let first = benchmark_corpora::benchmark_report_cases(6 << WINDOW_LOG)
        .into_iter()
        .find(|case| case.name == "wikipedia")
        .expect("wikipedia is a benchmark corpus")
        .input;
    let second = benchmark_corpora::benchmark_report_cases((1 << WINDOW_LOG) + 4321)
        .into_iter()
        .find(|case| case.name == "json-records")
        .expect("json-records is a benchmark corpus")
        .input;
    let dictionary_source = benchmark_corpora::benchmark_report_cases(32 << 10)
        .into_iter()
        .find(|case| case.name == "log-lines")
        .expect("log-lines is a benchmark corpus")
        .input;
    let dictionary = EncoderDictionary::new(&dictionary_source).unwrap();

    let frame = |encoder: &mut StreamingEncoder<'_>, body: &[u8]| {
        let mut out = encoder.take_output();
        for chunk in body.chunks(32 << 10) {
            encoder.push(chunk).unwrap();
            out.extend_from_slice(&encoder.take_output());
        }
        encoder.finish().unwrap();
        out.extend_from_slice(&encoder.take_output());
        out
    };

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
        for min_match in [3u32, 4] {
            for long_distance_matching in [LdmMode::Disabled, LdmMode::Enabled] {
                for with_dictionary in [false, true] {
                    // The encoder refuses the pair, and reports it rather than
                    // silently dropping the matcher.
                    if long_distance_matching == LdmMode::Enabled && with_dictionary {
                        continue;
                    }
                    let case = format!(
                        "{strategy:?} min_match {min_match} \
                         {long_distance_matching:?} dictionary {with_dictionary}"
                    );
                    let options = EncoderOptions {
                        compression_level: CompressionLevel::try_new(12).unwrap(),
                        parameters: ParameterOverrides {
                            strategy: Some(strategy),
                            window_log: Some(WINDOW_LOG),
                            min_match: Some(min_match),
                            long_distance_matching,
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    let make = || {
                        if with_dictionary {
                            StreamingEncoder::with_prepared_dict(&dictionary, options).unwrap()
                        } else {
                            StreamingEncoder::new(options).unwrap()
                        }
                    };

                    let expected = frame(&mut make(), &second);

                    let mut reused = make();
                    let _ = frame(&mut reused, &first);
                    reused.reset().unwrap();
                    let after_one = frame(&mut reused, &second);
                    assert_eq!(
                        after_one.len(),
                        expected.len(),
                        "{case}: frame after one reset differs in size from a fresh one"
                    );
                    assert!(
                        after_one == expected,
                        "{case}: frame after one reset differs from a fresh one"
                    );

                    reused.reset().unwrap();
                    let after_two = frame(&mut reused, &second);
                    assert!(
                        after_two == expected,
                        "{case}: frame after two resets differs from a fresh one"
                    );
                }
            }
        }
    }
}

#[test]
fn streaming_encoder_reset_requires_a_finished_frame() {
    let mut encoder = StreamingEncoder::new(EncoderOptions::default()).unwrap();
    encoder.push(b"partial").unwrap();

    let err = encoder.reset().unwrap_err();
    assert_eq!(err, Error::InvalidParameter("cannot reset before finish"));
}

/// The dictionary has to survive into the second block, so the window has to be
/// wide enough to still contain it there.
///
/// `window_log` is pinned rather than left to the level for two reasons, both
/// consequences of blocks now being capped at the window. A stream carrying a
/// dictionary and no pledged size selects the smallest parameter row — upstream
/// does the same, because its unknown source size wraps to a few hundred bytes
/// in `ZSTD_getCParamRowSize` — which is a 16 KiB window. Under that window the
/// 64 KiB `block_size` asked for below is cut to 16 KiB, so "the second block"
/// would be 16 KiB in rather than one whole half of the input; and the
/// dictionary would have aged out of the window long before it, which is the
/// opposite of what this is checking. 128 KiB holds the whole input, so the
/// blocks land where the test means them to and the dictionary is still live.
#[test]
fn streaming_encoder_keeps_dictionary_available_after_first_block() {
    let dictionary = raw_test_dictionary();
    let block1 = build_incompressible_pattern(64 * 1024);
    let block2 = build_dictionary_echo_pattern(dictionary, 48 * 1024);
    let mut data = block1.clone();
    data.extend_from_slice(&block2);
    let mut encoder = StreamingEncoder::with_dict(
        dictionary,
        EncoderOptions {
            block_size: block1.len(),
            checksum: false,
            parameters: ParameterOverrides {
                window_log: Some(17),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();

    let mut encoded = encoder.take_output();
    for chunk in data.chunks(7_777) {
        encoder.push(chunk).unwrap();
        encoded.extend_from_slice(&encoder.take_output());
    }
    encoder.finish().unwrap();
    encoded.extend_from_slice(&encoder.take_output());

    let blocks = parse_frame_blocks(&encoded);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[1].block_type, BlockType::Compressed);

    let payload = &encoded[blocks[1].payload_start..blocks[1].payload_end];
    assert!(
        compressed_block_sequence_count(payload) > 0,
        "expected the streamed second block to keep matching against the dictionary",
    );

    assert_eq!(decode_all_with_dict(&encoded, dictionary).unwrap(), data);
}

#[test]
fn streaming_encoder_uses_prior_block_history_for_later_blocks() {
    let block = build_incompressible_pattern(64 * 1024);
    let mut data = block.clone();
    data.extend_from_slice(&block);

    let mut encoder = StreamingEncoder::new(EncoderOptions {
        block_size: block.len(),
        checksum: false,
        ..Default::default()
    })
    .unwrap();

    let mut encoded = encoder.take_output();
    encoder.push(&block).unwrap();
    encoded.extend_from_slice(&encoder.take_output());
    encoder.push(&block).unwrap();
    encoded.extend_from_slice(&encoder.take_output());
    encoder.finish().unwrap();
    encoded.extend_from_slice(&encoder.take_output());

    let blocks = parse_frame_blocks(&encoded);
    assert!(blocks.len() >= 2);
    assert_eq!(blocks[1].block_type, BlockType::Compressed);

    let payload = &encoded[blocks[1].payload_start..blocks[1].payload_end];
    assert!(
        compressed_block_sequence_count(payload) > 0,
        "expected the second streamed block to reuse prior history"
    );

    assert_eq!(decode_all(&encoded).unwrap(), data);
}

#[test]
fn streaming_encoder_closes_exact_block_boundaries_with_an_empty_last_block() {
    let data = build_incompressible_pattern(128 * 1024);
    let mut encoder = StreamingEncoder::new(EncoderOptions {
        block_size: 64 * 1024,
        checksum: false,
        ..Default::default()
    })
    .unwrap();

    let mut encoded = encoder.take_output();
    encoder.push(&data[..64 * 1024]).unwrap();
    encoded.extend_from_slice(&encoder.take_output());
    encoder.push(&data[64 * 1024..]).unwrap();
    encoded.extend_from_slice(&encoder.take_output());
    encoder.finish().unwrap();
    encoded.extend_from_slice(&encoder.take_output());

    let header_size = match parse_frame_header(&encoded).unwrap() {
        FrameHeader::Zstandard(header) => header.header_size,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let mut cursor = header_size;
    let mut blocks = Vec::new();
    loop {
        let block = parse_block_header(&encoded[cursor..]).unwrap();
        cursor += 3 + match block.block_type {
            BlockType::Raw | BlockType::Compressed => block.block_size as usize,
            BlockType::Rle => 1,
        };
        blocks.push(block);
        if block.last_block {
            break;
        }
    }

    assert_eq!(blocks.len(), 3);
    let last = blocks.last().unwrap();
    assert_eq!(last.block_type, BlockType::Raw);
    assert_eq!(last.block_size, 0);
    assert_eq!(decode_all(&encoded).unwrap(), data);
}

#[test]
fn streaming_decoder_handles_chunked_input_and_output() {
    let left = encode_all_with_options(
        b"left chunked payload",
        EncoderOptions {
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();
    let skip = write_skippable_frame(3, b"metadata").unwrap();
    let right = encode_all_with_options(
        &build_pattern(180_000),
        EncoderOptions {
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let mut encoded = Vec::new();
    encoded.extend_from_slice(&left);
    encoded.extend_from_slice(&skip);
    encoded.extend_from_slice(&right);

    let mut decoder = StreamingDecoder::new(DecoderOptions::default());
    let mut decoded = Vec::new();
    let mut scratch = [0u8; 11_111];
    for chunk in encoded.chunks(97) {
        decoder.push(chunk).unwrap();
        drain_decoder(&mut decoder, &mut scratch, &mut decoded);
    }
    decoder.finish().unwrap();
    assert!(decoder.is_finished());
    drain_decoder(&mut decoder, &mut scratch, &mut decoded);

    let mut expected = b"left chunked payload".to_vec();
    expected.extend_from_slice(&build_pattern(180_000));
    assert_eq!(decoded, expected);
}

#[test]
fn streaming_decoder_reports_truncated_input_on_finish() {
    let mut encoded = encode_all_with_options(
        &build_pattern(48_000),
        EncoderOptions {
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();
    encoded.pop();

    let mut decoder = StreamingDecoder::new(DecoderOptions::default());
    for chunk in encoded.chunks(251) {
        decoder.push(chunk).unwrap();
    }

    let err = decoder.finish().unwrap_err();
    assert_eq!(err, Error::UnexpectedEof);
}

#[test]
fn streaming_decoder_reset_reuses_context_for_independent_streams() {
    let first = encode_all_with_options(
        &build_pattern(48_000),
        EncoderOptions {
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();
    let second = encode_all_with_options(
        &build_huff_friendly_pattern(36_000),
        EncoderOptions {
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let mut decoder = StreamingDecoder::new(DecoderOptions::default());
    let mut decoded = Vec::new();
    let mut scratch = [0u8; 4096];

    for chunk in first.chunks(137) {
        decoder.push(chunk).unwrap();
        drain_decoder(&mut decoder, &mut scratch, &mut decoded);
    }
    decoder.finish().unwrap();
    drain_decoder(&mut decoder, &mut scratch, &mut decoded);

    decoder.reset().unwrap();

    for chunk in second.chunks(149) {
        decoder.push(chunk).unwrap();
        drain_decoder(&mut decoder, &mut scratch, &mut decoded);
    }
    decoder.finish().unwrap();
    drain_decoder(&mut decoder, &mut scratch, &mut decoded);

    let mut expected = build_pattern(48_000);
    expected.extend_from_slice(&build_huff_friendly_pattern(36_000));
    assert_eq!(decoded, expected);
}

/// `reset` keeps whatever the caller has not read yet, which means the next
/// stream decodes into a buffer that already has bytes in it.
///
/// That buffer is also the match history, so the two streams sharing it is
/// exactly the case where a frame could reach backwards into a previous one.
/// The second stream's frame has to start after the leftovers rather than at
/// zero, and the leftovers have to survive in front of it.
#[test]
fn streaming_decoder_reset_preserves_undrained_output() {
    let first_body = build_pattern(48_000);
    let second_body = build_huff_friendly_pattern(36_000);
    let first = encode_all_with_options(&first_body, EncoderOptions::default()).unwrap();
    let second = encode_all_with_options(&second_body, EncoderOptions::default()).unwrap();

    let mut decoder = StreamingDecoder::new(DecoderOptions::default());
    for chunk in first.chunks(137) {
        decoder.push(chunk).unwrap();
    }
    decoder.finish().unwrap();
    // Deliberately not drained: the first stream's output is still held.
    decoder.reset().unwrap();
    for chunk in second.chunks(149) {
        decoder.push(chunk).unwrap();
    }
    decoder.finish().unwrap();

    let mut expected = first_body;
    expected.extend_from_slice(&second_body);
    assert_eq!(decoder.take_output(), expected);
}

#[test]
fn streaming_decoder_reset_requires_a_finished_stream() {
    let encoded =
        encode_all_with_options(&build_pattern(12_000), EncoderOptions::default()).unwrap();

    let mut decoder = StreamingDecoder::new(DecoderOptions::default());
    decoder.push(&encoded[..encoded.len() / 2]).unwrap();

    let err = decoder.reset().unwrap_err();
    assert_eq!(err, Error::UnexpectedEof);
}

/// Shapes that lean on the decoder's sliding window: outputs several times the
/// window size so it wraps repeatedly, and matches whose offset is far shorter
/// than their length so the overlapping copy has to repeat a pattern.
///
/// Every other streaming test in this file decodes less than one window's worth,
/// so the window never wrapped and none of this was covered. The one-shot
/// decoder is a genuine oracle here because it reconstructs matches through
/// entirely separate code in `src/decode.rs`.
#[test]
fn streaming_decode_matches_one_shot_across_window_wraps_and_short_offsets() {
    let mut cases: Vec<(String, Vec<u8>)> = Vec::new();

    // Periods that do and do not divide the window, so the pattern seam and the
    // ring seam drift in and out of phase.
    for period in [1usize, 2, 3, 7, 16, 255, 4096] {
        let unit: Vec<u8> = (0..period).map(|i| b'a' + (i % 26) as u8).collect();
        let repeats = (3 << 20) / period;
        cases.push((format!("period-{period}"), unit.repeat(repeats.max(1))));
    }

    // Long-range repetition: a body of unique-ish text repeated, so matches
    // reach back most of a window rather than a handful of bytes.
    let mut block = Vec::new();
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..600_000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        block.push((state & 0x3f) as u8 + b'0');
    }
    let mut long_range = Vec::new();
    for _ in 0..6 {
        long_range.extend_from_slice(&block);
    }
    cases.push(("long-range".to_string(), long_range));

    for (name, body) in cases {
        for level in [1i32, 3, 9] {
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                checksum: true,
                ..Default::default()
            };
            let frame = encode_all_with_options(&body, options).unwrap();
            let one_shot = decode_all(&frame).unwrap();
            assert_eq!(one_shot, body, "{name} at level {level}: one-shot decode");

            // Chunked so block boundaries and push boundaries do not coincide,
            // which is where the window's write position is most likely to drift.
            let mut decoder = StreamingDecoder::new(DecoderOptions::default());
            let mut streamed = Vec::new();
            for chunk in frame.chunks(7_919) {
                decoder.push(chunk).unwrap();
                streamed.extend_from_slice(&decoder.take_output());
            }
            decoder.finish().unwrap();
            streamed.extend_from_slice(&decoder.take_output());

            assert_eq!(
                streamed, body,
                "{name} at level {level}: streaming decode disagrees with the input"
            );
        }
    }
}

/// The decoder used to retire history with `Vec::drain(..1)`, a memmove of the
/// whole window for every byte produced, so streaming decode ran in
/// `O(output * window)`. This frame is 329 bytes and expands to 3.2 MB; it took
/// **42 seconds**, against well under a millisecond for the same frame one-shot.
/// That is unbounded amplification from a tiny input, reachable through
/// `StreamingDecoder` and the `io::Read` adapter built on it.
///
/// The bound asserts the shape of the cost rather than a throughput target. It
/// sits roughly four orders of magnitude above what the repaired decoder needs
/// and an order of magnitude below what the broken one managed, so it can
/// neither flake on a loaded machine nor miss a return to quadratic.
#[test]
fn streaming_decode_of_a_window_filling_frame_is_not_quadratic() {
    let unit = b"abcabcabd".repeat(40);
    let mut body = Vec::new();
    for _ in 0..9_000 {
        body.extend_from_slice(&unit);
        body.push(b'\n');
    }

    let options = EncoderOptions {
        compression_level: CompressionLevel::try_new(1).unwrap(),
        ..Default::default()
    };
    let frame = encode_all_with_options(&body, options).unwrap();

    let started = std::time::Instant::now();
    let mut decoder = StreamingDecoder::new(DecoderOptions::default());
    decoder.push(&frame).unwrap();
    decoder.finish().unwrap();
    let streamed = decoder.take_output();
    let elapsed = started.elapsed();

    assert_eq!(streamed, body);
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "decoding {} bytes of output from a {}-byte frame took {elapsed:?}; \
         history eviction is quadratic again",
        body.len(),
        frame.len()
    );
}

/// The streaming encoder used to build a fresh match finder for every block and
/// re-insert the entire retained prefix into it, so encode cost grew with the
/// square of the frame length until the window filled. At level 15 a 526 KB
/// stream took 0.438s against 0.011s for the same bytes one-shot.
///
/// Timing a single encode would measure the machine; timing two and comparing
/// measures the shape. Quadratic cost quadruples when the input doubles, so a
/// four-fold input predicts roughly 4x for linear against 16x for quadratic.
/// The bound sits between them with about a factor of two either side: the
/// repaired encoder measures near 4.3, the broken one near 17.
///
/// Level 13 is the cheapest level that uses a binary tree, which is where the
/// rebuild hurt most, and its tables are small enough that allocating them does
/// not swamp the smaller of the two measurements.
#[test]
fn streaming_encode_time_grows_linearly_with_frame_length() {
    const SMALL: usize = 1 << 20;
    const LARGE: usize = 4 << 20;

    let mut body = Vec::with_capacity(LARGE);
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut n = 0u64;
    while body.len() < LARGE {
        body.extend_from_slice(
            format!(
                "2026-07-27T12:00:{:02}Z seq={n} path=/api/v1/items status=200\n",
                n % 60
            )
            .as_bytes(),
        );
        n += 1;
        // Filler so the parser has to search rather than ride one long match.
        if n.is_multiple_of(9) {
            for _ in 0..16 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                body.push((state >> 24) as u8);
            }
        }
    }
    body.truncate(LARGE);

    let options = EncoderOptions {
        compression_level: CompressionLevel::try_new(13).unwrap(),
        ..Default::default()
    };
    let encode = |bytes: &[u8]| {
        let started = std::time::Instant::now();
        let mut encoder = StreamingEncoder::new(options).unwrap();
        for piece in bytes.chunks(32 * 1024) {
            encoder.push(piece).unwrap();
            let _ = encoder.take_output();
        }
        encoder.finish().unwrap();
        let _ = encoder.take_output();
        started.elapsed()
    };

    // Take the fastest of a few interleaved rounds rather than one shot each.
    // `cargo test` runs the test binaries concurrently, so a single pair of
    // measurements can catch this test while another binary owns the cores,
    // and the ratio of two differently-perturbed runs is not the ratio of two
    // costs. The quickest run of each size is the one least disturbed, and a
    // quadratic is still quadratic at its best.
    const ROUNDS: usize = 3;
    let mut small = std::time::Duration::MAX;
    let mut large = std::time::Duration::MAX;
    for _ in 0..ROUNDS {
        small = small.min(encode(&body[..SMALL]));
        large = large.min(encode(&body));
    }
    let growth = large.as_secs_f64() / small.as_secs_f64();

    assert!(
        growth < 9.0,
        "encoding {LARGE} bytes took {large:?} against {small:?} for {SMALL}, a factor of \
         {growth:.1} for four times the input; the per-block match finder rebuild is back"
    );
}

#[test]
fn skips_skippable_frames_between_zstd_frames() {
    let left = encode_all_with_options(
        b"left",
        EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();
    let skip = write_skippable_frame(2, b"metadata").unwrap();
    let right = encode_all_with_options(
        b"right",
        EncoderOptions {
            block_size: 128 * 1024,
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();

    let mut joined = Vec::new();
    joined.extend_from_slice(&left);
    joined.extend_from_slice(&skip);
    joined.extend_from_slice(&right);

    let decoded = decode_all(&joined).unwrap();
    assert_eq!(decoded, b"leftright");
}

#[test]
fn detects_content_checksum_mismatch() {
    let mut encoded = encode_all_with_options(
        b"checksummed data",
        EncoderOptions {
            block_size: 128 * 1024,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();
    let last = encoded.len() - 1;
    encoded[last] ^= 0x80;

    let err = decode_all(&encoded).unwrap_err();
    assert!(matches!(err, Error::ChecksumMismatch { .. }));
}

#[test]
fn rejects_frame_header_reserved_bit() {
    let frame = vec![0x28, 0xB5, 0x2F, 0xFD, (1 << 5) | (1 << 3), 0];

    let err = parse_frame_header(&frame).unwrap_err();
    assert_eq!(err, Error::Corruption("frame header reserved bit is set"));

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(err, Error::Corruption("frame header reserved bit is set"));
}

#[test]
fn rejects_truncated_dictionary_id_in_frame_header() {
    let frame = vec![0x28, 0xB5, 0x2F, 0xFD, (1 << 5) | 1];

    let err = parse_frame_header(&frame).unwrap_err();
    assert_eq!(err, Error::UnexpectedEof);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(err, Error::UnexpectedEof);
}

#[test]
fn rejects_truncated_skippable_frame_payload() {
    let mut frame = write_partial_skippable_frame(2, 4);
    frame.push(b'x');

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(err, Error::UnexpectedEof);
}

#[test]
fn requires_a_dictionary_when_the_frame_declares_one() {
    let mut frame = write_single_segment_header_with_dict(5, 7);
    append_raw_block(&mut frame, b"hello", true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(err, Error::DictionaryRequired(Some(7)));
}

#[test]
fn rejects_mismatched_dictionary_ids_before_block_decode() {
    let mut frame = write_single_segment_header_with_dict(5, 7);
    append_raw_block(&mut frame, b"hello", true);

    let err = decode_all_with_dict(&frame, b"raw dictionary").unwrap_err();
    assert_eq!(
        err,
        Error::DictionaryMismatch {
            expected: 7,
            actual: 0,
        }
    );
}

#[test]
fn decodes_literals_only_compressed_blocks() {
    let literals = b"hello";
    let mut frame = Vec::new();
    frame.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
    frame.push(0x00);
    frame.push(0x00);

    let block_content_size = 1 + literals.len() + 1;
    let block_header = 1u32 | (2u32 << 1) | ((block_content_size as u32) << 3);
    frame.push((block_header & 0xFF) as u8);
    frame.push(((block_header >> 8) & 0xFF) as u8);
    frame.push(((block_header >> 16) & 0xFF) as u8);
    frame.push((literals.len() as u8) << 3);
    frame.extend_from_slice(literals);
    frame.push(0);

    let decoded = decode_all(&frame).unwrap();
    assert_eq!(decoded, literals);
}

#[test]
fn rejects_treeless_literals_without_a_previous_huffman_table() {
    let mut frame = write_single_segment_header(5);
    let mut block_payload = encode_compressed_literals_header(3, 0, 0, 1);
    block_payload.push(1);
    block_payload.push(0);
    append_compressed_block(&mut frame, &block_payload, true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(
        err,
        Error::Corruption("treeless literals require a prior Huff0 table")
    );
}

#[test]
fn rejects_zero_sequence_blocks_with_trailing_payload() {
    let mut frame = write_single_segment_header(32);
    let mut block_payload = raw_literals_section(b"");
    block_payload.extend_from_slice(&[0, 0xAA]);
    append_compressed_block(&mut frame, &block_payload, true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(
        err,
        Error::Corruption("zero-sequence blocks must not contain additional sequence payload")
    );
}

#[test]
fn rejects_reserved_sequence_mode_bits() {
    let mut frame = write_single_segment_header(32);
    let mut block_payload = raw_literals_section(b"");
    block_payload.extend_from_slice(&[1, 0x01]);
    append_compressed_block(&mut frame, &block_payload, true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(
        err,
        Error::Corruption("sequence compression modes reserved bits are set")
    );
}

#[test]
fn rejects_repeat_mode_without_previous_sequence_tables() {
    let mut frame = write_single_segment_header(32);
    let mut block_payload = raw_literals_section(b"");
    block_payload.extend_from_slice(&[1, 0b1111_1100]);
    append_compressed_block(&mut frame, &block_payload, true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(
        err,
        Error::Corruption("sequence repeat mode requires a previous FSE table")
    );
}

#[test]
fn rejects_invalid_sequence_fse_table_description() {
    let mut frame = write_single_segment_header(32);
    let mut block_payload = raw_literals_section(b"");
    block_payload.extend_from_slice(&[1, 0b1000_0000]);
    append_compressed_block(&mut frame, &block_payload, true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(err, Error::Corruption("invalid FSE ncount"));
}

#[test]
fn rejects_sequences_that_read_beyond_the_literals_buffer() {
    let mut frame = write_single_segment_header(32);
    let mut block_payload = raw_literals_section(b"");
    block_payload.extend_from_slice(&[1, 0b0101_0100, 7, 0, 0, 1]);
    append_compressed_block(&mut frame, &block_payload, true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(
        err,
        Error::Corruption("sequence literal length exceeds literals buffer")
    );
}

#[test]
fn rejects_sequences_with_offsets_beyond_the_available_history_window() {
    let mut frame = write_single_segment_header(32);
    let mut block_payload = raw_literals_section(b"");
    block_payload.extend_from_slice(&[1, 0b0101_0100, 0, 3, 0, 0b0000_1010]);
    append_compressed_block(&mut frame, &block_payload, true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(
        err,
        Error::Corruption("sequence offset exceeds the available history window")
    );
}

#[test]
fn rejects_repeat_offsets_that_underflow_to_zero() {
    let mut frame = write_single_segment_header(32);
    let mut block_payload = raw_literals_section(b"");
    block_payload.extend_from_slice(&[1, 0b0101_0100, 0, 1, 0, 0b0000_0011]);
    append_compressed_block(&mut frame, &block_payload, true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(err, Error::Corruption("repeat offset 1 minus 1 is zero"));
}

#[test]
fn rejects_reserved_block_types() {
    let mut frame = write_single_segment_header(0);
    append_custom_block_header(&mut frame, 3, 0, true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(err, Error::Corruption("reserved block type"));
}

#[test]
fn rejects_blocks_larger_than_the_frame_limit() {
    let mut frame = write_single_segment_header(1);
    append_raw_block_with_declared_size(&mut frame, b"ab", 2, true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(
        err,
        Error::Corruption("block size exceeds frame block size limit")
    );
}

#[test]
fn rejects_truncated_raw_block_payloads() {
    let mut frame = write_single_segment_header(4);
    append_raw_block_with_declared_size(&mut frame, b"ab", 4, true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(err, Error::UnexpectedEof);
}

#[test]
fn rejects_truncated_rle_block_payloads() {
    let mut frame = write_single_segment_header(4);
    append_custom_block_header(&mut frame, 1, 4, true);

    let err = decode_all(&frame).unwrap_err();
    assert_eq!(err, Error::UnexpectedEof);
}

#[test]
fn rejects_truncated_checksum_fields() {
    let mut encoded = encode_all_with_options(
        b"checksummed data",
        EncoderOptions {
            block_size: 128 * 1024,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();
    encoded.pop();

    let err = decode_all(&encoded).unwrap_err();
    assert_eq!(err, Error::UnexpectedEof);
}

// ---------------------------------------------------------------------------
// Conformance edge-case tests
// ---------------------------------------------------------------------------

/// Empty input (0 bytes of content). The frame header declares content size 0
/// and the single block carries no payload.
#[test]
fn conformance_empty_frame() {
    let data: &[u8] = &[];
    for checksum in [false, true] {
        let encoded = encode_all_with_options(
            data,
            EncoderOptions {
                compression_level: CompressionLevel::FASTEST,
                checksum,
                ..Default::default()
            },
        )
        .unwrap();

        // Verify the frame header reports content_size == 0.
        let header = parse_frame_header(&encoded).unwrap();
        match header {
            FrameHeader::Zstandard(h) => {
                assert_eq!(h.content_size, Some(0));
                assert_eq!(h.checksum, checksum);
            }
            _ => panic!("expected zstandard frame"),
        }

        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, data);
    }
}

/// Single-byte frame (content size exactly 1).
#[test]
fn conformance_single_byte_frame() {
    let data: &[u8] = &[0x42];
    let encoded = encode_all_with_options(
        data,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let header = parse_frame_header(&encoded).unwrap();
    match header {
        FrameHeader::Zstandard(h) => {
            assert_eq!(h.content_size, Some(1));
        }
        _ => panic!("expected zstandard frame"),
    }

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

/// Very small frames: 2, 3, and 4 bytes of content. These sit right at the
/// boundary where compressed blocks cannot beat raw blocks.
#[test]
fn conformance_very_small_frames() {
    for size in [2, 3, 4] {
        let data: Vec<u8> = (0..size).map(|i| (i * 37) as u8).collect();
        let encoded = encode_all_with_options(
            &data,
            EncoderOptions {
                compression_level: CompressionLevel::FASTEST,
                checksum: true,
                ..Default::default()
            },
        )
        .unwrap();

        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, data, "roundtrip failed for size={size}");
    }
}

/// Content size exactly at one block boundary (128 * 1024 = 131072 bytes).
/// With the default block_size this should produce a single block.
#[test]
fn conformance_content_size_at_block_boundary() {
    let size = 128 * 1024;
    let data = build_pattern(size);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let header = parse_frame_header(&encoded).unwrap();
    match header {
        FrameHeader::Zstandard(h) => {
            assert_eq!(h.content_size, Some(size as u64));
        }
        _ => panic!("expected zstandard frame"),
    }

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

/// Content size one byte over the block boundary (128 * 1024 + 1 = 131073 bytes).
/// This forces at least two blocks when using the default block_size.
#[test]
fn conformance_content_size_over_block_boundary() {
    let size = 128 * 1024 + 1;
    let data = build_pattern(size);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let header = parse_frame_header(&encoded).unwrap();
    match header {
        FrameHeader::Zstandard(h) => {
            assert_eq!(h.content_size, Some(size as u64));
        }
        _ => panic!("expected zstandard frame"),
    }

    // Must contain at least two blocks.
    let blocks = parse_frame_blocks(&encoded);
    assert!(
        blocks.len() >= 2,
        "expected >=2 blocks for {size} bytes, got {}",
        blocks.len()
    );

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

/// Empty content with checksum enabled. The checksum must be the lower 32 bits
/// of xxh64 over an empty input with seed 0.
#[test]
fn conformance_empty_frame_with_checksum() {
    let data: &[u8] = &[];
    let encoded = encode_all_with_options(
        data,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let header = parse_frame_header(&encoded).unwrap();
    match header {
        FrameHeader::Zstandard(h) => {
            assert_eq!(h.content_size, Some(0));
            assert!(h.checksum, "checksum flag must be set");
        }
        _ => panic!("expected zstandard frame"),
    }

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

/// Roundtrip all-zeros payload at the exact block boundary. All-zeros data
/// should produce RLE blocks, and this exercises the RLE path at the maximum
/// single-block size.
#[test]
fn conformance_all_zeros_at_block_boundary() {
    let size = 128 * 1024;
    let data = vec![0u8; size];
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    // The first block should be RLE.
    let header = parse_frame_header(&encoded).unwrap();
    let header_size = match header {
        FrameHeader::Zstandard(h) => h.header_size,
        _ => panic!("expected zstandard frame"),
    };
    let block = parse_block_header(&encoded[header_size..]).unwrap();
    assert_eq!(block.block_type, BlockType::Rle);

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

/// All-zeros payload one byte over the block boundary forces two RLE blocks.
#[test]
fn conformance_all_zeros_over_block_boundary() {
    let size = 128 * 1024 + 1;
    let data = vec![0u8; size];
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let blocks = parse_frame_blocks(&encoded);
    assert!(
        blocks.len() >= 2,
        "expected >=2 blocks for {size} zeros, got {}",
        blocks.len()
    );
    // The large block (128 KB) must be RLE; the trailing 1-byte block may be
    // Raw or RLE depending on encoder heuristics.
    assert_eq!(blocks[0].block_type, BlockType::Rle);

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

/// Streaming roundtrip of an empty frame -- the streaming encoder/decoder must
/// handle the zero-length case gracefully.
#[test]
fn conformance_streaming_empty_frame() {
    let data: &[u8] = &[];

    // Encode via streaming encoder.
    let mut encoder = StreamingEncoder::new(EncoderOptions {
        compression_level: CompressionLevel::FASTEST,
        checksum: true,
        ..Default::default()
    })
    .unwrap();
    let mut encoded = encoder.take_output();
    encoder.push(data).unwrap();
    encoded.extend_from_slice(&encoder.take_output());
    encoder.finish().unwrap();
    encoded.extend_from_slice(&encoder.take_output());

    // Decode via streaming decoder.
    let mut decoder = StreamingDecoder::new(DecoderOptions::default());
    decoder.push(&encoded).unwrap();
    decoder.finish().unwrap();
    assert!(decoder.is_finished());
    let mut scratch = [0u8; 64];
    let n = decoder.read(&mut scratch);
    assert_eq!(n, 0);
}

/// Streaming roundtrip of a single byte.
#[test]
fn conformance_streaming_single_byte() {
    let data: &[u8] = &[0xFE];

    let mut encoder = StreamingEncoder::new(EncoderOptions {
        compression_level: CompressionLevel::FASTEST,
        checksum: true,
        ..Default::default()
    })
    .unwrap();
    let mut encoded = encoder.take_output();
    encoder.push(data).unwrap();
    encoded.extend_from_slice(&encoder.take_output());
    encoder.finish().unwrap();
    encoded.extend_from_slice(&encoder.take_output());

    let mut decoder = StreamingDecoder::new(DecoderOptions::default());
    decoder.push(&encoded).unwrap();
    decoder.finish().unwrap();
    let mut output = Vec::new();
    let mut scratch = [0u8; 64];
    drain_decoder(&mut decoder, &mut scratch, &mut output);
    assert_eq!(output, data);
}

/// Roundtrip with the smallest non-default block_size to force many blocks.
/// This exercises multi-block framing even for small payloads.
#[test]
fn conformance_many_small_blocks() {
    let data = build_pattern(4096);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            block_size: 1024,
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let blocks = parse_frame_blocks(&encoded);
    assert!(
        blocks.len() >= 4,
        "expected >=4 blocks for 4096 bytes with block_size=1024, got {}",
        blocks.len()
    );

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

/// Roundtrip of incompressible data at the block boundary. The encoder should
/// emit raw blocks when compression does not reduce size.
#[test]
fn conformance_incompressible_at_block_boundary() {
    let size = 128 * 1024;
    let data = build_incompressible_pattern(size);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    // At least the first block should be raw (incompressible data).
    let blocks = parse_frame_blocks(&encoded);
    assert!(!blocks.is_empty());
    assert_eq!(
        blocks[0].block_type,
        BlockType::Raw,
        "incompressible data should produce raw blocks"
    );

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

/// Roundtrip of incompressible data one byte over block boundary.
#[test]
fn conformance_incompressible_over_block_boundary() {
    let size = 128 * 1024 + 1;
    let data = build_incompressible_pattern(size);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let blocks = parse_frame_blocks(&encoded);
    assert!(
        blocks.len() >= 2,
        "expected >=2 blocks for incompressible {size} bytes, got {}",
        blocks.len()
    );

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

/// Frame Content Size field encoding edge case: content size 255 fits in a
/// 1-byte FCS field.
#[test]
fn conformance_fcs_field_size_1_byte_max() {
    let data = build_pattern(255);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let header = parse_frame_header(&encoded).unwrap();
    match header {
        FrameHeader::Zstandard(h) => {
            assert_eq!(h.content_size, Some(255));
        }
        _ => panic!("expected zstandard frame"),
    }

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

/// Frame Content Size field encoding edge case: content size 256 requires a
/// 2-byte FCS field (value stored as size - 256 per the spec).
#[test]
fn conformance_fcs_field_size_2_byte_min() {
    let data = build_pattern(256);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let header = parse_frame_header(&encoded).unwrap();
    match header {
        FrameHeader::Zstandard(h) => {
            assert_eq!(h.content_size, Some(256));
        }
        _ => panic!("expected zstandard frame"),
    }

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

/// Frame Content Size field encoding edge case: content size 65791 is the
/// maximum representable in a 2-byte FCS field (256 + 0xFFFF).
#[test]
fn conformance_fcs_field_size_2_byte_max() {
    let data = build_pattern(65791);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let header = parse_frame_header(&encoded).unwrap();
    match header {
        FrameHeader::Zstandard(h) => {
            assert_eq!(h.content_size, Some(65791));
        }
        _ => panic!("expected zstandard frame"),
    }

    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(decoded, data);
}

/// Multiple compression levels should all produce correct roundtrips for
/// data at the block boundary.
#[test]
fn conformance_all_levels_at_block_boundary() {
    let size = 128 * 1024;
    let data = build_pattern(size);
    for level in 1..=9 {
        let compression_level = CompressionLevel::try_new(level).unwrap();
        let encoded = encode_all_with_options(
            &data,
            EncoderOptions {
                compression_level,
                checksum: true,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, data, "roundtrip failed at level {level}");
    }
}

/// Multiple compression levels should all produce correct roundtrips for
/// data one byte over the block boundary.
#[test]
fn conformance_all_levels_over_block_boundary() {
    let size = 128 * 1024 + 1;
    let data = build_pattern(size);
    for level in 1..=9 {
        let compression_level = CompressionLevel::try_new(level).unwrap();
        let encoded = encode_all_with_options(
            &data,
            EncoderOptions {
                compression_level,
                checksum: true,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = decode_all(&encoded).unwrap();
        assert_eq!(decoded, data, "roundtrip failed at level {level}");
    }
}

/// Concatenated frames: empty frame followed by a non-empty frame. The decoder
/// must handle zero-length frames appearing within a multi-frame stream.
#[test]
fn conformance_concatenated_empty_then_nonempty() {
    let empty_enc = encode_all_with_options(
        &[],
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();
    let payload = b"conformance payload";
    let payload_enc = encode_all_with_options(
        payload,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let mut combined = Vec::new();
    combined.extend_from_slice(&empty_enc);
    combined.extend_from_slice(&payload_enc);

    let decoded = decode_all(&combined).unwrap();
    assert_eq!(decoded, payload);
}

/// Concatenated frames: non-empty frame followed by an empty frame.
#[test]
fn conformance_concatenated_nonempty_then_empty() {
    let payload = b"conformance payload";
    let payload_enc = encode_all_with_options(
        payload,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();
    let empty_enc = encode_all_with_options(
        &[],
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let mut combined = Vec::new();
    combined.extend_from_slice(&payload_enc);
    combined.extend_from_slice(&empty_enc);

    let decoded = decode_all(&combined).unwrap();
    assert_eq!(decoded, payload);
}

/// Verify that a frame with checksum disabled followed by a frame with checksum
/// enabled decodes correctly as concatenated frames.
#[test]
fn conformance_concatenated_mixed_checksum() {
    let data_a = build_pattern(1000);
    let data_b = build_pattern(2000);
    let enc_a = encode_all_with_options(
        &data_a,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: false,
            ..Default::default()
        },
    )
    .unwrap();
    let enc_b = encode_all_with_options(
        &data_b,
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let mut combined = Vec::new();
    combined.extend_from_slice(&enc_a);
    combined.extend_from_slice(&enc_b);

    let decoded = decode_all(&combined).unwrap();
    let mut expected = data_a;
    expected.extend_from_slice(&data_b);
    assert_eq!(decoded, expected);
}

#[test]
fn decode_all_into_matches_decode_all_and_keeps_its_allocation() {
    let data = build_pattern(300_000);
    let encoded = encode_all_with_options(
        &data,
        EncoderOptions {
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    let mut decoder = Decoder::new();
    let mut out = Vec::new();
    decoder.decode_all_into(&encoded, &mut out).unwrap();
    assert_eq!(out, decode_all(&encoded).unwrap());

    // The point of the API is that the second decode does not go back to the
    // allocator. A capacity that survives is the only observable evidence of
    // that, so assert on it rather than on the timing it exists to improve.
    let capacity = out.capacity();
    let pointer = out.as_ptr();
    decoder.decode_all_into(&encoded, &mut out).unwrap();
    assert_eq!(out, data);
    assert_eq!(out.capacity(), capacity);
    assert_eq!(out.as_ptr(), pointer);
}

#[test]
fn decode_all_into_discards_whatever_the_buffer_held() {
    let long = build_pattern(200_000);
    let short = build_huff_friendly_pattern(4_000);
    let long_encoded = encode_all(&long).unwrap();
    let short_encoded = encode_all(&short).unwrap();

    let mut decoder = Decoder::new();
    let mut out = Vec::new();
    decoder.decode_all_into(&long_encoded, &mut out).unwrap();

    // Decoding something shorter into the same buffer must not leave a tail of
    // the previous frame behind it: the buffer is reused, not appended to.
    decoder.decode_all_into(&short_encoded, &mut out).unwrap();
    assert_eq!(out, short);

    // Nor may unrelated bytes a caller left in the buffer survive.
    out.clear();
    out.extend_from_slice(b"caller data that must not appear in the output");
    decoder.decode_all_into(&short_encoded, &mut out).unwrap();
    assert_eq!(out, short);
}

#[test]
fn decode_all_into_concatenates_every_frame_in_one_call() {
    let first = build_pattern(9_000);
    let second = build_huff_friendly_pattern(11_000);
    let mut encoded = encode_all(&first).unwrap();
    encoded.extend_from_slice(&encode_all(&second).unwrap());

    let mut out = Vec::new();
    Decoder::new().decode_all_into(&encoded, &mut out).unwrap();

    let mut expected = first;
    expected.extend_from_slice(&second);
    assert_eq!(out, expected);
}

#[test]
fn decode_all_into_does_not_present_stale_output_after_a_failure() {
    let data = build_pattern(50_000);
    let encoded = encode_all(&data).unwrap();

    let mut decoder = Decoder::new();
    let mut out = Vec::new();
    decoder.decode_all_into(&encoded, &mut out).unwrap();
    assert_eq!(out, data);

    // A caller that ignores the error and reads the buffer anyway must not find
    // the previous frame sitting there looking like a successful decode.
    let truncated = &encoded[..encoded.len() / 2];
    assert!(decoder.decode_all_into(truncated, &mut out).is_err());
    assert_ne!(out, data);

    // And the decoder is still usable afterwards.
    decoder.decode_all_into(&encoded, &mut out).unwrap();
    assert_eq!(out, data);
}

#[test]
fn decode_all_into_with_prepared_dict_matches_the_allocating_form() {
    let dictionary_bytes = build_pattern(24_000);
    let dictionary = EncoderDictionary::new(&dictionary_bytes).unwrap();
    let decoding = DecoderDictionary::new(&dictionary_bytes).unwrap();
    let data = build_pattern(60_000);
    let encoded = encode_all_with_prepared_dict(&data, &dictionary).unwrap();

    let mut out = Vec::new();
    Decoder::new()
        .decode_all_into_with_prepared_dict(&encoded, &mut out, &decoding)
        .unwrap();

    assert_eq!(
        out,
        decode_all_with_prepared_dict(&encoded, &decoding).unwrap()
    );
    assert_eq!(out, data);
}

fn build_pattern(size: usize) -> Vec<u8> {
    (0..size)
        .map(|index| ((index * 31) as u8).wrapping_add((index >> 7) as u8))
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

fn build_incompressible_pattern(size: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9u32;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

/// A body that makes the parser emit as many sequences per block as it can: a
/// three-byte period, so matches are everywhere, with one byte per period
/// rewritten so none of them runs long and the parser has to keep going back to
/// its hash table rather than riding a single repeat offset.
///
/// This is the shape that fills a sequence plan to its capacity bound. The
/// evenly-repeating patterns elsewhere in this file produce a handful of long
/// sequences per block and leave that bound untouched.
fn build_dense_sequence_pattern(size: usize) -> Vec<u8> {
    const PERIOD: &[u8] = b"\x23\x91\xb0";

    let mut out = Vec::with_capacity(size);
    let mut index = 0u32;
    while out.len() < size {
        let start = out.len();
        let take = PERIOD.len().min(size - start);
        out.extend_from_slice(&PERIOD[..take]);
        out[start + (index as usize * 7) % take] ^= (index as u8) | 1;
        index = index.wrapping_add(1);
    }
    out
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

fn build_structured_log_pattern(size: usize) -> Vec<u8> {
    let levels = ["INFO", "WARN", "ERROR"];
    let components = ["gateway", "scheduler", "replicator", "api"];
    let actions = ["read", "write", "flush", "rebalance"];

    let mut out = Vec::with_capacity(size);
    let mut index = 0u32;
    while out.len() < size {
        let level = levels[index as usize % levels.len()];
        let component = components[(index as usize / 2) % components.len()];
        let action = actions[(index as usize / 3) % actions.len()];
        let record = format!(
            "2026-03-08T12:{minute:02}:{second:02}Z {level} {component} node={node:02} shard={shard:03} action={action} duration_ms={duration} status={status} trace={trace:016x}\n",
            minute = index % 60,
            second = (index * 11) % 60,
            node = index % 24,
            shard = (index * 17) % 512,
            duration = 1 + (index * 29) % 3_000,
            status = if index.is_multiple_of(11) {
                "retry"
            } else {
                "ok"
            },
            trace = (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        );
        let remaining = size - out.len();
        out.extend_from_slice(&record.as_bytes()[..remaining.min(record.len())]);
        index += 1;
    }
    out
}

fn raw_test_dictionary() -> &'static [u8] {
    b"GET /api/v1/users?id=123&status=active HTTP/1.1\r\n\
Host: example.internal\r\n\
Accept: application/json\r\n\
{\"status\":\"active\",\"role\":\"admin\",\"region\":\"us-central\"}\n"
}

fn build_dictionary_echo_pattern(dictionary: &[u8], size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    let mut suffix = 0u32;
    while out.len() < size {
        let record = format!("{suffix:08x}:");
        let remaining = size - out.len();
        let prefix_len = remaining.min(record.len());
        out.extend_from_slice(&record.as_bytes()[..prefix_len]);
        if out.len() == size {
            break;
        }
        let remaining = size - out.len();
        out.extend_from_slice(&dictionary[..remaining.min(dictionary.len())]);
        suffix = suffix.wrapping_add(1);
    }
    out
}

fn compressed_block_sequence_count(payload: &[u8]) -> usize {
    let literals = parse_literals_header(payload);
    let sequence_section = &payload[literals.payload_end()..];
    decode_sequence_count(sequence_section)
}

fn compressed_block_sequence_counts(frame: &[u8]) -> Vec<usize> {
    let header = parse_frame_header(frame).unwrap();
    let header_size = match header {
        FrameHeader::Zstandard(header) => header.header_size,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };

    let mut cursor = header_size;
    let mut counts = Vec::new();
    loop {
        let block = parse_block_header(&frame[cursor..]).unwrap();
        cursor += 3;
        let payload_end = cursor
            + match block.block_type {
                BlockType::Raw | BlockType::Compressed => block.block_size as usize,
                BlockType::Rle => 1,
            };
        if block.block_type == BlockType::Compressed {
            counts.push(compressed_block_sequence_count(&frame[cursor..payload_end]));
        }
        cursor = payload_end;
        if block.last_block {
            break;
        }
    }
    counts
}

#[derive(Debug, Clone, Copy)]
struct ParsedBlock {
    block_type: BlockType,
    payload_start: usize,
    payload_end: usize,
}

fn parse_frame_blocks(frame: &[u8]) -> Vec<ParsedBlock> {
    let header = parse_frame_header(frame).unwrap();
    let header_size = match header {
        FrameHeader::Zstandard(header) => header.header_size,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };

    let mut cursor = header_size;
    let mut blocks = Vec::new();
    loop {
        let block = parse_block_header(&frame[cursor..]).unwrap();
        cursor += 3;
        let payload_end = cursor
            + match block.block_type {
                BlockType::Raw | BlockType::Compressed => block.block_size as usize,
                BlockType::Rle => 1,
            };
        blocks.push(ParsedBlock {
            block_type: block.block_type,
            payload_start: cursor,
            payload_end,
        });
        cursor = payload_end;
        if block.last_block {
            break;
        }
    }
    blocks
}

fn decode_sequence_count(src: &[u8]) -> usize {
    parse_sequence_count(src).0
}

fn parse_sequence_count(src: &[u8]) -> (usize, usize) {
    let byte0 = src[0] as usize;
    if byte0 < 128 {
        (byte0, 1)
    } else if byte0 < 255 {
        (((byte0 - 128) << 8) + src[1] as usize, 2)
    } else {
        (0x7F00 + src[1] as usize + ((src[2] as usize) << 8), 3)
    }
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

fn write_single_segment_header(content_size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
    out.push((1 << 5) | if content_size <= 255 { 0 } else { 1 << 6 });
    if content_size <= 255 {
        out.push(content_size as u8);
    } else {
        out.extend_from_slice(&((content_size - 256) as u16).to_le_bytes());
    }
    out
}

fn write_partial_skippable_frame(magic_variant: u8, declared_size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(0x184D_2A50u32 + u32::from(magic_variant)).to_le_bytes());
    out.extend_from_slice(&declared_size.to_le_bytes());
    out
}

fn write_single_segment_header_with_dict(content_size: usize, dictionary_id: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let (dict_size_flag, dict_bytes) = if dictionary_id <= u8::MAX as u32 {
        (1u8, vec![dictionary_id as u8])
    } else if dictionary_id <= u16::MAX as u32 {
        (2u8, (dictionary_id as u16).to_le_bytes().to_vec())
    } else {
        (3u8, dictionary_id.to_le_bytes().to_vec())
    };

    out.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
    out.push((1 << 5) | dict_size_flag | if content_size <= 255 { 0 } else { 1 << 6 });
    out.extend_from_slice(&dict_bytes);
    if content_size <= 255 {
        out.push(content_size as u8);
    } else {
        out.extend_from_slice(&((content_size - 256) as u16).to_le_bytes());
    }
    out
}

fn append_raw_block(frame: &mut Vec<u8>, payload: &[u8], last_block: bool) {
    append_custom_block_header(frame, 0, payload.len() as u32, last_block);
    frame.extend_from_slice(payload);
}

fn append_raw_block_with_declared_size(
    frame: &mut Vec<u8>,
    payload: &[u8],
    declared_size: u32,
    last_block: bool,
) {
    append_custom_block_header(frame, 0, declared_size, last_block);
    frame.extend_from_slice(payload);
}

fn append_compressed_block(frame: &mut Vec<u8>, payload: &[u8], last_block: bool) {
    append_custom_block_header(frame, 2, payload.len() as u32, last_block);
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

fn raw_literals_section(literals: &[u8]) -> Vec<u8> {
    let mut payload = vec![(literals.len() as u8) << 3];
    payload.extend_from_slice(literals);
    payload
}

#[derive(Debug, Clone, Copy)]
struct LiteralsHeader {
    header_size: usize,
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

    let (header_size, compressed_size) = match block_type {
        0 | 1 => match size_format {
            0 | 2 => (1, (header0 >> 3) as usize),
            1 => (2, ((src[0] as usize) >> 4) | ((src[1] as usize) << 4)),
            3 => {
                let value =
                    (src[0] as usize) | ((src[1] as usize) << 8) | ((src[2] as usize) << 16);
                (3, value >> 4)
            }
            _ => unreachable!(),
        },
        2 | 3 => match size_format {
            0 | 1 => {
                let value =
                    (src[0] as usize) | ((src[1] as usize) << 8) | ((src[2] as usize) << 16);
                (3, (value >> 14) & 0x03ff)
            }
            2 => {
                let value = u32::from_le_bytes([src[0], src[1], src[2], src[3]]) as usize;
                (4, (value >> 18) & 0x3fff)
            }
            3 => {
                let value = (src[0] as u64)
                    | ((src[1] as u64) << 8)
                    | ((src[2] as u64) << 16)
                    | ((src[3] as u64) << 24)
                    | ((src[4] as u64) << 32);
                (5, ((value >> 22) & 0x3ffff) as usize)
            }
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };

    LiteralsHeader {
        header_size,
        compressed_size,
    }
}

fn encode_compressed_literals_header(
    block_type: u8,
    size_format: u8,
    regenerated_size: usize,
    compressed_size: usize,
) -> Vec<u8> {
    let value = u64::from(block_type)
        | (u64::from(size_format) << 2)
        | ((regenerated_size as u64) << 4)
        | ((compressed_size as u64) << 14);
    vec![
        (value & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        ((value >> 16) & 0xff) as u8,
    ]
}

fn decode_hex_bytes(hex: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(hex.len() / 2);
    let mut chars = hex.chars().filter(|ch| !ch.is_whitespace());
    while let Some(high) = chars.next() {
        let low = chars.next().expect("hex input must have an even length");
        let high = high.to_digit(16).expect("invalid hex") as u8;
        let low = low.to_digit(16).expect("invalid hex") as u8;
        out.push((high << 4) | low);
    }
    out
}

// ---------------------------------------------------------------------------
// Differential boundary-condition tests
// ---------------------------------------------------------------------------

// --- Block size boundary tests ---

#[test]
fn differential_block_exactly_128kb() {
    let input = build_pattern(128 * 1024);
    let options = EncoderOptions {
        block_size: 128 * 1024,
        checksum: true,
        ..Default::default()
    };
    let compressed = encode_all_with_options(&input, options).unwrap();
    let decoded = decode_all(&compressed).unwrap();
    assert_eq!(input, decoded);
}

#[test]
fn differential_block_one_under_128kb() {
    let input = build_pattern(128 * 1024 - 1);
    let options = EncoderOptions {
        block_size: 128 * 1024,
        checksum: true,
        ..Default::default()
    };
    let compressed = encode_all_with_options(&input, options).unwrap();
    let decoded = decode_all(&compressed).unwrap();
    assert_eq!(input, decoded);
}

#[test]
fn differential_block_one_over_128kb() {
    // Input is one byte larger than the block size, so the encoder must
    // emit two blocks (128 KiB + 1 byte).
    let input = build_pattern(128 * 1024 + 1);
    let options = EncoderOptions {
        block_size: 128 * 1024,
        checksum: true,
        ..Default::default()
    };
    let compressed = encode_all_with_options(&input, options).unwrap();

    // Verify we get at least two blocks by checking the first block is not
    // marked as "last".
    let header = parse_frame_header(&compressed).unwrap();
    let header_size = match header {
        FrameHeader::Zstandard(h) => h.header_size,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let first_block = parse_block_header(&compressed[header_size..]).unwrap();
    assert!(
        !first_block.last_block,
        "first block should not be the last block when input exceeds block_size"
    );

    let decoded = decode_all(&compressed).unwrap();
    assert_eq!(input, decoded);
}

// --- Concatenated frame tests ---

#[test]
fn differential_concat_10_frames() {
    // Encode 10 different inputs as separate frames, concatenate the
    // compressed bytes, then decode_all which handles multiple frames.
    let mut concatenated_compressed = Vec::new();
    let mut expected_output = Vec::new();

    for i in 0u8..10 {
        let input: Vec<u8> = (0..256).map(|b| b as u8 ^ i).collect();
        let compressed = encode_all_with_options(
            &input,
            EncoderOptions {
                checksum: true,
                ..Default::default()
            },
        )
        .unwrap();
        concatenated_compressed.extend_from_slice(&compressed);
        expected_output.extend_from_slice(&input);
    }

    let decoded = decode_all(&concatenated_compressed).unwrap();
    assert_eq!(expected_output, decoded);
}

#[test]
fn differential_concat_empty_between_frames() {
    // Interleave empty-input frames between regular frames.
    let mut concatenated_compressed = Vec::new();
    let mut expected_output = Vec::new();

    let empty_compressed = encode_all(&[]).unwrap();

    for i in 0u8..5 {
        // Insert an empty frame before each regular frame.
        concatenated_compressed.extend_from_slice(&empty_compressed);

        let input: Vec<u8> = vec![i; 100];
        let compressed = encode_all(&input).unwrap();
        concatenated_compressed.extend_from_slice(&compressed);
        expected_output.extend_from_slice(&input);
    }

    // Trailing empty frame.
    concatenated_compressed.extend_from_slice(&empty_compressed);

    let decoded = decode_all(&concatenated_compressed).unwrap();
    assert_eq!(expected_output, decoded);
}

// --- Small input edge cases ---

#[test]
fn differential_empty_frame_roundtrip() {
    let input: Vec<u8> = Vec::new();
    let compressed = encode_all_with_options(
        &input,
        EncoderOptions {
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();
    let decoded = decode_all(&compressed).unwrap();
    assert!(decoded.is_empty());
}

#[test]
fn differential_single_byte_frames() {
    // Every possible single byte value must roundtrip correctly.
    for byte in 0x00u16..=0xFF {
        let input = vec![byte as u8];
        let compressed = encode_all_with_options(
            &input,
            EncoderOptions {
                checksum: true,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = decode_all(&compressed).unwrap();
        assert_eq!(
            input, decoded,
            "roundtrip failed for single byte 0x{:02X}",
            byte
        );
    }
}

#[test]
fn differential_below_min_match() {
    // Inputs of 1, 2, 3 bytes are below the minimum match length of most
    // LZ-style compressors (zstd minimum match is 3 for sequences). These
    // should still roundtrip via raw or RLE blocks.
    for size in 1..=3 {
        let input = build_pattern(size);
        let compressed = encode_all_with_options(
            &input,
            EncoderOptions {
                checksum: true,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = decode_all(&compressed).unwrap();
        assert_eq!(input, decoded, "roundtrip failed for {}-byte input", size);
    }
}

// --- Multi-block with various sizes ---

#[test]
fn differential_many_small_blocks() {
    // Encode 1 MB of data with a tiny block size (1024 bytes), producing
    // roughly 1024 blocks. Verify the roundtrip is correct.
    let input = build_pattern(1024 * 1024);
    let options = EncoderOptions {
        block_size: 1024,
        checksum: true,
        ..Default::default()
    };
    let compressed = encode_all_with_options(&input, options).unwrap();
    let decoded = decode_all(&compressed).unwrap();
    assert_eq!(input, decoded);
}

#[test]
fn differential_single_large_block() {
    // Encode 64 KB of data with a block_size much larger than the input
    // (clamped to BLOCK_SIZE_MAX = 128 KiB). The entire input fits in one
    // block.
    let input = build_pattern(64 * 1024);
    let options = EncoderOptions {
        block_size: 128 * 1024,
        checksum: true,
        ..Default::default()
    };
    let compressed = encode_all_with_options(&input, options).unwrap();

    // The first (and only) block should be marked as last.
    let header = parse_frame_header(&compressed).unwrap();
    let header_size = match header {
        FrameHeader::Zstandard(h) => h.header_size,
        FrameHeader::Skippable(_) => panic!("unexpected skippable frame"),
    };
    let block = parse_block_header(&compressed[header_size..]).unwrap();
    assert!(block.last_block, "single block should be marked as last");

    let decoded = decode_all(&compressed).unwrap();
    assert_eq!(input, decoded);
}

/// Line-oriented text with enough structure to produce real matches at a
/// range of distances. Deliberately larger than one block so streaming has to
/// carry history across block boundaries.
fn build_log_corpus(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut i = 0u64;
    while out.len() < len {
        out.extend_from_slice(
            format!(
                "2026-07-27T12:00:{:02}Z level=info request_id={} path=/api/v1/items status=200 dur_ms={}\n",
                i % 60,
                i,
                i % 997
            )
            .as_bytes(),
        );
        i += 1;
    }
    out.truncate(len);
    out
}

/// The streaming encoder used to declare `Window_Size = block_size` while
/// retaining a full block of history, so a match at the end of a block could
/// reference the start of the prefix at an offset of nearly `2 * block_size`.
/// Those frames were rejected by this crate's own decoder.
#[test]
fn streaming_frames_decode_at_every_level() {
    let input = build_log_corpus(400 * 1024);

    for level in 1..=(CompressionLevel::MAX.as_i32() as u8) {
        let mut encoder = StreamingEncoder::new(EncoderOptions {
            compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
            ..Default::default()
        })
        .unwrap();

        let mut compressed = encoder.take_output();
        for chunk in input.chunks(16 * 1024) {
            encoder.push(chunk).unwrap();
            compressed.extend_from_slice(&encoder.take_output());
        }
        encoder.finish().unwrap();
        compressed.extend_from_slice(&encoder.take_output());

        let decoded = decode_all(&compressed)
            .unwrap_or_else(|err| panic!("level {level} produced an undecodable frame: {err:?}"));
        assert_eq!(decoded, input, "level {level} roundtrip mismatch");
    }
}

/// Every offset the encoder emits must fit inside the window the frame header
/// declares, at both the one-shot and streaming entry points.
#[test]
fn declared_window_covers_every_emitted_offset() {
    let input = build_log_corpus(600 * 1024);

    for level in [1, 3, 9, 19] {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };

        let one_shot = encode_all_with_options(&input, options).unwrap();
        let decoded = decode_all(&one_shot).unwrap();
        assert_eq!(decoded, input, "one-shot level {level}");

        let mut encoder = StreamingEncoder::new(options).unwrap();
        let mut streamed = encoder.take_output();
        for chunk in input.chunks(7_777) {
            encoder.push(chunk).unwrap();
            streamed.extend_from_slice(&encoder.take_output());
        }
        encoder.finish().unwrap();
        streamed.extend_from_slice(&encoder.take_output());
        assert_eq!(
            decode_all(&streamed).unwrap(),
            input,
            "streaming level {level}"
        );

        // A streaming frame declares a window rather than a content size, so
        // the decoder must be able to replay it under a window limit set to
        // exactly what the header asks for.
        let FrameHeader::Zstandard(header) = parse_frame_header(&streamed).unwrap() else {
            panic!("expected a zstandard frame");
        };
        let strict = DecoderOptions {
            max_window_size: Some(header.window_size),
            ..Default::default()
        };
        assert_eq!(
            zstandard::decode_all_with_options(&streamed, strict).unwrap(),
            input,
            "streaming level {level} under a window limit equal to its own declaration"
        );
    }
}

/// A frame header may declare any content size it likes. Reserving it
/// directly let a 17-byte input abort the process inside the allocator.
#[test]
fn absurd_declared_content_size_is_an_error_not_an_abort() {
    let mut frame = Vec::new();
    frame.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
    // Frame_Header_Descriptor: FCS_Field_Size = 3 (8 bytes) | Single_Segment_flag
    frame.push(0xE0);
    frame.extend_from_slice(&(1u64 << 46).to_le_bytes());
    // One final RLE block of length 1.
    let block_header: u32 = (1 << 3) | (1 << 1) | 1;
    frame.extend_from_slice(&block_header.to_le_bytes()[..3]);
    frame.push(b'A');

    assert_eq!(frame.len(), 17);
    assert!(
        decode_all(&frame).is_err(),
        "a 17-byte frame declaring 2^46 bytes must be rejected"
    );
}

/// Frames large enough to exceed the level's window must declare a real
/// window rather than a single segment, or the reference implementation
/// refuses them for requiring too much decode memory.
#[test]
fn large_frames_declare_a_bounded_window() {
    let input = build_log_corpus(3 * 1024 * 1024);
    let options = EncoderOptions {
        compression_level: CompressionLevel::try_new(1).unwrap(),
        ..Default::default()
    };
    let compressed = encode_all_with_options(&input, options).unwrap();

    let FrameHeader::Zstandard(header) = parse_frame_header(&compressed).unwrap() else {
        panic!("expected a zstandard frame");
    };
    assert!(
        header.window_size < input.len() as u64,
        "level 1 declared a {}-byte window for {} bytes of content; it only needs 512 KiB of history",
        header.window_size,
        input.len()
    );
    assert_eq!(decode_all(&compressed).unwrap(), input);
}

/// The sequence-bitstream writer accumulates bits in a machine word and
/// flushes whole bytes out of it. C's `ZSTD_encodeSequences_body` flushes
/// once after writing the first sequence's extra bits; omitting that flush
/// left the hot loop starting with a nearly-full accumulator, which
/// overflowed and silently produced a frame no decoder could read.
///
/// Reaching it needs a block whose last sequence carries wide literal-length,
/// match-length and offset fields at once — here a large incompressible run
/// followed by a long match back to the previous period.
#[test]
fn wide_sequence_fields_do_not_overflow_the_bitstream_accumulator() {
    for noise in [4643usize, 4644, 4645, 5000, 8192] {
        let period = 131_072usize;
        let mut body = Vec::with_capacity(period);
        let mut i = 0u64;
        while body.len() < period {
            body.extend_from_slice(
                format!(
                    "2026-07-27T12:00:{:02}Z tmpl=0 seq={i} path=/api/v0/items status=200\n",
                    i % 60
                )
                .as_bytes(),
            );
            i += 1;
        }
        body.truncate(period);

        let mut filler = Vec::with_capacity(noise);
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..noise {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            filler.push((state >> 24) as u8);
        }

        let mut input = Vec::new();
        while input.len() < 260_000 {
            input.extend_from_slice(&body);
            input.extend_from_slice(&filler);
        }

        for level in [16, 17, 19, 20, 22] {
            let compressed = encode_all_with_options(
                &input,
                EncoderOptions {
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    ..Default::default()
                },
            )
            .unwrap();
            let decoded = decode_all(&compressed).unwrap_or_else(|err| {
                panic!("noise {noise}, level {level}: undecodable frame: {err:?}")
            });
            assert_eq!(decoded, input, "noise {noise}, level {level}");
        }
    }
}

/// Deterministic incompressible bytes.
fn incompressible(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push((state >> 24) as u8);
    }
    out
}

/// Data that is locally random but globally periodic: four copies of one
/// incompressible megabyte. Every match sits at a 1 MiB offset, comfortably
/// inside the window from level 3 up.
///
/// A sampling heuristic used to inspect a block, conclude it looked random,
/// and emit it raw without ever consulting history — so the encoder returned
/// the input essentially uncompressed while upstream reached 4:1. Any future
/// "skip the work, this looks incompressible" shortcut has to survive this.
#[test]
fn globally_periodic_incompressible_data_still_compresses() {
    let block = incompressible(1024 * 1024);
    let mut input = Vec::with_capacity(4 * block.len());
    for _ in 0..4 {
        input.extend_from_slice(&block);
    }

    for level in [3, 9, 19] {
        let compressed = encode_all_with_options(
            &input,
            EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(decode_all(&compressed).unwrap(), input, "level {level}");

        // Three of the four megabytes are pure repeats and must collapse.
        assert!(
            compressed.len() < input.len() / 3,
            "level {level} emitted {} bytes for {} bytes that repeat every 1 MiB",
            compressed.len(),
            input.len()
        );
    }
}

/// Truly incompressible input must not expand meaningfully, at any level.
#[test]
fn incompressible_input_does_not_expand() {
    let input = incompressible(512 * 1024);
    for level in [1, 3, 9, 19, 22] {
        let compressed = encode_all_with_options(
            &input,
            EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(decode_all(&compressed).unwrap(), input, "level {level}");
        assert!(
            compressed.len() <= input.len() + input.len() / 128,
            "level {level} expanded {} bytes to {}",
            input.len(),
            compressed.len()
        );
    }
}

/// A frame that bounds its window must still declare how much it decodes to.
///
/// `Single_Segment_flag` and `Frame_Content_Size` are independent header
/// fields. Emitting a window descriptor without a content size produced frames
/// that `ZSTD_decompress` and `ZSTD_getFrameContentSize` reject outright, since
/// they need the size before allocating.
#[test]
fn large_frames_declare_both_a_window_and_a_content_size() {
    let input = build_log_corpus(3 * 1024 * 1024);
    let compressed = encode_all_with_options(
        &input,
        EncoderOptions {
            compression_level: CompressionLevel::try_new(1).unwrap(),
            ..Default::default()
        },
    )
    .unwrap();

    let FrameHeader::Zstandard(header) = parse_frame_header(&compressed).unwrap() else {
        panic!("expected a zstandard frame");
    };
    assert!(
        !header.single_segment,
        "a 3 MiB frame should not claim a single segment at level 1"
    );
    assert!(
        header.window_size < input.len() as u64,
        "window {} should be smaller than the {}-byte payload",
        header.window_size,
        input.len()
    );
    assert_eq!(
        header.content_size,
        Some(input.len() as u64),
        "content size must be declared even when a window is"
    );
    assert_eq!(decode_all(&compressed).unwrap(), input);
}

/// The streaming decoder must reject the same hostile frame the one-shot path
/// does, and for the same reason: a declared content size is a claim, not a
/// measurement.
#[test]
fn streaming_decoder_rejects_absurd_declared_content_size() {
    let mut frame = Vec::new();
    frame.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
    frame.push(0xE0);
    frame.extend_from_slice(&(1u64 << 46).to_le_bytes());
    let block_header: u32 = (1 << 3) | (1 << 1) | 1;
    frame.extend_from_slice(&block_header.to_le_bytes()[..3]);
    frame.push(b'A');

    let mut decoder = StreamingDecoder::new(DecoderOptions::default());
    let pushed = decoder.push(&frame);
    let finished = decoder.finish();
    assert!(
        pushed.is_err() || finished.is_err(),
        "a 17-byte frame declaring 2^46 bytes must be rejected"
    );
}

/// `BinaryTreeFinder::insert` bounded its DUBT traversal below by `btLow`,
/// which is zero until the tree outgrows the chain table. C bounds the same
/// loop by `windowLow`, which it asserts is positive and never lets fall below
/// `ZSTD_WINDOW_START_INDEX`. The zero-filled hash heads therefore steered the
/// walk onto the phantom entries at biased positions 0 and 1, where un-biasing
/// them underflowed: a panic under `debug_assertions`, and a wrapped index
/// that silently truncated the walk otherwise. Ordinary structured text
/// reproduced it at every optimal level, within the first block. The corpus is
/// kept small because the optimal parsers are slow in a debug build and the
/// walk reaches a phantom entry almost immediately.
#[test]
fn optimal_levels_do_not_walk_the_phantom_tree_positions() {
    let input = build_log_corpus(64 * 1024);

    for level in 16..=(CompressionLevel::MAX.as_i32() as u8) {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(i32::from(level)).unwrap(),
            ..Default::default()
        };
        let encoded = encode_all_with_options(&input, options)
            .unwrap_or_else(|err| panic!("level {level} failed to encode: {err:?}"));
        let decoded = decode_all(&encoded)
            .unwrap_or_else(|err| panic!("level {level} produced an undecodable frame: {err:?}"));
        assert_eq!(decoded, input, "level {level} roundtrip mismatch");
    }
}

/// The Huff0 literals decode table was declared with `1 << (TABLELOG_MAX - 1)`
/// entries while everything indexing it assumed `1 << TABLELOG_MAX`. A literals
/// section whose Huffman weights sum to a `table_log` of 12 needs the full
/// size, so building the table wrote past the end of the array — a panic
/// reachable from `decode_all` on 40 bytes of input, which `docs/SEMVER.md`
/// classes as a security bug rather than an API change.
///
/// This frame is the reproducer libFuzzer found in under 30 seconds on the
/// `full_decode` target, the first time that target had ever run. Upstream
/// `zstd` rejects it too, at the literals header; the verdict is what has to
/// agree, not the message.
#[test]
fn table_log_twelve_literals_are_rejected_rather_than_overrunning_the_decode_table() {
    let frame = [
        0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x02, 0xfd, 0x00, 0x00, 0x36, 0x00, 0x06, 0x9f, 0xa4, 0x00,
        0x00, 0x00, 0x00, 0x01, 0x37, 0x00, 0x00, 0x00, 0x01, 0x00, 0x90, 0x60, 0x28, 0xb5, 0x2f,
        0x00, 0x00, 0x00, 0x00, 0x00, 0xa4, 0xa4, 0xb4, 0x03, 0x00,
    ];

    assert!(
        decode_all(&frame).is_err(),
        "a frame whose literals table cannot be built must be rejected, not decoded"
    );
}

/// A four-stream literals section splits its output at multiples of
/// `ceil(regenerated_size / 4)`, which for a regenerated size of 5 puts the
/// fourth segment's start at 6 — past the end. The decode loop writes through
/// `get_unchecked_mut` bounded by those starts, so this was an out-of-bounds
/// write in a release build, not a panic. Upstream rejects the whole range
/// below 6 ("stream 4-split doesn't work"); `huff0` now does too, and this
/// pins the public path.
///
/// The frame is the second reproducer from the `full_decode` fuzz target, found
/// once the table-overrun above stopped masking it. Upstream rejects it as
/// well, at the literals header.
#[test]
fn four_stream_literals_too_small_to_split_are_rejected() {
    let frame = [
        0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0xfd, 0x00, 0x00, 0x26, 0x40, 0x05, 0x83, 0x20, 0x11,
        0x07, 0x00, 0x01, 0x00, 0x01, 0x00, 0xc5, 0x00, 0x00, 0xad, 0x2f, 0xad, 0xad, 0xad, 0x11,
        0x00, 0x56, 0x48, 0x70, 0x03, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
        0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
        0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
        0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96,
        0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96,
        0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96,
        0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96, 0x96,
        0x96, 0x96, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
        0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
        0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
        0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x79, 0x73, 0xff, 0x12, 0x00, 0xbd, 0xc5,
    ];

    assert!(
        decode_all(&frame).is_err(),
        "a four-stream literals section too small to split must be rejected"
    );
}

/// The three frames upstream ships in `tests/golden-decompression-errors/`,
/// inlined so the rejection is asserted on every `cargo test` rather than only
/// where an upstream checkout happens to be present. `tests/upstream_interop.rs`
/// walks that directory and cross-checks the verdicts against the reference
/// decoder; this is the copy that runs unconditionally.
///
/// `truncated_huff_state.zst` was accepted for the life of the project and
/// returned seven bytes of invented output. Its Huffman weight description is
/// an FSE bitstream too short to hold the two initial decoder states, and
/// `src/entropy/fse.rs` omitted the overflow check upstream applies directly
/// after initializing them. That is the worse half of the accept/reject class:
/// not a panic a caller can catch, but `Ok` carrying bytes that were never in
/// the input. The other two were already rejected and are pinned here to keep
/// them that way.
#[test]
fn upstream_golden_decompression_error_frames_are_rejected() {
    let fixtures: [(&str, &[u8]); 3] = [
        (
            "off0.bin.zst",
            &[
                0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x45, 0x00, 0x00, 0x08, 0x00, 0x02, 0x00, 0x2f,
                0x43, 0x0b, 0xae,
            ],
        ),
        (
            "truncated_huff_state.zst",
            &[
                0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x55, 0x00, 0x00, 0x72, 0x80, 0x01, 0x04, 0x20,
                0x7e, 0x1f, 0x02, 0xaa, 0x00,
            ],
        ),
        (
            "zeroSeq_extraneous.zst",
            &[
                0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x95, 0x00, 0x00, 0x68, 0x48, 0x65, 0x6c, 0x6c,
                0x6f, 0x20, 0x57, 0x6f, 0x72, 0x6c, 0x64, 0x21, 0x0a, 0x80, 0x00, 0x00, 0x00,
            ],
        ),
    ];

    // Collected rather than asserted per fixture so a failure names every frame
    // that got through, not just the first.
    let accepted: Vec<&str> = fixtures
        .iter()
        .filter(|(_, frame)| decode_all(frame).is_ok())
        .map(|(name, _)| *name)
        .collect();

    assert!(
        accepted.is_empty(),
        "frames the reference decoder rejects were decoded instead: {accepted:?}"
    );
}

/// A frame with the shape of a decompression bomb: no declared content size,
/// so the decoder cannot reject it from the header and has to stop mid-decode.
fn amplifying_frame_without_content_size() -> (Vec<u8>, Vec<u8>) {
    // Long runs plus a recurring phrase. Not RLE, and not compressible by
    // entropy coding alone — the ratio asserted below is what proves the
    // decoder reaches the FSE sequence path rather than a literals-only or raw
    // block, which is the path this test exists to cover.
    let mut data = Vec::new();
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let alphabet: Vec<u8> = (b'a'..=b'z').collect();
    while data.len() < 1_000_000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let run = 40 + (seed >> 33) as usize % 200;
        let ch = alphabet[((seed >> 17) % alphabet.len() as u64) as usize];
        data.extend(std::iter::repeat_n(ch, run));
        data.extend_from_slice(b"the quick brown fox jumps over the lazy dog ");
    }

    let mut encoder = StreamingEncoder::new(EncoderOptions {
        compression_level: CompressionLevel::try_new(3).unwrap(),
        ..Default::default()
    })
    .unwrap();
    encoder.push(&data).unwrap();
    encoder.finish().unwrap();
    let frame = encoder.take_output();

    let zstandard::FrameHeader::Zstandard(header) = parse_frame_header(&frame).unwrap() else {
        panic!("expected a zstandard frame");
    };
    assert_eq!(
        header.content_size, None,
        "the streaming encoder must leave content size undeclared, or the \
         decoder rejects this from the header and never reaches the sequence path"
    );
    assert!(
        data.len() / frame.len() > 10,
        "{}:1 is not enough amplification to guarantee matches, and without \
         matches there are no sequences to bound",
        data.len() / frame.len()
    );

    (data, frame)
}

/// The output cap has to fire *inside* the sequence loop, and has to say so.
///
/// That loop folds the block-size limit and `max_output_size` into one counter,
/// so when it ran out it reported the block-size limit unconditionally: a
/// caller who set a decompression-bomb limit was told their input was corrupt.
/// The streaming decoder, which checks the two separately, returned
/// `OutputSizeTooLarge` on the same bytes. The two disagreeing is what makes
/// this a defect rather than a wording preference.
#[test]
fn output_cap_fires_on_the_sequence_path_and_is_not_reported_as_corruption() {
    let (data, frame) = amplifying_frame_without_content_size();

    // Uncapped first. If this did not round-trip, everything below would pass
    // for the wrong reason — `Corruption` would have been the honest answer.
    assert_eq!(decode_all(&frame).unwrap(), data);

    for limit in [100_000usize, 300_000, 999_999] {
        let err = zstandard::decode_all_with_options(
            &frame,
            DecoderOptions {
                max_output_size: Some(limit),
                ..Default::default()
            },
        )
        .unwrap_err();
        match err {
            Error::OutputSizeTooLarge {
                output_size,
                max_output_size,
            } => {
                assert_eq!(max_output_size, limit);
                assert!(
                    output_size > limit as u64,
                    "reported size {output_size} does not exceed the limit it broke"
                );
            }
            other => panic!("limit {limit} produced {other:?}, not the output cap"),
        }

        // The streaming decoder must agree on the variant. It reports a
        // coarser `output_size` because it rejects a whole block up front
        // rather than stopping at the sequence that crosses the line; both are
        // truthful about what the frame would have produced.
        let mut streaming = StreamingDecoder::new(DecoderOptions {
            max_output_size: Some(limit),
            ..Default::default()
        });
        let streaming_err = streaming
            .push(&frame)
            .and_then(|()| streaming.finish())
            .unwrap_err();
        assert!(
            matches!(
                streaming_err,
                Error::OutputSizeTooLarge {
                    max_output_size,
                    ..
                } if max_output_size == limit
            ),
            "streaming decoder reported {streaming_err:?} at limit {limit}"
        );
    }

    // The other direction, which matters more: a corrupt frame must not be
    // excused as a cap violation. Attributing it to the cap sends a caller back
    // to a damaged archive with a bigger buffer, and it re-opens the
    // one-shot/streaming divergence from the other side.
    //
    // The mutations are searched for rather than hardcoded. A fixed byte index
    // only reaches this path for one particular frame, so it would quietly
    // stop testing anything the first time the corpus or the parser changed,
    // and the assertion below that any were found is what makes that visible.
    corrupt_frames_are_not_excused_as_cap_violations();

    // Exactly at the boundary the frame is allowed through, so the cap is not
    // off by one in the permissive direction either.
    assert_eq!(
        zstandard::decode_all_with_options(
            &frame,
            DecoderOptions {
                max_output_size: Some(data.len()),
                ..Default::default()
            },
        )
        .unwrap(),
        data
    );
}

/// `single_frame` exists for payloads carried inside another protocol, where
/// the enclosing framing already fixed the length. There, a second frame is not
/// more data; it means the length was wrong. The default stays permissive.
#[test]
fn single_frame_rejects_anything_after_the_first_frame() {
    let payload = build_pattern(50_000);
    let frame = encode_all(&payload).unwrap();
    let strict = DecoderOptions {
        single_frame: true,
        ..Default::default()
    };

    // A lone frame is unaffected.
    assert_eq!(
        zstandard::decode_all_with_options(&frame, strict).unwrap(),
        payload
    );

    // Two concatenated frames: the permissive default concatenates their
    // contents, which is what makes the failure silent without this option.
    let mut doubled = frame.clone();
    doubled.extend_from_slice(&frame);
    let mut expected = payload.clone();
    expected.extend_from_slice(&payload);
    assert_eq!(decode_all(&doubled).unwrap(), expected);

    match zstandard::decode_all_with_options(&doubled, strict).unwrap_err() {
        Error::TrailingInput { offset } => assert_eq!(offset, frame.len()),
        other => panic!("expected TrailingInput, got {other:?}"),
    }

    // A single trailing byte is the framing bug this is really aimed at: too
    // little to be a frame, enough to prove a length was miscomputed.
    let mut trailing = frame.clone();
    trailing.push(0);
    assert!(
        decode_all(&trailing).is_err(),
        "a stray byte is not a frame"
    );
    match zstandard::decode_all_with_options(&trailing, strict).unwrap_err() {
        Error::TrailingInput { offset } => assert_eq!(offset, frame.len()),
        other => panic!("expected TrailingInput, got {other:?}"),
    }

    // Skippable frames are metadata the default passes over. Under
    // `single_frame` they are simply not the one frame that was asked for.
    let skippable = write_skippable_frame(0, &[1, 2, 3, 4]).unwrap();
    let mut leading = skippable.clone();
    leading.extend_from_slice(&frame);
    assert_eq!(decode_all(&leading).unwrap(), payload);
    match zstandard::decode_all_with_options(&leading, strict).unwrap_err() {
        Error::TrailingInput { offset } => assert_eq!(offset, 0),
        other => panic!("expected TrailingInput, got {other:?}"),
    }

    let mut trailing_skippable = frame.clone();
    trailing_skippable.extend_from_slice(&skippable);
    assert_eq!(decode_all(&trailing_skippable).unwrap(), payload);
    match zstandard::decode_all_with_options(&trailing_skippable, strict).unwrap_err() {
        Error::TrailingInput { offset } => assert_eq!(offset, frame.len()),
        other => panic!("expected TrailingInput, got {other:?}"),
    }
}

/// The streaming decoder has to reach the same verdict as the one-shot path,
/// and has to reach it without seeing the whole input at once.
#[test]
fn single_frame_is_enforced_across_streaming_chunk_boundaries() {
    // Deliberately not `build_pattern`, which compresses about 100:1 and would
    // put the whole frame inside one push at every chunk size below. The point
    // here is to drive the frame boundary across many pushes, so the frame has
    // to be big enough to have many.
    let payload = build_huff_friendly_pattern(400_000);
    let frame = encode_all(&payload).unwrap();
    assert!(
        frame.len() > 64 * 1024,
        "frame is {} bytes, too small for the chunk sizes below to mean anything",
        frame.len()
    );
    let mut doubled = frame.clone();
    doubled.extend_from_slice(&frame);

    // Chunk sizes chosen to land the frame boundary inside a chunk, at a chunk
    // edge, and one byte either side of it, since the decoder buffers and
    // compacts input as it goes.
    for chunk in [
        1usize,
        7,
        frame.len() - 1,
        frame.len(),
        frame.len() + 1,
        8192,
    ] {
        let mut decoder = StreamingDecoder::new(DecoderOptions {
            single_frame: true,
            ..Default::default()
        });
        let mut err = None;
        for piece in doubled.chunks(chunk) {
            if let Err(e) = decoder.push(piece) {
                err = Some(e);
                break;
            }
        }
        let err = err
            .or_else(|| decoder.finish().err())
            .unwrap_or_else(|| panic!("chunk size {chunk} accepted two frames"));
        match err {
            Error::TrailingInput { offset } => assert_eq!(
                offset,
                frame.len(),
                "chunk size {chunk} reported the wrong offset"
            ),
            other => panic!("chunk size {chunk} produced {other:?}"),
        }
    }

    // One frame still streams cleanly at every one of those chunk sizes.
    for chunk in [1usize, 7, frame.len() - 1, frame.len(), 8192] {
        let mut decoder = StreamingDecoder::new(DecoderOptions {
            single_frame: true,
            ..Default::default()
        });
        for piece in frame.chunks(chunk) {
            decoder.push(piece).unwrap();
        }
        decoder.finish().unwrap();
        assert_eq!(decoder.take_output(), payload, "chunk size {chunk}");
    }
}

/// What the frame consumed, and what came after it.
///
/// The permissive default cannot answer this, and the way it fails is the
/// argument for the option: it reads the trailing bytes as the next frame's
/// header and reports `BadMagic`. That names the symptom at the wrong layer —
/// nothing is wrong with any magic number, the payload length was wrong.
#[test]
fn streaming_decoder_reports_what_the_frame_consumed_and_what_followed() {
    // Same reason as the chunk-boundary test: this needs a frame that spans
    // many pushes, so that `input_dropped` actually accumulates across
    // compactions rather than being exercised by a single one.
    let payload = build_huff_friendly_pattern(400_000);
    let frame = encode_all(&payload).unwrap();
    assert!(frame.len() > 64 * 1024, "frame is {} bytes", frame.len());
    let tail = b"not part of the frame".to_vec();
    let mut embedded = frame.clone();
    embedded.extend_from_slice(&tail);

    let mut decoder = StreamingDecoder::new(DecoderOptions::default());
    assert!(
        matches!(decoder.push(&embedded), Err(Error::BadMagic(_))),
        "the default decoder is expected to mis-read the tail as a frame header"
    );

    // Strict mode names the actual problem, and leaves the evidence in place.
    let mut decoder = StreamingDecoder::new(DecoderOptions {
        single_frame: true,
        ..Default::default()
    });
    match decoder.push(&embedded).unwrap_err() {
        Error::TrailingInput { offset } => assert_eq!(offset, frame.len()),
        other => panic!("expected TrailingInput, got {other:?}"),
    }
    assert_eq!(decoder.input_consumed(), frame.len());
    assert_eq!(decoder.unconsumed_input(), tail.as_slice());
    // The frame that did decode is still recoverable; the error is about what
    // followed it, not about the frame.
    assert_eq!(decoder.take_output(), payload);

    // The same numbers when the frame spans many pushes, which is what the
    // `input_dropped` accounting is for: compaction drops consumed bytes off
    // the front of the buffer and rewinds it, so a buffer-relative position
    // would be wrong here.
    let mut decoder = StreamingDecoder::new(DecoderOptions {
        single_frame: true,
        ..Default::default()
    });
    let mut hit = None;
    for piece in embedded.chunks(4096) {
        if let Err(err) = decoder.push(piece) {
            hit = Some(err);
            break;
        }
    }
    match hit.expect("chunked push must reject the tail") {
        Error::TrailingInput { offset } => assert_eq!(offset, frame.len()),
        other => panic!("expected TrailingInput, got {other:?}"),
    }
    assert_eq!(decoder.input_consumed(), frame.len());

    // A frame that is the whole input leaves nothing behind: the affirmative
    // form of the same check, and the one a caller runs on good input.
    let mut decoder = StreamingDecoder::new(DecoderOptions::default());
    decoder.push(&frame).unwrap();
    decoder.finish().unwrap();
    assert_eq!(decoder.input_consumed(), frame.len());
    assert!(decoder.unconsumed_input().is_empty());
}

#[test]
fn io_reader_hands_back_the_bytes_it_over_read() {
    use std::io::Read;

    let payload = build_pattern(70_000);
    let frame = encode_all(&payload).unwrap();
    let tail = b"the next message".to_vec();
    let mut source = frame.clone();
    source.extend_from_slice(&tail);

    let mut reader = zstandard::io::Reader::with_options(
        source.as_slice(),
        DecoderOptions {
            single_frame: true,
            ..Default::default()
        },
    );
    let mut decoded = Vec::new();
    let err = reader.read_to_end(&mut decoded).unwrap_err();
    assert!(
        matches!(
            err.get_ref().and_then(|e| e.downcast_ref::<Error>()),
            Some(Error::TrailingInput { offset }) if *offset == frame.len()
        ),
        "expected TrailingInput from the reader, got {err:?}"
    );

    // `into_inner` would report an empty source here: the reader pulled the
    // tail out of it in a fixed-size chunk, so the source's own cursor has
    // already moved past bytes the frame never used. Recovering them is the
    // point of the accessor.
    let (rest, remainder) = reader.into_inner_with_remainder();
    assert!(
        rest.is_empty(),
        "the chunked reader is expected to have drained the source"
    );
    assert_eq!(remainder, tail);
}

/// The other direction of the same attribution, and the one that matters more:
/// a corrupt frame must not be reported as a cap violation. That sends a caller
/// back to a damaged archive with a bigger buffer, and it re-opens the
/// one-shot/streaming divergence from the opposite side.
///
/// The payload is deliberately smaller than one block. In a single-block frame
/// any sequence that overruns the block-size limit necessarily also overruns a
/// cap set just above the honest output size, because the block limit is the
/// larger of the two. That makes the misordered case reachable by construction
/// rather than by luck — on a multi-block payload the same mutation exceeds one
/// block's remainder without coming near a whole-stream cap, which is why an
/// earlier version of this check passed against the bug.
fn corrupt_frames_are_not_excused_as_cap_violations() {
    const BLOCK_LIMIT: &str = "compressed block output exceeds the frame block size limit";

    let mut payload = Vec::new();
    let mut seed = 0x51_7C_C1_B7_27_22_0A_95u64;
    let alphabet: Vec<u8> = (b'a'..=b'z').collect();
    while payload.len() < 100_000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let run = 30 + (seed >> 33) as usize % 120;
        payload.extend(std::iter::repeat_n(
            alphabet[((seed >> 17) % alphabet.len() as u64) as usize],
            run,
        ));
        payload.extend_from_slice(b"pack my box with five dozen liquor jugs ");
    }
    assert!(
        payload.len() < 128 * 1024,
        "the argument above depends on this fitting in one block"
    );

    let mut encoder = StreamingEncoder::new(EncoderOptions {
        compression_level: CompressionLevel::try_new(3).unwrap(),
        ..Default::default()
    })
    .unwrap();
    encoder.push(&payload).unwrap();
    encoder.finish().unwrap();
    let frame = encoder.take_output();
    assert_eq!(decode_all(&frame).unwrap(), payload);

    // The cap is set to exactly the block-size limit, and that is the whole
    // trick. On a single-block frame the two budgets then coincide at every
    // position, so a capped decode aborts at precisely the sequence an uncapped
    // one does, for precisely the same overrun. Any difference in the reported
    // error is therefore attribution and nothing else.
    //
    // A cap just above the payload size does not work here, and an earlier
    // version of this test used one: corruption inflates output, so such a cap
    // is genuinely reachable and the capped decode aborts earlier, at a
    // sequence that really did break only the cap. The two verdicts then differ
    // for a legitimate reason and the test fails against correct code.
    let cap_at_block_limit = DecoderOptions {
        max_output_size: Some(128 * 1024),
        ..Default::default()
    };
    assert_eq!(
        zstandard::decode_all_with_options(&frame, cap_at_block_limit).unwrap(),
        payload,
        "the intact frame must decode under this cap, or it is not the benign cap it claims to be"
    );

    // Searched rather than hardcoded: a fixed byte index only reaches this path
    // for one particular frame, so it would quietly stop testing anything the
    // first time the corpus or the parser moved. The count assertion below is
    // what makes that visible instead of silent.
    let mut checked = 0;
    'search: for index in 0..frame.len() {
        for bit in 0..8u32 {
            let mut corrupt = frame.clone();
            corrupt[index] ^= 1 << bit;
            // Most mutations decode fine or fail some other way; only the ones
            // that overrun the block size limit exercise the ordering.
            let Err(uncapped) =
                zstandard::decode_all_with_options(&corrupt, DecoderOptions::default())
            else {
                continue;
            };
            if uncapped != Error::Corruption(BLOCK_LIMIT) {
                continue;
            }
            let capped =
                zstandard::decode_all_with_options(&corrupt, cap_at_block_limit).unwrap_err();
            assert_eq!(
                capped, uncapped,
                "byte {index} bit {bit}: a corrupt frame was excused as a cap violation"
            );
            checked += 1;
            if checked == 4 {
                break 'search;
            }
        }
    }
    assert_eq!(
        checked, 4,
        "found too few corrupt frames that overrun the block size limit, so the \
         false-exculpation case went untested"
    );
}

// ── Bounded-output encoding ─────────────────────────────────────────────────

#[test]
fn compress_bound_holds_for_every_level_and_shape() {
    // The shapes that matter are the ones that stress a *different* term of
    // the bound: incompressible input maximizes the payload, an RLE run
    // minimizes it while keeping the block count, and an empty input is the
    // case where the block count cannot be derived from the length at all.
    let shapes: [(&str, Vec<u8>); 6] = [
        ("empty", Vec::new()),
        ("one byte", vec![0x5A]),
        ("rle", vec![0xC3; 300_000]),
        ("incompressible", build_incompressible_pattern(300_000)),
        ("compressible", build_pattern(300_000)),
        // Banks enough savings early for the block splitter to engage, then
        // changes byte distribution every 8 KiB so it keeps engaging. This is
        // the only shape here that exercises the splitter's minimum block, and
        // it does not come close to binding the bound — see `compress_bound`
        // for why splitting and incompressibility cannot coexist.
        ("splitter bait", build_splitter_bait_pattern(300_000)),
    ];

    // 1 KiB is the smallest block the encoder accepts and so the highest block
    // count per byte; 128 KiB is the only size at which the splitter runs, and
    // is therefore the only one where the block count is not simply the length
    // divided by the block size.
    let block_sizes = [1_024usize, 16_384, 128 * 1024];

    for (name, input) in &shapes {
        for &block_size in &block_sizes {
            for checksum in [false, true] {
                for level in 1..=22i32 {
                    // The optimal parsers are slow enough that sweeping them
                    // over every shape would dominate the suite; they share the
                    // block and frame framing this bound is about.
                    if level > 9 && input.len() > 1 {
                        continue;
                    }
                    let options = EncoderOptions {
                        block_size,
                        checksum,
                        compression_level: CompressionLevel::try_new(level).unwrap(),
                        ..Default::default()
                    };
                    let encoded = encode_all_with_options(input, options).unwrap();
                    let bound = zstandard::compress_bound(input.len(), options);
                    assert!(
                        encoded.len() <= bound,
                        "{name} at level {level}, block_size {block_size}, checksum {checksum}: \
                         emitted {} bytes against a bound of {bound}",
                        encoded.len()
                    );
                }
            }
        }
    }
}

/// Compressible enough at the front to bank the savings the block splitter
/// gates on, then alternating byte distributions every 8 KiB so the splitter
/// keeps finding a boundary to cut on.
fn build_splitter_bait_pattern(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    out.extend(std::iter::repeat_n(b'A', 16 * 1024));
    let mut run = 0u32;
    while out.len() < size {
        let chunk = build_incompressible_pattern(8 * 1024);
        if run.is_multiple_of(2) {
            out.extend(chunk);
        } else {
            // Half the alphabet, so the fingerprint of this chunk is a long
            // way from its neighbour's and the splitter cuts between them.
            out.extend(chunk.into_iter().map(|byte| byte & 0x7f));
        }
        run += 1;
    }
    out.truncate(size);
    out
}

#[test]
fn compress_bound_stays_tight_on_the_shape_that_binds_it() {
    // A bound is only useful if it is close. Nothing above fails when this one
    // is widened, so without this test the obvious repair for a future
    // `compress_bound` failure is to add slack until it passes — which would
    // keep every other assertion green while quietly turning a bound callers
    // size real buffers with into a guess.
    //
    // Incompressible input at the smallest block size is where the payload and
    // per-block terms are both exactly binding, and the whole remaining gap is
    // the frame-header allowance. Measured: 9 bytes on a 1 MB frame. The
    // ceiling is 32, which is under the 18-byte header allowance plus a block
    // header and leaves no room to absorb a per-block error.
    let input = build_incompressible_pattern(1_000_000);
    let options = EncoderOptions {
        block_size: 1_024,
        checksum: true,
        ..Default::default()
    };

    let encoded = encode_all_with_options(&input, options).unwrap();
    let bound = zstandard::compress_bound(input.len(), options);
    let slack = bound - encoded.len();

    assert!(
        encoded.len() <= bound,
        "the bound must hold before it can be judged tight: {} vs {bound}",
        encoded.len()
    );
    assert!(
        slack <= 32,
        "compress_bound has gone slack: {slack} bytes unused on a {}-byte frame. \
         If an encoder change made this necessary, re-derive the bound rather \
         than raising this ceiling.",
        encoded.len()
    );
}

#[test]
fn a_buffer_sized_by_compress_bound_always_fits_the_frame() {
    for size in [0usize, 1, 7, 1_024, 130_000, 300_000] {
        for (name, input) in [
            ("incompressible", build_incompressible_pattern(size)),
            ("compressible", build_pattern(size)),
        ] {
            for block_size in [1_024usize, 128 * 1024] {
                let options = EncoderOptions {
                    block_size,
                    checksum: true,
                    ..Default::default()
                };
                let mut buffer = vec![0u8; zstandard::compress_bound(input.len(), options)];
                let written = zstandard::encode_into_slice(&input, &mut buffer, options)
                    .unwrap_or_else(|error| {
                        panic!("{name} at {size} bytes, block_size {block_size}: {error:?}")
                    });
                assert_eq!(decode_all(&buffer[..written]).unwrap(), input);
            }
        }
    }
}

#[test]
fn encode_into_slice_matches_encode_all_byte_for_byte() {
    // The fixed sink counts bytes it could not store, so the encoder's block
    // sizing and header offsets are identical to the growable path's. If that
    // ever stops being true this is the test that says so, and it says it as a
    // byte difference rather than as a ratio drift nobody would notice.
    for size in [0usize, 1, 4_096, 200_000] {
        for level in [1i32, 3, 9, 19] {
            let input = build_pattern(size);
            let options = EncoderOptions {
                checksum: true,
                compression_level: CompressionLevel::try_new(level).unwrap(),
                ..Default::default()
            };
            let expected = encode_all_with_options(&input, options).unwrap();

            let mut buffer = vec![0u8; zstandard::compress_bound(input.len(), options)];
            let written = zstandard::encode_into_slice(&input, &mut buffer, options).unwrap();
            assert_eq!(
                &buffer[..written],
                &expected[..],
                "level {level} at {size} bytes: slice encode diverged from the allocating path"
            );
        }
    }
}

#[test]
fn encode_into_slice_reports_too_small_rather_than_truncating() {
    let input = build_pattern(100_000);
    let options = EncoderOptions::default();
    let full = encode_all_with_options(&input, options).unwrap();

    // One byte short of the frame is the interesting case: the encode runs to
    // completion and only the last write does not fit. Anything that reported
    // success here would hand back a frame missing its tail, which still parses
    // as a frame right up until the truncated block.
    for capacity in [0usize, 1, 13, full.len() / 2, full.len() - 1] {
        let mut buffer = vec![0u8; capacity];
        assert_eq!(
            zstandard::encode_into_slice(&input, &mut buffer, options),
            Err(Error::DstSizeTooSmall),
            "a {capacity}-byte buffer must not accept a {}-byte frame",
            full.len()
        );
    }

    let mut exact = vec![0u8; full.len()];
    let written = zstandard::encode_into_slice(&input, &mut exact, options).unwrap();
    assert_eq!(written, full.len());
    assert_eq!(
        exact, full,
        "a buffer of exactly the frame size must succeed"
    );
}

#[test]
fn a_failed_slice_encode_leaves_the_encoder_usable() {
    // The fixed sink carries an overflow count. If a failed encode left it set,
    // the next call on the same Encoder would report DstSizeTooSmall on a
    // buffer that was never too small — and the sink is per-call state, so this
    // is the test that pins it there.
    let input = build_pattern(50_000);
    let options = EncoderOptions::default();
    let mut encoder = Encoder::new();

    let mut too_small = vec![0u8; 16];
    assert_eq!(
        encoder.encode_into_slice(&input, &mut too_small, options),
        Err(Error::DstSizeTooSmall)
    );

    let mut buffer = vec![0u8; zstandard::compress_bound(input.len(), options)];
    let written = encoder
        .encode_into_slice(&input, &mut buffer, options)
        .unwrap();
    assert_eq!(decode_all(&buffer[..written]).unwrap(), input);
}

#[test]
fn reusing_an_encoder_for_slice_encodes_gives_identical_frames() {
    let options = EncoderOptions::default();
    let mut encoder = Encoder::new();
    let inputs = [
        build_pattern(9_000),
        vec![0x11; 5_000],
        build_pattern(9_000),
    ];

    let mut frames = Vec::new();
    for input in &inputs {
        let mut buffer = vec![0u8; zstandard::compress_bound(input.len(), options)];
        let written = encoder
            .encode_into_slice(input, &mut buffer, options)
            .unwrap();
        buffer.truncate(written);
        assert_eq!(decode_all(&buffer).unwrap(), *input);
        frames.push(buffer);
    }
    assert_eq!(
        frames[0], frames[2],
        "the same input through a reused encoder must produce the same frame"
    );
}

#[test]
fn slice_encoding_works_with_a_prepared_dictionary() {
    let dictionary_bytes = build_pattern(8_192);
    let dictionary = EncoderDictionary::new(&dictionary_bytes).unwrap();
    let decoding = DecoderDictionary::new(&dictionary_bytes).unwrap();
    let input = build_pattern(20_000);
    let options = EncoderOptions::default();

    let expected = encode_all_with_prepared_dict(&input, &dictionary).unwrap();

    let mut buffer = vec![0u8; zstandard::compress_bound(input.len(), options)];
    let written = Encoder::new()
        .encode_into_slice_with_prepared_dict_and_options(&input, &mut buffer, &dictionary, options)
        .unwrap();

    assert_eq!(&buffer[..written], &expected[..]);
    assert_eq!(
        decode_all_with_prepared_dict(&buffer[..written], &decoding).unwrap(),
        input
    );
}

/// Text records with a drifting numeric field.
///
/// The shape matters and `build_pattern` will not do: reproducing the stale
/// row-table read needs a bucket whose surviving tag collision carries a far
/// position, and that depends on the byte distribution. `build_pattern`'s
/// arithmetic ramp does not produce one, so a version of the test written
/// against it passed with the fix reverted.
fn build_record_text_pattern(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    let mut counter = 0u64;
    while out.len() < size {
        out.extend_from_slice(
            format!("record {counter} field=value other={}\n", counter % 97).as_bytes(),
        );
        counter += 1;
    }
    out.truncate(size);
    out
}

/// Records with a *random* field, and pseudorandom bytes.
///
/// A third and fourth shape, because the stale-row case below is as sensitive
/// to the byte distribution as the out-of-bounds one above: it needs a row the
/// previous frame filed under a tag the current frame also produces, holding a
/// position whose bytes do not match. `build_pattern`'s ramp and
/// `build_record_text_pattern`'s drifting field are both too regular to leave
/// one -- every helper pairing of those two encodes identically whether the
/// tables were cleared or not.
fn build_random_field_records(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size + 32);
    let mut x: u32 = 0xABCD_1234;
    while out.len() < size {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.extend_from_slice(format!("rec {:07} f={}\n", out.len(), x % 1000).as_bytes());
    }
    out.truncate(size);
    out
}

fn build_incompressible_bytes(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size + 4);
    let mut x: u32 = 0xABCD_1234 ^ 7u32.wrapping_mul(0x9E37_79B9);
    while out.len() < size {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(size);
    out
}

#[test]
fn reusing_an_encoder_across_a_long_then_short_frame_stays_in_bounds() {
    // `Encoder` exists to be reused, and its row match finder used to keep its
    // position table across frames, relying on a rotated hash salt to
    // invalidate the old tags. Tag collisions got through that, and the entry
    // they carried was a position in the *previous* frame -- which, after a
    // long frame, can be far past the end of the current source. Both the
    // candidate prefetch and the match-length count take that index unchecked.
    //
    // `RowHashFinder::reset` now clears that table, so this no longer has a
    // stale entry to find. It is kept because the bound it established in
    // `row_collect_match_indices_*` is what makes an unchecked index safe, and
    // because nothing else here reuses an `Encoder` across two sizes.
    //
    // Encoding a long frame and then a strict prefix of it is the shape that
    // reproduces it: identical bytes hash to identical buckets, so the entries
    // most likely to survive are exactly the ones holding far-away positions.
    // Before the upper-bound filter in `row_collect_match_indices_*` this read
    // up to 864 KB past the end of a 128 KiB slice, in release, on levels 4-6
    // and 9-10. It is a debug assertion failure and an out-of-bounds read
    // respectively, and no other test in this suite reuses an `Encoder` across
    // two sizes, which is why it survived.
    let long = build_record_text_pattern(1 << 20);

    for level in [4i32, 5, 6, 9, 10] {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };
        for short_len in [512usize, 2_048, 16 * 1024, 64 * 1024, 256 * 1024] {
            let mut encoder = Encoder::new();
            let short = &long[..short_len];

            encoder.encode_all_with_options(&long, options).unwrap();
            let reused = encoder.encode_all_with_options(short, options).unwrap();

            // Correctness, not just survival: a reused encoder must produce the
            // frame a fresh one would. A filter that dropped legitimate
            // candidates as well as stale ones would still pass a round-trip.
            let fresh = Encoder::new()
                .encode_all_with_options(short, options)
                .unwrap();
            assert_eq!(
                reused, fresh,
                "level {level}, {short_len}-byte frame after a 1 MiB one: a reused \
                 encoder diverged from a fresh one"
            );
            assert_eq!(decode_all(&reused).unwrap(), short);
        }
    }
}

/// Changing a parameter between two encodes on one `Encoder` gives the frame a
/// fresh encoder would, for every finder.
///
/// The finders keep clamped copies of `min_match`, `chain_log` and
/// `search_log`, and none of their `reset` methods restores one. Reuse was
/// gated on a hand-listed subset of the parameters -- table geometry, mostly
/// `hash_bits` -- so four of the five took a state built for one set and
/// parsed the next frame under another. Only `Fast` checked `min_match`. The
/// frames still round-tripped, which is why nothing caught it: they were valid
/// frames, just not the ones the parameters asked for, and each came out at
/// exactly the size the *previous* parameters produce.
///
/// Every row is chosen to be live rather than merely plausible: each pair
/// produces two different frames from a *fresh* encoder, which is what makes
/// the reused encode able to get it wrong. A pair that compresses to the same
/// bytes either way proves nothing, and several obvious ones do -- on this
/// data a hash chain gives the same frame at `min_match` 4 and 7, and 2^16
/// against 2^20 of `chain_log` is no difference at all when 32 KiB of source
/// fits in both.
#[test]
fn switching_parameters_on_a_reused_encoder_matches_a_fresh_one() {
    let data = build_record_text_pattern(32 * 1024);

    let with = |strategy: Strategy,
                row: RowMatchFinderMode,
                min_match: Option<u32>,
                chain_log: Option<u32>,
                search_log: Option<u32>| EncoderOptions {
        compression_level: CompressionLevel::try_new(7).unwrap(),
        parameters: ParameterOverrides {
            strategy: Some(strategy),
            use_row_match_finder: row,
            min_match,
            chain_log,
            search_log,
            ..Default::default()
        },
        ..Default::default()
    };
    // `Disabled` where the strategy would otherwise be diverted to the row
    // finder, so that the row named in the label is the finder under test.
    let off = RowMatchFinderMode::Disabled;
    let on = RowMatchFinderMode::Enabled;

    // Every variant `ContiguousBlockMatchState` can build, against the
    // parameter each one stores a clamped copy of.
    for (label, first, second) in [
        (
            "fast, min_match",
            with(Strategy::Fast, off, Some(4), None, None),
            with(Strategy::Fast, off, Some(6), None, None),
        ),
        (
            "double-fast, min_match",
            with(Strategy::DoubleFast, off, Some(4), None, None),
            with(Strategy::DoubleFast, off, Some(6), None, None),
        ),
        (
            "hash chain, chain_log",
            with(Strategy::Greedy, off, None, Some(10), None),
            with(Strategy::Greedy, off, None, Some(20), None),
        ),
        (
            "row, search_log",
            with(Strategy::Lazy2, on, None, None, Some(4)),
            with(Strategy::Lazy2, on, None, None, Some(6)),
        ),
        (
            "binary tree, min_match",
            with(Strategy::BinaryTreeLazy2, off, Some(4), None, None),
            with(Strategy::BinaryTreeLazy2, off, Some(6), None, None),
        ),
        (
            "binary tree, chain_log",
            with(Strategy::BinaryTreeLazy2, off, None, Some(11), None),
            with(Strategy::BinaryTreeLazy2, off, None, Some(20), None),
        ),
        (
            "optimal, chain_log",
            with(Strategy::BinaryTreeOpt, off, None, Some(11), None),
            with(Strategy::BinaryTreeOpt, off, None, Some(20), None),
        ),
    ] {
        let fresh_first = Encoder::new()
            .encode_all_with_options(&data, first)
            .unwrap();
        let fresh = Encoder::new()
            .encode_all_with_options(&data, second)
            .unwrap();
        assert_ne!(
            fresh_first, fresh,
            "{label}: the two parameter sets give the same frame, so this row \
             cannot tell a stale state from a fresh one"
        );

        let mut encoder = Encoder::new();
        encoder.encode_all_with_options(&data, first).unwrap();
        let reused = encoder.encode_all_with_options(&data, second).unwrap();
        assert_eq!(
            reused, fresh,
            "{label}: a reused encoder carried the first encode's parameters into the second"
        );
        assert_eq!(decode_all(&reused).unwrap(), data);

        // And back, so a row proves the reuse is symmetric rather than that
        // one of the two parameter sets happens to win.
        let back = encoder.encode_all_with_options(&data, first).unwrap();
        assert_eq!(
            back, fresh_first,
            "{label}: reverting the parameter diverged"
        );
    }
}

/// A reused encoder produces the same frame as a fresh one when the row match
/// finder is in use.
///
/// A dictionary offset does not outlive the window it was found in.
///
/// A dictionary may be referenced in full while its last byte is still inside
/// the window, so an offset found then can legally exceed `Window_Size` -- the
/// decoder holds the dictionary outside the window. What it cannot do is
/// survive: once the frame has run far enough that C's `ZSTD_checkDictValidity`
/// retires the dictionary, that offset addresses nothing either side can
/// reach, and repeating it produces a frame this crate's own decoder rejects
/// with `sequence offset exceeds the available history window`.
///
/// C retires the carried repeat offsets against the block's window at the top
/// of every parser, under `if (dictMode == ZSTD_noDict)`. The guard is narrower
/// than it reads: `dictMode` is recomputed per block, so a block whose
/// dictionary has just been retired takes that clamp. This crate kept its
/// prefixed parsers on the prefixed path across retirement and so skipped it.
///
/// `window_log` 10 against a 1408-byte body is what reaches it: the blocks are
/// capped at the window, so the first ends at exactly 1024 and stays live --
/// `blockEndIdx > maxDist + loadedDictEnd` is `1069 > 1069`, false -- while the
/// second is retired. An offset of 1025 stored at source 989 in the first was
/// repeated at source 1025 in the second, one byte past the window.
#[test]
fn a_dictionary_offset_does_not_outlive_the_window_it_was_found_in() {
    // The fuzzer's input, for the reason the row-salt one is kept: the offset
    // has to be found in the live block and still be `offset_1` at the first
    // position of the retired one.
    const REPRO: &[u8] =
        include_bytes!("../dev/repro/dictionary-offset-outlives-window-fuzzer-found.bin");
    let dictionary = &REPRO[8..53];
    let body = &REPRO[53..];
    assert_eq!(body.len(), 1408, "the reproducer changed shape");

    let options = EncoderOptions {
        write_dict_id: false,
        write_content_size: false,
        compression_level: CompressionLevel::try_new(7).unwrap(),
        parameters: ParameterOverrides {
            window_log: Some(10),
            ..Default::default()
        },
        ..Default::default()
    };
    let encoder_dictionary = EncoderDictionary::new(dictionary).unwrap();
    let decoder_dictionary = DecoderDictionary::new(dictionary).unwrap();

    // Both finders, since the defect is in the floor rather than in any one
    // parser: forcing the row finder off reproduced it just as well.
    for row in [
        RowMatchFinderMode::Enabled,
        RowMatchFinderMode::Disabled,
        RowMatchFinderMode::Auto,
    ] {
        let options = EncoderOptions {
            parameters: ParameterOverrides {
                use_row_match_finder: row,
                ..options.parameters
            },
            ..options
        };
        let frame =
            encode_all_with_prepared_dict_and_options(body, &encoder_dictionary, options).unwrap();
        let decoded = decode_all_with_prepared_dict(&frame, &decoder_dictionary)
            .unwrap_or_else(|e| panic!("{row:?}: this crate refused its own frame: {e}"));
        assert_eq!(decoded, body, "{row:?}: dictionary round trip");
    }

    // The window that reproduces it is the narrow one, but nothing about the
    // fix is specific to it: a window the body fits inside never retires the
    // dictionary at all, and one below that retires it earlier.
    for window_log in [10u32, 11, 12, 14, 17] {
        let options = EncoderOptions {
            parameters: ParameterOverrides {
                window_log: Some(window_log),
                ..options.parameters
            },
            ..options
        };
        let frame =
            encode_all_with_prepared_dict_and_options(body, &encoder_dictionary, options).unwrap();
        assert_eq!(
            decode_all_with_prepared_dict(&frame, &decoder_dictionary).unwrap(),
            body,
            "window_log {window_log}"
        );
    }
}

/// `RowHashFinder::reset` used to rotate its hash salt rather than restore it,
/// and the salt feeds every row hash, so the second frame filed the same bytes
/// into different rows and came out a byte longer. No test above reached that:
/// they either force the row finder off, or leave the parameters at the
/// defaults, under which no level they sweep diverges.
///
/// The body is the fuzzer's, not a synthetic stand-in, because the divergence
/// needs a surviving tag collision and that depends on the exact bytes. Eight
/// pattern generators over eight sizes, eight levels and every `min_match`
/// reproduced it on none of them; `dictionary_encode_roundtrip` found it in
/// five minutes. What the fuzzer's input contributes is the collision, so the
/// file is kept rather than reduced to a generator.
///
/// The two overrides below are the load-bearing ones, established by dropping
/// each in turn: without `min_match` or with the finder left at `Auto` the
/// frames agree. The dictionary the target used is not among them -- an empty
/// one takes the same contiguous path as none at all -- so this reaches the
/// defect through the plain entry point.
///
/// The fix has two halves and this covers both, in two sections, because
/// either half alone passes the other's. Pinning the salt is what fixes the
/// repeated encodes; clearing the tables is what fixes a frame that follows a
/// different one, and needs its own body to reach.
#[test]
fn reusing_an_encoder_with_the_row_match_finder_reproduces_a_fresh_encode() {
    // `dev/repro/row-hash-salt-reuse-fuzzer-found.bin` is a fuzz target input:
    // eight control bytes, then the body seed. Its dictionary split byte is
    // zero, so the remaining 448 bytes are the body the target encoded.
    const REPRO: &[u8] = include_bytes!("../dev/repro/row-hash-salt-reuse-fuzzer-found.bin");
    let body = &REPRO[8..];
    assert_eq!(body.len(), 448, "the reproducer changed size");

    let options = EncoderOptions {
        compression_level: CompressionLevel::try_new(7).unwrap(),
        parameters: ParameterOverrides {
            min_match: Some(7),
            use_row_match_finder: RowMatchFinderMode::Enabled,
            ..Default::default()
        },
        ..Default::default()
    };

    let fresh = Encoder::new()
        .encode_all_with_options(body, options)
        .unwrap();

    // Three rounds rather than two. A salt that rotated produced 108, 109, 108
    // bytes, so a second encode alone reads as a state that merely alternates;
    // the third is what shows the frame is fixed rather than periodic.
    let mut encoder = Encoder::new();
    for round in 0..3 {
        let reused = encoder.encode_all_with_options(body, options).unwrap();
        assert_eq!(
            reused, fresh,
            "encode {round} on a reused encoder diverged from a fresh one"
        );
        assert_eq!(decode_all(&reused).unwrap(), body);
    }

    // The half of the fix that clears the tables. The rounds above do not reach
    // it: they re-encode one body, so a table kept from the previous frame
    // holds what this frame would have filed anyway, and pinning the salt alone
    // passes them. A stale entry only changes the parse when the previous frame
    // filed *different* bytes at a position this one searches past -- entries
    // at or beyond the search position are rejected already, so the previous
    // frame also has to be no longer than this one.
    //
    // Hence a different body of the same size. With the salt pinned and the
    // tables kept this comes out at 1199 bytes against a fresh encoder's 1197.
    let target = build_random_field_records(4096);
    let previous = build_incompressible_bytes(4096);
    let stale_options = EncoderOptions {
        compression_level: CompressionLevel::try_new(7).unwrap(),
        parameters: ParameterOverrides {
            strategy: Some(Strategy::Greedy),
            min_match: Some(6),
            use_row_match_finder: RowMatchFinderMode::Enabled,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut encoder = Encoder::new();
    encoder
        .encode_all_with_options(&previous, stale_options)
        .unwrap();
    let after_previous = encoder
        .encode_all_with_options(&target, stale_options)
        .unwrap();
    let target_fresh = Encoder::new()
        .encode_all_with_options(&target, stale_options)
        .unwrap();
    assert_eq!(
        after_previous, target_fresh,
        "a frame following a different one of the same size diverged from a fresh encode"
    );
    assert_eq!(decode_all(&after_previous).unwrap(), target);
}

/// A block whose matchable content starts after its first eighth still gets
/// parsed.
///
/// The fast and double-fast parsers used to give up on a block outright once
/// they had scanned `block_len / 8` positions without finding a single match,
/// on the theory that the block was incompressible. The theory does not hold
/// for a block that *starts* incompressible and turns compressible later, which
/// is the ordinary shape of a log with an embedded binary blob, or of any
/// stream whose block boundaries do not line up with its content. Everything
/// past the give-up point became literals.
///
/// Upstream has no such early exit in `zstd_fast.c` or `zstd_double_fast.c`,
/// and removing ours left every one-shot corpus row byte-identical, so it was
/// only ever firing on this shape.
#[test]
fn a_block_whose_matches_start_late_is_still_compressed() {
    // Sized so the second block opens on a run of noise longer than its own
    // first eighth (16 KiB), and only then reaches content that repeats the
    // first block. Land the noise short of that and the parser finds a match
    // before the give-up point, and the test cannot fail.
    let block_size = 128 * 1024;
    let body = build_pattern(120 * 1024);
    let noise = build_incompressible_pattern(40 * 1024);

    let mut input = Vec::new();
    input.extend_from_slice(&body);
    input.extend_from_slice(&noise);
    input.extend_from_slice(&body);

    let noise_left_in_second_block = body.len() + noise.len() - block_size;
    assert!(
        noise_left_in_second_block > block_size / 8,
        "the second block must open on more noise than its early-exit window, \
         or this test asserts nothing: {noise_left_in_second_block} bytes"
    );

    for level in [1, 2, 3, 4] {
        let options = EncoderOptions {
            block_size,
            compression_level: CompressionLevel::try_new(level).unwrap(),
            ..Default::default()
        };
        let encoded = encode_all_with_options(&input, options).unwrap();
        assert_eq!(decode_all(&encoded).unwrap(), input);

        // The repeated body is 120 KiB that the encoder has already seen, so a
        // parser that reaches it spends almost nothing on it: the whole frame
        // should not cost much more than the noise it genuinely has to store.
        // Giving up on the second block instead cost 96 KiB of literals here.
        let ceiling = noise.len() + 24 * 1024;
        assert!(
            encoded.len() <= ceiling,
            "level {level}: {} bytes for {} of input, over the {ceiling}-byte ceiling; \
             the second block's matches were not found",
            encoded.len(),
            input.len()
        );
    }
}

/// Every compression parameter rejects both ends of its range, and accepts
/// both edges of it.
///
/// The bounds are public, so this walks them rather than repeating the
/// numbers: a constant and a check that disagreed would otherwise both have to
/// be edited to stay wrong.
#[test]
fn parameter_overrides_reject_values_outside_their_bounds() {
    struct Field {
        name: &'static str,
        bounds: ParameterBounds,
        set: fn(u32) -> ParameterOverrides,
    }

    let fields = [
        Field {
            name: "window_log",
            bounds: ParameterOverrides::WINDOW_LOG,
            set: |value| ParameterOverrides {
                window_log: Some(value),
                ..Default::default()
            },
        },
        Field {
            name: "hash_log",
            bounds: ParameterOverrides::HASH_LOG,
            set: |value| ParameterOverrides {
                hash_log: Some(value),
                ..Default::default()
            },
        },
        Field {
            name: "chain_log",
            bounds: ParameterOverrides::CHAIN_LOG,
            set: |value| ParameterOverrides {
                chain_log: Some(value),
                ..Default::default()
            },
        },
        Field {
            name: "search_log",
            bounds: ParameterOverrides::SEARCH_LOG,
            set: |value| ParameterOverrides {
                search_log: Some(value),
                ..Default::default()
            },
        },
        Field {
            name: "min_match",
            bounds: ParameterOverrides::MIN_MATCH,
            set: |value| ParameterOverrides {
                min_match: Some(value),
                ..Default::default()
            },
        },
        Field {
            name: "target_length",
            bounds: ParameterOverrides::TARGET_LENGTH,
            set: |value| ParameterOverrides {
                target_length: Some(value),
                ..Default::default()
            },
        },
    ];

    let input = b"parameter bounds are checked before anything is encoded";
    for field in fields {
        for offending in [
            field.bounds.min.checked_sub(1),
            field.bounds.max.checked_add(1),
            Some(u32::MAX),
        ]
        .into_iter()
        .flatten()
        {
            assert!(
                !field.bounds.contains(offending),
                "{}: {offending} is inside the bounds it was chosen to sit outside",
                field.name
            );
            let options = EncoderOptions {
                parameters: (field.set)(offending),
                ..Default::default()
            };
            assert!(
                matches!(
                    encode_all_with_options(input, options),
                    Err(Error::InvalidParameter(_))
                ),
                "{} accepted {offending}",
                field.name
            );
            // The streaming encoder shares the check, so it has to report the
            // same thing rather than building a context around bad parameters.
            assert!(
                matches!(
                    StreamingEncoder::new(options),
                    Err(Error::InvalidParameter(_))
                ),
                "{} accepted {offending} on the streaming path",
                field.name
            );
        }

        for accepted in [field.bounds.min, field.bounds.max] {
            let options = EncoderOptions {
                parameters: (field.set)(accepted),
                ..Default::default()
            };
            let encoded = encode_all_with_options(input, options)
                .unwrap_or_else(|error| panic!("{} rejected {accepted}: {error}", field.name));
            assert_eq!(decode_all(&encoded).unwrap(), input, "{}", field.name);
        }
    }
}

/// The window ceiling is the largest window this crate will declare, so a
/// caller reading the bounds cannot ask for a frame the reference decoder
/// refuses at its default settings.
#[test]
fn the_window_log_ceiling_matches_the_largest_declarable_window() {
    assert_eq!(
        1u64 << ParameterOverrides::WINDOW_LOG.max,
        DecoderOptions::DEFAULT_MAX_WINDOW_SIZE
    );
    assert_eq!(ParameterOverrides::WINDOW_LOG.min, 10);
}

/// Every strategy round-trips at every level family, and produces a frame the
/// crate's own decoder reads.
#[test]
fn every_strategy_override_round_trips() {
    let input = build_structured_log_pattern(60_000);
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
        for level in [-5, 1, 9, 22] {
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                parameters: ParameterOverrides {
                    strategy: Some(strategy),
                    ..Default::default()
                },
                ..Default::default()
            };
            let encoded = encode_all_with_options(&input, options).unwrap();
            assert_eq!(
                decode_all(&encoded).unwrap(),
                input,
                "{strategy:?} at level {level}"
            );

            let mut encoder = StreamingEncoder::new(options).unwrap();
            let mut streamed = Vec::new();
            for chunk in input.chunks(7_777) {
                encoder.push(chunk).unwrap();
                streamed.extend_from_slice(&encoder.take_output());
            }
            encoder.finish().unwrap();
            streamed.extend_from_slice(&encoder.take_output());
            assert_eq!(
                decode_all(&streamed).unwrap(),
                input,
                "{strategy:?} at level {level}, streaming"
            );
        }
    }
}

/// `Strategy`'s discriminants are upstream's `ZSTD_strategy` values, and its
/// ordering runs cheapest to most thorough.
#[test]
fn strategy_values_match_upstream() {
    let ordered = [
        (Strategy::Fast, 1),
        (Strategy::DoubleFast, 2),
        (Strategy::Greedy, 3),
        (Strategy::Lazy, 4),
        (Strategy::Lazy2, 5),
        (Strategy::BinaryTreeLazy2, 6),
        (Strategy::BinaryTreeOpt, 7),
        (Strategy::BinaryTreeUltra, 8),
        (Strategy::BinaryTreeUltra2, 9),
    ];
    for (strategy, value) in ordered {
        assert_eq!(strategy.as_u32(), value, "{strategy:?}");
    }
    for pair in ordered.windows(2) {
        assert!(pair[0].0 < pair[1].0, "{:?} < {:?}", pair[0].0, pair[1].0);
    }
}

/// A magicless frame is four bytes shorter and needs a decoder told to expect
/// one.
#[test]
fn magicless_frames_need_a_matching_decoder() {
    let input = build_structured_log_pattern(20_000);
    let magicless = encode_all_with_options(
        &input,
        EncoderOptions {
            format: Format::Zstd1Magicless,
            ..Default::default()
        },
    )
    .unwrap();
    let magicless_options = DecoderOptions {
        format: Format::Zstd1Magicless,
        ..Default::default()
    };

    assert_eq!(
        decode_all_with_options(&magicless, magicless_options).unwrap(),
        input
    );

    // A magicless frame cannot be recognised, only asserted. Handing one to a
    // default decoder has to fail rather than half-decode: the first four
    // bytes are the frame header descriptor and the start of the first block,
    // and reading those as a magic number is not a Zstandard frame.
    assert!(decode_all(&magicless).is_err());
    // And the reverse: a standard frame read as magicless mis-parses its own
    // magic number as a header.
    let standard = encode_all_with_options(&input, EncoderOptions::default()).unwrap();
    assert!(decode_all_with_options(&standard, magicless_options).is_err());

    // The header parser follows the same rule, and reports the size of the
    // header it actually read.
    let FrameHeader::Zstandard(magicless_header) =
        parse_frame_header_with_format(&magicless, Format::Zstd1Magicless).unwrap()
    else {
        panic!("expected a Zstandard frame");
    };
    let FrameHeader::Zstandard(standard_header) = parse_frame_header(&standard).unwrap() else {
        panic!("expected a Zstandard frame");
    };
    assert_eq!(
        magicless_header.header_size + 4,
        standard_header.header_size
    );
    assert_eq!(magicless_header.content_size, standard_header.content_size);
    assert_eq!(magicless_header.window_size, standard_header.window_size);
}

/// The one-shot entry points know the real length, so a pledge that disagrees
/// with it is a caller bug rather than something to silently prefer one way.
#[test]
fn a_one_shot_pledge_must_match_the_input() {
    let input = build_structured_log_pattern(8_000);
    for pledged in [0u64, 7_999, 8_001] {
        assert!(
            matches!(
                encode_all_with_options(
                    &input,
                    EncoderOptions {
                        pledged_src_size: Some(pledged),
                        ..Default::default()
                    }
                ),
                Err(Error::InvalidParameter(_))
            ),
            "a pledge of {pledged} against {} bytes was accepted",
            input.len()
        );
    }

    let encoded = encode_all_with_options(
        &input,
        EncoderOptions {
            pledged_src_size: Some(input.len() as u64),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(decode_all(&encoded).unwrap(), input);
}

/// A stream reused for a second frame re-checks its pledge instead of carrying
/// the first frame's byte count forward.
#[test]
fn a_reset_stream_re_checks_its_pledge() {
    let input = build_structured_log_pattern(5_000);
    let options = EncoderOptions {
        pledged_src_size: Some(input.len() as u64),
        ..Default::default()
    };
    let mut encoder = StreamingEncoder::new(options).unwrap();

    for _ in 0..2 {
        encoder.push(&input).unwrap();
        encoder.finish().unwrap();
        assert_eq!(decode_all(&encoder.take_output()).unwrap(), input);
        encoder.reset().unwrap();
    }

    // The third frame carries too little, and the reset must not have left the
    // second frame's count behind.
    encoder.push(&input[..10]).unwrap();
    assert!(matches!(encoder.finish(), Err(Error::InvalidParameter(_))));
}

/// A hash wider than the tagged tables can carry does not take the encoder
/// out of bounds.
///
/// The fast and double-fast finders pack a table index and an 8-bit tag into
/// one 32-bit hash, so a `hash_log` above 24 leaves nothing for the tag: the
/// shift underflowed, which panicked under a debug build and produced an
/// out-of-range table index under a release one. No compression level reaches
/// it — the widest `hash_log` on a fast row is 17 — but `hash_log: Some(30)`
/// does, as does any level whose row is wide enough once `strategy` is
/// overridden down to a fast parser.
#[test]
fn a_hash_log_wider_than_the_tag_stays_in_bounds() {
    let input = build_structured_log_pattern(40_000);

    for hash_log in [24u32, 25, 26, 30] {
        for strategy in [Strategy::Fast, Strategy::DoubleFast] {
            for chain_log in [24u32, 30] {
                let options = EncoderOptions {
                    parameters: ParameterOverrides {
                        hash_log: Some(hash_log),
                        chain_log: Some(chain_log),
                        strategy: Some(strategy),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let encoded = encode_all_with_options(&input, options).unwrap();
                assert_eq!(
                    decode_all(&encoded).unwrap(),
                    input,
                    "{strategy:?} hash_log {hash_log} chain_log {chain_log}"
                );
            }
        }
    }

    // The same shape through the streaming encoder, whose parameters come from
    // an unknown source size and so start from the widest tier's rows.
    for level in [1, 22] {
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            parameters: ParameterOverrides {
                strategy: Some(Strategy::Fast),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut encoder = StreamingEncoder::new(options).unwrap();
        encoder.push(&input).unwrap();
        encoder.finish().unwrap();
        assert_eq!(
            decode_all(&encoder.take_output()).unwrap(),
            input,
            "streaming at level {level}"
        );
    }
}

/// A reused `Encoder` rebuilds its match state when the parameters change.
///
/// The cached state is keyed on the finder's *built* dimensions, not on the
/// requested ones, and those differ wherever a finder clamps: the fast tables
/// cap their hash at 24. A key that compared the requested value would either
/// stop reusing anything (harmless but slow) or reuse a table built for a
/// different shape (not harmless at all).
#[test]
fn an_encoder_rebuilds_its_match_state_when_parameters_change() {
    let input = build_structured_log_pattern(50_000);
    let mut encoder = Encoder::new();

    let configurations = [
        ParameterOverrides::default(),
        ParameterOverrides {
            hash_log: Some(30),
            strategy: Some(Strategy::Fast),
            ..Default::default()
        },
        ParameterOverrides {
            hash_log: Some(12),
            strategy: Some(Strategy::Fast),
            ..Default::default()
        },
        ParameterOverrides {
            hash_log: Some(30),
            chain_log: Some(30),
            strategy: Some(Strategy::DoubleFast),
            ..Default::default()
        },
        ParameterOverrides {
            strategy: Some(Strategy::BinaryTreeUltra2),
            ..Default::default()
        },
        ParameterOverrides {
            hash_log: Some(30),
            strategy: Some(Strategy::Fast),
            ..Default::default()
        },
    ];

    // Twice through, so every configuration follows a different one and every
    // one is also reached a second time from a cache that already holds it.
    for _ in 0..2 {
        for parameters in configurations {
            let encoded = encoder
                .encode_all_with_options(
                    &input,
                    EncoderOptions {
                        parameters,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(decode_all(&encoded).unwrap(), input, "{parameters:?}");

            // The same options through a fresh encoder have to produce the
            // same bytes: a reused state that survived when it should not
            // would show up here and nowhere else.
            let fresh = encode_all_with_options(
                &input,
                EncoderOptions {
                    parameters,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(encoded, fresh, "{parameters:?} differed after reuse");
        }
    }
}

/// An empty-content dictionary plus a window narrower than the frame must not
/// emit an offset past the window the frame declares.
///
/// A dictionary whose content is empty used to take a third encode path: not
/// the contiguous one, and not the prefixed one, but the one that hands the
/// parser the dictionary and the frame history as a pair of prefix slices. That
/// path measured its history limit from the block's *start* while the other two
/// measure from its end, so a match late in a block could reach a whole block
/// further back than the frame said it would — and this crate's own decoder
/// rejects the result.
///
/// That path is gone: an empty dictionary is now the contiguous case, which is
/// what C does, and
/// `an_empty_dictionary_encodes_the_same_frame_as_no_dictionary` pins it there.
/// This stays as the narrow-window round trip, which is a bound the contiguous
/// path has to respect too, and which no other empty-dictionary test overrides
/// `window_log` to reach.
///
/// No compression level reaches it: adjustment leaves the window at least as
/// wide as the source plus the dictionary, so the limit is wider than
/// everything in front of the block anyway. `window_log: Some(10)` against a
/// body of several blocks does reach it, which is how the dictionary fuzz
/// target found it within four minutes of the override surface existing.
/// An empty dictionary is not a dictionary, and must not move a single byte.
///
/// C settles this before any parameter is chosen: `ZSTD_CCtx_loadDictionary_advanced`
/// clears the dictionary slot and returns for `dictSize == 0`
/// (`zstd_compress.c:1293`), and `ZSTD_compressBegin_internal` gates the CDict
/// path on `cdict->dictContentSize > 0` (`:5255`).
///
/// Both halves of this crate dispatched on whether a dictionary was *supplied*
/// rather than on whether it had content, and each half was wrong in its own
/// way. The block loop sent an empty dictionary down a third path that rebuilt
/// every block's history as a prefix slice; that path emitted matches which do
/// not exist in the source — a twelve-byte match whose bytes agree for four —
/// so the frame failed its own checksum, and upstream rejected it too.
/// Parameter selection sent it to `upstream_full_dict_cparams_for_level`, which
/// skips the source-size adjustment a real dictionary makes unnecessary: at
/// level 13 on a small source that is chain_log 22 against 12, and btlazy2
/// against btultra.
///
/// Round-tripping catches only the first, and only sometimes: the frame that
/// started this was found by a fuzz target, not by the round-trip test that
/// already covered empty dictionaries. Equality with the no-dictionary frame is
/// what pins both, and it is the property that actually holds.
#[test]
fn an_empty_dictionary_encodes_the_same_frame_as_no_dictionary() {
    let input = build_structured_log_pattern(8 * 1024);
    let dictionary = EncoderDictionary::new(&[]).unwrap();

    for level in [1i32, 5, 12, 13, 15, 16, 19, 22] {
        // The first two sizes give several blocks, so a block has history in
        // front of it. Both defects needed a block after the first; a
        // single-block frame is identical either way and proves nothing.
        for block_size in [512usize, 1031, 128 * 1024] {
            let options = EncoderOptions {
                block_size,
                compression_level: CompressionLevel::try_new(level).unwrap(),
                ..Default::default()
            };
            let with_dictionary =
                encode_all_with_prepared_dict_and_options(&input, &dictionary, options).unwrap();
            let without = encode_all_with_options(&input, options).unwrap();
            // Reported as sizes and the first differing byte: these frames run
            // to several kilobytes, and `assert_eq!` on the pair prints both in
            // full, which buries the one number that identifies the failure.
            let first_difference = with_dictionary
                .iter()
                .zip(&without)
                .position(|(a, b)| a != b);
            assert!(
                with_dictionary == without,
                "level {level}, block_size {block_size}: an empty dictionary changed the frame \
                 ({} bytes with, {} bytes without, first difference at {first_difference:?})",
                with_dictionary.len(),
                without.len(),
            );
            assert_eq!(
                decode_all(&with_dictionary).unwrap(),
                input,
                "level {level}, block_size {block_size}"
            );
        }
    }
}

#[test]
fn a_narrow_window_with_an_empty_dictionary_stays_inside_it() {
    // Several blocks at every block size below, so a later block has history in
    // front of it and the floor has something to get wrong.
    let input = build_structured_log_pattern(64 * 1024);
    let dictionary = EncoderDictionary::new(&[]).unwrap();
    let decoding = DecoderDictionary::new(&[]).unwrap();

    for window_log in [10u32, 12, 15] {
        for block_size in [1024usize, 16 * 1024] {
            for level in [1, 5, 14] {
                let options = EncoderOptions {
                    block_size,
                    compression_level: CompressionLevel::try_new(level).unwrap(),
                    parameters: ParameterOverrides {
                        window_log: Some(window_log),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let encoded =
                    encode_all_with_prepared_dict_and_options(&input, &dictionary, options)
                        .unwrap();
                assert_eq!(
                    decode_all_with_prepared_dict(&encoded, &decoding).unwrap(),
                    input,
                    "window_log {window_log}, block_size {block_size}, level {level}"
                );
            }
        }
    }
}

/// A block narrower than its window declares the window itself, so any offset
/// past it is a frame this crate's own decoder rejects — and a non-conforming
/// one: "all offsets leading to previously decoded data must be smaller than
/// `Window_Size`".
///
/// The parsers take their match floor at the position doing the looking. The
/// lazy family looks from more positions than the one its outer loop is at: the
/// depth-1 and depth-2 probes search from `pos + 1` and `pos + 2`, and the
/// immediate-repcode chain advances on its own. Resolving the floor once per
/// outer position and reusing it leaves those probes a floor that is one or two
/// bytes too low, and they emit an offset that far past the window. It is worth
/// one byte, it needs a match that sits exactly on the oldest reachable
/// position, and no compression level reaches it without a `window_log`
/// override — which is why nothing but this caught it.
#[test]
fn every_parser_stays_inside_the_window_it_declares() {
    const STRATEGIES: [Strategy; 6] = [
        Strategy::Greedy,
        Strategy::Lazy,
        Strategy::Lazy2,
        Strategy::BinaryTreeLazy2,
        Strategy::BinaryTreeOpt,
        Strategy::BinaryTreeUltra,
    ];

    let mut checked = 0usize;
    for window_log in [11u32, 13, 14] {
        let window = 1usize << window_log;
        for &strategy in &STRATEGIES {
            for level in [6i32, 10, 18] {
                for (period, alphabet, noise) in
                    [(window + 245, 2usize, 8u64), (window * 2 + 113, 5, 128)]
                {
                    // A random block repeated, so the only repetition is at
                    // `period` and the parser is pushed to the window's edge.
                    // A generator whose value also depends on `i % alphabet`
                    // would be matched at the alphabet's distance instead and
                    // never reach the window at all.
                    let mut x: u64 = 0x5DEE_CE66_D125_1234 ^ (period as u64);
                    let mut next = move || {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        x
                    };
                    let block: Vec<u8> = (0..period)
                        .map(|_| (next() >> 33) as u8 % alphabet as u8)
                        .collect();
                    let input: Vec<u8> = (0..window * 6)
                        .map(|i| {
                            let b = block[i % period];
                            // Imperfect periodicity, so the parser cannot ride
                            // one repeat offset for the whole frame.
                            if next() % noise == 0 { b ^ 1 } else { b }
                        })
                        .collect();

                    let options = EncoderOptions {
                        compression_level: CompressionLevel::try_new(level).unwrap(),
                        // At or below the window, so `Window_Size` is the window
                        // and the decoder is the check.
                        block_size: window / 2,
                        parameters: ParameterOverrides {
                            window_log: Some(window_log),
                            strategy: Some(strategy),
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    let encoded = encode_all_with_options(&input, options).unwrap();
                    let FrameHeader::Zstandard(header) = parse_frame_header(&encoded).unwrap()
                    else {
                        panic!("expected a Zstandard frame");
                    };
                    assert_eq!(header.window_size, window as u64);
                    assert_eq!(
                        decode_all(&encoded).unwrap(),
                        input,
                        "window_log={window_log} {strategy:?} level={level} period={period}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(
        checked,
        3 * 6 * 3 * 2,
        "the sweep did not cover its own matrix"
    );
}

/// A block never exceeds the window, and the frame declares the window alone.
///
/// This is upstream's arrangement: `blockSize = MIN(maxBlockSize, windowSize)`
/// (`zstd_compress.c:2132`) and a header carrying `1 << windowLog`
/// (`:4703`). It replaces the one this crate had, which kept the caller's block
/// size and declared a window wide enough for whatever such a block could
/// reach — a window nobody else would have chosen and which upstream's own
/// frames never carry.
///
/// The caller's `block_size` is left at its 128 KiB default here on purpose:
/// the point is that the encoder holds the block down, not that a caller can.
#[test]
fn a_block_never_exceeds_the_window_the_frame_declares() {
    let input = build_pattern(128 * 1024);
    let mut checked = 0usize;
    for window_log in [10u32, 12, 15, 17] {
        let window = 1usize << window_log;
        // Enough blocks to cover the input at the capped size. Asserting this
        // rather than only the declared window is what keeps the test honest:
        // a header claiming the right window while the blocks stayed at the
        // caller's 128 KiB would satisfy every other assertion here.
        let least_blocks = input.len().div_ceil(window);
        for level in [1i32, 5, 13, 19] {
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                parameters: ParameterOverrides {
                    window_log: Some(window_log),
                    ..Default::default()
                },
                ..Default::default()
            };

            // The streaming encoder reaches the same arrangement from a
            // different direction: it has no source length, so its window is
            // the level's outright and its blocks are capped against that.
            let mut encoder = StreamingEncoder::new(options).unwrap();
            let mut streamed = encoder.take_output();
            for chunk in input.chunks(7_919) {
                encoder.push(chunk).unwrap();
                streamed.extend_from_slice(&encoder.take_output());
            }
            encoder.finish().unwrap();
            streamed.extend_from_slice(&encoder.take_output());

            for (label, encoded) in [
                (
                    "one-shot",
                    encode_all_with_options(&input, options).unwrap(),
                ),
                ("streaming", streamed),
            ] {
                let where_ = format!("{label} window_log={window_log} level={level}");
                let FrameHeader::Zstandard(header) = parse_frame_header(&encoded).unwrap() else {
                    panic!("expected a Zstandard frame");
                };
                assert_eq!(header.window_size, window as u64, "{where_}");
                assert_eq!(
                    header.block_size_max,
                    window.min(128 * 1024) as u32,
                    "{where_}: Block_Maximum_Size is min(Window_Size, 128 KiB)"
                );

                let blocks = parse_frame_blocks(&encoded);
                assert!(
                    blocks.len() >= least_blocks,
                    "{where_}: {} blocks for {} bytes, too few to be capped at {window}",
                    blocks.len(),
                    input.len(),
                );
                for block in &blocks {
                    assert!(
                        block.payload_end - block.payload_start <= window,
                        "{where_}: a block payload outgrew the window"
                    );
                }
                // The decoder enforces the decompressed bound the header
                // declares, so a round trip closes what the payload sizes
                // above can only imply.
                assert_eq!(decode_all(&encoded).unwrap(), input, "{where_}");
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 4 * 4, "the sweep did not cover its own matrix");
}

/// The same window bound, but with a dictionary in front of the frame.
///
/// A dictionary gives the match finders a second region to address, and one of
/// them indexes both regions in a single space: `BinaryTreeFinder` stores
/// prefix position `p` at `p + 2` and source position `p` at `prefix_len + p`.
/// It was handed the source half of the floor and compared it against those
/// combined indices, so the bound was short by the whole dictionary length and
/// btlazy2 emitted offsets past the window that the decoder then rejected.
///
/// Every other parser addresses the prefix and the source separately and was
/// never affected, which is why this sweep found 119 failures and all 119 were
/// `BinaryTreeLazy2`. The sweep keeps the other strategies anyway: they cost
/// little and they are what makes "only btlazy2" a measurement rather than an
/// assumption.
///
/// `block_size` is the window, so `Window_Size` is the window and the decoder
/// is the check.
#[test]
fn every_parser_stays_inside_its_window_with_a_dictionary() {
    const STRATEGIES: [Strategy; 7] = [
        Strategy::Fast,
        Strategy::DoubleFast,
        Strategy::Greedy,
        Strategy::Lazy2,
        Strategy::BinaryTreeLazy2,
        Strategy::BinaryTreeOpt,
        Strategy::BinaryTreeUltra,
    ];

    let mut checked = 0usize;
    for window_log in [10u32, 12] {
        let window = 1usize << window_log;
        for &strategy in &STRATEGIES {
            // Dictionaries either side of the window, so the floor is exercised
            // both while the dictionary is still inside it and after it has
            // aged out. Those are the two branches of the floor, and they
            // resolve to bounds in different regions of the tree.
            for dictionary_len in [64usize, window / 2, window * 2] {
                for level in [1i32, 13, 19] {
                    let dictionary = build_structured_log_pattern(dictionary_len);
                    let input = build_pattern(window * 4);
                    let prepared = EncoderDictionary::new(&dictionary).unwrap();
                    let prepared_decoding = DecoderDictionary::new(&dictionary).unwrap();
                    let options = EncoderOptions {
                        block_size: window,
                        compression_level: CompressionLevel::try_new(level).unwrap(),
                        parameters: ParameterOverrides {
                            window_log: Some(window_log),
                            strategy: Some(strategy),
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    let encoded =
                        encode_all_with_prepared_dict_and_options(&input, &prepared, options)
                            .unwrap();
                    assert_eq!(
                        decode_all_with_prepared_dict(&encoded, &prepared_decoding).unwrap(),
                        input,
                        "window_log={window_log} {strategy:?} level={level} \
                         dictionary_len={dictionary_len}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(
        checked,
        2 * 7 * 3 * 3,
        "the sweep did not cover its own matrix"
    );
}

/// A body whose repeats sit further apart than any block, so a match that
/// crosses them is one only the long-distance matcher can have found.
fn long_range_repeat_body() -> Vec<u8> {
    let planted: Vec<u8> = (0..(96u32 << 10))
        .map(|index| (index.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let filler: Vec<u8> = (0..(768u32 << 10))
        .map(|index| (index.wrapping_mul(40_503) >> 13) as u8)
        .collect();
    let mut body = planted.clone();
    body.extend_from_slice(&filler);
    body.extend_from_slice(&planted);
    body
}

/// Every parser family round-trips with long-distance matching on, and finds
/// the planted repeat that sits past the parser's own reach.
///
/// The ratio bound is what makes this more than a round-trip: the same body
/// without long-distance matching is measurably larger, so a run that silently
/// stopped emitting long-distance matches would still decode and would still
/// fail here. It covers both halves of `ZSTD_ldm_blockCompress` -- the
/// strategies that take the matcher's output, and the optimal ones that price
/// it as a candidate and could decline it.
#[test]
fn long_distance_matching_reaches_past_the_parsers_window() {
    let body = long_range_repeat_body();
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
    let mut improved = 0usize;
    for strategy in strategies {
        // Narrow enough that the parser cannot reach the first copy from the
        // second, which is what leaves the long-distance matcher something to
        // find that nothing else can.
        let parameters = ParameterOverrides {
            strategy: Some(strategy),
            window_log: Some(18),
            ..Default::default()
        };
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(5).unwrap(),
            parameters,
            ..Default::default()
        };
        let without = encode_all_with_options(&body, options).unwrap();

        let with_ldm = EncoderOptions {
            parameters: ParameterOverrides {
                long_distance_matching: LdmMode::Enabled,
                ..parameters
            },
            ..options
        };
        let with = encode_all_with_options(&body, with_ldm).unwrap();
        assert_eq!(
            decode_all(&with).unwrap(),
            body,
            "{strategy:?} did not round-trip with long-distance matching"
        );
        if with.len() < without.len() {
            improved += 1;
        }
    }
    assert_eq!(
        improved,
        strategies.len(),
        "long-distance matching bought nothing on a body built for it"
    );
}

/// Long-distance matching is accepted by every encoder, including in front of a
/// dictionary.
///
/// This used to assert the opposite. The combination was refused rather than
/// silently encoded without the matcher, because a frame that quietly omits
/// what the caller asked for is indistinguishable from one that used it. The
/// refusal is gone now that both halves exist -- the matcher searches a
/// dictionary, and the prefixed block compressor consumes what it finds -- so
/// what is left to check is that the door is open and that what comes through
/// it round-trips.
#[test]
fn long_distance_matching_is_accepted_with_and_without_a_dictionary() {
    let body = vec![b'x'; 4 << 10];
    let options = EncoderOptions {
        parameters: ParameterOverrides {
            long_distance_matching: LdmMode::Enabled,
            ..Default::default()
        },
        ..Default::default()
    };

    let dictionary = vec![b'z'; 1 << 10];
    let encoded = encode_all_with_dict_and_options(&body, &dictionary, options)
        .expect("a dictionary encode must accept long-distance matching");
    assert_eq!(
        decode_all_with_dict(&encoded, &dictionary).unwrap(),
        body,
        "a dictionary frame with long-distance matching did not round-trip"
    );

    let mut stream = StreamingEncoder::with_dict(&dictionary, options)
        .expect("a dictionary stream must accept long-distance matching");
    stream.push(&body).unwrap();
    stream.finish().unwrap();
    let streamed = stream.take_output();
    assert_eq!(
        decode_all_with_dict(&streamed, &dictionary).unwrap(),
        body,
        "a dictionary stream with long-distance matching did not round-trip"
    );

    assert!(
        StreamingEncoder::new(options).is_ok(),
        "the streaming encoder refused long-distance matching without a dictionary"
    );
}

/// Enabling long-distance matching *sets* the declared window to `1 << 27`,
/// and an explicit `window_log` still beats it.
///
/// Both halves matter: the force is an assignment rather than a maximum, and it
/// sits before the override in `ZSTD_getCParamsFromCCtxParams`. The window is
/// then fitted to the source like any other, so the body has to be large enough
/// for the forced value to survive.
#[test]
fn enabling_long_distance_matching_widens_the_window_unless_overridden() {
    let body = long_range_repeat_body();
    // Level 1 rather than 5, because the force is only observable where the
    // level's own window is narrower than the source: fitting the window to the
    // source only ever shrinks it, so on a body smaller than the level's window
    // both values land on the source's own log and the force proves nothing.
    let window_of = |parameters| {
        let frame = encode_all_with_options(
            &body,
            EncoderOptions {
                compression_level: CompressionLevel::try_new(1).unwrap(),
                parameters,
                write_content_size: false,
                ..Default::default()
            },
        )
        .unwrap();
        match parse_frame_header(&frame).unwrap() {
            FrameHeader::Zstandard(header) => header.window_size,
            other => panic!("unexpected frame header {other:?}"),
        }
    };

    let plain = window_of(ParameterOverrides::default());
    let forced = window_of(ParameterOverrides {
        long_distance_matching: LdmMode::Enabled,
        ..Default::default()
    });
    let overridden = window_of(ParameterOverrides {
        long_distance_matching: LdmMode::Enabled,
        window_log: Some(18),
        ..Default::default()
    });

    assert_eq!(plain, 1 << 19, "level 1's own window moved");
    assert_eq!(
        forced,
        1 << 20,
        "enabling long-distance matching did not widen the window"
    );
    assert_eq!(
        overridden,
        1 << 18,
        "an explicit window_log did not beat the long-distance force"
    );
}

/// A body whose repeats sit inside the window but straddle the streaming
/// encoder's buffer compactions: the same block recurs every `period` bytes,
/// separated by filler no two periods share.
///
/// `period` below the window is what makes the repeats legitimately findable;
/// the total length is what makes the encoder compact several times before it
/// runs out.
fn compaction_spanning_repeat_body(period: usize, periods: usize) -> Vec<u8> {
    // Incompressible, so the only cheap way to encode a later copy is to match
    // it against an earlier one.
    let mut state = 0x1234_5678u32;
    let planted: Vec<u8> = std::iter::repeat_with(|| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 24) as u8
    })
    .take(32 << 10)
    .collect();

    let mut body = Vec::with_capacity(period * periods);
    for run in 0..periods as u32 {
        body.extend_from_slice(&planted);
        // Cheap to encode and unique to this period, so it neither repeats
        // anything the matcher has already seen nor costs enough to drown out
        // what the planted repeats save.
        let filler = (period - planted.len()) as u32;
        body.extend((0..filler).map(|index| {
            let phase = index % 509;
            (phase.wrapping_mul(31).wrapping_add(run.wrapping_mul(7)) ^ (index >> 11)) as u8
        }));
    }
    body
}

/// Compacting the history buffer must not cost the parser its match state, at
/// any strategy.
///
/// The one-shot encoder is the control: identical parameters, identical window,
/// and no compaction, so any difference between the two frames is what the
/// compaction cost. Nothing else in the suite makes that comparison above the
/// hash-table strategies. `streaming_stays_at_upstream_size_across_repeated_compactions`
/// covers levels 1 and 2 only, which are exactly the two finders whose positions
/// could always be rebased; the chain and binary-tree finders reached this code
/// by a different path and were never measured on it.
///
/// The binary tree is the one that cannot be rebuilt from the bytes it
/// describes, and it fails in the direction a round-trip test cannot see: it
/// still decodes, just much larger.
///
/// All three cases are load-bearing, because a cycle-indexed table survives a
/// compaction for a different reason in each and any one of them alone would
/// have called the other two fixed:
///
/// - Level 5 puts the cycle inside the window, where the drop is rounded to it.
///   Leaving it unrounded costs 176.3% here at `BinaryTreeLazy2`.
/// - Level 12 puts it past the whole buffer, where nothing wraps and the table
///   moves bodily instead. Falling back to the rebuild costs 321.7%.
/// - Level 5 with the chain log pinned to 21 puts it between the two, where
///   neither holds until the buffer is widened to a cycle.
///
/// The bound is set by what compaction still costs the finders this is *not*
/// about: level 12 at `DoubleFast` gives up 1.60% and at `Fast` 0.39%, both of
/// them rebasing every entry they hold and neither of them touched by this. On a
/// body short enough not to compact, all four hash-table rows are within 0.12%,
/// so that residue is compaction's; it is recorded in `docs/PARITY_PLAN.md`.
/// What this test is here to catch arrives in the tens of percent.
///
/// It does not cover everything the optimal parsers carry across a compaction.
/// They keep a second table for three-byte matches, held only when `min_match`
/// is 3, which neither level here asks for. Adding `min_match` as a dimension
/// looks like the fix and is not one: this body is planted random blocks in
/// low-alphabet filler, three-byte matches are worth almost nothing in it, and
/// with that table's rebase removed the forced-3 rows move by at most 0.14%. A
/// dimension that inert would read as coverage while testing nothing. See
/// `streaming_compaction_keeps_the_three_byte_table_on_text` for the same
/// question asked on a body that can answer it.
#[test]
fn streaming_compaction_keeps_the_match_state_at_every_strategy() {
    const WINDOW_LOG: u32 = 19;
    let body = compaction_spanning_repeat_body((7 << WINDOW_LOG) / 8, 8);

    // The window is half a megabyte and a block is an eighth of that, so a
    // chain log of 21 is a cycle of one megabyte: above the window, below the
    // buffer.
    for (level, chain_log) in [(5i32, None), (12, None), (5, Some(21u32))] {
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
            let case = format!("level {level} chain_log {chain_log:?} {strategy:?}");
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(level).unwrap(),
                parameters: ParameterOverrides {
                    strategy: Some(strategy),
                    window_log: Some(WINDOW_LOG),
                    chain_log,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut encoder = StreamingEncoder::new(options).unwrap();
            let mut streamed = encoder.take_output();
            for chunk in body.chunks(64 << 10) {
                encoder.push(chunk).unwrap();
                streamed.extend_from_slice(&encoder.take_output());
            }
            encoder.finish().unwrap();
            streamed.extend_from_slice(&encoder.take_output());

            assert_eq!(
                decode_all(&streamed).unwrap(),
                body,
                "{case} did not round-trip"
            );

            let one_shot = encode_all_with_options(&body, options).unwrap();
            let over = (streamed.len() as f64 - one_shot.len() as f64) / one_shot.len() as f64;
            assert!(
                over <= 0.02,
                "{case}: streaming across compactions emitted {} bytes \
                 against one-shot's {} ({:+.2}%)",
                streamed.len(),
                one_shot.len(),
                over * 100.0,
            );
        }
    }
}

/// The optimal parsers' three-byte table must be re-keyed when the streaming
/// encoder compacts its history buffer, as C re-keys it in `ZSTD_reduceIndex`
/// alongside the hash and chain tables.
///
/// It is the one search structure that does not live on the match state -- it
/// sits on the sequence plan, which is reused across blocks -- and so it was the
/// one a compaction never reached. It is also direct-mapped and refilled only
/// forward, from the tree's frontier to the position being searched, so a
/// bucket the parse does not revisit is never corrected: the loss is permanent
/// for the frame and compounds at every further compaction. That is why this
/// grows with the body rather than settling, which is the shape a one-time
/// rebuild cost would have.
///
/// The one-shot encoder is the control, and a strict one: it runs the same
/// parser over the same window and never compacts, so the two frames should be
/// the same size. Before the rebase they were not, by 19.5% at `BinaryTreeOpt`
/// and 15.5% at both ultra strategies on 1 MiB, rising to 19.6% at 2 MiB.
///
/// Three things about the shape here are load-bearing, and each was arrived at
/// by a measurement rather than by taste:
///
/// - **Text.** The defect is invisible on the synthetic body used by
///   `streaming_compaction_keeps_the_match_state_at_every_strategy`, where it is
///   worth 0.14%, because three-byte matches barely pay there.
/// - **`min_match` of 3.** At any other value the table is never allocated and
///   the whole question is unreachable. `Lazy2` is the control for that: it is
///   the strategy that takes a `min_match` of 3 and still has no such table, and
///   it was already clean.
/// - **A body several windows long.** The window is 128 KiB, the buffer holds
///   two of those, and it first drops history when the frame would exceed three
///   -- so nothing at all diverges below 384 KiB, and the first divergence
///   appears in the very next block.
#[test]
fn streaming_compaction_keeps_the_three_byte_table_on_text() {
    const WINDOW_LOG: u32 = 17;
    const PIECE: usize = 32 << 10;
    // Eight windows, so the frame is compacted six times after the first drop.
    let body = benchmark_corpora::benchmark_report_cases(8 << WINDOW_LOG)
        .into_iter()
        .find(|case| case.name == "wikipedia")
        .expect("wikipedia is a benchmark corpus")
        .input;

    // Level 16's own tables, and the much smaller ones an attached trained
    // dictionary resolves to. The second is not a hypothetical: it is what
    // `should_attach_full_dictionary` produces for a streamed frame at this
    // level, so a caller reaches it by passing a dictionary and nothing else.
    // It is also where the loss is largest, because the smaller the tree the
    // more of the parse the three-byte table is carrying.
    for dictionary_shaped in [false, true] {
        for strategy in [
            // The control: it reads `min_match` and has no three-byte table.
            Strategy::Lazy2,
            Strategy::BinaryTreeOpt,
            Strategy::BinaryTreeUltra,
            Strategy::BinaryTreeUltra2,
        ] {
            let case = format!("{strategy:?} dictionary_shaped={dictionary_shaped}");
            let options = EncoderOptions {
                compression_level: CompressionLevel::try_new(16).unwrap(),
                parameters: ParameterOverrides {
                    strategy: Some(strategy),
                    window_log: Some(WINDOW_LOG),
                    min_match: Some(3),
                    hash_log: dictionary_shaped.then_some(12),
                    chain_log: dictionary_shaped.then_some(12),
                    search_log: dictionary_shaped.then_some(5),
                    target_length: dictionary_shaped.then_some(48),
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut encoder = StreamingEncoder::new(options).unwrap();
            let mut streamed = encoder.take_output();
            for chunk in body.chunks(PIECE) {
                encoder.push(chunk).unwrap();
                streamed.extend_from_slice(&encoder.take_output());
            }
            encoder.finish().unwrap();
            streamed.extend_from_slice(&encoder.take_output());

            assert_eq!(
                decode_all(&streamed).unwrap(),
                body,
                "{case} did not round-trip"
            );

            let one_shot = encode_all_with_options(&body, options).unwrap();
            let over = (streamed.len() as f64 - one_shot.len() as f64) / one_shot.len() as f64;
            assert!(
                over <= 0.005,
                "{case}: streaming across compactions emitted {} bytes against \
                 one-shot's {} ({:+.2}%)",
                streamed.len(),
                one_shot.len(),
                over * 100.0,
            );
        }
    }
}

/// Compaction must hold across the *geometry* it is computed from: the window
/// against the finder's cycle.
///
/// The two tests above each fix a window and vary the parser. This varies the
/// pair of logs that `aligned_drop`, `rebase_period` and
/// `ContiguousBlockMatchState::shift_positions` are all derived from, because
/// that is where the arithmetic lives. A cycle narrower than the drop is
/// handled by aligning the drop to it; a cycle wider than the whole buffer
/// shifts the table bodily; between them the buffer is widened until the first
/// route applies. Three deltas around the window put cases in each band.
///
/// The body is scaled to the window rather than fixed, at six windows, so every
/// row compacts (the first drop comes at three) and the narrow-window rows stay
/// cheap. The whole grid is 192 cases in under two seconds.
///
/// Two things about the shape are load-bearing, and both are corrections to an
/// earlier version of this sweep that reported divergences it had created
/// itself:
///
/// - **The odd tail on the body length, and the pledge.** A streamed frame
///   whose content is an exact multiple of the block size ends with an empty
///   raw block carrying the last-block flag, because the last data block went
///   out before the encoder knew it was last. That is three bytes, it is what
///   C does too (`ZSTD_writeEpilogue` writes the same block when the final
///   chunk produced no output), and on the narrowest row here it is 0.2% of a
///   1.5 KiB frame. The pledge matters for a larger reason: without it the
///   stream cannot shrink its tables to the content and the one-shot control
///   does, so the two run *different parameters* and the comparison measures
///   nothing. That was worth 5.80% on one corpus before it was noticed.
/// - **One corpus, and not `mixed-entropy`.** Streaming reproduces upstream's
///   streaming block layout, which splits at most twice per chunk, while
///   one-shot reproduces upstream's one-shot layout, which splits many times
///   over -- see `encode_buffered_chunk`, where reproducing the one-shot layout
///   from a stream is recorded as costing 1.84% to 2.31% against upstream's
///   *streaming* output. On a corpus that engages the splitter the one-shot
///   encoder is therefore not the oracle, and `mixed-entropy` diverges by up to
///   0.27% for that reason alone.
///
/// What this grid actually covers was measured by injection rather than
/// asserted, and it is less than the row count suggests:
///
/// - Dropping the three-byte table's rebase moves 10 rows, all at `min_match`
///   3 under the optimal parser, by up to 8.44%.
/// - Disabling the contiguous rebase, so every compaction falls through to a
///   rebuild, moves 96 rows: every binary-tree row and nothing else.
/// - Disabling compaction's re-keying altogether moves 144: every tree row and
///   every row-finder row.
/// - `DoubleFast` moves under none of the three, which is why it is here. The
///   fast pair file every position as they parse and consult no cursor, so a
///   stale table is overwritten by the ongoing parse and a wrong candidate
///   fails the byte comparison. Re-keying is load-bearing for the row and tree
///   finders; for these it is not, and a grid that flagged them would be
///   reporting something other than compaction.
///
/// The bound has full margin: every row of this grid is at or below one-shot
/// today, so nothing sits near 0.5%.
#[test]
fn streaming_compaction_holds_across_window_and_cycle_geometry() {
    const PIECE: usize = 32 << 10;
    // Wide enough to slice six windows out of for the widest row below.
    let corpus = benchmark_corpora::benchmark_report_cases(8 << 17)
        .into_iter()
        .find(|case| case.name == "wikipedia")
        .expect("wikipedia is a benchmark corpus")
        .input;

    for strategy in [
        // The measured control: compaction provably does not reach it.
        Strategy::DoubleFast,
        Strategy::Lazy2,
        Strategy::BinaryTreeLazy2,
        Strategy::BinaryTreeOpt,
    ] {
        // 10 is `ZSTD_WINDOWLOG_ABSOLUTEMIN`.
        for window_log in 10u32..=17 {
            let body = &corpus[..((6usize << window_log) + 777).min(corpus.len())];
            for chain_delta in [-4i32, 0, 4] {
                let chain_log = u32::try_from(window_log as i32 + chain_delta)
                    .ok()
                    .filter(|log| ParameterOverrides::CHAIN_LOG.contains(*log));
                // 3 is the only value that allocates the three-byte table.
                for min_match in [3u32, 5] {
                    let case = format!(
                        "{strategy:?} window_log {window_log} chain_log {chain_log:?} \
                         min_match {min_match}"
                    );
                    let options = EncoderOptions {
                        compression_level: CompressionLevel::try_new(12).unwrap(),
                        pledged_src_size: Some(body.len() as u64),
                        parameters: ParameterOverrides {
                            strategy: Some(strategy),
                            window_log: Some(window_log),
                            chain_log,
                            hash_log: Some(window_log),
                            min_match: Some(min_match),
                            long_distance_matching: LdmMode::Disabled,
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    let mut encoder = StreamingEncoder::new(options).unwrap();
                    let mut streamed = encoder.take_output();
                    for chunk in body.chunks(PIECE) {
                        encoder.push(chunk).unwrap();
                        streamed.extend_from_slice(&encoder.take_output());
                    }
                    encoder.finish().unwrap();
                    streamed.extend_from_slice(&encoder.take_output());

                    assert_eq!(
                        decode_all(&streamed).unwrap(),
                        body,
                        "{case} did not round-trip"
                    );

                    let one_shot = encode_all_with_options(body, options).unwrap();
                    let over =
                        (streamed.len() as f64 - one_shot.len() as f64) / one_shot.len() as f64;
                    assert!(
                        over <= 0.005,
                        "{case}: streaming across compactions emitted {} bytes against \
                         one-shot's {} ({:+.2}%)",
                        streamed.len(),
                        one_shot.len(),
                        over * 100.0,
                    );
                }
            }
        }
    }
}

/// Long-distance matching must survive the streaming encoder compacting its
/// history buffer, which is the one thing the streaming path has to do that the
/// one-shot path does not.
///
/// Every position in the table is an index into that buffer, so dropping bytes
/// off the front invalidates all of them at once. Unlike the match finders,
/// which can be rebuilt over the retained bytes at a cost, this table has no
/// rebuild: it is filled by hashing forward over each block as it arrives, so a
/// cleared table has forgotten the whole frame and will only ever learn the
/// blocks still to come.
///
/// The one-shot encoder is the control. It runs the same matcher over the same
/// window and never compacts, so the two frames should come out the same size,
/// and they land a byte apart. Removing the rebase does not merely cost ratio
/// here: the first stale entry the matcher reaches is a position past the end of
/// the compacted buffer, and this panics on the bounds check rather than
/// reaching the size assertion at all.
///
/// `BinaryTreeOpt` took no such control until the parser's own match state
/// survived a compaction, which it did not when this was written: on this body
/// that alone was worth 3.76x with the matcher switched off on both sides. See
/// `streaming_compaction_keeps_the_match_state_at_every_strategy`.
#[test]
fn long_distance_matching_survives_streaming_buffer_compaction() {
    // 512 KiB of history, so the encoder compacts once per 512 KiB of input
    // and this body sees six of them. The repeats sit at seven eighths of
    // that: inside the window, and across the compactions.
    const WINDOW_LOG: u32 = 19;
    let body = compaction_spanning_repeat_body((7 << WINDOW_LOG) / 8, 8);

    let stream = |options| {
        let mut encoder = StreamingEncoder::new(options).unwrap();
        let mut out = encoder.take_output();
        for chunk in body.chunks(64 << 10) {
            encoder.push(chunk).unwrap();
            out.extend_from_slice(&encoder.take_output());
        }
        encoder.finish().unwrap();
        out.extend_from_slice(&encoder.take_output());
        out
    };

    for (level, strategy) in [(1, Strategy::Fast), (12, Strategy::BinaryTreeOpt)] {
        let parameters = ParameterOverrides {
            strategy: Some(strategy),
            window_log: Some(WINDOW_LOG),
            ..Default::default()
        };
        let with_ldm = ParameterOverrides {
            long_distance_matching: LdmMode::Enabled,
            ..parameters
        };
        let options = |parameters| EncoderOptions {
            compression_level: CompressionLevel::try_new(level).unwrap(),
            parameters,
            ..Default::default()
        };

        let streamed = stream(options(with_ldm));
        assert_eq!(
            decode_all(&streamed).unwrap(),
            body,
            "{strategy:?} did not round-trip"
        );

        // That the matcher ran at all, measured against the same stream
        // without it. What "ran at all" can be asserted as depends on whether
        // the parser could have found the repeats by itself.
        //
        // `Fast` cannot: its table holds one entry per hash and the repeats sit
        // seven eighths of a window apart, so the matcher is the only thing
        // that reaches them and the difference is enormous.
        //
        // `BinaryTreeOpt` can, and that is a change. This assertion used to
        // read `streamed.len() < without.len()` for both, on the stated premise
        // that "nothing else here reaches back far enough". For the tree that
        // premise was a defect rather than a fact: its insert bounded the
        // traversal at the start of the buffer instead of at
        // `ZSTD_getLowestMatchIndex(ms, target, windowLog)`, which left the
        // tree threaded through positions already outside the window and cut
        // searches short exactly at the distances this body plants. With that
        // fixed the plain parser finds them too -- 55638 bytes became 54577 --
        // and the matcher's own 54821 became 54735, so the matcher now *costs*
        // 158 bytes here rather than saving 817.
        //
        // So the tree gets the weaker claim it can still support: the matcher
        // costs no more than a small margin. Both numbers moved in the right
        // direction; only their order changed.
        let without = stream(options(parameters));
        match strategy {
            Strategy::BinaryTreeOpt => assert!(
                streamed.len() <= without.len() + without.len() / 100,
                "{strategy:?}: long-distance matching cost more than 1% streaming, \
                 {} bytes against {}",
                streamed.len(),
                without.len()
            ),
            _ => assert!(
                streamed.len() < without.len(),
                "{strategy:?}: long-distance matching bought nothing streaming, \
                 {} bytes against {}",
                streamed.len(),
                without.len()
            ),
        }

        // And that it kept finding them across every compaction.
        let one_shot = encode_all_with_options(&body, options(with_ldm)).unwrap();
        let over = (streamed.len() as f64 - one_shot.len() as f64) / one_shot.len() as f64;
        assert!(
            over <= 0.005,
            "{strategy:?}: streaming emitted {} bytes against one-shot's {} ({:+.2}%)",
            streamed.len(),
            one_shot.len(),
            over * 100.0,
        );
    }
}

/// A reset starts the long-distance table over.
///
/// Every entry in it names a position in the frame's own buffer, and the next
/// frame's decoder has never seen those bytes. An entry that survived a reset
/// would either be read past the end of a buffer that has just been emptied or,
/// worse, be turned into an offset reaching outside the frame entirely -- a
/// frame that encodes without complaint and cannot be decoded.
///
/// Both frames carry the same planted content, so a table that had not been
/// cleared would certainly match against the first frame while encoding the
/// second.
#[test]
fn long_distance_matching_starts_over_after_a_streaming_reset() {
    let body = compaction_spanning_repeat_body((7 << 19) / 8, 4);
    let options = EncoderOptions {
        compression_level: CompressionLevel::try_new(1).unwrap(),
        parameters: ParameterOverrides {
            long_distance_matching: LdmMode::Enabled,
            window_log: Some(19),
            ..Default::default()
        },
        ..Default::default()
    };

    let mut encoder = StreamingEncoder::new(options).unwrap();
    let mut encoded = encoder.take_output();
    for _ in 0..2 {
        encoder.push(&body).unwrap();
        encoder.finish().unwrap();
        encoded.extend_from_slice(&encoder.take_output());
        encoder.reset().unwrap();
    }

    let mut expected = body.clone();
    expected.extend_from_slice(&body);
    assert_eq!(decode_all(&encoded).unwrap(), expected);
}

/// Drain a streaming encoder through `read` into a fixed buffer, the
/// `ZSTD_outBuffer` shape.
fn drain_encoder(encoder: &mut StreamingEncoder<'_>, scratch: &mut [u8], out: &mut Vec<u8>) {
    loop {
        let produced = encoder.read(scratch);
        if produced == 0 {
            return;
        }
        out.extend_from_slice(&scratch[..produced]);
    }
}

#[test]
fn streaming_encoder_read_produces_the_same_frame_as_take_output() {
    let data = build_pattern(600_000);
    let options = EncoderOptions {
        checksum: true,
        ..Default::default()
    };

    let mut encoder = StreamingEncoder::new(options).unwrap();
    let mut taken = Vec::new();
    for chunk in data.chunks(7_777) {
        encoder.push(chunk).unwrap();
        taken.extend_from_slice(&encoder.take_output());
    }
    encoder.finish().unwrap();
    taken.extend_from_slice(&encoder.take_output());

    // The drain shape must not change the bytes. Every output size here is a
    // different number of `read` calls per block, including one that never
    // empties the queue in a single call.
    for window in [1usize, 13, 4_096, StreamingEncoder::RECOMMENDED_OUTPUT_SIZE] {
        let mut scratch = vec![0u8; window];
        let mut encoder = StreamingEncoder::new(options).unwrap();
        let mut read_out = Vec::new();
        for chunk in data.chunks(7_777) {
            encoder.push(chunk).unwrap();
            drain_encoder(&mut encoder, &mut scratch, &mut read_out);
        }
        encoder.finish().unwrap();
        drain_encoder(&mut encoder, &mut scratch, &mut read_out);

        assert_eq!(read_out, taken, "read window {window}");
        assert_eq!(decode_all(&read_out).unwrap(), data, "read window {window}");
    }
}

#[test]
fn streaming_encoder_pending_output_and_consume_agree_with_read() {
    let data = build_pattern(300_000);
    let options = EncoderOptions::default();

    let mut encoder = StreamingEncoder::new(options).unwrap();
    let mut borrowed = Vec::new();
    for chunk in data.chunks(9_001) {
        encoder.push(chunk).unwrap();
        // Consume in two bites so the partial-consume path and its compaction
        // are exercised rather than only the drain-everything one.
        let half = encoder.pending_output_len() / 2;
        borrowed.extend_from_slice(&encoder.pending_output()[..half]);
        encoder.consume_output(half);
        let rest = encoder.pending_output_len();
        borrowed.extend_from_slice(encoder.pending_output());
        encoder.consume_output(rest);
        assert_eq!(encoder.pending_output_len(), 0);
        assert!(encoder.pending_output().is_empty());
    }
    encoder.finish().unwrap();
    borrowed.extend_from_slice(encoder.pending_output());
    let tail = encoder.pending_output_len();
    encoder.consume_output(tail);

    assert_eq!(decode_all(&borrowed).unwrap(), data);
}

#[test]
#[should_panic(expected = "consumed 9999 bytes of pending output")]
fn streaming_encoder_rejects_consuming_more_than_it_produced() {
    // Clamping instead would silently drop compressed bytes and leave a
    // truncated frame that still parses as a frame.
    let mut encoder = StreamingEncoder::new(EncoderOptions::default()).unwrap();
    encoder.push(&build_pattern(200_000)).unwrap();
    let pending = encoder.pending_output_len();
    assert!(pending > 0 && pending < 9999, "pending was {pending}");
    encoder.consume_output(9999);
}

#[test]
fn streaming_encoder_take_output_after_a_partial_read_keeps_the_remainder() {
    // `take_output` and `read` address the same queue, so mixing them must not
    // hand back a byte twice or lose one between them.
    let data = build_pattern(200_000);
    let mut encoder = StreamingEncoder::new(EncoderOptions::default()).unwrap();
    encoder.push(&data).unwrap();

    let mut head = vec![0u8; 100];
    let read = encoder.read(&mut head);
    assert_eq!(read, 100);

    let mut out = head[..read].to_vec();
    out.extend_from_slice(&encoder.take_output());
    encoder.finish().unwrap();
    out.extend_from_slice(&encoder.take_output());

    assert_eq!(decode_all(&out).unwrap(), data);
}

#[test]
fn the_recommended_output_size_takes_a_whole_block_in_one_read() {
    // The constant's claim. An incompressible block is the worst case: it is
    // emitted raw, so the payload is the full block and the frame's header and
    // checksum have to fit beside it.
    let mut noise = Vec::with_capacity(400_000);
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    while noise.len() < 400_000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        noise.extend_from_slice(&state.to_le_bytes());
    }

    let mut encoder = StreamingEncoder::new(EncoderOptions {
        checksum: true,
        ..Default::default()
    })
    .unwrap();
    let mut scratch = vec![0u8; StreamingEncoder::RECOMMENDED_OUTPUT_SIZE];
    let mut out = Vec::new();
    let mut reads_that_filled_the_buffer = 0;

    for chunk in noise.chunks(StreamingEncoder::RECOMMENDED_INPUT_SIZE) {
        encoder.push(chunk).unwrap();
        loop {
            let produced = encoder.read(&mut scratch);
            if produced == 0 {
                break;
            }
            if produced == scratch.len() {
                reads_that_filled_the_buffer += 1;
            }
            out.extend_from_slice(&scratch[..produced]);
        }
    }
    encoder.finish().unwrap();
    drain_encoder(&mut encoder, &mut scratch, &mut out);

    assert_eq!(decode_all(&out).unwrap(), noise);
    assert_eq!(
        reads_that_filled_the_buffer, 0,
        "a read filled the recommended buffer exactly, so the next block may \
         not have fit in one and the constant is too small"
    );
}

/// A frame under test: what it is, its bytes, and what it decodes to.
///
/// `payload` is `None` for the rows a bare `decode_all` cannot check — a
/// dictionary frame and a skippable frame — which are here for the walk, not
/// for the round trip.
struct ZooFrame {
    name: &'static str,
    bytes: Vec<u8>,
    payload: Option<Vec<u8>>,
}

fn build_noise(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size + 8);
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    while out.len() < size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(size);
    out
}

/// One frame of each shape the size walk has to handle.
///
/// The walk skips block payloads by their headers, so what varies here is what
/// makes those headers differ: compressed blocks against raw ones against RLE
/// ones, a single-segment frame with no window descriptor, a checksum trailing
/// the last block, a dictionary id widening the header, and a skippable frame
/// that has no blocks at all.
fn frame_zoo() -> Vec<ZooFrame> {
    fn row(name: &'static str, bytes: Vec<u8>, payload: Option<Vec<u8>>) -> ZooFrame {
        ZooFrame {
            name,
            bytes,
            payload,
        }
    }

    let text = build_pattern(300_000);
    let noise = build_noise(300_000);
    let run = vec![b'z'; 200_000];
    let tiny = b"single segment".to_vec();
    let dictionary = build_pattern(4096);
    let prepared = EncoderDictionary::new(&dictionary).unwrap();
    let with_dict = build_pattern(50_000);

    vec![
        row(
            "multi-block, content size declared",
            encode_all(&text).unwrap(),
            Some(text.clone()),
        ),
        row(
            "multi-block, checksum",
            encode_all_with_options(
                &text,
                EncoderOptions {
                    checksum: true,
                    ..Default::default()
                },
            )
            .unwrap(),
            Some(text.clone()),
        ),
        row(
            "multi-block, no content size",
            encode_all_with_options(
                &text,
                EncoderOptions {
                    write_content_size: false,
                    ..Default::default()
                },
            )
            .unwrap(),
            Some(text),
        ),
        row(
            "incompressible, raw blocks",
            encode_all(&noise).unwrap(),
            Some(noise),
        ),
        row("rle blocks", encode_all(&run).unwrap(), Some(run)),
        row("single segment", encode_all(&tiny).unwrap(), Some(tiny)),
        row("empty frame", encode_all(b"").unwrap(), Some(Vec::new())),
        row(
            "dictionary id in the header",
            encode_all_with_prepared_dict(&with_dict, &prepared).unwrap(),
            None,
        ),
        row(
            "skippable frame",
            write_skippable_frame(7, b"sidecar metadata").unwrap(),
            None,
        ),
    ]
}

#[test]
fn find_frame_compressed_size_lands_on_every_frame_boundary() {
    // The claim is exactly checkable: cut the stream where the walk says the
    // frame ends, and the piece must decode to that frame's payload while the
    // remainder still starts with a frame.
    let zoo = frame_zoo();
    let mut stream = Vec::new();
    for frame in &zoo {
        stream.extend_from_slice(&frame.bytes);
    }

    let mut pos = 0;
    for frame in &zoo {
        let name = frame.name;
        let measured = zstandard::find_frame_compressed_size(&stream[pos..]).unwrap();
        assert_eq!(measured, frame.bytes.len(), "{name}");

        let slice = &stream[pos..pos + measured];
        assert_eq!(slice, &frame.bytes[..], "{name}");
        if let Some(payload) = &frame.payload {
            assert_eq!(&decode_all(slice).unwrap(), payload, "{name}");
        }
        pos += measured;
    }
    assert_eq!(pos, stream.len(), "the walk did not consume the stream");
}

#[test]
fn find_frame_compressed_size_reports_truncation_rather_than_a_short_frame() {
    // Answering from a truncated frame would be worse than failing: the caller
    // would slice at the wrong place and hand a decoder something that is not
    // a frame.
    for frame in frame_zoo() {
        let name = frame.name;
        let bytes = &frame.bytes;
        for cut in [1usize, 3, 5, 9, bytes.len() / 2, bytes.len() - 1] {
            if cut == 0 || cut >= bytes.len() {
                continue;
            }
            let err = zstandard::find_frame_compressed_size(&bytes[..cut]).unwrap_err();
            assert!(
                matches!(err, Error::UnexpectedEof | Error::SrcSizeWrong),
                "{name} cut at {cut} reported {err:?}"
            );
        }
    }
}

#[test]
fn decompress_bound_is_never_below_what_the_frames_actually_produce() {
    // The one property the bound must have. Frames that declare their content
    // size make it exact; the rest are bounded by their block count, which is
    // looser but must never be short.
    let zoo = frame_zoo();
    let mut checked = 0;
    for frame in &zoo {
        let name = frame.name;
        let Some(payload) = &frame.payload else {
            continue;
        };
        let bound = zstandard::decompress_bound(&frame.bytes).unwrap();
        assert!(
            bound >= payload.len() as u64,
            "{name}: bound {bound} is below the {} bytes it decodes to",
            payload.len()
        );
        assert_eq!(decode_all(&frame.bytes).unwrap(), *payload, "{name}");
        checked += 1;
    }
    assert!(checked >= 7, "only {checked} rows were decodable");

    // And over the concatenation, which is the form a caller sizing one buffer
    // for a whole stream would ask about. The skippable frame is included: it
    // contributes nothing to the output and must not be counted as if it did.
    let mut stream = Vec::new();
    let mut total = 0u64;
    for frame in &zoo {
        if frame.name == "dictionary id in the header" {
            continue;
        }
        stream.extend_from_slice(&frame.bytes);
        total += frame.payload.as_ref().map_or(0, Vec::len) as u64;
    }
    assert!(zstandard::decompress_bound(&stream).unwrap() >= total);
    assert_eq!(decode_all(&stream).unwrap().len() as u64, total);
}

#[test]
fn a_declared_content_size_makes_the_bound_exact() {
    for size in [0usize, 1, 100, 130_000, 300_000] {
        let payload = build_pattern(size);
        let frame = encode_all(&payload).unwrap();
        assert_eq!(
            zstandard::decompress_bound(&frame).unwrap(),
            size as u64,
            "size {size}"
        );
        assert_eq!(
            zstandard::decompressed_size(&frame).unwrap(),
            Some(size as u64),
            "size {size}"
        );
    }
}

#[test]
fn an_undeclared_content_size_bounds_by_blocks_and_reports_none() {
    let payload = build_pattern(300_000);
    let frame = encode_all_with_options(
        &payload,
        EncoderOptions {
            write_content_size: false,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(zstandard::decompressed_size(&frame).unwrap(), None);
    let bound = zstandard::decompress_bound(&frame).unwrap();
    assert!(
        bound >= payload.len() as u64,
        "bound {bound} is below the payload"
    );
    // Loose, but not unboundedly so: one maximal block per block header.
    let blocks = payload.len().div_ceil(128 * 1024) as u64;
    assert!(
        bound <= blocks.max(1) * 128 * 1024,
        "bound {bound} exceeds one maximal block per block in the frame"
    );
}

#[test]
fn one_undeclared_frame_makes_the_whole_stream_undeclared() {
    // A sum that silently skipped the frame that did not declare would be
    // short, and a caller sizing a buffer from it would overflow it.
    let declared = encode_all(&build_pattern(1000)).unwrap();
    let undeclared = encode_all_with_options(
        &build_pattern(1000),
        EncoderOptions {
            write_content_size: false,
            ..Default::default()
        },
    )
    .unwrap();

    let mut stream = declared.clone();
    stream.extend_from_slice(&declared);
    assert_eq!(zstandard::decompressed_size(&stream).unwrap(), Some(2000));

    stream.extend_from_slice(&undeclared);
    assert_eq!(zstandard::decompressed_size(&stream).unwrap(), None);

    // The frame that declares nothing is last here, so a fold that stopped at
    // the first frame would still say `Some`.
    let mut leading = undeclared.clone();
    leading.extend_from_slice(&declared);
    assert_eq!(zstandard::decompressed_size(&leading).unwrap(), None);
}

#[test]
fn the_size_walk_reads_magicless_frames_when_told_to() {
    let payload = build_pattern(200_000);
    let frame = encode_all_with_options(
        &payload,
        EncoderOptions {
            format: Format::Zstd1Magicless,
            checksum: true,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        zstandard::find_frame_compressed_size_with_format(&frame, Format::Zstd1Magicless).unwrap(),
        frame.len()
    );
    assert_eq!(
        zstandard::decompressed_size_with_format(&frame, Format::Zstd1Magicless).unwrap(),
        Some(payload.len() as u64)
    );
    assert_eq!(
        zstandard::decompress_bound_with_format(&frame, Format::Zstd1Magicless).unwrap(),
        payload.len() as u64
    );
    // And reading it as a normal frame fails rather than answering wrongly.
    assert!(zstandard::find_frame_compressed_size(&frame).is_err());
}

#[test]
fn the_size_walk_rejects_empty_and_unrecognised_input() {
    assert!(matches!(
        zstandard::find_frame_compressed_size(b"").unwrap_err(),
        Error::UnexpectedEof
    ));
    assert!(matches!(
        zstandard::decompress_bound(b"").unwrap_err(),
        Error::UnexpectedEof
    ));
    assert!(matches!(
        zstandard::decompressed_size(b"").unwrap_err(),
        Error::UnexpectedEof
    ));
    assert!(matches!(
        zstandard::find_frame_compressed_size(b"not a zstd frame").unwrap_err(),
        Error::BadMagic(_)
    ));
}

#[test]
fn streaming_encoder_debug_reports_what_is_still_pending() {
    // The `Debug` field is named `pending_output_len`, and a caller reading it
    // to decide whether to drain again must not be shown the drained prefix.
    let mut encoder = StreamingEncoder::new(EncoderOptions::default()).unwrap();
    encoder.push(&build_pattern(200_000)).unwrap();
    let pending = encoder.pending_output_len();
    assert!(pending > 200, "nothing was produced to drain");

    let mut sink = vec![0u8; 100];
    assert_eq!(encoder.read(&mut sink), 100);

    let rendered = format!("{encoder:?}");
    assert!(
        rendered.contains(&format!("pending_output_len: {}", pending - 100)),
        "after reading 100 of {pending} bytes, Debug rendered {rendered}"
    );
}

/// Bytes laid down after a fixed decode destination so an overrun has
/// somewhere to show itself.
///
/// The whole point of the byte-exact tail path is that nothing is written past
/// `dst`. Asserting on the decoded bytes alone cannot see a violation: the
/// wildcopy's overshoot lands *after* the output, so the output is correct
/// either way and the damage is to whatever the caller put next in memory. This
/// is what notices. Two `WILDCOPY_OVERLENGTH`s wide, so a regression that
/// overshoots further than the one being guarded against is caught too.
const DECODE_GUARD: usize = 64;
const DECODE_GUARD_BYTE: u8 = 0xA5;

/// Decode into a `capacity`-byte destination that is followed by guard bytes.
///
/// Returns what the decode reported and the destination's contents. Panics if
/// the decoder touched a byte past the destination, whether the decode
/// succeeded or failed.
fn decode_into_guarded_slice(
    compressed: &[u8],
    capacity: usize,
    options: DecoderOptions,
) -> (Result<usize, Error>, Vec<u8>) {
    let mut backing = vec![DECODE_GUARD_BYTE; capacity + DECODE_GUARD];
    let (dst, guard) = backing.split_at_mut(capacity);
    let result = zstandard::decode_into_slice_with_options(compressed, dst, options);
    let clobbered = guard
        .iter()
        .filter(|&&byte| byte != DECODE_GUARD_BYTE)
        .count();
    assert_eq!(
        clobbered, 0,
        "the decoder wrote over {clobbered} of the {DECODE_GUARD} guard bytes \
         following a {capacity}-byte destination"
    );
    backing.truncate(capacity);
    (result, backing)
}

/// Repetitive data with a short period and a drifting field.
///
/// `build_pattern` compresses into a handful of very long matches, which puts
/// at most one sequence anywhere near the end of the frame. This shape puts
/// many short ones there, so the byte-exact tail path has to take over
/// mid-block and run for several sequences rather than one.
fn build_short_matches(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size + 12);
    let mut counter = 0u32;
    while out.len() < size {
        out.extend_from_slice(b"abcdefgh");
        out.extend_from_slice(&counter.to_le_bytes());
        counter = counter.wrapping_add(1);
    }
    out.truncate(size);
    out
}

#[test]
fn decode_into_slice_matches_decode_all_across_the_frame_zoo() {
    for frame in frame_zoo() {
        let Some(payload) = frame.payload else {
            continue;
        };
        let (result, filled) =
            decode_into_guarded_slice(&frame.bytes, payload.len(), DecoderOptions::default());
        let written = result.unwrap_or_else(|error| panic!("{}: {error:?}", frame.name));
        assert_eq!(written, payload.len(), "{}", frame.name);
        assert_eq!(filled, payload, "{}", frame.name);
    }
}

#[test]
fn decode_into_slice_needs_no_headroom_at_any_tail_alignment() {
    // The claim under test is that a destination of exactly the decompressed
    // size is enough — no padding, which is what a C caller assumes and what
    // `decompressed_size` hands them.
    //
    // What decides whether that holds is where the last sequences land relative
    // to the end of the buffer, because the wildcopy overshoots a match by up
    // to 31 bytes. Sweeping every length across two runs longer than that puts
    // the final match at every distance from the end, and the second run
    // crosses a block boundary so the tail path is entered on a later block
    // with history behind it rather than on the first.
    for size in (0usize..200).chain(131_000..131_120) {
        for (name, input) in [
            ("short matches", build_short_matches(size)),
            ("long matches", build_pattern(size)),
            ("incompressible", build_noise(size)),
        ] {
            let compressed = encode_all_with_options(
                &input,
                EncoderOptions {
                    checksum: true,
                    ..Default::default()
                },
            )
            .unwrap();
            let (result, filled) =
                decode_into_guarded_slice(&compressed, size, DecoderOptions::default());
            assert_eq!(
                result,
                Ok(size),
                "{name} at {size} bytes: an exactly-sized destination was refused"
            );
            assert_eq!(filled, input, "{name} at {size} bytes");
        }
    }
}

#[test]
fn decode_into_slice_repeats_a_short_offset_match_in_phase() {
    // An overlapping match is expanded by `append_match_from_history`, which a
    // short offset once routed to a separate expander: it tiled the offset into
    // a 32-byte buffer and stamped that buffer out every 32 bytes, which
    // restarts the period out of phase for every offset that does not divide 32
    // -- 3, 5, 6 and 7 -- silently corrupting the match past its first 32 bytes.
    // The periods that do divide 32 are the control.
    //
    // This reaches that expander the way an ordinary caller does: only a fixed
    // destination runs a prefix match through it, since a growable one has the
    // wildcopy slack the hot executor needs. See
    // `decode_with_dict_repeats_a_match_across_the_dictionary_boundary_in_phase`
    // for the other route in, which a growable destination does reach.
    for period in 2..=7usize {
        let unit: Vec<u8> = (0..period).map(|index| b'a' + index as u8).collect();
        for size in [40usize, 64, 65, 96, 200, 1000, 5000] {
            let input: Vec<u8> = unit.iter().copied().cycle().take(size).collect();
            let compressed = encode_all(&input).unwrap();
            let (result, filled) =
                decode_into_guarded_slice(&compressed, size, DecoderOptions::default());
            assert_eq!(
                result,
                Ok(size),
                "period {period} at {size} bytes: an exactly-sized destination was refused"
            );
            assert_eq!(filled, input, "period {period} at {size} bytes");
        }
    }
}

/// Pack a sequence bitstream: bits in the order the decoder reads them, the
/// first landing at the top of the last byte and running down into earlier
/// ones. The decoder requires the stream to be consumed exactly, so the caller
/// has to hand over a whole number of bytes.
fn pack_sequence_bitstream(bits: &[u8]) -> Vec<u8> {
    assert_eq!(
        bits.len() % 8,
        0,
        "the bitstream must end on a byte boundary"
    );
    let mut out = vec![0u8; bits.len() / 8];
    let last = out.len() - 1;
    for (index, &bit) in bits.iter().enumerate() {
        out[last - index / 8] |= bit << (7 - index % 8);
    }
    out
}

#[test]
fn decode_with_dict_repeats_a_match_across_the_dictionary_boundary_in_phase() {
    // The other way into the short-offset expander, and the one a growable
    // destination reaches: a match that starts inside the dictionary and runs
    // on past the frame's first byte is split, and the part continuing into the
    // frame is appended from history at an offset of everything produced so far
    // -- under 8 whenever the match begins near the frame start. So
    // `decode_all_with_dict` could return corrupted bytes, not just the
    // fixed-destination entry points.
    //
    // No encoder here emits this shape, and no fuzz target decodes with a
    // dictionary at all, so the frame is built by hand: zero literals and one
    // sequence reaching `period` bytes back into the dictionary tail.
    for period in 2..=7u32 {
        let unit: Vec<u8> = (0..period).map(|index| b'a' + index as u8).collect();
        let mut dictionary = vec![0u8; 4000];
        dictionary.extend_from_slice(&unit);

        // `offset_value` is the stored offset, three above the real one. Its
        // code is the position of its high bit, and the remainder is sent as
        // that many extra bits.
        let offset_value = period + 3;
        let offset_code = 31 - offset_value.leading_zeros();
        let offset_extra = offset_value - (1 << offset_code);

        // The match-length code is chosen so its extra bits, the offset's, and
        // the one padding bit fill exactly one byte. Both codes below carry a
        // match far longer than the 32 bytes the old expander got right.
        let (match_code, match_baseline, match_bits) = match offset_code {
            2 => (42u8, 99u32, 5u32),
            _ => (41, 83, 4),
        };
        let match_length = match_baseline + ((1 << match_bits) - 1);

        let mut bits = vec![1u8];
        for shift in (0..offset_code).rev() {
            bits.push(((offset_extra >> shift) & 1) as u8);
        }
        for shift in (0..match_bits).rev() {
            bits.push((((match_length - match_baseline) >> shift) & 1) as u8);
        }

        // Literal-length, offset and match-length tables all in RLE mode, so
        // each is one symbol byte and the bitstream carries only extra bits.
        let mut payload = raw_literals_section(b"");
        payload.extend_from_slice(&[1, 0b0101_0100, 0, offset_code as u8, match_code]);
        payload.extend_from_slice(&pack_sequence_bitstream(&bits));

        let mut frame = write_single_segment_header(match_length as usize);
        append_compressed_block(&mut frame, &payload, true);

        let expected: Vec<u8> = unit
            .iter()
            .copied()
            .cycle()
            .take(match_length as usize)
            .collect();
        let decoded = decode_all_with_dict(&frame, &dictionary)
            .unwrap_or_else(|error| panic!("period {period}: {error:?}"));
        assert_eq!(decoded, expected, "period {period}");
    }
}

#[test]
fn decode_into_slice_reports_too_small_rather_than_truncating() {
    let input = build_short_matches(300_000);
    let compressed = encode_all(&input).unwrap();

    // One byte short is the interesting case: everything decodes and only the
    // final write does not fit. Reporting success there would hand back a
    // truncated message that reads as a complete one.
    for capacity in [0usize, 1, 13, input.len() / 2, input.len() - 1] {
        let (result, _) =
            decode_into_guarded_slice(&compressed, capacity, DecoderOptions::default());
        assert_eq!(
            result,
            Err(Error::DstSizeTooSmall),
            "a {capacity}-byte destination must not accept {} bytes of output",
            input.len()
        );
    }

    let (result, filled) =
        decode_into_guarded_slice(&compressed, input.len(), DecoderOptions::default());
    assert_eq!(result, Ok(input.len()));
    assert_eq!(filled, input);
}

#[test]
fn a_destination_sized_by_decompress_bound_always_fits() {
    // `decompress_bound` is the sizing helper for frames that declare nothing,
    // and it is only useful if it is never short. The no-content-size rows of
    // the zoo are the ones that exercise the estimate rather than the
    // declaration.
    for frame in frame_zoo() {
        let Some(payload) = frame.payload else {
            continue;
        };
        let bound = zstandard::decompress_bound(&frame.bytes)
            .unwrap_or_else(|error| panic!("{}: {error:?}", frame.name));
        assert!(
            bound >= payload.len() as u64,
            "{}: bound {bound} is below the {} bytes it decodes to",
            frame.name,
            payload.len()
        );

        let (result, filled) = decode_into_guarded_slice(
            &frame.bytes,
            usize::try_from(bound).unwrap(),
            DecoderOptions::default(),
        );
        let written = result.unwrap_or_else(|error| panic!("{}: {error:?}", frame.name));
        assert_eq!(&filled[..written], &payload[..], "{}", frame.name);
    }
}

#[test]
fn decode_into_slice_walks_a_concatenated_stream() {
    // Same contract as `decode_all`: every frame decoded and concatenated, and
    // skippable frames passed over rather than counted.
    let mut stream = Vec::new();
    let mut expected = Vec::new();
    for frame in frame_zoo() {
        if let Some(payload) = &frame.payload {
            stream.extend_from_slice(&frame.bytes);
            expected.extend_from_slice(payload);
        } else if frame.name == "skippable frame" {
            stream.extend_from_slice(&frame.bytes);
        }
    }

    let (result, filled) =
        decode_into_guarded_slice(&stream, expected.len(), DecoderOptions::default());
    assert_eq!(result, Ok(expected.len()));
    assert_eq!(filled, expected);
    assert_eq!(decode_all(&stream).unwrap(), expected);
}

#[test]
fn a_failed_slice_decode_leaves_the_decoder_usable() {
    // The fixed destination is per-call state. If a refused write left anything
    // behind in the decoder, the next call would fail on a destination that was
    // never too small.
    let input = build_short_matches(50_000);
    let compressed = encode_all(&input).unwrap();
    let mut decoder = Decoder::new();

    let mut too_small = vec![0u8; 16];
    assert_eq!(
        decoder.decode_into_slice(&compressed, &mut too_small),
        Err(Error::DstSizeTooSmall)
    );

    let mut exact = vec![0u8; input.len()];
    let written = decoder.decode_into_slice(&compressed, &mut exact).unwrap();
    assert_eq!(written, input.len());
    assert_eq!(exact, input);

    // And the growable entry points on the same decoder are unaffected.
    assert_eq!(decoder.decode_all(&compressed).unwrap(), input);
}

#[test]
fn slice_decoding_works_with_a_prepared_dictionary() {
    let dictionary_bytes = build_pattern(8_192);
    let encoding = EncoderDictionary::new(&dictionary_bytes).unwrap();
    let decoding = DecoderDictionary::new(&dictionary_bytes).unwrap();
    let input = build_short_matches(20_000);
    let compressed = encode_all_with_prepared_dict(&input, &encoding).unwrap();

    let mut dst = vec![0u8; input.len()];
    let written = Decoder::new()
        .decode_into_slice_with_prepared_dict(&compressed, &mut dst, &decoding)
        .unwrap();
    assert_eq!(written, input.len());
    assert_eq!(dst, input);

    // A dictionary match reaching back before the frame takes a different copy
    // path from a prefix match, and it has to respect the destination's end
    // too. One byte short is what says so.
    let mut short = vec![0u8; input.len() - 1];
    assert_eq!(
        Decoder::new().decode_into_slice_with_prepared_dict(&compressed, &mut short, &decoding),
        Err(Error::DstSizeTooSmall)
    );
}

#[test]
fn the_output_cap_outranks_the_destination_size() {
    // Two different refusals that both mean "it did not fit", and a caller
    // needs them apart: `DstSizeTooSmall` says give me a bigger buffer,
    // `OutputSizeTooLarge` says the frame exceeded the guard you asked for and
    // a bigger buffer will not help.
    let input = build_short_matches(200_000);
    let compressed = encode_all(&input).unwrap();

    let capped = DecoderOptions {
        max_output_size: Some(1_000),
        ..Default::default()
    };
    let (result, _) = decode_into_guarded_slice(&compressed, input.len(), capped);
    assert!(
        matches!(result, Err(Error::OutputSizeTooLarge { .. })),
        "a roomy destination under a small cap must report the cap, got {result:?}"
    );

    let (result, _) =
        decode_into_guarded_slice(&compressed, input.len() - 1, DecoderOptions::default());
    assert_eq!(result, Err(Error::DstSizeTooSmall));
}

#[test]
fn slice_decoding_rejects_a_corrupt_frame_without_overrunning() {
    // Corruption reaches the tail path with lengths the frame invented, and the
    // destination is exactly sized, so there is no slack to absorb a bad one.
    // What must not happen is a write past `dst` on the way to the error.
    let input = build_short_matches(40_000);
    let compressed = encode_all(&input).unwrap();

    for cut in [
        compressed.len() / 3,
        compressed.len() / 2,
        compressed.len() - 1,
    ] {
        let (result, _) =
            decode_into_guarded_slice(&compressed[..cut], input.len(), DecoderOptions::default());
        assert!(result.is_err(), "a frame cut at {cut} decoded anyway");
    }

    for flip in [compressed.len() / 4, compressed.len() / 2] {
        let mut damaged = compressed.clone();
        damaged[flip] ^= 0xFF;
        // Either outcome is acceptable — a flipped bit may still describe a
        // legal frame — but the guard assertion inside the helper is not
        // optional either way.
        let _ = decode_into_guarded_slice(&damaged, input.len(), DecoderOptions::default());
    }
}

/// Input whose final sequence is a match of exactly `match_length` bytes,
/// followed by `trailing` bytes that match nothing.
///
/// The wildcopy's overshoot is worst when the output ends in a match. Its
/// 32-bytes-per-iteration loop stops on the first iteration that reaches the
/// match length, so how far it runs past that length is decided by
/// `match_length` modulo 32, and the worst case is a full 31 bytes. What
/// decides whether that runs off the end of the destination is how much output
/// follows, which is what `trailing` sweeps.
///
/// A generic corpus does not reliably end in a match at all, let alone in one
/// of every length at every distance from the end. This builds one that does: a
/// run of noise nothing else in the frame can match, compressible filler to
/// keep the block worth compressing at all, the head of that noise again as the
/// closing match, and a slice of noise from elsewhere as literals nothing can
/// match.
fn build_ending_in_match(match_length: usize, trailing: usize) -> Vec<u8> {
    let noise = build_noise(256);
    let mut out = noise[..96].to_vec();
    out.extend_from_slice(&build_pattern(4_096));
    out.extend_from_slice(&noise[..match_length]);
    out.extend_from_slice(&noise[160..160 + trailing]);
    out
}

#[test]
fn a_match_near_the_end_does_not_overshoot_the_destination() {
    // `WILDCOPY_OVERLENGTH` is 32, so every distance a closing match can sit
    // from the end of the output and still have its overshoot land outside is
    // covered by sweeping `trailing` past it, and every overshoot length is
    // covered by sweeping `match_length` past 32.
    for match_length in 3usize..=96 {
        for trailing in 0usize..=34 {
            let input = build_ending_in_match(match_length, trailing);
            let compressed = encode_all(&input).unwrap();
            let (result, filled) =
                decode_into_guarded_slice(&compressed, input.len(), DecoderOptions::default());
            assert_eq!(
                result,
                Ok(input.len()),
                "a {match_length}-byte match {trailing} bytes from the end was refused an \
                 exactly-sized destination"
            );
            assert_eq!(filled, input, "match {match_length}, trailing {trailing}");
        }
    }
}
