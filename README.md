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

On one core of an Intel Xeon (Sapphire Rapids class, 2.1 GHz base), against
`snap` 1.1.1 and C++ snappy 1.3.1 built with clang 18, geometric mean over 24
cases:

| | snipsnap | `snap` | C++ |
|---|---|---|---|
| decompress | 7.2 GB/s | 3.7 GB/s | 4.7 GB/s |
| compress | 2.9 GB/s | 2.5 GB/s | 2.5 GB/s |

C++ remains a few percent faster at decompressing plain text and HTML in
large blocks; snipsnap is faster on small blocks and on data with long or
repeated matches. Individual cases move by 10–30% with code layout alone (for
all three libraries), so compare best-of-several runs. C++ snappy is tuned
for clang; a GCC build decompresses 2–3x slower.

## Status

This is an initial `0.0.x` release. The framed stream support is
intentionally small and emits valid uncompressed frame chunks.
