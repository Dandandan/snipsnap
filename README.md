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
| decompress, Intel Xeon (Cascade Lake class, 2.8 GHz) | 6.6 GB/s | 3.3 GB/s | 4.2 GB/s |
| decompress, AMD EPYC 7763 (Zen 3, GitHub's x86 runner) | 4.9 GB/s | 2.7 GB/s | 3.2 GB/s |
| decompress, AMD EPYC 9V45 (Zen 5, GitHub's x86 runner) | 12.7 GB/s | 5.6 GB/s | 7.0 GB/s |
| decompress, Arm Neoverse-N2 (GitHub's Arm runner) | 8.9 GB/s | 4.7 GB/s | 6.9 GB/s |
| compress, Intel Xeon | 2.4 GB/s | 2.0 GB/s | 2.2 GB/s |
| compress, AMD EPYC 7763 | 2.3 GB/s | 2.1 GB/s | 2.3 GB/s |
| compress, AMD EPYC 9V45 | 4.5 GB/s | 3.7 GB/s | 4.2 GB/s |
| compress, Arm Neoverse-N2 | 3.1 GB/s | 2.6 GB/s | 2.9 GB/s |

Most of the lead comes from integer columns, run-length-like data and
small blocks:

- Integer columns decompress 1.0–2.8x as fast as C++.
- Periodic data decompresses 3.6–39x as fast on x86 and 2.5–3.2x on Arm.
- 256-byte blocks decompress 2.2–3.0x as fast.

Text, HTML, URLs and protobuf decompress 1.07–1.11x as fast as C++ on Arm
and 0.98–1.03x on Zen 5, and at 0.89–1.00x its speed on Zen 3 and
0.93–0.96x on the Cascade Lake-class Xeon; 1.0–2.4x as fast as `snap`. On
x86 the fast loop copies 64 bytes for every element instead of branching
on its length, which costs plain text a few percent and pays off on
ClickBench, below. Where the CPU has AVX (detected at run time), that loop
runs in a copy compiled with AVX, which copies the 64 bytes with two
stores instead of four: 30% faster on text and 28% on ClickBench's pages
on Zen 3, where the stores held the loop back, 2–4% on Zen 5, and 1–3%
slower on the Cascade Lake-class Xeon.

On ClickBench (`hits_0.parquet`: 728 Snappy pages, 378 MB decoded, mostly
1 MB pages of titles and URLs), the pages decompress 1.03x as fast as C++
on Arm and on the Cascade Lake-class Xeon, 1.06x on Zen 3 and 1.22x on
Zen 5, and 1.5–2.5x as fast as `snap`. Weighted by the columns each of
the 43 queries reads, x86 decompression measured 1.13x as fast as C++ on
the Xeon. URLs, read by 10 of the queries, are most of that work.

On Intel cores from Skylake to Cascade Lake with Intel's microcode update
for the JCC erratum, a 32-byte block of code holding a branch that crosses
or ends on its boundary is decoded again on every pass. That slowed the
decoder's main loop by 10–20%, and the encoder's HTML and protobuf
compression by up to 47%, depending on where the linker placed them. Both
loops are aligned so that their layout does not depend on the linker; the
decoder's branches stay clear of those boundaries, and three of the C++
library's do not. On such a host, an earlier version of snipsnap measured
1.21x as fast as C++ on the query-weighted ClickBench pages, and 5.1 GB/s
against 3.1 GB/s on the 24 cases.

Building snipsnap with profile-guided optimization, LLVM BOLT, both, or
fat LTO, trained on the 24 cases, does not make it faster overall, on x86
or on Arm: decompression stays within 2.5% and compression is up to 3%
slower. Individual cases do move; on Arm, BOLT compresses HTML and
protobuf 11% faster but URLs 5% and 4 KiB blocks 7% slower. Those builds
were how the encoder's layout problem, and a store-alignment problem in
the decoding of long runs, were found and fixed in the source. Huge pages
for malloc (`GLIBC_TUNABLES=glibc.malloc.hugetlb=1`) change less than 1%.

Long runs are also written with AVX stores where the CPU has AVX, which
makes them 6–26% faster on the Cascade Lake-class Xeon and on Zen 3 and
Zen 5. With the AVX code chosen at run time, a build for
`-Ctarget-cpu=x86-64-v3` decompresses at the same speed on Zen 3 and Zen
5; it compresses long matches 12–17% slower on the Cascade Lake-class
Xeon, whose `vptest` does not fuse with its branch, and 14–16% faster on
Zen 3. On Arm, `-Ctarget-cpu=neoverse-n2` decompresses text 2% faster on
Neoverse-N2.

Framed streams check every chunk's CRC-32C with the CPU's CRC instructions
where it has them (SSE4.2 on x86, the CRC extension on Arm, detected at run
time), in three interleaved streams. The reader decodes chunks straight into
the caller's buffer when they fit. On x86, reading an 8 MiB framed stream
written by `snap` runs at 1.2–5.9 GB/s, 1.5–1.9x as fast as `snap`'s
reader, and stored chunks read at 6.7–7.0 GB/s.

Other cases can still move by 10–30% with code layout (for all three
libraries), so compare best-of-several runs. C++ snappy is tuned for clang;
a GCC build decompresses 2–3x slower.

## Status

This is an initial `0.0.x` release. The framed stream support is
intentionally small and emits valid uncompressed frame chunks.
