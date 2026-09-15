#[inline(always)]
pub(crate) fn highbit32(value: u32) -> u32 {
    debug_assert!(value != 0);
    31u32 ^ value.leading_zeros()
}

#[inline(always)]
pub(crate) fn read_u32(input: &[u8], offset: usize) -> u32 {
    let bytes = input[offset..].first_chunk::<4>().unwrap();
    u32::from_le_bytes(*bytes)
}

/// Read a little-endian u32 without bounds checking.
///
/// # Safety
///
/// `offset + 4 <= input.len()` must hold.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn read_u32_unchecked(input: &[u8], offset: usize) -> u32 {
    debug_assert!(offset + 4 <= input.len());
    // SAFETY: the caller guarantees `offset + 4 <= input.len()`, so `offset`
    // stays inside the allocation and four bytes are readable from there. The
    // read is unaligned, so `input` carries no alignment requirement.
    let raw = unsafe { core::ptr::read_unaligned(input.as_ptr().add(offset) as *const u32) };
    raw.to_le()
}

#[inline(always)]
pub(crate) fn read_u64(input: &[u8], offset: usize) -> u64 {
    let bytes = input[offset..].first_chunk::<8>().unwrap();
    u64::from_le_bytes(*bytes)
}

/// Read a little-endian u64 without bounds checking.
///
/// # Safety
///
/// `offset + 8 <= input.len()` must hold.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn read_u64_unchecked(input: &[u8], offset: usize) -> u64 {
    debug_assert!(offset + 8 <= input.len());
    // SAFETY: the caller guarantees `offset + 8 <= input.len()`, so `offset`
    // stays inside the allocation and eight bytes are readable from there. The
    // read is unaligned, so `input` carries no alignment requirement.
    let raw = unsafe { core::ptr::read_unaligned(input.as_ptr().add(offset) as *const u64) };
    raw.to_le()
}

#[inline(always)]
pub(crate) fn write_u64(output: &mut [u8], offset: usize, value: u64) {
    output[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// Prefetch data into L1 cache. This is a hint instruction with no correctness impact.
/// The pointer need not be valid — prefetch of an invalid address is a no-op on x86_64/aarch64.
///
/// Compiled away entirely under Miri, which cannot execute inline assembly or
/// the x86 prefetch intrinsics. That costs the check nothing: a prefetch cannot
/// change a result, and the loads it anticipates still happen where Miri sees
/// them.
///
/// # Safety
///
/// There is no memory-safety requirement on `ptr`. A prefetch of an unmapped
/// address is architecturally a no-op on both targets this emits a real
/// instruction for. The signature stays `unsafe` because the body is inline
/// assembly and target intrinsics, and because every caller already holds an
/// `unsafe` block for the pointer arithmetic that produced `ptr` — making it
/// safe would remove no `unsafe` from any call site.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn prefetch_l1(ptr: *const u8) {
    #[cfg(miri)]
    let _ = ptr;
    #[cfg(not(miri))]
    {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: `_mm_prefetch` has no validity requirement on its
            // argument; the instruction is a hint and faults on nothing.
            #[cfg(target_feature = "sse")]
            unsafe {
                core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(
                    ptr as *const i8,
                )
            };
        }
        // SAFETY: `prfm` is a hint instruction that cannot fault or trap on any
        // address. `nostack` holds because the block touches no stack memory,
        // and `preserves_flags` because `prfm` writes no condition flags.
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("prfm pldl1keep, [{ptr}]", ptr = in(reg) ptr,
                             options(nostack, preserves_flags))
        };
        // No-op on other architectures.
        #[cfg(not(any(
            all(target_arch = "x86_64", target_feature = "sse"),
            target_arch = "aarch64"
        )))]
        {
            let _ = ptr;
        }
    }
}

/// Prefetch data into L2 cache. Better for large offsets where the data is unlikely
/// to be in L1. Avoids polluting L1 for data that may be evicted before use.
///
/// Compiled away under Miri, for the reason given on [`prefetch_l1`].
///
/// # Safety
///
/// None; see [`prefetch_l1`].
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn prefetch_l2(ptr: *const u8) {
    #[cfg(miri)]
    let _ = ptr;
    #[cfg(not(miri))]
    {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: as in `prefetch_l1`, a prefetch hint has no validity
            // requirement on its argument.
            #[cfg(target_feature = "sse")]
            unsafe {
                core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T1 }>(
                    ptr as *const i8,
                )
            };
        }
        // SAFETY: as in `prefetch_l1`; `prfm` cannot fault, touches no stack,
        // and writes no condition flags.
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("prfm pldl2keep, [{ptr}]", ptr = in(reg) ptr,
                             options(nostack, preserves_flags))
        };
        #[cfg(not(any(
            all(target_arch = "x86_64", target_feature = "sse"),
            target_arch = "aarch64"
        )))]
        {
            let _ = ptr;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_little_endian_words() {
        let mut bytes = [0u8; 16];
        let value = 0x1122_3344_5566_7788u64;
        write_u64(&mut bytes, 2, value);
        assert_eq!(read_u64(&bytes, 2), value);
        bytes[..4].copy_from_slice(&0xA1B2_C3D4u32.to_le_bytes());
        assert_eq!(read_u32(&bytes, 0), 0xA1B2_C3D4);
    }

    #[test]
    fn computes_highbit32() {
        assert_eq!(highbit32(1), 0);
        assert_eq!(highbit32(2), 1);
        assert_eq!(highbit32(3), 1);
        assert_eq!(highbit32(0x8000_0000), 31);
    }
}
