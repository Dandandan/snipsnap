//! Head-to-head comparison of snipsnap, the `snap` crate and (optionally) the
//! C++ snappy library.
//!
//! ```text
//! SNAPPY_CPP_PREFIX=/path/to/snappy cargo run --release --example compare
//! cargo run --release --example compare -- --filter txt --samples 30
//! cargo run --release --example compare -- --only decode:snipsnap:data_html --iters 100
//! ```
//!
//! Timings are the minimum over interleaved samples, so every implementation
//! sees the same machine state. `cyc/B` uses the time stamp counter, i.e.
//! reference cycles. `--only` runs a single operation `--iters` times without
//! timing so that tools like `valgrind --tool=cachegrind` can count
//! instructions per call.

use std::hint::black_box;
use std::mem::MaybeUninit;
use std::time::Instant;

#[cfg(snappy_cpp)]
mod cpp {
    use std::ffi::c_char;

    unsafe extern "C" {
        fn snappy_uncompress(
            compressed: *const c_char,
            compressed_length: usize,
            uncompressed: *mut c_char,
            uncompressed_length: *mut usize,
        ) -> i32;
        fn snappy_compress(
            input: *const c_char,
            input_length: usize,
            compressed: *mut c_char,
            compressed_length: *mut usize,
        ) -> i32;
        fn snappy_max_compressed_length(source_length: usize) -> usize;
    }

    pub fn uncompress(src: &[u8], dst: &mut [u8]) -> usize {
        let mut len = dst.len();
        let status = unsafe {
            snappy_uncompress(
                src.as_ptr().cast(),
                src.len(),
                dst.as_mut_ptr().cast(),
                &mut len,
            )
        };
        assert_eq!(status, 0);
        len
    }

    pub fn compress(src: &[u8], dst: &mut [u8]) -> usize {
        let mut len = dst.len();
        let status = unsafe {
            snappy_compress(
                src.as_ptr().cast(),
                src.len(),
                dst.as_mut_ptr().cast(),
                &mut len,
            )
        };
        assert_eq!(status, 0);
        len
    }

    pub fn max_compressed_len(len: usize) -> usize {
        unsafe { snappy_max_compressed_length(len) }
    }
}

#[inline(always)]
fn ticks() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_rdtsc()
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let value: u64;
        std::arch::asm!("mrs {}, cntvct_el0", out(reg) value);
        value
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        0
    }
}

struct Case {
    name: String,
    /// Uncompressed blocks. Most cases have one; parquet pages have many.
    blocks: Vec<Vec<u8>>,
}

impl Case {
    fn single(name: &str, data: Vec<u8>) -> Self {
        Self {
            name: name.to_string(),
            blocks: vec![data],
        }
    }

    fn len(&self) -> usize {
        self.blocks.iter().map(Vec::len).sum()
    }
}

fn literal_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9u32;
    (0..len)
        .map(|i| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8 ^ i as u8
        })
        .collect()
}

fn period_bytes(period: usize, len: usize) -> Vec<u8> {
    (0..len).map(|i| b'a' + (i % period) as u8).collect()
}

fn mixed_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| match i & 63 {
            0..=47 => b'0' + (i % 10) as u8,
            _ => ((i * 31) >> 3) as u8,
        })
        .collect()
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

/// Pseudo-random, log-like text: low-entropy tokens drawn from a vocabulary,
/// which yields a realistic mix of short literals and short copies.
fn log_lines(len: usize) -> Vec<u8> {
    const LEVELS: &[&str] = &["INFO", "WARN", "DEBUG", "ERROR"];
    const SERVICES: &[&str] = &["api", "auth", "billing", "search", "ingest", "web"];
    const PATHS: &[&str] = &[
        "/v1/users",
        "/v1/orders",
        "/health",
        "/v2/search?q=",
        "/static/app.js",
    ];
    let mut state = 0x2545_f491u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut out = Vec::with_capacity(len + 256);
    while out.len() < len {
        let r = next();
        let line = format!(
            "2026-10-{:02}T{:02}:{:02}:{:02}.{:03}Z {} {} request_id={:016x} path={}{} status={} latency_ms={}\n",
            1 + r % 28,
            (r >> 5) % 24,
            (r >> 10) % 60,
            (r >> 16) % 60,
            (r >> 22) % 1000,
            LEVELS[(r >> 32) as usize % LEVELS.len()],
            SERVICES[(r >> 36) as usize % SERVICES.len()],
            next() & 0xffff_ffff,
            PATHS[(r >> 40) as usize % PATHS.len()],
            (r >> 44) % 100,
            [200, 200, 200, 404, 500][(r >> 52) as usize % 5],
            (r >> 56) % 250,
        );
        out.extend_from_slice(line.as_bytes());
    }
    out.truncate(len);
    out
}

/// Little-endian integers with small deltas, as found in columnar data.
fn int_column(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut value = 1_000_000u32;
    let mut state = 0x1234_5678u32;
    while out.len() + 4 <= len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        value = value.wrapping_add((state >> 28) & 3);
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.resize(len, 0);
    out
}

const FILE_CASES: &[(&str, &[u8])] = &[
    ("html", include_bytes!("../tests/rust-snappy-data/html")),
    ("urls", include_bytes!("../tests/rust-snappy-data/urls.10K")),
    (
        "jpg",
        include_bytes!("../tests/rust-snappy-data/fireworks.jpeg"),
    ),
    (
        "pdf",
        include_bytes!("../tests/rust-snappy-data/paper-100k.pdf"),
    ),
    (
        "html4",
        include_bytes!("../tests/rust-snappy-data/html_x_4"),
    ),
    (
        "txt1",
        include_bytes!("../tests/rust-snappy-data/alice29.txt"),
    ),
    (
        "txt2",
        include_bytes!("../tests/rust-snappy-data/asyoulik.txt"),
    ),
    (
        "txt3",
        include_bytes!("../tests/rust-snappy-data/lcet10.txt"),
    ),
    (
        "txt4",
        include_bytes!("../tests/rust-snappy-data/plrabn12.txt"),
    ),
    (
        "pb",
        include_bytes!("../tests/rust-snappy-data/geo.protodata"),
    ),
    (
        "gaviota",
        include_bytes!("../tests/rust-snappy-data/kppkn.gtb"),
    ),
];

#[allow(deprecated)]
fn parquet_pages(path: &str) -> Option<Vec<Vec<u8>>> {
    #[allow(deprecated)]
    use parquet::format::{PageHeader, PageType};
    use parquet::{
        basic::Compression,
        file::reader::{FileReader, SerializedFileReader},
        thrift::TSerializable,
    };
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    use thrift::protocol::TCompactInputProtocol;

    let reader = SerializedFileReader::new(File::open(path).ok()?).ok()?;
    let metadata = reader.metadata();
    let mut file = File::open(path).ok()?;
    let mut pages = Vec::new();
    for rg in 0..metadata.num_row_groups() {
        let row_group = metadata.row_group(rg);
        for ci in 0..row_group.num_columns() {
            let column = row_group.column(ci);
            if column.compression() != Compression::SNAPPY {
                continue;
            }
            let (start, len) = column.byte_range();
            let mut chunk = vec![0; len as usize];
            file.seek(SeekFrom::Start(start)).ok()?;
            file.read_exact(&mut chunk).ok()?;
            let mut offset = 0usize;
            while offset < chunk.len() {
                let mut cursor = std::io::Cursor::new(&chunk[offset..]);
                #[allow(deprecated)]
                let Ok(header) = ({
                    let mut protocol = TCompactInputProtocol::new(&mut cursor);
                    PageHeader::read_from_in_protocol(&mut protocol)
                }) else {
                    break;
                };
                offset += cursor.position() as usize;
                let end = offset + header.compressed_page_size as usize;
                #[allow(deprecated)]
                let is_data = matches!(
                    header.type_,
                    PageType::DATA_PAGE | PageType::DICTIONARY_PAGE
                );
                if is_data
                    && header.data_page_header_v2.is_none()
                    && header.compressed_page_size > 0
                    && end <= chunk.len()
                {
                    if let Ok(page) =
                        snap::raw::Decoder::new().decompress_vec(&chunk[offset..end])
                    {
                        pages.push(page);
                    }
                }
                offset = end;
            }
        }
    }
    (!pages.is_empty()).then_some(pages)
}

fn cases() -> Vec<Case> {
    let mut cases = vec![
        Case::single("small_text_256", arrowish_strings(256)),
        Case::single("literal_64k", literal_bytes(64 * 1024)),
        Case::single("period1_64k", period_bytes(1, 64 * 1024)),
        Case::single("period3_64k", period_bytes(3, 64 * 1024)),
        Case::single("period8_64k", period_bytes(8, 64 * 1024)),
        Case::single("period16_64k", period_bytes(16, 64 * 1024)),
        Case::single("mixed_256k", mixed_bytes(256 * 1024)),
        Case::single("arrowish_1m", arrowish_strings(1024 * 1024)),
        Case::single("logs_1m", log_lines(1024 * 1024)),
        Case::single("ints_1m", int_column(1024 * 1024)),
    ];
    for &(name, data) in FILE_CASES {
        cases.push(Case::single(name, data.to_vec()));
    }
    // The full standard corpus concatenated: a large, multi-block input.
    let mut all = Vec::new();
    for &(_, data) in FILE_CASES {
        all.extend_from_slice(data);
    }
    cases.push(Case::single("corpus_concat", all));
    // Small blocks: the text files split into 4 KiB pages, a common
    // size for database pages and RPC payloads.
    let mut small = Vec::new();
    for &(name, data) in FILE_CASES {
        if name.starts_with("txt") || name.starts_with("html") {
            small.extend(data.chunks(4096).map(<[u8]>::to_vec));
        }
    }
    cases.push(Case {
        name: "text_4k_blocks".to_string(),
        blocks: small,
    });
    let clickbench = std::env::var("SNIPSNAP_PARQUET_BENCH").unwrap_or_else(|_| {
        "../datafusion/core/tests/data/clickbench_hits_10.parquet".to_string()
    });
    if let Some(pages) = parquet_pages(&clickbench) {
        cases.push(Case {
            name: "parquet_pages".to_string(),
            blocks: pages,
        });
    }
    cases
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Decode,
    Encode,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Imp {
    Snipsnap,
    Snap,
    #[cfg(snappy_cpp)]
    Cpp,
}

impl Imp {
    fn all() -> Vec<Imp> {
        vec![
            Imp::Snipsnap,
            Imp::Snap,
            #[cfg(snappy_cpp)]
            Imp::Cpp,
        ]
    }

    fn name(self) -> &'static str {
        match self {
            Imp::Snipsnap => "snipsnap",
            Imp::Snap => "snap",
            #[cfg(snappy_cpp)]
            Imp::Cpp => "cpp",
        }
    }
}

/// Pre-built state so that the timed closure does no allocation.
struct Runner {
    imp: Imp,
    op: Op,
    inputs: Vec<Vec<u8>>,
    outputs: Vec<Vec<u8>>,
    uninit: Vec<Vec<MaybeUninit<u8>>>,
    snap_dec: snap::raw::Decoder,
    snap_enc: snap::raw::Encoder,
    snipsnap_enc: snipsnap::raw::Encoder,
}

impl Runner {
    fn new(imp: Imp, op: Op, case: &Case, compressed: &[Vec<u8>]) -> Self {
        let (inputs, out_lens): (Vec<Vec<u8>>, Vec<usize>) = match op {
            Op::Decode => (
                compressed.to_vec(),
                case.blocks.iter().map(Vec::len).collect(),
            ),
            Op::Encode => (
                case.blocks.clone(),
                case.blocks
                    .iter()
                    .map(|b| {
                        #[cfg(snappy_cpp)]
                        let cpp = cpp::max_compressed_len(b.len());
                        #[cfg(not(snappy_cpp))]
                        let cpp = 0;
                        snipsnap::max_compress_len(b.len()).max(cpp)
                    })
                    .collect(),
            ),
        };
        let outputs = out_lens.iter().map(|&n| vec![0u8; n]).collect();
        let uninit = out_lens
            .iter()
            .map(|&n| {
                let mut v = Vec::with_capacity(n);
                // SAFETY: MaybeUninit<u8> needs no initialization.
                unsafe { v.set_len(n) };
                v
            })
            .collect();
        Self {
            imp,
            op,
            inputs,
            outputs,
            uninit,
            snap_dec: snap::raw::Decoder::new(),
            snap_enc: snap::raw::Encoder::new(),
            snipsnap_enc: snipsnap::raw::Encoder::new(),
        }
    }

    /// Runs the operation over every block once; returns total output bytes.
    #[inline(never)]
    fn run(&mut self) -> usize {
        let mut total = 0;
        for i in 0..self.inputs.len() {
            let input = black_box(&self.inputs[i]);
            total += match (self.imp, self.op) {
                (Imp::Snipsnap, Op::Decode) => {
                    snipsnap::decompress_into_uninit(input, &mut self.uninit[i])
                        .unwrap()
                        .len()
                }
                (Imp::Snap, Op::Decode) => self
                    .snap_dec
                    .decompress(input, &mut self.outputs[i])
                    .unwrap(),
                #[cfg(snappy_cpp)]
                (Imp::Cpp, Op::Decode) => cpp::uncompress(input, &mut self.outputs[i]),
                (Imp::Snipsnap, Op::Encode) => self
                    .snipsnap_enc
                    .compress(input, &mut self.outputs[i])
                    .unwrap(),
                (Imp::Snap, Op::Encode) => {
                    self.snap_enc.compress(input, &mut self.outputs[i]).unwrap()
                }
                #[cfg(snappy_cpp)]
                (Imp::Cpp, Op::Encode) => cpp::compress(input, &mut self.outputs[i]),
            };
        }
        black_box(&self.outputs);
        black_box(&self.uninit);
        total
    }
}

struct Args {
    filter: Vec<String>,
    samples: usize,
    sample_ms: f64,
    only: Option<(Op, String, String)>,
    iters: usize,
    ops: Vec<Op>,
    stats: bool,
    identical: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        filter: Vec::new(),
        samples: 15,
        sample_ms: 4.0,
        only: None,
        iters: 1,
        ops: vec![Op::Decode, Op::Encode],
        stats: false,
        identical: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--filter" => args.filter.push(it.next().expect("--filter value")),
            "--samples" => args.samples = it.next().unwrap().parse().unwrap(),
            "--sample-ms" => args.sample_ms = it.next().unwrap().parse().unwrap(),
            "--iters" => args.iters = it.next().unwrap().parse().unwrap(),
            "--stats" => args.stats = true,
            "--identical" => args.identical = true,
            "--decode" => args.ops = vec![Op::Decode],
            "--encode" => args.ops = vec![Op::Encode],
            "--only" => {
                let spec = it.next().expect("--only op:impl:case");
                let mut parts = spec.splitn(3, ':');
                let op = match parts.next().unwrap() {
                    "decode" => Op::Decode,
                    "encode" => Op::Encode,
                    other => panic!("unknown op {other}"),
                };
                args.only = Some((
                    op,
                    parts.next().unwrap().to_string(),
                    parts.next().unwrap().to_string(),
                ));
            }
            "--bench" => {}
            other => panic!("unknown argument {other}"),
        }
    }
    args
}

fn main() {
    let args = parse_args();
    let cases: Vec<Case> = cases()
        .into_iter()
        .filter(|c| {
            args.filter.is_empty() || args.filter.iter().any(|f| c.name.contains(f))
        })
        .collect();

    if args.stats {
        print_stats(&cases);
        return;
    }

    if args.identical {
        check_identical(&cases);
        return;
    }

    if let Some((op, imp_name, case_name)) = &args.only {
        let case = cases
            .iter()
            .find(|c| &c.name == case_name)
            .expect("unknown case");
        let imp = *Imp::all()
            .iter()
            .find(|i| i.name() == imp_name)
            .expect("unknown impl");
        let compressed = compress_with_snap(case);
        let mut runner = Runner::new(imp, *op, case, &compressed);
        let mut total = 0;
        for _ in 0..args.iters {
            total += runner.run();
        }
        println!("{total}");
        return;
    }

    let imps = Imp::all();
    println!(
        "{:<16} {:>9} | {}",
        "case",
        "bytes",
        imps.iter()
            .map(|i| format!("{:>10} {:>6}", format!("{} MB/s", i.name()), "cyc/B"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    for &op in &args.ops {
        println!("--- {op:?} ---");
        let mut geo = vec![0f64; imps.len()];
        let mut count = 0usize;
        for case in &cases {
            let compressed = compress_with_snap(case);
            verify(case, &compressed);
            let mut runners: Vec<Runner> = imps
                .iter()
                .map(|&imp| Runner::new(imp, op, case, &compressed))
                .collect();
            let bytes = case.len();

            // Calibrate a batch size so one sample takes about `sample_ms`.
            let start = Instant::now();
            let mut calib = 0usize;
            while start.elapsed().as_secs_f64() * 1e3 < args.sample_ms {
                runners[0].run();
                calib += 1;
            }
            let batch = calib.max(1);

            let mut best_ns = vec![f64::MAX; imps.len()];
            let mut best_ticks = vec![f64::MAX; imps.len()];
            for _ in 0..args.samples {
                for (k, runner) in runners.iter_mut().enumerate() {
                    let t0 = Instant::now();
                    let c0 = ticks();
                    for _ in 0..batch {
                        black_box(runner.run());
                    }
                    let c1 = ticks();
                    let ns = t0.elapsed().as_nanos() as f64 / batch as f64;
                    best_ns[k] = best_ns[k].min(ns);
                    best_ticks[k] = best_ticks[k].min((c1 - c0) as f64 / batch as f64);
                }
            }

            let mut line = format!("{:<16} {:>9} |", case.name, bytes);
            for k in 0..imps.len() {
                let mbs = bytes as f64 / best_ns[k] * 1e3;
                geo[k] += mbs.ln();
                line.push_str(&format!(
                    " {:>10.0} {:>6.3} |",
                    mbs,
                    best_ticks[k] / bytes as f64
                ));
            }
            if op == Op::Encode {
                let ratios: Vec<String> = runners
                    .iter_mut()
                    .map(|r| {
                        let out = r.run();
                        format!("{:.3}", bytes as f64 / out as f64)
                    })
                    .collect();
                line.push_str(&format!(" ratio {}", ratios.join("/")));
            } else {
                let rel: Vec<String> = (1..imps.len())
                    .map(|k| format!("{:.2}x", best_ns[k] / best_ns[0]))
                    .collect();
                line.push_str(&format!(" speedup {}", rel.join("/")));
            }
            println!("{line}");
            count += 1;
        }
        let mut line = format!("{:<16} {:>9} |", "geomean", "");
        for g in &geo {
            line.push_str(&format!(" {:>10.0} {:>6} |", (g / count as f64).exp(), ""));
        }
        println!("{line}");
    }
}

/// Checks that snipsnap's compressed output is byte-identical to the C++
/// library's (true when the C++ library uses its portable hash).
fn check_identical(cases: &[Case]) {
    #[cfg(snappy_cpp)]
    for case in cases {
        let mut differing = 0;
        for block in &case.blocks {
            let ours = snipsnap::compress(block).unwrap();
            let mut theirs = vec![0u8; cpp::max_compressed_len(block.len())];
            let n = cpp::compress(block, &mut theirs);
            differing += usize::from(ours != theirs[..n]);
        }
        println!(
            "{:<16} {} of {} blocks differ",
            case.name,
            differing,
            case.blocks.len()
        );
    }
    #[cfg(not(snappy_cpp))]
    {
        let _ = cases;
        println!("built without SNAPPY_CPP_PREFIX");
    }
}

/// Prints the tag mix of the snap-compressed form of each case.
fn print_stats(cases: &[Case]) {
    println!(
        "{:<16} {:>8} {:>8} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}",
        "case",
        "tags",
        "B/tag",
        "lit%",
        "litlen",
        "cp1%",
        "cp2%",
        "cplen",
        "ovl%",
        "off<16",
        "off<32",
        "off<64"
    );
    for case in cases {
        let (mut tags, mut lits, mut lit_bytes, mut cp1, mut cp2, mut cp_bytes, mut ovl) =
            (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
        let mut near = [0usize; 3];
        for comp in compress_with_snap(case) {
            let mut ip = 0;
            while comp[ip] >= 0x80 {
                ip += 1;
            }
            ip += 1;
            while ip < comp.len() {
                let tag = comp[ip];
                ip += 1;
                tags += 1;
                let (len, offset) = match tag & 3 {
                    0 => {
                        let mut len = usize::from(tag >> 2) + 1;
                        if len > 60 {
                            let n = len - 60;
                            len = 1;
                            for i in 0..n {
                                len += usize::from(comp[ip + i]) << (8 * i);
                            }
                            ip += n;
                        }
                        ip += len;
                        lits += 1;
                        lit_bytes += len;
                        continue;
                    }
                    1 => {
                        cp1 += 1;
                        ip += 1;
                        (
                            4 + usize::from((tag >> 2) & 7),
                            (usize::from(tag & 0xe0) << 3) | usize::from(comp[ip - 1]),
                        )
                    }
                    2 => {
                        cp2 += 1;
                        ip += 2;
                        (
                            1 + usize::from(tag >> 2),
                            usize::from(u16::from_le_bytes([comp[ip - 2], comp[ip - 1]])),
                        )
                    }
                    _ => {
                        ip += 4;
                        (1 + usize::from(tag >> 2), usize::MAX)
                    }
                };
                cp_bytes += len;
                ovl += usize::from(offset < len);
                for (k, limit) in [16, 32, 64].into_iter().enumerate() {
                    near[k] += usize::from(offset < limit);
                }
            }
        }
        if case.blocks.len() > 1 {
            println!("  {} blocks", case.blocks.len());
            let sizes: Vec<String> = compress_with_snap(case)
                .iter()
                .zip(&case.blocks)
                .map(|(c, b)| format!("{}->{}", b.len(), c.len()))
                .collect();
            println!("  blocks: {}", sizes.join(" "));
        }
        let copies = (tags - lits).max(1);
        println!(
            "{:<16} {:>8} {:>8.2} {:>7.1} {:>7.2} {:>7.1} {:>7.1} {:>7.2} {:>7.2} {:>7.1} {:>7.1} {:>7.1}",
            case.name,
            tags,
            case.len() as f64 / tags as f64,
            100.0 * lits as f64 / tags as f64,
            lit_bytes as f64 / lits.max(1) as f64,
            100.0 * cp1 as f64 / tags as f64,
            100.0 * cp2 as f64 / tags as f64,
            cp_bytes as f64 / copies as f64,
            100.0 * ovl as f64 / copies as f64,
            100.0 * near[0] as f64 / copies as f64,
            100.0 * near[1] as f64 / copies as f64,
            100.0 * near[2] as f64 / copies as f64,
        );
    }
}

fn compress_with_snap(case: &Case) -> Vec<Vec<u8>> {
    case.blocks
        .iter()
        .map(|b| snap::raw::Encoder::new().compress_vec(b).unwrap())
        .collect()
}

fn verify(case: &Case, compressed: &[Vec<u8>]) {
    for (block, comp) in case.blocks.iter().zip(compressed) {
        assert_eq!(&snipsnap::decompress(comp).unwrap(), block, "{}", case.name);
        let ours = snipsnap::compress(block).unwrap();
        assert_eq!(
            &snap::raw::Decoder::new().decompress_vec(&ours).unwrap(),
            block,
            "{}",
            case.name
        );
        #[cfg(snappy_cpp)]
        {
            let mut out = vec![0u8; block.len()];
            let n = cpp::uncompress(&ours, &mut out);
            assert_eq!(
                &out[..n],
                &block[..],
                "{} cpp decode of snipsnap",
                case.name
            );
        }
    }
}
