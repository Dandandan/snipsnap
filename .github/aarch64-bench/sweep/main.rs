//! Decodes and encodes a 64 KiB incompressible block with the output at 64
//! offsets across a 4 KiB page, and prints the spread of the speeds.
use std::time::Instant;

fn spread(name: &str, mut speeds: Vec<f64>) {
    speeds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| speeds[((speeds.len() - 1) as f64 * q) as usize];
    println!(
        "{name:<8} GB/s min {:6.1}  p10 {:6.1}  median {:6.1}  max {:6.1}",
        at(0.0), at(0.1), at(0.5), at(1.0)
    );
}

fn main() {
    let n = 1 << 16;
    let mut state = 0x9e37_79b9u32;
    let data: Vec<u8> = (0..n)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect();
    let compressed = snipsnap::compress(&data).unwrap();
    let mut out = vec![0u8; n + 8192];
    let mut enc_out = vec![0u8; snipsnap::max_compress_len(n) + 8192];
    let mut encoder = snipsnap::raw::Encoder::new();
    let (mut dec, mut enc) = (Vec::new(), Vec::new());
    for d in (0..4096).step_by(64) {
        let time = |f: &mut dyn FnMut()| {
            for _ in 0..20 { f(); }
            let start = Instant::now();
            let mut reps = 0u32;
            while start.elapsed().as_millis() < 15 { f(); reps += 1; }
            reps as f64 * n as f64 / start.elapsed().as_secs_f64() / 1e9
        };
        let dst = &mut out[d..d + n];
        dec.push(time(&mut || { snipsnap::decompress_into(&compressed, dst).unwrap(); }));
        let dst = &mut enc_out[d..];
        enc.push(time(&mut || { encoder.compress(&data, dst).unwrap(); }));
    }
    spread("decode", dec);
    spread("encode", enc);
}
