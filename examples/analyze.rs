use std::collections::BTreeMap;

type CopyOp = (usize, usize);
type AlternatingRun = ((CopyOp, CopyOp), usize, usize, usize);

fn compress(data: &[u8]) -> Vec<u8> {
    snap::raw::Encoder::new().compress_vec(data).unwrap()
}

fn mixed_bytes(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(match i & 63 {
            0..=47 => b'0' + (i % 10) as u8,
            _ => ((i * 31) >> 3) as u8,
        });
    }
    out
}

fn arrowish_strings(len: usize) -> Vec<u8> {
    const WORDS: &[&[u8]] = &[
        b"customer_id",
        b"order_total",
        b"2026-06-06",
        b"warehouse",
        b"region",
        b"status=closed",
        b"sku",
        b"quantity",
    ];
    let mut out = Vec::with_capacity(len);
    let mut i = 0usize;
    while out.len() < len {
        let word = WORDS[i & (WORDS.len() - 1)];
        let remaining = len - out.len();
        out.extend_from_slice(&word[..word.len().min(remaining)]);
        if out.len() < len {
            out.push(b'\n');
        }
        i = i.wrapping_add(1);
    }
    out
}

fn read_header(src: &[u8]) -> usize {
    for (i, &byte) in src.iter().enumerate() {
        if byte < 0x80 {
            return i + 1;
        }
    }
    panic!("invalid header")
}

fn literal_len(src: &[u8], ip: &mut usize, tag: u8) -> usize {
    let mut len = usize::from(tag >> 2) + 1;
    if len > 60 {
        let bytes = len - 60;
        len = 1;
        for i in 0..bytes {
            len += usize::from(src[*ip + i]) << (8 * i);
        }
        *ip += bytes;
    }
    len
}

fn analyze(name: &str, data: Vec<u8>) {
    let compressed = compress(&data);
    let fast_stats = fast_path_stats(&compressed, data.len());
    let mut ip = read_header(&compressed);
    let mut copy_ops = Vec::<(usize, usize)>::new();
    let mut literals = 0usize;
    let mut literal_bytes = 0usize;
    let mut copies = 0usize;
    let mut copy_bytes = 0usize;
    let mut offsets = BTreeMap::<usize, (usize, usize)>::new();
    let mut lens = BTreeMap::<usize, usize>::new();
    let mut runs = BTreeMap::<(usize, usize), (usize, usize, usize)>::new();
    let mut previous_copy = None::<(usize, usize)>;
    let mut current_run = 0usize;

    while ip < compressed.len() {
        let tag = compressed[ip];
        ip += 1;
        match tag & 3 {
            0 => {
                if let Some((len, offset)) = previous_copy.take() {
                    record_run(&mut runs, len, offset, current_run);
                }
                current_run = 0;
                let len = literal_len(&compressed, &mut ip, tag);
                ip += len;
                literals += 1;
                literal_bytes += len;
            }
            1 => {
                let len = 4 + usize::from((tag >> 2) & 7);
                let offset = (usize::from(tag & 0xe0) << 3) | usize::from(compressed[ip]);
                ip += 1;
                copies += 1;
                copy_bytes += len;
                let entry = offsets.entry(offset).or_default();
                entry.0 += 1;
                entry.1 += len;
                *lens.entry(len).or_default() += 1;
                copy_ops.push((len, offset));
                update_run(&mut runs, &mut previous_copy, &mut current_run, len, offset);
            }
            2 => {
                let len = 1 + usize::from(tag >> 2);
                let offset =
                    u16::from_le_bytes([compressed[ip], compressed[ip + 1]]) as usize;
                ip += 2;
                copies += 1;
                copy_bytes += len;
                let entry = offsets.entry(offset).or_default();
                entry.0 += 1;
                entry.1 += len;
                *lens.entry(len).or_default() += 1;
                copy_ops.push((len, offset));
                update_run(&mut runs, &mut previous_copy, &mut current_run, len, offset);
            }
            _ => {
                let len = 1 + usize::from(tag >> 2);
                let offset = u32::from_le_bytes([
                    compressed[ip],
                    compressed[ip + 1],
                    compressed[ip + 2],
                    compressed[ip + 3],
                ]) as usize;
                ip += 4;
                copies += 1;
                copy_bytes += len;
                let entry = offsets.entry(offset).or_default();
                entry.0 += 1;
                entry.1 += len;
                *lens.entry(len).or_default() += 1;
                copy_ops.push((len, offset));
                update_run(&mut runs, &mut previous_copy, &mut current_run, len, offset);
            }
        }
    }
    if let Some((len, offset)) = previous_copy.take() {
        record_run(&mut runs, len, offset, current_run);
    }

    println!("{name}: {} -> {}", data.len(), compressed.len());
    println!(
        "fast stats: loops {} fast {} fused {} bulk tags {} fallback {}",
        fast_stats.loops,
        fast_stats.fast_ops,
        fast_stats.fused_ops,
        fast_stats.bulk_tags,
        fast_stats.fallback_ops
    );
    println!("copy2 window fused counts: {:?}", fast_stats.window_runs);
    println!(
        "copy2 window by caller: exact {:?}, copy1 {:?}",
        fast_stats.exact_window_runs, fast_stats.copy1_window_runs
    );
    println!("loop-start tags:");
    let mut loop_tags = fast_stats.loop_tags.into_iter().collect::<Vec<_>>();
    loop_tags.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for (tag, count) in loop_tags.into_iter().take(12) {
        println!("  tag 0x{tag:02x}: {count}");
    }
    println!("loop-start transitions:");
    let mut loop_pairs = fast_stats.loop_pairs.into_iter().collect::<Vec<_>>();
    loop_pairs.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for ((left, right), count) in loop_pairs.into_iter().take(12) {
        println!("  tag 0x{left:02x} -> 0x{right:02x}: {count}");
    }
    println!("literals {literals} / {literal_bytes} bytes");
    println!("copies {copies} / {copy_bytes} bytes");
    println!("top offsets:");
    let mut offsets = offsets.into_iter().collect::<Vec<_>>();
    offsets.sort_by_key(|(_, (_, bytes))| std::cmp::Reverse(*bytes));
    for (offset, (count, bytes)) in offsets.into_iter().take(12) {
        println!("  offset {offset:5}: {count:5} tags, {bytes:8} bytes");
    }
    println!("copy lengths:");
    for (len, count) in lens {
        println!("  len {len:2}: {count}");
    }
    println!("top same copy runs:");
    let mut runs = runs.into_iter().collect::<Vec<_>>();
    runs.sort_by_key(|(_, (_, max_run, bytes))| std::cmp::Reverse((*max_run, *bytes)));
    for ((len, offset), (runs, max_run, bytes)) in runs.into_iter().take(12) {
        println!(
            "  len {len:2} offset {offset:5}: {runs:5} runs, max {max_run:5}, {bytes:8} bytes"
        );
    }
    println!("top alternating pair runs:");
    for ((left, right), runs, max_pairs, bytes) in top_alternating_runs(&copy_ops) {
        println!(
            "  ({:2},{:5}) <-> ({:2},{:5}): {runs:5} runs, max {max_pairs:5} pairs, {bytes:8} bytes",
            left.0, left.1, right.0, right.1
        );
    }
}

#[derive(Default)]
struct FastStats {
    loops: usize,
    fast_ops: usize,
    fused_ops: usize,
    bulk_tags: usize,
    fallback_ops: usize,
    window_runs: [usize; 4],
    exact_window_runs: [usize; 4],
    copy1_window_runs: [usize; 4],
    loop_tags: BTreeMap<u8, usize>,
    loop_pairs: BTreeMap<(u8, u8), usize>,
}

fn fast_path_stats(src: &[u8], dst_len: usize) -> FastStats {
    let mut stats = FastStats::default();
    let mut ip = read_header(src);
    let mut op = 0usize;
    let mut previous_loop_tag = None;
    while ip < src.len() {
        stats.loops += 1;
        let tag = src[ip];
        *stats.loop_tags.entry(tag).or_default() += 1;
        if let Some(previous) = previous_loop_tag {
            *stats.loop_pairs.entry((previous, tag)).or_default() += 1;
        }
        previous_loop_tag = Some(tag);
        ip += 1;

        if 2 <= src.len() - ip && 64 <= dst_len - op {
            match tag {
                0xfe => {
                    let offset = u16::from_le_bytes([src[ip], src[ip + 1]]) as usize;
                    if offset > 0 && offset <= op {
                        let mut copies = 1usize;
                        let mut run_ip = ip + 2;
                        while copies < (dst_len - op) / 64
                            && run_ip + 2 < src.len()
                            && src[run_ip] == 0xfe
                            && u16::from_le_bytes([src[run_ip + 1], src[run_ip + 2]])
                                as usize
                                == offset
                        {
                            copies += 1;
                            run_ip += 3;
                        }
                        if copies >= 4 {
                            ip = run_ip;
                            op += copies * 64;
                            stats.fast_ops += 1;
                            stats.bulk_tags += copies;
                            continue;
                        }
                        if offset >= 64 {
                            ip += 2;
                            op += 64;
                            stats.fast_ops += 1;
                            continue;
                        }
                    }
                }
                0xa6 => {
                    if 2 <= src.len() - ip
                        && try_fast_copy2_after_tag_with_len(
                            dst_len,
                            u16::from_le_bytes([src[ip], src[ip + 1]]) as usize,
                            42,
                            42,
                            &mut ip,
                            &mut op,
                        )
                    {
                        stats.fast_ops += 1;
                        let fused = fast_copy2_window_run(src, dst_len, &mut ip, &mut op);
                        stats.fused_ops += fused;
                        stats.window_runs[fused] += 1;
                        stats.exact_window_runs[fused] += 1;
                        continue;
                    }
                }
                0x3e | 0xbe | 0xae | 0x96 => {
                    if try_fast_copy2_after_tag(src, dst_len, tag, &mut ip, &mut op) {
                        stats.fast_ops += 1;
                        let fused = fast_copy2_window_run(src, dst_len, &mut ip, &mut op);
                        stats.fused_ops += fused;
                        stats.window_runs[fused] += 1;
                        stats.exact_window_runs[fused] += 1;
                        continue;
                    }
                }
                _ if tag & 3 == 0 => {
                    let len = usize::from(tag >> 2) + 1;
                    if len <= 16 && len <= src.len() - ip {
                        ip += len;
                        op += len;
                        stats.fast_ops += 1;
                        if ip < src.len() {
                            let next_tag = src[ip];
                            if next_tag & 3 == 1 && ip + 1 < src.len() {
                                let next_offset = (usize::from(next_tag & 0xe0) << 3)
                                    | usize::from(src[ip + 1]);
                                if next_offset >= 16 && next_offset <= op {
                                    let next_len = 4 + usize::from((next_tag >> 2) & 7);
                                    ip += 2;
                                    op += next_len;
                                    stats.fused_ops += 1;
                                }
                            }
                        }
                        continue;
                    }
                }
                _ if tag & 3 == 1 => {
                    let len = 4 + usize::from((tag >> 2) & 7);
                    let offset = (usize::from(tag & 0xe0) << 3) | usize::from(src[ip]);
                    if offset >= 16 && offset <= op {
                        ip += 1;
                        op += len;
                        stats.fast_ops += 1;
                        let fused = fast_copy2_window_run(src, dst_len, &mut ip, &mut op);
                        stats.fused_ops += fused;
                        stats.window_runs[fused] += 1;
                        stats.copy1_window_runs[fused] += 1;
                        continue;
                    }
                }
                _ => {}
            }
        }

        stats.fallback_ops += 1;
        match tag & 3 {
            0 => {
                let len = literal_len(src, &mut ip, tag);
                ip += len;
                op += len;
            }
            1 => {
                let len = 4 + usize::from((tag >> 2) & 7);
                ip += 1;
                op += len;
            }
            2 => {
                let len = 1 + usize::from(tag >> 2);
                ip += 2;
                op += len;
            }
            _ => {
                let len = 1 + usize::from(tag >> 2);
                ip += 4;
                op += len;
            }
        }
    }
    stats
}

fn fast_copy2_window_run(
    src: &[u8],
    dst_len: usize,
    ip: &mut usize,
    op: &mut usize,
) -> usize {
    let mut fused = 0usize;
    for _ in 0..3 {
        if !try_fast_copy2(src, dst_len, ip, op) {
            break;
        }
        fused += 1;
    }
    fused
}

fn try_fast_copy2(src: &[u8], dst_len: usize, ip: &mut usize, op: &mut usize) -> bool {
    if 3 > src.len() - *ip {
        return false;
    }
    let tag = src[*ip];
    *ip += 1;
    if try_fast_copy2_after_tag(src, dst_len, tag, ip, op) {
        return true;
    }
    *ip -= 1;
    false
}

fn try_fast_copy2_after_tag(
    src: &[u8],
    dst_len: usize,
    tag: u8,
    ip: &mut usize,
    op: &mut usize,
) -> bool {
    if 2 > src.len() - *ip {
        return false;
    }
    let offset = u16::from_le_bytes([src[*ip], src[*ip + 1]]) as usize;
    let Some((len, min_offset)) = fast_copy2_len_and_min_offset(tag) else {
        return false;
    };
    try_fast_copy2_after_tag_with_len(dst_len, offset, len, min_offset, ip, op)
}

fn try_fast_copy2_after_tag_with_len(
    dst_len: usize,
    offset: usize,
    len: usize,
    min_offset: usize,
    ip: &mut usize,
    op: &mut usize,
) -> bool {
    if offset < min_offset || offset > *op || len > dst_len - *op {
        return false;
    }
    *ip += 2;
    *op += len;
    true
}

fn fast_copy2_len_and_min_offset(tag: u8) -> Option<(usize, usize)> {
    match tag {
        0x3e => Some((16, 16)),
        0xbe => Some((48, 48)),
        0xae => Some((44, 44)),
        0x96 => Some((38, 8)),
        _ => None,
    }
}

fn update_run(
    runs: &mut BTreeMap<(usize, usize), (usize, usize, usize)>,
    previous_copy: &mut Option<(usize, usize)>,
    current_run: &mut usize,
    len: usize,
    offset: usize,
) {
    match *previous_copy {
        Some(previous) if previous == (len, offset) => {
            *current_run += 1;
        }
        Some((previous_len, previous_offset)) => {
            record_run(runs, previous_len, previous_offset, *current_run);
            *previous_copy = Some((len, offset));
            *current_run = 1;
        }
        None => {
            *previous_copy = Some((len, offset));
            *current_run = 1;
        }
    }
}

fn record_run(
    runs: &mut BTreeMap<(usize, usize), (usize, usize, usize)>,
    len: usize,
    offset: usize,
    run: usize,
) {
    let entry = runs.entry((len, offset)).or_default();
    entry.0 += 1;
    entry.1 = entry.1.max(run);
    entry.2 += len * run;
}

fn top_alternating_runs(ops: &[CopyOp]) -> Vec<AlternatingRun> {
    let mut runs =
        BTreeMap::<((usize, usize), (usize, usize)), (usize, usize, usize)>::new();
    let mut i = 0usize;
    while i + 3 < ops.len() {
        let left = ops[i];
        let right = ops[i + 1];
        if left == right {
            i += 1;
            continue;
        }
        let mut pairs = 1usize;
        while i + pairs * 2 + 1 < ops.len()
            && ops[i + pairs * 2] == left
            && ops[i + pairs * 2 + 1] == right
        {
            pairs += 1;
        }
        if pairs > 1 {
            let key = if left <= right {
                (left, right)
            } else {
                (right, left)
            };
            let entry = runs.entry(key).or_default();
            entry.0 += 1;
            entry.1 = entry.1.max(pairs);
            entry.2 += pairs * (left.0 + right.0);
            i += pairs * 2;
        } else {
            i += 1;
        }
    }

    let mut runs = runs
        .into_iter()
        .map(|(key, (runs, max_pairs, bytes))| (key, runs, max_pairs, bytes))
        .collect::<Vec<_>>();
    runs.sort_by_key(|(_, _, max_pairs, bytes)| std::cmp::Reverse((*max_pairs, *bytes)));
    runs.truncate(12);
    runs
}

fn main() {
    analyze("mixed_256k", mixed_bytes(256 * 1024));
    analyze("arrowish_1m", arrowish_strings(1024 * 1024));
}
