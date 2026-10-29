//! In-process, interleaved comparison of decoder variants and C++ snappy.
#![allow(dead_code, unused_imports, clippy::all)]
use std::mem::MaybeUninit;

mod select;
mod bytes;
mod synth { #![allow(dead_code)] include!("gen.rs"); }
include!("variants.rs");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    LiteralRead { len: u64, remaining: u64 },
    LiteralWrite { len: u64, remaining: u64 },
    CopyRead { len: u64, remaining: u64 },
    CopyWrite { len: u64, remaining: u64 },
    Offset { offset: usize, dst_pos: usize },
    HeaderMismatch { expected: usize, actual: usize },
}
pub type Result<T> = std::result::Result<T, Error>;
#[cold]
#[inline(never)]
fn decode_error<T>(error: Error) -> Result<T> { Err(error) }

unsafe extern "C" {
    fn snappy_uncompress(c: *const u8, cl: usize, u: *mut u8, ul: *mut usize) -> i32;
}

fn header(src: &[u8]) -> (usize, usize) {
    let (mut v, mut shift, mut i) = (0usize, 0, 0);
    loop { let b = src[i]; v |= ((b & 0x7f) as usize) << shift; i += 1; if b < 0x80 { break } shift += 7; }
    (v, i)
}

fn count_tags(body: &[u8]) -> usize {
    let (mut ip, mut n) = (0, 0);
    while ip < body.len() {
        let tag = body[ip] as usize; n += 1;
        ip += match tag & 3 {
            0 => { let l = (tag >> 2) + 1; if l > 60 { let e = l - 60; 1 + e + 1 + body[ip+1..ip+1+e].iter().rev().fold(0, |a, &b| a * 256 + b as usize) } else { 1 + l } }
            1 => 2, 2 => 3, _ => 5,
        };
    }
    n
}

/// 300,000 dependent adds: about 300,000 core cycles.
#[inline(never)]
fn calib() -> u64 {
    let mut x: u64 = 1;
    #[cfg(target_arch = "x86_64")]
    unsafe { std::arch::asm!("mov {n}, 100000", "2:", "add {x}, {y}", "add {x}, {y}", "add {x}, {y}", "dec {n}", "jnz 2b", x = inout(reg) x, y = in(reg) 3u64, n = out(reg) _); }
    #[cfg(target_arch = "aarch64")]
    unsafe { std::arch::asm!("mov {n}, #100000", "2:", "add {x}, {x}, {y}", "add {x}, {x}, {y}", "add {x}, {x}, {y}", "subs {n}, {n}, #1", "b.ne 2b", x = inout(reg) x, y = in(reg) 3u64, n = out(reg) _); }
    x
}
#[cfg(target_arch = "x86_64")]
fn tsc() -> u64 { unsafe { core::arch::x86_64::_rdtsc() } }
#[cfg(target_arch = "aarch64")]
fn tsc() -> u64 { let t: u64; unsafe { std::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) t) }; t }

fn multi_block(f: &str, dir: &str, samples: usize, names: &[&str], glog: &mut [f64]) {
    let mut blocks = Vec::new();
    for name in ["alice29.txt", "asyoulik.txt", "lcet10.txt", "plrabn12.txt", "html", "html_x_4"] {
        let data = std::fs::read(format!("{dir}/{name}")).unwrap();
        blocks.extend(data.chunks(4096).map(<[u8]>::to_vec));
    }
    let comp: Vec<Vec<u8>> = blocks.iter().map(|b| snap::raw::Encoder::new().compress_vec(b).unwrap()).collect();
    let tags: usize = comp.iter().map(|c| { let (_, h) = header(c); count_tags(&c[h..]) }).sum();
    let mut out = vec![MaybeUninit::<u8>::uninit(); 4096];
    let mut out_cpp = vec![0u8; 4096];
    let mut best = vec![f64::MAX; names.len() + 2];
    let mut cal = f64::MAX;
    let mut snap_dec = snap::raw::Decoder::new();
    for _ in 0..samples {
        for (i, v) in VARIANTS.iter().enumerate() {
            let c0 = tsc();
            for c in &comp { let (len, h) = header(c); let _ = (v.1)(std::hint::black_box(&c[h..]), &mut out[..len]); }
            best[i] = best[i].min((tsc() - c0) as f64);
        }
        let c0 = tsc();
        for c in &comp { let mut ul = 4096; unsafe { snappy_uncompress(std::hint::black_box(c.as_ptr()), c.len(), out_cpp.as_mut_ptr(), &mut ul) }; }
        best[names.len()] = best[names.len()].min((tsc() - c0) as f64);
        let c0 = tsc();
        for c in &comp { let _ = snap_dec.decompress(std::hint::black_box(c), &mut out_cpp); }
        best[names.len() + 1] = best[names.len() + 1].min((tsc() - c0) as f64);
        let c0 = tsc(); std::hint::black_box(calib()); cal = cal.min((tsc() - c0) as f64 / 300000.0);
    }
    for v in VARIANTS.iter() {
        for (c, b) in comp.iter().zip(&blocks) {
            let (len, h) = header(c);
            (v.1)(&c[h..], &mut out[..len]).unwrap();
            let o: &[u8] = unsafe { &*(&out[..len] as *const [MaybeUninit<u8>] as *const [u8]) };
            assert!(o == b.as_slice(), "{} wrong on {f}", v.0);
        }
    }
    let bytes: usize = blocks.iter().map(Vec::len).sum();
    print!("{:<16} {:>6.2}", f, bytes as f64 / tags as f64);
    for i in 0..=names.len() + 1 {
        glog[i] += (best[i] / best[0]).ln();
        print!(" {:>9.3}", best[i] / cal / tags as f64);
    }
    print!("  ");
    for i in 1..=names.len() + 1 { print!(" {:.3}", best[0] / best[i]); }
    println!();
}


fn clickbench(f: &str, cols: &[u32], samples: usize, names: &[&str], glog: &mut [f64]) {
    let raw = std::fs::read(std::env::var("DBENCH_PAGES").expect("DBENCH_PAGES")).unwrap();
    let mut comp = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let col = u32::from_le_bytes(raw[i..i + 4].try_into().unwrap());
        let n = u32::from_le_bytes(raw[i + 8..i + 12].try_into().unwrap()) as usize;
        if cols.is_empty() || cols.contains(&col) { comp.push(raw[i + 12..i + 12 + n].to_vec()); }
        i += 12 + n;
    }
    let blocks: Vec<Vec<u8>> = comp.iter().map(|c| snap::raw::Decoder::new().decompress_vec(c).unwrap()).collect();
    let tags: usize = comp.iter().map(|c| { let (_, h) = header(c); count_tags(&c[h..]) }).sum();
    let max = blocks.iter().map(Vec::len).max().unwrap();
    let mut out = vec![MaybeUninit::<u8>::uninit(); max];
    let mut out_cpp = vec![0u8; max];
    let mut best = vec![f64::MAX; names.len() + 2];
    let mut cal = f64::MAX;
    let mut snap_dec = snap::raw::Decoder::new();
    for _ in 0..samples {
        for (i, v) in VARIANTS.iter().enumerate() {
            let c0 = tsc();
            for c in &comp { let (len, h) = header(c); let _ = (v.1)(std::hint::black_box(&c[h..]), &mut out[..len]); }
            best[i] = best[i].min((tsc() - c0) as f64);
        }
        let c0 = tsc();
        for c in &comp { let (len, _) = header(c); let mut ul = len; unsafe { snappy_uncompress(std::hint::black_box(c.as_ptr()), c.len(), out_cpp.as_mut_ptr(), &mut ul) }; }
        best[names.len()] = best[names.len()].min((tsc() - c0) as f64);
        let c0 = tsc();
        for c in &comp { let (len, _) = header(c); let _ = snap_dec.decompress(std::hint::black_box(c), &mut out_cpp[..len]); }
        best[names.len() + 1] = best[names.len() + 1].min((tsc() - c0) as f64);
        let c0 = tsc(); std::hint::black_box(calib()); cal = cal.min((tsc() - c0) as f64 / 300000.0);
    }
    if std::env::var("NOCHECK").is_err() {
        for v in VARIANTS.iter() {
            for (c, b) in comp.iter().zip(&blocks) {
                let (len, h) = header(c);
                (v.1)(&c[h..], &mut out[..len]).unwrap();
                let o: &[u8] = unsafe { &*(&out[..len] as *const [MaybeUninit<u8>] as *const [u8]) };
                assert!(o == b.as_slice(), "{} wrong on {f}", v.0);
            }
        }
    }
    let bytes: usize = blocks.iter().map(Vec::len).sum();
    print!("{:<16} {:>6.2}", f, bytes as f64 / tags as f64);
    for i in 0..=names.len() + 1 {
        glog[i] += (best[i] / best[0]).ln();
        print!(" {:>9.3}", best[i] / cal / tags as f64);
    }
    print!("  ");
    for i in 1..=names.len() + 1 { print!(" {:.3}", best[0] / best[i]); }
    println!();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let samples: usize = std::env::var("SAMPLES").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let dir = &std::env::var("DBENCH_CORPUS").expect("DBENCH_CORPUS");
    let all = ["html", "urls.10K", "alice29.txt", "asyoulik.txt", "lcet10.txt", "plrabn12.txt", "geo.protodata", "kppkn.gtb", "paper-100k.pdf", "html_x_4", "fireworks.jpeg"];
    let synth = ["@cb_title", "@cb_url", "@cb_referer", "@cb_origurl", "@cb_other", "@ints_1m", "@mixed_256k", "@arrowish_1m", "@logs_1m", "@period1_64k", "@period3_64k", "@period16_64k", "@small_4k_text", "@text_4k_blocks", "@small_256", "@small_1k"];
    let all: Vec<&str> = all.iter().copied().chain(synth).collect();
    let files: Vec<&str> = if args.is_empty() { all.to_vec() } else { all.iter().copied().filter(|f| args.iter().any(|a| f.contains(a.as_str()))).collect() };
    let names: Vec<&str> = VARIANTS.iter().map(|v| v.0).collect();
    print!("{:<16} {:>6}", "file", "B/tag");
    for n in &names { print!(" {:>9}", n); }
    println!(" {:>9} {:>9}   (core cycles/tag; ratios vs first)", "C++", "snap");
    let mut glog = vec![0f64; names.len() + 2];
    for f in &files {
        let cb: Option<Vec<u32>> = match *f {
            "@cb_title" => Some(vec![2]), "@cb_url" => Some(vec![13]), "@cb_referer" => Some(vec![14]), "@cb_origurl" => Some(vec![56]),
            "@cb_other" => Some((0..105).filter(|c| ![2, 13, 14, 56].contains(c)).collect()),
            _ => None,
        };
        if let Some(cols) = cb { clickbench(f, &cols, samples.min(8), &names, &mut glog); continue; }
        if *f == "@text_4k_blocks" {
            multi_block(f, dir, samples, &names, &mut glog);
            continue;
        }
        let data = match *f {
            "@ints_1m" => synth::int_column(1 << 20),
            "@mixed_256k" => synth::mixed_bytes(256 << 10),
            "@arrowish_1m" => synth::arrowish_strings(1 << 20),
            "@logs_1m" => synth::log_lines(1 << 20),
            "@period1_64k" => synth::period_bytes(1, 64 << 10),
            "@period3_64k" => synth::period_bytes(3, 64 << 10),
            "@period16_64k" => synth::period_bytes(16, 64 << 10),
            "@small_256" => synth::arrowish_strings(256),
            "@small_1k" => synth::arrowish_strings(1024),
            "@small_4k_text" => std::fs::read(format!("{dir}/alice29.txt")).unwrap()[..4096].to_vec(),
            _ => std::fs::read(format!("{dir}/{f}")).unwrap(),
        };
        let comp = snap::raw::Encoder::new().compress_vec(&data).unwrap();
        let (len, h) = header(&comp);
        let tags = count_tags(&comp[h..]);
        if std::env::var("STATS").is_ok() {
            let body = &comp[h..];
            let (mut ip, mut long, mut ovl, mut lit, mut c1) = (0, 0, 0, 0, 0);
            let mut runs = 0; let mut prev_long = false; let mut flips = 0;
            while ip < body.len() {
                let tag = body[ip] as usize;
                let (len, off, adv) = match tag & 3 {
                    0 => { let l = (tag >> 2) + 1; if l > 60 { let e = l - 60; let l2 = 1 + body[ip+1..ip+1+e].iter().rev().fold(0, |a, &b| a * 256 + b as usize); (l2, 0, 1 + e + l2) } else { (l, 0, 1 + l) } }
                    1 => ((tag >> 2 & 7) + 4, ((tag & 0xe0) << 3) | body[ip+1] as usize, 2),
                    2 => ((tag >> 2) + 1, body[ip+1] as usize | (body[ip+2] as usize) << 8, 3),
                    _ => ((tag >> 2) + 1, 1 << 20, 5),
                };
                if tag & 3 == 0 { lit += 1 } else if tag & 3 == 1 { c1 += 1 }
                let is_long = len > 32;
                if is_long { long += 1 }
                if is_long != prev_long { flips += 1 }
                prev_long = is_long;
                if tag & 3 != 0 && off < len { ovl += 1 }
                let _ = runs; runs = 0;
                ip += adv;
            }
            println!("{f:<16} tags {tags:>6}  len>32 {:5.1}%  flips {:5.1}%  overlap {:5.2}%  lit {:5.1}%", 100.0 * long as f64 / tags as f64, 100.0 * flips as f64 / tags as f64, 100.0 * ovl as f64 / tags as f64, 100.0 * lit as f64 / tags as f64);
            continue;
        }
        let mut out = vec![MaybeUninit::<u8>::uninit(); len];
        let mut out_cpp = vec![0u8; len];
        let reps = (2_000_000 / len).max(1).min(20_000);
        let mut best = vec![f64::MAX; names.len() + 2];
        let mut cal = f64::MAX;
        let mut snap_dec = snap::raw::Decoder::new();
        for _ in 0..samples {
            for (i, v) in VARIANTS.iter().enumerate() {
                let c0 = tsc();
                for _ in 0..reps { let _ = (v.1)(std::hint::black_box(&comp[h..]), &mut out); }
                best[i] = best[i].min((tsc() - c0) as f64 / reps as f64);
            }
            let c0 = tsc();
            for _ in 0..reps { let mut ul = len; unsafe { snappy_uncompress(std::hint::black_box(comp.as_ptr()), comp.len(), out_cpp.as_mut_ptr(), &mut ul) }; }
            best[names.len()] = best[names.len()].min((tsc() - c0) as f64 / reps as f64);
            let c0 = tsc();
            for _ in 0..reps { let _ = snap_dec.decompress(std::hint::black_box(&comp), &mut out_cpp); }
            best[names.len() + 1] = best[names.len() + 1].min((tsc() - c0) as f64 / reps as f64);
            let c0 = tsc(); std::hint::black_box(calib()); cal = cal.min((tsc() - c0) as f64 / 300000.0);
        }
        // check
        for v in VARIANTS.iter().filter(|_| std::env::var("NOCHECK").is_err()) {
            (v.1)(&comp[h..], &mut out).unwrap();
            let o: &[u8] = unsafe { &*(out.as_slice() as *const [MaybeUninit<u8>] as *const [u8]) };
            assert!(o == data.as_slice(), "{} wrong on {f}", v.0);
        }
        print!("{:<16} {:>6.2}", f, data.len() as f64 / tags as f64);
        for i in 0..=names.len() + 1 {
            let c = best[i] / cal / tags as f64;
            glog[i] += (best[i] / best[0]).ln();
            print!(" {:>9.3}", c);
        }
        print!("  ");
        for i in 1..=names.len() + 1 { print!(" {:.3}", best[0] / best[i]); }
        println!();
    }
    print!("{:<23}", "geomean speed vs first");
    for g in &glog { print!(" {:>9.3}", (-g / files.len() as f64).exp()); }
    println!();
}
