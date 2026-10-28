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

use std::mem::MaybeUninit;
use std::ptr;

use crate::select::opaque;
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

    #[inline]
    pub(crate) fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        check_output_len(input.len(), output.len())?;
        let needed = table_size(input.len().min(BLOCK_SIZE));
        if self.table.len() < needed {
            self.grow_table(needed);
        }
        // SAFETY: the output length was checked and the table is zeroed and
        // large enough.
        Ok(unsafe { compress_checked(input, output, &mut self.table, true) })
    }

    #[cold]
    #[inline(never)]
    fn grow_table(&mut self, len: usize) {
        self.table = vec![0; len];
    }

    pub(crate) fn compress_vec(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        let mut output = vec![0; max_compress_len(input.len())];
        let len = self.compress(input, &mut output)?;
        output.truncate(len);
        Ok(output)
    }
}

/// One-shot compression, without allocating a hash table for small inputs
/// or clearing it after the last block.
pub(crate) fn compress_into(input: &[u8], output: &mut [u8]) -> Result<usize> {
    check_output_len(input.len(), output.len())?;
    if input.len() < MIN_SEARCH_LEN {
        // SAFETY: the output length was checked, and inputs this short are
        // emitted as one literal without using the table.
        return Ok(unsafe { compress_checked(input, output, &mut [], false) });
    }
    if input.len() > STACK_TABLE_SIZE {
        let mut table = vec![0; table_size(input.len().min(BLOCK_SIZE))];
        // SAFETY: the output length was checked and the table is zeroed and
        // large enough.
        return Ok(unsafe { compress_checked(input, output, &mut table, false) });
    }
    // Only the part of the table this input uses is initialized.
    let mut table = [MaybeUninit::<u16>::uninit(); STACK_TABLE_SIZE];
    let table = &mut table[..table_size(input.len())];
    table.fill(MaybeUninit::new(0));
    // SAFETY: every element of `table` was just initialized, and the output
    // length was checked.
    unsafe {
        let table = &mut *(table as *mut [MaybeUninit<u16>] as *mut [u16]);
        Ok(compress_checked(input, output, table, false))
    }
}

fn check_output_len(input_len: usize, output_len: usize) -> Result<()> {
    let min_len = max_compress_len(input_len);
    if min_len == 0 {
        return Err(Error::TooBig {
            given: input_len as u64,
            max: MAX_BLOCK_LEN,
        });
    }
    if output_len < min_len {
        return Err(Error::OutputTooSmall {
            given: output_len,
            min: min_len,
        });
    }
    Ok(())
}

/// Hash table size for a block: the input length rounded up to a power of
/// two, clamped to `MIN_TABLE_SIZE..=MAX_TABLE_SIZE`.
#[inline(always)]
fn table_size(block_len: usize) -> usize {
    if block_len <= MIN_TABLE_SIZE {
        MIN_TABLE_SIZE
    } else {
        block_len.min(MAX_TABLE_SIZE).next_power_of_two()
    }
}

/// Compresses `input` using `table`, returning the compressed length.
///
/// # Safety
///
/// `output` must have room for `max_compress_len(input.len())` bytes, which
/// covers everything written including over-copies (see the module
/// documentation). `table` must be zeroed and have at least
/// `table_size(min(input.len(), BLOCK_SIZE))` entries, unless the input is
/// shorter than `MIN_SEARCH_LEN`; it is left zeroed if `reuse` is set.
#[inline(always)]
unsafe fn compress_checked(
    input: &[u8],
    output: &mut [u8],
    table: &mut [u16],
    reuse: bool,
) -> usize {
    debug_assert!(output.len() >= max_compress_len(input.len()));
    let mut op = write_varint(output, input.len() as u32);
    // SAFETY: (for both branches) as documented above.
    unsafe {
        if input.len() <= BLOCK_SIZE {
            op += compress_block_restoring(
                input,
                output.as_mut_ptr().add(op),
                table,
                reuse,
            );
        } else {
            let mut blocks = input.chunks(BLOCK_SIZE);
            let last = blocks.next_back().unwrap();
            for block in blocks {
                op += compress_block_restoring(
                    block,
                    output.as_mut_ptr().add(op),
                    table,
                    true,
                );
            }
            op +=
                compress_block_restoring(last, output.as_mut_ptr().add(op), table, reuse);
        }
    }
    debug_assert!(op <= output.len());
    op
}

/// Blocks shorter than this are emitted as one literal: the search for a
/// match would not probe them anyway, as its first probe needs two bytes
/// past the input margin.
const MIN_SEARCH_LEN: usize = INPUT_MARGIN + 2;

/// Compresses one block to `op`, and leaves `table` zeroed if `restore` is
/// set.
///
/// # Safety
///
/// As for [`compress_block`], but `table` may be longer than needed.
#[inline(always)]
unsafe fn compress_block_restoring(
    block: &[u8],
    op: *mut u8,
    table: &mut [u16],
    restore: bool,
) -> usize {
    if block.len() < MIN_SEARCH_LEN {
        if block.is_empty() {
            return 0;
        }
        // SAFETY: the bound covers a single literal.
        return unsafe { emit_literal(op, block.as_ptr(), block.len()).offset_from(op) }
            as usize;
    }
    let table = &mut table[..table_size(block.len())];
    debug_assert!(table.iter().all(|&entry| entry == 0));
    // SAFETY: as documented above.
    let written = unsafe { compress_block(block, op, table) };
    if !restore {
        return written;
    }
    // Leave the table zeroed for the next block (or input). Clearing a large
    // table dominates the time spent on incompressible blocks, but a block
    // without any copy only wrote the entries its search probed, so those
    // are cleared instead.
    // SAFETY: `compress_block` wrote at least one element.
    if table.len() <= REPLAY_MIN_TABLE_SIZE
        || !unsafe { is_single_literal(op, block.len()) }
    {
        table.fill(0);
    } else {
        clear_probed(block, table);
    }
    written
}

/// Whether the compressed block at `op` starts with a literal of all `len`
/// bytes, which means that no copy was emitted: the first element of a block
/// is always a literal, and if a copy follows it, it covers fewer bytes.
///
/// # Safety
///
/// `op` must be readable for 3 bytes.
#[inline(always)]
unsafe fn is_single_literal(op: *const u8, len: usize) -> bool {
    debug_assert!((MIN_SEARCH_LEN..=BLOCK_SIZE).contains(&len));
    let n = len - 1;
    // SAFETY: as documented above.
    let tag = unsafe { [*op, *op.add(1), *op.add(2)] };
    if n < 60 {
        tag[0] == (n << 2) as u8
    } else if n < 256 {
        tag[..2] == [60 << 2, n as u8]
    } else {
        tag == [61 << 2, n as u8, (n >> 8) as u8]
    }
}

/// Tables up to this size are cleared in full; repeating the probes of a
/// search costs more than that.
const REPLAY_MIN_TABLE_SIZE: usize = 1 << 12;

/// Clears the table entries written for a block that was emitted as a
/// single literal, by repeating the probes of [`compress_block`]'s search
/// for a match.
fn clear_probed(input: &[u8], table: &mut [u16]) {
    debug_assert!(input.len() >= MIN_SEARCH_LEN);
    clear_search(input, table, 0, None);
    debug_assert!(table.iter().all(|&entry| entry == 0));
}

/// Clears the entries probed by a search for a match that started after
/// `next_emit` and found one at `found`, or ran out of input.
fn clear_search(input: &[u8], table: &mut [u16], next_emit: usize, found: Option<usize>) {
    let ip_limit = input.len() - INPUT_MARGIN;
    let mut ip = next_emit + 1;
    let mut skip = 32u32;
    if ip + 16 <= ip_limit {
        for i in 0..16 {
            clear_entry(input, table, ip + i);
            if found == Some(ip + i) {
                return;
            }
        }
        ip += 16;
        skip += 16;
    }
    loop {
        let step = (skip >> 5) as usize;
        skip += step as u32;
        if ip + step > ip_limit {
            debug_assert!(found.is_none());
            return;
        }
        clear_entry(input, table, ip);
        if found == Some(ip) {
            return;
        }
        ip += step;
    }
}

#[inline(always)]
fn clear_entry(input: &[u8], table: &mut [u16], pos: usize) {
    let bytes = u32::from_le_bytes(input[pos..pos + 4].try_into().unwrap());
    let mask = table.len() as u32 - 1;
    table[hash(bytes, mask)] = 0;
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
fn hash(bytes: u32, mask: u32) -> usize {
    ((bytes.wrapping_mul(0x1e35_a7bd) >> (32 - MAX_TABLE_BITS)) & mask) as usize
}

/// Compresses one block of at most `BLOCK_SIZE` bytes to `op`, returning
/// the number of bytes written.
///
/// # Safety
///
/// `op` must be valid for writes of the block's worst-case compressed size
/// plus the slack described in the module documentation, and `table` must
/// be zeroed, a power of two in length and at most `MAX_TABLE_SIZE`.
#[inline(never)]
unsafe fn compress_block(input: &[u8], op_start: *mut u8, table: &mut [u16]) -> usize {
    debug_assert!(input.len() <= BLOCK_SIZE);
    debug_assert!(table.len().is_power_of_two() && table.len() <= MAX_TABLE_SIZE);
    let base = input.as_ptr();
    let len = input.len();
    let mask = table.len() as u32 - 1;
    let table = table.as_mut_ptr();
    let mut op = op_start;
    // `ip` is the current input position. Table entries are positions in the
    // block, all of them before `ip`, so every candidate load below reads
    // initialized input.
    let mut ip = 0usize;

    if len >= INPUT_MARGIN {
        // The search stops here. It is also the bound below which
        // `find_match_length` compares 16 bytes at a time, so that one
        // register serves both.
        let ip_limit = len - INPUT_MARGIN;
        debug_assert!(len >= MIN_SEARCH_LEN);
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
                        base,
                        base.add(candidate + 4),
                        ip + 4,
                        ip_limit,
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
/// at position `s2` of the block at `base`, and whether that is fewer than
/// eight. The match ends at the end of the block, `INPUT_MARGIN` bytes after
/// `ip_limit`. On return, the low five bytes of `data` are the input after
/// the match, unless the match ran to within eight bytes of the end.
///
/// Computing `data` from the loads used for the comparison keeps the next
/// hash lookup off the chain of loads that depend on the match length.
///
/// # Safety
///
/// `s1` points into the block before `s2`, and `s2 <= ip_limit + 4`: the
/// match starts at or before `ip_limit`.
#[inline(always)]
unsafe fn find_match_length(
    base: *const u8,
    s1: *const u8,
    s2: usize,
    ip_limit: usize,
    data: &mut u64,
) -> (usize, bool) {
    // 16 bytes can be read at `s2 + matched` while that is below `ip_limit`.
    const _: () = assert!(INPUT_MARGIN == 15);
    let mut matched = 0;
    // SAFETY: (throughout) reads at `s2 + matched` stay within the block, as
    // above, and reads at `s1 + matched` trail them by the match offset.
    unsafe {
        if s2 < ip_limit {
            let a1 = read_u64(s1);
            let a2 = read_u64(base.add(s2));
            if a1 != a2 {
                let xor = a1 ^ a2;
                let shift = xor.trailing_zeros();
                let a3 = read_u64(base.add(s2 + 4));
                let a2 = select_if_low_zero(xor, a3, a2);
                *data = a2 >> (shift & 24);
                return ((shift >> 3) as usize, true);
            }
            // Longer matches. Hiding the counter from LLVM keeps it from
            // rewriting the loops to use more than one.
            let p2 = base.add(s2);
            let limit = ip_limit - s2;
            matched = 8;
            let limit_8 = limit.min(LONG_MATCH);
            while matched < limit_8 {
                let a1 = read_u64(s1.add(matched));
                let a2 = read_u64(p2.add(matched));
                if a1 != a2 {
                    let xor = a1 ^ a2;
                    let shift = xor.trailing_zeros();
                    let a3 = read_u64(p2.add(matched + 4));
                    let a2 = select_if_low_zero(xor, a3, a2);
                    *data = a2 >> (shift & 24);
                    return (matched + (shift >> 3) as usize, false);
                }
                matched = opaque(matched + 8);
            }
            // Long matches: 16 bytes at a time. Finding where such a match
            // ends takes longer, but happens less often. The reads reach 23
            // bytes past `p2 + matched`.
            let limit_16 = limit.saturating_sub(7);
            while matched < limit_16 {
                if read_16(s1.add(matched)) != read_16(p2.add(matched)) {
                    let low = read_u64(s1.add(matched)) ^ read_u64(p2.add(matched));
                    let high =
                        read_u64(s1.add(matched + 8)) ^ read_u64(p2.add(matched + 8));
                    let bits = if low != 0 {
                        low.trailing_zeros()
                    } else {
                        64 + high.trailing_zeros()
                    };
                    matched += (bits >> 3) as usize;
                    *data = read_u64(p2.add(matched));
                    return (matched, false);
                }
                matched = opaque(matched + 16);
            }
        }
        // The last bytes of the block, which often all match in small
        // blocks: 8 at a time, then the rest in one comparison of the eight
        // bytes that end at the block end. Those are in bounds at `s1` too:
        // the match starts at or before `ip_limit`, so its offset is less
        // than the block length less `INPUT_MARGIN`.
        let end = ip_limit + INPUT_MARGIN - s2;
        let p2 = base.add(s2);
        while matched + 8 <= end {
            let xor = read_u64(s1.add(matched)) ^ read_u64(p2.add(matched));
            if xor != 0 {
                matched += (xor.trailing_zeros() >> 3) as usize;
                if matched + 8 <= end {
                    *data = read_u64(p2.add(matched));
                }
                return (matched, matched < 8);
            }
            matched += 8;
        }
        let rest = end - matched;
        if rest > 0 {
            debug_assert!(s1.addr() - base.addr() + end >= 8);
            let xor = read_u64(s1.add(end).sub(8)) ^ read_u64(p2.add(end).sub(8));
            // Only the high `rest` bytes are still to compare.
            let xor = xor >> (64 - 8 * rest);
            matched += if xor == 0 {
                rest
            } else {
                (xor.trailing_zeros() >> 3) as usize
            };
        }
    }
    (matched, matched < 8)
}

/// Matches are compared 8 bytes at a time up to this length, and 16 at a
/// time after it.
const LONG_MATCH: usize = 64;

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

/// Returns `below` if `offset < 2048`, else `otherwise`, with a conditional
/// move (see [`emit_copy_at_most_64`]).
#[inline(always)]
fn select_if_below_2048(offset: u32, below: u32, otherwise: u32) -> u32 {
    let mut result = below;
    // SAFETY: (for both) register-only instructions without memory access.
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    unsafe {
        std::arch::asm!(
            "cmp {o:e}, 2048",
            "cmovae {r:e}, {x:e}",
            o = in(reg) offset,
            x = in(reg) otherwise,
            r = inout(reg) result,
            options(pure, nomem, nostack),
        );
    }
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    unsafe {
        std::arch::asm!(
            "cmp {o:w}, #2048",
            "csel {r:w}, {r:w}, {x:w}, lo",
            o = in(reg) offset,
            x = in(reg) otherwise,
            r = inout(reg) result,
            options(pure, nomem, nostack),
        );
    }
    #[cfg(not(all(any(target_arch = "x86_64", target_arch = "aarch64"), not(miri))))]
    if offset >= 2048 {
        result = otherwise;
    }
    result
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
        copy_exact(literal, op, len);
        op.add(len)
    }
}

/// Copies `len` bytes: up to 64 inline, as fixed-size copies that may
/// overlap each other, and more with `memcpy`. Small blocks often end in a
/// short literal, where a call would cost more than the copy.
///
/// # Safety
///
/// As for [`ptr::copy_nonoverlapping`].
#[inline(always)]
unsafe fn copy_exact(from: *const u8, to: *mut u8, len: usize) {
    unsafe {
        if len > 64 {
            ptr::copy_nonoverlapping(from, to, len);
        } else if len > 32 {
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
            // constants cancel the `len << 2` term's offset. LLVM would
            // otherwise turn the selects into a branch.
            let copy1 = 1u32.wrapping_sub(4 << 2).wrapping_add((offset >> 3) & 0xe0);
            let copy2 = 2u32.wrapping_sub(1 << 2);
            let u = ((len << 2) + (offset << 8))
                .wrapping_add(select_if_below_2048(offset, copy1, copy2));
            write_u32(op, u);
            op.add(2 + usize::from(offset >= 2048))
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
unsafe fn write_u64(ptr: *mut u8, value: u64) {
    unsafe { ptr::write_unaligned(ptr.cast::<u64>(), value.to_le()) }
}

#[inline(always)]
unsafe fn read_16(ptr: *const u8) -> [u8; 16] {
    unsafe { ptr::read_unaligned(ptr.cast::<[u8; 16]>()) }
}

#[inline(always)]
unsafe fn copy_16(from: *const u8, to: *mut u8) {
    unsafe { ptr::copy_nonoverlapping(from, to, 16) }
}
