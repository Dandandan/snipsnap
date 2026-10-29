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

use crate::bytes::{
    copy_16_to_64, copy_exact, copy_short, read_32, read_u16, read_u32, read_u64,
    write_16, write_32, write_u16, write_u32, write_u64,
};
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
    /// Mask selecting the offset bytes that follow a tag, by tag type. Only
    /// x86-64 reads it (see [`OFFSET_MASKS`]).
    offset_mask: [u32; 4],
}

/// The masks of [`Tables::offset_mask`], 16 bits each, which other targets
/// shift into place instead of loading: on aarch64 a load cannot add both
/// the table's offset and an index to the base register.
const OFFSET_MASKS: u64 = 0xffff_00ff_0000;

static TABLES: Tables = build_tables();

const fn build_tables() -> Tables {
    let mut tables = Tables {
        advance: [1; 256],
        len_minus_offset: [0; 256],
        offset_mask: [0, 0xff, 0xffff, 0],
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
            copy_exact(literal.as_ptr(), dst.as_mut_ptr().cast::<u8>(), dst_len);
        }
        return Ok(());
    }
    if src.len() <= input_margin(2, true) || dst_len <= output_margin(2, true) {
        // The loops for the ends of the buffers only pay for the cost of
        // entering them after the main one has run, so blocks too small for
        // that are left to the careful path entirely.
        // SAFETY: `dst` is valid for `dst_len` writes, and nothing has been
        // decoded yet.
        return unsafe { decode_careful(src, 0, dst.as_mut_ptr().cast(), dst_len, 0) };
    }
    decode_in_place(src, dst)
}

/// Decodes the tags from `src[ip]` on with the careful path only, and checks
/// that they end the output.
///
/// # Safety
///
/// `op <= dst_len`, `dst` must be valid for `dst_len` writes and `dst[..op]`
/// must be initialized.
#[inline(never)]
unsafe fn decode_careful(
    src: &[u8],
    mut ip: usize,
    dst: *mut u8,
    dst_len: usize,
    mut op: usize,
) -> Result<()> {
    while ip < src.len() {
        // SAFETY: `ip < src.len()`; the rest as documented above, and kept
        // by `decode_one`.
        (ip, op) = unsafe { decode_one(src, ip, dst, dst_len, op)? };
    }
    if op != dst_len {
        return decode_error(Error::HeaderMismatch {
            expected: dst_len,
            actual: op,
        });
    }
    Ok(())
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
    while ip < src_len {
        // SAFETY: (for all three calls) the limits leave the margins of the
        // respective loop after the starting positions.
        let (ip_limit, op_limit) = if ip < ip_limit_2 && op < op_limit_2 {
            (ip, op) = unsafe {
                decode_fast::<2, true>(src_ptr, ip, ip_limit_2, dst_ptr, op, op_limit_2)
            };
            (ip_limit_2, op_limit_2)
        } else if ip < ip_limit_1 && op < op_limit_1 {
            (ip, op) = unsafe {
                decode_fast::<1, true>(src_ptr, ip, ip_limit_1, dst_ptr, op, op_limit_1)
            };
            (ip_limit_1, op_limit_1)
        } else if ip < ip_limit_n && op < op_limit_n {
            (ip, op) = unsafe {
                decode_fast::<1, false>(src_ptr, ip, ip_limit_n, dst_ptr, op, op_limit_n)
            };
            (ip_limit_n, op_limit_n)
        } else {
            // `ip` and `op` only grow, and the narrow loop has the largest
            // limits, so none of the loops applies again.
            break;
        };
        if ip >= ip_limit || op >= op_limit {
            // The loop reached its limits (or stopped at a tag past them,
            // which the next loop or the careful path decodes).
            continue;
        }
        // The loop stopped at a tag it does not decode, which may start a
        // run of identical copies.
        // SAFETY: `ip < src_len`, `op <= dst_len` and `dst_ptr` is valid for
        // `dst_len` writes.
        if let Some(next) = unsafe { decode_run(src, ip, dst_ptr, dst_len, op) } {
            (ip, op) = next;
            continue;
        }
        // SAFETY: as for `decode_run`.
        (ip, op) = unsafe { decode_one(src, ip, dst_ptr, dst_len, op)? };
    }
    // SAFETY: `op <= dst_len`, `dst_ptr` is valid for `dst_len` writes and
    // `dst[..op]` was decoded.
    unsafe { decode_careful(src, ip, dst_ptr, dst_len, op) }
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
) -> (usize, usize) {
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
    // On x86-64 one base register addresses both tables. Elsewhere this
    // one gets a register of its own, which saves adding its offset to the
    // base for every tag.
    let len_minus_offset: &[i16; 256] = if cfg!(target_arch = "x86_64") {
        &tables.len_minus_offset
    } else {
        // SAFETY: `opaque` returns the address it is given.
        unsafe {
            &*(opaque(tables.len_minus_offset.as_ptr() as usize) as *const [i16; 256])
        }
    };
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
            // SAFETY: both candidates for the next tag are within
            // `TAG_READ_SLOP` of the tag, and in the narrow loop within
            // `FAST_COPY + 2`: longer literals are left to the careful path,
            // so their next tag is not needed.
            let (tag_type, next_tag) = unsafe { next_tag::<WIDE>(ip, tag) };

            // SAFETY: `tag` is a byte (`next_tag` hides that from LLVM), and
            // the reads are within `TAG_READ_SLOP` of the tag. The advance is
            // only used once the tag is known to be a fast-loop tag. The
            // narrow loop's margin does not cover the advance of a literal it
            // leaves to the careful path, hence the wrapping add.
            let (entry, trailer, next_ip) = unsafe {
                let advance = usize::from(*tables.advance.get_unchecked(tag));
                (
                    isize::from(*len_minus_offset.get_unchecked(tag)),
                    if cfg!(target_arch = "x86_64") {
                        (read_u32(ip) & *tables.offset_mask.get_unchecked(tag_type))
                            as usize
                    } else {
                        let mask = OFFSET_MASKS >> (16 * tag_type);
                        (u64::from(read_u16(ip)) & mask) as usize
                    },
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
            // `entry` is in `-1792..=255` and `trailer` in `0..=0xffff`.
            // `opaque` keeps LLVM from subtracting twice, once for the
            // overlap test and once for `match_from`.
            let len_minus_offset = opaque((entry - trailer as isize) as usize) as isize;
            // SAFETY: the pending copy is at most 64 bytes, within the
            // output margin.
            let element_out = unsafe { out.add(pending_len) };
            // `element_out - offset`, which is before `dst` if the offset
            // reaches before the start of the output. Literals have a
            // spurious offset of 256 and copy from the input instead.
            let match_from = element_out
                .cast_const()
                .wrapping_offset(len_minus_offset - len as isize);
            let overlaps = len_minus_offset > 0;
            if !WIDE && (len > FAST_COPY || overlaps) {
                break $exit;
            }
            if overlaps {
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
                    // Literals have a spurious offset and get here near the
                    // start of the output. `black_box` keeps LLVM from
                    // testing the tag type first, on the hot path, where
                    // that branch would mispredict.
                    if std::hint::black_box(tag_type) != 0 {
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
                        align_32::<UNROLL>();
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
        align_32::<UNROLL>();
        step!('outer);
        if UNROLL == 2 {
            align_32::<UNROLL>();
            step!('outer);
        }
        align_32::<UNROLL>();
        // The output test comes first, which keeps both branches clear of
        // 32-byte boundaries.
        // SAFETY: as for `element_out`.
        if unsafe { out.add(pending_len) } >= out_limit || ip >= ip_limit {
            break;
        }
    }
    if UNROLL == 2 {
        align_function_64();
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
        )
    }
}

/// Returns the type of `tag` (at `ip - 1`) and the tag that follows its
/// element: at `ip + (tag >> 2) + 1` for a literal, at `ip + tag_type` for a
/// copy.
///
/// Both candidates are loaded before selecting, which keeps the select off
/// the load-to-load dependency chain. On x86-64 and aarch64 the select uses
/// the flags of `tag & 3`, as in the C++ library, which saves a compare per
/// tag.
///
/// # Safety
///
/// Both candidates must be readable. Unless `WIDE`, the literal candidate is
/// capped at `ip + FAST_COPY + 1`.
#[inline(always)]
unsafe fn next_tag<const WIDE: bool>(ip: *const u8, tag: usize) -> (usize, usize) {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    if WIDE {
        let (tag_type, next_tag);
        // SAFETY: the caller guarantees both loads are in bounds.
        unsafe {
            std::arch::asm!(
                "mov {t:e}, {tag:e}",
                "shr {l:e}, 2",
                "and {t:e}, 3",
                "movzx {n:e}, byte ptr [{ip} + {t}]",
                "movzx {l:e}, byte ptr [{ip} + {l} + 1]",
                "cmovz {n:e}, {l:e}",
                tag = in(reg) tag,
                ip = in(reg) ip,
                t = out(reg) tag_type,
                n = out(reg) next_tag,
                l = inout(reg) tag => _,
                options(pure, readonly, nostack),
            );
        }
        return (tag_type, next_tag);
    }
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    if WIDE {
        let (tag_type, next_tag);
        // SAFETY: the caller guarantees both loads are in bounds.
        unsafe {
            std::arch::asm!(
                "ands {t}, {tag}, #3",
                "add {l}, {ip}, {tag}, lsr #2",
                "ldrb {n:w}, [{ip}, {t}]",
                "ldrb {l:w}, [{l}, #1]",
                "csel {n}, {l}, {n}, eq",
                tag = in(reg) tag,
                ip = in(reg) ip,
                t = out(reg) tag_type,
                n = out(reg) next_tag,
                l = out(reg) _,
                options(pure, readonly, nostack),
            );
        }
        return (tag_type, next_tag);
    }
    let tag_type = tag & 3;
    let literal_len = if WIDE {
        (tag >> 2) + 1
    } else {
        ((tag >> 2) + 1).min(FAST_COPY + 1)
    };
    // The volatile reads stop LLVM from turning this into one load from a
    // selected address.
    // SAFETY: as documented above.
    let next_literal = usize::from(unsafe { ptr::read_volatile(ip.add(literal_len)) });
    let next_copy = usize::from(unsafe { ptr::read_volatile(ip.add(tag_type)) });
    (tag_type, select_if_zero(tag_type, next_literal, next_copy))
}

/// Aligns the start of the calling function to 64 bytes (on x86-64), so that
/// the layout of its loops does not depend on where the linker places it.
///
/// That layout matters on cores with Intel's JCC erratum (Skylake to Cascade
/// Lake and their client parts): a 32-byte block of code holding a jump that
/// crosses or ends on its boundary is not cached as decoded micro-ops, which
/// slowed the fast loop by 10-20% in unlucky placements. The directive raises
/// the alignment of the function's section; it also pads to 64 bytes where it
/// stands, so it belongs on a path that is taken once per call at most.
#[inline(always)]
fn align_function_64() {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    // SAFETY: an assembler directive; the padding is executed as nops.
    unsafe {
        std::arch::asm!(".p2align 6", options(nomem, nostack, preserves_flags));
    }
}

/// In the main fast loop (`UNROLL == 2`), aligns the next instruction to 32
/// bytes, which keeps its branches clear of 32-byte boundaries (see
/// [`align_function_64`]).
#[inline(always)]
fn align_32<const UNROLL: usize>() {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    if UNROLL == 2 {
        // SAFETY: an assembler directive; the padding is executed as nops.
        unsafe {
            std::arch::asm!(".p2align 5", options(nomem, nostack, preserves_flags));
        }
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

/// Longest period [`decode_run`] handles.
///
/// The fast loop hands it runs of overlapping copies, whose offsets are
/// below 64. Runs of 64-byte copies at longer periods are decoded as
/// separate copies: spotting them in the fast loop took a branch that, in
/// ClickBench's URLs and titles, mispredicted more often than the runs it
/// found saved.
const MAX_RUN_PERIOD: usize = 64;

/// How far [`fill_periodic`] may write past the end of a run: up to 63 bytes
/// for its last block, and up to 79 from the start of the run for its first.
const RUN_WRITE_SLOP: usize = 80;

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
        (read_u32(ip.sub(1)) ^ read_u32(next_ip.sub(1))) & mask == 0
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
            let &offset_low = src.get(ip + 1)?;
            (
                2,
                4 + usize::from((tag >> 2) & 7),
                (usize::from(tag & 0xe0) << 3) | usize::from(offset_low),
            )
        }
        2 => {
            let bytes = src.get(ip + 1..ip + 3)?.try_into().ok()?;
            (
                3,
                1 + usize::from(tag >> 2),
                usize::from(u16::from_le_bytes(bytes)),
            )
        }
        _ => return None,
    };
    // `fill_periodic` reads 8 bytes before the run for short periods.
    if offset == 0 || offset > op || offset > MAX_RUN_PERIOD || (offset < 8 && op < 8) {
        return None;
    }
    // Leave room for the overshoot of `fill_periodic`.
    let max_count = (dst_len - op).saturating_sub(RUN_WRITE_SLOP) / len;
    // Elements are compared as masked four-byte words, which compiles to a
    // load and a compare rather than a call to `bcmp`. Elements in the last
    // three bytes of the input are left to the careful path.
    let mask = if size == 2 { 0xffff } else { 0xff_ffff };
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(src.get(at..at + 4)?.try_into().ok()?) & mask)
    };
    let value = word(ip)?;
    let mut count = 1;
    let mut next = ip + size;
    while count < max_count && word(next) == Some(value) {
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
/// possibly writing up to `RUN_WRITE_SLOP` more bytes past the end.
///
/// The output is written in blocks copied from a source that is complete
/// before the first block is stored, so no load depends on a recent store.
///
/// # Safety
///
/// `1 <= offset <= MAX_RUN_PERIOD`, `max(offset, 8)` bytes before `start`
/// initialized and `start[..total + RUN_WRITE_SLOP]` valid for writes.
#[inline(always)]
unsafe fn fill_periodic(start: *mut u8, offset: usize, total: usize) {
    debug_assert!((1..=MAX_RUN_PERIOD).contains(&offset));
    unsafe {
        // The periods are first written out as a block of whole periods, at
        // most 64 bytes, which is then copied. Larger blocks measured slower.
        let block = offset * (64 / offset);
        if offset >= 16 {
            let mut pos = 0;
            while pos < block {
                copy_16_to_64(start.sub(offset), start.add(pos), offset);
                pos += offset;
            }
        } else {
            copy_overlapping(start, start, offset, block);
        }
        let mut pos = block;
        while pos < total {
            copy_16_to_64(start, start.add(pos), block);
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
        let mut len = u64::from(tag >> 2) + 1;
        if len > 60 {
            // The length less one follows in `len - 60` little-endian bytes.
            let len_bytes = (len - 60) as usize;
            let Some(bytes) = src.get(ip..ip + len_bytes) else {
                return decode_error(Error::LiteralRead {
                    len: len_bytes as u64,
                    remaining: (src_len - ip) as u64,
                });
            };
            len = bytes
                .iter()
                .rev()
                .fold(0, |value, &byte| value << 8 | u64::from(byte))
                + 1;
            ip += len_bytes;
        }
        if len > (src_len - ip) as u64 {
            return decode_error(Error::LiteralRead {
                len,
                remaining: (src_len - ip) as u64,
            });
        }
        if len > dst_remaining as u64 {
            return decode_error(Error::LiteralWrite {
                len,
                remaining: dst_remaining as u64,
            });
        }
        let len = len as usize;
        // SAFETY: both ranges were bounds checked above; the wide path only
        // runs when there is room for the overshoot on both sides.
        unsafe {
            let from = src_ptr.add(ip);
            let to = dst.add(op);
            if len <= 16 && src_len - ip >= 16 && dst_remaining >= 16 {
                ptr::copy_nonoverlapping(from, to, 16);
            } else {
                copy_exact(from, to, len);
            }
        }
        return Ok((ip + len, op + len));
    }

    let truncated = |len: u64| {
        decode_error(Error::CopyRead {
            len,
            remaining: (src_len - ip) as u64,
        })
    };
    let (len, offset, size) = match tag & 3 {
        1 => {
            let Some(&low) = src.get(ip) else {
                return truncated(1);
            };
            let offset = (usize::from(tag & 0xe0) << 3) | usize::from(low);
            (4 + usize::from((tag >> 2) & 7), offset, 1)
        }
        2 => {
            let Some(&[low, high]) = src.get(ip..ip + 2) else {
                return truncated(2);
            };
            let offset = usize::from(u16::from_le_bytes([low, high]));
            (1 + usize::from(tag >> 2), offset, 2)
        }
        _ => {
            let Some(&[b0, b1, b2, b3]) = src.get(ip..ip + 4) else {
                return truncated(4);
            };
            let offset = u32::from_le_bytes([b0, b1, b2, b3]) as usize;
            (1 + usize::from(tag >> 2), offset, 4)
        }
    };
    ip += size;
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
                copy_short(from, to, len);
            }
        } else if offset <= 8 {
            copy_short_pattern(to, offset, len, op);
        } else if dst_remaining >= TAG_WRITE_SLOP {
            copy_overlapping(to, to, offset, len);
        } else {
            // Exactly: whole periods, then part of one, each copied from
            // the `offset > 8` bytes before `to`, which are not written.
            let mut i = 0;
            while len - i > offset {
                copy_short(from, to.add(i), offset);
                i += offset;
            }
            copy_short(from, to.add(i), len - i);
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
    debug_assert!((1..=8).contains(&offset) && offset < len && len <= 64 && offset <= op);
    unsafe {
        let pattern = if op >= 8 {
            pattern_8(dst, offset)
        } else {
            // Only the `offset` bytes before `dst` are initialized: read
            // them in two pieces that may overlap.
            let from = dst.sub(offset);
            let seed = if offset >= 4 {
                u64::from(read_u32(from))
                    | u64::from(read_u32(dst.sub(4))) << (8 * (offset - 4))
            } else {
                u64::from(*from)
                    | u64::from(*from.add(offset / 2)) << (8 * (offset / 2))
                    | u64::from(*from.add(offset - 1)) << (8 * (offset - 1))
            };
            seed.wrapping_mul(REPEAT_MUL[offset])
        };
        if offset.is_power_of_two() {
            fill_period_8(dst, pattern, offset, len);
            return;
        }
        let stride = REPEAT_STRIDE[offset];
        let mut i = 0;
        while i + 8 <= len {
            write_u64(dst.add(i), pattern);
            i += stride;
        }
        // `i` is a multiple of the offset, so the tail starts the pattern
        // over.
        let tail = len - i;
        let to = dst.add(i);
        if tail >= 4 {
            write_u32(to, pattern as u32);
            write_u32(to.add(tail - 4), (pattern >> (8 * (tail - 4))) as u32);
        } else if tail >= 2 {
            write_u16(to, pattern as u16);
            write_u16(to.add(tail - 2), (pattern >> (8 * (tail - 2))) as u16);
        } else if tail == 1 {
            *to = pattern as u8;
        }
    }
}

/// Writes exactly `len` bytes at `dst` repeating `pattern`, eight bytes with
/// a period of `offset`, which divides 8, from the pattern's first byte.
///
/// Any eight bytes that start a whole number of periods in hold `pattern`,
/// and those that end at `len` hold it rotated by `len % offset` bytes, so
/// a few stores that may overlap write it without a loop.
///
/// # Safety
///
/// `offset` is 1, 2, 4 or 8, `offset < len <= 64`, and `dst[..len]` must be
/// valid for writes.
#[inline(always)]
unsafe fn fill_period_8(dst: *mut u8, pattern: u64, offset: usize, len: usize) {
    debug_assert!(offset.is_power_of_two() && offset <= 8 && offset < len && len <= 64);
    let end = pattern.rotate_right(8 * (len & (offset - 1)) as u32);
    let pair = |word: u64| {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&word.to_le_bytes());
        bytes[8..].copy_from_slice(&word.to_le_bytes());
        bytes
    };
    unsafe {
        if len >= 16 {
            let (head, end) = (pair(pattern), pair(end));
            write_16(dst, head);
            if len > 32 {
                write_16(dst.add(16), head);
                write_16(dst.add(len - 32), end);
            }
            write_16(dst.add(len - 16), end);
        } else if len >= 8 {
            write_u64(dst, pattern);
            write_u64(dst.add(len - 8), end);
        } else if len >= 4 {
            write_u32(dst, pattern as u32);
            write_u32(dst.add(len - 4), (end >> 32) as u32);
        } else {
            write_u16(dst, pattern as u16);
            write_u16(dst.add(len - 2), (end >> 48) as u16);
        }
    }
}

/// `sum(1 << (8 * offset * k))` over the copies of an `offset`-byte pattern
/// that fit in 8 bytes, indexed by offset.
static REPEAT_MUL: [u64; 9] = {
    let mut table = [0u64; 9];
    let mut offset = 1;
    while offset <= 8 {
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
static REPEAT_STRIDE: [usize; 9] = [8, 8, 8, 6, 8, 5, 6, 7, 8];

/// Eight bytes of the pattern that repeats the `offset <= 8` bytes before
/// `end`, starting with the first of them.
///
/// # Safety
///
/// The 8 bytes before `end` must be readable and initialized.
#[inline(always)]
unsafe fn pattern_8(end: *const u8, offset: usize) -> u64 {
    debug_assert!((1..=8).contains(&offset));
    let tail = unsafe { read_u64(end.sub(8)) };
    (tail >> (8 * (8 - offset))).wrapping_mul(REPEAT_MUL[offset])
}

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
            // bytes, then store it at a stride that keeps the phase. The
            // first four stores, at least 20 bytes, are unconditional: in
            // integer columns most such copies need one or two, and a loop
            // exit would mispredict between them.
            let pattern = pattern_8(before, offset);
            let stride = REPEAT_STRIDE[offset];
            write_u64(dst, pattern);
            write_u64(dst.add(stride), pattern);
            write_u64(dst.add(2 * stride), pattern);
            write_u64(dst.add(3 * stride), pattern);
            let mut i = 4 * stride;
            while i < len {
                write_u64(dst.add(i), pattern);
                i += stride;
            }
        } else if offset < 16 {
            copy_periods::<8>(before, dst, offset, len);
        } else if offset < 32 {
            copy_periods::<16>(before, dst, offset, len);
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

/// Writes whole periods of the `offset` bytes before `before` at `dst`, for
/// `W <= offset < 2 * W`, as a `W`-byte head and a `W`-byte tail each.
///
/// # Safety
///
/// As for [`copy_overlapping`].
#[inline(always)]
unsafe fn copy_periods<const W: usize>(
    before: *const u8,
    dst: *mut u8,
    offset: usize,
    len: usize,
) {
    debug_assert!(W <= offset && offset < 2 * W);
    unsafe {
        let head = ptr::read_unaligned(before.sub(offset).cast::<[u8; W]>());
        let tail = ptr::read_unaligned(before.sub(W).cast::<[u8; W]>());
        let mut i = 0;
        loop {
            ptr::write_unaligned(dst.add(i).cast::<[u8; W]>(), head);
            ptr::write_unaligned(dst.add(i + offset - W).cast::<[u8; W]>(), tail);
            i += offset;
            if i >= len {
                break;
            }
        }
    }
}
