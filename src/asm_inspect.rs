//! Assembly-inspection wrappers for comparing Rust vs C zstd decode hot-loop.
//!
//! Each function isolates one hot-loop operation behind `#[inline(never)]`
//! `#[unsafe(no_mangle)]` so it appears as a named symbol in `--emit asm` output.
//! The matching C functions live in `oracles/asm/c_pieces.c`.
//!
//! ## Usage
//!
//! ```sh
//! # Compile Rust assembly
//! cargo rustc --release --lib --features asm-inspect -- --emit asm
//!
//! # Run the comparison script (compiles C side too)
//! ./oracles/asm/compare.sh
//! ```
//!
//! ## Design
//!
//! Functions are `extern "C"` + `#[unsafe(no_mangle)]` for easy grepping in the `.s`
//! output but are never actually called across FFI. The `asm-inspect` feature
//! gate keeps this module out of normal builds. See `oracles/asm/FINDINGS.md`
//! for analysis results.

#![allow(unsafe_code)]
// These shims are `extern "C"` only so that `#[unsafe(no_mangle)]` gives them
// stable, greppable names in `--emit asm` output. Nothing calls them, from Rust
// or from C — the module header says so, and `oracles/asm/compare.sh` only ever
// reads the generated assembly. So the ABI of a `BitDStreamStatus` return or a
// tuple return is not merely acceptable here, it is unobservable. Keeping the
// Rust types is the point: the whole exercise is comparing the code LLVM
// generates for *this crate's* types against C's, which a
// repr(C)-ified stand-in would defeat.
#![allow(improper_ctypes_definitions)]

use crate::entropy::bitstream::{BitDStream, BitDStreamStatus};
use crate::entropy::fse::{DState, SequenceDecodeEntry};

// ---------------------------------------------------------------------------
// Piece 1: Bit read — read_bits_fast_zero_safe vs BIT_readBitsFast
// Called 6x per iteration (3 extra-bit reads + 3 FSE state updates).
// ---------------------------------------------------------------------------

/// Rust's zero-safe bit read (handles nb_bits==0). Expected overhead vs C:
/// +2-3 insns (extra shift + sub + and for the zero-safe mask).
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn asm_read_bits_fast_zero_safe(stream: &mut BitDStream, nb_bits: u32) -> u64 {
    stream.read_bits_fast_zero_safe(nb_bits)
}

/// Rust's fast bit read (requires nb_bits >= 1, matches C BIT_readBitsFast).
/// Should produce identical instruction count to C.
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn asm_read_bits_fast(stream: &mut BitDStream, nb_bits: u32) -> u64 {
    stream.read_bits_fast(nb_bits)
}

/// Rust's look_bits_fast (no skip, matches C BIT_lookBitsFast).
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn asm_look_bits_fast(stream: &BitDStream, nb_bits: u32) -> u64 {
    stream.look_bits_fast(nb_bits)
}

// ---------------------------------------------------------------------------
// Piece 2: FSE state update — update_state_with_seq_entry_fast
// Called 3x per iteration.
// ---------------------------------------------------------------------------

/// FSE state update using SequenceDecodeEntry. Internally calls
/// read_bits_fast_zero_safe. Overhead comes from the zero-safe mask.
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn asm_fse_update_state(
    state: &mut DState,
    stream: &mut BitDStream,
    entry: SequenceDecodeEntry,
) {
    crate::entropy::fse::update_state_with_seq_entry_fast(state, stream, entry);
}

// ---------------------------------------------------------------------------
// Piece 3: Bitstream reload — reload vs BIT_reloadDStream
// Called 2x per iteration.
// ---------------------------------------------------------------------------

/// Full reload with all branches (overflow, end-of-buffer, fast path).
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn asm_reload(stream: &mut BitDStream) -> BitDStreamStatus {
    stream.reload()
}

/// Fast-path reload only (ptr >= limitPtr).
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn asm_reload_fast(stream: &mut BitDStream) -> BitDStreamStatus {
    stream.reload_fast()
}

// ---------------------------------------------------------------------------
// Piece 4: Offset resolution — inlined in execute_sequence! macro
// We extract the branching logic into a standalone function for measurement.
// ---------------------------------------------------------------------------

/// Offset resolution with rep-offset branching. Returns (offset, rep1, rep2, rep3).
/// This isolates the branching structure from the execute_sequence! macro
/// (sequence.rs:941-974) for assembly comparison with C's offset resolution
/// (zstd_decompress_block.c:1288-1321).
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn asm_resolve_offset(
    offset_value: u32,
    literal_length: usize,
    rep1: u32,
    rep2: u32,
    rep3: u32,
) -> (usize, u32, u32, u32) {
    let mut r1 = rep1;
    let mut r2 = rep2;
    let mut r3 = rep3;

    let offset: usize = if offset_value > 3 {
        let o = offset_value - 3;
        r3 = r2;
        r2 = r1;
        r1 = o;
        o as usize
    } else if offset_value == 1 && literal_length != 0 {
        r1 as usize
    } else if offset_value == 0 {
        // In the real code this returns Err; here we return 0 as sentinel.
        0
    } else {
        let rep_index = offset_value + (literal_length == 0) as u32;
        if rep_index == 2 {
            // The swap leaves the old `r2` — the value this branch resolves to
            // — in `r1`.
            std::mem::swap(&mut r2, &mut r1);
            r1 as usize
        } else if rep_index == 3 {
            let o = r3;
            r3 = r2;
            r2 = r1;
            r1 = o;
            o as usize
        } else {
            // rep1 - 1 case
            let o = r1.wrapping_sub(1);
            r3 = r2;
            r2 = r1;
            r1 = o;
            o as usize
        }
    };

    (offset, r1, r2, r3)
}

// ---------------------------------------------------------------------------
// Piece 5: Literal copy
// ---------------------------------------------------------------------------

/// Inline 16-byte SIMD copy for use inside composite functions.
/// This MUST be #[inline(always)] so composite functions get the SIMD
/// instructions inlined, matching how the real hot loop works.
///
/// # Safety
///
/// `src` must be readable and `dst` writable for 16 bytes.
#[inline(always)]
unsafe fn copy_16_inline(dst: *mut u8, src: *const u8) {
    // SAFETY: 16 readable bytes at `src` and 16 writable at `dst` by contract;
    // every arm is an unaligned move. Mirrors `sequence::copy_16`.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let v = core::arch::aarch64::vld1q_u8(src);
        core::arch::aarch64::vst1q_u8(dst, v);
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: as above.
        #[cfg(target_feature = "sse2")]
        unsafe {
            let v = core::arch::x86_64::_mm_loadu_si128(src as *const core::arch::x86_64::__m128i);
            core::arch::x86_64::_mm_storeu_si128(dst as *mut core::arch::x86_64::__m128i, v);
        }
        // SAFETY: as above.
        #[cfg(not(target_feature = "sse2"))]
        unsafe {
            core::ptr::copy_nonoverlapping(src, dst, 16)
        };
    }
    // SAFETY: as above.
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    unsafe {
        core::ptr::copy_nonoverlapping(src, dst, 16)
    };
}

/// 16-byte SIMD copy as standalone function (for individual comparison).
///
/// # Safety
///
/// As [`copy_16_inline`].
#[inline(never)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asm_copy_16(dst: *mut u8, src: *const u8) {
    // SAFETY: this function's contract is exactly the callee's.
    unsafe { copy_16_inline(dst, src) };
}

/// Literal copy: copy_16 + simple 16-byte loop (matching C's ZSTD_wildcopy).
/// Rust guards with `if literal_length > 0`; C always does ZSTD_copy16.
/// The last iteration may overshoot by up to 15 bytes (safe with WILDCOPY_OVERLENGTH).
///
/// # Safety
///
/// `src` must be readable and `dst` writable for `literal_length` bytes
/// rounded up to a multiple of 16.
#[inline(never)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asm_copy_literals(dst: *mut u8, src: *const u8, literal_length: usize) {
    // SAFETY: the caller has provided the rounded-up length, which is what the
    // deliberate overshoot of the last iteration needs.
    unsafe {
        if literal_length > 0 {
            copy_16_inline(dst, src);
            if literal_length > 16 {
                let mut pos = 16usize;
                while pos < literal_length {
                    copy_16_inline(dst.add(pos), src.add(pos));
                    pos += 16;
                }
            }
        }
    }
}

/// Literal copy WITHOUT the zero guard — always copies, like C.
///
/// # Safety
///
/// As [`asm_copy_literals`], and additionally 16 bytes must be accessible even
/// when `literal_length` is 0, since this variant drops that guard.
#[inline(never)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asm_copy_literals_no_guard(
    dst: *mut u8,
    src: *const u8,
    literal_length: usize,
) {
    // SAFETY: as in `asm_copy_literals`, plus the unconditional first copy.
    unsafe {
        copy_16_inline(dst, src);
        if literal_length > 16 {
            let mut pos = 16usize;
            while pos < literal_length {
                copy_16_inline(dst.add(pos), src.add(pos));
                pos += 16;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Piece 6: Match copy — copy_match_inline
// ---------------------------------------------------------------------------

/// Match copy with offset >= 16 (non-overlapping, SIMD).
/// Unified loop from pos=0, matching C zstd. Last iteration may overshoot.
///
/// # Safety
///
/// As [`asm_copy_literals`], with `match_length` in place of the literal
/// length.
#[inline(never)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asm_copy_match_large_offset(
    dst: *mut u8,
    src: *const u8,
    match_length: usize,
) {
    // SAFETY: the caller has provided the rounded-up length.
    unsafe {
        let mut pos = 0usize;
        while pos < match_length {
            copy_16_inline(dst.add(pos), src.add(pos));
            pos += 16;
        }
    }
}

/// Match copy with small offset (< 16), overlap-safe.
/// Reproduces the DEC32/DEC64 overlap copy + 8-byte wildcopy.
///
/// # Safety
///
/// As [`crate::sequence`]'s `copy_match_inline`, whose small-offset branch this
/// mirrors: `base[match_src..out_pos]` initialized, and
/// `out_pos + match_length + WILDCOPY_OVERLENGTH` within the allocation.
#[inline(never)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asm_copy_match_small_offset(
    base: *mut u8,
    out_pos: usize,
    match_src: usize,
    match_length: usize,
) {
    let offset = out_pos - match_src;
    // SAFETY: as in `sequence::copy_match_inline`; every pointer here derives
    // from `base` and stays inside the caller's reservation.
    unsafe {
        let dst = base.add(out_pos);
        let src = base.add(match_src);

        let mut ip = src;
        let mut op = dst;

        if offset < 8 {
            const DEC32TABLE: [u32; 8] = [0, 1, 2, 1, 4, 4, 4, 4];
            const DEC64TABLE: [i32; 8] = [8, 8, 8, 7, 8, 9, 10, 11];

            *op = *ip;
            *op.add(1) = *ip.add(1);
            *op.add(2) = *ip.add(2);
            *op.add(3) = *ip.add(3);
            ip = ip.add(DEC32TABLE[offset] as usize);
            core::ptr::copy_nonoverlapping(ip, op.add(4), 4);
            // Fused `-dec64` and `+8` into one signed move, matching
            // `sequence::copy_match_inline`. Applying them separately forms an
            // intermediate pointer before the start of the buffer for offsets
            // 5, 6 and 7, which is UB even though the final address is in
            // bounds — the defect Miri found in the real hot loop. Nothing here
            // is ever executed, but this file exists to emit assembly that
            // represents the shipped code, so it has to track it.
            ip = ip.offset(8isize - DEC64TABLE[offset] as isize);
        } else {
            core::ptr::copy_nonoverlapping(ip, op, 8);
            ip = ip.add(8);
        }
        op = op.add(8);

        if match_length > 8 {
            let end = dst.add(match_length);
            while op < end {
                core::ptr::copy_nonoverlapping(ip, op, 8);
                ip = ip.add(8);
                op = op.add(8);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Piece 7: Guard checks — bounds + history window
// ---------------------------------------------------------------------------

/// Bounds guard check (literal end + sequence end).
/// Returns true if within bounds, false if out of bounds.
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn asm_bounds_check(
    literal_cursor: usize,
    literal_length: usize,
    lit_len: usize,
    out_pos: usize,
    match_length: usize,
    out_end: usize,
) -> bool {
    let literal_end = literal_cursor + literal_length;
    let seq_end = out_pos + literal_length + match_length;
    literal_end <= lit_len && seq_end <= out_end
}

/// History window check. Returns true if offset is valid.
/// Uses && instead of .min() to enable ccmp on aarch64.
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn asm_window_check(
    offset: usize,
    out_pos: usize,
    frame_start: usize,
    window_size: usize,
) -> bool {
    offset <= out_pos - frame_start && offset <= window_size
}

/// Combined guard: bounds + window (matches the full guard sequence in the hot loop).
/// Uses || for bounds and && for window to enable ccmp chains on aarch64.
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn asm_full_guard(
    literal_cursor: usize,
    literal_length: usize,
    lit_len: usize,
    out_pos: usize,
    match_length: usize,
    out_end: usize,
    offset: usize,
    frame_start: usize,
    window_size: usize,
) -> bool {
    let total = literal_length + match_length;
    let literal_end = literal_cursor + literal_length;
    if total > out_end || literal_end > lit_len {
        return false;
    }
    offset <= out_pos - frame_start && offset <= window_size
}

// ---------------------------------------------------------------------------
// Bonus: Prefetch (Rust has 2 explicit prefetch calls per iteration)
// ---------------------------------------------------------------------------

/// Prefetch pair using single inline asm block with immediate +64 offset.
/// Saves one instruction vs two separate prefetch_l1 calls.
///
/// # Safety
///
/// `base.add(pos)` must be a pointer the caller may legitimately form; the
/// prefetch itself imposes nothing further.
#[inline(never)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asm_prefetch_pair(base: *const u8, pos: usize) {
    // SAFETY: `base.add(pos)` is in bounds by contract; `prefetch_match` places
    // no validity requirement on the address it is handed.
    unsafe { crate::sequence::prefetch_match(base.add(pos)) };
}

// ---------------------------------------------------------------------------
// Composite: Full decode step (without execute) to measure total decode overhead
// ---------------------------------------------------------------------------

/// One full decode step: read 3 extra-bit fields + 3 FSE state updates.
/// This is the "decode" half of the hot loop iteration.
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn asm_full_decode_step(
    reader: &mut BitDStream,
    literal_state: &mut DState,
    match_state: &mut DState,
    offset_state: &mut DState,
    offset_entry: SequenceDecodeEntry,
    match_entry: SequenceDecodeEntry,
    literal_entry: SequenceDecodeEntry,
) -> (usize, usize, u32) {
    let offset_code = offset_entry.nb_additional_bits as u32;
    let match_extra_bits = match_entry.nb_additional_bits as u32;
    let literal_extra_bits = literal_entry.nb_additional_bits as u32;

    let offset_extra = reader.read_bits_fast_zero_safe(offset_code) as u32;
    let match_extra = reader.read_bits_fast_zero_safe(match_extra_bits) as u32;
    if offset_code + match_extra_bits + literal_extra_bits >= 31 {
        let _ = reader.reload();
    }
    let literal_extra = reader.read_bits_fast_zero_safe(literal_extra_bits) as u32;

    let literal_length = (literal_entry.baseline + literal_extra) as usize;
    let match_length = (match_entry.baseline + match_extra) as usize;
    let offset_value = (1u32 << offset_code) + offset_extra;

    // FSE state updates
    crate::entropy::fse::update_state_with_seq_entry_fast(literal_state, reader, literal_entry);
    crate::entropy::fse::update_state_with_seq_entry_fast(match_state, reader, match_entry);
    crate::entropy::fse::update_state_with_seq_entry_fast(offset_state, reader, offset_entry);

    (literal_length, match_length, offset_value)
}
