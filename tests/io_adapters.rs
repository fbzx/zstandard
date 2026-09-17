//! Coverage for the `std::io` adapters, including the composition cases that
//! motivate them: `io::copy`, chunked writers, and short reads.

use std::io::{Read, Write};

use zstandard::io::{Reader, Writer};
use zstandard::{CompressionLevel, DecoderOptions, EncoderOptions, decode_all, encode_all};

fn corpus(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut i = 0u64;
    while out.len() < len {
        out.extend_from_slice(
            format!("record {i} name=widget-{} qty={}\n", i % 977, i % 13).as_bytes(),
        );
        i += 1;
    }
    out.truncate(len);
    out
}

#[test]
fn writer_output_is_a_normal_frame() {
    let payload = corpus(300_000);
    let mut writer = Writer::new(Vec::new()).unwrap();
    writer.write_all(&payload).unwrap();
    let compressed = writer.finish().unwrap();

    // Anything that reads a `.zst` must accept it, not just our Reader.
    assert_eq!(decode_all(&compressed).unwrap(), payload);
    assert!(compressed.len() < payload.len() / 4);
}

#[test]
fn reader_decodes_frames_from_the_one_shot_encoder() {
    let payload = corpus(300_000);
    let compressed = encode_all(&payload).unwrap();

    let mut restored = Vec::new();
    Reader::new(&compressed[..])
        .read_to_end(&mut restored)
        .unwrap();
    assert_eq!(restored, payload);
}

#[test]
fn roundtrips_through_io_copy() {
    let payload = corpus(500_000);

    let mut writer = Writer::new(Vec::new()).unwrap();
    std::io::copy(&mut &payload[..], &mut writer).unwrap();
    let compressed = writer.finish().unwrap();

    let mut restored = Vec::new();
    std::io::copy(&mut Reader::new(&compressed[..]), &mut restored).unwrap();
    assert_eq!(restored, payload);
}

/// Write boundaries must not change the output's meaning.
#[test]
fn arbitrary_write_chunking_roundtrips() {
    let payload = corpus(400_000);
    for chunk in [1, 7, 1024, 65_536, 300_000] {
        let mut writer = Writer::new(Vec::new()).unwrap();
        for piece in payload.chunks(chunk) {
            writer.write_all(piece).unwrap();
        }
        let compressed = writer.finish().unwrap();
        assert_eq!(
            decode_all(&compressed).unwrap(),
            payload,
            "write chunk {chunk}"
        );
    }
}

/// Read boundaries must not change the output's meaning, including across
/// concatenated frames.
#[test]
fn arbitrary_read_chunking_roundtrips() {
    let first = corpus(200_000);
    let second = corpus(300_000);
    let mut stream = encode_all(&first).unwrap();
    stream.extend_from_slice(&encode_all(&second).unwrap());
    let mut expected = first;
    expected.extend_from_slice(&second);

    for chunk in [1, 7, 1024, 65_536, 300_000] {
        let mut reader = Reader::new(&stream[..]);
        let mut restored = Vec::new();
        let mut buf = vec![0u8; chunk];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            restored.extend_from_slice(&buf[..n]);
        }
        assert_eq!(restored, expected, "read chunk {chunk}");
    }
}

/// A reader that hands back at most `limit` bytes per call, the way a socket
/// or a pipe does.
struct Trickle<'a> {
    data: &'a [u8],
    limit: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, dst: &mut [u8]) -> std::io::Result<usize> {
        let n = self.data.len().min(dst.len()).min(self.limit);
        dst[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

#[test]
fn short_reads_from_the_inner_reader_roundtrip() {
    let payload = corpus(200_000);
    let compressed = encode_all(&payload).unwrap();

    for limit in [1, 3, 997, 8192] {
        let mut restored = Vec::new();
        Reader::new(Trickle {
            data: &compressed,
            limit,
        })
        .read_to_end(&mut restored)
        .unwrap();
        assert_eq!(restored, payload, "read limit {limit}");
    }
}

#[test]
fn reader_reports_truncated_input_instead_of_a_short_read() {
    let payload = corpus(200_000);
    let compressed = encode_all(&payload).unwrap();

    let mut restored = Vec::new();
    let err = Reader::new(&compressed[..compressed.len() - 32])
        .read_to_end(&mut restored)
        .expect_err("a truncated frame must not read as a clean EOF");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn writer_honors_encoder_options() {
    let payload = corpus(300_000);
    let options = EncoderOptions {
        compression_level: CompressionLevel::BEST,
        checksum: true,
        ..Default::default()
    };
    let mut writer = Writer::with_options(Vec::new(), options).unwrap();
    writer.write_all(&payload).unwrap();
    let strong = writer.finish().unwrap();

    let mut writer = Writer::with_options(
        Vec::new(),
        EncoderOptions {
            compression_level: CompressionLevel::FASTEST,
            ..Default::default()
        },
    )
    .unwrap();
    writer.write_all(&payload).unwrap();
    let fast = writer.finish().unwrap();

    assert_eq!(decode_all(&strong).unwrap(), payload);
    assert_eq!(decode_all(&fast).unwrap(), payload);
    assert!(
        strong.len() < fast.len(),
        "level 22 ({}) should beat level 1 ({})",
        strong.len(),
        fast.len()
    );
}

/// `flush` must make already-written bytes readable without ending the frame.
#[test]
fn flush_makes_progress_visible_mid_stream() {
    let head = corpus(150_000);
    let tail = corpus(150_000);

    let mut writer = Writer::new(Vec::new()).unwrap();
    writer.write_all(&head).unwrap();
    writer.flush().unwrap();
    let after_flush = writer.get_ref().len();
    assert!(after_flush > 0, "flush should have emitted blocks");

    writer.write_all(&tail).unwrap();
    let compressed = writer.finish().unwrap();

    let mut expected = head.clone();
    expected.extend_from_slice(&tail);
    assert_eq!(decode_all(&compressed).unwrap(), expected);
}

#[test]
fn reader_respects_decoder_limits() {
    let payload = corpus(300_000);
    let compressed = encode_all(&payload).unwrap();

    let options = DecoderOptions {
        max_output_size: Some(1024),
        ..Default::default()
    };
    let mut restored = Vec::new();
    let err = Reader::with_options(&compressed[..], options)
        .read_to_end(&mut restored)
        .expect_err("output limit must be enforced");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn reader_handles_concatenated_frames() {
    let first = corpus(50_000);
    let second = corpus(70_000);
    let mut stream = encode_all(&first).unwrap();
    stream.extend_from_slice(&encode_all(&second).unwrap());

    let mut restored = Vec::new();
    Reader::new(&stream[..]).read_to_end(&mut restored).unwrap();

    let mut expected = first.clone();
    expected.extend_from_slice(&second);
    assert_eq!(restored, expected);
}

#[test]
fn empty_input_roundtrips() {
    let mut writer = Writer::new(Vec::new()).unwrap();
    writer.write_all(b"").unwrap();
    let compressed = writer.finish().unwrap();

    let mut restored = Vec::new();
    Reader::new(&compressed[..])
        .read_to_end(&mut restored)
        .unwrap();
    assert!(restored.is_empty());
}

/// `Error` must survive the trip through `io::Error` so callers can inspect it.
#[test]
fn codec_errors_are_recoverable_from_io_errors() {
    let mut restored = Vec::new();
    let err = Reader::new(&b"not a zstd frame at all"[..])
        .read_to_end(&mut restored)
        .expect_err("garbage input must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

    let inner = err
        .into_inner()
        .expect("the codec error should be attached");
    assert!(
        inner.downcast_ref::<zstandard::Error>().is_some(),
        "expected a zstandard::Error payload, got {inner:?}"
    );
}

#[test]
fn dictionary_roundtrip_through_the_adapters() {
    let dictionary = corpus(4096);
    let prepared = zstandard::EncoderDictionary::new(&dictionary).unwrap();
    let prepared_decoding = zstandard::DecoderDictionary::new(&dictionary).unwrap();
    let payload = corpus(200_000);

    let mut writer =
        Writer::with_prepared_dict(Vec::new(), &prepared, EncoderOptions::default()).unwrap();
    writer.write_all(&payload).unwrap();
    let compressed = writer.finish().unwrap();

    let mut restored = Vec::new();
    Reader::with_prepared_dict(
        &compressed[..],
        &prepared_decoding,
        DecoderOptions::default(),
    )
    .read_to_end(&mut restored)
    .unwrap();
    assert_eq!(restored, payload);

    // And the frame is a normal dictionary frame, readable by the one-shot API.
    assert_eq!(
        zstandard::decode_all_with_prepared_dict(&compressed, &prepared_decoding).unwrap(),
        payload
    );
}

/// A `Writer` that pledges a size of one block or less writes the frame the
/// one-shot encoder writes, whatever it is handed and however it is chunked.
///
/// `Writer` owns a `StreamingEncoder` and forwards to its `push` and `finish`,
/// so it inherits `push_target` holding the only block back for `finish` to
/// flag. That is worth a test of its own because it is the form most callers
/// reach for -- `pledged_src_size` is usually known when the payload is a file
/// or an in-memory buffer -- and because `Writer::finish` must reach the
/// encoder's `finish` without a `flush` in between, which would send the block
/// early and put the empty last block back.
#[test]
fn a_pledged_writer_of_one_block_matches_one_shot() {
    for size in [1usize, 4096, 128 * 1024] {
        let payload = corpus(size);
        let options = EncoderOptions {
            compression_level: CompressionLevel::try_new(9).unwrap(),
            pledged_src_size: Some(size as u64),
            ..Default::default()
        };
        let one_shot = zstandard::encode_all_with_options(&payload, options).unwrap();

        for chunk in [size, 1000] {
            let mut writer = Writer::with_options(Vec::new(), options).unwrap();
            for piece in payload.chunks(chunk.max(1)) {
                writer.write_all(piece).unwrap();
            }
            let compressed = writer.finish().unwrap();
            assert_eq!(
                compressed, one_shot,
                "{size} bytes written {chunk} at a time: Writer's frame differs from one-shot"
            );
        }
    }
}
