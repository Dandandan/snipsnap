//! Small Snappy encoder and decoder.
//!
//! This supports the Snappy "raw" block format: a varint decompressed length
//! followed by literal and copy tags. It also provides basic Snappy framed
//! stream readers and writers.

use std::fmt;
use std::io;
use std::mem::MaybeUninit;
use std::ptr;
use std::slice;

const MAX_BLOCK_LEN: u64 = u32::MAX as u64;
const MAX_FRAME_BLOCK_LEN: usize = 1 << 16;
const MAX_TABLE_SIZE: usize = 1 << 14;
const SMALL_TABLE_SIZE: usize = 1 << 10;
const LITERAL_ONLY_MAX_INPUT_LEN: usize = 1024;
const INPUT_MARGIN: usize = 16 - 1;
const MIN_NON_LITERAL_BLOCK_SIZE: usize = 1 + 1 + INPUT_MARGIN;
const EXACT_COPY_TAG_MIN_LEN: usize = 1024;
const BULK_COPY_RUN_MIN_LEN: usize = 64 * 1024;
const BULK_COPY_RUN_MIN_COPIES: usize = 4;
const TEXT_STREAM_MAX_RATIO: usize = 10;
#[cfg(not(target_arch = "aarch64"))]
const WORD_MASK: [usize; 5] = [0, 0xff, 0xffff, 0xff_ffff, 0xffff_ffff];
const COPY_TAG_TABLE: [u16; 256] = build_copy_tag_table();

#[inline(always)]
fn word_mask(byte_count: usize) -> usize {
    debug_assert!((1..=4).contains(&byte_count));

    #[cfg(target_arch = "aarch64")]
    {
        (u32::MAX >> ((4 - byte_count as u32) << 3)) as usize
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        WORD_MASK[byte_count]
    }
}

/// Result type for Snappy encoding and decoding.
pub type Result<T> = std::result::Result<T, Error>;

/// Snappy encoding and decoding error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The input is empty.
    Empty,
    /// The leading uncompressed-length varint is invalid.
    Header,
    /// The decoded block length exceeds Snappy's 32-bit limit.
    TooBig { given: u64, max: u64 },
    /// The output buffer is smaller than the decoded block length.
    OutputTooSmall { given: usize, min: usize },
    /// The output vector could not reserve enough spare capacity.
    Allocation { requested: usize },
    /// A literal tag asks to read beyond the compressed input.
    LiteralRead { len: u64, remaining: u64 },
    /// A literal tag asks to write beyond the decoded block length.
    LiteralWrite { len: u64, remaining: u64 },
    /// A copy tag's offset bytes are truncated.
    CopyRead { len: u64, remaining: u64 },
    /// A copy tag asks to write beyond the decoded block length.
    CopyWrite { len: u64, remaining: u64 },
    /// A copy offset is zero or refers before the start of the output.
    Offset { offset: usize, dst_pos: usize },
    /// The input ended before producing the header's decoded block length.
    HeaderMismatch { expected: usize, actual: usize },
    /// A Snappy frame did not start with a stream identifier chunk.
    StreamHeader { byte: u8 },
    /// A Snappy frame stream identifier body did not match `sNaPpY`.
    StreamHeaderMismatch,
    /// A Snappy frame used a reserved, unskippable chunk type.
    UnsupportedChunkType { byte: u8 },
    /// A Snappy frame chunk length exceeded this implementation's limits.
    UnsupportedChunkLength { len: u64, header: bool },
    /// A Snappy frame chunk checksum did not match the decoded bytes.
    Checksum { expected: u32, got: u32 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Empty => f.write_str("empty snappy input"),
            Error::Header => f.write_str("invalid snappy header"),
            Error::TooBig { given, max } => {
                write!(f, "snappy block length {given} exceeds maximum {max}")
            }
            Error::OutputTooSmall { given, min } => {
                write!(f, "output buffer has {given} bytes, needs {min}")
            }
            Error::Allocation { requested } => {
                write!(f, "could not reserve {requested} output bytes")
            }
            Error::LiteralRead { len, remaining } => {
                write!(
                    f,
                    "literal needs {len} input bytes, only {remaining} remain"
                )
            }
            Error::LiteralWrite { len, remaining } => {
                write!(
                    f,
                    "literal writes {len} bytes, only {remaining} output bytes remain"
                )
            }
            Error::CopyRead { len, remaining } => {
                write!(
                    f,
                    "copy offset needs {len} input bytes, only {remaining} remain"
                )
            }
            Error::CopyWrite { len, remaining } => {
                write!(
                    f,
                    "copy writes {len} bytes, only {remaining} output bytes remain"
                )
            }
            Error::Offset { offset, dst_pos } => {
                write!(
                    f,
                    "copy offset {offset} is invalid at output position {dst_pos}"
                )
            }
            Error::HeaderMismatch { expected, actual } => {
                write!(
                    f,
                    "snappy header expected {expected} decoded bytes, got {actual}"
                )
            }
            Error::StreamHeader { byte } => {
                write!(
                    f,
                    "snappy frame expected stream header, got chunk type {byte:#04x}"
                )
            }
            Error::StreamHeaderMismatch => {
                f.write_str("snappy frame stream header bytes did not match")
            }
            Error::UnsupportedChunkType { byte } => {
                write!(f, "unsupported snappy frame chunk type {byte:#04x}")
            }
            Error::UnsupportedChunkLength { len, header } => {
                if header {
                    write!(f, "unsupported snappy frame header chunk length {len}")
                } else {
                    write!(f, "unsupported snappy frame chunk length {len}")
                }
            }
            Error::Checksum { expected, got } => {
                write!(
                    f,
                    "snappy frame checksum mismatch: expected {expected:#010x}, got {got:#010x}"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for io::Error {
    fn from(error: Error) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

#[cold]
#[inline(never)]
fn decode_error<T>(error: Error) -> Result<T> {
    Err(error)
}

/// Returns the decompressed length recorded in a raw Snappy block header.
#[inline]
pub fn decompress_len(src: &[u8]) -> Result<usize> {
    read_header(src).map(|(len, _)| len)
}

/// Decompresses a raw Snappy block into a new `Vec`.
///
/// The vector is allocated with capacity only; bytes are written through
/// spare capacity so the output is not zero-filled before decompression.
#[inline]
pub fn decompress(src: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    decompress_to_vec(src, &mut out)?;
    Ok(out)
}

/// Decompresses a raw Snappy block into `dst`.
///
/// Returns the initialized output prefix. `dst` must be at least
/// [`decompress_len`] bytes long.
#[inline]
pub fn decompress_into<'a>(src: &[u8], dst: &'a mut [u8]) -> Result<&'a mut [u8]> {
    let len = decompress_len(src)?;
    if dst.len() < len {
        return Err(Error::OutputTooSmall {
            given: dst.len(),
            min: len,
        });
    }
    let dst = &mut dst[..len];
    let dst = unsafe {
        slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<MaybeUninit<u8>>(), len)
    };
    decompress_into_uninit(src, dst)
}

/// Decompresses a raw Snappy block into `dst` without requiring zeroed output.
///
/// This is the core API for callers that want to use `Vec::spare_capacity_mut`.
/// On success, the returned prefix is initialized. On error, some bytes in
/// `dst` may have been written, but no initialized slice is returned.
#[inline]
pub fn decompress_into_uninit<'a>(
    src: &[u8],
    dst: &'a mut [MaybeUninit<u8>],
) -> Result<&'a mut [u8]> {
    let (len, header_len) = read_header(src)?;
    if dst.len() < len {
        return Err(Error::OutputTooSmall {
            given: dst.len(),
            min: len,
        });
    }
    let dst = &mut dst[..len];
    let body = &src[header_len..];
    if len < EXACT_COPY_TAG_MIN_LEN {
        if !try_decode_tiny(body, dst) {
            decode_body::<false, false>(body, dst)?;
        }
    } else if len <= body.len() {
        if !try_decode_literal_only(body, dst) {
            decode_body_table(body, dst)?;
        }
    } else if len <= body.len() * TEXT_STREAM_MAX_RATIO {
        decode_body_table(body, dst)?;
    } else if len < BULK_COPY_RUN_MIN_LEN {
        decode_body::<true, false>(body, dst)?;
    } else {
        decode_body::<true, true>(body, dst)?;
    }
    Ok(unsafe { slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<u8>(), len) })
}

/// Clears `dst` and decompresses a raw Snappy block into its spare capacity.
///
/// Unlike `vec![0; len]`, this avoids zero-filling the output before decode.
#[inline]
pub fn decompress_to_vec<'a>(src: &[u8], dst: &'a mut Vec<u8>) -> Result<&'a mut [u8]> {
    let len = decompress_len(src)?;
    dst.clear();
    dst.try_reserve(len)
        .map_err(|_| Error::Allocation { requested: len })?;
    decompress_into_uninit(src, &mut dst.spare_capacity_mut()[..len])?;
    unsafe {
        dst.set_len(len);
    }
    Ok(dst.as_mut_slice())
}

/// Returns the maximum raw Snappy compressed length for an input length.
///
/// Returns `0` when `input_len` exceeds Snappy's 32-bit block limit.
#[inline]
pub fn max_compress_len(input_len: usize) -> usize {
    let input_len = input_len as u64;
    if input_len > MAX_BLOCK_LEN {
        return 0;
    }
    let max = 32 + input_len + (input_len / 6);
    if max > MAX_BLOCK_LEN { 0 } else { max as usize }
}

/// Compresses bytes into a raw Snappy block.
#[inline]
pub fn compress(src: &[u8]) -> Result<Vec<u8>> {
    RawEncoder::new().compress_vec(src)
}

/// Compresses bytes into `dst` as a raw Snappy block.
#[inline]
pub fn compress_into(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    RawEncoder::new().compress(src, dst)
}

/// Low-level raw Snappy block APIs.
pub mod raw {
    use std::fmt;

    pub use crate::max_compress_len;
    use crate::{RawEncoder, Result, decompress, decompress_into};

    /// Raw Snappy block encoder.
    pub struct Encoder {
        inner: RawEncoder,
    }

    impl Encoder {
        /// Creates a new raw Snappy block encoder.
        pub fn new() -> Self {
            Self {
                inner: RawEncoder::new(),
            }
        }

        /// Compresses `input` into `output`.
        pub fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
            self.inner.compress(input, output)
        }

        /// Compresses `input` into a new `Vec`.
        pub fn compress_vec(&mut self, input: &[u8]) -> Result<Vec<u8>> {
            self.inner.compress_vec(input)
        }
    }

    impl Default for Encoder {
        fn default() -> Self {
            Self::new()
        }
    }

    impl fmt::Debug for Encoder {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("Encoder(...)")
        }
    }

    /// Raw Snappy block decoder.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Decoder;

    impl Decoder {
        /// Creates a new raw Snappy block decoder.
        pub fn new() -> Self {
            Self
        }

        /// Decompresses `input` into `output`.
        pub fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
            decompress_into(input, output).map(|decoded| decoded.len())
        }

        /// Decompresses `input` into a new `Vec`.
        pub fn decompress_vec(&mut self, input: &[u8]) -> Result<Vec<u8>> {
            decompress(input)
        }
    }
}

/// Streaming readers for Snappy framed streams.
pub mod read {
    use std::cmp;
    use std::fmt;
    use std::io::{self, Read};

    use crate::{Error, MAX_FRAME_BLOCK_LEN, decompress, decompress_len, frame};

    /// A reader that decodes the Snappy framed stream format.
    pub struct FrameDecoder<R: Read> {
        inner: R,
        src: Vec<u8>,
        dst: Vec<u8>,
        pos: usize,
        end: usize,
        read_stream_ident: bool,
    }

    impl<R: Read> FrameDecoder<R> {
        /// Creates a new Snappy frame decoder.
        pub fn new(inner: R) -> Self {
            Self {
                inner,
                src: Vec::new(),
                dst: Vec::new(),
                pos: 0,
                end: 0,
                read_stream_ident: false,
            }
        }

        /// Gets a shared reference to the wrapped reader.
        pub fn get_ref(&self) -> &R {
            &self.inner
        }

        /// Gets a mutable reference to the wrapped reader.
        pub fn get_mut(&mut self) -> &mut R {
            &mut self.inner
        }

        /// Consumes this decoder and returns the wrapped reader.
        pub fn into_inner(self) -> R {
            self.inner
        }

        fn read_chunk(&mut self) -> io::Result<bool> {
            let mut header = [0u8; 4];
            if !frame::read_exact_eof(&mut self.inner, &mut header)? {
                return Ok(false);
            }

            let chunk_type = header[0];
            let len = frame::read_u24_le(&header[1..]) as usize;
            if !self.read_stream_ident {
                if chunk_type != frame::STREAM_IDENTIFIER_TYPE {
                    return Err(Error::StreamHeader { byte: chunk_type }.into());
                }
                self.read_stream_ident = true;
            }

            match chunk_type {
                frame::STREAM_IDENTIFIER_TYPE => {
                    if len != frame::STREAM_BODY.len() {
                        return Err(Error::UnsupportedChunkLength {
                            len: len as u64,
                            header: true,
                        }
                        .into());
                    }
                    self.src.resize(len, 0);
                    self.inner.read_exact(&mut self.src)?;
                    if self.src != frame::STREAM_BODY {
                        return Err(Error::StreamHeaderMismatch.into());
                    }
                }
                frame::UNCOMPRESSED_DATA_TYPE => {
                    if len < frame::CHECKSUM_LEN {
                        return Err(Error::UnsupportedChunkLength {
                            len: len as u64,
                            header: false,
                        }
                        .into());
                    }

                    let data_len = len - frame::CHECKSUM_LEN;
                    if data_len > MAX_FRAME_BLOCK_LEN {
                        return Err(Error::UnsupportedChunkLength {
                            len: data_len as u64,
                            header: false,
                        }
                        .into());
                    }

                    let mut checksum = [0u8; frame::CHECKSUM_LEN];
                    self.inner.read_exact(&mut checksum)?;
                    self.dst.resize(data_len, 0);
                    self.inner.read_exact(&mut self.dst)?;

                    let expected = frame::read_u32_le(&checksum);
                    let got = frame::masked_crc32c(&self.dst);
                    if expected != got {
                        return Err(Error::Checksum { expected, got }.into());
                    }
                    self.pos = 0;
                    self.end = data_len;
                }
                frame::COMPRESSED_DATA_TYPE => {
                    if len < frame::CHECKSUM_LEN {
                        return Err(Error::UnsupportedChunkLength {
                            len: len as u64,
                            header: false,
                        }
                        .into());
                    }

                    let data_len = len - frame::CHECKSUM_LEN;
                    self.src.resize(data_len, 0);
                    let mut checksum = [0u8; frame::CHECKSUM_LEN];
                    self.inner.read_exact(&mut checksum)?;
                    self.inner.read_exact(&mut self.src)?;

                    let decoded_len = decompress_len(&self.src)?;
                    if decoded_len > MAX_FRAME_BLOCK_LEN {
                        return Err(Error::UnsupportedChunkLength {
                            len: decoded_len as u64,
                            header: false,
                        }
                        .into());
                    }

                    self.dst = decompress(&self.src)?;
                    let expected = frame::read_u32_le(&checksum);
                    let got = frame::masked_crc32c(&self.dst);
                    if expected != got {
                        return Err(Error::Checksum { expected, got }.into());
                    }
                    self.pos = 0;
                    self.end = self.dst.len();
                }
                frame::PADDING_TYPE => {
                    frame::skip_exact(&mut self.inner, len)?;
                }
                0x80..=0xfd => {
                    frame::skip_exact(&mut self.inner, len)?;
                }
                0x02..=0x7f => {
                    return Err(Error::UnsupportedChunkType { byte: chunk_type }.into());
                }
            }

            Ok(true)
        }
    }

    impl<R: Read> Read for FrameDecoder<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }

            loop {
                if self.pos < self.end {
                    let len = cmp::min(self.end - self.pos, buf.len());
                    buf[..len].copy_from_slice(&self.dst[self.pos..self.pos + len]);
                    self.pos += len;
                    return Ok(len);
                }

                if !self.read_chunk()? {
                    return Ok(0);
                }
            }
        }
    }

    impl<R: fmt::Debug + Read> fmt::Debug for FrameDecoder<R> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("FrameDecoder")
                .field("inner", &self.inner)
                .field("src", &"[...]")
                .field("dst", &"[...]")
                .field("pos", &self.pos)
                .field("end", &self.end)
                .field("read_stream_ident", &self.read_stream_ident)
                .finish()
        }
    }

    /// A reader that encodes bytes into the Snappy framed stream format.
    pub struct FrameEncoder<R: Read> {
        inner: R,
        src: Vec<u8>,
        dst: Vec<u8>,
        pos: usize,
        done: bool,
        wrote_stream_ident: bool,
    }

    impl<R: Read> FrameEncoder<R> {
        /// Creates a new Snappy frame encoder.
        pub fn new(inner: R) -> Self {
            Self {
                inner,
                src: vec![0; MAX_FRAME_BLOCK_LEN],
                dst: Vec::new(),
                pos: 0,
                done: false,
                wrote_stream_ident: false,
            }
        }

        /// Gets a shared reference to the wrapped reader.
        pub fn get_ref(&self) -> &R {
            &self.inner
        }

        /// Gets a mutable reference to the wrapped reader.
        pub fn get_mut(&mut self) -> &mut R {
            &mut self.inner
        }

        /// Consumes this encoder and returns the wrapped reader.
        pub fn into_inner(self) -> R {
            self.inner
        }

        fn refill(&mut self) -> io::Result<bool> {
            if self.done {
                return Ok(false);
            }

            let n = loop {
                match self.inner.read(&mut self.src) {
                    Ok(n) => break n,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            };

            if n == 0 {
                self.done = true;
                return Ok(false);
            }

            self.dst.clear();
            if !self.wrote_stream_ident {
                self.dst.extend_from_slice(frame::STREAM_IDENTIFIER);
                self.wrote_stream_ident = true;
            }
            frame::encode_uncompressed_chunk(&self.src[..n], &mut self.dst);
            self.pos = 0;
            Ok(true)
        }
    }

    impl<R: Read> Read for FrameEncoder<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }

            loop {
                if self.pos < self.dst.len() {
                    let len = cmp::min(self.dst.len() - self.pos, buf.len());
                    buf[..len].copy_from_slice(&self.dst[self.pos..self.pos + len]);
                    self.pos += len;
                    return Ok(len);
                }

                if !self.refill()? {
                    return Ok(0);
                }
            }
        }
    }

    impl<R: fmt::Debug + Read> fmt::Debug for FrameEncoder<R> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("FrameEncoder")
                .field("inner", &self.inner)
                .field("src", &"[...]")
                .field("dst", &"[...]")
                .field("pos", &self.pos)
                .field("done", &self.done)
                .field("wrote_stream_ident", &self.wrote_stream_ident)
                .finish()
        }
    }
}

/// Streaming writers for Snappy framed streams.
pub mod write {
    use std::fmt;
    use std::io::{self, Write};

    use crate::{MAX_FRAME_BLOCK_LEN, frame};

    /// A writer that encodes bytes into the Snappy framed stream format.
    pub struct FrameEncoder<W: Write> {
        inner: Option<W>,
        src: Vec<u8>,
        wrote_stream_ident: bool,
    }

    impl<W: Write> FrameEncoder<W> {
        /// Creates a new Snappy frame encoder.
        pub fn new(inner: W) -> Self {
            Self {
                inner: Some(inner),
                src: Vec::with_capacity(MAX_FRAME_BLOCK_LEN),
                wrote_stream_ident: false,
            }
        }

        /// Flushes buffered frame data and returns the wrapped writer.
        pub fn into_inner(mut self) -> io::Result<W> {
            self.flush()?;
            Ok(self.inner.take().expect("frame encoder inner writer"))
        }

        /// Gets a shared reference to the wrapped writer.
        pub fn get_ref(&self) -> &W {
            self.inner.as_ref().expect("frame encoder inner writer")
        }

        /// Gets a mutable reference to the wrapped writer.
        pub fn get_mut(&mut self) -> &mut W {
            self.inner.as_mut().expect("frame encoder inner writer")
        }

        fn write_chunk(&mut self, src: &[u8]) -> io::Result<()> {
            debug_assert!(!src.is_empty());
            debug_assert!(src.len() <= MAX_FRAME_BLOCK_LEN);

            let inner = self.inner.as_mut().expect("frame encoder inner writer");
            if !self.wrote_stream_ident {
                inner.write_all(frame::STREAM_IDENTIFIER)?;
                self.wrote_stream_ident = true;
            }
            frame::write_uncompressed_chunk(inner, src)
        }
    }

    impl<W: Write> Drop for FrameEncoder<W> {
        fn drop(&mut self) {
            if self.inner.is_some() {
                let _ = self.flush();
            }
        }
    }

    impl<W: Write> Write for FrameEncoder<W> {
        fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
            let written = buf.len();
            while !buf.is_empty() {
                if self.src.is_empty() && buf.len() >= MAX_FRAME_BLOCK_LEN {
                    let (chunk, rest) = buf.split_at(MAX_FRAME_BLOCK_LEN);
                    self.write_chunk(chunk)?;
                    buf = rest;
                    continue;
                }

                let available = MAX_FRAME_BLOCK_LEN - self.src.len();
                let take = cmp_len(buf.len(), available);
                self.src.extend_from_slice(&buf[..take]);
                buf = &buf[take..];

                if self.src.len() == MAX_FRAME_BLOCK_LEN {
                    let chunk = std::mem::take(&mut self.src);
                    self.write_chunk(&chunk)?;
                    self.src = chunk;
                    self.src.clear();
                }
            }
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            if !self.src.is_empty() {
                let chunk = std::mem::take(&mut self.src);
                self.write_chunk(&chunk)?;
                self.src = chunk;
                self.src.clear();
            }
            self.inner
                .as_mut()
                .expect("frame encoder inner writer")
                .flush()
        }
    }

    impl<W: fmt::Debug + Write> fmt::Debug for FrameEncoder<W> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("FrameEncoder")
                .field("inner", &self.inner)
                .field("src", &"[...]")
                .field("wrote_stream_ident", &self.wrote_stream_ident)
                .finish()
        }
    }

    #[inline]
    fn cmp_len(left: usize, right: usize) -> usize {
        if left < right { left } else { right }
    }
}

mod frame {
    use std::io::{self, Read, Write};

    pub const STREAM_IDENTIFIER_TYPE: u8 = 0xff;
    pub const COMPRESSED_DATA_TYPE: u8 = 0x00;
    pub const UNCOMPRESSED_DATA_TYPE: u8 = 0x01;
    pub const PADDING_TYPE: u8 = 0xfe;
    pub const STREAM_BODY: &[u8] = b"sNaPpY";
    pub const STREAM_IDENTIFIER: &[u8] = b"\xff\x06\x00\x00sNaPpY";
    pub const CHECKSUM_LEN: usize = 4;

    pub fn encode_uncompressed_chunk(src: &[u8], dst: &mut Vec<u8>) {
        debug_assert!(src.len() <= super::MAX_FRAME_BLOCK_LEN);

        dst.push(UNCOMPRESSED_DATA_TYPE);
        write_u24_le((CHECKSUM_LEN + src.len()) as u32, dst);
        write_u32_le(masked_crc32c(src), dst);
        dst.extend_from_slice(src);
    }

    pub fn write_uncompressed_chunk<W: Write>(
        writer: &mut W,
        src: &[u8],
    ) -> io::Result<()> {
        let mut header = [0u8; 8];
        header[0] = UNCOMPRESSED_DATA_TYPE;
        write_u24_le_to_slice((CHECKSUM_LEN + src.len()) as u32, &mut header[1..4]);
        write_u32_le_to_slice(masked_crc32c(src), &mut header[4..8]);
        writer.write_all(&header)?;
        writer.write_all(src)
    }

    #[inline]
    pub fn read_u24_le(src: &[u8]) -> u32 {
        u32::from(src[0]) | (u32::from(src[1]) << 8) | (u32::from(src[2]) << 16)
    }

    #[inline]
    pub fn read_u32_le(src: &[u8]) -> u32 {
        u32::from_le_bytes(src[..4].try_into().expect("u32 bytes"))
    }

    #[inline]
    fn write_u24_le(n: u32, dst: &mut Vec<u8>) {
        dst.push(n as u8);
        dst.push((n >> 8) as u8);
        dst.push((n >> 16) as u8);
    }

    #[inline]
    fn write_u24_le_to_slice(n: u32, dst: &mut [u8]) {
        dst[0] = n as u8;
        dst[1] = (n >> 8) as u8;
        dst[2] = (n >> 16) as u8;
    }

    #[inline]
    fn write_u32_le(n: u32, dst: &mut Vec<u8>) {
        dst.extend_from_slice(&n.to_le_bytes());
    }

    #[inline]
    fn write_u32_le_to_slice(n: u32, dst: &mut [u8]) {
        dst.copy_from_slice(&n.to_le_bytes());
    }

    pub fn read_exact_eof<R: Read>(
        reader: &mut R,
        mut buf: &mut [u8],
    ) -> io::Result<bool> {
        let mut read_any = false;
        while !buf.is_empty() {
            match reader.read(buf) {
                Ok(0) if read_any => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ));
                }
                Ok(0) => return Ok(false),
                Ok(n) => {
                    read_any = true;
                    let tmp = buf;
                    buf = &mut tmp[n..];
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    pub fn skip_exact<R: Read>(reader: &mut R, mut len: usize) -> io::Result<()> {
        let mut buf = [0u8; 8192];
        while len > 0 {
            let take = buf.len().min(len);
            reader.read_exact(&mut buf[..take])?;
            len -= take;
        }
        Ok(())
    }

    pub fn masked_crc32c(src: &[u8]) -> u32 {
        let crc = crc32c(src);
        crc.rotate_right(15).wrapping_add(0xa282_ead8)
    }

    fn crc32c(src: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in src {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = 0u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0x82f6_3b78 & mask);
            }
        }
        !crc
    }
}

struct RawEncoder {
    small: [u16; SMALL_TABLE_SIZE],
    big: Vec<u16>,
}

impl RawEncoder {
    fn new() -> Self {
        Self {
            small: [0; SMALL_TABLE_SIZE],
            big: Vec::new(),
        }
    }

    fn compress_vec(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        let mut out = vec![0; max_compress_len(input.len())];
        let len = self.compress(input, &mut out)?;
        out.truncate(len);
        Ok(out)
    }

    fn compress(&mut self, mut input: &[u8], output: &mut [u8]) -> Result<usize> {
        let min_len = max_compress_len(input.len());
        if min_len == 0 {
            return Err(Error::TooBig {
                given: input.len() as u64,
                max: MAX_BLOCK_LEN,
            });
        }
        if output.len() < min_len {
            return Err(Error::OutputTooSmall {
                given: output.len(),
                min: min_len,
            });
        }

        if input.is_empty() {
            output[0] = 0;
            return Ok(1);
        }

        let mut dst_pos = write_varu64(output, input.len() as u64);
        if should_emit_literal_only(input) {
            emit_literal_blocks(input, output, &mut dst_pos);
            return Ok(dst_pos);
        }

        while !input.is_empty() {
            let block_len = input.len().min(MAX_FRAME_BLOCK_LEN);
            let (block, rest) = input.split_at(block_len);
            input = rest;

            if block.len() < MIN_NON_LITERAL_BLOCK_SIZE {
                emit_literal(block, 0, block.len(), output, &mut dst_pos);
            } else {
                let (table, shift) = self.block_table(block.len());
                compress_block(block, output, &mut dst_pos, table, shift);
            }
        }
        Ok(dst_pos)
    }

    fn block_table(&mut self, block_size: usize) -> (&mut [u16], u32) {
        let mut shift = 32 - 8;
        let mut table_size = 256;
        while table_size < MAX_TABLE_SIZE && table_size < block_size {
            shift -= 1;
            table_size *= 2;
        }

        let table = if table_size <= SMALL_TABLE_SIZE {
            &mut self.small[..table_size]
        } else {
            if self.big.is_empty() {
                self.big = vec![0; MAX_TABLE_SIZE];
            }
            &mut self.big[..table_size]
        };
        for slot in &mut *table {
            *slot = 0;
        }
        (table, shift)
    }
}

fn should_emit_literal_only(input: &[u8]) -> bool {
    if input.len() <= LITERAL_ONLY_MAX_INPUT_LEN {
        return true;
    }
    if input.len() < 4096 {
        return false;
    }

    const SAMPLE_LEN: usize = 128;
    let sample_len = input.len().min(SAMPLE_LEN);
    let mut table = [usize::MAX; 256];
    for pos in 0..sample_len - 3 {
        let bytes = load_u32_ne(input, pos);
        let hash = (bytes.wrapping_mul(0x1e35_a7bd) >> 24) as usize;
        let candidate = table[hash];
        if candidate != usize::MAX && load_u32_ne(input, candidate) == bytes {
            return false;
        }
        table[hash] = pos;
    }
    true
}

fn emit_literal_blocks(mut input: &[u8], dst: &mut [u8], dst_pos: &mut usize) {
    while !input.is_empty() {
        let len = input.len().min(MAX_FRAME_BLOCK_LEN);
        emit_literal(input, 0, len, dst, dst_pos);
        input = &input[len..];
    }
}

fn compress_block(
    src: &[u8],
    dst: &mut [u8],
    dst_pos: &mut usize,
    table: &mut [u16],
    shift: u32,
) {
    debug_assert!(!table.is_empty());
    debug_assert!(src.len() >= MIN_NON_LITERAL_BLOCK_SIZE);

    let mut src_pos = 1usize;
    let src_limit = src.len() - INPUT_MARGIN;
    let mut next_emit = 0usize;
    let mut next_hash = hash(load_u32_le(src, src_pos), shift);

    loop {
        let mut skip = 128usize;
        let mut candidate;
        let mut src_next = src_pos;
        loop {
            src_pos = src_next;
            let bytes_between_hash_lookups = skip >> 5;
            src_next = src_pos + bytes_between_hash_lookups;
            skip += bytes_between_hash_lookups;
            if src_next > src_limit {
                if next_emit < src.len() {
                    emit_literal(src, next_emit, src.len(), dst, dst_pos);
                }
                return;
            }

            unsafe {
                candidate = *table.get_unchecked(next_hash) as usize;
                *table.get_unchecked_mut(next_hash) = src_pos as u16;
            }

            let next = load_u32_le(src, src_next);
            next_hash = hash(next, shift);
            if load_u32_ne(src, src_pos) == load_u32_ne(src, candidate) {
                break;
            }
        }

        emit_literal(src, next_emit, src_pos, dst, dst_pos);

        loop {
            let base = src_pos;
            src_pos += 4;
            src_pos = extend_match(src, src_pos, candidate + 4);
            let offset = base - candidate;
            let len = src_pos - base;
            emit_copy(offset, len, dst, dst_pos);
            next_emit = src_pos;

            if src_pos >= src_limit {
                if next_emit < src.len() {
                    emit_literal(src, next_emit, src.len(), dst, dst_pos);
                }
                return;
            }

            let bytes = load_u64_le(src, src_pos - 1);
            let prev_hash = hash(bytes as u32, shift);
            unsafe {
                *table.get_unchecked_mut(prev_hash) = (src_pos - 1) as u16;
            }

            let cur_hash = hash((bytes >> 8) as u32, shift);
            unsafe {
                candidate = *table.get_unchecked(cur_hash) as usize;
                *table.get_unchecked_mut(cur_hash) = src_pos as u16;
            }

            if (bytes >> 8) as u32 != load_u32_le(src, candidate) {
                next_hash = hash((bytes >> 16) as u32, shift);
                src_pos += 1;
                break;
            }
        }
    }
}

#[inline(always)]
fn emit_literal(
    src: &[u8],
    start: usize,
    end: usize,
    dst: &mut [u8],
    dst_pos: &mut usize,
) {
    let len = end - start;
    let n = len - 1;
    if n <= 59 {
        dst[*dst_pos] = (n as u8) << 2;
        *dst_pos += 1;
        if len <= 16 && start + 16 <= src.len() {
            unsafe {
                ptr::copy_nonoverlapping(
                    src.as_ptr().add(start),
                    dst.as_mut_ptr().add(*dst_pos),
                    16,
                );
            }
            *dst_pos += len;
            return;
        }
    } else if n < 256 {
        dst[*dst_pos] = 60 << 2;
        dst[*dst_pos + 1] = n as u8;
        *dst_pos += 2;
    } else {
        dst[*dst_pos] = 61 << 2;
        write_u16_le(n as u16, &mut dst[*dst_pos + 1..]);
        *dst_pos += 3;
    }

    unsafe {
        ptr::copy_nonoverlapping(
            src.as_ptr().add(start),
            dst.as_mut_ptr().add(*dst_pos),
            len,
        );
    }
    *dst_pos += len;
}

#[inline(always)]
fn emit_copy(offset: usize, mut len: usize, dst: &mut [u8], dst_pos: &mut usize) {
    debug_assert!((1..=65535).contains(&offset));
    debug_assert!((4..=65535).contains(&len));

    while len >= 68 {
        emit_copy2(offset, 64, dst, dst_pos);
        len -= 64;
    }
    if len > 64 {
        emit_copy2(offset, 60, dst, dst_pos);
        len -= 60;
    }
    if len <= 11 && offset <= 2047 {
        dst[*dst_pos] = (((offset >> 8) as u8) << 5) | (((len - 4) as u8) << 2) | 1;
        dst[*dst_pos + 1] = offset as u8;
        *dst_pos += 2;
    } else {
        emit_copy2(offset, len, dst, dst_pos);
    }
}

#[inline(always)]
fn emit_copy2(offset: usize, len: usize, dst: &mut [u8], dst_pos: &mut usize) {
    debug_assert!((1..=65535).contains(&offset));
    debug_assert!((1..=64).contains(&len));

    dst[*dst_pos] = (((len - 1) as u8) << 2) | 2;
    write_u16_le(offset as u16, &mut dst[*dst_pos + 1..]);
    *dst_pos += 3;
}

#[inline(always)]
fn extend_match(src: &[u8], mut src_pos: usize, mut candidate: usize) -> usize {
    debug_assert!(candidate < src_pos);

    while src_pos + 8 <= src.len() {
        let current = load_u64_ne(src, src_pos);
        let previous = load_u64_ne(src, candidate);
        if current == previous {
            src_pos += 8;
            candidate += 8;
        } else {
            let diff = current.to_le() ^ previous.to_le();
            return src_pos + (diff.trailing_zeros() as usize / 8);
        }
    }

    while src_pos < src.len() && src[src_pos] == src[candidate] {
        src_pos += 1;
        candidate += 1;
    }
    src_pos
}

#[inline(always)]
fn hash(bytes: u32, shift: u32) -> usize {
    (bytes.wrapping_mul(0x1e35_a7bd) >> shift) as usize
}

#[inline(always)]
fn write_varu64(dst: &mut [u8], mut value: u64) -> usize {
    let mut pos = 0;
    while value >= 0x80 {
        dst[pos] = (value as u8) | 0x80;
        value >>= 7;
        pos += 1;
    }
    dst[pos] = value as u8;
    pos + 1
}

#[inline(always)]
fn write_u16_le(value: u16, dst: &mut [u8]) {
    unsafe {
        ptr::write_unaligned(dst.as_mut_ptr().cast::<u16>(), value.to_le());
    }
}

#[inline(always)]
fn load_u32_le(src: &[u8], pos: usize) -> u32 {
    load_u32_ne(src, pos).to_le()
}

#[inline(always)]
fn load_u32_ne(src: &[u8], pos: usize) -> u32 {
    unsafe { ptr::read_unaligned(src.as_ptr().add(pos).cast::<u32>()) }
}

#[inline(always)]
fn load_u64_le(src: &[u8], pos: usize) -> u64 {
    load_u64_ne(src, pos).to_le()
}

#[inline(always)]
fn load_u64_ne(src: &[u8], pos: usize) -> u64 {
    unsafe { ptr::read_unaligned(src.as_ptr().add(pos).cast::<u64>()) }
}

#[inline(always)]
fn read_header(src: &[u8]) -> Result<(usize, usize)> {
    if src.is_empty() {
        return Err(Error::Empty);
    }

    let first = src[0];
    if first < 0x80 {
        return Ok((usize::from(first), 1));
    }

    let mut len = u64::from(first & 0x7f);
    let mut shift = 7;
    for (i, &byte) in src.iter().take(5).enumerate().skip(1) {
        len |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            if len > MAX_BLOCK_LEN {
                return Err(Error::TooBig {
                    given: len,
                    max: MAX_BLOCK_LEN,
                });
            }
            return Ok((len as usize, i + 1));
        }
        shift += 7;
    }
    Err(Error::Header)
}

#[inline(always)]
fn try_decode_tiny(src: &[u8], dst: &mut [MaybeUninit<u8>]) -> bool {
    let src_len = src.len();
    let dst_len = dst.len();
    if src_len == 0 || dst_len == 0 {
        return src_len == 0 && dst_len == 0;
    }

    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr().cast::<u8>();
    if src_len == dst_len + 1 {
        let tag = unsafe { *src_ptr };
        if tag & 0x03 == 0 && usize::from(tag >> 2) + 1 == dst_len {
            unsafe {
                copy_literal(src_ptr.add(1), dst_ptr, dst_len, dst_len, dst_len);
            }
            return true;
        }
    }
    if try_decode_literal_offset1_copy(src_ptr, src_len, dst_ptr, dst_len) {
        return true;
    }
    let mut ip = 0usize;
    let mut op = 0usize;
    while ip < src_len {
        let tag = unsafe { *src_ptr.add(ip) };
        ip += 1;
        match tag & 0x03 {
            0 => {
                let mut len = usize::from(tag >> 2) + 1;
                if len > 60 {
                    let len_bytes = len - 60;
                    if len_bytes > src_len - ip {
                        return false;
                    }
                    let mut len64 = 1usize;
                    for i in 0..len_bytes {
                        len64 += unsafe { usize::from(*src_ptr.add(ip + i)) } << (8 * i);
                    }
                    ip += len_bytes;
                    len = len64;
                }
                if len > src_len - ip || len > dst_len - op {
                    return false;
                }
                unsafe {
                    copy_literal(
                        src_ptr.add(ip),
                        dst_ptr.add(op),
                        len,
                        src_len - ip,
                        dst_len - op,
                    );
                }
                ip += len;
                op += len;
                unsafe {
                    copy_table_short_copy_after_literal(
                        src_ptr, src_len, dst_ptr, dst_len, &mut ip, &mut op,
                    );
                }
            }
            1 => {
                if ip >= src_len {
                    return false;
                }
                let len = 4 + usize::from((tag >> 2) & 0x07);
                let offset = (usize::from(tag & 0xe0) << 3)
                    | unsafe { usize::from(*src_ptr.add(ip)) };
                ip += 1;
                if !try_copy_tiny_match(dst_ptr, dst_len, op, offset, len) {
                    return false;
                }
                op += len;
            }
            2 => {
                if 2 > src_len - ip {
                    return false;
                }
                let len = 1 + usize::from(tag >> 2);
                let offset = unsafe { read_u16(src_ptr.add(ip)) };
                ip += 2;
                if !try_copy_tiny_match(dst_ptr, dst_len, op, offset, len) {
                    return false;
                }
                op += len;
            }
            _ => {
                if 4 > src_len - ip {
                    return false;
                }
                let len = 1 + usize::from(tag >> 2);
                let offset = unsafe { read_u32(src_ptr.add(ip)) as usize };
                ip += 4;
                if !try_copy_tiny_match(dst_ptr, dst_len, op, offset, len) {
                    return false;
                }
                op += len;
            }
        }
    }
    op == dst_len
}

#[inline(always)]
fn try_decode_literal_offset1_copy(
    src_ptr: *const u8,
    src_len: usize,
    dst_ptr: *mut u8,
    dst_len: usize,
) -> bool {
    if src_len < 4 || dst_len < 2 {
        return false;
    }
    let literal_tag = unsafe { *src_ptr };
    if literal_tag & 0x03 != 0 {
        return false;
    }
    let lit_len = usize::from(literal_tag >> 2) + 1;
    if lit_len > 60 || lit_len + 2 >= src_len || lit_len >= dst_len {
        return false;
    }

    let copy_tag_pos = 1 + lit_len;
    let copy_tag = unsafe { *src_ptr.add(copy_tag_pos) };
    let (copy_len, offset) = match copy_tag & 0x03 {
        1 => {
            if copy_tag_pos + 2 != src_len {
                return false;
            }
            let len = 4 + usize::from((copy_tag >> 2) & 0x07);
            let offset = (usize::from(copy_tag & 0xe0) << 3)
                | unsafe { usize::from(*src_ptr.add(copy_tag_pos + 1)) };
            (len, offset)
        }
        2 => {
            if copy_tag_pos + 3 != src_len {
                return false;
            }
            let len = 1 + usize::from(copy_tag >> 2);
            let offset = unsafe { read_u16(src_ptr.add(copy_tag_pos + 1)) };
            (len, offset)
        }
        _ => return false,
    };

    if offset != 1 || lit_len + copy_len != dst_len {
        return false;
    }

    unsafe {
        copy_literal(src_ptr.add(1), dst_ptr, lit_len, src_len - 1, dst_len);
        let repeat = *src_ptr.add(lit_len);
        copy_offset1_repeat(dst_ptr.add(lit_len), repeat, copy_len);
    }
    true
}

#[inline(always)]
fn try_copy_tiny_match(
    dst_ptr: *mut u8,
    dst_len: usize,
    op: usize,
    offset: usize,
    len: usize,
) -> bool {
    if offset == 0 || offset > op || len > dst_len - op {
        return false;
    }
    unsafe {
        if offset == 1 {
            let repeat = *dst_ptr.add(op - 1);
            copy_offset1_repeat(dst_ptr.add(op), repeat, len);
        } else if offset >= len {
            ptr::copy_nonoverlapping(dst_ptr.add(op - offset), dst_ptr.add(op), len);
        } else if copy_match(dst_ptr, dst_len, op, offset, len).is_err() {
            return false;
        }
    }
    true
}

#[inline(always)]
fn try_decode_literal_only(src: &[u8], dst: &mut [MaybeUninit<u8>]) -> bool {
    let src_len = src.len();
    let dst_len = dst.len();
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr().cast::<u8>();
    let mut ip = 0usize;
    let mut op = 0usize;

    while ip < src_len {
        let tag = unsafe { *src_ptr.add(ip) };
        ip += 1;
        if tag & 3 != 0 {
            return false;
        }

        let mut len = usize::from(tag >> 2) + 1;
        if len > 60 {
            let len_bytes = len - 60;
            if len_bytes > src_len - ip || 4 > src_len - ip {
                return false;
            }
            len = (unsafe { read_u32(src_ptr.add(ip)) as usize } & word_mask(len_bytes))
                + 1;
            ip += len_bytes;
        }
        if len > src_len - ip || len > dst_len - op {
            return false;
        }

        ip += len;
        op += len;
    }

    if op != dst_len {
        return false;
    }

    ip = 0;
    op = 0;
    while ip < src_len {
        let tag = unsafe { *src_ptr.add(ip) };
        ip += 1;

        let mut len = usize::from(tag >> 2) + 1;
        if len > 60 {
            let len_bytes = len - 60;
            len = (unsafe { read_u32(src_ptr.add(ip)) as usize } & word_mask(len_bytes))
                + 1;
            ip += len_bytes;
        }

        unsafe {
            ptr::copy_nonoverlapping(src_ptr.add(ip), dst_ptr.add(op), len);
        }
        ip += len;
        op += len;
    }

    true
}

const fn build_copy_tag_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut tag = 0usize;
    while tag < 256 {
        let entry = match tag & 3 {
            1 => {
                let len = 4 + ((tag >> 2) & 7);
                let offset_high = (tag & 0xe0) << 3;
                (1 << 11) | offset_high | len
            }
            2 => {
                let len = 1 + (tag >> 2);
                (2 << 11) | len
            }
            3 => {
                let len = 1 + (tag >> 2);
                (4 << 11) | len
            }
            _ => 0,
        };
        table[tag] = entry as u16;
        tag += 1;
    }
    table
}

#[inline(always)]
fn decode_body_table_fast(
    src: &[u8],
    dst: &mut [MaybeUninit<u8>],
) -> Result<(usize, usize)> {
    let src_len = src.len();
    let dst_len = dst.len();
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr().cast::<u8>();
    let mut ip = 0usize;
    let mut op = 0usize;

    if src_len >= 17 && dst_len >= 88 {
        let ip_limit = src_len - 17;
        let op_limit = dst_len - 88;
        let mut preload = unsafe { u32::from(*src_ptr.add(ip)) };
        let mut next_entry = usize::from(COPY_TAG_TABLE[preload as u8 as usize]);
        let mut next_loaded = unsafe { read_u32(src_ptr.add(ip + 1)) };

        loop {
            let tag = preload as u8;
            if tag & 3 != 0 {
                let loaded = next_loaded;
                let tag_type = usize::from(tag & 3);
                if tag_type == 3 {
                    break;
                }

                let entry = next_entry;
                let len = entry & 0xff;
                ip += 1 + tag_type;

                let mask = (1u32 << (tag_type as u32 * 8)).wrapping_sub(1);
                let offset = (entry & 0x700) | ((loaded & mask) as usize);
                let copy_dst = unsafe { dst_ptr.add(op) };
                if op <= offset.wrapping_sub(1) {
                    return decode_error(Error::Offset {
                        offset,
                        dst_pos: op,
                    });
                }

                preload = loaded >> (tag_type as u32 * 8);
                next_entry = usize::from(COPY_TAG_TABLE[preload as u8 as usize]);
                next_loaded = unsafe { read_u32(src_ptr.add(ip + 1)) };

                unsafe {
                    copy_table_match_fast(copy_dst, offset, len);
                }
                op += len;
            } else {
                ip += 1;
                let len = if tag <= 0x3c {
                    let len = usize::from(tag >> 2) + 1;
                    unsafe {
                        ptr::copy_nonoverlapping(src_ptr.add(ip), dst_ptr.add(op), 16);
                    }
                    ip += len;
                    op += len;
                    if ip > ip_limit || op > op_limit {
                        break;
                    }
                    preload = unsafe { u32::from(*src_ptr.add(ip)) };
                    next_entry = usize::from(COPY_TAG_TABLE[preload as u8 as usize]);
                    next_loaded = unsafe { read_u32(src_ptr.add(ip + 1)) };
                    continue;
                } else if tag <= 0xec {
                    usize::from(tag >> 2) + 1
                } else {
                    let (next_ip, next_op) = unsafe {
                        decode_long_literal_fast(
                            src_ptr, src_len, dst_ptr, dst_len, ip, op, tag,
                        )?
                    };
                    ip = next_ip;
                    op = next_op;
                    if ip > ip_limit || op > op_limit {
                        break;
                    }
                    preload = unsafe { u32::from(*src_ptr.add(ip)) };
                    next_entry = usize::from(COPY_TAG_TABLE[preload as u8 as usize]);
                    next_loaded = unsafe { read_u32(src_ptr.add(ip + 1)) };
                    continue;
                };

                if len <= 64 && ip + 64 <= src_len {
                    unsafe {
                        copy_literal_wide(src_ptr.add(ip), dst_ptr.add(op), len);
                    }
                } else {
                    if len > src_len - ip {
                        return decode_error(Error::LiteralRead {
                            len: len as u64,
                            remaining: (src_len - ip) as u64,
                        });
                    }
                    if len > dst_len - op {
                        return decode_error(Error::LiteralWrite {
                            len: len as u64,
                            remaining: (dst_len - op) as u64,
                        });
                    }
                    unsafe {
                        ptr::copy_nonoverlapping(src_ptr.add(ip), dst_ptr.add(op), len);
                    }
                }
                ip += len;
                op += len;

                if ip > ip_limit || op > op_limit {
                    break;
                }
                preload = unsafe { u32::from(*src_ptr.add(ip)) };
                next_entry = usize::from(COPY_TAG_TABLE[preload as u8 as usize]);
                next_loaded = unsafe { read_u32(src_ptr.add(ip + 1)) };
            }

            if ip > ip_limit || op > op_limit {
                break;
            }
        }
    }

    Ok((ip, op))
}

#[inline(never)]
fn decode_body_table(src: &[u8], dst: &mut [MaybeUninit<u8>]) -> Result<()> {
    let src_len = src.len();
    let dst_len = dst.len();
    let (mut ip, mut op) = decode_body_table_fast(src, dst)?;
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr().cast::<u8>();
    let fast_short_literals = dst_len <= src_len.saturating_mul(2);

    while ip < src_len {
        let tag = unsafe { *src_ptr.add(ip) };
        ip += 1;

        if fast_short_literals
            && tag & 3 == 0
            && tag <= 0x3c
            && 16 <= src_len - ip
            && 16 <= dst_len - op
        {
            let len = usize::from(tag >> 2) + 1;
            unsafe {
                ptr::copy_nonoverlapping(src_ptr.add(ip), dst_ptr.add(op), 16);
            }
            ip += len;
            op += len;
            unsafe {
                copy_table_short_copy_after_literal(
                    src_ptr, src_len, dst_ptr, dst_len, &mut ip, &mut op,
                );
            }
            continue;
        }

        if tag & 3 == 0 {
            let mut len = usize::from(tag >> 2) + 1;
            if len > 60 {
                let len_bytes = len - 60;
                if len_bytes > src_len - ip {
                    return decode_error(Error::LiteralRead {
                        len: len_bytes as u64,
                        remaining: (src_len - ip) as u64,
                    });
                }
                if 4 > src_len - ip {
                    return decode_error(Error::LiteralRead {
                        len: 4,
                        remaining: (src_len - ip) as u64,
                    });
                }
                len = (unsafe { read_u32(src_ptr.add(ip)) as usize }
                    & word_mask(len_bytes))
                    + 1;
                ip += len_bytes;
            }
            if len > src_len - ip {
                return decode_error(Error::LiteralRead {
                    len: len as u64,
                    remaining: (src_len - ip) as u64,
                });
            }
            if len > dst_len - op {
                return decode_error(Error::LiteralWrite {
                    len: len as u64,
                    remaining: (dst_len - op) as u64,
                });
            }
            unsafe {
                copy_literal(
                    src_ptr.add(ip),
                    dst_ptr.add(op),
                    len,
                    src_len - ip,
                    dst_len - op,
                );
            }
            ip += len;
            op += len;
            unsafe {
                copy_table_short_copy_after_literal(
                    src_ptr, src_len, dst_ptr, dst_len, &mut ip, &mut op,
                );
            }
            continue;
        }

        let entry = usize::from(COPY_TAG_TABLE[tag as usize]);
        let tag_bytes = entry >> 11;
        let trailer = if ip + 4 <= src_len {
            (unsafe { read_u32(src_ptr.add(ip)) as usize }) & word_mask(tag_bytes)
        } else if tag_bytes == 1 {
            if ip >= src_len {
                return decode_error(Error::CopyRead {
                    len: 1,
                    remaining: 0,
                });
            }
            unsafe { usize::from(*src_ptr.add(ip)) }
        } else if tag_bytes == 2 {
            if 2 > src_len - ip {
                return decode_error(Error::CopyRead {
                    len: 2,
                    remaining: (src_len - ip) as u64,
                });
            }
            unsafe { read_u16(src_ptr.add(ip)) }
        } else {
            return decode_error(Error::CopyRead {
                len: tag_bytes as u64,
                remaining: (src_len - ip) as u64,
            });
        };
        ip += tag_bytes;

        let len = entry & 0xff;
        let offset = (entry & 0x700) | trailer;
        unsafe {
            copy_table_match(dst_ptr, dst_len, &mut op, offset, len)?;
        }
    }

    if op != dst_len {
        return decode_error(Error::HeaderMismatch {
            expected: dst_len,
            actual: op,
        });
    }
    Ok(())
}

#[inline(never)]
unsafe fn decode_long_literal_fast(
    src_ptr: *const u8,
    src_len: usize,
    dst_ptr: *mut u8,
    dst_len: usize,
    mut ip: usize,
    mut op: usize,
    tag: u8,
) -> Result<(usize, usize)> {
    let len_bytes = usize::from(tag >> 2) + 1 - 60;
    if len_bytes > src_len - ip {
        return decode_error(Error::LiteralRead {
            len: len_bytes as u64,
            remaining: (src_len - ip) as u64,
        });
    }
    if 4 > src_len - ip {
        return decode_error(Error::LiteralRead {
            len: 4,
            remaining: (src_len - ip) as u64,
        });
    }

    let len = (unsafe { read_u32(src_ptr.add(ip)) as usize } & word_mask(len_bytes)) + 1;
    ip += len_bytes;
    if len > src_len - ip {
        return decode_error(Error::LiteralRead {
            len: len as u64,
            remaining: (src_len - ip) as u64,
        });
    }
    if len > dst_len - op {
        return decode_error(Error::LiteralWrite {
            len: len as u64,
            remaining: (dst_len - op) as u64,
        });
    }

    unsafe {
        if len <= 64 && ip + 64 <= src_len && op + 64 <= dst_len {
            copy_literal_wide(src_ptr.add(ip), dst_ptr.add(op), len);
        } else {
            ptr::copy_nonoverlapping(src_ptr.add(ip), dst_ptr.add(op), len);
        }
    }
    ip += len;
    op += len;
    Ok((ip, op))
}

#[inline(always)]
unsafe fn copy_table_match(
    dst_ptr: *mut u8,
    dst_len: usize,
    op: &mut usize,
    offset: usize,
    len: usize,
) -> Result<()> {
    if *op <= offset.wrapping_sub(1) {
        return decode_error(Error::Offset {
            offset,
            dst_pos: *op,
        });
    }

    let end = *op + len;
    if offset >= 16 && len <= 16 && 16 <= dst_len - *op {
        unsafe {
            copy_match_16_exact(dst_ptr.add(*op - offset), dst_ptr.add(*op));
        }
    } else if offset >= 8 && len <= 16 && 16 <= dst_len - *op {
        unsafe {
            let copy_src = dst_ptr.add(*op - offset);
            let copy_dst = dst_ptr.add(*op);
            ptr::copy_nonoverlapping(copy_src, copy_dst, 8);
            ptr::copy_nonoverlapping(copy_src.add(8), copy_dst.add(8), 8);
        }
    } else if end + 24 <= dst_len {
        unsafe {
            let mut copy_dst = dst_ptr.add(*op);
            let mut copy_src = copy_dst.sub(offset);
            let mut copied_to = *op;
            loop {
                let diff = copy_dst as usize - copy_src as usize;
                if diff >= 16 {
                    break;
                }
                ptr::copy(copy_src, copy_dst, 16);
                copied_to += diff;
                copy_dst = copy_dst.add(diff);
            }
            while copied_to < end {
                ptr::copy_nonoverlapping(copy_src, copy_dst, 16);
                copy_src = copy_src.add(16);
                copy_dst = copy_dst.add(16);
                copied_to += 16;
            }
        }
    } else {
        if len > dst_len - *op {
            return decode_error(Error::CopyWrite {
                len: len as u64,
                remaining: (dst_len - *op) as u64,
            });
        }
        unsafe {
            copy_match(dst_ptr, dst_len, *op, offset, len)?;
        }
    }
    *op = end;
    Ok(())
}

#[inline(always)]
unsafe fn copy_literal_wide(src: *const u8, dst: *mut u8, len: usize) {
    debug_assert!((17..=64).contains(&len));
    unsafe {
        ptr::copy_nonoverlapping(src, dst, 32);
        if len > 32 {
            ptr::copy_nonoverlapping(src.add(32), dst.add(32), 32);
        }
    }
}

#[inline(always)]
unsafe fn copy_table_match_fast(dst: *mut u8, offset: usize, len: usize) {
    debug_assert!((1..=64).contains(&len));
    unsafe {
        if len <= 16 && offset >= 8 {
            let src = dst.sub(offset);
            ptr::copy_nonoverlapping(src, dst, 8);
            ptr::copy_nonoverlapping(src.add(8), dst.add(8), 8);
        } else {
            copy_table_match_fast_cold(dst, offset, len);
        }
    }
}

#[inline(always)]
unsafe fn copy_table_match_fast_cold(dst: *mut u8, offset: usize, len: usize) {
    let src = unsafe { dst.sub(offset) };
    unsafe {
        if offset >= 32 {
            ptr::copy_nonoverlapping(src, dst, 32);
            if len > 32 {
                ptr::copy_nonoverlapping(src.add(32), dst.add(32), 32);
            }
        } else if offset >= 16 {
            ptr::copy_nonoverlapping(src, dst, 16);
            ptr::copy_nonoverlapping(src.add(16), dst.add(16), 16);
            if len > 32 {
                ptr::copy_nonoverlapping(src.add(32), dst.add(32), 16);
                if len > 48 {
                    ptr::copy_nonoverlapping(src.add(48), dst.add(48), 16);
                }
            }
        } else if offset >= len {
            ptr::copy_nonoverlapping(src, dst, len);
        } else if offset == 1 {
            copy_offset1_repeat(dst, *src, len);
        } else {
            copy_table_match_fast_overlap(dst, offset, len);
        }
    }
}

#[inline(always)]
unsafe fn copy_table_match_fast_overlap(dst: *mut u8, offset: usize, len: usize) {
    let end = unsafe { dst.add(len) };
    let mut copy_dst = dst;
    let mut copy_src = unsafe { dst.sub(offset) };
    loop {
        let diff = copy_dst as usize - copy_src as usize;
        if diff >= 16 {
            break;
        }
        unsafe {
            ptr::copy(copy_src, copy_dst, 16);
            copy_dst = copy_dst.add(diff);
        }
    }
    while copy_dst < end {
        unsafe {
            ptr::copy_nonoverlapping(copy_src, copy_dst, 16);
            copy_src = copy_src.add(16);
            copy_dst = copy_dst.add(16);
        }
    }
}

#[inline(always)]
unsafe fn copy_table_short_copy_after_literal(
    src_ptr: *const u8,
    src_len: usize,
    dst_ptr: *mut u8,
    dst_len: usize,
    ip: &mut usize,
    op: &mut usize,
) {
    if 2 > src_len - *ip || 16 > dst_len - *op {
        return;
    }

    let tag = unsafe { *src_ptr.add(*ip) };
    if tag & 3 == 1 {
        let len = 4 + usize::from((tag >> 2) & 7);
        let offset = (usize::from(tag & 0xe0) << 3)
            | unsafe { usize::from(*src_ptr.add(*ip + 1)) };
        if offset >= 16 && offset <= *op {
            unsafe {
                copy_match_16_exact(dst_ptr.add(*op - offset), dst_ptr.add(*op));
            }
            *ip += 2;
            *op += len;
        } else if offset >= 8 && offset <= *op {
            unsafe {
                let copy_src = dst_ptr.add(*op - offset);
                let copy_dst = dst_ptr.add(*op);
                ptr::copy_nonoverlapping(copy_src, copy_dst, 8);
                ptr::copy_nonoverlapping(copy_src.add(8), copy_dst.add(8), 8);
            }
            *ip += 2;
            *op += len;
        }
    } else if tag & 3 == 2 && 3 <= src_len - *ip {
        let len = 1 + usize::from(tag >> 2);
        if len <= 16 {
            let offset = unsafe { read_u16(src_ptr.add(*ip + 1)) };
            if offset >= 16 && offset <= *op {
                unsafe {
                    copy_match_16_exact(dst_ptr.add(*op - offset), dst_ptr.add(*op));
                }
                *ip += 3;
                *op += len;
            } else if offset >= 8 && offset <= *op {
                unsafe {
                    let copy_src = dst_ptr.add(*op - offset);
                    let copy_dst = dst_ptr.add(*op);
                    ptr::copy_nonoverlapping(copy_src, copy_dst, 8);
                    ptr::copy_nonoverlapping(copy_src.add(8), copy_dst.add(8), 8);
                }
                *ip += 3;
                *op += len;
            }
        }
    }
}

#[inline(never)]
fn decode_body<const FAST_COPY_TAGS: bool, const BULK_COPY_RUNS: bool>(
    src: &[u8],
    dst: &mut [MaybeUninit<u8>],
) -> Result<()> {
    let src_len = src.len();
    let dst_len = dst.len();
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr().cast::<u8>();
    let mut ip = 0usize;
    let mut op = 0usize;

    while ip < src_len {
        let tag = unsafe { *src_ptr.add(ip) };
        ip += 1;

        if FAST_COPY_TAGS {
            if 2 <= src_len - ip && 64 <= dst_len - op {
                if tag & 0x03 == 1 {
                    let len = 4 + usize::from((tag >> 2) & 0x07);
                    let offset = (usize::from(tag & 0xe0) << 3)
                        | unsafe { usize::from(*src_ptr.add(ip)) };
                    if offset >= 16 && offset <= op {
                        ip += 1;
                        unsafe {
                            copy_match_16_exact(
                                dst_ptr.add(op - offset),
                                dst_ptr.add(op),
                            );
                        }
                        op += len;
                        unsafe {
                            copy_fast_copy2_window_run_first_room(
                                src_ptr, src_len, dst_ptr, dst_len, &mut ip, &mut op,
                            );
                        }
                        continue;
                    }
                } else if tag == 0x3e {
                    let offset = unsafe { read_u16(src_ptr.add(ip)) };
                    if offset >= 16 && offset <= op {
                        ip += 2;
                        unsafe {
                            copy_match_16_exact(
                                dst_ptr.add(op - offset),
                                dst_ptr.add(op),
                            );
                        }
                        op += 16;
                        unsafe {
                            copy_fast_copy2_window_run_first_room(
                                src_ptr, src_len, dst_ptr, dst_len, &mut ip, &mut op,
                            );
                        }
                        continue;
                    }
                } else if tag & 0x03 == 0 {
                    let len = usize::from(tag >> 2) + 1;
                    if len <= 16 && len <= src_len - ip {
                        unsafe {
                            copy_literal(
                                src_ptr.add(ip),
                                dst_ptr.add(op),
                                len,
                                src_len - ip,
                                dst_len - op,
                            );
                        }
                        ip += len;
                        op += len;
                        if ip < src_len && 16 <= dst_len - op {
                            let next_tag = unsafe { *src_ptr.add(ip) };
                            if next_tag & 0x03 == 1 && ip + 1 < src_len {
                                let next_len = 4 + usize::from((next_tag >> 2) & 0x07);
                                let next_offset = (usize::from(next_tag & 0xe0) << 3)
                                    | unsafe { usize::from(*src_ptr.add(ip + 1)) };
                                if next_offset >= 16 && next_offset <= op {
                                    ip += 2;
                                    unsafe {
                                        copy_match_16_exact(
                                            dst_ptr.add(op - next_offset),
                                            dst_ptr.add(op),
                                        );
                                    }
                                    op += next_len;
                                }
                            }
                        }
                        continue;
                    }
                } else {
                    match tag {
                        0xfe => {
                            let offset = unsafe { read_u16(src_ptr.add(ip)) };
                            if offset >= 64 && offset <= op {
                                if BULK_COPY_RUNS {
                                    let mut run_ip = ip + 2;
                                    let mut copies = 1usize;
                                    let max_copies = (dst_len - op) / 64;
                                    scan_copy2_len64_run(
                                        src_ptr,
                                        src_len,
                                        offset,
                                        &mut run_ip,
                                        &mut copies,
                                        max_copies,
                                    );
                                    if copies >= BULK_COPY_RUN_MIN_COPIES {
                                        ip = run_ip;
                                        let run_len = copies * 64;
                                        unsafe {
                                            copy_match_repeat(
                                                dst_ptr.add(op - offset),
                                                dst_ptr.add(op),
                                                offset,
                                                run_len,
                                            );
                                        }
                                        op += run_len;
                                        continue;
                                    }
                                }
                                ip += 2;
                                unsafe {
                                    copy_match_64(
                                        dst_ptr.add(op - offset),
                                        dst_ptr.add(op),
                                    );
                                }
                                op += 64;
                                continue;
                            }
                        }
                        0xbe => {
                            let offset = unsafe { read_u16(src_ptr.add(ip)) };
                            if offset >= 48 && offset <= op {
                                ip += 2;
                                unsafe {
                                    copy_match_48(
                                        dst_ptr.add(op - offset),
                                        dst_ptr.add(op),
                                    );
                                }
                                op += 48;
                                unsafe {
                                    copy_fast_copy2_window_run(
                                        src_ptr, src_len, dst_ptr, dst_len, &mut ip,
                                        &mut op,
                                    );
                                }
                                continue;
                            }
                        }
                        0xae => {
                            let offset = unsafe { read_u16(src_ptr.add(ip)) };
                            if offset >= 44 && offset <= op {
                                ip += 2;
                                unsafe {
                                    copy_match_44(
                                        dst_ptr.add(op - offset),
                                        dst_ptr.add(op),
                                    );
                                }
                                op += 44;
                                unsafe {
                                    copy_fast_copy2_window_run(
                                        src_ptr, src_len, dst_ptr, dst_len, &mut ip,
                                        &mut op,
                                    );
                                }
                                continue;
                            }
                        }
                        0xa6 => {
                            let offset = unsafe { read_u16(src_ptr.add(ip)) };
                            if offset >= 42 && offset <= op {
                                ip += 2;
                                unsafe {
                                    copy_match_42(
                                        dst_ptr.add(op - offset),
                                        dst_ptr.add(op),
                                    );
                                }
                                op += 42;
                                unsafe {
                                    copy_fast_copy2_window_run(
                                        src_ptr, src_len, dst_ptr, dst_len, &mut ip,
                                        &mut op,
                                    );
                                }
                                continue;
                            }
                        }
                        0x96 => {
                            let offset = unsafe { read_u16(src_ptr.add(ip)) };
                            if offset >= 38 && offset <= op {
                                ip += 2;
                                unsafe {
                                    copy_match_38(
                                        dst_ptr.add(op - offset),
                                        dst_ptr.add(op),
                                    );
                                }
                                op += 38;
                                unsafe {
                                    copy_fast_copy2_window_run(
                                        src_ptr, src_len, dst_ptr, dst_len, &mut ip,
                                        &mut op,
                                    );
                                }
                                continue;
                            }
                            if offset >= 8 && offset <= op {
                                ip += 2;
                                unsafe {
                                    copy_match_38_overlap(
                                        dst_ptr.add(op - offset),
                                        dst_ptr.add(op),
                                    );
                                }
                                op += 38;
                                unsafe {
                                    copy_fast_copy2_window_run(
                                        src_ptr, src_len, dst_ptr, dst_len, &mut ip,
                                        &mut op,
                                    );
                                }
                                continue;
                            }
                        }
                        _ => {}
                    }
                }
            }
            if tag == 0xfe && 2 <= src_len - ip {
                let offset = unsafe { read_u16(src_ptr.add(ip)) };
                if offset >= 64 && offset <= op && 64 <= dst_len - op {
                    if BULK_COPY_RUNS {
                        let mut run_ip = ip + 2;
                        let mut copies = 1usize;
                        let max_copies = (dst_len - op) / 64;
                        scan_copy2_len64_run(
                            src_ptr,
                            src_len,
                            offset,
                            &mut run_ip,
                            &mut copies,
                            max_copies,
                        );
                        if copies >= BULK_COPY_RUN_MIN_COPIES {
                            ip = run_ip;
                            let run_len = copies * 64;
                            unsafe {
                                copy_match_repeat(
                                    dst_ptr.add(op - offset),
                                    dst_ptr.add(op),
                                    offset,
                                    run_len,
                                );
                            }
                            op += run_len;
                            continue;
                        }
                    }
                    ip += 2;
                    unsafe {
                        copy_match_64(dst_ptr.add(op - offset), dst_ptr.add(op));
                    }
                    op += 64;
                    continue;
                }
                if BULK_COPY_RUNS && offset > 0 && offset <= op && 64 <= dst_len - op {
                    let mut run_ip = ip + 2;
                    let mut copies = 1usize;
                    let max_copies = (dst_len - op) / 64;
                    scan_copy2_len64_run(
                        src_ptr,
                        src_len,
                        offset,
                        &mut run_ip,
                        &mut copies,
                        max_copies,
                    );
                    if copies >= BULK_COPY_RUN_MIN_COPIES {
                        ip = run_ip;
                        let run_len = copies * 64;
                        unsafe {
                            copy_match_repeat(
                                dst_ptr.add(op - offset),
                                dst_ptr.add(op),
                                offset,
                                run_len,
                            );
                        }
                        op += run_len;
                        continue;
                    }
                }
            }
            if 2 <= src_len - ip {
                match tag {
                    0x3e => {
                        let offset = unsafe { read_u16(src_ptr.add(ip)) };
                        if offset >= 16 && offset <= op && 16 <= dst_len - op {
                            ip += 2;
                            unsafe {
                                copy_match_16_exact(
                                    dst_ptr.add(op - offset),
                                    dst_ptr.add(op),
                                );
                            }
                            op += 16;
                            continue;
                        }
                    }
                    0xbe => {
                        let offset = unsafe { read_u16(src_ptr.add(ip)) };
                        if offset >= 48 && offset <= op && 48 <= dst_len - op {
                            ip += 2;
                            unsafe {
                                copy_match_48(dst_ptr.add(op - offset), dst_ptr.add(op));
                            }
                            op += 48;
                            continue;
                        }
                    }
                    0xae => {
                        let offset = unsafe { read_u16(src_ptr.add(ip)) };
                        if offset >= 44 && offset <= op && 44 <= dst_len - op {
                            ip += 2;
                            unsafe {
                                copy_match_44(dst_ptr.add(op - offset), dst_ptr.add(op));
                            }
                            op += 44;
                            continue;
                        }
                    }
                    0xa6 => {
                        let offset = unsafe { read_u16(src_ptr.add(ip)) };
                        if offset >= 42 && offset <= op && 42 <= dst_len - op {
                            ip += 2;
                            unsafe {
                                copy_match_42(dst_ptr.add(op - offset), dst_ptr.add(op));
                            }
                            op += 42;
                            continue;
                        }
                    }
                    0x96 => {
                        let offset = unsafe { read_u16(src_ptr.add(ip)) };
                        if offset >= 38 && offset <= op && 38 <= dst_len - op {
                            ip += 2;
                            unsafe {
                                copy_match_38(dst_ptr.add(op - offset), dst_ptr.add(op));
                            }
                            op += 38;
                            continue;
                        }
                        if offset >= 8 && offset <= op && 38 <= dst_len - op {
                            ip += 2;
                            unsafe {
                                copy_match_38_overlap(
                                    dst_ptr.add(op - offset),
                                    dst_ptr.add(op),
                                );
                            }
                            op += 38;
                            continue;
                        }
                    }
                    _ => {}
                }
            }
        }

        match tag & 0x03 {
            0 => {
                let mut len = usize::from(tag >> 2) + 1;
                if len > 60 {
                    let len_bytes = len - 60;
                    if len_bytes > src_len - ip {
                        return decode_error(Error::LiteralRead {
                            len: len_bytes as u64,
                            remaining: (src_len - ip) as u64,
                        });
                    }
                    let len64 =
                        unsafe { read_literal_len(src_ptr.add(ip), len_bytes) } + 1;
                    ip += len_bytes;
                    if len64 > (src_len - ip) as u64 {
                        return decode_error(Error::LiteralRead {
                            len: len64,
                            remaining: (src_len - ip) as u64,
                        });
                    }
                    if len64 > (dst_len - op) as u64 {
                        return decode_error(Error::LiteralWrite {
                            len: len64,
                            remaining: (dst_len - op) as u64,
                        });
                    }
                    len = len64 as usize;
                }
                if len > src_len - ip {
                    return decode_error(Error::LiteralRead {
                        len: len as u64,
                        remaining: (src_len - ip) as u64,
                    });
                }
                if len > dst_len - op {
                    return decode_error(Error::LiteralWrite {
                        len: len as u64,
                        remaining: (dst_len - op) as u64,
                    });
                }
                unsafe {
                    copy_literal(
                        src_ptr.add(ip),
                        dst_ptr.add(op),
                        len,
                        src_len - ip,
                        dst_len - op,
                    );
                }
                ip += len;
                op += len;
            }
            1 => {
                if ip >= src_len {
                    return decode_error(Error::CopyRead {
                        len: 1,
                        remaining: 0,
                    });
                }
                let len = 4 + usize::from((tag >> 2) & 0x07);
                let offset = (usize::from(tag & 0xe0) << 3)
                    | unsafe { usize::from(*src_ptr.add(ip)) };
                ip += 1;
                if offset >= 16 {
                    if offset > op {
                        return decode_error(Error::Offset {
                            offset,
                            dst_pos: op,
                        });
                    }
                    if 16 <= dst_len - op {
                        unsafe {
                            copy_match_16_exact(
                                dst_ptr.add(op - offset),
                                dst_ptr.add(op),
                            );
                        }
                        op += len;
                        continue;
                    }
                }
                unsafe {
                    copy_match(dst_ptr, dst_len, op, offset, len)?;
                }
                op += len;
            }
            2 => {
                if 2 > src_len - ip {
                    return decode_error(Error::CopyRead {
                        len: 2,
                        remaining: (src_len - ip) as u64,
                    });
                }
                let len = 1 + usize::from(tag >> 2);
                let offset = unsafe { read_u16(src_ptr.add(ip)) };
                ip += 2;
                if offset >= 16 {
                    if offset > op {
                        return decode_error(Error::Offset {
                            offset,
                            dst_pos: op,
                        });
                    }
                    let dst_remaining = dst_len - op;
                    if dst_remaining >= 64 {
                        unsafe {
                            copy_match_16_plus(
                                dst_ptr.add(op - offset),
                                dst_ptr.add(op),
                                offset,
                                len,
                            );
                        }
                    } else {
                        if len > dst_remaining {
                            return decode_error(Error::CopyWrite {
                                len: len as u64,
                                remaining: dst_remaining as u64,
                            });
                        }
                        unsafe {
                            if offset == 16 {
                                copy_match_offset16(
                                    dst_ptr.add(op - offset),
                                    dst_ptr.add(op),
                                    len,
                                );
                            } else {
                                copy_match_16(
                                    dst_ptr.add(op - offset),
                                    dst_ptr.add(op),
                                    len,
                                    dst_remaining,
                                );
                            }
                        }
                    }
                } else {
                    unsafe {
                        copy_match(dst_ptr, dst_len, op, offset, len)?;
                    }
                }
                op += len;
            }
            _ => {
                if 4 > src_len - ip {
                    return decode_error(Error::CopyRead {
                        len: 4,
                        remaining: (src_len - ip) as u64,
                    });
                }
                let len = 1 + usize::from(tag >> 2);
                let offset = unsafe { read_u32(src_ptr.add(ip)) as usize };
                ip += 4;
                unsafe {
                    copy_match(dst_ptr, dst_len, op, offset, len)?;
                }
                op += len;
            }
        }
    }

    if op != dst_len {
        return decode_error(Error::HeaderMismatch {
            expected: dst_len,
            actual: op,
        });
    }
    Ok(())
}

#[inline(always)]
unsafe fn copy_fast_copy2_window_run(
    src_ptr: *const u8,
    src_len: usize,
    dst_ptr: *mut u8,
    dst_len: usize,
    ip: &mut usize,
    op: &mut usize,
) {
    let mut ip_pos = *ip;
    let mut op_pos = *op;
    if 9 <= src_len - ip_pos {
        if !unsafe {
            try_copy_fast_copy2_window_unchecked_src(
                src_ptr,
                dst_ptr,
                dst_len,
                &mut ip_pos,
                &mut op_pos,
            )
        } {
            *ip = ip_pos;
            *op = op_pos;
            return;
        }
        if !unsafe {
            try_copy_fast_copy2_window_unchecked_src(
                src_ptr,
                dst_ptr,
                dst_len,
                &mut ip_pos,
                &mut op_pos,
            )
        } {
            *ip = ip_pos;
            *op = op_pos;
            return;
        }
        if !unsafe {
            try_copy_fast_copy2_window_unchecked_src(
                src_ptr,
                dst_ptr,
                dst_len,
                &mut ip_pos,
                &mut op_pos,
            )
        } {
            *ip = ip_pos;
            *op = op_pos;
            return;
        }
    }
    *ip = ip_pos;
    *op = op_pos;
}

#[inline(always)]
unsafe fn copy_fast_copy2_window_run_first_room(
    src_ptr: *const u8,
    src_len: usize,
    dst_ptr: *mut u8,
    dst_len: usize,
    ip: &mut usize,
    op: &mut usize,
) {
    let mut ip_pos = *ip;
    let mut op_pos = *op;
    if 9 <= src_len - ip_pos {
        if !unsafe {
            try_copy_fast_copy2_window_unchecked_src_first_room(
                src_ptr,
                dst_ptr,
                &mut ip_pos,
                &mut op_pos,
            )
        } {
            *ip = ip_pos;
            *op = op_pos;
            return;
        }
        if !unsafe {
            try_copy_fast_copy2_window_unchecked_src(
                src_ptr,
                dst_ptr,
                dst_len,
                &mut ip_pos,
                &mut op_pos,
            )
        } {
            *ip = ip_pos;
            *op = op_pos;
            return;
        }
        if !unsafe {
            try_copy_fast_copy2_window_unchecked_src(
                src_ptr,
                dst_ptr,
                dst_len,
                &mut ip_pos,
                &mut op_pos,
            )
        } {
            *ip = ip_pos;
            *op = op_pos;
            return;
        }
    }
    *ip = ip_pos;
    *op = op_pos;
}

#[inline(always)]
unsafe fn try_copy_fast_copy2_window_unchecked_src_first_room(
    src_ptr: *const u8,
    dst_ptr: *mut u8,
    ip: &mut usize,
    op: &mut usize,
) -> bool {
    let tag = unsafe { *src_ptr.add(*ip) };
    let offset = unsafe { read_u16(src_ptr.add(*ip + 1)) };
    let len = match tag {
        0x3e => 16usize,
        0xbe => 48,
        0xae => 44,
        0x96 => 38,
        _ => return false,
    };
    if offset > *op {
        return false;
    }
    unsafe {
        if offset >= len {
            match tag {
                0x3e => copy_match_16_exact(dst_ptr.add(*op - offset), dst_ptr.add(*op)),
                0xbe => copy_match_48(dst_ptr.add(*op - offset), dst_ptr.add(*op)),
                0xae => copy_match_44(dst_ptr.add(*op - offset), dst_ptr.add(*op)),
                _ => copy_match_38(dst_ptr.add(*op - offset), dst_ptr.add(*op)),
            }
        } else if tag == 0x96 && offset >= 8 {
            copy_match_38_overlap(dst_ptr.add(*op - offset), dst_ptr.add(*op));
        } else {
            return false;
        }
    }
    *ip += 3;
    *op += len;
    true
}

#[inline(always)]
unsafe fn try_copy_fast_copy2_window_unchecked_src(
    src_ptr: *const u8,
    dst_ptr: *mut u8,
    dst_len: usize,
    ip: &mut usize,
    op: &mut usize,
) -> bool {
    let tag = unsafe { *src_ptr.add(*ip) };
    let offset = unsafe { read_u16(src_ptr.add(*ip + 1)) };
    let len = match tag {
        0x3e => 16usize,
        0xbe => 48,
        0xae => 44,
        0x96 => 38,
        _ => return false,
    };
    let dst_remaining = dst_len - *op;
    if offset > *op || len > dst_remaining {
        return false;
    }
    unsafe {
        if offset >= len {
            match tag {
                0x3e => copy_match_16_exact(dst_ptr.add(*op - offset), dst_ptr.add(*op)),
                0xbe => copy_match_48(dst_ptr.add(*op - offset), dst_ptr.add(*op)),
                0xae => copy_match_44(dst_ptr.add(*op - offset), dst_ptr.add(*op)),
                _ => copy_match_38(dst_ptr.add(*op - offset), dst_ptr.add(*op)),
            }
        } else if tag == 0x96 && offset >= 8 {
            copy_match_38_overlap(dst_ptr.add(*op - offset), dst_ptr.add(*op));
        } else {
            return false;
        }
    }
    *ip += 3;
    *op += len;
    true
}

#[inline(always)]
unsafe fn read_literal_len(ptr: *const u8, len_bytes: usize) -> u64 {
    let mut len = 0u64;
    for i in 0..len_bytes {
        len |= unsafe { u64::from(*ptr.add(i)) } << (8 * i);
    }
    len
}

#[inline(always)]
unsafe fn read_u16(ptr: *const u8) -> usize {
    u16::from_le(unsafe { ptr::read_unaligned(ptr.cast::<u16>()) }) as usize
}

#[inline(always)]
unsafe fn read_u32(ptr: *const u8) -> u32 {
    u32::from_le(unsafe { ptr::read_unaligned(ptr.cast::<u32>()) })
}

#[inline(always)]
unsafe fn read_u64(ptr: *const u8) -> u64 {
    u64::from_le(unsafe { ptr::read_unaligned(ptr.cast::<u64>()) })
}

#[inline(always)]
fn scan_copy2_len64_run(
    src_ptr: *const u8,
    src_len: usize,
    offset: usize,
    run_ip: &mut usize,
    copies: &mut usize,
    max_copies: usize,
) {
    let offset_lo = offset as u8;
    let offset_hi = (offset >> 8) as u8;
    let pattern8 = u64::from_le_bytes([
        0xfe, offset_lo, offset_hi, 0xfe, offset_lo, offset_hi, 0xfe, offset_lo,
    ]);
    let pattern4 = u32::from_le_bytes([offset_hi, 0xfe, offset_lo, offset_hi]);

    while *copies + 4 <= max_copies && 12 <= src_len - *run_ip {
        let prefix = unsafe { read_u64(src_ptr.add(*run_ip)) };
        let suffix = unsafe { read_u32(src_ptr.add(*run_ip + 8)) };
        if prefix != pattern8 || suffix != pattern4 {
            break;
        }
        *run_ip += 12;
        *copies += 4;
    }
    while *copies < max_copies && 3 <= src_len - *run_ip {
        if unsafe { *src_ptr.add(*run_ip) } != 0xfe {
            break;
        }
        let next_offset = unsafe { read_u16(src_ptr.add(*run_ip + 1)) };
        if next_offset != offset {
            break;
        }
        *run_ip += 3;
        *copies += 1;
    }
}

#[inline(always)]
unsafe fn copy_literal(
    src: *const u8,
    dst: *mut u8,
    len: usize,
    src_remaining: usize,
    dst_remaining: usize,
) {
    if len <= 16 && src_remaining >= 16 && dst_remaining >= 16 {
        unsafe {
            ptr::copy_nonoverlapping(src, dst, 16);
        }
    } else if len <= 16 {
        unsafe {
            match len {
                0 => {}
                1 => *dst = *src,
                2 => {
                    let word = ptr::read_unaligned(src.cast::<u16>());
                    ptr::write_unaligned(dst.cast::<u16>(), word);
                }
                3 => {
                    let word = ptr::read_unaligned(src.cast::<u16>());
                    ptr::write_unaligned(dst.cast::<u16>(), word);
                    *dst.add(2) = *src.add(2);
                }
                4 => {
                    let word = ptr::read_unaligned(src.cast::<u32>());
                    ptr::write_unaligned(dst.cast::<u32>(), word);
                }
                5 => {
                    let word = ptr::read_unaligned(src.cast::<u32>());
                    ptr::write_unaligned(dst.cast::<u32>(), word);
                    *dst.add(4) = *src.add(4);
                }
                6 => {
                    let word = ptr::read_unaligned(src.cast::<u32>());
                    ptr::write_unaligned(dst.cast::<u32>(), word);
                    let tail = ptr::read_unaligned(src.add(4).cast::<u16>());
                    ptr::write_unaligned(dst.add(4).cast::<u16>(), tail);
                }
                7 => {
                    let word = ptr::read_unaligned(src.cast::<u32>());
                    ptr::write_unaligned(dst.cast::<u32>(), word);
                    let tail = ptr::read_unaligned(src.add(4).cast::<u16>());
                    ptr::write_unaligned(dst.add(4).cast::<u16>(), tail);
                    *dst.add(6) = *src.add(6);
                }
                8 => {
                    let word = ptr::read_unaligned(src.cast::<u64>());
                    ptr::write_unaligned(dst.cast::<u64>(), word);
                }
                _ => {
                    let head = ptr::read_unaligned(src.cast::<u64>());
                    ptr::write_unaligned(dst.cast::<u64>(), head);
                    let tail_offset = len - 8;
                    let tail = ptr::read_unaligned(src.add(tail_offset).cast::<u64>());
                    ptr::write_unaligned(dst.add(tail_offset).cast::<u64>(), tail);
                }
            }
        }
    } else {
        unsafe {
            ptr::copy_nonoverlapping(src, dst, len);
        }
    }
}

#[inline(always)]
unsafe fn copy_match(
    dst: *mut u8,
    dst_len: usize,
    op: usize,
    offset: usize,
    len: usize,
) -> Result<()> {
    if offset == 0 || offset > op {
        return decode_error(Error::Offset {
            offset,
            dst_pos: op,
        });
    }
    if len > dst_len - op {
        return decode_error(Error::CopyWrite {
            len: len as u64,
            remaining: (dst_len - op) as u64,
        });
    }

    let dst_remaining = dst_len - op;
    if offset == 16 {
        unsafe {
            copy_match_offset16(dst.add(op - offset), dst.add(op), len);
        }
    } else if offset >= 16 {
        unsafe {
            copy_match_16(dst.add(op - offset), dst.add(op), len, dst_remaining);
        }
    } else if offset >= len {
        unsafe {
            ptr::copy_nonoverlapping(dst.add(op - offset), dst.add(op), len);
        }
    } else if offset == 8 {
        unsafe {
            copy_match_offset8(dst.add(op - offset), dst.add(op), len);
        }
    } else if offset >= 8 {
        unsafe {
            copy_match_wide(dst.add(op - offset), dst.add(op), len);
        }
    } else if offset == 3 {
        unsafe {
            copy_match_offset3(dst.add(op - offset), dst.add(op), len);
        }
    } else if matches!(offset, 1 | 2 | 4) {
        unsafe {
            copy_match_short_word(dst.add(op - offset), dst.add(op), offset, len);
        }
    } else {
        unsafe {
            copy_match_short_period(dst.add(op - offset), dst.add(op), offset, len);
        }
    }
    Ok(())
}

#[inline(always)]
unsafe fn copy_match_16_plus(src: *const u8, dst: *mut u8, offset: usize, len: usize) {
    unsafe {
        if len == 16 {
            copy_match_16_exact(src, dst);
        } else if offset == 16 {
            copy_match_offset16(src, dst, len);
        } else if len == 64 && offset >= 64 {
            copy_match_64(src, dst);
        } else if len == 48 && offset >= 48 {
            copy_match_48(src, dst);
        } else if len == 44 && offset >= 44 {
            copy_match_44(src, dst);
        } else if len == 38 && offset >= 38 {
            copy_match_38(src, dst);
        } else {
            copy_match_16(src, dst, len, 64);
        }
    }
}

#[inline(always)]
unsafe fn copy_match_16_exact(src: *const u8, dst: *mut u8) {
    unsafe {
        let word = ptr::read_unaligned(src.cast::<u128>());
        ptr::write_unaligned(dst.cast::<u128>(), word);
    }
}

#[inline(always)]
unsafe fn copy_match_64(src: *const u8, dst: *mut u8) {
    unsafe {
        let word0 = ptr::read_unaligned(src.cast::<u128>());
        ptr::write_unaligned(dst.cast::<u128>(), word0);
        let word1 = ptr::read_unaligned(src.add(16).cast::<u128>());
        ptr::write_unaligned(dst.add(16).cast::<u128>(), word1);
        let word2 = ptr::read_unaligned(src.add(32).cast::<u128>());
        ptr::write_unaligned(dst.add(32).cast::<u128>(), word2);
        let word3 = ptr::read_unaligned(src.add(48).cast::<u128>());
        ptr::write_unaligned(dst.add(48).cast::<u128>(), word3);
    }
}

#[inline(always)]
unsafe fn copy_match_repeat(src: *const u8, dst: *mut u8, offset: usize, len: usize) {
    let head = offset.min(len);
    unsafe {
        ptr::copy_nonoverlapping(src, dst, head);
    }
    let mut copied = head;
    while copied < len {
        let step = copied.min(len - copied);
        unsafe {
            ptr::copy_nonoverlapping(dst, dst.add(copied), step);
        }
        copied += step;
    }
}

#[inline(always)]
unsafe fn copy_match_48(src: *const u8, dst: *mut u8) {
    unsafe {
        ptr::copy_nonoverlapping(src, dst, 48);
    }
}

#[inline(always)]
unsafe fn copy_match_44(src: *const u8, dst: *mut u8) {
    unsafe {
        ptr::copy_nonoverlapping(src, dst, 44);
    }
}

#[inline(always)]
unsafe fn copy_match_42(src: *const u8, dst: *mut u8) {
    unsafe {
        ptr::copy_nonoverlapping(src, dst, 42);
    }
}

#[inline(always)]
unsafe fn copy_match_38(src: *const u8, dst: *mut u8) {
    unsafe {
        ptr::copy_nonoverlapping(src, dst, 38);
    }
}

#[inline(always)]
unsafe fn copy_match_38_overlap(src: *const u8, dst: *mut u8) {
    unsafe {
        let word0 = ptr::read_unaligned(src.cast::<u64>());
        ptr::write_unaligned(dst.cast::<u64>(), word0);
        let word1 = ptr::read_unaligned(src.add(8).cast::<u64>());
        ptr::write_unaligned(dst.add(8).cast::<u64>(), word1);
        let word2 = ptr::read_unaligned(src.add(16).cast::<u64>());
        ptr::write_unaligned(dst.add(16).cast::<u64>(), word2);
        let word3 = ptr::read_unaligned(src.add(24).cast::<u64>());
        ptr::write_unaligned(dst.add(24).cast::<u64>(), word3);
        let word4 = ptr::read_unaligned(src.add(32).cast::<u32>());
        ptr::write_unaligned(dst.add(32).cast::<u32>(), word4);
        let word5 = ptr::read_unaligned(src.add(36).cast::<u16>());
        ptr::write_unaligned(dst.add(36).cast::<u16>(), word5);
    }
}

#[inline(always)]
unsafe fn copy_offset1_repeat(dst: *mut u8, byte: u8, len: usize) {
    let wide = u128::from_ne_bytes([byte; 16]);
    let word = wide as u64;
    unsafe {
        if len >= 16 {
            ptr::write_unaligned(dst.cast::<u128>(), wide);
            if len > 32 {
                ptr::write_unaligned(dst.add(16).cast::<u128>(), wide);
            }
            if len > 48 {
                ptr::write_unaligned(dst.add(32).cast::<u128>(), wide);
            }
            if len > 16 {
                ptr::write_unaligned(dst.add(len - 16).cast::<u128>(), wide);
            }
        } else if len >= 8 {
            ptr::write_unaligned(dst.cast::<u64>(), word);
            if len > 8 {
                ptr::write_unaligned(dst.add(len - 8).cast::<u64>(), word);
            }
        } else if len >= 4 {
            ptr::write_unaligned(dst.cast::<u32>(), word as u32);
            if len > 4 {
                ptr::write_unaligned(dst.add(len - 4).cast::<u32>(), word as u32);
            }
        } else if len >= 2 {
            ptr::write_unaligned(dst.cast::<u16>(), word as u16);
            if len > 2 {
                ptr::write_unaligned(dst.add(len - 2).cast::<u16>(), word as u16);
            }
        } else if len == 1 {
            *dst = byte;
        }
    }
}

#[inline(always)]
unsafe fn copy_match_short_word(src: *const u8, dst: *mut u8, offset: usize, len: usize) {
    let word = match offset {
        1 => {
            let byte = unsafe { *src };
            u64::from_ne_bytes([byte; 8])
        }
        2 => {
            let a = unsafe { *src };
            let b = unsafe { *src.add(1) };
            u64::from_ne_bytes([a, b, a, b, a, b, a, b])
        }
        _ => {
            let word = unsafe { ptr::read_unaligned(src.cast::<u32>()) };
            u64::from(word) | (u64::from(word) << 32)
        }
    };

    let mut copied = 0usize;
    while copied + 8 <= len {
        unsafe {
            ptr::write_unaligned(dst.add(copied).cast::<u64>(), word);
        }
        copied += 8;
    }
    while copied < len {
        unsafe {
            *dst.add(copied) = *src.add(copied & (offset - 1));
        }
        copied += 1;
    }
}

#[inline(always)]
unsafe fn copy_match_offset3(src: *const u8, dst: *mut u8, len: usize) {
    let a = unsafe { *src };
    let b = unsafe { *src.add(1) };
    let c = unsafe { *src.add(2) };
    let phase0 = u64::from_ne_bytes([a, b, c, a, b, c, a, b]);
    let phase1 = u64::from_ne_bytes([b, c, a, b, c, a, b, c]);
    let phase2 = u64::from_ne_bytes([c, a, b, c, a, b, c, a]);

    let mut copied = 0usize;
    while copied + 24 <= len {
        unsafe {
            ptr::write_unaligned(dst.add(copied).cast::<u64>(), phase0);
            ptr::write_unaligned(dst.add(copied + 8).cast::<u64>(), phase2);
            ptr::write_unaligned(dst.add(copied + 16).cast::<u64>(), phase1);
        }
        copied += 24;
    }
    while copied + 8 <= len {
        let word = match copied % 3 {
            0 => phase0,
            1 => phase1,
            _ => phase2,
        };
        unsafe {
            ptr::write_unaligned(dst.add(copied).cast::<u64>(), word);
        }
        copied += 8;
    }
    while copied < len {
        let byte = match copied % 3 {
            0 => a,
            1 => b,
            _ => c,
        };
        unsafe {
            *dst.add(copied) = byte;
        }
        copied += 1;
    }
}

#[inline(always)]
unsafe fn copy_match_offset8(src: *const u8, dst: *mut u8, len: usize) {
    let word = unsafe { ptr::read_unaligned(src.cast::<u64>()) };
    let mut copied = 0usize;
    while copied + 8 <= len {
        unsafe {
            ptr::write_unaligned(dst.add(copied).cast::<u64>(), word);
        }
        copied += 8;
    }
    while copied < len {
        unsafe {
            *dst.add(copied) = *src.add(copied & 7);
        }
        copied += 1;
    }
}

#[inline(always)]
unsafe fn copy_match_offset16(src: *const u8, dst: *mut u8, len: usize) {
    let word = unsafe { ptr::read_unaligned(src.cast::<u128>()) };
    let word0 = unsafe { ptr::read_unaligned(src.cast::<u64>()) };
    let mut copied = 0usize;
    while copied + 16 <= len {
        unsafe {
            ptr::write_unaligned(dst.add(copied).cast::<u128>(), word);
        }
        copied += 16;
    }
    if copied + 8 <= len {
        unsafe {
            ptr::write_unaligned(dst.add(copied).cast::<u64>(), word0);
        }
        copied += 8;
    }
    while copied < len {
        unsafe {
            *dst.add(copied) = *src.add(copied & 15);
        }
        copied += 1;
    }
}

#[inline(always)]
unsafe fn copy_match_16(src: *const u8, dst: *mut u8, len: usize, dst_remaining: usize) {
    if len <= 16 && dst_remaining >= 16 {
        unsafe {
            let word = ptr::read_unaligned(src.cast::<u128>());
            ptr::write_unaligned(dst.cast::<u128>(), word);
        }
        return;
    }

    let mut copied = 0usize;
    while copied + 16 <= len {
        unsafe {
            let word = ptr::read_unaligned(src.add(copied).cast::<u128>());
            ptr::write_unaligned(dst.add(copied).cast::<u128>(), word);
        }
        copied += 16;
    }
    if copied < len {
        unsafe {
            ptr::copy_nonoverlapping(src.add(copied), dst.add(copied), len - copied);
        }
    }
}

#[inline(always)]
unsafe fn copy_match_wide(src: *const u8, dst: *mut u8, len: usize) {
    let mut copied = 0usize;
    while copied + 8 <= len {
        unsafe {
            let word = ptr::read_unaligned(src.add(copied).cast::<u64>());
            ptr::write_unaligned(dst.add(copied).cast::<u64>(), word);
        }
        copied += 8;
    }
    while copied < len {
        unsafe {
            *dst.add(copied) = *src.add(copied);
        }
        copied += 1;
    }
}

#[inline(always)]
unsafe fn copy_match_short_period(
    src: *const u8,
    dst: *mut u8,
    offset: usize,
    len: usize,
) {
    let mut words = [0u64; 8];
    for (phase, word) in words.iter_mut().enumerate().take(offset) {
        let mut pattern = [0u8; 8];
        for (i, byte) in pattern.iter_mut().enumerate() {
            *byte = unsafe { *src.add((phase + i) % offset) };
        }
        *word = u64::from_ne_bytes(pattern);
    }

    let mut copied = 0usize;
    let mut phase = 0usize;
    let step = 8 % offset;
    while copied + 8 <= len {
        unsafe {
            ptr::write_unaligned(dst.add(copied).cast::<u64>(), words[phase]);
        }
        copied += 8;
        phase += step;
        if phase >= offset {
            phase -= offset;
        }
    }
    while copied < len {
        unsafe {
            *dst.add(copied) = *src.add(phase);
        }
        copied += 1;
        phase += 1;
        if phase == offset {
            phase = 0;
        }
    }
}
