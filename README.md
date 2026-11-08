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
the same bytes as the C++ library built for the same CPU features, so it
compresses exactly as well. Like the C++ library, it hashes with the CRC-32C
instruction where the build enables one (SSE4.2 on x86-64, the CRC extension
on aarch64, which Apple's arm64 targets enable by default) and with a
multiply otherwise. `examples/compare.rs`
compares both directions against the `snap` crate and, when built with
`SNAPPY_CPP_PREFIX` pointing at an installed C++ snappy, against the C++
library, on synthetic data, the standard Snappy test corpus, 4 KiB text pages
and Parquet data pages.

Geometric means over 24 cases, on one core, against `snap` 1.1.1 and C++
snappy 1.3.1 built with clang 18 (Apple's clang on macOS):

| | snipsnap | `snap` | C++ |
|---|---|---|---|
| decompress, Intel Xeon (Cascade Lake class, 2.8 GHz) | 6.9 GB/s | 3.4 GB/s | 4.2 GB/s |
| decompress, AMD EPYC 7763 (Zen 3, GitHub's x86 runner) | 6.8 GB/s | 2.7 GB/s | 3.2 GB/s |
| decompress, AMD EPYC 9V45 (Zen 5, GitHub's x86 runner) | 12.8 GB/s | 5.6 GB/s | 7.0 GB/s |
| decompress, Arm Neoverse-N2 (GitHub's Arm runner) | 8.9 GB/s | 4.7 GB/s | 6.9 GB/s |
| decompress, Apple M1 (GitHub's macOS runner, not pinned) | 8.6 GB/s | 4.2 GB/s | 6.5 GB/s |
| compress, Intel Xeon | 2.4 GB/s | 2.1 GB/s | 2.2 GB/s |
| compress, AMD EPYC 7763 | 2.7 GB/s | 2.1 GB/s | 2.3 GB/s |
| compress, AMD EPYC 9V45 | 4.5 GB/s | 3.7 GB/s | 4.2 GB/s |
| compress, Arm Neoverse-N2 | 3.1 GB/s | 2.6 GB/s | 2.9 GB/s |
| compress, Apple M1 | 3.2 GB/s | 2.5 GB/s | 2.9 GB/s |

Most of the lead comes from integer columns, run-length-like data and
small blocks:

- Integer columns decompress 1.0–2.8x as fast as C++.
- Periodic data decompresses 3.6–39x as fast on x86 and 2.5–3.2x on Arm.
- 256-byte blocks decompress 2.2–3.0x as fast.

Text, HTML, URLs and protobuf decompress 1.07–1.19x as fast as C++ on
Zen 3, 1.07–1.11x on Neoverse-N2, 1.03–1.10x on Apple M1, 0.99–1.06x on
Zen 5 and 0.97–1.02x on the Cascade Lake-class Xeon; 1.0–2.4x as fast as
`snap`. On x86 the fast loop copies 32 bytes of each element and
branches for longer ones, until those turn out to be common, more than
one per 128 bytes of output: it then finishes the block with a copy of
itself that copies 64 bytes of every element, without the branch. Text,
with almost no elements longer than 32 bytes, decodes 3.5–8% faster that
way than with 64-byte copies throughout (21% on Zen 3), and ClickBench's
pages, below, which switch within their first few KiB, as fast (8–9%
faster on Zen 3, where the loop keeps the branch throughout, and on the
Cascade Lake-class Xeon). With the branch throughout, those pages
decoded 6–20% slower elsewhere. Where the CPU has AVX (detected at run
time), that loop runs in a copy compiled with AVX, which copies with
32-byte stores instead of 16-byte ones: with the 64-byte copies, 30%
faster on text and 28% on ClickBench's pages on Zen 3, where the stores
held the loop back, 2–4% on Zen 5, and 2% slower on text (0.4% overall)
on the Cascade Lake-class Xeon.

On ClickBench (`hits_0.parquet`: 728 Snappy pages, 378 MB decoded,
mostly 1 MB pages of titles and URLs), the pages decompress 1.03x as
fast as C++ on Arm, 1.08x on the Cascade Lake-class Xeon, 1.10–1.11x on
Intel's Emerald and Granite Rapids Xeons, 1.14–1.15x on Zen 3 and Zen 4
and 1.15–1.22x on Zen 5, and 1.5–2.5x as fast as `snap`. Weighted by the
columns each of the 43 queries reads, x86 decompression measured 1.13x
as fast as C++ on the Xeon. URLs, read by 10 of the queries, are most of
that work.

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
fat LTO, trained on the 24 cases, does not make it faster overall, on
x86 or on Arm: decompression stays within 2.5% and compression is up to
3% slower. Individual cases do move; on Arm, BOLT compresses HTML and
protobuf 11% faster but URLs 5% and 4 KiB blocks 7% slower. It runs the
same instructions; its layout makes branches mispredict 41–56% less
often on HTML and protobuf and 7–13% more often on URLs and 4 KiB
blocks. On GitHub's AMD runners, BOLT compresses text 3.5–6% faster on
Zen 3 and Zen 5 (single files 12% faster on Zen 3 and up to 26% on Zen
5, where snipsnap otherwise compresses some texts at 0.87x C++'s speed)
and 1–2% slower on Intel. Applied to the encoder's main function alone
it gains as much, while moving that function's code by 16 to 48 bytes
changes Zen 3 by 1%: the gain is BOLT's order of its blocks. Branch
hints that reproduce the most visible changes of that order did not
help. Those builds were how the encoder's layout problem, and a
store-alignment problem in the decoding of long runs, were found and
fixed in the source. Huge pages for malloc
(`GLIBC_TUNABLES=glibc.malloc.hugetlb=1`) change less than 1%.

Long runs are also written with AVX stores where the CPU has AVX, which
makes them 6–26% faster on the Cascade Lake-class Xeon and on Zen 3 and
Zen 5. With the AVX code chosen at run time, a build for
`-Ctarget-cpu=x86-64-v3` decompresses at the same speed on Zen 3 and Zen
5; it compresses long matches 12–17% slower on the Cascade Lake-class
Xeon, whose `vptest` does not fuse with its branch, and 14–16% faster on
Zen 3.

On Arm, building for a particular core changes little on Neoverse-N2. For
`neoverse-n1`, `neoverse-n2`, `neoverse-v1`, `neoverse-v2`, `cortex-x2` or
`native`, decompression stays within 0.4% of the default build, except
that 256-byte blocks decompress 5% faster. `ampere1` decompresses 2%
slower, and `cortex-a72` and `cortex-a76` compress 2% slower. No target
makes LLVM use SVE here. On Apple M1 the default target is the fastest;
`generic` and `neoverse-n1` builds decompress 3% slower.

Hashing with the CRC-32C instruction, where the build enables it,
compressed 5–6% faster on Apple M1 and 0.5% faster on Neoverse-N2 (with
`-Ctarget-feature=+crc`). On Zen 4 with `-Ctarget-cpu=x86-64-v2`, it
compressed text, HTML, protobuf and integers 2–6% faster, but runs 18%
slower than the multiply in the same build: the long-match loop is the
same code, placed differently. Copying 64 bytes per element with AVX-512
instead of AVX made decompression 1–9% slower on Zen 4 and on Intel Ice
Lake and Cascade Lake Xeons, and BMI2 builds changed it by less than 1%.

Matches longer than 64 bytes are compared 32 bytes at a time with NEON on
aarch64, and past 256 bytes with SSE2 on x86-64: runs compressed 28–44%
faster on Neoverse-N2 and Apple M1, and 17–24% faster on Zen 3.

Framed streams check every chunk's CRC-32C with the CPU's CRC instructions
where it has them (SSE4.2 on x86, the CRC extension on Arm, detected at run
time), in three interleaved streams. The reader decodes chunks straight into
the caller's buffer when they fit. On x86, reading an 8 MiB framed stream
written by `snap` runs at 1.2–5.9 GB/s, 1.5–1.9x as fast as `snap`'s
reader, and stored chunks read at 6.7–7.0 GB/s.

glibc's `memcpy` copies 2112 bytes and more with `rep movsb` on CPUs
with fast short `rep movsb`. On AMD Zen 3 and Zen 4 that instruction's
speed depends on the distance between the buffers, and copies that land
badly run at 3–5 GB/s instead of 25–50. In the benchmark they landed
badly on every Zen 3 runner, for all three libraries: 64 KiB literals
decoded at 2.7–3.0 GB/s. On AMD family 19h CPUs (Zen 3 and Zen 4,
detected at run time), snipsnap copies literals longer than 2 KiB in
2 KiB pieces, which glibc copies with vector loads and stores. On Zen 3
that decodes 64 KiB literals 18x as fast and compresses them 12x as fast,
decodes the PDF case 6.3x as fast, and makes decompression 23% and
compression 11% faster over the 24 cases. On Zen 4 it decodes 64 KiB
literals 36% faster and compresses them 29% faster, and decompression is
2.7% faster overall; without it, some output offsets compressed 64 KiB
literals there at 2.6 GB/s.
`GLIBC_TUNABLES=glibc.cpu.x86_rep_movsb_threshold=0x7fffffffffff` turns
`rep movsb` off for the whole process instead, but on Zen 5, where it is
fast, that made long literals 13–29% slower. The check is kept out of
line: inlined, it made HTML and protobuf compression 3–7% slower on
Intel Xeons, which never take it.

Other cases can still move by 10–30% with code layout (for all three
libraries), so compare best-of-several runs. C++ snappy is tuned for clang;
a GCC build decompresses 2–3x slower.

## Status

This is an initial `0.0.x` release. The framed stream support is
intentionally small and emits valid uncompressed frame chunks.
