//! Framed-stream decoding with read buffers smaller and larger than a chunk,
//! which take the decoder's buffered and direct paths.

use std::io::{ErrorKind, Read, Write};

/// Reads all of `stream` with reads of at most `step` bytes.
fn read_in_steps(stream: &[u8], step: usize) -> std::io::Result<Vec<u8>> {
    let mut decoder = snipsnap::read::FrameDecoder::new(stream);
    let mut out = Vec::new();
    let mut buf = vec![0; step];
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

fn data() -> Vec<u8> {
    let text = include_bytes!("rust-snappy-data/html");
    let mut data: Vec<u8> = text.iter().copied().cycle().take(200_000).collect();
    // An incompressible stretch, which snap stores as uncompressed chunks.
    let mut x = 0x9e37_79b9u32;
    data.extend((0..150_000).map(|_| {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x as u8
    }));
    data
}

fn streams(data: &[u8]) -> [(&'static str, Vec<u8>); 2] {
    let mut snap_encoder = snap::write::FrameEncoder::new(vec![]);
    snap_encoder.write_all(data).unwrap();
    let mut snipsnap_encoder = snipsnap::write::FrameEncoder::new(vec![]);
    snipsnap_encoder.write_all(data).unwrap();
    [
        ("snap", snap_encoder.into_inner().unwrap()),
        ("snipsnap", snipsnap_encoder.into_inner().unwrap()),
    ]
}

#[test]
fn frames_decode_with_any_read_size() {
    let data = data();
    for (name, stream) in streams(&data) {
        for step in [1, 7, 4096, 65_535, 65_536, 65_537, 1 << 20] {
            let out = read_in_steps(&stream, step).unwrap();
            assert!(out == data, "{name} stream, reads of {step} bytes");
        }
        let mut out = Vec::new();
        snipsnap::read::FrameDecoder::new(&stream[..])
            .read_to_end(&mut out)
            .unwrap();
        assert!(out == data, "{name} stream, read_to_end");
    }
}

#[test]
fn corrupted_frames_fail_with_any_read_size() {
    let data = data();
    for (name, stream) in streams(&data) {
        // The stored checksum of the second chunk, after the 10-byte stream
        // identifier and the first chunk: the data still decodes, so only
        // the checksum test can catch it.
        let first_len = usize::from(stream[11])
            | usize::from(stream[12]) << 8
            | usize::from(stream[13]) << 16;
        let at = 10 + 4 + first_len + 4;
        let mut bad = stream.clone();
        bad[at] ^= 1;
        for step in [1, 4096, 1 << 20] {
            let err = read_in_steps(&bad, step).unwrap_err();
            assert_eq!(
                err.kind(),
                ErrorKind::InvalidData,
                "{name}, reads of {step}"
            );
        }
    }
}
