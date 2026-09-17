//! [`std::io`] adapters.
//!
//! [`Writer`] compresses everything written to it and forwards the frame to an
//! inner [`Write`]; [`Reader`] decompresses a frame as it is pulled from an
//! inner [`Read`]. They exist so `zstandard` can be dropped into code that already
//! speaks `std::io` — `io::copy`, HTTP bodies, `BufReader`, archive readers —
//! without the caller hand-rolling the push/drain loop that
//! [`StreamingEncoder`] and
//! [`StreamingDecoder`] expose.
//!
//! ```
//! use std::io::{Read, Write};
//! use zstandard::io::{Reader, Writer};
//!
//! let payload = b"io adapters make this composable".repeat(500);
//!
//! let mut writer = Writer::new(Vec::new())?;
//! writer.write_all(&payload)?;
//! let compressed = writer.finish()?;
//!
//! let mut restored = Vec::new();
//! Reader::new(&compressed[..]).read_to_end(&mut restored)?;
//! assert_eq!(restored, payload);
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! Setting only the compression level goes through [`EncoderOptions`], where
//! every encoder knob lives. `i32` and `u8` both convert into a
//! [`CompressionLevel`](crate::CompressionLevel), and [`Error`] converts into
//! an [`io::Error`], so a level taken from configuration is validated inline
//! rather than in a separate step:
//!
//! ```
//! use std::io::Write;
//! use zstandard::{EncoderOptions, io::Writer};
//!
//! # fn compress_at(sink: Vec<u8>, level: i32) -> std::io::Result<Vec<u8>> {
//! let mut writer = Writer::with_options(
//!     sink,
//!     EncoderOptions::default().with_compression_level(level.try_into()?),
//! )?;
//! writer.write_all(b"payload")?;
//! writer.finish()
//! # }
//! # assert!(!compress_at(Vec::new(), 9)?.is_empty());
//! # assert!(compress_at(Vec::new(), 99).is_err());
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! A level fixed at compile time is one of the `CompressionLevel` constants and
//! needs no conversion:
//! `EncoderOptions::default().with_compression_level(CompressionLevel::BETTER)`.

use std::io::{self, Read, Write};

use crate::{
    DecoderDictionary, DecoderOptions, EncoderDictionary, EncoderOptions, Error, StreamingDecoder,
    StreamingEncoder,
};

/// Bytes pulled from the inner reader per refill.
///
/// [`StreamingDecoder::RECOMMENDED_INPUT_SIZE`] is a maximal block plus its
/// header, so every refill can complete a block rather than leaving a partial
/// one buffered for the next one.
const READ_CHUNK: usize = StreamingDecoder::RECOMMENDED_INPUT_SIZE;

/// First refill size. The chunk grows toward `READ_CHUNK` only when a refill
/// fills it, so a source shorter than this never pays for the full buffer.
///
/// `READ_CHUNK` is above the threshold at which the macOS allocator serves an
/// allocation from fresh pages rather than from a free list, and the reader
/// zeroes it whole up front; on a source of a few hundred bytes that mapping
/// and its page faults were most of what `Reader` cost over `decode_all`.
const READ_CHUNK_INITIAL: usize = 32768;

impl From<Error> for io::Error {
    fn from(error: Error) -> Self {
        // `Error` covers malformed input and rejected configuration, neither
        // of which maps to a more specific `ErrorKind`. The payload is kept so
        // callers can recover it with `io::Error::downcast`.
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

/// Compresses everything written to it into an inner [`Write`].
///
/// Call [`finish`](Self::finish) to terminate the frame and recover the inner
/// writer. Dropping a `Writer` without finishing flushes whatever is already
/// buffered but **does not** write the final block, leaving a truncated frame;
/// there is no way to report an error from `Drop`, so the frame tail is your
/// responsibility.
///
/// # Footprint
///
/// Roughly 25 KB by value, nearly all of it the [`StreamingEncoder`] inside;
/// see its footprint note for why. Box it when it is going anywhere other than
/// a local:
///
/// ```
/// # use zstandard::io::Writer;
/// enum Sink {
///     Plain(Vec<u8>),
///     Compressed(Box<Writer<'static, Vec<u8>>>),
/// }
/// # let _ = Sink::Compressed(Box::new(Writer::new(Vec::new())?));
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct Writer<'a, W: Write> {
    inner: Option<W>,
    encoder: StreamingEncoder<'a>,
}

impl<W: Write> Writer<'static, W> {
    /// Wrap `inner` with default [`EncoderOptions`].
    pub fn new(inner: W) -> io::Result<Self> {
        Self::with_options(inner, EncoderOptions::default())
    }

    /// Wrap `inner`, configuring the encoder.
    pub fn with_options(inner: W, options: EncoderOptions) -> io::Result<Self> {
        Ok(Self {
            inner: Some(inner),
            encoder: StreamingEncoder::new(options)?,
        })
    }
}

impl<'a, W: Write> Writer<'a, W> {
    /// Wrap `inner`, compressing against a prepared dictionary.
    ///
    /// The writer borrows the dictionary, so it may live on the stack rather
    /// than being leaked to `'static`.
    pub fn with_prepared_dict(
        inner: W,
        dictionary: &EncoderDictionary<'a>,
        options: EncoderOptions,
    ) -> io::Result<Self> {
        Ok(Self {
            inner: Some(inner),
            encoder: StreamingEncoder::with_prepared_dict(dictionary, options)?,
        })
    }

    /// Borrow the inner writer. Compressed bytes may still be buffered in the
    /// encoder, so what it has received is not the whole frame until
    /// [`finish`](Self::finish) returns.
    pub fn get_ref(&self) -> &W {
        self.inner.as_ref().expect("inner writer taken by finish")
    }

    /// Terminate the frame and return the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.encoder.finish()?;
        self.drain()?;
        let mut inner = self.inner.take().expect("inner writer taken by finish");
        inner.flush()?;
        Ok(inner)
    }

    /// Move compressed bytes out of the encoder and into the inner writer.
    ///
    /// The encoder's buffer is borrowed rather than taken, so it keeps its
    /// capacity and the next block writes into memory that is already there.
    /// Taking it would hand the allocation to the inner writer's `write_all`
    /// and leave the encoder to grow a fresh one for every block, which on a
    /// long stream is one allocation per 128 KiB written.
    fn drain(&mut self) -> io::Result<()> {
        let pending = self.encoder.pending_output_len();
        if pending == 0 {
            return Ok(());
        }
        let inner = self.inner.as_mut().expect("inner writer taken by finish");
        // Consumed whether or not the write succeeded. `write_all` does not
        // report how far it got, so the bytes cannot be retried; keeping them
        // would only make `Drop` write them a second time.
        let result = inner.write_all(self.encoder.pending_output());
        self.encoder.consume_output(pending);
        result
    }
}

impl<W: Write> Write for Writer<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.encoder.push(buf)?;
        self.drain()?;
        Ok(buf.len())
    }

    /// Emits any buffered input as a block and flushes the inner writer.
    ///
    /// This does not end the frame — the result is not a complete `.zst` until
    /// [`finish`](Self::finish) runs. Flushing mid-stream closes a block early
    /// and costs some ratio, so call it when a reader needs to make progress,
    /// not routinely.
    fn flush(&mut self) -> io::Result<()> {
        self.encoder.flush()?;
        self.drain()?;
        self.inner
            .as_mut()
            .expect("inner writer taken by finish")
            .flush()
    }
}

impl<W: Write> Drop for Writer<'_, W> {
    fn drop(&mut self) {
        // Best effort: push out whatever is already encoded. The frame's final
        // block is not written, because `finish` can fail and `Drop` cannot
        // report it.
        if self.inner.is_some() {
            let _ = self.drain();
        }
    }
}

impl<W: Write> std::fmt::Debug for Writer<'_, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("finished", &self.inner.is_none())
            .field("pending_output_len", &self.encoder.pending_output_len())
            .finish_non_exhaustive()
    }
}

/// Decompresses a Zstandard stream pulled from an inner [`Read`].
///
/// Reads to the end of the last frame in the stream; concatenated frames are
/// decoded in sequence and skippable frames are passed over, matching
/// [`decode_all`](crate::decode_all). Set
/// [`DecoderOptions::single_frame`](crate::DecoderOptions::single_frame) to
/// stop at the first frame and reject anything after it.
///
/// A few hundred bytes by value, so unlike [`Writer`] it embeds anywhere without
/// ceremony; the frame's ~35 KB of decode tables are allocated per frame.
pub struct Reader<'a, R: Read> {
    inner: R,
    decoder: StreamingDecoder<'a>,
    chunk: Vec<u8>,
    /// Inner reader returned 0 bytes; no more input will arrive.
    input_done: bool,
    /// `finish` has been called on the decoder.
    finished: bool,
}

impl<R: Read> Reader<'static, R> {
    /// Wrap `inner` with default [`DecoderOptions`].
    pub fn new(inner: R) -> Self {
        Self::with_options(inner, DecoderOptions::default())
    }

    /// Wrap `inner`, configuring the decoder. The defaults bound window size;
    /// see [`DecoderOptions`].
    pub fn with_options(inner: R, options: DecoderOptions) -> Self {
        Self {
            inner,
            decoder: StreamingDecoder::new(options),
            chunk: Vec::new(),
            input_done: false,
            finished: false,
        }
    }
}

impl<'a, R: Read> Reader<'a, R> {
    /// Wrap `inner`, decompressing against a prepared dictionary.
    ///
    /// The reader borrows the dictionary, so it may live on the stack rather
    /// than being leaked to `'static`.
    pub fn with_prepared_dict(
        inner: R,
        dictionary: &DecoderDictionary<'a>,
        options: DecoderOptions,
    ) -> Self {
        Self {
            inner,
            decoder: StreamingDecoder::with_prepared_dict(dictionary, options),
            chunk: Vec::new(),
            input_done: false,
            finished: false,
        }
    }

    /// Borrow the inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Recover the inner reader, discarding any undelivered output.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Recover the inner reader together with the compressed bytes this reader
    /// pulled from it but did not consume.
    ///
    /// [`into_inner`](Self::into_inner) alone cannot be used to find the end of
    /// a frame inside a longer source. This reader pulls whole chunks, so by
    /// the time a frame ends it has usually taken more from the source than
    /// the frame needed, and those bytes are gone from the source's cursor.
    /// The returned buffer is exactly that overshoot.
    ///
    /// It is non-empty in one case: a
    /// [`DecoderOptions::single_frame`](crate::DecoderOptions::single_frame)
    /// reader that stopped because something followed the frame. This is then
    /// that input, available for a diagnostic or to hand to whatever should
    /// have received it.
    ///
    /// It is empty after a read that ran to completion, and also after a
    /// partial read, which is worth stating because it looks like it should
    /// not be: this reader pulls in chunks of up to 128 KiB and the decoder
    /// consumes a whole chunk into its output buffer before any of it is
    /// handed out. A caller that reads sixteen bytes and stops has usually
    /// already drained its source and decoded all of it. Stopping a read early
    /// therefore discards decompressed output and recovers nothing on the
    /// compressed side; it is not a way to find a frame boundary.
    ///
    /// Undelivered decompressed output is still discarded; this is about the
    /// compressed side.
    pub fn into_inner_with_remainder(self) -> (R, Vec<u8>) {
        let remainder = self.decoder.unconsumed_input().to_vec();
        (self.inner, remainder)
    }
}

impl<R: Read> Read for Reader<'_, R> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if dst.is_empty() {
            return Ok(0);
        }

        loop {
            let produced = self.decoder.read(dst);
            if produced != 0 {
                return Ok(produced);
            }
            if self.finished {
                return Ok(0);
            }
            if self.input_done {
                // The decoder has no more buffered output and no more input is
                // coming, so the stream must end on a frame boundary. `finish`
                // is what reports a truncated frame rather than silently
                // returning a short read.
                self.decoder.finish()?;
                self.finished = true;
                continue;
            }

            if self.refill()? == 0 {
                self.input_done = true;
            }
        }
    }

    /// Appends the decoder's output to `buf` straight from its buffer.
    ///
    /// The default goes through [`read`](Self::read) into `buf`'s spare
    /// capacity, which it zeroes before every call and grows a step at a
    /// time; each step also had the decoder shifting what was left down its
    /// own buffer. Whole frames usually decode in one refill, so this is one
    /// `extend_from_slice` per frame instead. `io::copy` into a `Vec` lands
    /// here.
    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let start = buf.len();
        loop {
            let pending = self.decoder.pending_output_len();
            if pending != 0 {
                buf.extend_from_slice(self.decoder.pending_output());
                self.decoder.consume_output(pending);
                continue;
            }
            if self.finished {
                return Ok(buf.len() - start);
            }
            if self.input_done {
                self.decoder.finish()?;
                self.finished = true;
                continue;
            }

            match self.refill() {
                Ok(0) => self.input_done = true,
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
    }
}

impl<R: Read> Reader<'_, R> {
    /// Pull one chunk from the inner reader and push it to the decoder,
    /// returning how much arrived. The chunk is allocated on first use and
    /// grows to `READ_CHUNK` after a refill that filled it, so a short source
    /// stays on a small buffer and a long one moves to the full size after
    /// its first read.
    fn refill(&mut self) -> io::Result<usize> {
        if self.chunk.is_empty() {
            self.chunk = vec![0u8; READ_CHUNK_INITIAL];
        }
        let read = self.inner.read(&mut self.chunk)?;
        if read != 0 {
            self.decoder.push(&self.chunk[..read])?;
        }
        if read == self.chunk.len() && read < READ_CHUNK {
            self.chunk = vec![0u8; READ_CHUNK];
        }
        Ok(read)
    }
}

impl<R: Read> std::fmt::Debug for Reader<'_, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("input_done", &self.input_done)
            .field("finished", &self.finished)
            .field("pending_output_len", &self.decoder.pending_output_len())
            .finish_non_exhaustive()
    }
}
