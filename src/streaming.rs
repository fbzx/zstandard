use std::mem;
use std::ops::Range;

use crate::{
    DecoderOptions,
    block::{BLOCK_SIZE_MAX, BlockHeader, BlockType},
    decode::{append_bytes, append_repeated, decode_compressed_block_into},
    decode_out::DecodeOut,
    dictionary::{DecoderDictionary, Dictionary, EncoderDictionary},
    encode::{
        CHECKSUM_SIZE, CompressionParameters, EncoderOptions, EntropyEncodeScratch,
        FRAME_HEADER_MAX, LiteralsEncodingState, UPSTREAM_SPLIT_CHUNK_SIZE,
        compression_parameters_for_options, encode_block_into_contiguous,
        encode_block_into_prefixed_contiguous, seed_optimal_prices_from_first_block,
        upstream_optimal_block_size, validate_options,
    },
    error::{Error, Result},
    frame::{
        FrameHeader, MAX_DECLARABLE_WINDOW_SIZE, ZstandardFrameHeader,
        parse_frame_header_with_format, write_single_segment_header_with_dict,
        write_windowed_header_with_content_size, write_windowed_header_with_dict,
    },
    literals::LiteralsState,
    outbuf::OutBuf,
    sequence::{OutputLimit, RepeatOffsets, SequenceEncodingState, SequenceTablesState},
    window::{ContiguousBlockMatchState, LdmFrameState, PrefixMatchMode, PrefixedBlockMatchState},
    xxhash::Xxh64State,
};

/// Streaming encoder that consumes input in chunks and produces compressed
/// output progressively.
///
/// Call [`push`](Self::push) to feed input and [`take_output`](Self::take_output)
/// to drain produced bytes; finalize the frame with [`finish`](Self::finish),
/// then optionally [`reset`](Self::reset) to begin a new frame on the same
/// context (preserving allocations and dictionary).
///
/// # Footprint
///
/// Roughly 25 KB by value. The bulk is the Huffman compression workspace and the
/// literals encoding state, held as inline arrays so the per-block path reaches
/// them without an indirection and reuses them across blocks and frames. The
/// cost is that the type is unpleasant to embed: it will trip
/// `clippy::large_enum_variant` in an enum, and it enlarges any future holding
/// it across an `await`. Box it when it is not a local. Everything wrapping an
/// encoder inherits this, [`Writer`](crate::io::Writer) included;
/// [`StreamingDecoder`] is a few hundred bytes and needs none of it.
pub struct StreamingEncoder<'a> {
    options: EncoderOptions,
    dictionary: Option<EncoderDictionary<'a>>,
    params: CompressionParameters,
    buffered_input: Vec<u8>,
    literals_state: LiteralsEncodingState,
    repeat_offsets: RepeatOffsets,
    sequence_tables: SequenceEncodingState,
    scratch: EntropyEncodeScratch,
    checksum: Option<Xxh64State>,
    /// Compressed bytes waiting to be drained, as `[ drained | pending ]`.
    ///
    /// Unlike the decoder's, this buffer is *only* a queue: the encoder's match
    /// history lives in `frame`, so a drained byte here is dead and the front
    /// of the buffer can be reclaimed as soon as the caller has taken it.
    output: Vec<u8>,
    /// How much of `output` the caller has taken. Non-zero only between a
    /// partial [`read`](StreamingEncoder::read) or
    /// [`consume_output`](StreamingEncoder::consume_output) and the compaction
    /// that follows it.
    output_pos: usize,
    /// The frame so far as one contiguous buffer: retained history followed by
    /// the block being encoded. Sequences address it by absolute index, which
    /// is what lets `match_state` stay valid from one block to the next.
    ///
    /// This used to be a separate `history` vector with each block handed to
    /// the parser as its own slice. That forced a fresh match finder per
    /// block, re-inserting the entire retained prefix every time: encoding was
    /// quadratic in frame length up to the window size and then linear with a
    /// factor of `window / block_size` on top. At level 15 a 526 KB stream
    /// took 0.438s against 0.011s for the same bytes one-shot. See
    /// `streaming_encode_time_grows_linearly_with_frame_length`.
    frame: Vec<u8>,
    /// Match finder state for the frame in progress, kept across blocks and
    /// rebuilt only when `frame` is compacted.
    match_state: StreamingMatchState,
    /// Long-distance matcher for the frame in progress, present only when the
    /// caller asked for one. Its table addresses `frame` the same way
    /// `match_state` does, and is rebased alongside it.
    ldm: Option<LdmFrameState>,
    /// Bytes saved so far in this frame, which is what the block-split
    /// heuristic gates on. See [`next_block_size`](Self::next_block_size).
    savings: i64,
    /// Bytes pushed into the frame in progress, checked against
    /// [`EncoderOptions::pledged_src_size`] by [`finish`](Self::finish).
    content_len: u64,
    finished: bool,
}

/// Which family of match finder the frame is using.
///
/// This is fixed for the life of a frame: a dictionary sits permanently in
/// front of the frame's own bytes, and the finders that search across that
/// boundary are a different shape from the ones that never have to.
enum StreamingMatchState {
    Contiguous(ContiguousBlockMatchState),
    Prefixed(PrefixedBlockMatchState),
}

fn new_match_state(
    dictionary: Option<&EncoderDictionary<'_>>,
    params: CompressionParameters,
    frame_capacity: usize,
    position_limit: usize,
) -> StreamingMatchState {
    let Some(dictionary) = dictionary else {
        return StreamingMatchState::Contiguous(
            ContiguousBlockMatchState::new_with_position_limit(
                frame_capacity,
                position_limit,
                params.match_finder,
            ),
        );
    };
    let content = dictionary.as_inner().content();
    if content.is_empty() {
        return StreamingMatchState::Contiguous(
            ContiguousBlockMatchState::new_with_position_limit(
                frame_capacity,
                position_limit,
                params.match_finder,
            ),
        );
    }
    StreamingMatchState::Prefixed(
        PrefixedBlockMatchState::new_with_prepared_match_state_and_position_limit(
            content,
            frame_capacity,
            position_limit,
            params.match_finder,
            if dictionary.as_inner().is_raw_content() {
                PrefixMatchMode::ExtDict
            } else {
                PrefixMatchMode::DictMatchState
            },
            dictionary
                .prepared_match_state(params.match_finder)
                .as_deref(),
        ),
    )
}

/// Largest block this frame may emit.
///
/// Deliberately *not* upstream's `zc->blockSizeMax`,
/// `MIN(maxBlockSize, MAX(1, MIN(1 << windowLog, pledgedSrcSize)))`
/// (`zstd_compress.c:2131-2132`), which shrinks a block to the window. Neither
/// encoder here does that; both declare a window wide enough for the blocks
/// they emit instead (see [`StreamingEncoder::frame_window_size`] and
/// `frame_window_size_for`, which carries the reasons).
///
/// The case is routine: a dictionary stream with no pledged size takes its
/// parameters from a source size of "unknown plus the dictionary" and lands on
/// a 16 KiB window against 128 KiB blocks. Capping there produces frames this
/// crate's own decoder rejects, and capping is not what fixes them — the
/// prefixed parsers can emit a match below the source floor they were given
/// whether or not the block is capped. The wide declaration is load-bearing
/// rather than merely conservative. Recorded in `docs/PARITY_PLAN.md`.
/// Largest block this encoder will emit, C's `blockSize = MIN(maxBlockSize,
/// windowSize)` (`zstd_compress.c:2132`).
///
/// Capped at the window for the same reason the one-shot encoder caps it: a
/// block wider than its own window leaves the fast pair floored at the block's
/// start rather than a window back from its end, and the frame would have to
/// declare a window it does not otherwise need. See
/// [`block_size_max_for`](crate::encode::block_size_max_for).
///
/// Unlike the one-shot encoder this cannot bound the window by the content,
/// because a stream does not know its length until `finish`. A pledge is the
/// caller saying it in advance, and is used when given.
fn block_size_for(params: CompressionParameters, options: EncoderOptions) -> usize {
    let window = history_limit_for(params)
        .min(
            options
                .pledged_src_size
                .unwrap_or(u64::MAX)
                .try_into()
                .unwrap_or(usize::MAX),
        )
        .max(1);
    options.block_size.max(1 << 10).min(window)
}

/// Bytes of already-encoded content the parser may still reach back into: the
/// level's full window, bounded by what a decoder will accept declaring. It is
/// *not* on its own the widest offset a block can emit — see
/// [`frame_window_size`](StreamingEncoder::frame_window_size), which is what
/// the frame declares.
///
/// This used to be clamped to `block_size`, which threw away every match
/// beyond a single block.
fn history_limit_for(params: CompressionParameters) -> usize {
    params.max_history_bytes.min(MAX_DECLARABLE_WINDOW_SIZE)
}

/// How much `StreamingEncoder::frame` holds beyond the window before
/// `compact_frame` runs.
///
/// A window's worth is what makes the compaction affordable: one per window of
/// input rather than one per block.
///
/// Widening it is what makes the compaction *effective* for a chain or binary
/// tree, which can only be rebased across a drop that is a whole number of their
/// cycles: a buffer that never has a cycle to give up leaves them nothing but
/// the rebuild, which on a binary tree is not a rebuild at all -- see
/// [`BinaryTreeFinder::shift_positions`]. A cycle wider than the window is
/// ordinary rather than exotic: C only shrinks the chain log to fit the window
/// when the source size is known (`zstd_compress.c:1577-1583`), and a stream by
/// definition does not know it, so level 12 against a window of 19 runs a
/// two-megabyte cycle against half a megabyte of history.
///
/// It widens only over the band where widening is what helps. Below the band the
/// window already holds a cycle. Above it the cycle exceeds the whole buffer, so
/// nothing wraps and [`ContiguousBlockMatchState::shift_positions`] moves the
/// table bodily instead -- and widening there would be unbounded, because it is
/// the cycle that is large. That leaves the buffer at no more than three windows
/// and two blocks, where it was two windows and a block.
fn history_slack_for(params: CompressionParameters, options: EncoderOptions) -> usize {
    let history_limit = history_limit_for(params);
    let period = params.match_finder.rebase_period();
    let unwidened = history_limit
        .saturating_mul(2)
        .saturating_add(block_size_for(params, options));
    if period >= unwidened {
        history_limit
    } else {
        history_limit.max(period)
    }
}

/// The most `StreamingEncoder::frame` ever holds: a full window of history, the
/// slack above, and the block being encoded on top.
fn frame_capacity_for(params: CompressionParameters, options: EncoderOptions) -> usize {
    history_limit_for(params)
        .saturating_add(history_slack_for(params, options))
        .saturating_add(block_size_for(params, options))
}

/// Largest indexed buffer a streaming finder can see before compaction.
///
/// A pledge says the frame ends sooner and keeps its packed tables even when
/// an explicit window would otherwise reserve a wider buffer. Without one the
/// encoder cannot change representations mid-frame, so construction uses the
/// compaction bound from [`frame_capacity_for`].
///
/// [`finish`](StreamingEncoder::finish) is where a pledge is checked, so a
/// caller that pledges short and pushes past it can carry packed tables over
/// the boundary after all. That costs the ratio and nothing else, which is
/// what the truncation cost before any of this: the position a truncated
/// entry names is still one the parser bounds by `window_low` and confirms
/// byte for byte before emitting a sequence, so the blocks already out stay
/// valid -- and `finish` rejects the stream regardless.
fn match_position_limit_for(params: CompressionParameters, options: EncoderOptions) -> usize {
    frame_capacity_for(params, options).min(
        options
            .pledged_src_size
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(usize::MAX),
    )
}

impl StreamingEncoder<'static> {
    /// Construct a dictionary-less streaming encoder with the given options.
    /// Writes the frame header into the output buffer immediately.
    pub fn new(options: EncoderOptions) -> Result<Self> {
        Self::with_prepared_dictionary(None, options)
    }
}

impl<'a> StreamingEncoder<'a> {
    /// Input chunk size that keeps the encoder working on whole blocks,
    /// upstream's `ZSTD_CStreamInSize`.
    ///
    /// Nothing requires it: [`push`](Self::push) buffers whatever it is given
    /// and encodes a block once one is complete. Pushing this much at a time
    /// simply means no byte waits in that buffer for a later call -- except on
    /// a stream whose pledge is its whole block, which holds that block back
    /// for [`finish`](Self::finish) to flag; see [`push`](Self::push).
    pub const RECOMMENDED_INPUT_SIZE: usize = BLOCK_SIZE_MAX;

    /// Output buffer size that can always take one complete block in a single
    /// [`read`](Self::read), upstream's `ZSTD_CStreamOutSize`.
    ///
    /// The worst case it covers is a block that would not compress: the
    /// encoder emits it raw, so the payload is the full block, and the frame
    /// header and the trailing checksum both have to fit alongside it. A
    /// 128 KiB block may also be split into 8 KiB sub-blocks, each paying its
    /// own 3-byte header, which is the middle term. This is
    /// [`compress_bound`](crate::compress_bound) of one block, as a constant.
    ///
    /// A smaller buffer is not an error; `read` just returns short and the
    /// remainder stays queued.
    pub const RECOMMENDED_OUTPUT_SIZE: usize = FRAME_HEADER_MAX
        + (BLOCK_SIZE_MAX / UPSTREAM_SPLIT_CHUNK_SIZE) * BlockHeader::SIZE
        + BLOCK_SIZE_MAX
        + CHECKSUM_SIZE;

    /// Construct a streaming encoder that uses `dict` as a dictionary. The
    /// dictionary bytes are parsed once during construction.
    pub fn with_dict(dict: &'a [u8], options: EncoderOptions) -> Result<Self> {
        let dictionary = EncoderDictionary::new(dict)?;
        Self::with_prepared_dict(&dictionary, options)
    }

    /// Construct a streaming encoder that shares an already-parsed dictionary.
    /// The clone is cheap and reuses any cached parser-built tables.
    pub fn with_prepared_dict(
        dict: &EncoderDictionary<'a>,
        options: EncoderOptions,
    ) -> Result<Self> {
        Self::with_prepared_dictionary(Some(dict.clone()), options)
    }

    fn with_prepared_dictionary(
        dictionary: Option<EncoderDictionary<'a>>,
        options: EncoderOptions,
    ) -> Result<Self> {
        validate_options(options)?;
        let params = compression_parameters_for_options(
            options,
            None,
            dictionary.as_ref().map(EncoderDictionary::as_inner),
        );
        let match_state = new_match_state(
            dictionary.as_ref(),
            params,
            frame_capacity_for(params, options),
            match_position_limit_for(params, options),
        );
        let mut encoder = Self {
            options,
            dictionary,
            params,
            buffered_input: Vec::with_capacity(block_size_for(params, options)),
            literals_state: LiteralsEncodingState::default(),
            repeat_offsets: RepeatOffsets::default(),
            sequence_tables: SequenceEncodingState::default(),
            scratch: EntropyEncodeScratch::default(),
            checksum: None,
            output: Vec::new(),
            output_pos: 0,
            // Grown on demand up to `frame_capacity()`; pre-reserving that
            // would allocate a quarter of a gigabyte at the highest levels
            // before a single byte is pushed.
            frame: Vec::with_capacity(block_size_for(params, options)),
            match_state,
            // Built by `reset_frame_state`, which every frame goes through.
            ldm: None,
            savings: 0,
            content_len: 0,
            finished: false,
        };
        encoder.reset_frame_state()?;
        Ok(encoder)
    }

    /// Append `src` to the input stream. Complete blocks are encoded
    /// immediately; the trailing partial block stays buffered until enough
    /// input arrives or [`flush`](Self::flush) / [`finish`](Self::finish)
    /// runs. Returns an error if called after `finish` without an intervening `reset`.
    ///
    /// One stream produces nothing here at all. The block size is capped at
    /// [`pledged_src_size`](crate::EncoderOptions::pledged_src_size), so a
    /// pledge that fits in one block -- at default parameters, any pledge of
    /// 128 KiB or less -- *is* the block size, and the whole content then waits
    /// for `finish`, so that the single block carries the last-block flag
    /// itself instead of trailing an empty block behind it. A caller pumping
    /// [`read`](Self::read) between pushes sees the frame header and then
    /// nothing until it finishes. This is upstream's behavior under the same
    /// pledge, and it makes the frame identical to
    /// [`encode_all_with_options`](crate::encode_all_with_options)'.
    pub fn push(&mut self, mut src: &[u8]) -> Result<()> {
        if self.finished {
            return Err(Error::InvalidParameter("cannot push after finish"));
        }

        if let Some(checksum) = self.checksum.as_mut() {
            checksum.update(src);
        }
        self.content_len += src.len() as u64;

        // Everything goes through the buffer, even when a caller hands over
        // whole blocks at a time, because where a block *ends* is decided by
        // looking at a full block of input. Encoding straight from `src` would
        // decide that with less to look at than the one-shot encoder has, and
        // on content whose statistics shift mid-block that is worth several
        // times the compressed size.
        //
        // Topping the buffer up to one block at a time rather than absorbing
        // `src` whole is what keeps a single large push from being held in
        // memory in its entirety.
        let target = self.push_target();
        while !src.is_empty() {
            // Held by `encode_buffered_chunk` draining the buffer outright, and
            // by `flush` and `finish` doing the same. `target` is at least 1,
            // so the top-up is too and the loop always advances.
            debug_assert!(self.buffered_input.len() < target);
            let take = (target - self.buffered_input.len()).min(src.len());
            self.buffered_input.extend_from_slice(&src[..take]);
            src = &src[take..];
            if self.buffered_input.len() == target {
                self.encode_buffered_chunk()?;
            }
        }
        Ok(())
    }

    /// How full the buffer must be before `push` encodes it.
    ///
    /// One block, except when the caller pledged exactly one block of
    /// content: then it is one byte more, so the buffer never fills on a
    /// stream that keeps its pledge and the only block goes out from `finish`,
    /// flagged as last. Encoding it from `push` would send it unflagged and
    /// leave `finish` to close the frame with an empty block, three bytes the
    /// one-shot encoder does not spend on the same input.
    ///
    /// Upstream's `ZSTD_CCtx_init_compressStream2` does the same:
    /// `inBuffTarget = blockSizeMax + (blockSizeMax == pledgedSrcSize)`. A stream
    /// that then exceeds its pledge fills the buffer after all and encodes it
    /// as two blocks, which does not matter because `finish` rejects it.
    fn push_target(&self) -> usize {
        let block_size = self.block_size();
        block_size + usize::from(self.options.pledged_src_size == Some(block_size as u64))
    }

    /// Encode one full buffer's worth of input, emptying the buffer.
    ///
    /// The split heuristic runs *once* per full buffer, and whatever it leaves
    /// behind goes out as a single block rather than being carried forward and
    /// split again. That is upstream's `ZSTD_compress_frameChunk` loop, whose
    /// splitter declines outright once fewer than 128 KiB remain in the chunk
    /// it was handed:
    ///
    /// ```c
    /// if (srcSize < 128 KB || blockSizeMax < 128 KB)
    ///     return MIN(srcSize, blockSizeMax);
    /// ```
    ///
    /// In buffered streaming mode upstream hands that loop exactly one
    /// `blockSizeMax` at a time, so a chunk yields at most two blocks. Its
    /// one-shot path hands over the whole input instead, so `srcSize` stays
    /// above the floor and the same corpus is split many times over. The two
    /// layouts are genuinely different, and this is the streaming one.
    ///
    /// Returning the tail to the buffer and topping it back up to a full block
    /// -- which is what this used to do -- reproduces the *one-shot* layout
    /// from a streaming encoder. On `binary-structured` that cost 1.84% at 1
    /// MiB and 2.31% at 4 MiB against upstream's streaming output, on a corpus
    /// where our one-shot encoder is byte-identical to upstream's. Nothing
    /// caught it because every upstream comparison in the tree was one-shot on
    /// both sides; see `streaming_block_layout_matches_upstream`.
    fn encode_buffered_chunk(&mut self) -> Result<()> {
        let n = self.next_block_size();
        if std::env::var_os("ZSTANDARD_TRACE_CHUNK").is_some() {
            eprintln!(
                "chunk: buffered={} split={} savings={}",
                self.buffered_input.len(),
                n,
                self.savings
            );
        }
        self.encode_buffered_block(n, false)?;
        if !self.buffered_input.is_empty() {
            if std::env::var_os("ZSTANDARD_TRACE_CHUNK").is_some() {
                eprintln!("  tail: {}", self.buffered_input.len());
            }
            self.encode_buffered_block(self.buffered_input.len(), false)?;
        }
        Ok(())
    }

    /// How many of the buffered bytes the next block should take.
    ///
    /// The optimal parsers always take a full block. Every other strategy runs
    /// the fingerprinting split heuristic that the one-shot encoder runs, which
    /// ends the block where the content's statistics change rather than at a
    /// fixed size. It reads exactly one block of input, which is what makes it
    /// usable here at all.
    fn next_block_size(&self) -> usize {
        let available = self.buffered_input.len();
        if self.params.upstream_cparams.strategy.is_optimal() {
            return available.min(self.block_size());
        }
        upstream_optimal_block_size(
            &self.buffered_input,
            0,
            self.block_size(),
            self.params.upstream_cparams.strategy,
            self.savings,
        )
        // The heuristic's smallest split is a whole 8 KiB chunk, so this clamp
        // never binds today. It is here because `push` loops until the buffer
        // falls below a block, and resting that loop's termination on a
        // constant in another module is a bad trade for one comparison.
        .clamp(1, available)
    }

    /// Encode any buffered partial block as a non-final block, making its
    /// output bytes available via [`take_output`](Self::take_output). Useful
    /// for forcing progress on slow producers.
    pub fn flush(&mut self) -> Result<()> {
        if self.finished {
            return Err(Error::InvalidParameter("cannot flush after finish"));
        }
        if self.buffered_input.is_empty() {
            return Ok(());
        }

        // A flush is a promise to emit what is buffered now, so the whole
        // buffer goes out as one block regardless of where the split heuristic
        // would have preferred to end it.
        self.encode_buffered_block(self.buffered_input.len(), false)
    }

    /// Emit the final block (marked as last) and, if checksums are enabled,
    /// the four-byte content checksum. After this returns the frame is
    /// complete and `push`/`flush` will fail until [`reset`](Self::reset) runs.
    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }

        // Checked before the last block goes out, so a frame whose header
        // already declared the wrong length is not also completed. Upstream
        // reports the same mismatch as `srcSize_wrong`.
        if let Some(pledged) = self.options.pledged_src_size {
            if pledged != self.content_len {
                return Err(Error::InvalidParameter(
                    "stream carried a different number of bytes than pledged_src_size",
                ));
            }
        }

        // When the buffer is empty this emits a raw block of no bytes whose
        // only content is the last-block flag: three bytes the one-shot
        // encoder does not spend, because it knows which block is last before
        // it writes it. A stream only knows once the caller says so, and by
        // then the final full block has gone out unflagged.
        //
        // Upstream does exactly this, in `ZSTD_writeEpilogue`
        // (`zstd_compress.c:5361`), which writes the same empty block whenever
        // the closing chunk left the context short of `ZSTDcs_ending` -- that
        // is, produced no block of its own to carry the flag. So the cost is
        // the format's rather than this encoder's, and it falls only on a
        // frame whose content is an exact multiple of the block size -- and
        // not even then when the content was pledged as exactly one block,
        // which `push_target` holds back for this call to flag.
        //
        // It is worth naming because it is a floor under every comparison
        // between a streamed frame and a one-shot one: three bytes is 0.2% of
        // a small frame, and a sweep that does not account for it reports a
        // divergence on every block-aligned row. See
        // `streaming_compaction_holds_across_window_and_cycle_geometry`, which
        // sizes its bodies to avoid it.
        self.encode_buffered_block(self.buffered_input.len(), true)?;

        if let Some(checksum) = self.checksum.as_ref() {
            self.output
                .extend_from_slice(&(checksum.digest() as u32).to_le_bytes());
        }

        self.finished = true;
        Ok(())
    }

    /// Begin a fresh frame on the same context, preserving allocations and
    /// the configured dictionary. Must be called only after [`finish`](Self::finish).
    pub fn reset(&mut self) -> Result<()> {
        if !self.finished {
            return Err(Error::InvalidParameter("cannot reset before finish"));
        }
        self.reset_frame_state()
    }

    /// Hand back the bytes produced so far and reset the internal output
    /// buffer to empty. Safe to call at any time; combine with `push` /
    /// `finish` to drain output incrementally.
    ///
    /// This hands over the buffer itself, so the encoder starts the next block
    /// with no output capacity and grows a fresh allocation for it. That is the
    /// right trade when the caller wants to own the `Vec`, and the wrong one
    /// for a pump that drains every block into somewhere else:
    /// [`read`](Self::read) and [`pending_output`](Self::pending_output) reuse
    /// the buffer instead and allocate nothing per block.
    pub fn take_output(&mut self) -> Vec<u8> {
        if self.output_pos == 0 {
            return mem::take(&mut self.output);
        }
        let out = self.output[self.output_pos..].to_vec();
        self.output.clear();
        self.output_pos = 0;
        out
    }

    /// Copy up to `dst.len()` compressed bytes into `dst` and return how many
    /// were copied. Bytes that are read are removed from the internal buffer,
    /// which keeps its capacity for the next block.
    ///
    /// This is the counterpart to
    /// [`StreamingDecoder::read`](crate::StreamingDecoder::read), and the
    /// closest thing here to handing upstream a `ZSTD_outBuffer`: a pump built
    /// on it allocates once for `dst` and never again.
    ///
    /// ```
    /// use zstandard::{EncoderOptions, StreamingEncoder, decode_all};
    ///
    /// let payload = b"streamed into a fixed buffer".repeat(4_000);
    /// let mut encoder = StreamingEncoder::new(EncoderOptions::default())?;
    /// let mut window = vec![0u8; StreamingEncoder::RECOMMENDED_OUTPUT_SIZE];
    /// let mut compressed = Vec::new();
    ///
    /// for chunk in payload.chunks(StreamingEncoder::RECOMMENDED_INPUT_SIZE) {
    ///     encoder.push(chunk)?;
    ///     loop {
    ///         let n = encoder.read(&mut window);
    ///         if n == 0 { break; }
    ///         compressed.extend_from_slice(&window[..n]);
    ///     }
    /// }
    /// encoder.finish()?;
    /// loop {
    ///     let n = encoder.read(&mut window);
    ///     if n == 0 { break; }
    ///     compressed.extend_from_slice(&window[..n]);
    /// }
    ///
    /// assert_eq!(decode_all(&compressed)?, payload);
    /// # Ok::<(), zstandard::Error>(())
    /// ```
    pub fn read(&mut self, dst: &mut [u8]) -> usize {
        let count = dst.len().min(self.pending_output_len());
        if count == 0 {
            return 0;
        }

        dst[..count].copy_from_slice(&self.output[self.output_pos..self.output_pos + count]);
        self.output_pos += count;
        self.compact_output();
        count
    }

    /// Borrow the compressed bytes buffered so far without removing them.
    ///
    /// Pair with [`consume_output`](Self::consume_output) to forward output
    /// somewhere that takes a slice, with no copy in between and no allocation
    /// per block. This is what [`io::Writer`](crate::io::Writer) uses.
    pub fn pending_output(&self) -> &[u8] {
        &self.output[self.output_pos..]
    }

    /// Discard the first `count` bytes of [`pending_output`](Self::pending_output).
    ///
    /// # Panics
    ///
    /// If `count` exceeds [`pending_output_len`](Self::pending_output_len).
    /// Consuming more than was produced is a caller bug, and silently clamping
    /// it would drop compressed bytes and leave a truncated frame that still
    /// parses as a frame.
    pub fn consume_output(&mut self, count: usize) {
        assert!(
            count <= self.pending_output_len(),
            "consumed {count} bytes of pending output but only {} are pending",
            self.pending_output_len()
        );
        self.output_pos += count;
        self.compact_output();
    }

    /// Reclaim the drained prefix of `output`.
    ///
    /// Unlike the decoder's compaction there is no history to preserve here,
    /// so a fully drained buffer is simply cleared and keeps its capacity. A
    /// partially drained one is only shifted when the drained prefix is at
    /// least half of it, which bounds the memmove a caller reading in small
    /// pieces can provoke to amortized O(1) per byte.
    fn compact_output(&mut self) {
        if self.output_pos == 0 {
            return;
        }
        if self.output_pos == self.output.len() {
            self.output.clear();
            self.output_pos = 0;
            return;
        }
        if self.output_pos * 2 >= self.output.len() {
            self.output.drain(..self.output_pos);
            self.output_pos = 0;
        }
    }

    /// Number of compressed bytes currently waiting to be drained by [`take_output`](Self::take_output).
    pub fn pending_output_len(&self) -> usize {
        self.output.len() - self.output_pos
    }

    /// `true` if [`finish`](Self::finish) has been called and no `reset` has
    /// followed it.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Move the first `take` buffered bytes into `frame` and encode them as one
    /// block.
    fn encode_buffered_block(&mut self, take: usize, last_block: bool) -> Result<()> {
        let history_limit = self.history_limit();
        if self.frame.len()
            > history_limit.saturating_add(history_slack_for(self.params, self.options))
        {
            self.compact_frame(history_limit);
        }

        let block_start = self.frame.len();
        self.frame.extend_from_slice(&self.buffered_input[..take]);
        self.buffered_input.drain(..take);
        let block_end = self.frame.len();

        // The parser must not reach further back than the window this frame
        // declared, and between compactions `frame` deliberately holds more
        // than that. Bounding the reach here rather than by the buffer length
        // is what keeps those two lengths independent of each other.
        let mut params = self.params;
        params.max_history_bytes = history_limit;

        // The block that opens the frame is the one btultra2 parses twice, and
        // only a frame with no dictionary in front of it qualifies: a prefixed
        // state has already loaded bytes the seeding pass is required not to
        // have seen.
        if block_start == 0
            && let StreamingMatchState::Contiguous(match_state) = &mut self.match_state
        {
            seed_optimal_prices_from_first_block(
                &mut self.scratch,
                &self.frame[..block_end],
                self.repeat_offsets,
                params,
                match_state,
                self.ldm.as_mut(),
            )?;
        }

        let output_before = self.output.len();
        let result = match &mut self.match_state {
            StreamingMatchState::Contiguous(match_state) => encode_block_into_contiguous(
                &mut OutBuf::growable(&mut self.output),
                &self.frame,
                block_start,
                block_end,
                match_state,
                &mut self.literals_state,
                last_block,
                &mut self.repeat_offsets,
                &mut self.sequence_tables,
                &mut self.scratch,
                params,
                self.ldm.as_mut(),
            ),
            StreamingMatchState::Prefixed(match_state) => encode_block_into_prefixed_contiguous(
                &mut OutBuf::growable(&mut self.output),
                self.dictionary
                    .as_ref()
                    .map_or(&[][..], |dictionary| dictionary.as_inner().content()),
                &self.frame,
                block_start,
                block_end,
                match_state,
                &mut self.literals_state,
                last_block,
                &mut self.repeat_offsets,
                &mut self.sequence_tables,
                &mut self.scratch,
                params,
                self.ldm.as_mut(),
            ),
        };
        result?;

        // What the split heuristic gates on: it stays off until the frame has
        // shown that compression is paying for itself, as in the one-shot loop.
        self.savings += take as i64 - (self.output.len() - output_before) as i64;
        Ok(())
    }

    /// Drop the history that has scrolled out of the window, and re-key the
    /// match state to the shifted buffer.
    ///
    /// Every position a finder holds is an index into `frame`, so moving those
    /// bytes invalidates all of them at once. Where the finder can rebase them
    /// -- subtract the shift from every entry in place, as C's
    /// `ZSTD_reduceIndex` does -- the parser keeps exactly the table it built.
    /// Where it cannot, the state is cleared and rebuilt over what is kept, and
    /// the dense table that produces is a different and worse one than the
    /// parser's own incremental fills: rebuilding rather than rebasing cost
    /// 3.26% at level 1 on `json-records` for as long as it was the only path.
    ///
    /// A contiguous state now always rebases, by one route or the other -- see
    /// [`ContiguousBlockMatchState::shift_positions`], and `history_slack_for`
    /// for how the buffer is sized so that one of them always applies. What is
    /// left on the rebuild path is the prefixed state, which has to become a
    /// contiguous one here regardless.
    ///
    /// Waiting until the buffer holds a window beyond the window is what makes
    /// that affordable: one compaction per window of input rather than one per
    /// *block*, which is what the slack buys in return.
    fn compact_frame(&mut self, history_limit: usize) {
        let live_end = self.frame.len();
        let dropped = self.aligned_drop(live_end - history_limit);
        self.frame.drain(..dropped);
        let retained = self.frame.len();

        // The long-distance table rebases unconditionally, and has no other
        // option: it is filled by hashing forward over each block as it
        // arrives, so there is nothing here that could rebuild it over the
        // retained bytes. See [`LdmState::shift_positions`].
        if let Some(ldm) = self.ldm.as_mut() {
            ldm.shift_positions(dropped);
        }

        // The optimal parser's three-byte table is the third table C reduces in
        // `ZSTD_reduceIndex`, and it addresses `frame` exactly as the other two
        // do. It rebases here rather than with the match state because it is
        // not part of the match state: it lives on the sequence plan, which is
        // why every route below -- rebase, reset in place, rebuild -- had been
        // leaving it keyed to bytes that are no longer there.
        //
        // Unconditional, like the long-distance table above and for the same
        // reason: none of the routes below rebuilds it, so there is no path on
        // which leaving it alone would be repaired later.
        self.scratch.shift_frame_positions(dropped);

        // Rebasing keeps the accumulated state, so it is worth trying before
        // anything below considers throwing that state away. Only the
        // contiguous state can take it: a prefixed one has to drop to
        // contiguous here regardless, for the reason given just below.
        if let StreamingMatchState::Contiguous(match_state) = &mut self.match_state {
            if match_state.shift_positions(dropped, live_end) {
                return;
            }
        }

        // Any dictionary is finished by the time this first runs, and stays
        // finished. It sits immediately before frame position 0, so it leaves
        // the window as soon as a block starts more than `history_limit` bytes
        // in -- and compaction does not happen until twice that. Carrying the
        // prefixed state past this point would keep offering matches against
        // content the decoder can no longer reach.
        let reset_in_place = match &mut self.match_state {
            StreamingMatchState::Contiguous(match_state) => match_state.reset_if_compatible(
                self.params.match_finder,
                match_position_limit_for(self.params, self.options),
            ),
            StreamingMatchState::Prefixed(_) => false,
        };
        if !reset_in_place {
            self.match_state = StreamingMatchState::Contiguous(
                ContiguousBlockMatchState::new_with_position_limit(
                    self.frame_capacity(),
                    match_position_limit_for(self.params, self.options),
                    self.params.match_finder,
                ),
            );
        }

        // Binary trees catch up lazily: the planner inserts everything from
        // `next_to_update` forward, and the rebuild above put that back at the
        // start of the buffer. Inserting here as well would enter every
        // retained position into the tree twice, which corrupts it.
        if self.params.match_finder.parser_strategy.is_binary_tree() {
            return;
        }
        if let StreamingMatchState::Contiguous(match_state) = &mut self.match_state {
            match_state.insert_range(&self.frame, 0, retained);
        }
    }

    /// How much of `frame` to actually drop, given that `want` bytes have
    /// scrolled out of the window.
    ///
    /// Chain and binary-tree finders index a table by `position & mask`, so they
    /// can only rebase across a drop that leaves those low bits alone. That is a
    /// property of the *drop*, not of the finder, so it is decided here. C makes
    /// the same choice at the same point: `ZSTD_window_correctOverflow` composes
    /// its correction from `curr & cycleMask` plus whole cycles precisely so the
    /// tables it is about to reduce stay in their slots.
    ///
    /// Rounding down means keeping a little more than the window, which the
    /// buffer already allows for and the parser already ignores -- its reach is
    /// bounded by `max_history_bytes`, not by how much `frame` happens to hold.
    /// The alternative, rounding up, would drop history the frame's declared
    /// window still promises.
    ///
    /// A cycle wider than `want` is not rounded to, because there is no whole
    /// one here to round to. That is the band `history_slack_for` declines to
    /// widen for, where the cycle exceeds the whole buffer and the table shifts
    /// bodily instead; alignment is not what makes that route work.
    fn aligned_drop(&self, want: usize) -> usize {
        let period = self.params.match_finder.rebase_period();
        debug_assert!(
            match &self.match_state {
                StreamingMatchState::Contiguous(state) => state.rebase_period() == period,
                // A prefixed state is about to become a contiguous one anyway,
                // and rebases nothing on the way.
                StreamingMatchState::Prefixed(_) => true,
            },
            "the buffer was sized around a cycle the finder does not have"
        );
        if period > want {
            return want;
        }
        want - want % period
    }

    fn frame_capacity(&self) -> usize {
        frame_capacity_for(self.params, self.options)
    }

    fn history_limit(&self) -> usize {
        history_limit_for(self.params)
    }

    fn block_size(&self) -> usize {
        block_size_for(self.params, self.options)
    }

    /// `Window_Size` to declare in the frame header.
    ///
    /// The history alone, which is what upstream declares (`windowSize = (U32)1
    /// << params->cParams.windowLog`, `zstd_compress.c:4703`), and which bounds
    /// the widest offset every parser can emit: the lazy, greedy, binary-tree
    /// and optimal families take their floor at the position doing the looking,
    /// and the fast pair's block-constant floor cannot reach past it now that
    /// [`block_size_for`] keeps a block inside its own window.
    ///
    /// That last part is why this no longer takes the block into account. The
    /// format defines `Block_Maximum_Size` as `min(Window_Size, 128 KiB)`, so a
    /// window below the blocks this encoder emits would make its own frames
    /// non-conforming; the blocks are shrunk to fit rather than the window
    /// widened to cover them.
    fn frame_window_size(&self) -> usize {
        self.history_limit()
    }

    fn reset_frame_state(&mut self) -> Result<()> {
        self.buffered_input.clear();
        self.frame.clear();
        self.savings = 0;
        self.match_state = new_match_state(
            self.dictionary.as_ref(),
            self.params,
            self.frame_capacity(),
            match_position_limit_for(self.params, self.options),
        );
        // A new frame starts at its own position zero, so the table cannot be
        // carried over: every entry in it names a byte the new frame's decoder
        // has never seen.
        self.ldm = self.params.ldm.map(|ldm| {
            let mut state = LdmFrameState::new(ldm, self.history_limit());
            // Hashed in before the first block, as `ZSTD_loadDictionaryContent`
            // does. Note this is the *only* place the table learns those bytes:
            // the dictionary is never part of `self.frame`, so no block ever
            // hashes over it, and a compaction that forgets the frame's own
            // positions drops the dictionary's credit with them.
            if let Some(dictionary) = self.dictionary.as_ref() {
                state.load_dictionary(dictionary.as_inner().content());
            }
            state
        });
        self.sequence_tables = self
            .dictionary
            .as_ref()
            .map_or_else(SequenceEncodingState::default, |dictionary| {
                dictionary.as_inner().sequence_encoding_state()
            });
        self.repeat_offsets = self
            .dictionary
            .as_ref()
            .map_or_else(RepeatOffsets::default, |dictionary| {
                dictionary.as_inner().repeat_offsets()
            });
        self.literals_state = LiteralsEncodingState::new(
            self.dictionary.as_ref().map(EncoderDictionary::as_inner),
            self.params,
        );
        self.checksum = self.options.checksum.then(|| Xxh64State::new(0));
        self.scratch.clear_frame_parser_state();
        self.content_len = 0;
        self.finished = false;

        let frame_window_size = self.frame_window_size() as u64;
        let dictionary_id = self
            .options
            .write_dict_id
            .then(|| {
                self.dictionary
                    .as_ref()
                    .and_then(|dictionary| dictionary.as_inner().frame_dictionary_id())
            })
            .flatten();
        let out = &mut OutBuf::growable(&mut self.output);

        // A stream can only declare a content size if it has been told one.
        // Upstream is the same shape: `ZSTD_writeFrameHeader` asserts that the
        // content-size flag and an unknown `pledgedSrcSize` never coincide
        // (`zstd_compress.c:4711`), and its own streaming path clears the flag
        // when no pledge was made (`:2198`).
        match self
            .options
            .pledged_src_size
            .filter(|_| self.options.write_content_size)
        {
            Some(pledged) if pledged <= frame_window_size => {
                write_single_segment_header_with_dict(
                    out,
                    pledged,
                    self.options.checksum,
                    dictionary_id,
                    self.options.format,
                );
            }
            Some(pledged) => write_windowed_header_with_content_size(
                out,
                frame_window_size,
                pledged,
                self.options.checksum,
                dictionary_id,
                self.options.format,
            )?,
            None => write_windowed_header_with_dict(
                out,
                frame_window_size,
                self.options.checksum,
                dictionary_id,
                self.options.format,
            )?,
        }
        Ok(())
    }
}

impl Default for StreamingEncoder<'static> {
    fn default() -> Self {
        Self::new(EncoderOptions::default()).expect("default encoder options must be valid")
    }
}

/// Streaming decoder that consumes compressed input in chunks and produces
/// decoded output progressively.
///
/// Call [`push`](Self::push) to feed compressed bytes and
/// [`take_output`](Self::take_output) (or [`read`](Self::read)) to drain
/// decoded bytes. Call [`finish`](Self::finish) once the input stream ends to
/// validate frame trailers; a context can be re-used for another stream via
/// [`reset`](Self::reset).
///
/// # Footprint
///
/// 256 bytes by value, so it embeds without ceremony. A frame's FSE and Huffman
/// decoding tables are ~35 KB, but they live behind one allocation the decoder
/// holds and reuses from frame to frame rather than inline. A decoder decoding
/// many frames allocates them once; only one thrown away after every frame pays
/// per frame, and that at about 1% on frames as small as 24 KiB.
pub struct StreamingDecoder<'a> {
    options: DecoderOptions,
    dictionary: Option<Dictionary<'a>>,
    input: Vec<u8>,
    input_pos: usize,
    /// Decoded bytes, as `[ released .. | drained | pending ]`.
    ///
    /// This is both the caller's output queue and the frame's match history, so
    /// a byte stays here after the caller has drained it, until it also falls
    /// out of match range. Keeping the two in one buffer is what lets a block
    /// decode with the same executor the one-shot decoder uses: a match is a
    /// copy from earlier in this very `Vec`, not a lookup in a side structure.
    output: Vec<u8>,
    /// How much of `output` the caller has taken.
    output_pos: usize,
    /// Scratch for the literals section, reused across blocks.
    literals_scratch: Vec<u8>,
    state: DecoderState,
    /// Boxed rather than inline: this is ~35 KB of entropy tables and was the
    /// whole weight of the decoder, which by value made `StreamingDecoder`
    /// 35,520 bytes rather than 256. The box is not freed between frames; see
    /// `spare_frame`.
    current_frame: Option<Box<FrameDecodeState>>,
    /// The finished frame's box, kept for the next frame to assign into.
    ///
    /// Without it every frame in a stream would allocate and free ~35 KB of its
    /// own, which is the entire cost of boxing `current_frame` and is what made
    /// small frames measure slower than they did when the tables were inline.
    /// With it a decoder allocates them once however many frames it decodes, and
    /// 24 KiB frames on a `reset` decoder run at the speed they did before the
    /// boxing. A decoder discarded after each frame is the one shape this cannot
    /// help, and it is the shape `reset` exists to avoid.
    ///
    /// Only ever an allocation, never state: the next frame overwrites it whole
    /// before reading a byte of it. The cost is that a decoder parked between
    /// streams holds 35 KB it is not using; freeing it would mean allocating
    /// again on the next frame, which is the thing this exists to avoid.
    spare_frame: Option<Box<FrameDecodeState>>,
    total_output_size: u64,
    received_input: bool,
    finished_input: bool,
    /// Bytes dropped off the front of `input` by compaction. Added to
    /// `input_pos` to recover a position in the pushed stream.
    input_dropped: usize,
    /// A Zstandard frame has completed. Only consulted under
    /// `DecoderOptions::single_frame`, where a second one is an error.
    decoded_a_frame: bool,
}

impl StreamingDecoder<'static> {
    /// Construct a dictionary-less streaming decoder with the given options.
    pub fn new(options: DecoderOptions) -> Self {
        Self::with_parsed_dictionary(None, options)
    }
}

impl Default for StreamingDecoder<'static> {
    fn default() -> Self {
        Self::new(DecoderOptions::default())
    }
}

impl<'a> StreamingDecoder<'a> {
    /// Input chunk size that always carries at least one whole block,
    /// upstream's `ZSTD_DStreamInSize`: a maximal block plus its header.
    ///
    /// Pushing less is fine and pushing more is fine; this is the size at
    /// which every `push` can complete a block rather than leaving a partial
    /// one buffered.
    pub const RECOMMENDED_INPUT_SIZE: usize = BLOCK_SIZE_MAX + BlockHeader::SIZE;

    /// Output buffer size that can always take one whole block in a single
    /// [`read`](Self::read), upstream's `ZSTD_DStreamOutSize`.
    ///
    /// A block never decodes to more than this, so a buffer of this size
    /// cannot be the thing that stops the decoder making progress.
    pub const RECOMMENDED_OUTPUT_SIZE: usize = BLOCK_SIZE_MAX;

    /// Construct a streaming decoder that uses `dict` as a dictionary. The
    /// dictionary bytes are parsed once during construction.
    pub fn with_dict(dict: &'a [u8], options: DecoderOptions) -> Result<Self> {
        let dictionary = DecoderDictionary::new(dict)?;
        Ok(Self::with_prepared_dict(&dictionary, options))
    }

    /// Construct a streaming decoder that shares an already-parsed dictionary.
    pub fn with_prepared_dict(dict: &DecoderDictionary<'a>, options: DecoderOptions) -> Self {
        Self::with_parsed_dictionary(Some(dict.as_inner().clone()), options)
    }

    /// Feed `src` to the decoder. Bytes are decoded eagerly when enough input
    /// is buffered to advance the state machine; partial blocks remain
    /// buffered until more input arrives.
    pub fn push(&mut self, src: &[u8]) -> Result<()> {
        if self.finished_input {
            return Err(Error::InvalidParameter("cannot push after finish"));
        }

        self.received_input |= !src.is_empty();
        self.input.extend_from_slice(src);
        self.process()
    }

    /// Mark the input stream complete. Returns an error if the buffered
    /// input does not end on a frame boundary or no input was ever pushed.
    pub fn finish(&mut self) -> Result<()> {
        if self.finished_input {
            return self.finish_status();
        }

        self.finished_input = true;
        self.process()?;
        self.finish_status()
    }

    /// Reset to the initial state so the context can decode another stream,
    /// preserving the input and output buffers, the frame decode tables'
    /// allocation, and the configured dictionary. Must follow a successful
    /// [`finish`](Self::finish).
    pub fn reset(&mut self) -> Result<()> {
        self.finish_status()?;
        self.reset_state();
        Ok(())
    }

    /// Copy up to `dst.len()` decoded bytes into `dst` and return how many
    /// were copied. Bytes that are read are removed from the internal buffer.
    pub fn read(&mut self, dst: &mut [u8]) -> usize {
        let count = dst.len().min(self.pending_output_len());
        if count == 0 {
            return 0;
        }

        dst[..count].copy_from_slice(&self.output[self.output_pos..self.output_pos + count]);
        self.output_pos += count;
        self.compact_output();
        count
    }

    /// Hand back all currently buffered decoded bytes.
    ///
    /// Mid-frame the bytes are copied out rather than the buffer being handed
    /// over, because that buffer is also the match history the next block reads
    /// from. What the copy costs is partly repaid by the buffer keeping its
    /// capacity instead of a fresh one being grown for every drain.
    ///
    /// Between frames there is no history to keep and the buffer itself is
    /// handed over, so the common shape of pushing a whole stream and taking
    /// the result once still does not copy.
    pub fn take_output(&mut self) -> Vec<u8> {
        if self.output_pos == 0 && self.current_frame.is_none() {
            return mem::take(&mut self.output);
        }
        let out = self.output[self.output_pos..].to_vec();
        self.output_pos = self.output.len();
        self.compact_output();
        out
    }

    /// Number of decoded bytes currently available to drain via
    /// [`take_output`](Self::take_output) or [`read`](Self::read).
    pub fn pending_output_len(&self) -> usize {
        self.output.len().saturating_sub(self.output_pos)
    }

    /// Compressed bytes that have been pushed but not yet consumed.
    ///
    /// **Only meaningful with
    /// [`DecoderOptions::single_frame`](crate::DecoderOptions::single_frame)
    /// set.** Without it the decoder does not stop at a frame boundary — it
    /// reads straight on into whatever follows, treating a second frame as
    /// more of the same stream — so by the time control returns, trailing
    /// bytes have usually been consumed or rejected as a bad frame header, and
    /// an empty result here does not mean the input was one frame. Under
    /// `single_frame` the decoder stops at the boundary and this is exactly
    /// what followed it.
    ///
    /// Mid-frame it is a partial block awaiting more input and says nothing.
    ///
    /// Note this reports what the *decoder* holds, not what the caller's
    /// source holds. A reader that pulls fixed-size chunks may have taken more
    /// from its source than the frame needed; those bytes are here.
    pub fn unconsumed_input(&self) -> &[u8] {
        &self.input[self.input_pos..]
    }

    /// Total bytes consumed from the pushed stream so far.
    ///
    /// **Only a frame length with
    /// [`DecoderOptions::single_frame`](crate::DecoderOptions::single_frame)
    /// set**, and for the same reason as
    /// [`unconsumed_input`](Self::unconsumed_input): the permissive default
    /// keeps consuming past the first frame, so this becomes the length of
    /// everything decoded rather than of the frame. Under `single_frame` it is
    /// the first frame's exact compressed length, which is the number a caller
    /// needs to advance its own cursor when the frame is embedded in a larger
    /// buffer.
    pub fn input_consumed(&self) -> usize {
        self.stream_input_pos()
    }

    /// Decoded bytes the decoder is holding: the caller's pending output plus
    /// the match history retained behind it.
    ///
    /// Exposed for the tests that assert compaction both fires and stops short
    /// of the history a match still needs, neither of which is observable from
    /// the decoded bytes alone — dropping history too eagerly produces a decode
    /// *error*, but never dropping it at all produces correct output out of a
    /// buffer that grows without bound.
    #[cfg(any(test, feature = "internal-trace"))]
    #[doc(hidden)]
    pub fn retained_output_len(&self) -> usize {
        self.output.len()
    }

    /// `true` once [`finish`](Self::finish) has run and all input has been
    /// consumed at a frame boundary.
    pub fn is_finished(&self) -> bool {
        self.finished_input
            && matches!(self.state, DecoderState::FrameHeader)
            && self.current_frame.is_none()
            && self.input_pos == self.input.len()
    }

    fn with_parsed_dictionary(dictionary: Option<Dictionary<'a>>, options: DecoderOptions) -> Self {
        let mut decoder = Self {
            options,
            dictionary,
            input: Vec::new(),
            input_pos: 0,
            output: Vec::new(),
            output_pos: 0,
            literals_scratch: Vec::new(),
            state: DecoderState::FrameHeader,
            current_frame: None,
            spare_frame: None,
            total_output_size: 0,
            received_input: false,
            finished_input: false,
            input_dropped: 0,
            decoded_a_frame: false,
        };
        decoder.reset_state();
        decoder
    }

    fn process(&mut self) -> Result<()> {
        loop {
            let progress = match self.state {
                DecoderState::FrameHeader => self.process_frame_header()?,
                DecoderState::Skippable { .. } => self.process_skippable_frame(),
                DecoderState::BlockHeader => self.process_block_header()?,
                DecoderState::BlockPayload { .. } => self.process_block_payload()?,
                DecoderState::FrameChecksum => self.process_frame_checksum()?,
            };

            if !progress {
                break;
            }

            self.compact_input();
        }

        Ok(())
    }

    fn process_frame_header(&mut self) -> Result<bool> {
        if self.input_pos == self.input.len() {
            return Ok(false);
        }

        // Reaching a frame header for the second time under `single_frame`
        // means a frame already completed and the input did not stop there.
        // Raised here rather than at `finish`, so a caller streaming a large
        // second frame is told before decoding it.
        if self.options.single_frame && self.decoded_a_frame {
            return Err(Error::TrailingInput {
                offset: self.stream_input_pos(),
            });
        }

        match parse_frame_header_with_format(&self.input[self.input_pos..], self.options.format) {
            Ok(FrameHeader::Skippable(skippable)) => {
                if self.options.single_frame {
                    return Err(Error::TrailingInput {
                        offset: self.stream_input_pos(),
                    });
                }
                self.input_pos += skippable.header_size;
                self.state = DecoderState::Skippable {
                    remaining: skippable.size as usize,
                };
                Ok(true)
            }
            Ok(FrameHeader::Zstandard(header)) => {
                self.input_pos += header.header_size;
                self.start_frame(header)?;
                self.state = DecoderState::BlockHeader;
                Ok(true)
            }
            Err(Error::UnexpectedEof) => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn process_skippable_frame(&mut self) -> bool {
        let DecoderState::Skippable { remaining } = &mut self.state else {
            unreachable!();
        };

        let available = self.input.len() - self.input_pos;
        let consumed = available.min(*remaining);
        self.input_pos += consumed;
        *remaining -= consumed;
        if *remaining == 0 {
            self.state = DecoderState::FrameHeader;
        }
        consumed != 0
    }

    fn process_block_header(&mut self) -> Result<bool> {
        if self.input.len() - self.input_pos < BlockHeader::SIZE {
            return Ok(false);
        }

        let header =
            BlockHeader::parse(&self.input[self.input_pos..self.input_pos + BlockHeader::SIZE])?;
        self.input_pos += BlockHeader::SIZE;
        self.state = DecoderState::BlockPayload { header };
        Ok(true)
    }

    fn process_block_payload(&mut self) -> Result<bool> {
        let DecoderState::BlockPayload { header } = self.state else {
            unreachable!();
        };
        let payload_size = header.payload_size();
        if self.input.len() - self.input_pos < payload_size {
            self.state = DecoderState::BlockPayload { header };
            return Ok(false);
        }

        let payload = self.input_pos..self.input_pos + payload_size;
        self.input_pos += payload_size;
        self.decode_block(header, payload)?;

        let checksum = self
            .current_frame
            .as_ref()
            .expect("compressed block requires an active frame")
            .header
            .checksum;
        self.state = if header.last_block {
            if checksum {
                DecoderState::FrameChecksum
            } else {
                self.finish_frame()?;
                DecoderState::FrameHeader
            }
        } else {
            DecoderState::BlockHeader
        };
        Ok(true)
    }

    fn process_frame_checksum(&mut self) -> Result<bool> {
        if self.input.len() - self.input_pos < 4 {
            return Ok(false);
        }

        let expected = u32::from_le_bytes([
            self.input[self.input_pos],
            self.input[self.input_pos + 1],
            self.input[self.input_pos + 2],
            self.input[self.input_pos + 3],
        ]);
        self.input_pos += 4;

        let frame = self.current_frame.as_ref().ok_or(Error::Generic)?;
        if self.options.verify_checksum {
            let actual = frame
                .checksum
                .as_ref()
                .ok_or(Error::Corruption("missing frame checksum state"))?
                .digest() as u32;
            if actual != expected {
                return Err(Error::ChecksumMismatch { expected, actual });
            }
        }

        self.finish_frame()?;
        self.state = DecoderState::FrameHeader;
        Ok(true)
    }

    fn start_frame(&mut self, header: ZstandardFrameHeader) -> Result<()> {
        let dictionary = match (self.dictionary.as_ref(), header.dictionary_id) {
            (None, Some(dictionary_id)) => {
                return Err(Error::DictionaryRequired(Some(dictionary_id)));
            }
            (Some(dictionary), Some(dictionary_id)) if dictionary.id() != dictionary_id => {
                return Err(Error::DictionaryMismatch {
                    expected: dictionary_id,
                    actual: dictionary.id(),
                });
            }
            (dictionary, _) => dictionary,
        };

        if let Some(max_window_size) = self.options.max_window_size {
            if header.window_size > max_window_size {
                return Err(Error::WindowSizeTooLarge {
                    window_size: header.window_size,
                    max_window_size,
                });
            }
        }
        if let Some(content_size) = header.content_size {
            ensure_total_size_limit(
                self.total_output_size,
                content_size,
                self.options.max_output_size,
            )?;
        }

        let checksum = header.checksum;
        let window_size = usize::try_from(header.window_size).unwrap_or(usize::MAX);
        let frame = FrameDecodeState {
            literals_state: dictionary
                .map_or_else(LiteralsState::default, Dictionary::literals_state),
            sequence_tables: dictionary
                .map_or_else(SequenceTablesState::default, Dictionary::sequence_tables),
            repeat_offsets: dictionary
                .map_or_else(RepeatOffsets::default, Dictionary::repeat_offsets),
            checksum: checksum.then(|| Xxh64State::new(0)),
            frame_output_size: 0,
            window_size,
            // Whatever an earlier frame left undrained stays in the buffer, so
            // this frame starts after it rather than at zero.
            frame_start: self.output.len(),
            header,
        };
        // Into the previous frame's box where there is one. Assigning the whole
        // struct is what makes this safe, and it must stay that way: `frame`
        // above is the same initializer a fresh `Box::new` would take, the
        // compiler requires every field in it, and the assignment overwrites
        // every field. Only the allocation is reused.
        //
        // Do not "optimize" this into a field-by-field reinitialization that
        // skips the entropy tables. It would appear to work: a stale
        // `sequence_tables` or `literals_state` is invisible until a frame opens
        // a section in `Repeat` mode against it, and frames from this crate's
        // encoder do not, so the whole test suite passes with either one
        // deliberately carried over -- verified by injecting exactly that. The
        // reader that would catch it is someone else's encoder, in the field,
        // producing silently wrong bytes. Only `repeat_offsets` is caught by
        // tests, in `a_recycled_decoder_decodes_every_frame_like_a_fresh_one`.
        self.current_frame = Some(match self.spare_frame.take() {
            Some(mut recycled) => {
                *recycled = frame;
                recycled
            }
            None => Box::new(frame),
        });
        Ok(())
    }

    fn decode_block(
        &mut self,
        block_header: BlockHeader,
        payload_range: Range<usize>,
    ) -> Result<()> {
        let block_start = self.output.len();
        // The executors measure the output-size cap from `out.len()`, but this
        // buffer holds only the tail of the stream. The difference is what the
        // stream has produced and already released.
        //
        // It can only saturate after a `reset` that left output undrained,
        // because that restarts `total_output_size` while those bytes stay in
        // the buffer. Saturating there charges the leftovers to the new
        // stream's budget, which errs towards the tighter cap — the safe
        // direction for a limit whose job is to bound untrusted expansion.
        let released = self.total_output_size.saturating_sub(block_start as u64);
        let limit = OutputLimit::after(released, self.options.max_output_size);

        // Borrowed straight out of the input buffer. Copying it to a `Vec`
        // first, as this did, cost an allocation and a memcpy of the whole
        // compressed block on every block; the fields read below are disjoint
        // from `input`, which is all the borrow checker needed to be told.
        let payload = &self.input[payload_range];
        // Borrowed from `self.dictionary` rather than cached in the frame
        // state: a dictionary that owns its bytes cannot hand out a slice that
        // outlives the borrow it came from, and the frame state is the only
        // thing that ever wanted one. Disjoint fields, so this coexists with
        // the mutable borrow below.
        let dictionary_content = self.dictionary.as_ref().map(Dictionary::content);
        let frame = self.current_frame.as_mut().ok_or(Error::Generic)?;
        let oversized = block_header.block_size > frame.header.block_size_max;

        match block_header.block_type {
            BlockType::Raw => {
                if oversized {
                    return Err(Error::Corruption(
                        "block size exceeds frame block size limit",
                    ));
                }
                append_bytes(&mut DecodeOut::growable(&mut self.output), payload, limit)?
            }
            BlockType::Rle => {
                if oversized {
                    return Err(Error::Corruption(
                        "block size exceeds frame block size limit",
                    ));
                }
                append_repeated(
                    &mut DecodeOut::growable(&mut self.output),
                    *payload.first().ok_or(Error::UnexpectedEof)?,
                    block_header.block_size as usize,
                    limit,
                )?
            }
            BlockType::Compressed => decode_compressed_block_into(
                payload,
                frame.header.block_size_max as usize,
                frame.frame_start,
                frame.window_size,
                dictionary_content,
                &mut frame.literals_state,
                &mut frame.sequence_tables,
                &mut frame.repeat_offsets,
                &mut self.literals_scratch,
                &mut DecodeOut::growable(&mut self.output),
                limit,
            )?,
        }

        let produced = self.output.len() - block_start;
        frame.absorb_block_output(&self.output[block_start..]);
        self.total_output_size += produced as u64;
        Ok(())
    }

    fn finish_frame(&mut self) -> Result<()> {
        let frame = self.current_frame.take().ok_or(Error::Generic)?;
        if let Some(expected_size) = frame.header.content_size {
            if frame.frame_output_size != expected_size {
                return Err(Error::ContentSizeMismatch {
                    expected: expected_size,
                    actual: frame.frame_output_size,
                });
            }
        }
        self.decoded_a_frame = true;
        self.spare_frame = Some(frame);
        Ok(())
    }

    fn finish_status(&self) -> Result<()> {
        if !self.received_input {
            return Err(Error::UnexpectedEof);
        }
        if self.current_frame.is_some() || !matches!(self.state, DecoderState::FrameHeader) {
            return Err(Error::UnexpectedEof);
        }
        if self.input_pos != self.input.len() {
            return Err(Error::UnexpectedEof);
        }
        Ok(())
    }

    fn compact_input(&mut self) {
        if self.input_pos == 0 {
            return;
        }
        if self.input_pos == self.input.len() {
            self.input_dropped += self.input_pos;
            self.input.clear();
            self.input_pos = 0;
            return;
        }
        if self.input_pos >= 4096 || self.input_pos * 2 >= self.input.len() {
            self.input_dropped += self.input_pos;
            self.input.drain(..self.input_pos);
            self.input_pos = 0;
        }
    }

    /// Offset of the decoder's read cursor within the whole pushed stream.
    ///
    /// `input_pos` alone cannot answer this: `compact_input` drops consumed
    /// bytes off the front of the buffer and rewinds it, so it is a position
    /// in the buffer, not in the stream. Errors have to name the second.
    fn stream_input_pos(&self) -> usize {
        self.input_dropped + self.input_pos
    }

    /// Drop the front of `output` once it is neither owed to the caller nor
    /// reachable by a match.
    ///
    /// A drained byte cannot simply be discarded: it is still this frame's
    /// match history. What may go is the prefix that is both drained and far
    /// enough behind the write head, and `retain_for_matches` is what says how
    /// far that is.
    ///
    /// Compaction waits until the droppable prefix is at least half the buffer.
    /// That bounds the memmove: each one moves at most what it leaves behind,
    /// so a byte is moved at most once on average no matter how the caller
    /// paces its reads. The cost of waiting is that the buffer settles at twice
    /// the retained history rather than at exactly it.
    fn compact_output(&mut self) {
        let retain = self.retain_for_matches();
        let droppable = self
            .output_pos
            .min(self.output.len().saturating_sub(retain));
        if droppable == 0 {
            return;
        }
        if droppable == self.output.len() {
            self.output.clear();
            self.output_pos = 0;
            self.release_history(droppable);
            return;
        }
        if droppable * 2 >= self.output.len() {
            self.output.drain(..droppable);
            self.output_pos -= droppable;
            self.release_history(droppable);
        }
    }

    /// Bytes at the end of `output` that a later match may still read.
    ///
    /// One past the window rather than exactly the window, and the extra byte
    /// is load-bearing. The executor reads `out_pos - frame_start` as the
    /// frame's output so far, and decides two things with it. A match is served
    /// from the buffer when `offset <= produced_in_frame` and
    /// `offset <= window_size`, for which a window's worth is enough. But the
    /// same quantity also decides whether the frame has outrun its dictionary,
    /// which stops being reachable once the frame has produced *more* than a
    /// window. Retaining exactly a window leaves that second comparison reading
    /// equal, so a frame that outran its dictionary long ago would go on
    /// matching against it. One more byte makes it come out strictly greater,
    /// which is the truth.
    ///
    /// Zero between frames, where nothing can match backwards at all.
    fn retain_for_matches(&self) -> usize {
        self.current_frame
            .as_ref()
            .map_or(0, |frame| frame.window_size.saturating_add(1))
    }

    /// Move the current frame's start down by the `dropped` bytes compaction
    /// removed, so it keeps pointing at the same byte.
    fn release_history(&mut self, dropped: usize) {
        let retained = self.output.len();
        if let Some(frame) = self.current_frame.as_mut() {
            frame.frame_start = frame.frame_start.saturating_sub(dropped);
            // Checked first so the subtraction below cannot underflow, and
            // worth stating on its own: the frame's start is a position in
            // `output`, and compaction moving it past the end would mean it had
            // dropped bytes this frame had not produced yet.
            debug_assert!(
                frame.frame_start <= retained,
                "compaction left the frame starting past the end of the buffer"
            );
            // `retained - frame_start` is what the executor reads as the
            // frame's output so far, and it decides two different things:
            // whether a match source is still in the buffer, and whether the
            // frame has outrun its dictionary.
            //
            // It is a safe stand-in for the true count in exactly two states.
            // Either nothing of *this* frame has been dropped, in which case it
            // is not a stand-in but the count itself — which is the usual case,
            // since compaction reaches the current frame only after eating
            // whatever an earlier one left behind. Or it is more than a window,
            // which is enough for both readings: every legal offset fits inside
            // it, and the dictionary comparison is strict, so it correctly
            // reports a frame that has outrun its window. What must never
            // happen is an undercount landing at or below the window, because
            // that reads as "still within the window" for a frame that left it
            // behind, and its matches would go on resolving against a
            // dictionary no longer reachable.
            let held = (retained - frame.frame_start) as u64;
            debug_assert!(
                held == frame.frame_output_size || held > frame.window_size as u64,
                "compaction left {held} bytes of a {}-byte frame, inside its {}-byte window",
                frame.frame_output_size,
                frame.window_size
            );
        }
    }

    fn reset_state(&mut self) {
        self.input.clear();
        self.input_pos = 0;
        self.state = DecoderState::FrameHeader;
        // Kept rather than dropped, so a reset decoder starts its next frame
        // without allocating. An abandoned frame's box is as reusable as a
        // completed one's: the next frame overwrites it whole.
        self.spare_frame = self.current_frame.take().or(self.spare_frame.take());
        self.total_output_size = 0;
        self.received_input = false;
        self.finished_input = false;
        self.input_dropped = 0;
        self.decoded_a_frame = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderState {
    FrameHeader,
    Skippable { remaining: usize },
    BlockHeader,
    BlockPayload { header: BlockHeader },
    FrameChecksum,
}

struct FrameDecodeState {
    header: ZstandardFrameHeader,
    literals_state: LiteralsState,
    sequence_tables: SequenceTablesState,
    repeat_offsets: RepeatOffsets,
    checksum: Option<Xxh64State>,
    frame_output_size: u64,
    window_size: usize,
    /// Index in the decoder's `output` where this frame's output begins.
    ///
    /// A match may not reach behind it into an earlier frame, which is the same
    /// bound the one-shot decoder applies. Compaction moves it down as it drops
    /// bytes off the front.
    frame_start: usize,
}

impl FrameDecodeState {
    /// Account for a block's output once it has been appended to `output`.
    ///
    /// The checksum runs over the block in one pass here rather than per
    /// literal run and per match inside the executor, which is both a great
    /// deal less call overhead and a single sequential read of memory that is
    /// still in cache from having just been written.
    fn absorb_block_output(&mut self, produced: &[u8]) {
        if let Some(checksum) = self.checksum.as_mut() {
            checksum.update(produced);
        }
        self.frame_output_size += produced.len() as u64;
    }
}

fn ensure_total_size_limit(
    current_output_size: u64,
    content_size: u64,
    max_output_size: Option<usize>,
) -> Result<()> {
    if let Some(limit) = max_output_size {
        let total = current_output_size
            .checked_add(content_size)
            .ok_or(Error::OutputSizeOverflow)?;
        if total > limit as u64 {
            return Err(Error::OutputSizeTooLarge {
                output_size: total,
                max_output_size: limit,
            });
        }
    }
    Ok(())
}

impl std::fmt::Debug for StreamingEncoder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingEncoder")
            .field("compression_level", &self.options.compression_level)
            .field("block_size", &self.options.block_size)
            .field("has_dictionary", &self.dictionary.is_some())
            .field("buffered_input_len", &self.buffered_input.len())
            // Not `self.output.len()`: after a partial `read` that still counts
            // the drained prefix, which is exactly the number a caller reading
            // this to decide whether to drain again must not be given.
            .field("pending_output_len", &self.pending_output_len())
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for StreamingDecoder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingDecoder")
            .field("pending_output_len", &self.pending_output_len())
            .field("finished", &self.is_finished())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CompressionLevel;

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Content with matches at several distances, so a parser that loses its
    /// history shows up as a worse ratio rather than as identical output.
    fn repetitive_body(len: usize) -> Vec<u8> {
        const LINES: [&str; 5] = [
            "the quick brown fox jumps over the lazy dog\n",
            "pack my box with five dozen liquor jugs\n",
            "how vexingly quick daft zebras jump\n",
            "sphinx of black quartz judge my vow\n",
            "the five boxing wizards jump quickly\n",
        ];
        let mut out = Vec::with_capacity(len);
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut n = 0usize;
        while out.len() < len {
            out.extend_from_slice(LINES[n % LINES.len()].as_bytes());
            // Enough noise that the window genuinely has to hold distinct
            // content, not so much that nothing matches.
            if n.is_multiple_of(11) {
                for _ in 0..24 {
                    out.push((xorshift(&mut state) & 0xff) as u8);
                }
            }
            n += 1;
        }
        out.truncate(len);
        out
    }

    /// Streaming decode keeps its history in the same buffer it hands the
    /// caller from, so it has to drop the front of that buffer as the caller
    /// drains it -- but only as far back as a match can still reach.
    ///
    /// Both mistakes are quiet in their own way. Dropping too much shows up as
    /// a decode error rather than as wrong bytes, because a match reaching past
    /// the buffer is rejected, so a plain round-trip assertion would catch it.
    /// Dropping too little is invisible from the output: every byte is correct
    /// and the buffer simply grows for the length of the stream. Only the
    /// second needs a probe, which is what `retained_output_len` is for.
    ///
    /// What varies is how often the caller reads, not which parser wrote the
    /// frame: compaction is driven by what the caller has drained, and the
    /// decoder is indifferent to how the matches it is replaying were chosen. A
    /// caller that reads rarely leaves bytes undroppable for longer, which is
    /// the case where a bound taken on the wrong side of `output_pos` would
    /// show up.
    #[test]
    fn streaming_decode_drops_history_but_never_what_a_match_needs() {
        // Level 1 for its narrow window: the body has to be several windows
        // long before any of it is droppable, and the higher levels declare
        // windows wider than a test-sized body.
        let body = repetitive_body(4_000_000);
        let options =
            EncoderOptions::default().with_compression_level(CompressionLevel::try_new(1).unwrap());
        let mut encoder = StreamingEncoder::new(options).unwrap();
        let mut compressed = Vec::new();
        for piece in body.chunks(7_919) {
            encoder.push(piece).unwrap();
            compressed.extend_from_slice(&encoder.take_output());
        }
        encoder.finish().unwrap();
        compressed.extend_from_slice(&encoder.take_output());

        let FrameHeader::Zstandard(header) =
            parse_frame_header_with_format(&compressed, crate::frame::Format::Zstd1).unwrap()
        else {
            panic!("expected a Zstandard frame");
        };
        let window = usize::try_from(header.window_size).unwrap();
        assert!(
            window * 4 <= body.len(),
            "the frame declares a {window}-byte window, too wide against a {}-byte body for \
             anything to become droppable",
            body.len()
        );

        for drain_every in [1usize, 3, 16] {
            let mut decoder = StreamingDecoder::new(DecoderOptions::default());
            let mut decoded = Vec::new();
            let mut peak_retained = 0usize;
            for (index, piece) in compressed.chunks(4_093).enumerate() {
                decoder.push(piece).unwrap();
                if index % drain_every == 0 {
                    decoded.extend_from_slice(&decoder.take_output());
                }
                peak_retained = peak_retained.max(decoder.retained_output_len());
            }
            decoder.finish().unwrap();
            decoded.extend_from_slice(&decoder.take_output());

            assert_eq!(decoded, body, "drain_every {drain_every}");
            assert!(
                peak_retained > window,
                "drain_every {drain_every}: held only {peak_retained} bytes, which is inside \
                 the {window}-byte window a match may still reach into"
            );
            // Compaction waits until half the buffer is droppable, so it
            // settles at twice the retained history. On top of that sits
            // whatever arrived since the last drain.
            // u64: the product overflows a 32-bit usize.
            let undrained =
                (drain_every as u64 * 4_093 * body.len() as u64 / compressed.len() as u64) as usize;
            let ceiling = 2 * (window + 1) + header.block_size_max as usize + undrained;
            assert!(
                peak_retained <= ceiling,
                "drain_every {drain_every}: held {peak_retained} bytes against a \
                 {window}-byte window, so history was never dropped"
            );
        }
    }

    /// Two frames in one stream is where compaction crosses a frame boundary:
    /// the second frame can be a few bytes old while the buffer still holds the
    /// tail of the first, so the prefix being dropped belongs entirely to a
    /// frame that has already ended and the current frame loses nothing.
    ///
    /// That state reads differently from the usual one. The retained count is
    /// exact rather than a bound, and it is legitimately far below the window —
    /// a frame that has produced nothing has nothing to retain. An invariant
    /// written for the case where the *current* frame is the one losing bytes
    /// rejects it, which is what `cargo fuzz run streaming_decode` found within
    /// ten minutes of this decoder existing.
    #[test]
    fn streaming_decode_compacts_across_a_frame_boundary() {
        // Level 1 for its narrow window: the first frame has to outrun it
        // before any of it becomes droppable.
        let first_body = repetitive_body(2_000_000);
        let second_body = repetitive_body(50_000);
        let mut stream = Vec::new();
        for body in [&first_body, &second_body] {
            let options = EncoderOptions::default()
                .with_compression_level(CompressionLevel::try_new(1).unwrap());
            let mut encoder = StreamingEncoder::new(options).unwrap();
            for piece in body.chunks(7_919) {
                encoder.push(piece).unwrap();
                stream.extend_from_slice(&encoder.take_output());
            }
            encoder.finish().unwrap();
            stream.extend_from_slice(&encoder.take_output());
        }

        let mut decoder = StreamingDecoder::new(DecoderOptions::default());
        let mut decoded = Vec::new();
        for piece in stream.chunks(4_093) {
            decoder.push(piece).unwrap();
            decoded.extend_from_slice(&decoder.take_output());
        }
        decoder.finish().unwrap();
        decoded.extend_from_slice(&decoder.take_output());

        let mut expected = first_body;
        expected.extend_from_slice(&second_body);
        assert_eq!(decoded, expected);
    }

    /// A caller that never reads is the opposite shape: nothing is droppable,
    /// every byte stays, and the decode still has to come out right.
    #[test]
    fn streaming_decode_without_draining_retains_everything() {
        let body = repetitive_body(300_000);
        let options =
            EncoderOptions::default().with_compression_level(CompressionLevel::try_new(9).unwrap());
        let mut encoder = StreamingEncoder::new(options).unwrap();
        encoder.params.max_history_bytes = 16 * 1024;
        let mut compressed = Vec::new();
        for piece in body.chunks(4_099) {
            encoder.push(piece).unwrap();
            compressed.extend_from_slice(&encoder.take_output());
        }
        encoder.finish().unwrap();
        compressed.extend_from_slice(&encoder.take_output());

        let mut decoder = StreamingDecoder::new(DecoderOptions::default());
        for piece in compressed.chunks(1_021) {
            decoder.push(piece).unwrap();
        }
        decoder.finish().unwrap();
        assert_eq!(decoder.retained_output_len(), body.len());
        assert_eq!(decoder.take_output(), body);
    }

    /// Compaction is the one place the match state has to be thrown away and
    /// rebuilt, because every position a finder holds is an index into `frame`
    /// and dropping bytes off the front moves all of them at once. Getting the
    /// rebuild wrong is quiet: the encoder keeps emitting valid frames and just
    /// stops finding matches, or -- for the binary tree, which is rebuilt lazily
    /// by the planner rather than eagerly here -- enters positions twice and
    /// corrupts the search.
    ///
    /// A real stream only reaches this after twice the level's window, which is
    /// tens of megabytes at the levels where the tree is used. Shrinking the
    /// window instead drives the same path in a few hundred kilobytes, and does
    /// it for every parser strategy rather than just the cheap ones. The header
    /// has already been written from the original window at this point, and
    /// declaring more than the encoder goes on to use is safe.
    #[test]
    fn compaction_preserves_history_across_every_parser_strategy() {
        let body = repetitive_body(400_000);

        for level in [1, 3, 5, 7, 9, 12, 13, 15, 17, 19, 22] {
            let options = EncoderOptions::default()
                .with_compression_level(CompressionLevel::try_new(level).unwrap());
            let mut encoder = StreamingEncoder::new(options).unwrap();
            encoder.params.max_history_bytes = 32 * 1024;

            let history_limit = encoder.history_limit();
            let bound = encoder.frame_capacity();
            let mut compressed = Vec::new();
            // A chunk size that is neither the block size nor a divisor of it,
            // so compaction lands mid-block as often as not.
            for piece in body.chunks(7_919) {
                encoder.push(piece).unwrap();
                compressed.extend_from_slice(&encoder.take_output());
                assert!(
                    encoder.frame.len() <= bound,
                    "L{level}: frame grew to {} against a documented bound of {bound}",
                    encoder.frame.len(),
                );
            }
            encoder.finish().unwrap();
            compressed.extend_from_slice(&encoder.take_output());

            assert!(
                body.len() > 4 * history_limit,
                "L{level}: the body must outrun the window several times over \
                 or this never compacts at all",
            );
            assert_eq!(
                crate::decode_all(&compressed).unwrap(),
                body,
                "L{level}: round-trip through a compacted window",
            );
            // A parser that dropped its history on every compaction still
            // compresses this content roughly 4x within a single window; one
            // that kept it manages better than 10x. The gap between those is
            // what this bound is watching.
            assert!(
                compressed.len() * 8 < body.len(),
                "L{level}: compacting cost the parser its history: {} bytes from {}",
                compressed.len(),
                body.len(),
            );
        }
    }

    /// The dictionary path keeps a different match state, seeded with the
    /// dictionary's own entries, and rebuilds it through a different route.
    #[test]
    fn compaction_preserves_history_with_a_dictionary() {
        let dictionary = repetitive_body(24_000);
        let body = repetitive_body(400_000);

        for level in [1, 5, 9, 13, 19] {
            let options = EncoderOptions::default()
                .with_compression_level(CompressionLevel::try_new(level).unwrap());
            let mut encoder = StreamingEncoder::with_dict(&dictionary, options).unwrap();
            encoder.params.max_history_bytes = 32 * 1024;

            let bound = encoder.frame_capacity();
            let mut compressed = Vec::new();
            for piece in body.chunks(7_919) {
                encoder.push(piece).unwrap();
                compressed.extend_from_slice(&encoder.take_output());
                assert!(
                    encoder.frame.len() <= bound,
                    "L{level}: frame exceeded {bound}"
                );
            }
            encoder.finish().unwrap();
            compressed.extend_from_slice(&encoder.take_output());

            assert_eq!(
                crate::decode_all_with_dict(&compressed, &dictionary).unwrap(),
                body,
                "L{level}: round-trip through a compacted window with a dictionary",
            );
            assert!(
                compressed.len() * 8 < body.len(),
                "L{level}: compacting cost the parser its history: {} bytes from {}",
                compressed.len(),
                body.len(),
            );
        }
    }
}
