# snipsnap

`snipsnap` is a small Rust implementation of Snappy raw blocks plus basic
Snappy framed streams.

The core API focuses on low-overhead raw block decompression, including
preallocated and `MaybeUninit` output buffers for callers that want to avoid
zero-filling destination memory before decoding.

## Features

- Raw Snappy block compression and decompression.
- Framed stream `Read` decoder and `Read`/`Write` encoders.
- Preallocated raw decode APIs:
  - `decompress_into`
  - `decompress_into_uninit`
  - `decompress_to_vec`
- No runtime dependencies.

## Example

```rust
let input = b"hello hello hello";
let compressed = snipsnap::compress(input)?;
let decoded = snipsnap::decompress(&compressed)?;
assert_eq!(decoded, input);
# Ok::<(), snipsnap::Error>(())
```

Using a preallocated output buffer:

```rust
let input = b"hello hello hello";
let compressed = snipsnap::compress(input)?;
let len = snipsnap::decompress_len(&compressed)?;
let mut output = vec![0; len];
let decoded = snipsnap::decompress_into(&compressed, &mut output)?;
assert_eq!(decoded, input);
# Ok::<(), snipsnap::Error>(())
```

## Performance

The raw compressor implements the C++ library's default algorithm and produces
the same bytes, so it compresses exactly as well. `examples/compare.rs`
compares both directions against the `snap` crate and, when built with
`SNAPPY_CPP_PREFIX` pointing at an installed C++ snappy, against the C++
library, on synthetic data, the standard Snappy test corpus, 4 KiB text pages
and Parquet data pages.

On one core of an Intel Xeon (Cascade Lake class, 2.8 GHz), against `snap`
1.1.1 and C++ snappy 1.3.1 built with clang 18, geometric mean over 24 cases:

| | snipsnap | `snap` | C++ |
|---|---|---|---|
| decompress | 5.1 GB/s | 2.7 GB/s | 3.1 GB/s |
| compress | 1.8 GB/s | 1.6 GB/s | 1.6 GB/s |

snipsnap decompresses text, HTML, URLs and protobuf 1.25–1.36x as fast as C++
and about 2.2x as fast as `snap`. On the Parquet pages (mostly under 100
bytes), it decompresses 1.7x as fast as `snap` and compresses 1.05x as fast.
(An earlier version measured 8.4, 4.0 and 5.1 GB/s for decompression on a
Sapphire Rapids-class Xeon, a faster machine.)

On ClickBench (`hits_0.parquet`: 728 Snappy pages, 378 MB decoded, mostly
1 MB pages of titles and URLs), decompressing the pages of the columns that
each of the 43 queries reads is 1.21x as fast as with C++ and 2.0x as fast as
with `snap`. URLs, read by 10 of the queries, are most of that work.

On Intel cores from Skylake to Cascade Lake, a 32-byte block of code holding
a branch that crosses or ends on its boundary (the JCC erratum) is decoded
again on every pass, which slowed the decoder's main loop by 10–20%
depending on where the linker placed it. That loop is aligned so that its
branches stay clear of those boundaries. Other cases can still move by
10–30% with code layout (for all three libraries), so compare best-of-several
runs. C++ snappy is tuned for clang; a GCC build decompresses 2–3x slower.

## Status

This is an initial `0.0.x` release. The framed stream support is
intentionally small and emits valid uncompressed frame chunks.
