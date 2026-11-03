//! Unaligned little-endian loads and stores, and short exact copies, for the
//! encoder's and decoder's hot paths.
//!
//! These take raw pointers: callers guarantee that the bytes are in bounds,
//! and initialized where they are read.

use std::ptr;

#[inline(always)]
pub(crate) unsafe fn read_u16(ptr: *const u8) -> u16 {
    u16::from_le(unsafe { ptr::read_unaligned(ptr.cast::<u16>()) })
}

#[inline(always)]
pub(crate) unsafe fn read_u32(ptr: *const u8) -> u32 {
    u32::from_le(unsafe { ptr::read_unaligned(ptr.cast::<u32>()) })
}

#[inline(always)]
pub(crate) unsafe fn read_u64(ptr: *const u8) -> u64 {
    u64::from_le(unsafe { ptr::read_unaligned(ptr.cast::<u64>()) })
}

#[inline(always)]
pub(crate) unsafe fn write_u16(ptr: *mut u8, value: u16) {
    unsafe { ptr::write_unaligned(ptr.cast::<u16>(), value.to_le()) }
}

#[inline(always)]
pub(crate) unsafe fn write_u32(ptr: *mut u8, value: u32) {
    unsafe { ptr::write_unaligned(ptr.cast::<u32>(), value.to_le()) }
}

#[inline(always)]
pub(crate) unsafe fn write_u64(ptr: *mut u8, value: u64) {
    unsafe { ptr::write_unaligned(ptr.cast::<u64>(), value.to_le()) }
}

#[inline(always)]
pub(crate) unsafe fn read_16(ptr: *const u8) -> [u8; 16] {
    unsafe { ptr::read_unaligned(ptr.cast::<[u8; 16]>()) }
}

#[inline(always)]
pub(crate) unsafe fn write_16(ptr: *mut u8, value: [u8; 16]) {
    unsafe { ptr::write_unaligned(ptr.cast::<[u8; 16]>(), value) }
}

#[inline(always)]
pub(crate) unsafe fn read_32(ptr: *const u8) -> [u8; 32] {
    unsafe { ptr::read_unaligned(ptr.cast::<[u8; 32]>()) }
}

#[inline(always)]
pub(crate) unsafe fn write_32(ptr: *mut u8, value: [u8; 32]) {
    unsafe { ptr::write_unaligned(ptr.cast::<[u8; 32]>(), value) }
}

#[inline(always)]
pub(crate) unsafe fn copy_16(from: *const u8, to: *mut u8) {
    unsafe { ptr::copy_nonoverlapping(from, to, 16) }
}

/// Copies exactly `len` bytes, `16 <= len <= 64`, between non-overlapping
/// ranges, as two fixed-size copies that may overlap each other.
///
/// Written as a loop of 16-byte copies, this compiles to a call to `memcpy`.
#[inline(always)]
pub(crate) unsafe fn copy_16_to_64(from: *const u8, to: *mut u8, len: usize) {
    debug_assert!((16..=64).contains(&len));
    unsafe {
        if len > 32 {
            ptr::copy_nonoverlapping(from, to, 32);
            ptr::copy_nonoverlapping(from.add(len - 32), to.add(len - 32), 32);
        } else {
            ptr::copy_nonoverlapping(from, to, 16);
            ptr::copy_nonoverlapping(from.add(len - 16), to.add(len - 16), 16);
        }
    }
}

/// Copies exactly `len <= 64` bytes between non-overlapping ranges.
///
/// Below 16 bytes, both pieces are loaded before either is stored, which
/// measured faster in the encoder than letting each store follow its load.
#[inline(always)]
pub(crate) unsafe fn copy_short(from: *const u8, to: *mut u8, len: usize) {
    debug_assert!(len <= 64);
    unsafe {
        if len > 32 {
            copy_16(from, to);
            copy_16(from.add(16), to.add(16));
            copy_16(from.add(len - 32), to.add(len - 32));
            copy_16(from.add(len - 16), to.add(len - 16));
        } else if len >= 16 {
            copy_16(from, to);
            copy_16(from.add(len - 16), to.add(len - 16));
        } else if len >= 8 {
            let (head, tail) = (read_u64(from), read_u64(from.add(len - 8)));
            write_u64(to, head);
            write_u64(to.add(len - 8), tail);
        } else if len >= 4 {
            let (head, tail) = (read_u32(from), read_u32(from.add(len - 4)));
            write_u32(to, head);
            write_u32(to.add(len - 4), tail);
        } else if len > 0 {
            // Bytes 0, `len / 2` and `len - 1` cover lengths 1 to 3.
            let (a, b, c) = (*from, *from.add(len / 2), *from.add(len - 1));
            *to = a;
            *to.add(len / 2) = b;
            *to.add(len - 1) = c;
        }
    }
}

/// Copies exactly `len` bytes between non-overlapping ranges: up to 64
/// inline, more with [`copy_long`]. Short copies are common at the ends of
/// small blocks, where a call would cost more than the copy.
#[inline(always)]
pub(crate) unsafe fn copy_exact(from: *const u8, to: *mut u8, len: usize) {
    unsafe {
        if len <= 64 {
            copy_short(from, to, len);
        } else {
            copy_long(from, to, len);
        }
    }
}

/// Copies `len` bytes between non-overlapping ranges with `memcpy`.
///
/// glibc copies 2112 bytes and more with `rep movsb` on CPUs with fast
/// short `rep movsb`. On AMD Zen 3 and Zen 4 that runs at 3-5 GB/s instead of
/// 25-50 for some distances between the buffers, which made 64 KiB literals
/// decode 18x and encode 12x slower on Zen 3. There the copy is made in
/// pieces below that size, which glibc copies with vector loads and stores.
///
/// On x86-64 this stays out of line, so that the loops calling it keep the
/// code they had with a call to `memcpy`: inlined, the check slowed HTML and
/// protobuf compression by 3-7% on Ice Lake and Emerald Rapids Xeons, which
/// never take it, and by up to 27% on a Cascade Lake-class Xeon.
#[cfg_attr(target_arch = "x86_64", inline(never))]
#[cfg_attr(not(target_arch = "x86_64"), inline(always))]
pub(crate) unsafe fn copy_long(from: *const u8, to: *mut u8, len: usize) {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    if len > LONG_COPY_PIECE && slow_rep_movsb() {
        return unsafe { copy_in_pieces(from, to, len) };
    }
    unsafe { ptr::copy_nonoverlapping(from, to, len) }
}

/// Below glibc's `rep movsb` threshold on Zen 3 and Zen 4, 2112 bytes.
#[cfg(all(target_arch = "x86_64", not(miri)))]
const LONG_COPY_PIECE: usize = 2048;

/// [`copy_long`] in pieces of `LONG_COPY_PIECE` bytes.
#[cfg(all(target_arch = "x86_64", not(miri)))]
#[inline(never)]
unsafe fn copy_in_pieces(from: *const u8, to: *mut u8, len: usize) {
    let mut done = 0;
    // SAFETY: the pieces cover `0..len` of both ranges.
    unsafe {
        while len - done > LONG_COPY_PIECE {
            ptr::copy_nonoverlapping(from.add(done), to.add(done), LONG_COPY_PIECE);
            done += LONG_COPY_PIECE;
        }
        ptr::copy_nonoverlapping(from.add(done), to.add(done), len - done);
    }
}

/// Whether the CPU is an AMD family 19h core, Zen 3 or Zen 4, on which
/// `rep movsb` is slow for some buffer addresses. Zen 5 (family 1Ah) copies
/// faster with it.
#[cfg(all(target_arch = "x86_64", not(miri)))]
fn slow_rep_movsb() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    // 0: not yet known; then 1 + the answer.
    static STATE: AtomicU8 = AtomicU8::new(0);
    match STATE.load(Ordering::Relaxed) {
        0 => {
            #[allow(unused_unsafe)]
            // SAFETY: CPUID is available on every x86-64 CPU.
            let (vendor, signature) = unsafe {
                use std::arch::x86_64::__cpuid;
                (__cpuid(0), __cpuid(1).eax)
            };
            let amd = (vendor.ebx, vendor.edx, vendor.ecx)
                == (
                    u32::from_le_bytes(*b"Auth"),
                    u32::from_le_bytes(*b"enti"),
                    u32::from_le_bytes(*b"cAMD"),
                );
            // The family is the base family plus the extended one.
            let family = ((signature >> 8) & 0xf) + ((signature >> 20) & 0xff);
            let slow = amd && family == 0x19;
            STATE.store(1 + u8::from(slow), Ordering::Relaxed);
            slow
        }
        state => state == 2,
    }
}

#[cfg(all(test, target_arch = "x86_64", not(miri)))]
mod tests {
    #[test]
    fn copy_in_pieces_copies_every_byte() {
        let src: Vec<u8> = (0..70_000u32).map(|i| (i * 7 + i / 251) as u8).collect();
        for len in [0, 1, 2047, 2048, 2049, 4096, 4097, 65_536, 70_000] {
            let mut dst = vec![0; len];
            unsafe { super::copy_in_pieces(src.as_ptr(), dst.as_mut_ptr(), len) };
            assert_eq!(dst, src[..len]);
        }
    }
}
