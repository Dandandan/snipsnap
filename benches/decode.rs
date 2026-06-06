use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    mem::MaybeUninit,
};

use criterion::{
    BatchSize, Criterion, Throughput, black_box, criterion_group, criterion_main,
};
#[allow(deprecated)]
use parquet::format::{PageHeader, PageType};
use parquet::{
    basic::Compression,
    file::reader::{FileReader, SerializedFileReader},
    thrift::TSerializable,
};
use thrift::protocol::TCompactInputProtocol;

#[cfg(snappy_cpp)]
mod snappy_cpp {
    use std::ffi::c_char;

    const SNAPPY_OK: i32 = 0;

    unsafe extern "C" {
        fn snappy_uncompress(
            compressed: *const c_char,
            compressed_length: usize,
            uncompressed: *mut c_char,
            uncompressed_length: *mut usize,
        ) -> i32;
    }

    pub fn uncompress(src: &[u8], dst: &mut [u8]) -> usize {
        let mut len = dst.len();
        let status = unsafe {
            snappy_uncompress(
                src.as_ptr().cast::<c_char>(),
                src.len(),
                dst.as_mut_ptr().cast::<c_char>(),
                &mut len,
            )
        };
        assert_eq!(status, SNAPPY_OK);
        len
    }
}

fn compress(data: &[u8]) -> Vec<u8> {
    snap::raw::Encoder::new().compress_vec(data).unwrap()
}

fn literal_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9u32;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push((state >> 24) as u8 ^ i as u8);
    }
    out
}

fn repeated_bytes(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(b'a' + (i & 3) as u8);
    }
    out
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

fn period_bytes(period: usize, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(b'a' + (i % period) as u8);
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

const FILE_CASES: &[(&str, &[u8])] = &[
    (
        "data_html",
        include_bytes!("../tests/rust-snappy-data/html"),
    ),
    (
        "data_urls",
        include_bytes!("../tests/rust-snappy-data/urls.10K"),
    ),
    (
        "data_jpg",
        include_bytes!("../tests/rust-snappy-data/fireworks.jpeg"),
    ),
    (
        "data_pdf",
        include_bytes!("../tests/rust-snappy-data/paper-100k.pdf"),
    ),
    (
        "data_html4",
        include_bytes!("../tests/rust-snappy-data/html_x_4"),
    ),
    (
        "data_txt1",
        include_bytes!("../tests/rust-snappy-data/alice29.txt"),
    ),
    (
        "data_txt2",
        include_bytes!("../tests/rust-snappy-data/asyoulik.txt"),
    ),
    (
        "data_txt3",
        include_bytes!("../tests/rust-snappy-data/lcet10.txt"),
    ),
    (
        "data_txt4",
        include_bytes!("../tests/rust-snappy-data/plrabn12.txt"),
    ),
    (
        "data_pb",
        include_bytes!("../tests/rust-snappy-data/geo.protodata"),
    ),
    (
        "data_gaviota",
        include_bytes!("../tests/rust-snappy-data/kppkn.gtb"),
    ),
    (
        "data_golden",
        include_bytes!("../tests/rust-snappy-data/Mark.Twain-Tom.Sawyer.txt"),
    ),
];

#[derive(Clone)]
struct SnappyBlock {
    compressed: Vec<u8>,
    uncompressed: Vec<u8>,
    uncompressed_len: usize,
}

#[allow(deprecated)]
fn read_page_header(src: &[u8]) -> Option<(usize, PageHeader)> {
    struct TrackedRead<R> {
        inner: R,
        bytes_read: usize,
    }

    impl<R: Read> Read for TrackedRead<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let read = self.inner.read(buf)?;
            self.bytes_read += read;
            Ok(read)
        }
    }

    let cursor = std::io::Cursor::new(src);
    let mut tracked = TrackedRead {
        inner: cursor,
        bytes_read: 0,
    };
    let mut protocol = TCompactInputProtocol::new(&mut tracked);
    let header = PageHeader::read_from_in_protocol(&mut protocol).ok()?;
    Some((tracked.bytes_read, header))
}

#[allow(deprecated)]
fn clickbench_snappy_blocks() -> Vec<SnappyBlock> {
    let path = "../datafusion/core/tests/data/clickbench_hits_10.parquet";
    let file = File::open(path).unwrap_or_else(|error| {
        panic!("failed to open ClickBench fixture {path}: {error}")
    });
    let reader = SerializedFileReader::new(file).unwrap();
    let metadata = reader.metadata();

    let mut file = File::open(path).unwrap();
    let mut blocks = Vec::new();
    for row_group_index in 0..metadata.num_row_groups() {
        let row_group = metadata.row_group(row_group_index);
        for column_index in 0..row_group.num_columns() {
            let column = row_group.column(column_index);
            if column.compression() != Compression::SNAPPY {
                continue;
            }

            let (start, len) = column.byte_range();
            let mut chunk = vec![0; len as usize];
            file.seek(SeekFrom::Start(start)).unwrap();
            file.read_exact(&mut chunk).unwrap();

            let mut offset = 0usize;
            while offset < chunk.len() {
                let Some((header_len, header)) = read_page_header(&chunk[offset..])
                else {
                    break;
                };
                offset += header_len;

                let compressed_len = header.compressed_page_size as usize;
                let uncompressed_len = header.uncompressed_page_size as usize;
                let end = offset + compressed_len;
                assert!(
                    end <= chunk.len(),
                    "page payload in {path} row group {row_group_index} column {column_index} exceeds chunk"
                );

                if matches!(
                    header.type_,
                    PageType::DATA_PAGE | PageType::DICTIONARY_PAGE
                ) && header.data_page_header_v2.is_none()
                    && compressed_len > 0
                    && uncompressed_len > 0
                {
                    let compressed = chunk[offset..end].to_vec();
                    if let Ok(decoded_len) = snipsnap::decompress_len(&compressed)
                        && decoded_len == uncompressed_len
                    {
                        let mut snipsnap_out = vec![0u8; uncompressed_len];
                        let snipsnap_decoded =
                            snipsnap::decompress_into(&compressed, &mut snipsnap_out)
                                .unwrap();
                        let mut snap_decoder = snap::raw::Decoder::new();
                        let snap_decoded =
                            snap_decoder.decompress_vec(&compressed).unwrap();
                        assert_eq!(snipsnap_decoded, snap_decoded.as_slice());
                        blocks.push(SnappyBlock {
                            compressed,
                            uncompressed: snap_decoded,
                            uncompressed_len,
                        });
                    }
                }

                offset = end;
            }
        }
    }

    assert!(!blocks.is_empty(), "no Snappy pages found in {path}");
    blocks
}

fn bench_case(c: &mut Criterion, name: &str, data: Vec<u8>) {
    let compressed = compress(&data);
    let decoded_len = snipsnap::decompress_len(&compressed).unwrap();
    assert_eq!(decoded_len, data.len());
    println!(
        "{name}: compressed {} -> {} bytes ({:.2}x)",
        decoded_len,
        compressed.len(),
        decoded_len as f64 / compressed.len() as f64
    );

    let mut group = c.benchmark_group(name);
    group.throughput(Throughput::Bytes(decoded_len as u64));

    group.bench_function("snipsnap_vec_spare", |b| {
        b.iter_batched(
            Vec::<u8>::new,
            |mut out| {
                let decoded =
                    snipsnap::decompress_to_vec(black_box(&compressed), &mut out)
                        .unwrap();
                black_box(decoded);
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("snipsnap_initialized_reuse", |b| {
        let mut out = vec![0u8; decoded_len];
        b.iter(|| {
            let decoded =
                snipsnap::decompress_into(black_box(&compressed), &mut out).unwrap();
            black_box(decoded);
        })
    });

    group.bench_function("snipsnap_uninit_reuse", |b| {
        let mut out = Vec::<MaybeUninit<u8>>::with_capacity(decoded_len);
        unsafe {
            out.set_len(decoded_len);
        }
        b.iter(|| {
            let decoded =
                snipsnap::decompress_into_uninit(black_box(&compressed), &mut out)
                    .unwrap();
            black_box(decoded);
        })
    });

    group.bench_function("snap_vec_zeroed", |b| {
        b.iter_batched(
            snap::raw::Decoder::new,
            |mut decoder| {
                let decoded = decoder.decompress_vec(black_box(&compressed)).unwrap();
                black_box(decoded.as_slice());
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("snap_preallocated", |b| {
        let mut decoder = snap::raw::Decoder::new();
        let mut out = vec![0u8; decoded_len];
        b.iter(|| {
            let decoded = decoder
                .decompress(black_box(&compressed), &mut out)
                .unwrap();
            black_box(&out[..decoded]);
        })
    });

    #[cfg(snappy_cpp)]
    group.bench_function("snappy_cpp_preallocated", |b| {
        let mut out = vec![0u8; decoded_len];
        b.iter(|| {
            let decoded = snappy_cpp::uncompress(black_box(&compressed), &mut out);
            black_box(&out[..decoded]);
        })
    });

    group.finish();
}

fn bench_encode_case(c: &mut Criterion, name: &str, data: Vec<u8>) {
    let max_len = snipsnap::max_compress_len(data.len());
    assert_eq!(max_len, snap::raw::max_compress_len(data.len()));

    let mut snipsnap_encoder = snipsnap::raw::Encoder::new();
    let mut snap_encoder = snap::raw::Encoder::new();
    let mut snipsnap_out = vec![0u8; max_len];
    let mut snap_out = vec![0u8; max_len];
    let snipsnap_len = snipsnap_encoder.compress(&data, &mut snipsnap_out).unwrap();
    let snap_len = snap_encoder.compress(&data, &mut snap_out).unwrap();
    assert_eq!(
        snap::raw::Decoder::new()
            .decompress_vec(&snipsnap_out[..snipsnap_len])
            .unwrap(),
        data
    );
    assert_eq!(snipsnap::decompress(&snap_out[..snap_len]).unwrap(), data);

    let mut group = c.benchmark_group(format!("encode_{name}"));
    group.throughput(Throughput::Bytes(data.len() as u64));

    group.bench_function("snipsnap_preallocated", |b| {
        let mut encoder = snipsnap::raw::Encoder::new();
        let mut out = vec![0u8; max_len];
        b.iter(|| {
            let encoded = encoder.compress(black_box(&data), &mut out).unwrap();
            black_box(&out[..encoded]);
        })
    });

    group.bench_function("snap_preallocated", |b| {
        let mut encoder = snap::raw::Encoder::new();
        let mut out = vec![0u8; max_len];
        b.iter(|| {
            let encoded = encoder.compress(black_box(&data), &mut out).unwrap();
            black_box(&out[..encoded]);
        })
    });

    group.finish();
}

fn bench_blocks(c: &mut Criterion, name: &str, blocks: Vec<SnappyBlock>) {
    let decoded_len = blocks
        .iter()
        .map(|block| block.uncompressed_len)
        .sum::<usize>();
    let compressed_len = blocks
        .iter()
        .map(|block| block.compressed.len())
        .sum::<usize>();
    println!(
        "{name}: {} pages, {} -> {} bytes ({:.2}x)",
        blocks.len(),
        decoded_len,
        compressed_len,
        decoded_len as f64 / compressed_len as f64
    );

    let mut group = c.benchmark_group(name);
    group.throughput(Throughput::Bytes(decoded_len as u64));

    group.bench_function("snipsnap_uninit_reuse", |b| {
        let mut out = blocks
            .iter()
            .map(|block| {
                let mut out =
                    Vec::<MaybeUninit<u8>>::with_capacity(block.uncompressed_len);
                unsafe {
                    out.set_len(block.uncompressed_len);
                }
                out
            })
            .collect::<Vec<_>>();
        b.iter(|| {
            for (block, out) in blocks.iter().zip(out.iter_mut()) {
                let decoded =
                    snipsnap::decompress_into_uninit(black_box(&block.compressed), out)
                        .unwrap();
                black_box(decoded);
            }
        })
    });

    group.bench_function("snap_preallocated", |b| {
        let mut decoder = snap::raw::Decoder::new();
        let mut out = blocks
            .iter()
            .map(|block| vec![0u8; block.uncompressed_len])
            .collect::<Vec<_>>();
        b.iter(|| {
            for (block, out) in blocks.iter().zip(out.iter_mut()) {
                let decoded = decoder
                    .decompress(black_box(&block.compressed), out)
                    .unwrap();
                black_box(&out[..decoded]);
            }
        })
    });

    #[cfg(snappy_cpp)]
    group.bench_function("snappy_cpp_preallocated", |b| {
        let mut out = blocks
            .iter()
            .map(|block| vec![0u8; block.uncompressed_len])
            .collect::<Vec<_>>();
        b.iter(|| {
            for (block, out) in blocks.iter().zip(out.iter_mut()) {
                let decoded = snappy_cpp::uncompress(black_box(&block.compressed), out);
                black_box(&out[..decoded]);
            }
        })
    });

    group.finish();
}

fn bench_encode_blocks(c: &mut Criterion, name: &str, blocks: Vec<SnappyBlock>) {
    let decoded_len = blocks
        .iter()
        .map(|block| block.uncompressed_len)
        .sum::<usize>();
    println!("{name}: {} pages, {decoded_len} input bytes", blocks.len());

    for block in &blocks {
        let mut snipsnap_encoder = snipsnap::raw::Encoder::new();
        let mut snap_encoder = snap::raw::Encoder::new();
        let snipsnap_encoded =
            snipsnap_encoder.compress_vec(&block.uncompressed).unwrap();
        let snap_encoded = snap_encoder.compress_vec(&block.uncompressed).unwrap();
        assert_eq!(
            snap::raw::Decoder::new()
                .decompress_vec(&snipsnap_encoded)
                .unwrap(),
            block.uncompressed
        );
        assert_eq!(
            snipsnap::decompress(&snap_encoded).unwrap(),
            block.uncompressed
        );
    }

    let mut group = c.benchmark_group(name);
    group.throughput(Throughput::Bytes(decoded_len as u64));

    group.bench_function("snipsnap_preallocated", |b| {
        let mut encoder = snipsnap::raw::Encoder::new();
        let mut out = blocks
            .iter()
            .map(|block| vec![0u8; snipsnap::max_compress_len(block.uncompressed_len)])
            .collect::<Vec<_>>();
        b.iter(|| {
            for (block, out) in blocks.iter().zip(out.iter_mut()) {
                let encoded = encoder
                    .compress(black_box(&block.uncompressed), out)
                    .unwrap();
                black_box(&out[..encoded]);
            }
        })
    });

    group.bench_function("snap_preallocated", |b| {
        let mut encoder = snap::raw::Encoder::new();
        let mut out = blocks
            .iter()
            .map(|block| vec![0u8; snap::raw::max_compress_len(block.uncompressed_len)])
            .collect::<Vec<_>>();
        b.iter(|| {
            for (block, out) in blocks.iter().zip(out.iter_mut()) {
                let encoded = encoder
                    .compress(black_box(&block.uncompressed), out)
                    .unwrap();
                black_box(&out[..encoded]);
            }
        })
    });

    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let cases = [
        ("small_text_256", arrowish_strings(256)),
        ("literal_64k", literal_bytes(64 * 1024)),
        ("period1_64k", period_bytes(1, 64 * 1024)),
        ("period3_64k", period_bytes(3, 64 * 1024)),
        ("period8_64k", period_bytes(8, 64 * 1024)),
        ("period16_64k", period_bytes(16, 64 * 1024)),
        ("repeated_64k", repeated_bytes(64 * 1024)),
        ("mixed_256k", mixed_bytes(256 * 1024)),
        ("arrowish_1m", arrowish_strings(1024 * 1024)),
    ];

    for (name, data) in cases {
        bench_case(c, name, data.clone());
        bench_encode_case(c, name, data);
    }

    for &(name, data) in FILE_CASES {
        bench_case(c, name, data.to_vec());
        bench_encode_case(c, name, data.to_vec());
    }

    let clickbench_blocks = clickbench_snappy_blocks();
    bench_blocks(c, "clickbench_pages", clickbench_blocks.clone());
    bench_encode_blocks(c, "encode_clickbench_pages", clickbench_blocks);
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
