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

## Status

This is an initial `0.0.x` release. The raw decompressor is the most optimized
part of the crate. The framed stream support is intentionally small and emits
valid uncompressed frame chunks.
