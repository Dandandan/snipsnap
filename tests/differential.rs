//! Differential tests of the raw decoder against the `snap` crate.
//!
//! The decoder has a fast loop that over-reads and over-writes within fixed
//! margins, a careful path for buffer ends, and special handling for runs of
//! identical copies. These tests generate inputs that exercise all of them
//! (and their boundaries) and check that snipsnap agrees with `snap` on
//! every input: same output for valid blocks, rejection of invalid ones.

use std::mem::MaybeUninit;

/// Small xorshift generator, so the tests need no extra dependencies.
struct Rng(u64);

/// Scales iteration counts and sizes down under Miri, which is much slower.
fn scaled(n: usize) -> usize {
    if cfg!(miri) { (n / 50).max(4) } else { n }
}

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn snap_decode(src: &[u8]) -> Option<Vec<u8>> {
    snap::raw::Decoder::new().decompress_vec(src).ok()
}

/// Decodes `src` with every snipsnap API and checks that they agree with
/// each other and with `snap`.
fn check(src: &[u8]) {
    let expected = snap_decode(src);
    let decoded = snipsnap::decompress(src).ok();
    assert_eq!(
        decoded, expected,
        "decompress disagrees with snap on {src:02x?}"
    );

    if let Ok(len) = snipsnap::decompress_len(src) {
        if len > 1 << 20 {
            return;
        }
        // Exact-size and oversized buffers, so that buffer ends are tested
        // both at the decoded length and with spare room after it.
        for extra in [0usize, 7, 200] {
            let mut out = vec![0xa5u8; len + extra];
            let result = snipsnap::decompress_into(src, &mut out)
                .ok()
                .map(|d| d.to_vec());
            assert_eq!(result, expected, "decompress_into (+{extra}) disagrees");

            let mut out = vec![MaybeUninit::<u8>::uninit(); len + extra];
            let result = snipsnap::decompress_into_uninit(src, &mut out)
                .ok()
                .map(|d| d.to_vec());
            assert_eq!(
                result, expected,
                "decompress_into_uninit (+{extra}) disagrees"
            );
        }
    }
}

fn header(len: usize, out: &mut Vec<u8>) {
    let mut len = len;
    while len >= 0x80 {
        out.push(len as u8 | 0x80);
        len >>= 7;
    }
    out.push(len as u8);
}

fn literal(data: &[u8], out: &mut Vec<u8>) {
    let n = data.len() - 1;
    if n < 60 {
        out.push((n as u8) << 2);
    } else if n < 1 << 8 {
        out.extend([60 << 2, n as u8]);
    } else if n < 1 << 16 {
        out.extend([61 << 2, n as u8, (n >> 8) as u8]);
    } else {
        out.extend([62 << 2, n as u8, (n >> 8) as u8, (n >> 16) as u8]);
    }
    out.extend_from_slice(data);
}

/// Appends a copy element, choosing the encoding at random among those that
/// can represent it.
fn copy(rng: &mut Rng, len: usize, offset: usize, out: &mut Vec<u8>) {
    assert!((1..=64).contains(&len));
    let fits_copy1 = (4..=11).contains(&len) && offset < 2048;
    let fits_copy2 = offset < 65536;
    match rng.below(4) {
        0 | 1 if fits_copy1 => {
            out.push((((offset >> 8) as u8) << 5) | (((len - 4) as u8) << 2) | 1);
            out.push(offset as u8);
        }
        0..=2 if fits_copy2 => {
            out.push((((len - 1) as u8) << 2) | 2);
            out.extend((offset as u16).to_le_bytes());
        }
        _ => {
            out.push((((len - 1) as u8) << 2) | 3);
            out.extend((offset as u32).to_le_bytes());
        }
    }
}

/// Builds a random valid block element by element, returning the encoded
/// block and the expected output.
fn random_block(rng: &mut Rng, target_len: usize) -> (Vec<u8>, Vec<u8>) {
    let mut body = Vec::new();
    let mut expected: Vec<u8> = Vec::new();
    while expected.len() < target_len {
        let remaining = target_len - expected.len();
        let can_copy = !expected.is_empty();
        match rng.below(10) {
            0..=3 if can_copy => {
                // Copies with small offsets (overlapping and not) are the
                // interesting ones.
                let max_offset = if rng.below(4) == 0 {
                    expected.len()
                } else {
                    expected.len().min(1 + rng.below(80))
                };
                let offset = 1 + rng.below(max_offset);
                let len = 1 + rng.below(remaining.min(64));
                copy(rng, len, offset, &mut body);
                for _ in 0..len {
                    expected.push(expected[expected.len() - offset]);
                }
            }
            4 if can_copy => {
                // A run of identical copies.
                let offset = 1 + rng.below(expected.len().min(130));
                let len = [4, 11, 16, 38, 64][rng.below(5)].min(remaining);
                let element_start = body.len();
                copy(rng, len.max(1), offset, &mut body);
                let element = body[element_start..].to_vec();
                let mut produced = len.max(1);
                let repeats = rng.below(40);
                for _ in 0..repeats {
                    if produced + len.max(1) > remaining {
                        break;
                    }
                    body.extend_from_slice(&element);
                    produced += len.max(1);
                }
                for _ in 0..produced {
                    expected.push(expected[expected.len() - offset]);
                }
            }
            _ => {
                let len = match rng.below(8) {
                    0 => 1 + rng.below(remaining.min(300)),
                    1 if remaining > 300 => 61 + rng.below(remaining.min(70_000) - 60),
                    _ => 1 + rng.below(remaining.min(60)),
                };
                let data: Vec<u8> = (0..len).map(|_| rng.next() as u8 % 7).collect();
                literal(&data, &mut body);
                expected.extend_from_slice(&data);
            }
        }
    }
    let mut block = Vec::new();
    header(expected.len(), &mut block);
    block.extend_from_slice(&body);
    (block, expected)
}

/// Data with the kinds of structure that compressors turn into runs,
/// overlapping copies and long literals.
fn structured_data(rng: &mut Rng, len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    while data.len() < len {
        let n = 1 + rng.below(400);
        match rng.below(5) {
            0 => {
                let byte = rng.next() as u8;
                data.extend(std::iter::repeat_n(byte, n));
            }
            1 => {
                let period = 1 + rng.below(20);
                let pattern: Vec<u8> = (0..period).map(|_| rng.next() as u8).collect();
                data.extend((0..n).map(|i| pattern[i % period]));
            }
            2 => data.extend((0..n).map(|_| rng.next() as u8)),
            3 if !data.is_empty() => {
                let start = rng.below(data.len());
                let end = (start + n).min(data.len());
                data.extend_from_within(start..end);
            }
            _ => data.extend((0..n).map(|_| b'a' + rng.below(6) as u8)),
        }
    }
    data.truncate(len);
    data
}

#[test]
fn random_blocks_match_snap() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for round in 0..scaled(3000) {
        let target = match round % 4 {
            0 => rng.below(64),
            1 => rng.below(600),
            2 => rng.below(scaled(5000)),
            _ => rng.below(scaled(70_000)),
        };
        let (block, expected) = random_block(&mut rng, target);
        assert_eq!(snap_decode(&block).as_deref(), Some(&expected[..]));
        check(&block);
    }
}

#[test]
fn compressed_structured_data_roundtrips() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for round in 0..scaled(400) {
        let len = match round % 3 {
            0 => rng.below(300),
            1 => rng.below(scaled(10_000)),
            _ => rng.below(scaled(300_000)),
        };
        let data = structured_data(&mut rng, len);
        let compressed = snap::raw::Encoder::new().compress_vec(&data).unwrap();
        assert_eq!(snipsnap::decompress(&compressed).unwrap(), data);
        check(&compressed);
        let ours = snipsnap::compress(&data).unwrap();
        assert_eq!(snap_decode(&ours).unwrap(), data);
        assert_eq!(snipsnap::decompress(&ours).unwrap(), data);
    }
}

/// Every short length, where the encoder's main loop starts, ends or does
/// not run at all.
#[test]
fn short_inputs_roundtrip() {
    let mut rng = Rng(0x5851_f42d_4c95_7f2d);
    for len in 0..scaled(300).max(80) {
        for kind in 0..4 {
            let data: Vec<u8> = match kind {
                0 => vec![b'a'; len],
                1 => (0..len).map(|i| b"abcd"[i % 4]).collect(),
                2 => (0..len).map(|_| rng.next() as u8).collect(),
                _ => structured_data(&mut rng, len),
            };
            let ours = snipsnap::compress(&data).unwrap();
            assert_eq!(snap_decode(&ours).as_deref(), Some(&data[..]), "len {len}");
            assert_eq!(snipsnap::decompress(&ours).unwrap(), data, "len {len}");
            let mut encoder = snipsnap::raw::Encoder::new();
            assert_eq!(encoder.compress_vec(&data).unwrap(), ours, "len {len}");
        }
    }
}

/// Incompressible blocks with a few planted matches, around the number of
/// matches up to which the encoder clears its hash table entry by entry.
#[test]
fn few_matches_roundtrip() {
    let mut rng = Rng(0x6a09_e667_f3bc_c908);
    let mut encoder = snipsnap::raw::Encoder::new();
    for matches in 0..scaled(40).max(36) {
        for len in [
            scaled(3000).max(200),
            scaled(70_000).max(300),
            scaled(140_000).max(400),
        ] {
            let mut data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            for _ in 0..matches {
                let n = 4 + rng.below(60);
                let from = rng.below(len - n);
                let to = rng.below(len - n);
                data.copy_within(from..from + n, to);
            }
            let ours = encoder.compress_vec(&data).unwrap();
            assert_eq!(snap_decode(&ours).as_deref(), Some(&data[..]));
            assert_eq!(snipsnap::compress(&data).unwrap(), ours);
        }
    }
}

/// Random bytes repeated with a random period for `len` bytes (one long
/// match after the first period), then one byte that breaks the pattern and
/// `tail` more bytes of it, so that another match follows right away.
fn periodic(rng: &mut Rng, len: usize, tail: usize) -> Vec<u8> {
    let pattern: Vec<u8> = (0..1 + rng.below(40)).map(|_| rng.next() as u8).collect();
    let at = |i: usize| pattern[i % pattern.len()];
    let mut data: Vec<u8> = (0..len).map(at).collect();
    data.push(at(len) ^ 0x55);
    data.extend((len + 1..len + 1 + tail).map(at));
    data
}

/// Long matches ending at every position near the end of a block, and with
/// every length around the one where the encoder's match search switches to
/// 16-byte steps.
#[test]
fn long_matches_roundtrip() {
    let mut rng = Rng(0xbb67_ae85_84ca_a73b);
    let step = if cfg!(miri) { 7 } else { 1 };
    for tail in 0..24 {
        let mut inputs: Vec<Vec<u8>> = (1..200)
            .step_by(step)
            .map(|len| periodic(&mut rng, len, tail))
            .collect();
        if !cfg!(miri) {
            // A match running to the end of the first block, or across it.
            inputs.push(periodic(&mut rng, 65536 - tail, 0));
            inputs.push(periodic(&mut rng, 65536 + tail, 20));
        }
        for data in inputs {
            let ours = snipsnap::compress(&data).unwrap();
            let len = data.len();
            assert_eq!(snap_decode(&ours), Some(data), "len {len}, tail {tail}");
        }
    }
}

/// Copies in blocks small enough for the careful path: every offset below
/// 64 with every length up to 64, right after a literal of `offset` bytes
/// (so that short patterns start fewer than eight bytes into the output) or
/// a longer one, and followed by a few more bytes or none.
#[test]
fn small_block_copies_match_snap() {
    let mut rng = Rng(0x3c6e_f372_fe94_f82b);
    let step = if cfg!(miri) { 5 } else { 1 };
    for offset in (1..64).step_by(step) {
        for len in (1..=64).step_by(step) {
            for prefix in [0, 3, 9] {
                let mut expected: Vec<u8> =
                    (0..offset + prefix).map(|_| rng.next() as u8).collect();
                let mut body = Vec::new();
                literal(&expected, &mut body);
                copy(&mut rng, len, offset, &mut body);
                for _ in 0..len {
                    expected.push(expected[expected.len() - offset]);
                }
                let tail: Vec<u8> = (0..rng.below(9)).map(|_| rng.next() as u8).collect();
                if !tail.is_empty() {
                    literal(&tail, &mut body);
                    expected.extend_from_slice(&tail);
                }
                let mut block = Vec::new();
                header(expected.len(), &mut block);
                block.extend_from_slice(&body);
                assert_eq!(
                    snipsnap::decompress(&block).ok(),
                    Some(expected),
                    "offset {offset}, len {len}, prefix {prefix}"
                );
                check(&block);
            }
        }
    }
}

#[test]
fn runs_of_identical_copies_match_snap() {
    // Runs of identical copy-2 tags, which the decoder fills period by
    // period, at every period it batches and lengths that end each fill
    // block at a different point.
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let step = if cfg!(miri) { 7 } else { 1 };
    for period in (8..=120).step_by(step) {
        for (len, count) in [(64, 2), (64, 9), (61, 5), (17, 12), (40, 31)] {
            let mut expected: Vec<u8> = (0..200).map(|_| rng.next() as u8).collect();
            let mut body = Vec::new();
            literal(&expected, &mut body);
            for _ in 0..count {
                body.push((((len - 1) << 2) | 2) as u8);
                body.extend_from_slice(&(period as u16).to_le_bytes());
                for _ in 0..len {
                    expected.push(expected[expected.len() - period]);
                }
            }
            let tail: Vec<u8> = (0..rng.below(80)).map(|_| rng.next() as u8).collect();
            if !tail.is_empty() {
                literal(&tail, &mut body);
                expected.extend_from_slice(&tail);
            }
            let mut block = Vec::new();
            header(expected.len(), &mut block);
            block.extend_from_slice(&body);
            assert_eq!(
                snipsnap::decompress(&block).ok(),
                Some(expected),
                "period {period}, len {len}, count {count}"
            );
            check(&block);
        }
    }
}

#[test]
fn mutated_blocks_agree_with_snap() {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    for round in 0..scaled(4000) {
        let target = rng.below(if round % 2 == 0 { 300 } else { 3000 });
        let (mut block, _) = random_block(&mut rng, target);
        match rng.below(5) {
            0 => {
                let n = rng.below(block.len() + 1);
                block.truncate(n);
            }
            1 => {
                for _ in 0..1 + rng.below(4) {
                    block.push(rng.next() as u8);
                }
            }
            _ => {
                for _ in 0..1 + rng.below(3) {
                    if block.is_empty() {
                        break;
                    }
                    let i = rng.below(block.len());
                    block[i] ^= 1 << rng.below(8);
                }
            }
        }
        check(&block);
    }
}

#[test]
fn random_bytes_agree_with_snap() {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    for _ in 0..scaled(20_000) {
        let len = rng.below(40);
        let mut block: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if !block.is_empty() {
            // Keep the decoded length small so that more inputs get past the
            // header.
            block[0] &= 0x7f;
        }
        check(&block);
    }
}
