//! Raw Snappy block decoding.
//!
//! The decoder has two loops:
//!
//! * [`decode_fast`] handles the common tags (literals of up to 60 bytes and
//!   copies with 1- or 2-byte offsets) without branching on the tag type, and
//!   does each element's copy while decoding the next one. It needs enough
//!   input and output slack that every tag can over-read and over-write by a
//!   fixed amount, so it stops short of both buffer ends; variants that need
//!   less slack (one tag per limit check, then only short elements) take
//!   over closer to the ends.
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

use crate::select::{opaque, select_if_zero, select_ptr_if_zero};
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
/// The fast loop decoding `unroll` tags per limit check runs while
/// `ip < src.len() - input_margin(unroll, wide)`; see [`decode_fast`] for
/// `wide`.
const fn input_margin(unroll: usize, wide: bool) -> usize {
    if wide {
        (unroll - 1) * 61 + TAG_READ_SLOP + 4
    } else {
        // The tag, a literal of up to `FAST_COPY` bytes and the next tag.
        1 + FAST_COPY + 1 + 4
    }
}

/// The fast loop decoding `unroll` tags per limit check runs while
/// `op < dst.len() - output_margin(unroll, wide)`.
const fn output_margin(unroll: usize, wide: bool) -> usize {
    if wide {
        (unroll - 1) * 64 + TAG_WRITE_SLOP
    } else {
        FAST_COPY
    }
}

/// Why [`decode_fast`] returned.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// It reached one of its limits.
    Limit,
    /// The next tag needs the careful path.
    Tag,
    /// The next tag starts a run of identical copies (see [`decode_run`]).
    Run,
}

/// Lookup tables for the fast loop, indexed by tag and kept together so that
/// one base register addresses both. The byte table comes first: on aarch64 a
/// load can add an index register or an offset to the base, not both.
#[repr(C, align(64))]
struct Tables {
    /// Total encoded size of the element; only meaningful for fast-loop tags.
    advance: [u8; 256],
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
    len_minus_offset: [i16; 256],
}

/// Mask selecting the offset bytes that follow a tag, by tag type. Kept apart
/// from (and smaller than) a per-tag table, as in the C++ library, which
/// measured slightly faster.
static OFFSET_MASK: [u32; 4] = [0, 0xff, 0xffff, 0];

static TABLES: Tables = build_tables();

const fn build_tables() -> Tables {
    let mut tables = Tables {
        advance: [1; 256],
        len_minus_offset: [0; 256],
    };
    let mut tag = 0usize;
    while tag < 256 {
        let data = (tag >> 2) as i16;
        let (len, offset_high, advance) = match tag & 3 {
            0 if data < 60 => (data + 1, 1, data + 2),
            1 => ((data & 7) + 4, data >> 3, 2),
            2 => (data + 1, 0, 3),
            _ => (0xff, 0, 1),
        };
        tables.len_minus_offset[tag] = len - (offset_high << 8);
        tables.advance[tag] = advance as u8;
        tag += 1;
    }
    tables
}

/// Marks the enclosing branch as unlikely for block layout.
#[cold]
#[inline(always)]
fn cold() {}

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
///
/// The fast loop decodes two tags per limit check. Closer to the ends of the
/// buffers it decodes one tag per check, and closer still only elements of
/// up to `FAST_COPY` bytes that do not overlap their output. Each needs
/// smaller margins than the one before, so fewer tags are left to the
/// careful path, which matters for small blocks.
#[inline(never)]
fn decode_in_place(src: &[u8], dst: &mut [MaybeUninit<u8>]) -> Result<()> {
    let src_len = src.len();
    let dst_len = dst.len();
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr().cast::<u8>();
    let limits = |unroll, wide| {
        (
            src_len.saturating_sub(input_margin(unroll, wide)),
            dst_len.saturating_sub(output_margin(unroll, wide)),
        )
    };
    let (ip_limit_2, op_limit_2) = limits(2, true);
    let (ip_limit_1, op_limit_1) = limits(1, true);
    let (ip_limit_n, op_limit_n) = limits(1, false);
    let mut ip = 0usize;
    let mut op = 0usize;

    if ip_limit_2 == 0 || op_limit_2 == 0 {
        // The loops for the ends of the buffers only pay for the cost of
        // entering them after the main one has run, so blocks too small for
        // that are left to the careful path entirely.
        while ip < src_len {
            // SAFETY: `ip < src_len`, `op <= dst_len` and `dst_ptr` is valid
            // for `dst_len` writes.
            (ip, op) = unsafe { decode_one(src, ip, dst_ptr, dst_len, op)? };
        }
    }
    while ip < src_len {
        // SAFETY: (for all three calls) the limits leave the margins of the
        // respective loop after the starting positions.
        let fast = if ip < ip_limit_2 && op < op_limit_2 {
            Some(unsafe {
                decode_fast::<2, true>(src_ptr, ip, ip_limit_2, dst_ptr, op, op_limit_2)
            })
        } else if ip < ip_limit_1 && op < op_limit_1 {
            Some(unsafe {
                decode_fast::<1, true>(src_ptr, ip, ip_limit_1, dst_ptr, op, op_limit_1)
            })
        } else if ip < ip_limit_n && op < op_limit_n {
            Some(unsafe {
                decode_fast::<1, false>(src_ptr, ip, ip_limit_n, dst_ptr, op, op_limit_n)
            })
        } else {
            None
        };
        if let Some((next_ip, next_op, stop)) = fast {
            (ip, op) = (next_ip, next_op);
            match stop {
                Stop::Limit => continue,
                Stop::Tag => {}
                Stop::Run => {
                    // SAFETY: `ip < src_len`, `op <= dst_len` and `dst_ptr`
                    // is valid for `dst_len` writes.
                    if let Some(next) =
                        unsafe { decode_run(src, ip, dst_ptr, dst_len, op) }
                    {
                        (ip, op) = next;
                        continue;
                    }
                }
            }
        }
        // SAFETY: `ip < src_len`, `op <= dst_len` and `dst_ptr` is valid for
        // `dst_len` writes.
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

/// Decodes tags, `UNROLL` (1 or 2) per limit check, until `ip >= ip_limit`,
/// `op >= op_limit` or a tag that needs the careful path. Returns the
/// position of the first undecoded tag, the corresponding output position,
/// and why it stopped.
///
/// Unless `WIDE` is set, only elements of up to `FAST_COPY` bytes that do
/// not overlap their own output are decoded, which keeps the margins small.
///
/// # Safety
///
/// `src` must be valid for reads of `ip_limit + input_margin(UNROLL, WIDE)`
/// bytes and `dst` for writes of `op_limit + output_margin(UNROLL, WIDE)`
/// bytes; `dst[..op]` must be initialized.
#[inline(never)]
unsafe fn decode_fast<const UNROLL: usize, const WIDE: bool>(
    src: *const u8,
    ip: usize,
    ip_limit: usize,
    dst: *mut u8,
    op: usize,
    op_limit: usize,
) -> (usize, usize, Stop) {
    const { assert!(UNROLL == 1 || (UNROLL == 2 && WIDE)) };
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
    let mut stop = Stop::Tag;
    // Each element's copy is deferred to the next step: `pending_len` bytes
    // from `pending_from` go to `out`, and the element being decoded starts
    // after them. Doing the copy (and its rarely taken branch for more than
    // `FAST_COPY` bytes) after the next element's tag has been worked out
    // means that a mispredicted branch there does not throw away work on
    // the tag-to-tag dependency chain. `NO_COPY` stands in when nothing is
    // pending.
    let mut pending_from = NO_COPY.as_ptr();
    let mut pending_len = 0usize;

    // One element. `break $exit` leaves the loop with `ip` just past the tag
    // of the first element not decoded and the pending copy not yet done.
    macro_rules! step {
        ($exit:lifetime) => {{
            debug_assert!(tag < 256);
            let tag_type = tag & 3;

            // Load both candidates for the next tag before selecting, which
            // keeps the select off the load-to-load dependency chain. The
            // volatile reads stop LLVM from turning this into one load from
            // a selected address.
            // SAFETY: both are within `TAG_READ_SLOP` of the tag, and in the
            // narrow loop within `FAST_COPY + 2`: longer literals are left
            // to the careful path, so their next tag is not needed.
            let literal_len = if WIDE {
                (tag >> 2) + 1
            } else {
                ((tag >> 2) + 1).min(FAST_COPY + 1)
            };
            let next_literal =
                usize::from(unsafe { ptr::read_volatile(ip.add(literal_len)) });
            let next_copy = usize::from(unsafe { ptr::read_volatile(ip.add(tag_type)) });
            let next_tag = select_if_zero(tag_type, next_literal, next_copy);

            // SAFETY: `tag` is a byte (`select_if_zero` hides that from
            // LLVM), and the reads are within `TAG_READ_SLOP` of the tag. The
            // advance is only used once the tag is known to be a fast-loop
            // tag. The narrow loop's margin does not cover the advance of a
            // literal it leaves to the careful path, hence the wrapping add.
            let (entry, trailer, next_ip) = unsafe {
                let advance = usize::from(*tables.advance.get_unchecked(tag));
                (
                    isize::from(*tables.len_minus_offset.get_unchecked(tag)),
                    (read_u32_le(ip) & *OFFSET_MASK.get_unchecked(tag_type)) as usize,
                    if WIDE {
                        ip.add(advance)
                    } else {
                        ip.wrapping_add(advance)
                    },
                )
            };
            // Without `opaque` LLVM keeps `entry` alive to test its low byte
            // on the cold path, which costs a register copy per tag.
            let len = opaque((entry & 0xff) as usize);
            // SAFETY: `entry` is in `-1792..=255` and `trailer` in
            // `0..=0xffff`, so this cannot overflow; saying so lets LLVM
            // branch on the flags of the subtraction.
            let len_minus_offset = unsafe { entry.unchecked_sub(trailer as isize) };
            // SAFETY: the pending copy is at most 64 bytes, within the
            // output margin.
            let element_out = unsafe { out.add(pending_len) };
            // `element_out - offset`, which is before `dst` if the offset
            // reaches before the start of the output. Literals have a
            // spurious offset of 256 and copy from the input instead.
            let match_from = element_out
                .cast_const()
                .wrapping_offset(len_minus_offset - len as isize);
            if !WIDE && (len > FAST_COPY || len_minus_offset > 0) {
                break $exit;
            }
            if len_minus_offset > 0 {
                cold();
                if len & 0x80 != 0 {
                    // Long literal or 4-byte offset copy.
                    break $exit;
                }
                // Overlapping copy, or a copy with offset zero.
                let offset = (len as isize - len_minus_offset) as usize;
                // SAFETY: `element_out` was derived from `dst`.
                let op = unsafe { element_out.offset_from(dst) } as usize;
                if offset == 0 || offset > op || (offset < 8 && op < 8) {
                    break $exit;
                }
                // SAFETY: the next element is within the input margin.
                if unsafe { next_is_same(ip, next_ip, tag_type) } {
                    // A run of identical copies; see `decode_run`.
                    stop = Stop::Run;
                    break $exit;
                }
                // The pattern comes from the bytes before this element. If
                // the pending element holds all of them, they are read from
                // its source, so that the pattern does not wait for the
                // pending copy's stores.
                // SAFETY: `1 <= offset < len <= 64`, `offset <= op` and the
                // output margin covers the pending copy and the pattern
                // overshoot. The pending source holds the same bytes as the
                // output it is copied to.
                unsafe {
                    let before = if pending_len >= offset.max(8) {
                        pending_from.add(pending_len)
                    } else {
                        element_out.cast_const()
                    };
                    copy_pending(pending_from, out, pending_len);
                    copy_overlapping(before, element_out, offset, len);
                    out = element_out.add(len);
                }
                pending_from = NO_COPY.as_ptr();
                pending_len = 0;
            } else {
                if (match_from as usize) < (dst as usize) {
                    cold();
                    if tag_type != 0 {
                        break $exit;
                    }
                }
                let from = select_ptr_if_zero(tag_type, ip, match_from);
                // SAFETY: the pending copy is within the margins (see
                // `copy_pending`).
                unsafe {
                    ptr::copy(pending_from, out, FAST_COPY);
                    if WIDE && pending_len > FAST_COPY {
                        cold();
                        // Only a copy-2 tag of 64 bytes (`0xfe`) is longer
                        // than any literal here. If this element repeats it,
                        // the two start a run of identical copies, as found
                        // in data with periods of 64 to `MAX_RUN_PERIOD`
                        // bytes; see `decode_run`, which takes over at the
                        // pending element.
                        if tag == 0xfe
                            && pending_len == 64
                            && out.addr() - pending_from.addr() <= MAX_RUN_PERIOD
                            && next_is_same(ip.sub(3), ip, 2)
                        {
                            stop = Stop::Run;
                            ip = ip.sub(3);
                            pending_from = NO_COPY.as_ptr();
                            pending_len = 0;
                            break $exit;
                        }
                        ptr::copy(
                            pending_from.add(FAST_COPY),
                            out.add(FAST_COPY),
                            64 - FAST_COPY,
                        );
                    }
                }
                out = element_out;
                pending_from = from;
                pending_len = len;
            }
            ip = next_ip;
            tag = next_tag;
        }};
    }

    'outer: loop {
        step!('outer);
        if UNROLL == 2 {
            step!('outer);
        }
        // SAFETY: as for `element_out`.
        if ip >= ip_limit || unsafe { out.add(pending_len) } >= out_limit {
            stop = Stop::Limit;
            break;
        }
    }
    // SAFETY: as in the loop.
    unsafe {
        copy_pending(pending_from, out, pending_len);
        out = out.add(pending_len);
    }

    // SAFETY: `ip` and `out` were derived from `src` and `dst`; `ip` points
    // past a tag.
    unsafe {
        (
            ip.offset_from(src) as usize - 1,
            out.offset_from(dst) as usize,
            stop,
        )
    }
}

/// Source of the fast loop's deferred copy when no element is pending.
static NO_COPY: [u8; FAST_COPY] = [0; FAST_COPY];

/// Copies the element deferred by the fast loop: `len <= 64` bytes from
/// `from` to `to`, writing `FAST_COPY` or 64 bytes.
///
/// # Safety
///
/// `from` is either a literal within the input margin, at least `len` bytes
/// before `to` in the output, or `NO_COPY` with `len == 0`; `to` has 64
/// bytes of room. `ptr::copy` (memmove semantics) copies bytes untyped, so
/// stale output bytes past `to` are never read as integers.
#[inline(always)]
unsafe fn copy_pending(from: *const u8, to: *mut u8, len: usize) {
    unsafe {
        ptr::copy(from, to, FAST_COPY);
        if len > FAST_COPY {
            cold();
            ptr::copy(from.add(FAST_COPY), to.add(FAST_COPY), 64 - FAST_COPY);
        }
    }
}

/// Longest period [`decode_run`] handles, which bounds how far
/// [`fill_periodic`] writes past the end of a run.
const MAX_RUN_PERIOD: usize = 112;

/// Whether the copy whose tag (of type `tag_type`) is at `ip - 1` is
/// followed by an identical one; `next_ip` is just past the next element's
/// tag.
///
/// The size is taken from the tag type rather than from `next_ip`, so that
/// the fast loop need not keep the element size around for this rare check.
///
/// # Safety
///
/// Both elements must be readable, plus 4 bytes past the second tag.
#[inline(always)]
unsafe fn next_is_same(ip: *const u8, next_ip: *const u8, tag_type: usize) -> bool {
    unsafe {
        let mask = if tag_type == 1 { 0xffff } else { 0xff_ffff };
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
            copy_overlapping(start, start, offset, block);
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
            copy_overlapping(to, to, offset, len);
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

/// Writes `len` bytes at `dst` repeating the `offset` bytes before it, in
/// whole periods: up to 95 bytes in total (for an offset just below 64).
/// The bytes before `dst` are read before `before` instead, which is `dst`
/// itself or a copy of the same bytes.
///
/// Every source byte is loaded before anything is stored, so the copy
/// neither reads stale output nor stalls on store forwarding.
///
/// # Safety
///
/// `1 <= offset < len <= 64`; `max(offset, 8)` bytes before `before` must be
/// initialized, equal to those before `dst`, and `TAG_WRITE_SLOP` bytes from
/// `dst` valid for writes.
#[inline(always)]
unsafe fn copy_overlapping(before: *const u8, dst: *mut u8, offset: usize, len: usize) {
    debug_assert!(offset >= 1 && offset < len && len <= 64);
    unsafe {
        if offset < 8 {
            // Build eight bytes of the pattern from the last eight output
            // bytes, then store it at a stride that keeps the phase.
            let tail = read_u64_le(before.sub(8));
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
            let head = read_u64_le(before.sub(offset));
            let tail = read_u64_le(before.sub(8));
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
            let head = read_16(before.sub(offset));
            let tail = read_16(before.sub(16));
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
            let head = read_32(before.sub(offset));
            let tail = read_32(before.sub(32));
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
