use crate::{
    entropy::mem::{highbit32, read_u64, write_u64},
    error::{Error, Result},
};
use core::marker::PhantomData;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BitDStreamStatus {
    Unfinished = 0,
    EndOfBuffer = 1,
    Completed = 2,
    Overflow = 3,
}

/// The entropy accumulator, 64 bits wide on every target rather than the
/// machine word.
///
/// The format sets the width, not the host: one sequence can spend 63 bits
/// between reloads (a 31-bit offset plus 16 bits of match-length extra plus 16
/// of literal-length extra), so a 32-bit accumulator rejects valid frames and
/// silently truncates on the encode side.
pub(crate) type BitContainer = u64;

pub(crate) const CONTAINER_BYTES: usize = core::mem::size_of::<BitContainer>();
pub(crate) const CONTAINER_BITS: u32 = BitContainer::BITS;

/// Bits that always fit between two flushes, C zstd's
/// `ZSTD_STREAM_ACCUMULATOR_MIN`: a flush leaves up to 7 bits behind.
pub(crate) const STREAM_ACCUMULATOR_MIN: u32 = CONTAINER_BITS - 7;

const BIT_MASK: [BitContainer; 32] = [
    0, 1, 3, 7, 0xF, 0x1F, 0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF, 0x3FFF, 0x7FFF,
    0xFFFF, 0x1FFFF, 0x3FFFF, 0x7FFFF, 0xFFFFF, 0x1FFFFF, 0x3FFFFF, 0x7FFFFF, 0xFFFFFF, 0x1FFFFFF,
    0x3FFFFFF, 0x7FFFFFF, 0xFFFFFFF, 0x1FFFFFFF, 0x3FFFFFFF, 0x7FFFFFFF,
];

#[derive(Debug)]
pub(crate) struct BitCStream<'a> {
    pub(crate) bit_container: BitContainer,
    pub(crate) bit_pos: u32,
    dst: &'a mut [u8],
    byte_pos: usize,
    end_pos: usize,
}

impl<'a> BitCStream<'a> {
    #[inline(always)]
    pub(crate) fn new(dst: &'a mut [u8]) -> Result<Self> {
        if dst.len() <= CONTAINER_BYTES {
            return Err(Error::DstSizeTooSmall);
        }

        let end_pos = dst.len() - CONTAINER_BYTES;
        Ok(Self {
            bit_container: 0,
            bit_pos: 0,
            dst,
            byte_pos: 0,
            end_pos,
        })
    }

    #[inline(always)]
    pub(crate) fn add_bits(&mut self, value: BitContainer, nb_bits: u32) {
        debug_assert!(nb_bits < BIT_MASK.len() as u32);
        debug_assert!(nb_bits + self.bit_pos < CONTAINER_BITS);
        self.bit_container |= (value & BIT_MASK[nb_bits as usize]) << self.bit_pos;
        self.bit_pos += nb_bits;
    }

    #[inline(always)]
    pub(crate) fn add_bits_fast(&mut self, value: BitContainer, nb_bits: u32) {
        debug_assert!(nb_bits + self.bit_pos < CONTAINER_BITS);
        debug_assert!((value >> nb_bits) == 0);
        self.bit_container |= value << self.bit_pos;
        self.bit_pos += nb_bits;
    }

    #[inline(always)]
    pub(crate) fn flush_bits_fast(&mut self) {
        let nb_bytes = (self.bit_pos >> 3) as usize;
        debug_assert!(self.byte_pos <= self.end_pos);
        write_u64(self.dst, self.byte_pos, self.bit_container);
        self.byte_pos += nb_bytes;
        self.bit_pos &= 7;
        self.bit_container >>= nb_bytes * 8;
    }

    #[inline(always)]
    pub(crate) fn flush_bits(&mut self) {
        let nb_bytes = (self.bit_pos >> 3) as usize;
        debug_assert!(self.byte_pos <= self.end_pos);
        write_u64(self.dst, self.byte_pos, self.bit_container);
        self.byte_pos = (self.byte_pos + nb_bytes).min(self.end_pos);
        self.bit_pos &= 7;
        self.bit_container >>= nb_bytes * 8;
    }

    /// Like `flush_bits` but uses an unchecked write to eliminate the
    /// bounds check on `dst[byte_pos..byte_pos+8]`.
    ///
    /// # Safety
    ///
    /// `byte_pos + 8 <= dst.len()` must hold, i.e. `byte_pos <= end_pos`.
    #[allow(unsafe_code)]
    #[inline(always)]
    pub(crate) unsafe fn flush_bits_unchecked(&mut self) {
        let nb_bytes = (self.bit_pos >> 3) as usize;
        debug_assert!(self.byte_pos <= self.end_pos);
        // SAFETY: the caller guarantees `byte_pos + 8 <= dst.len()`, so the
        // offset pointer stays in bounds and a full 8-byte word is writable
        // from it. `end_pos` is `dst.len() - 8`, which is what makes the
        // `byte_pos <= end_pos` contract sufficient for an 8-byte store.
        // Unaligned, so `dst` needs no alignment.
        let ptr = unsafe { self.dst.as_mut_ptr().add(self.byte_pos) };
        unsafe { core::ptr::write_unaligned(ptr as *mut u64, self.bit_container.to_le()) };
        self.byte_pos += nb_bytes;
        self.bit_pos &= 7;
        self.bit_container >>= nb_bytes * 8;
    }

    #[inline(always)]
    pub(crate) fn close(mut self) -> usize {
        self.add_bits_fast(1, 1);
        self.flush_bits();
        if self.byte_pos >= self.end_pos {
            return 0;
        }
        self.byte_pos + usize::from(self.bit_pos > 0)
    }
}

/// Backward bitstream reader using raw pointers, matching C zstd's BIT_DStream_t.
///
/// Stores `ptr` and `start` as raw pointers instead of offsets into a slice.
/// This eliminates one load instruction per reload: C does `ldr data, [ptr]`
/// while the offset approach requires `ldr base, [struct]; ldr data, [base,
/// offset]`.
///
/// C's third field, `limitPtr`, has no counterpart here; `bytes_behind_ptr`
/// derives the same bound from the two pointers that are kept.
#[derive(Debug, Clone)]
pub(crate) struct BitDStream<'a> {
    pub(crate) bit_container: BitContainer,
    pub(crate) bits_consumed: u32,
    ptr: *const u8,
    start: *const u8,
    /// One past the last byte of the bitstream. Every container load reads
    /// `CONTAINER_BYTES` from `ptr`, and the invariant that keeps that in
    /// bounds — `ptr` starts at `end - CONTAINER_BYTES` and only moves back —
    /// is otherwise carried by argument alone. Debug builds and Miri get to
    /// check it; release builds carry no field.
    #[cfg(debug_assertions)]
    end: *const u8,
    _phantom: PhantomData<&'a [u8]>,
}

// Safety: BitDStream borrows immutable data via raw pointers for the lifetime 'a.
// The PhantomData<&'a [u8]> ensures the borrow is tracked. The raw pointers
// are never shared across threads beyond what &'a [u8] would allow.
#[allow(unsafe_code)]
unsafe impl Send for BitDStream<'_> {}
#[allow(unsafe_code)]
unsafe impl Sync for BitDStream<'_> {}

impl<'a> BitDStream<'a> {
    #[allow(unsafe_code)]
    #[inline(always)]
    pub(crate) fn new(src: &'a [u8]) -> Result<Self> {
        if src.is_empty() {
            return Err(Error::SrcSizeWrong);
        }

        let base = src.as_ptr();
        // SAFETY: one past the end of a slice is a valid pointer to form.
        #[cfg(debug_assertions)]
        let end = unsafe { base.add(src.len()) };
        let last_byte = src[src.len() - 1];
        if last_byte == 0 {
            return Err(Error::Corruption("bitstream end marker is zero"));
        }
        let bits_consumed = 8 - highbit32(last_byte as u32);

        if src.len() >= CONTAINER_BYTES {
            // SAFETY: `src.len() >= CONTAINER_BYTES`, so the offset is inside
            // the slice. This is also where the load invariant is established:
            // `ptr` is exactly `end - CONTAINER_BYTES` and only decreases.
            let ptr = unsafe { base.add(src.len() - CONTAINER_BYTES) };
            let bit_container = read_u64(src, src.len() - CONTAINER_BYTES);
            Ok(Self {
                bit_container,
                bits_consumed,
                ptr,
                start: base,
                #[cfg(debug_assertions)]
                end,
                _phantom: PhantomData,
            })
        } else {
            // Too short for a container-wide load. Assembling it byte by byte
            // is what keeps every later load in bounds: `ptr` stays at `start`,
            // so `reload` takes the `ptr == start` path and never loads.
            let mut bit_container: BitContainer = 0;
            for (index, byte) in src.iter().copied().enumerate() {
                bit_container |= BitContainer::from(byte) << (index * 8);
            }
            Ok(Self {
                bit_container,
                bits_consumed: bits_consumed + ((CONTAINER_BYTES - src.len()) * 8) as u32,
                ptr: base,
                start: base,
                #[cfg(debug_assertions)]
                end,
                _phantom: PhantomData,
            })
        }
    }

    #[inline(always)]
    fn reg_mask(&self) -> u32 {
        CONTAINER_BITS - 1
    }

    #[inline(always)]
    pub(crate) fn look_bits(&self, nb_bits: u32) -> BitContainer {
        let start = CONTAINER_BITS
            .wrapping_sub(self.bits_consumed)
            .wrapping_sub(nb_bits);
        self.get_middle_bits(start, nb_bits)
    }

    #[inline(always)]
    pub(crate) fn look_bits_fast(&self, nb_bits: u32) -> BitContainer {
        debug_assert!(nb_bits >= 1);
        let reg_mask = self.reg_mask();
        (self.bit_container << (self.bits_consumed & reg_mask))
            >> (((reg_mask + 1) - nb_bits) & reg_mask)
    }

    #[inline(always)]
    fn get_middle_bits(&self, start: u32, nb_bits: u32) -> BitContainer {
        let reg_mask = self.reg_mask();
        debug_assert!(nb_bits < BIT_MASK.len() as u32);
        (self.bit_container >> (start & reg_mask)) & BIT_MASK[nb_bits as usize]
    }

    #[inline(always)]
    pub(crate) fn skip_bits(&mut self, nb_bits: u32) {
        self.bits_consumed += nb_bits;
    }

    #[inline(always)]
    pub(crate) fn can_read_fast(&self, nb_bits: u32) -> bool {
        nb_bits != 0 && self.bits_consumed + nb_bits <= CONTAINER_BITS
    }

    #[inline(always)]
    pub(crate) fn read_bits(&mut self, nb_bits: u32) -> BitContainer {
        if nb_bits == 0 {
            return 0;
        }
        if self.can_read_fast(nb_bits) {
            return self.read_bits_fast(nb_bits);
        }
        let value = self.look_bits(nb_bits);
        self.skip_bits(nb_bits);
        value
    }

    #[inline(always)]
    pub(crate) fn read_bits_fast(&mut self, nb_bits: u32) -> BitContainer {
        let value = self.look_bits_fast(nb_bits);
        self.skip_bits(nb_bits);
        value
    }

    /// Like `read_bits_fast` but safe to call with `nb_bits == 0` (returns 0).
    /// Uses a mask to clear the result when nb_bits is 0, avoiding the
    /// undefined-shift issue in `look_bits_fast`.
    #[inline(always)]
    pub(crate) fn read_bits_fast_zero_safe(&mut self, nb_bits: u32) -> BitContainer {
        let reg_mask = self.reg_mask();
        let raw = (self.bit_container << (self.bits_consumed & reg_mask))
            >> (((reg_mask + 1) - nb_bits) & reg_mask);
        // When nb_bits == 0 the shift above returns the full container instead
        // of 0. Mask it: (1 << 0) - 1 == 0 clears the result.
        let value = raw & ((1 as BitContainer) << nb_bits).wrapping_sub(1);
        self.skip_bits(nb_bits);
        value
    }

    /// How many bytes sit between `start` and `ptr`, which is how far `ptr` can
    /// still walk backwards.
    ///
    /// This replaces a `limit_ptr()` helper that computed `start.add(size_of::
    /// <usize>())` and compared it against `ptr` as an integer. For a bitstream
    /// shorter than one word that limit is past the end of the allocation, and
    /// `pointer::add` is undefined behavior there whether or not the result is
    /// ever dereferenced — a short bitstream comes straight from the frame, so
    /// hostile input reaches it. Subtracting two addresses asks the same
    /// question (`ptr >= start + width`), cannot leave the allocation, and
    /// cannot overflow, since `ptr >= start` holds from construction onwards:
    /// `ptr` only ever moves back by at most `available`.
    ///
    /// Kept inline rather than cached in a field: as a field LLVM spilled it
    /// and reloaded it four times per decode iteration, and one SUB is cheaper
    /// than the spill slot.
    #[inline(always)]
    fn bytes_behind_ptr(&self) -> usize {
        self.ptr.addr() - self.start.addr()
    }

    /// Guards every container load: the `CONTAINER_BYTES` at `ptr` have to lie
    /// inside the bitstream, which takes a check at each end.
    ///
    /// The upper bound covers the read. `ptr` starts at `end - CONTAINER_BYTES`
    /// and only moves back, so a load that reaches past `end` means that
    /// invariant was broken somewhere.
    ///
    /// The lower bound covers the arithmetic that precedes the read. Both
    /// `reload` and `reload_fast` do `ptr = ptr.sub(nb_bytes)`, and computing a
    /// pointer below `start` is undefined behavior on its own, before anything
    /// is dereferenced. `reload` cannot get there: it rejects
    /// `bits_consumed > CONTAINER_BITS` before subtracting and then clamps
    /// `nb_bytes` to what is actually behind `ptr`. `reload_fast` makes neither
    /// check, and holds only because the huff0 four-stream loops that call it
    /// keep `bits_consumed` at 55 or below. That is a property of its callers
    /// rather than of itself, and it is `pub(crate)`, so one more caller is all
    /// it takes for this assertion to be the thing that catches the mistake.
    ///
    /// Both are checked after the subtraction rather than before, which is
    /// where Miri reports the pointer anyway; the assertion is what makes an
    /// ordinary debug build notice, since a `sub` past the start neither traps
    /// nor produces a wrong answer on real hardware.
    #[inline(always)]
    fn debug_assert_load_in_bounds(&self) {
        debug_assert!(self.ptr.addr() >= self.start.addr());
        #[cfg(debug_assertions)]
        debug_assert!(self.ptr.addr() + CONTAINER_BYTES <= self.end.addr());
    }

    #[allow(unsafe_code)]
    #[inline(always)]
    pub(crate) fn reload_fast(&mut self) -> BitDStreamStatus {
        if self.bytes_behind_ptr() < CONTAINER_BYTES {
            return BitDStreamStatus::Overflow;
        }

        let consumed_bytes = (self.bits_consumed >> 3) as usize;
        unsafe {
            self.ptr = self.ptr.sub(consumed_bytes);
            self.bits_consumed &= 7;
            self.debug_assert_load_in_bounds();
            self.bit_container = core::ptr::read_unaligned(self.ptr as *const u64).to_le();
        }
        BitDStreamStatus::Unfinished
    }

    #[allow(unsafe_code)]
    #[inline(always)]
    pub(crate) fn reload(&mut self) -> BitDStreamStatus {
        if self.bits_consumed > CONTAINER_BITS {
            return reload_overflow();
        }
        if self.bytes_behind_ptr() >= CONTAINER_BYTES {
            return self.reload_fast();
        }
        if self.ptr == self.start {
            return reload_at_start(self.bits_consumed);
        }

        let mut nb_bytes = (self.bits_consumed >> 3) as usize;
        let mut result = BitDStreamStatus::Unfinished;
        // Safety: ptr > start (checked above), so offset_from is valid.
        let available = unsafe { self.ptr.offset_from(self.start) as usize };
        if nb_bytes > available {
            nb_bytes = available;
            result = BitDStreamStatus::EndOfBuffer;
        }
        // Safety: ptr - nb_bytes >= start, and reading CONTAINER_BYTES from the
        // new ptr is within the original slice: `ptr` was set to
        // `end - CONTAINER_BYTES` at construction and only ever moves back.
        // Reaching here at all means the slice was at least that long, since a
        // shorter one leaves `ptr == start` and takes the branch above.
        unsafe {
            self.ptr = self.ptr.sub(nb_bytes);
            self.bits_consumed -= (nb_bytes * 8) as u32;
            self.debug_assert_load_in_bounds();
            self.bit_container = core::ptr::read_unaligned(self.ptr as *const u64).to_le();
        }
        result
    }

    #[inline(always)]
    pub(crate) fn end_of_stream(&self) -> bool {
        core::ptr::eq(self.ptr, self.start) && self.bits_consumed == CONTAINER_BITS
    }
}

// Cold helpers for `reload()` rare paths. Marking these `#[cold]` and
// `#[inline(never)]` tells LLVM to lay out the hot path contiguously.

#[cold]
#[inline(never)]
fn reload_overflow() -> BitDStreamStatus {
    BitDStreamStatus::Overflow
}

#[cold]
#[inline(never)]
fn reload_at_start(bits_consumed: u32) -> BitDStreamStatus {
    if bits_consumed < CONTAINER_BITS {
        BitDStreamStatus::EndOfBuffer
    } else {
        BitDStreamStatus::Completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(values: &[(BitContainer, u32)]) -> Vec<u8> {
        let mut bytes = vec![0u8; 64];
        let written = {
            let mut stream = BitCStream::new(&mut bytes).unwrap();
            for &(value, nb_bits) in values {
                if stream.bit_pos + nb_bits >= STREAM_ACCUMULATOR_MIN {
                    stream.flush_bits();
                }
                stream.add_bits(value, nb_bits);
            }
            stream.close()
        };
        bytes.truncate(written);
        bytes
    }

    #[test]
    fn roundtrips_values_in_reverse_bitstream_order() {
        let values = [
            (0b10, 2),
            (0b111, 3),
            (0x12, 5),
            (0x155, 9),
            (0x1A, 5),
            (0x1F, 5),
            (0x2AA, 10),
            (0x1234, 13),
        ];
        let encoded = encode(&values);
        let mut stream = BitDStream::new(&encoded).unwrap();

        for &(expected, nb_bits) in values.iter().rev() {
            assert_eq!(stream.read_bits(nb_bits), expected);
            assert_ne!(stream.reload(), BitDStreamStatus::Overflow);
        }
    }

    #[test]
    fn rejects_zero_terminated_input() {
        let err = BitDStream::new(&[0]).unwrap_err();
        assert_eq!(err, Error::Corruption("bitstream end marker is zero"));
    }

    #[test]
    fn fast_and_regular_reads_match() {
        let values = [(0b10101, 5), (0b1111111, 7), (0b1011, 4), (0b1110, 4)];
        let encoded = encode(&values);
        let mut regular = BitDStream::new(&encoded).unwrap();
        let mut fast = BitDStream::new(&encoded).unwrap();

        for &(_, nb_bits) in values.iter().rev() {
            assert_eq!(regular.read_bits(nb_bits), fast.read_bits_fast(nb_bits));
            assert_eq!(regular.reload(), fast.reload());
        }
    }

    /// The accumulator is 64 bits on every target, not the machine word: one
    /// zstd sequence can spend 31 + 16 + 16 bits between reloads.
    #[test]
    fn accumulates_a_full_sequences_worth_of_bits_without_a_flush() {
        const WIDEST_SEQUENCE_BITS: u32 = 31 + 16 + 16;
        const { assert!(WIDEST_SEQUENCE_BITS < CONTAINER_BITS) };

        let values: [(BitContainer, u32); 3] = [(0x7FFF_FFFF, 31), (0xFFFF, 16), (0xABCD, 16)];
        let mut bytes = vec![0u8; 64];
        let written = {
            let mut stream = BitCStream::new(&mut bytes).unwrap();
            for &(value, nb_bits) in &values {
                stream.add_bits(value, nb_bits);
            }
            assert_eq!(stream.bit_pos, WIDEST_SEQUENCE_BITS);
            // `close` adds the end marker, which needs room of its own.
            stream.flush_bits();
            stream.close()
        };
        bytes.truncate(written);

        let mut stream = BitDStream::new(&bytes).unwrap();
        for &(expected, nb_bits) in values.iter().rev() {
            assert_eq!(stream.read_bits(nb_bits), expected);
        }
        assert_ne!(stream.reload(), BitDStreamStatus::Overflow);
    }

    /// Every bitstream length from 1 byte up must load without reading past the
    /// slice. Miri and the debug bounds assertion are what make this a check
    /// rather than a smoke test; the vector is sized to its exact length so a
    /// native ASan-style run has a tail to fault on.
    #[test]
    fn reads_never_run_past_a_short_bitstream() {
        for len in 1..=(CONTAINER_BYTES * 3) {
            let mut bytes: Vec<u8> = (0..len).map(|i| (i as u8) | 1).collect();
            bytes[len - 1] |= 0x80;
            let mut stream = BitDStream::new(&bytes).unwrap();
            // Walk the whole stream in small reads; each one can trigger a
            // container load from a different offset.
            loop {
                let _ = stream.read_bits(5);
                if stream.reload() != BitDStreamStatus::Unfinished {
                    break;
                }
            }
        }
    }
}
