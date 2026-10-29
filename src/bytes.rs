//! Unaligned little-endian loads and stores, and short exact copies, for the
//! encoder's and decoder's hot paths.
//!
//! These take raw pointers: callers guarantee that the bytes are in bounds,
//! and initialized where they are read.

use std::ptr;

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

/// Copies exactly `len` bytes, `16 <= len <= 128`, between non-overlapping
/// ranges, as two fixed-size copies that may overlap each other.
///
/// Written as a loop of 16-byte copies, this compiles to a call to `memcpy`.
#[inline(always)]
pub(crate) unsafe fn copy_16_to_128(from: *const u8, to: *mut u8, len: usize) {
    debug_assert!((16..=128).contains(&len));
    unsafe {
        if len > 64 {
            // The second copy is the smallest that reaches the end: for 80
            // bytes, 64 and 16 rather than 64 and 64.
            ptr::copy_nonoverlapping(from, to, 64);
            let tail = if len <= 80 {
                16
            } else if len <= 96 {
                32
            } else {
                64
            };
            ptr::copy_nonoverlapping(from.add(len - tail), to.add(len - tail), tail);
        } else if len > 32 {
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
/// inline, more with `memcpy`. Short copies are common at the ends of small
/// blocks, where a call would cost more than the copy.
#[inline(always)]
pub(crate) unsafe fn copy_exact(from: *const u8, to: *mut u8, len: usize) {
    unsafe {
        if len <= 64 {
            copy_short(from, to, len);
        } else {
            ptr::copy_nonoverlapping(from, to, len);
        }
    }
}
