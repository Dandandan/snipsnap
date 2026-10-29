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

Geometric means over 24 cases, on one core, against `snap` 1.1.1 and C++
snappy 1.3.1 built with clang 18:

| | snipsnap | `snap` | C++ |
|---|---|---|---|
| decompress, Intel Xeon (Cascade Lake class, 2.8 GHz) | 5.9 GB/s | 3.3 GB/s | 4.2 GB/s |
| decompress, Arm Neoverse-N2 (GitHub's Arm runner) | 8.7 GB/s | 4.6 GB/s | 6.9 GB/s |
| compress, Intel Xeon | 2.3 GB/s | 2.1 GB/s | 2.2 GB/s |
| compress, Arm Neoverse-N2 | 3.1 GB/s | 2.6 GB/s | 2.9 GB/s |

Most of the lead comes from integer columns, run-length-like data and
small blocks:

- Integer columns decompress 1.0–2.2x as fast as C++.
- Periodic data decompresses 1.6–9.7x as fast on x86 and 2.4–3.2x on Arm.
- 256-byte blocks decompress 2.9x as fast.

Text, HTML, URLs and protobuf decompress 1.02–1.06x as fast as C++ on Arm
and at 0.96–1.01x its speed on x86, and 1.4–2.1x as fast as `snap`.

On ClickBench (`hits_0.parquet`: 728 Snappy pages, 378 MB decoded, mostly
1 MB pages of titles and URLs), the pages decompress 1.02x as fast as C++
on Arm and at 0.97x its speed on x86, and 1.5–1.6x as fast as `snap`.
Weighted by the columns each of the 43 queries reads, x86 decompression is
0.99x as fast as C++. URLs, read by 10 of the queries, are most of that
work.

On Intel cores from Skylake to Cascade Lake with Intel's microcode update
for the JCC erratum, a 32-byte block of code holding a branch that crosses
or ends on its boundary is decoded again on every pass. That slowed the
decoders' main loops by 10–20%, depending on where the linker placed them.
snipsnap's loop is aligned so that its branches stay clear of those
boundaries; three of the C++ library's do not. On such a host, an earlier
version of snipsnap measured 1.21x as fast as C++ on the query-weighted
ClickBench pages, and 5.1 GB/s against 3.1 GB/s on the 24 cases.

Other cases can still move by 10–30% with code layout (for all three
libraries), so compare best-of-several runs. C++ snappy is tuned for clang;
a GCC build decompresses 2–3x slower.

## Status

This is an initial `0.0.x` release. The framed stream support is
intentionally small and emits valid uncompressed frame chunks.
