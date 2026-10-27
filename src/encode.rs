//! Raw Snappy block encoding.
//!
//! The compressor follows the C++ reference implementation's default
//! (level 1) algorithm: the same block size, hash table sizes, hash function,
//! match-skipping heuristic and tag choices. For the same input it produces
//! the same bytes as the C++ library does with its portable hash, and so the
//! same compression ratio.
//!
//! Output bound: every literal except the last of a block is followed by a
//! copy of at least four bytes, and every copy element costs at most three
//! bytes for at least four bytes of input. A literal-and-copy pair therefore
//! only grows when the literal needs extra length bytes, by at most two bytes
//! per 61 or more input bytes, and the last literal of each 64 KiB block adds
//! at most three. With the 32 bytes of slack in [`crate::max_compress_len`]
//! this leaves room for the 16-byte over-copies of short literals, which the
//! main loop only performs while at least 15 input bytes remain.

use std::ptr;

use crate::{Error, MAX_BLOCK_LEN, Result, max_compress_len};

/// Input is compressed in independent blocks of this size.
const BLOCK_SIZE: usize = 1 << 16;
const MIN_TABLE_SIZE: usize = 1 << 8;
const MAX_TABLE_BITS: u32 = 15;
const MAX_TABLE_SIZE: usize = 1 << MAX_TABLE_BITS;
/// One-shot compression keeps hash tables up to this size on the stack.
const STACK_TABLE_SIZE: usize = 1 << 12;
/// The main loop stops this many bytes before the end of a block, so that
/// it can over-read and over-copy without checks.
const INPUT_MARGIN: usize = 15;

/// Reusable raw block compressor state: the hash table.
pub(crate) struct Encoder {
    table: Vec<u16>,
}

impl Encoder {
    pub(crate) fn new() -> Self {
        Self { table: Vec::new() }
    }

    pub(crate) fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        let needed = table_size(input.len().min(BLOCK_SIZE));
        if self.table.len() < needed {
            self.table = vec![0; needed];
        }
        compress_with_table(input, output, &mut self.table)
    }

    pub(crate) fn compress_vec(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        let mut output = vec![0; max_compress_len(input.len())];
        let len = self.compress(input, &mut output)?;
        output.truncate(len);
        Ok(output)
    }
}

/// One-shot compression, without allocating a hash table for small inputs.
pub(crate) fn compress_into(input: &[u8], output: &mut [u8]) -> Result<usize> {
    if input.len() <= STACK_TABLE_SIZE {
        let mut table = [0u16; STACK_TABLE_SIZE];
        compress_with_table(input, output, &mut table)
    } else {
        Encoder::new().compress(input, output)
    }
}

/// Hash table size for a block: the input length rounded up to a power of
/// two, clamped to `MIN_TABLE_SIZE..=MAX_TABLE_SIZE`.
fn table_size(block_len: usize) -> usize {
    block_len
        .clamp(MIN_TABLE_SIZE, MAX_TABLE_SIZE)
        .next_power_of_two()
}

fn compress_with_table(
    input: &[u8],
    output: &mut [u8],
    table: &mut [u16],
) -> Result<usize> {
    let min_len = max_compress_len(input.len());
    if min_len == 0 {
        return Err(Error::TooBig {
            given: input.len() as u64,
            max: MAX_BLOCK_LEN,
        });
    }
    if output.len() < min_len {
        return Err(Error::OutputTooSmall {
            given: output.len(),
            min: min_len,
        });
    }

    let mut op = write_varint(output, input.len() as u32);
    for block in input.chunks(BLOCK_SIZE) {
        let table = &mut table[..table_size(block.len())];
        table.fill(0);
        debug_assert!(output.len() - op >= block.len() + 16);
        // SAFETY: `output` has room for `max_compress_len(input.len())`
        // bytes, which covers everything written for this and the following
        // blocks, including over-copies (see the module documentation).
        op += unsafe { compress_block(block, output.as_mut_ptr().add(op), table) };
        debug_assert!(op <= output.len());
    }
    Ok(op)
}

fn write_varint(output: &mut [u8], mut value: u32) -> usize {
    let mut i = 0;
    while value >= 0x80 {
        output[i] = value as u8 | 0x80;
        value >>= 7;
        i += 1;
    }
    output[i] = value as u8;
    i + 1
}

/// Index into a table of `mask + 1` entries for the four bytes `bytes`.
///
/// This is the C++ implementation's multiplicative hash; keeping it, and
/// taking the same bits, keeps the output identical.
#[inline(always)]
fn hash(bytes: u32, mask: usize) -> usize {
    (bytes.wrapping_mul(0x1e35_a7bd) >> (32 - MAX_TABLE_BITS)) as usize & mask
}

/// Compresses one block of at most `BLOCK_SIZE` bytes to `op`, returning
/// the number of bytes written.
///
/// # Safety
///
/// `op` must be valid for writes of the block's worst-case compressed size
/// plus the slack described in the module documentation, and `table` must
/// be zeroed, a power of two in length and at most `MAX_TABLE_SIZE`.
unsafe fn compress_block(input: &[u8], op_start: *mut u8, table: &mut [u16]) -> usize {
    debug_assert!(input.len() <= BLOCK_SIZE);
    debug_assert!(table.len().is_power_of_two() && table.len() <= MAX_TABLE_SIZE);
    let base = input.as_ptr();
    let len = input.len();
    let mask = table.len() - 1;
    let table = table.as_mut_ptr();
    let mut op = op_start;
    // `ip` is the current input position. Table entries are positions in the
    // block, all of them before `ip`, so every candidate load below reads
    // initialized input.
    let mut ip = 0usize;

    if len >= INPUT_MARGIN {
        let ip_limit = len - INPUT_MARGIN;
        // SAFETY: (for the whole loop) all input reads are at most 16 bytes
        // past a position `<= ip_limit` or past an earlier match, so within
        // the block; table indexes are masked; output writes are covered by
        // the bound in the module documentation.
        unsafe {
            let mut preload = read_u32(base.add(ip + 1));
            'outer: loop {
                // Bytes in `next_emit..ip` will be emitted as a literal.
                let next_emit = ip;
                ip += 1;
                let mut data = read_u64(base.add(ip));
                let mut candidate;

                // Step 1: find a four-byte match. The first 16 positions are
                // probed one by one, unrolled, with the bytes taken from one
                // load. After that, the stride grows by one for every 32
                // probes without a match, so that incompressible input is
                // skipped through quickly.
                'found: {
                    let mut skip = 32u32;
                    // `ip` is `ip_limit + 1` for a block of exactly
                    // `INPUT_MARGIN` bytes, so this must not subtract.
                    if ip + 16 <= ip_limit {
                        for j in 0..4 {
                            for k in 0..4 {
                                let i = 4 * j + k;
                                let dword = if i == 0 { preload } else { data as u32 };
                                let entry = table.add(hash(dword, mask));
                                candidate = usize::from(*entry);
                                *entry = (ip + i) as u16;
                                if read_u32(base.add(candidate)) == dword {
                                    // A literal of `i + 1 <= 16` bytes.
                                    *op = (i << 2) as u8;
                                    copy_16(base.add(next_emit), op.add(1));
                                    ip += i;
                                    op = op.add(i + 2);
                                    break 'found;
                                }
                                data >>= 8;
                            }
                            data = read_u64(base.add(ip + 4 * j + 4));
                        }
                        ip += 16;
                        skip += 16;
                    }
                    loop {
                        let entry = table.add(hash(data as u32, mask));
                        let step = (skip >> 5) as usize;
                        skip += step as u32;
                        let next_ip = ip + step;
                        if next_ip > ip_limit {
                            ip = next_emit;
                            break 'outer;
                        }
                        candidate = usize::from(*entry);
                        *entry = ip as u16;
                        if data as u32 == read_u32(base.add(candidate)) {
                            break;
                        }
                        data = u64::from(read_u32(base.add(next_ip)));
                        ip = next_ip;
                    }
                    // Step 2: emit the bytes before the match as a literal.
                    op = emit_literal_fast(op, base.add(next_emit), ip - next_emit);
                }

                // Step 3: emit copies for as long as the bytes after the last
                // copy match again.
                loop {
                    let start = ip;
                    let (extra, short) = find_match_length(
                        base.add(candidate + 4),
                        base.add(ip + 4),
                        base.add(len),
                        &mut data,
                    );
                    let matched = 4 + extra;
                    ip += matched;
                    let offset = start - candidate;
                    op = if short {
                        emit_copy_at_most_64::<true>(op, offset, matched)
                    } else {
                        emit_copy(op, offset, matched)
                    };
                    if ip >= ip_limit {
                        break 'outer;
                    }
                    // `data` now holds at least five bytes from `ip`. Insert
                    // `ip - 1` too, to improve compression, and look up the
                    // next candidate.
                    *table.add(hash(read_u32(base.add(ip - 1)), mask)) = (ip - 1) as u16;
                    let entry = table.add(hash(data as u32, mask));
                    candidate = usize::from(*entry);
                    *entry = ip as u16;
                    if data as u32 != read_u32(base.add(candidate)) {
                        break;
                    }
                }
                preload = (data >> 8) as u32;
            }
        }
    }

    if ip < len {
        // SAFETY: the remaining output is covered by the bound.
        op = unsafe { emit_literal(op, base.add(ip), len - ip) };
    }
    // SAFETY: `op` was derived from `op_start`.
    unsafe { op.offset_from(op_start) as usize }
}

/// Returns the number of bytes after the first four that match at `s1` and
/// `s2` (not reading at or past `s2_limit`) and whether that is fewer than
/// eight. On return, the low five bytes of `data` are the input after the
/// match, unless the match ran to within eight bytes of `s2_limit`.
///
/// Computing `data` from the loads used for the comparison keeps the next
/// hash lookup off the chain of loads that depend on the match length.
///
/// # Safety
///
/// `s1 < s2 <= s2_limit` within the same block.
#[inline(always)]
unsafe fn find_match_length(
    s1: *const u8,
    mut s2: *const u8,
    s2_limit: *const u8,
    data: &mut u64,
) -> (usize, bool) {
    let mut matched = 0usize;
    // SAFETY: (throughout) reads at `s2` are at most 16 bytes before
    // `s2_limit` and reads at `s1` trail them by the match offset.
    unsafe {
        if s2_limit.offset_from(s2) >= 16 {
            let a1 = read_u64(s1);
            let a2 = read_u64(s2);
            if a1 != a2 {
                let xor = a1 ^ a2;
                let shift = xor.trailing_zeros();
                let a3 = read_u64(s2.add(4));
                let a2 = select_if_low_zero(xor, a3, a2);
                *data = a2 >> (shift & 24);
                return ((shift >> 3) as usize, true);
            }
            matched = 8;
            s2 = s2.add(8);
        }
        while s2_limit.offset_from(s2) >= 16 {
            let a1 = read_u64(s1.add(matched));
            let a2 = read_u64(s2);
            if a1 == a2 {
                s2 = s2.add(8);
                matched += 8;
            } else {
                let xor = a1 ^ a2;
                let shift = xor.trailing_zeros();
                let a3 = read_u64(s2.add(4));
                let a2 = select_if_low_zero(xor, a3, a2);
                *data = a2 >> (shift & 24);
                return (matched + (shift >> 3) as usize, false);
            }
        }
        while s2 < s2_limit {
            if *s1.add(matched) == *s2 {
                s2 = s2.add(1);
                matched += 1;
            } else {
                if s2_limit.offset_from(s2) >= 8 {
                    *data = read_u64(s2);
                }
                return (matched, matched < 8);
            }
        }
    }
    (matched, matched < 8)
}

/// Returns `if_zero` if the low 32 bits of `xor` are zero, else `otherwise`.
///
/// The choice depends on where two inputs first differ, which is hard to
/// predict, so this uses a conditional move rather than a branch.
#[inline(always)]
fn select_if_low_zero(xor: u64, if_zero: u64, otherwise: u64) -> u64 {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        let mut result = otherwise;
        // SAFETY: register-only instructions without memory access.
        unsafe {
            std::arch::asm!(
                "test {x:e}, {x:e}",
                "cmovz {r}, {z}",
                x = in(reg) xor,
                z = in(reg) if_zero,
                r = inout(reg) result,
                options(pure, nomem, nostack),
            );
        }
        result
    }
    #[cfg(not(all(target_arch = "x86_64", not(miri))))]
    {
        if xor as u32 == 0 { if_zero } else { otherwise }
    }
}

/// Emits a literal inside the main loop, where up to 15 bytes may be read
/// past it and written past its end.
///
/// # Safety
///
/// `literal` must be readable for `len + 15` bytes and `op` writable for
/// the literal plus 16 bytes.
#[inline(always)]
unsafe fn emit_literal_fast(op: *mut u8, literal: *const u8, len: usize) -> *mut u8 {
    debug_assert!(len > 0);
    unsafe {
        if len <= 16 {
            *op = ((len - 1) << 2) as u8;
            copy_16(literal, op.add(1));
            return op.add(1 + len);
        }
        let op = emit_literal_tag(op, len);
        if len <= 64 {
            let mut i = 0;
            loop {
                copy_16(literal.add(i), op.add(i));
                i += 16;
                if i >= len {
                    break;
                }
            }
        } else {
            ptr::copy_nonoverlapping(literal, op, len);
        }
        op.add(len)
    }
}

/// Emits a literal with an exact copy.
///
/// # Safety
///
/// `literal` must be readable for `len` bytes and `op` writable for the
/// literal plus 4 bytes.
#[inline(always)]
unsafe fn emit_literal(op: *mut u8, literal: *const u8, len: usize) -> *mut u8 {
    unsafe {
        let op = emit_literal_tag(op, len);
        ptr::copy_nonoverlapping(literal, op, len);
        op.add(len)
    }
}

/// Writes the tag (and length bytes) of a literal of `len` bytes.
///
/// # Safety
///
/// `op` must be writable for 5 bytes.
#[inline(always)]
unsafe fn emit_literal_tag(op: *mut u8, len: usize) -> *mut u8 {
    debug_assert!(len > 0 && len as u64 <= MAX_BLOCK_LEN);
    let n = len - 1;
    unsafe {
        if n < 60 {
            *op = (n << 2) as u8;
            op.add(1)
        } else {
            // Bytes needed for `n`; four are written, only `count` kept.
            let count = (n.ilog2() >> 3) as usize + 1;
            *op = ((59 + count) << 2) as u8;
            write_u32(op.add(1), n as u32);
            op.add(1 + count)
        }
    }
}

/// Emits a copy of `4..=64` bytes; `SHORT` says whether `len < 12`.
///
/// # Safety
///
/// `op` must be writable for 4 bytes; `1 <= offset < 65536`.
#[inline(always)]
unsafe fn emit_copy_at_most_64<const SHORT: bool>(
    op: *mut u8,
    offset: usize,
    len: usize,
) -> *mut u8 {
    debug_assert!((4..=64).contains(&len) && (1..65536).contains(&offset));
    debug_assert_eq!(SHORT, len < 12);
    let offset = offset as u32;
    let len = len as u32;
    unsafe {
        if SHORT {
            // A 1-byte-offset copy if the offset fits in 11 bits, else a
            // 2-byte-offset one. Which one is hard to predict, so both are
            // computed and one is selected.
            // The tag bytes are built with wrapping arithmetic, as the
            // constants cancel the `len << 2` term's offset.
            let mut u = (len << 2) + (offset << 8);
            let copy1 = 1u32.wrapping_sub(4 << 2).wrapping_add((offset >> 3) & 0xe0);
            let copy2 = 2u32.wrapping_sub(1 << 2);
            u = u.wrapping_add(if offset < 2048 { copy1 } else { copy2 });
            write_u32(op, u);
            op.add(if offset < 2048 { 2 } else { 3 })
        } else {
            write_u32(op, 2 + ((len - 1) << 2) + (offset << 8));
            op.add(3)
        }
    }
}

/// Emits a copy of at least 12 bytes as copies of at most 64 bytes.
///
/// # Safety
///
/// `op` must be writable for the copies plus one byte; `1 <= offset <
/// 65536`.
#[inline(always)]
unsafe fn emit_copy(mut op: *mut u8, offset: usize, mut len: usize) -> *mut u8 {
    debug_assert!(len >= 12);
    unsafe {
        // Keep at least four bytes for the last copy.
        while len >= 68 {
            op = emit_copy_at_most_64::<false>(op, offset, 64);
            len -= 64;
        }
        if len > 64 {
            op = emit_copy_at_most_64::<false>(op, offset, 60);
            len -= 60;
        }
        if len < 12 {
            emit_copy_at_most_64::<true>(op, offset, len)
        } else {
            emit_copy_at_most_64::<false>(op, offset, len)
        }
    }
}

#[inline(always)]
unsafe fn read_u32(ptr: *const u8) -> u32 {
    u32::from_le(unsafe { ptr::read_unaligned(ptr.cast::<u32>()) })
}

#[inline(always)]
unsafe fn read_u64(ptr: *const u8) -> u64 {
    u64::from_le(unsafe { ptr::read_unaligned(ptr.cast::<u64>()) })
}

#[inline(always)]
unsafe fn write_u32(ptr: *mut u8, value: u32) {
    unsafe { ptr::write_unaligned(ptr.cast::<u32>(), value.to_le()) }
}

#[inline(always)]
unsafe fn copy_16(from: *const u8, to: *mut u8) {
    unsafe { ptr::copy_nonoverlapping(from, to, 16) }
}
