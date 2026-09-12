# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- One-shot compression with the fast and double-fast strategies no longer loses matches after the first 16 MiB of input. Their tagged tables now retain full source positions when the input needs them, while smaller inputs keep the existing packed tables and byte-identical output.

## [0.1.4] - 2026-08-31

### Changed

- `StreamingDecoder` is 256 bytes by value instead of 35,520, and `io::Reader` 304 instead of 35,584. A frame's decoding tables now live behind one allocation the decoder holds and reuses across frames rather than inline, so a decoder embeds in an enum, a future, or a small stack without ceremony. Throughput is unchanged: a decoder allocates the tables once however many frames it decodes. Only a decoder discarded after every frame allocates per frame, and that costs about 1% on frames as small as 24 KiB.

### Documentation

- The encode types state their footprint. `io::Writer` and `StreamingEncoder` are ~25 KB by value and `Encoder` ~19 KB, because the Huffman and literals workspaces are inline arrays; each now says so and tells callers to box it when embedding. `tests/footprint.rs` pins the numbers.

## [0.1.3] - 2026-08-28

### Fixed

- A reused `Encoder` could produce different bytes for the same input and options. The row match finder's reset rotated its hash salt instead of restoring it, following C's `ZSTD_advanceHashSalt()`, and the salt feeds every row hash, so a second frame filed the same bytes into different rows. It also kept the previous frame's tables, which C clears on every reset. Frames stayed valid and round-tripped; they were simply not reproducible. Found by the `dictionary_encode_roundtrip` fuzz target.
- A dictionary frame could be encoded that this crate's own decoder refused, with `sequence offset exceeds the available history window`. A dictionary may be referenced in full while its last byte is inside the window, so an offset found then can legally exceed the window; it must not outlive the block that found it. C clamps the carried repeat offsets against each block's window under a guard that reads as "no dictionary" and also catches a dictionary just retired, since it recomputes that per block. This crate skipped the clamp for every parser but one. Needs a window narrower than the body and a dictionary still live in the first block. Found by the `dictionary_encode_roundtrip` fuzz target.
- Changing `min_match` between two encodes on one `Encoder` parsed the second frame at the first one's match length, and the same held for `chain_log` and `search_log`. Reuse was gated on a hand-listed subset of the parameters, so four of the five match finders accepted a state built for different ones. Reuse is now gated on the whole parameter set, which costs an allocation on a frame whose parameters changed and cannot fall out of date as parameters are added.

## [0.1.2] - 2026-08-28

### Fixed

- An overlapping match with an offset of 3, 5, 6 or 7 could decode to the wrong bytes past its first 32. A dedicated short-offset expander stamped a 32-byte pattern buffer out repeatedly, which restarts the period out of phase for the offsets that do not divide 32. Two routes reach it: any match in the final bytes of a `decode_into_slice` destination, and any match that starts in a dictionary and runs on into the frame, so `decode_all_with_dict` and the streaming decoder were affected as well. Found by the `slice_decode` fuzz target.

### Internal

- A `dictionary_decode` fuzz target feeds arbitrary bytes to the dictionary decode paths, which nothing did before, and `dictionary_encode_roundtrip` gains seeds pairing a dictionary that ends in a short period with a body that opens on the same one. That pairing is what reaches the dictionary boundary; without it the defect above survived five days of fuzzing.

## [0.1.1] - 2026-08-27

Documentation only. The library is unchanged from 0.1.0.

- The install snippet names the published crate instead of the git repository.
- The benchmark chart and the README's figures are regenerated and stamped with the revision they were measured at.

## [0.1.0] - 2026-08-27

First release. A Zstandard encoder and decoder in pure Rust, with no C dependency on either path.

Correctness is measured against the reference library rather than asserted. Frames produced here decode with `zstd` and frames produced by `zstd` decode here, and the interop suite holds compressed size to the same standard, comparing exact byte counts against a pinned upstream revision across 11 corpora at every level. Current figures are in [BENCHMARKS.md](BENCHMARKS.md).

### Added

**Encoding and decoding**

- One-shot `encode_all` / `decode_all`, with `_with_options`, `_with_dict`, and `_with_prepared_dict` variants.
- Streaming `StreamingEncoder` and `StreamingDecoder`, with `flush`, `finish`, and `reset`; both contexts are reusable across frames.
- `io::Writer` and `io::Reader` for `std::io` interoperability.
- `encode_into_slice` for encoding into a caller-owned buffer, with `compress_bound` to size it.
- `decode_into_slice` for decoding into one, upstream's `ZSTD_decompress`, with `decompressed_size` and `decompress_bound` to size it. Exactly the decompressed size is enough: the match copy's wildcopy overshoot is handled by a byte-exact tail path rather than by asking the caller for padding, as upstream's `ZSTD_execSequenceEnd` does. A warm `Decoder` writing into a slice makes no allocations at all.
- `StreamingEncoder::read` drains compressed bytes into a caller-owned buffer, the counterpart to `StreamingDecoder::read` and to upstream's `ZSTD_outBuffer`. `pending_output` and `consume_output` do the same without the copy. Unlike `take_output`, which hands over the buffer, these keep its capacity: a warm `io::Writer` or `read` pump makes no allocation per block, which `tests/allocation.rs` measures.
- `RECOMMENDED_INPUT_SIZE` and `RECOMMENDED_OUTPUT_SIZE` on both streaming types, upstream's `ZSTD_CStreamInSize` and its three siblings.
- Compression levels `-131072..=22`. `1..=22` selects progressively more thorough parsers; the negative levels are Zstandard's fast mode.

**Dictionaries**

- Raw-content and formatted dictionaries, on both the encode and decode side.
- `EncoderDictionary` and `DecoderDictionary` parse a dictionary once and reuse it across calls. `new` borrows; `from_shared` takes ownership and yields a `'static`, `Send + Sync` value that can be stored and cloned.
- The two directions are separate types, as upstream separates `ZSTD_CDict` and `ZSTD_DDict`, because each needs different tables built from the same bytes: 11 KiB of encoding tables against 22.5 KiB of decoding tables. Building both over one `Arc<[u8]>` uses a dictionary in both directions while storing its content once.
- `EncoderDictionary::prepare` builds the match-state tables up front instead of inside the first compression that needs them.
- `train_dictionary` builds a dictionary from sample data.

**Tuning**

- `ParameterOverrides` replaces the parameters a level would pick: window, hash and chain sizes, search depth, minimum match, target length, and the parser `Strategy`. Ranges are published as `ParameterBounds` constants, and an out-of-range value is reported rather than clamped.
- Long-distance matching (`long_distance_matching` and the four `ldm_*` table parameters).
- Three-state switches for the row match finder (`use_row_match_finder`) and literal compression (`literal_compression`), each defaulting to the rule the level would have applied.

**Frames**

- Content checksums, content-size declaration, dictionary-ID emission, and a pledged source size for streams.
- Magicless frames, concatenated frames, and skippable frames.
- `parse_frame_header`, `parse_block_header`, and `write_skippable_frame`.
- `find_frame_compressed_size`, `decompress_bound`, and `decompressed_size`, matching upstream's `ZSTD_findFrameCompressedSize`, `ZSTD_decompressBound`, and `ZSTD_findDecompressedSize`. Each walks block headers rather than decoding, and each has a `_with_format` variant. Both size answers come from the input's own headers, so `DecoderOptions::max_output_size` is still what bounds a decode of untrusted input.

**Decoder limits**

- `max_window_size` (128 MiB by default, matching upstream's `ZSTD_WINDOWLOG_LIMIT_DEFAULT`), `max_output_size`, and `single_frame`.

### Fixed before release

- **The hash-chain parsers ignored their search depth.** The chain walk stopped at a 64-byte match, which is the optimal parsers' `sufficient_len` and has no counterpart in upstream's `ZSTD_HcFindBestMatch`. It fired on 40% of searches, capped the parse at short matches where upstream kept walking to matches thousands of bytes long, and because it fired before the depth ran out it made the parse insensitive to `search_log` altogether: forced `Lazy` on 256 KiB gave byte-identical output at search depth 8, 32 and 128 while upstream halved over the same range. Forced `Lazy` and `Lazy2` ran at 1.99x upstream's size and now match it exactly; forced `Greedy` ran at 1.67x and now comes in 29% under.

  Only reachable with `use_row_match_finder` off or a window of `1 << 14` or narrower, so no compression level was affected. It went unnoticed because nothing covered those parsers; they are now in the regression baseline.

- **Greedy parsing in front of a dictionary weighed a repeat match against a search it should not have run.** Upstream takes the repeat at `ip+1` immediately at depth 0 and never searches. Three of the five prefixed parse paths searched anyway and kept whichever match was longer, which is a different choice: a repeat costs one offset code where an explicit offset costs its own bits, so the longer match is often the more expensive sequence. Worst measured was 1.16x upstream's size; the affected paths now come in at or under it.

- **Double-fast parsing alongside a dictionary looked two bytes ahead for a longer match.** Upstream's ladder stops one byte past the short match. The extra rung took a match at `ip+2` whenever it was longer, ignoring that starting two bytes later abandons those bytes to literals and gives up the short match entirely. Worst measured was 1.14x upstream's size across a sweep of every level and dictionary size that reaches this parser; all of it is now at or under upstream, and the total across that sweep moved from 0.1% above to 0.1% below.

### Safety

The crate root is `#![deny(unsafe_code)]`. The 145 individually annotated exceptions in shipping code are confined to the entropy coders, the match finders, the sequence execution loop, and the decoder's output destination, and one more sits in the off-by-default `asm-inspect` maintainer tool; CI counts all 146 and fails if the total grows. The frame, block, and dictionary layers use none.

The decoder is treated as a security boundary: soundness on adversarial input is a bug, not a hardening opportunity. Eight fuzz targets cover both codec paths, the `unsafe` paths run under Miri in CI, and [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) records what is and is not guaranteed.

### Known limitations

- No multithreaded compression and no seekable frame format.
- `ZSTD_c_targetCBlockSize` is not implemented. Both of upstream's block splitters are, and run on upstream's own automatic rules, but neither is settable.
- `LdmMode::Auto` is not wired up: long-distance matching must be asked for explicitly. The rule is implemented and verified against C, but nothing consults it yet.
- Long-distance matching alongside a dictionary is refused rather than encoded without it, since a frame that quietly omits it is indistinguishable from one that used it.
- Streaming output is held to a ratio bound against upstream rather than to byte parity.
- Compressed size still differs from upstream in a handful of measured configurations, listed with their byte counts in the interop suite. Most are in this crate's favour; the largest against it is 1.8%, on one corpus at one level with the row match finder turned off.

### Requirements

Rust 1.96 (2024 edition), `std`. `wasm32-unknown-unknown` is checked in CI. Licensed under `MIT OR Apache-2.0`.

---

Development history from before this release is in [dev/PRERELEASE_LOG.md](dev/PRERELEASE_LOG.md). None of it is release history, since `0.1.0` is the first published version, but it records why much of the code is shaped the way it is.

[0.1.4]: https://github.com/stephenberry/zstandard/releases/tag/v0.1.4
[0.1.3]: https://github.com/stephenberry/zstandard/releases/tag/v0.1.3
[0.1.2]: https://github.com/stephenberry/zstandard/releases/tag/v0.1.2
[0.1.1]: https://github.com/stephenberry/zstandard/releases/tag/v0.1.1
[0.1.0]: https://github.com/stephenberry/zstandard/releases/tag/v0.1.0
