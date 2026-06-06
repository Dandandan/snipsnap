use quickcheck::{QuickCheck, StdGen};
use std::io::{BufReader, Read, Write};

// Mirrors the raw decompression coverage in BurntSushi/rust-snappy's
// `test/tests.rs` and the default framed-stream tests. The upstream suite also
// covers optional C++ interop, which is not enabled by default upstream.

fn press(bytes: &[u8]) -> Vec<u8> {
    snipsnap::compress(bytes).unwrap()
}

fn depress(bytes: &[u8]) -> Vec<u8> {
    snipsnap::decompress(bytes).unwrap()
}

fn write_frame_press(bytes: &[u8]) -> Vec<u8> {
    let mut writer = snipsnap::write::FrameEncoder::new(vec![]);
    writer.write_all(bytes).unwrap();
    writer.into_inner().unwrap()
}

fn read_frame_press(bytes: &[u8]) -> Vec<u8> {
    let mut encoded = vec![];
    snipsnap::read::FrameEncoder::new(bytes)
        .read_to_end(&mut encoded)
        .unwrap();
    encoded
}

fn read_frame_depress(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = vec![];
    snipsnap::read::FrameDecoder::new(bytes)
        .read_to_end(&mut decoded)
        .unwrap();
    decoded
}

fn roundtrip(name: &str, bytes: &[u8]) {
    let compressed = press(bytes);
    assert_eq!(
        snipsnap::decompress_len(&compressed).unwrap(),
        bytes.len(),
        "{name}"
    );
    assert_eq!(depress(&compressed), bytes, "{name}");

    let mut out = vec![0xff; bytes.len() + 17];
    let decoded = snipsnap::decompress_into(&compressed, &mut out).unwrap();
    assert_eq!(decoded, bytes, "{name} initialized output");

    assert_eq!(
        read_frame_depress(&write_frame_press(bytes)),
        bytes,
        "{name} frame write roundtrip"
    );
    assert_eq!(
        read_frame_depress(&read_frame_press(bytes)),
        bytes,
        "{name} frame read roundtrip"
    );
    assert_eq!(
        read_frame_press(bytes),
        write_frame_press(bytes),
        "{name} read/write frame encoder match"
    );
}

fn assert_decode_error(name: &str, bytes: &[u8], bad_header: bool) {
    if bad_header {
        assert!(
            snipsnap::decompress_len(bytes).is_err(),
            "{name} should fail in decompress_len"
        );
    }
    assert!(
        snipsnap::decompress(bytes).is_err(),
        "{name} should fail in decompress"
    );
}

#[test]
fn upstream_raw_roundtrip_corpus() {
    let cases: &[(&str, &[u8])] = &[
        ("empty", &[]),
        ("one_zero", &[0]),
        ("data_html", include_bytes!("rust-snappy-data/html")),
        ("data_urls", include_bytes!("rust-snappy-data/urls.10K")),
        (
            "data_jpg",
            include_bytes!("rust-snappy-data/fireworks.jpeg"),
        ),
        (
            "data_pdf",
            include_bytes!("rust-snappy-data/paper-100k.pdf"),
        ),
        ("data_html4", include_bytes!("rust-snappy-data/html_x_4")),
        ("data_txt1", include_bytes!("rust-snappy-data/alice29.txt")),
        ("data_txt2", include_bytes!("rust-snappy-data/asyoulik.txt")),
        ("data_txt3", include_bytes!("rust-snappy-data/lcet10.txt")),
        ("data_txt4", include_bytes!("rust-snappy-data/plrabn12.txt")),
        ("data_pb", include_bytes!("rust-snappy-data/geo.protodata")),
        ("data_gaviota", include_bytes!("rust-snappy-data/kppkn.gtb")),
        (
            "data_golden",
            include_bytes!("rust-snappy-data/Mark.Twain-Tom.Sawyer.txt"),
        ),
    ];

    for &(name, bytes) in cases {
        roundtrip(name, bytes);
    }
}

#[test]
fn upstream_raw_golden_compressed() {
    let compressed =
        include_bytes!("rust-snappy-data/Mark.Twain-Tom.Sawyer.txt.rawsnappy");
    let expected = include_bytes!("rust-snappy-data/Mark.Twain-Tom.Sawyer.txt");
    let decoded = depress(compressed);
    assert_eq!(decoded, expected);
    let encoded = press(&decoded);
    assert_eq!(depress(&encoded), expected);
    assert_eq!(
        snap::raw::Decoder::new().decompress_vec(&encoded).unwrap(),
        expected
    );
}

#[test]
fn upstream_frame_interoperates_with_snap() {
    let cases: &[(&str, &[u8])] = &[
        ("empty", &[]),
        ("one_zero", &[0]),
        ("data_html", include_bytes!("rust-snappy-data/html")),
        ("data_urls", include_bytes!("rust-snappy-data/urls.10K")),
        (
            "data_golden",
            include_bytes!("rust-snappy-data/Mark.Twain-Tom.Sawyer.txt"),
        ),
    ];

    for &(name, bytes) in cases {
        let snipsnap_raw = press(bytes);
        let snap_decoded = snap::raw::Decoder::new()
            .decompress_vec(&snipsnap_raw)
            .unwrap();
        assert_eq!(snap_decoded, bytes, "{name} raw snipsnap decoded by snap");

        let mut snap_encoded = snap::write::FrameEncoder::new(vec![]);
        snap_encoded.write_all(bytes).unwrap();
        let snap_encoded = snap_encoded.into_inner().unwrap();
        assert_eq!(
            read_frame_depress(&snap_encoded),
            bytes,
            "{name} snap frame decoded by snipsnap"
        );

        let snipsnap_encoded = write_frame_press(bytes);
        let mut snap_decoded = vec![];
        snap::read::FrameDecoder::new(snipsnap_encoded.as_slice())
            .read_to_end(&mut snap_decoded)
            .unwrap();
        assert_eq!(snap_decoded, bytes, "{name} snipsnap frame decoded by snap");
    }
}

#[test]
fn upstream_raw_small_copy() {
    for i in 0..32 {
        let inner = "b".repeat(i);
        roundtrip("small_copy", format!("aaaa{inner}aaaabbbb").as_bytes());
    }
}

#[test]
fn upstream_raw_small_regular() {
    let mut i = 1usize;
    while i < 20_000 {
        let mut buf = vec![0; i];
        for (j, byte) in buf.iter_mut().enumerate() {
            *byte = (j % 10) as u8 + b'a';
        }
        roundtrip("small_regular", &buf);
        i += 23;
    }
}

#[test]
fn upstream_raw_decompress_copy_close_to_end_1() {
    let compressed = [
        27,
        0b0000_1000,
        1,
        2,
        3,
        0b0000_0010,
        3,
        0,
        0b0101_1000,
        4,
        5,
        6,
        7,
        8,
        9,
        10,
        11,
        12,
        13,
        14,
        15,
        16,
        17,
        18,
        19,
        20,
        21,
        22,
        23,
        24,
        25,
        26,
    ];
    let expected = [
        1, 2, 3, 1, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22,
        23, 24, 25, 26,
    ];
    assert_eq!(depress(&compressed), expected);
}

#[test]
fn upstream_raw_decompress_copy_close_to_end_2() {
    let compressed = [
        28,
        0b0000_1000,
        1,
        2,
        3,
        0b0000_0010,
        3,
        0,
        0b0101_1100,
        4,
        5,
        6,
        7,
        8,
        9,
        10,
        11,
        12,
        13,
        14,
        15,
        16,
        17,
        18,
        19,
        20,
        21,
        22,
        23,
        24,
        25,
        26,
        27,
    ];
    let expected = [
        1, 2, 3, 1, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22,
        23, 24, 25, 26, 27,
    ];
    assert_eq!(depress(&compressed), expected);
}

#[test]
fn upstream_read_frame_encoder_big_and_little_buffers() {
    let bytes = &include_bytes!("rust-snappy-data/html")[..];

    let mut big =
        BufReader::with_capacity(1_000_000, snipsnap::read::FrameEncoder::new(bytes));
    let mut big_out = vec![];
    big.read_to_end(&mut big_out).unwrap();

    let mut little =
        BufReader::with_capacity(5, snipsnap::read::FrameEncoder::new(bytes));
    let mut little_out = vec![];
    little.read_to_end(&mut little_out).unwrap();

    assert_eq!(big_out, little_out);
}

#[test]
fn upstream_raw_malformed_inputs() {
    let cases: &[(&str, &[u8], bool)] = &[
        ("err_empty", b"", false),
        ("err_header_mismatch", b"\x05\x00a", false),
        ("err_varint1", b"\xFF", true),
        (
            "err_varint2",
            b"\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\x00",
            true,
        ),
        ("err_varint3", b"\x80\x80\x80\x80\x10", true),
        ("err_lit", b"\x02\x00hi", false),
        ("err_lit_big1", b"\x02\xechi", false),
        ("err_lit_big2a", b"\x02\xf0hi", false),
        ("err_lit_big2b", b"\x02\xf0hi\x00\x00\x00", false),
        ("err_copy1", b"\x02\x00a\x01", false),
        ("err_copy2a", b"\x11\x00a\x3e", false),
        ("err_copy2b", b"\x11\x00a\x3e\x01", false),
        ("err_copy3a", b"\x11\x00a\x3f", false),
        ("err_copy3b", b"\x11\x00a\x3f\x00", false),
        ("err_copy3c", b"\x11\x00a\x3f\x00\x00", false),
        ("err_copy3d", b"\x11\x00a\x3f\x00\x00\x00", false),
        ("err_copy_offset_zero", b"\x11\x00a\x01\x00", false),
        ("err_copy_offset_big", b"\x11\x00a\x01\xFF", false),
        ("err_copy_len_big", b"\x05\x00a\x1d\x01", false),
    ];

    for &(name, bytes, bad_header) in cases {
        assert_decode_error(name, bytes, bad_header);
    }
}

#[test]
fn upstream_raw_random_witnesses() {
    const RANDOM1: &[u8] = &[
        0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 1, 1, 0, 0,
        1, 2, 0, 0, 2, 1, 0, 0, 2, 2, 0, 0, 0, 6, 0, 0, 3, 1, 0, 0, 0, 7, 0, 0, 1, 3, 0,
        0, 0, 8, 0, 0, 2, 3, 0, 0, 0, 9, 0, 0, 1, 4, 0, 0, 1, 0, 0, 3, 0, 0, 1, 0, 1, 0,
        0, 0, 10, 0, 0, 0, 0, 2, 4, 0, 0, 2, 0, 0, 3, 0, 1, 0, 0, 1, 5, 0, 0, 6, 0, 0, 0,
        0, 11, 0, 0, 1, 6, 0, 0, 1, 7, 0, 0, 0, 12, 0, 0, 3, 2, 0, 0, 0, 13, 0, 0, 2, 5,
        0, 0, 0, 3, 3, 0, 0, 0, 1, 8, 0, 0, 1, 0, 1, 0, 0, 0, 4, 1, 0, 0, 0, 0, 14, 0, 0,
        0, 1, 9, 0, 0, 0, 1, 10, 0, 0, 0, 0, 1, 11, 0, 0, 0, 1, 0, 2, 0, 0, 0, 1, 1, 1,
        0, 0, 0, 0, 5, 1, 0, 0, 0, 1, 2, 1, 0, 0, 0, 0, 0, 2, 6, 0, 0, 0, 0, 0, 1, 12, 0,
        0, 0, 0, 0, 3, 4, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 1, 0, 3, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    const RANDOM2: &[u8] = &[
        10, 2, 14, 13, 0, 8, 2, 10, 2, 14, 13, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    const RANDOM3: &[u8] = &[
        0, 0, 0, 4, 1, 4, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    const RANDOM4: &[u8] = &[
        0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 1, 1, 0, 0,
        1, 2, 0, 0, 1, 3, 0, 0, 1, 4, 0, 0, 2, 1, 0, 0, 0, 4, 0, 1, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0,
    ];

    for &(name, bytes) in &[
        ("random1", RANDOM1),
        ("random2", RANDOM2),
        ("random3", RANDOM3),
        ("random4", RANDOM4),
    ] {
        roundtrip(name, bytes);
    }
}

#[test]
fn upstream_raw_quickcheck_roundtrip() {
    fn p(bytes: Vec<u8>) -> bool {
        depress(&press(&bytes)) == bytes
    }
    QuickCheck::new()
        .r#gen(StdGen::new(rand::thread_rng(), 10_000))
        .tests(1_000)
        .quickcheck(p as fn(Vec<u8>) -> bool);
}

#[test]
fn upstream_frame_quickcheck_roundtrip() {
    fn p(bytes: Vec<u8>) -> bool {
        read_frame_depress(&write_frame_press(&bytes)) == bytes
    }
    QuickCheck::new()
        .r#gen(StdGen::new(rand::thread_rng(), 10_000))
        .tests(1_000)
        .quickcheck(p as fn(Vec<u8>) -> bool);
}

#[test]
fn upstream_frame_short_input() {
    let err = snipsnap::read::FrameDecoder::new(&b"123"[..]).read_to_end(&mut Vec::new());
    assert_eq!(err.unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
}
