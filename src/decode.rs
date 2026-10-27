//! Raw Snappy block decoding.
//!
//! The decoder has two loops:
//!
//! * [`decode_fast`] handles the common tags (literals of up to 60 bytes and
//!   copies with 1- or 2-byte offsets) without branching on the tag type. It
//!   needs enough input and output slack that every tag can over-read and
//!   over-write by a fixed amount, so it stops short of both buffer ends.
//! * [`decode_one`] decodes a single tag with exact bounds checks. It handles
//!   long literals, 4-byte-offset copies, malformed input, the last bytes of
//!   each buffer and blocks too small for the fast loop.
//!
//! Whenever the fast loop sees anything unusual it returns, the careful path
//! decodes (or rejects) that tag, and the fast loop resumes. Runs of
//! identical copies, as produced by run-length-like data, are handed to
//! [`decode_run`] instead.

use std::mem::MaybeUninit;
use std::ptr;

use crate::{Error, Result, decode_error};

/// Bytes the fast loop may read past the start of the tag it is decoding:
/// one tag byte, a literal of up to 60 bytes copied in 64-byte units, and the
/// following tag byte.
const TAG_READ_SLOP: usize = 1 + 64;
/// Bytes the fast loop may write past the output position of a tag. Copies
/// are written in 32/64-byte units and overlapping copies in whole pattern
/// periods, which reach at most 95 bytes (see [`copy_overlapping`]).
const TAG_WRITE_SLOP: usize = 96;
/// Bytes copied unconditionally per tag in the fast loop; longer elements
/// take a second, rarely used branch.
const FAST_COPY: usize = 32;
/// Tags decoded per fast-loop limit check.
const UNROLL: usize = 2;
/// The fast loop runs while `ip < src.len() - INPUT_MARGIN`.
const INPUT_MARGIN: usize = (UNROLL - 1) * 61 + TAG_READ_SLOP + 4;
/// The fast loop runs while `op < dst.len() - OUTPUT_MARGIN`.
const OUTPUT_MARGIN: usize = (UNROLL - 1) * 64 + TAG_WRITE_SLOP;

/// Lookup tables for the fast loop, indexed by tag and kept together so that
/// one base register addresses all of them.
#[repr(C, align(64))]
struct Tables {
    /// `len - (offset_high << 8)`, where `offset_high` is the part of a copy
    /// offset stored in the tag itself.
    ///
    /// Subtracting the offset bits that follow the tag yields `len - offset`,
    /// which is positive exactly when a copy overlaps its own output. Literals
    /// get a spurious offset of 256 so that they never look like an
    /// overlapping copy. Tags the fast loop does not handle (literals longer
    /// than 60 bytes and copies with 4-byte offsets) are `0xff`, which always
    /// looks like an overlapping copy and is then recognized by the high bit
    /// of its length.
    len_minus_offset: [isize; 256],
    /// Mask selecting the offset bytes that follow the tag.
    offset_mask: [usize; 256],
    /// Total encoded size of the element; only meaningful for fast-loop tags.
    advance: [u8; 256],
}

static TABLES: Tables = build_tables();

const fn build_tables() -> Tables {
    let mut tables = Tables {
        len_minus_offset: [0; 256],
        offset_mask: [0; 256],
        advance: [1; 256],
    };
    let mut tag = 0usize;
    while tag < 256 {
        let data = (tag >> 2) as i16;
        let (len, offset_high, mask, advance) = match tag & 3 {
            0 if data < 60 => (data + 1, 1, 0, data + 2),
            1 => ((data & 7) + 4, data >> 3, 0xff, 2),
            2 => (data + 1, 0, 0xffff, 3),
            _ => (0xff, 0, 0, 1),
        };
        tables.len_minus_offset[tag] = (len - (offset_high << 8)) as isize;
        tables.offset_mask[tag] = mask as usize;
        tables.advance[tag] = advance as u8;
        tag += 1;
    }
    tables
}

/// Marks the enclosing branch as unlikely for block layout.
#[cold]
#[inline(always)]
fn cold() {}

/// Hides the value of `value` from the optimizer, so that it stays in its
/// own register instead of being rederived from wherever it came from.
#[inline(always)]
fn opaque(value: usize) -> usize {
    #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), not(miri)))]
    {
        let mut value = value;
        // SAFETY: the empty assembly block only claims to modify `value`.
        unsafe {
            std::arch::asm!(
                "/* {0} */",
                inout(reg) value,
                options(pure, nomem, nostack, preserves_flags),
            );
        }
        value
    }
    #[cfg(any(not(any(target_arch = "x86_64", target_arch = "aarch64")), miri))]
    {
        value
    }
}

/// Generates `fn $name(selector, if_zero, otherwise) -> $ty` returning
/// `if_zero` when `selector == 0` and `otherwise` if not, using a conditional
/// move.
///
/// The fast loop selects between literal and copy this way. Written as plain
/// Rust, LLVM either narrows the select of the next tag to a byte and
/// re-extends it at the top of the loop (adding a cycle to the loop-carried
/// dependency chain), or turns a select into a branch on the tag type, which
/// mispredicts on most real data.
macro_rules! select_if_zero {
    ($name:ident, $ty:ty) => {
        #[inline(always)]
        #[allow(
            clippy::pointers_in_nomem_asm_block,
            reason = "the assembly only moves the pointer values"
        )]
        fn $name(selector: usize, if_zero: $ty, otherwise: $ty) -> $ty {
            #[cfg(all(target_arch = "x86_64", not(miri)))]
            {
                let mut result = otherwise;
                // SAFETY: register-only instructions without memory access.
                unsafe {
                    std::arch::asm!(
                        "test {s}, {s}",
                        "cmovz {r}, {z}",
                        s = in(reg) selector,
                        z = in(reg) if_zero,
                        r = inout(reg) result,
                        options(pure, nomem, nostack),
                    );
                }
                result
            }
            #[cfg(all(target_arch = "aarch64", not(miri)))]
            {
                let result;
                // SAFETY: register-only instructions without memory access.
                unsafe {
                    std::arch::asm!(
                        "cmp {s}, #0",
                        "csel {r}, {z}, {o}, eq",
                        s = in(reg) selector,
                        z = in(reg) if_zero,
                        o = in(reg) otherwise,
                        r = lateout(reg) result,
                        options(pure, nomem, nostack),
                    );
                }
                result
            }
            #[cfg(any(not(any(target_arch = "x86_64", target_arch = "aarch64")), miri))]
            {
                if selector == 0 { if_zero } else { otherwise }
            }
        }
    };
}

select_if_zero!(select_if_zero, usize);
select_if_zero!(select_ptr_if_zero, *const u8);

/// Decodes the body of a raw Snappy block (everything after the length
/// header) into `dst`, which must be exactly the decoded length.
pub(crate) fn decode_body(src: &[u8], dst: &mut [MaybeUninit<u8>]) -> Result<()> {
    let dst_len = dst.len();
    if let Some(literal) = single_literal(src, dst_len) {
        // Incompressible input is stored as one literal.
        // SAFETY: `literal` and `dst` both have `dst_len` bytes and cannot
        // overlap, as `dst` is exclusively borrowed.
        unsafe {
            ptr::copy_nonoverlapping(
                literal.as_ptr(),
                dst.as_mut_ptr().cast::<u8>(),
                dst_len,
            );
        }
        return Ok(());
    }
    decode_in_place(src, dst)
}

/// Returns the payload if `src` is exactly one literal of `dst_len` bytes.
#[inline(always)]
fn single_literal(src: &[u8], dst_len: usize) -> Option<&[u8]> {
    let (&tag, rest) = src.split_first()?;
    if tag & 3 != 0 {
        return None;
    }
    let len_minus_one = usize::from(tag >> 2);
    let (len_minus_one, payload) = match len_minus_one {
        0..=59 => (len_minus_one, rest),
        60 => (usize::from(*rest.first()?), &rest[1..]),
        61 => {
            let bytes = rest.get(..2)?;
            (
                usize::from(u16::from_le_bytes([bytes[0], bytes[1]])),
                &rest[2..],
            )
        }
        _ => return None,
    };
    (len_minus_one + 1 == dst_len && payload.len() == dst_len).then_some(payload)
}

/// Decodes directly into `dst`, using the fast loop where both buffers have
/// enough room left and the careful path near their ends.
#[inline(never)]
fn decode_in_place(src: &[u8], dst: &mut [MaybeUninit<u8>]) -> Result<()> {
    let src_len = src.len();
    let dst_len = dst.len();
    let dst_ptr = dst.as_mut_ptr().cast::<u8>();
    let fast_ip_limit = src_len.saturating_sub(INPUT_MARGIN);
    let fast_op_limit = dst_len.saturating_sub(OUTPUT_MARGIN);
    let mut ip = 0usize;
    let mut op = 0usize;

    while ip < src_len {
        if ip < fast_ip_limit && op < fast_op_limit {
            // SAFETY: the limits leave `INPUT_MARGIN` bytes of `src` and
            // `OUTPUT_MARGIN` bytes of `dst` after the starting positions.
            let at_run;
            (ip, op, at_run) = unsafe {
                decode_fast(src.as_ptr(), ip, fast_ip_limit, dst_ptr, op, fast_op_limit)
            };
            if ip >= src_len {
                break;
            }
            // SAFETY: `ip < src_len`, `op <= dst_len` and `dst_ptr` is valid
            // for `dst_len` writes.
            if at_run
                && let Some(next) = unsafe { decode_run(src, ip, dst_ptr, dst_len, op) }
            {
                (ip, op) = next;
                continue;
            }
        }
        // SAFETY: as above.
        (ip, op) = unsafe { decode_one(src, ip, dst_ptr, dst_len, op)? };
    }

    if op != dst_len {
        return decode_error(Error::HeaderMismatch {
            expected: dst_len,
            actual: op,
        });
    }
    Ok(())
}

/// Decodes tags until `ip >= ip_limit`, `op >= op_limit` or a tag that needs
/// the careful path. Returns the position of the first undecoded tag, the
/// corresponding output position, and whether that tag starts a run of
/// identical copies.
///
/// # Safety
///
/// `src` must be valid for reads of `ip_limit + INPUT_MARGIN` bytes and `dst`
/// for writes of `op_limit + OUTPUT_MARGIN` bytes; `dst[..op]` must be
/// initialized.
#[inline(never)]
unsafe fn decode_fast(
    src: *const u8,
    ip: usize,
    ip_limit: usize,
    dst: *mut u8,
    op: usize,
    op_limit: usize,
) -> (usize, usize, bool) {
    debug_assert!(ip < ip_limit && op < op_limit);
    // SAFETY: `ip < ip_limit` and `op < op_limit`, so all four are in bounds.
    // From here on `ip` points just past the tag being decoded. The output
    // position is kept as a pointer so that stores use simple addressing.
    let (mut ip, ip_limit, mut out, out_limit) = unsafe {
        (
            src.add(ip + 1),
            src.add(ip_limit + 1),
            dst.add(op),
            dst.add(op_limit),
        )
    };
    let mut tag = usize::from(unsafe { *ip.sub(1) });
    let tables = &TABLES;
    let mut at_run = false;

    'outer: loop {
        for _ in 0..UNROLL {
            debug_assert!(tag < 256);
            let tag_type = tag & 3;

            // Load both candidates for the next tag before selecting, which
            // keeps the select off the load-to-load dependency chain. The
            // volatile reads stop LLVM from turning this into one load from
            // a selected address.
            // SAFETY: both are within `TAG_READ_SLOP` of the tag.
            let next_literal =
                usize::from(unsafe { ptr::read_volatile(ip.add((tag >> 2) + 1)) });
            let next_copy = usize::from(unsafe { ptr::read_volatile(ip.add(tag_type)) });
            let next_tag = select_if_zero(tag_type, next_literal, next_copy);

            // SAFETY: `tag` is a byte (`select_if_zero` hides that from
            // LLVM), and the reads are within `TAG_READ_SLOP` of the tag. The
            // advance is only used once the tag is known to be a fast-loop
            // tag.
            let (entry, trailer, next_ip) = unsafe {
                (
                    *tables.len_minus_offset.get_unchecked(tag),
                    read_u32_le(ip) as usize & *tables.offset_mask.get_unchecked(tag),
                    ip.add(usize::from(*tables.advance.get_unchecked(tag))),
                )
            };
            // Without `opaque` LLVM keeps `entry` alive to test its low byte
            // on the cold path, which costs a register copy per tag.
            let len = opaque((entry & 0xff) as usize);
            // SAFETY: `entry` is in `-1792..=255` and `trailer` in
            // `0..=0xffff`, so this cannot overflow; saying so lets LLVM
            // branch on the flags of the subtraction.
            let len_minus_offset = unsafe { entry.unchecked_sub(trailer as isize) };
            if len_minus_offset > 0 {
                cold();
                if len & 0x80 != 0 {
                    // Long literal or 4-byte offset copy.
                    break 'outer;
                }
                // Overlapping copy, or a copy with offset zero.
                let offset = (len as isize - len_minus_offset) as usize;
                // SAFETY: `out` was derived from `dst`.
                let op = unsafe { out.offset_from(dst) } as usize;
                if offset == 0 || offset > op || (offset < 8 && op < 8) {
                    break 'outer;
                }
                // SAFETY: the next element is within the input margin.
                if unsafe { next_is_same(ip, next_ip) } {
                    // A run of identical copies; see `decode_run`.
                    at_run = true;
                    break 'outer;
                }
                // SAFETY: `1 <= offset < len <= 64`, `offset <= op` and the
                // output margin covers the pattern overshoot.
                unsafe { copy_overlapping(out, offset, len) };
            } else {
                // `out - offset`, which is before `dst` if the offset reaches
                // before the start of the output. Literals have a spurious
                // offset of 256 and copy from the input instead.
                let match_from = out
                    .cast_const()
                    .wrapping_offset(len_minus_offset - len as isize);
                if (match_from as usize) < (dst as usize) {
                    cold();
                    if tag_type != 0 {
                        break 'outer;
                    }
                }
                let from = select_ptr_if_zero(tag_type, ip, match_from);
                // SAFETY: `from` is either the literal (within the input
                // margin) or at least `len` bytes before `out`; the extra
                // bytes are within the slop on both sides. `ptr::copy`
                // (memmove semantics) copies bytes untyped, so stale output
                // bytes past `out` are never read as integers.
                unsafe {
                    ptr::copy(from, out, FAST_COPY);
                    if len > FAST_COPY {
                        cold();
                        // Runs of non-overlapping copies are runs of
                        // 64-byte copies, the longest a tag can encode.
                        if tag == 0xfe
                            && (len as isize - len_minus_offset) as usize
                                <= MAX_RUN_PERIOD
                            && next_is_same(ip, next_ip)
                        {
                            // A run of identical copies; see `decode_run`.
                            at_run = true;
                            break 'outer;
                        }
                        ptr::copy(
                            from.add(FAST_COPY),
                            out.add(FAST_COPY),
                            64 - FAST_COPY,
                        );
                    }
                }
            }
            // SAFETY: `len <= 64`, within the output margin.
            out = unsafe { out.add(len) };
            ip = next_ip;
            tag = next_tag;
        }
        if ip >= ip_limit || out >= out_limit {
            break;
        }
    }

    // SAFETY: `ip` and `out` were derived from `src` and `dst`; `ip` points
    // past a tag.
    unsafe {
        (
            ip.offset_from(src) as usize - 1,
            out.offset_from(dst) as usize,
            at_run,
        )
    }
}

/// Longest period [`decode_run`] handles, which bounds how far
/// [`fill_periodic`] writes past the end of a run.
const MAX_RUN_PERIOD: usize = 112;

/// Whether the element after the one whose tag is at `ip - 1` is identical
/// to it; `next_ip` is just past the next element's tag.
///
/// # Safety
///
/// Both elements must be readable, plus 4 bytes past the second tag.
#[inline(always)]
unsafe fn next_is_same(ip: *const u8, next_ip: *const u8) -> bool {
    unsafe {
        let size = next_ip.offset_from(ip) as usize;
        let mask = if size == 2 { 0xffff } else { 0xff_ffff };
        (read_u32_le(ip.sub(1)) ^ read_u32_le(next_ip.sub(1))) & mask == 0
    }
}

/// Decodes a copy at `src[ip]` together with the identical copies that
/// follow it, as many as fit the remaining output with slop to spare, or
/// returns `None` if the tag at `ip` does not start such a run.
///
/// Runs like this come from run-length-like data, where every element
/// copies from output written moments before. Decoding them one at a time
/// would make each copy wait for the previous stores to reach the cache,
/// while [`fill_periodic`] reads only bytes written before the run.
///
/// # Safety
///
/// `ip < src.len()`, `op <= dst_len`, `dst` must be valid for `dst_len`
/// writes and `dst[..op]` must be initialized.
#[inline(never)]
unsafe fn decode_run(
    src: &[u8],
    ip: usize,
    dst: *mut u8,
    dst_len: usize,
    op: usize,
) -> Option<(usize, usize)> {
    let tag = src[ip];
    let (size, len, offset) = match tag & 3 {
        1 => {
            let offset_low = *src.get(ip + 1)?;
            (
                2,
                4 + usize::from((tag >> 2) & 7),
                (usize::from(tag & 0xe0) << 3) | usize::from(offset_low),
            )
        }
        2 => {
            let bytes = src.get(ip + 1..ip + 3)?;
            (
                3,
                1 + usize::from(tag >> 2),
                usize::from(u16::from_le_bytes([bytes[0], bytes[1]])),
            )
        }
        _ => return None,
    };
    // `fill_periodic` reads 8 bytes before the run for short periods.
    if offset == 0 || offset > op || offset > MAX_RUN_PERIOD || (offset < 8 && op < 8) {
        return None;
    }
    // Leave room for the overshoot of `fill_periodic`.
    let max_count = (dst_len - op).saturating_sub(MAX_RUN_PERIOD) / len;
    let element = &src[ip..ip + size];
    let mut count = 1;
    let mut next = ip + size;
    while count < max_count && src.get(next..next + size) == Some(element) {
        count += 1;
        next += size;
    }
    if count < 2 {
        return None;
    }
    let total = count * len;
    // SAFETY: `1 <= offset <= op` and the room for the fill's overshoot was
    // checked above.
    unsafe { fill_periodic(dst.add(op), offset, total) };
    Some((next, op + total))
}

/// Writes `total` bytes at `start` that repeat the `offset` bytes before it,
/// possibly writing up to `MAX_RUN_PERIOD` more bytes past the end.
///
/// The output is written in blocks copied from a source that is complete
/// before the first block is stored, so no load depends on a recent store.
///
/// # Safety
///
/// `1 <= offset <= MAX_RUN_PERIOD`, `max(offset, 8)` bytes before `start`
/// initialized and `start[..total + MAX_RUN_PERIOD]` valid for writes.
#[inline(always)]
unsafe fn fill_periodic(start: *mut u8, offset: usize, total: usize) {
    debug_assert!((1..=MAX_RUN_PERIOD).contains(&offset));
    unsafe {
        let (source, block, mut pos) = if offset >= 16 {
            (start.sub(offset).cast_const(), offset, 0)
        } else {
            // Write a first block of whole periods, then copy that.
            let block = offset * 48usize.div_ceil(offset);
            copy_overlapping(start, offset, block);
            (start.cast_const(), block, block)
        };
        while pos < total {
            let to = start.add(pos);
            let mut i = 0;
            while i + 16 < block {
                ptr::copy_nonoverlapping(source.add(i), to.add(i), 16);
                i += 16;
            }
            ptr::copy_nonoverlapping(source.add(block - 16), to.add(block - 16), 16);
            pos += block;
        }
    }
}

/// Decodes the single tag at `src[ip]` with full bounds checks.
///
/// # Safety
///
/// `ip < src.len()`, `op <= dst_len`, `dst` must be valid for `dst_len`
/// writes and `dst[..op]` must be initialized.
#[inline(always)]
unsafe fn decode_one(
    src: &[u8],
    mut ip: usize,
    dst: *mut u8,
    dst_len: usize,
    op: usize,
) -> Result<(usize, usize)> {
    let src_len = src.len();
    let src_ptr = src.as_ptr();
    let tag = src[ip];
    ip += 1;
    let dst_remaining = dst_len - op;

    if tag & 3 == 0 {
        let mut len = usize::from(tag >> 2) + 1;
        if len > 60 {
            let len_bytes = len - 60;
            if len_bytes > src_len - ip {
                return decode_error(Error::LiteralRead {
                    len: len_bytes as u64,
                    remaining: (src_len - ip) as u64,
                });
            }
            let mut value = 0u64;
            for (i, &byte) in src[ip..ip + len_bytes].iter().enumerate() {
                value |= u64::from(byte) << (8 * i);
            }
            ip += len_bytes;
            let len64 = value + 1;
            if len64 > (src_len - ip) as u64 {
                return decode_error(Error::LiteralRead {
                    len: len64,
                    remaining: (src_len - ip) as u64,
                });
            }
            if len64 > dst_remaining as u64 {
                return decode_error(Error::LiteralWrite {
                    len: len64,
                    remaining: dst_remaining as u64,
                });
            }
            len = len64 as usize;
        } else {
            if len > src_len - ip {
                return decode_error(Error::LiteralRead {
                    len: len as u64,
                    remaining: (src_len - ip) as u64,
                });
            }
            if len > dst_remaining {
                return decode_error(Error::LiteralWrite {
                    len: len as u64,
                    remaining: dst_remaining as u64,
                });
            }
        }
        // SAFETY: both ranges were bounds checked above; the wide path only
        // runs when there is room for the overshoot on both sides.
        unsafe {
            let from = src_ptr.add(ip);
            let to = dst.add(op);
            if len <= 16 && src_len - ip >= 16 && dst_remaining >= 16 {
                ptr::copy_nonoverlapping(from, to, 16);
            } else if len <= 64 {
                copy_short_nonoverlapping(from, to, len);
            } else {
                ptr::copy_nonoverlapping(from, to, len);
            }
        }
        return Ok((ip + len, op + len));
    }

    let (len, offset) = match tag & 3 {
        1 => {
            if ip >= src_len {
                return decode_error(Error::CopyRead {
                    len: 1,
                    remaining: 0,
                });
            }
            let offset = (usize::from(tag & 0xe0) << 3) | usize::from(src[ip]);
            ip += 1;
            (4 + usize::from((tag >> 2) & 7), offset)
        }
        2 => {
            if 2 > src_len - ip {
                return decode_error(Error::CopyRead {
                    len: 2,
                    remaining: (src_len - ip) as u64,
                });
            }
            let offset = usize::from(u16::from_le_bytes([src[ip], src[ip + 1]]));
            ip += 2;
            (1 + usize::from(tag >> 2), offset)
        }
        _ => {
            if 4 > src_len - ip {
                return decode_error(Error::CopyRead {
                    len: 4,
                    remaining: (src_len - ip) as u64,
                });
            }
            let offset =
                u32::from_le_bytes([src[ip], src[ip + 1], src[ip + 2], src[ip + 3]])
                    as usize;
            ip += 4;
            (1 + usize::from(tag >> 2), offset)
        }
    };
    if offset == 0 || offset > op {
        return decode_error(Error::Offset {
            offset,
            dst_pos: op,
        });
    }
    if len > dst_remaining {
        return decode_error(Error::CopyWrite {
            len: len as u64,
            remaining: dst_remaining as u64,
        });
    }
    // SAFETY: `1 <= offset <= op` and `op + len <= dst_len` were checked.
    unsafe { copy_match(dst, op, offset, len, dst_remaining) };
    Ok((ip, op + len))
}

/// Copies `len <= 64` bytes from `offset` bytes before `dst[op]`.
///
/// # Safety
///
/// `1 <= offset <= op`, `len <= dst_remaining`, `dst[..op]` initialized and
/// `dst[op..op + dst_remaining]` valid for writes.
#[inline(always)]
unsafe fn copy_match(
    dst: *mut u8,
    op: usize,
    offset: usize,
    len: usize,
    dst_remaining: usize,
) {
    debug_assert!(len <= 64);
    unsafe {
        let to = dst.add(op);
        let from = to.sub(offset);
        if offset >= len {
            if dst_remaining >= 64 {
                ptr::copy(from, to, 64);
            } else {
                copy_short_nonoverlapping(from, to, len);
            }
        } else if offset < 8 {
            copy_short_pattern(to, offset, len, op);
        } else if dst_remaining >= TAG_WRITE_SLOP {
            copy_overlapping(to, offset, len);
        } else {
            for i in 0..len {
                *to.add(i) = *from.add(i);
            }
        }
    }
}

/// Writes exactly `len` bytes repeating the `offset < 8` bytes before `dst`.
///
/// # Safety
///
/// `1 <= offset <= op`, `offset < len <= 64`, the `op` bytes before `dst`
/// initialized and `dst[..len]` valid for writes.
#[inline(always)]
unsafe fn copy_short_pattern(dst: *mut u8, offset: usize, len: usize, op: usize) {
    debug_assert!((1..8).contains(&offset) && offset < len && len <= 64 && offset <= op);
    unsafe {
        // The low `offset` bytes of `seed` are the pattern.
        let seed = if op >= 8 {
            read_u64_le(dst.sub(8)) >> (8 * (8 - offset))
        } else {
            let mut seed = 0u64;
            for i in 0..offset {
                seed |= u64::from(*dst.sub(offset).add(i)) << (8 * i);
            }
            seed
        };
        let pattern = seed.wrapping_mul(REPEAT_MUL[offset]);
        let stride = REPEAT_STRIDE[offset];
        let mut i = 0;
        while i + 8 <= len {
            write_u64_le(dst.add(i), pattern);
            i += stride;
        }
        // `i` is a multiple of the offset, so the tail starts the pattern
        // over.
        let tail = len - i;
        let to = dst.add(i);
        if tail >= 4 {
            ptr::write_unaligned(to.cast::<u32>(), (pattern as u32).to_le());
            ptr::write_unaligned(
                to.add(tail - 4).cast::<u32>(),
                ((pattern >> (8 * (tail - 4))) as u32).to_le(),
            );
        } else if tail >= 2 {
            ptr::write_unaligned(to.cast::<u16>(), (pattern as u16).to_le());
            ptr::write_unaligned(
                to.add(tail - 2).cast::<u16>(),
                ((pattern >> (8 * (tail - 2))) as u16).to_le(),
            );
        } else if tail == 1 {
            *to = pattern as u8;
        }
    }
}

/// Copies exactly `len <= 64` bytes between non-overlapping ranges.
#[inline(always)]
unsafe fn copy_short_nonoverlapping(from: *const u8, to: *mut u8, len: usize) {
    debug_assert!(len <= 64);
    unsafe {
        if len >= 16 {
            let mut i = 0;
            while i + 16 < len {
                ptr::copy_nonoverlapping(from.add(i), to.add(i), 16);
                i += 16;
            }
            ptr::copy_nonoverlapping(from.add(len - 16), to.add(len - 16), 16);
        } else if len >= 8 {
            ptr::copy_nonoverlapping(from, to, 8);
            ptr::copy_nonoverlapping(from.add(len - 8), to.add(len - 8), 8);
        } else if len >= 4 {
            ptr::copy_nonoverlapping(from, to, 4);
            ptr::copy_nonoverlapping(from.add(len - 4), to.add(len - 4), 4);
        } else if len > 0 {
            *to = *from;
            *to.add(len / 2) = *from.add(len / 2);
            *to.add(len - 1) = *from.add(len - 1);
        }
    }
}

/// `sum(1 << (8 * offset * k))` over the copies of an `offset`-byte pattern
/// that fit in 8 bytes, indexed by offset.
static REPEAT_MUL: [u64; 8] = {
    let mut table = [0u64; 8];
    let mut offset = 1;
    while offset < 8 {
        let mut shift = 0;
        while shift < 8 {
            table[offset] |= 1 << (8 * shift);
            shift += offset;
        }
        offset += 1;
    }
    table
};

/// Largest multiple of the offset that is at most 8, indexed by offset.
static REPEAT_STRIDE: [usize; 8] = [8, 8, 8, 6, 8, 5, 6, 7];

/// Writes `len` bytes repeating the `offset` bytes before `dst`, in whole
/// periods: up to 95 bytes in total (for an offset just below 64).
///
/// Every source byte is loaded from before `dst` before anything is stored,
/// so the copy neither reads stale output nor stalls on store forwarding.
///
/// # Safety
///
/// `1 <= offset < len <= 64`; `max(offset, 8)` bytes before `dst` must be
/// initialized and `TAG_WRITE_SLOP` bytes from `dst` valid for writes.
#[inline(always)]
unsafe fn copy_overlapping(dst: *mut u8, offset: usize, len: usize) {
    debug_assert!(offset >= 1 && offset < len && len <= 64);
    unsafe {
        if offset < 8 {
            // Build eight bytes of the pattern from the last eight output
            // bytes, then store it at a stride that keeps the phase.
            let tail = read_u64_le(dst.sub(8));
            let pattern = (tail >> (8 * (8 - offset))).wrapping_mul(REPEAT_MUL[offset]);
            let stride = REPEAT_STRIDE[offset];
            let mut i = 0;
            loop {
                write_u64_le(dst.add(i), pattern);
                i += stride;
                if i >= len {
                    break;
                }
            }
        } else if offset < 16 {
            let head = read_u64_le(dst.sub(offset));
            let tail = read_u64_le(dst.sub(8));
            let mut i = 0;
            loop {
                write_u64_le(dst.add(i), head);
                write_u64_le(dst.add(i + offset - 8), tail);
                i += offset;
                if i >= len {
                    break;
                }
            }
        } else if offset < 32 {
            let head = read_16(dst.sub(offset));
            let tail = read_16(dst.sub(16));
            let mut i = 0;
            loop {
                write_16(dst.add(i), head);
                write_16(dst.add(i + offset - 16), tail);
                i += offset;
                if i >= len {
                    break;
                }
            }
        } else {
            // 32 <= offset < len <= 64: two periods at most.
            let head = read_32(dst.sub(offset));
            let tail = read_32(dst.sub(32));
            write_32(dst, head);
            write_32(dst.add(offset - 32), tail);
            write_32(dst.add(offset), head);
        }
    }
}

#[inline(always)]
unsafe fn read_u32_le(ptr: *const u8) -> u32 {
    u32::from_le(unsafe { ptr::read_unaligned(ptr.cast::<u32>()) })
}

#[inline(always)]
unsafe fn read_u64_le(ptr: *const u8) -> u64 {
    u64::from_le(unsafe { ptr::read_unaligned(ptr.cast::<u64>()) })
}

#[inline(always)]
unsafe fn write_u64_le(ptr: *mut u8, value: u64) {
    unsafe { ptr::write_unaligned(ptr.cast::<u64>(), value.to_le()) }
}

#[inline(always)]
unsafe fn read_16(ptr: *const u8) -> [u8; 16] {
    unsafe { ptr::read_unaligned(ptr.cast::<[u8; 16]>()) }
}

#[inline(always)]
unsafe fn write_16(ptr: *mut u8, value: [u8; 16]) {
    unsafe { ptr::write_unaligned(ptr.cast::<[u8; 16]>(), value) }
}

#[inline(always)]
unsafe fn read_32(ptr: *const u8) -> [u8; 32] {
    unsafe { ptr::read_unaligned(ptr.cast::<[u8; 32]>()) }
}

#[inline(always)]
unsafe fn write_32(ptr: *mut u8, value: [u8; 32]) {
    unsafe { ptr::write_unaligned(ptr.cast::<[u8; 32]>(), value) }
}
