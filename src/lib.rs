//! Small Snappy encoder and decoder.
//!
//! This supports the Snappy "raw" block format: a varint decompressed length
//! followed by literal and copy tags. It also provides basic Snappy framed
//! stream readers and writers.

use std::fmt;
use std::io;
use std::mem::MaybeUninit;
use std::slice;

mod bytes;
mod decode;
mod encode;
mod select;

const MAX_BLOCK_LEN: u64 = u32::MAX as u64;
const MAX_FRAME_BLOCK_LEN: usize = 1 << 16;

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
    // SAFETY: the decoder copies only bytes from `src`, from `dst` itself and
    // from initialized statics, so `dst` stays initialized.
    let dst = unsafe {
        slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<MaybeUninit<u8>>(), dst.len())
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
    decode::decode_body(&src[header_len..], dst)?;
    // SAFETY: `decode_body` initialized all `len` bytes on success.
    Ok(unsafe { slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<u8>(), len) })
}

/// Clears `dst` and decompresses a raw Snappy block into its spare capacity.
///
/// Unlike `vec![0; len]`, this avoids zero-filling the output before decode.
#[inline]
pub fn decompress_to_vec<'a>(src: &[u8], dst: &'a mut Vec<u8>) -> Result<&'a mut [u8]> {
    let (len, header_len) = read_header(src)?;
    dst.clear();
    dst.try_reserve(len)
        .map_err(|_| Error::Allocation { requested: len })?;
    decode::decode_body(&src[header_len..], &mut dst.spare_capacity_mut()[..len])?;
    // SAFETY: `decode_body` initialized the first `len` bytes of the spare
    // capacity.
    unsafe { dst.set_len(len) };
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
    encode::compress_vec(src)
}

/// Compresses bytes into `dst` as a raw Snappy block.
#[inline]
pub fn compress_into(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    encode::compress_into(src, dst)
}

/// Low-level raw Snappy block APIs.
pub mod raw {
    use std::fmt;

    pub use crate::max_compress_len;
    use crate::{Result, decompress, decompress_into, encode};

    /// Raw Snappy block encoder.
    pub struct Encoder {
        inner: encode::Encoder,
    }

    impl Encoder {
        /// Creates a new raw Snappy block encoder.
        pub fn new() -> Self {
            Self {
                inner: encode::Encoder::new(),
            }
        }

        /// Compresses `input` into `output`.
        #[inline]
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

    use crate::{
        Error, MAX_FRAME_BLOCK_LEN, decompress_into, decompress_len, decompress_to_vec,
        frame,
    };

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

        /// Reads the next chunk. Its data goes straight to `buf` if it fits,
        /// which saves copying it from `dst` afterwards.
        fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<Chunk> {
            let mut header = [0u8; 4];
            if !frame::read_exact_eof(&mut self.inner, &mut header)? {
                return Ok(Chunk::End);
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
                    let expected = frame::read_u32_le(&checksum);
                    if let Some(out) = buf.get_mut(..data_len) {
                        self.inner.read_exact(out)?;
                        frame::check_crc(expected, out)?;
                        return Ok(Chunk::Direct(data_len));
                    }
                    self.dst.resize(data_len, 0);
                    self.inner.read_exact(&mut self.dst)?;
                    frame::check_crc(expected, &self.dst)?;
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

                    let expected = frame::read_u32_le(&checksum);
                    if let Some(out) = buf.get_mut(..decoded_len) {
                        decompress_into(&self.src, out)?;
                        frame::check_crc(expected, out)?;
                        return Ok(Chunk::Direct(decoded_len));
                    }
                    decompress_to_vec(&self.src, &mut self.dst)?;
                    frame::check_crc(expected, &self.dst)?;
                    self.pos = 0;
                    self.end = self.dst.len();
                }
                frame::PADDING_TYPE | 0x80..=0xfd => {
                    frame::skip_exact(&mut self.inner, len)?;
                }
                0x02..=0x7f => {
                    return Err(Error::UnsupportedChunkType { byte: chunk_type }.into());
                }
            }

            Ok(Chunk::Buffered)
        }
    }

    /// What [`FrameDecoder::read_chunk`] did with a chunk.
    enum Chunk {
        /// The stream ended.
        End,
        /// Any data is in `dst[pos..end]`.
        Buffered,
        /// This many bytes went to the caller's buffer.
        Direct(usize),
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

                match self.read_chunk(buf)? {
                    Chunk::End => return Ok(0),
                    Chunk::Buffered | Chunk::Direct(0) => {}
                    Chunk::Direct(len) => return Ok(len),
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
                let take = buf.len().min(available);
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

    /// Checks a chunk's data against the masked CRC from its header.
    pub fn check_crc(expected: u32, data: &[u8]) -> io::Result<()> {
        let got = masked_crc32c(data);
        if expected == got {
            Ok(())
        } else {
            Err(super::Error::Checksum { expected, got }.into())
        }
    }

    pub fn masked_crc32c(src: &[u8]) -> u32 {
        let crc = crc32c(src);
        crc.rotate_right(15).wrapping_add(0xa282_ead8)
    }

    /// CRC-32C (Castagnoli), with the CPU's CRC-32C instructions where it has
    /// them (SSE4.2 on x86-64, the CRC extension on aarch64).
    fn crc32c(src: &[u8]) -> u32 {
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        if std::arch::is_x86_feature_detected!("sse4.2") {
            // SAFETY: the CPU supports SSE4.2.
            return unsafe { crc32c_sse42(src) };
        }
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        if std::arch::is_aarch64_feature_detected!("crc") {
            // SAFETY: the CPU supports the CRC extension.
            return unsafe { crc32c_arm(src) };
        }
        crc32c_tables(src)
    }

    /// The CRC-32C instructions' throughput is a third of their latency, so
    /// long inputs are split into three streams, whose CRCs are combined by
    /// shifting them over the bytes that follow (Mark Adler's method). `$crc8`
    /// and `$crc1` are the 8-byte and 1-byte instructions.
    macro_rules! crc32c_hw {
        ($src:expr, $crc8:path, $crc1:path) => {{
            let mut src: &[u8] = $src;
            let mut crc = !0u32;
            for (block, shift) in [(LONG, &CRC32C_LONG), (SHORT, &CRC32C_SHORT)] {
                while src.len() >= 3 * block {
                    let (a, rest) = src.split_at(block);
                    let (b, rest) = rest.split_at(block);
                    let (c, rest) = rest.split_at(block);
                    let (mut crc1, mut crc2) = (0, 0);
                    for ((a, b), c) in a
                        .chunks_exact(8)
                        .zip(b.chunks_exact(8))
                        .zip(c.chunks_exact(8))
                    {
                        crc = $crc8(crc, u64_le(a));
                        crc1 = $crc8(crc1, u64_le(b));
                        crc2 = $crc8(crc2, u64_le(c));
                    }
                    crc = crc32c_shift(shift, crc) ^ crc1;
                    crc = crc32c_shift(shift, crc) ^ crc2;
                    src = rest;
                }
            }
            let mut chunks = src.chunks_exact(8);
            for chunk in &mut chunks {
                crc = $crc8(crc, u64_le(chunk));
            }
            for &byte in chunks.remainder() {
                crc = $crc1(crc, byte);
            }
            !crc
        }};
    }

    #[cfg(all(target_arch = "x86_64", not(miri)))]
    #[target_feature(enable = "sse4.2")]
    fn crc32c_sse42(src: &[u8]) -> u32 {
        use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
        // The 64-bit instruction takes and returns the CRC in a 64-bit
        // register.
        #[target_feature(enable = "sse4.2")]
        fn crc8(crc: u32, data: u64) -> u32 {
            _mm_crc32_u64(u64::from(crc), data) as u32
        }
        crc32c_hw!(src, crc8, _mm_crc32_u8)
    }

    #[cfg(all(target_arch = "aarch64", not(miri)))]
    #[target_feature(enable = "crc")]
    fn crc32c_arm(src: &[u8]) -> u32 {
        use std::arch::aarch64::{__crc32cb, __crc32cd};
        crc32c_hw!(src, __crc32cd, __crc32cb)
    }

    #[inline(always)]
    fn u64_le(bytes: &[u8]) -> u64 {
        u64::from_le_bytes(bytes.try_into().unwrap())
    }

    /// Lengths of the blocks that `crc32c_hw!` processes three at a time.
    const LONG: usize = 8192;
    const SHORT: usize = 256;

    /// Tables that shift a CRC over `LONG` and `SHORT` zero bytes.
    static CRC32C_LONG: [[u32; 256]; 4] = crc32c_zeros(LONG);
    static CRC32C_SHORT: [[u32; 256]; 4] = crc32c_zeros(SHORT);

    /// Shifts the CRC register `crc` over the zero bytes `zeros` was built
    /// for.
    #[inline(always)]
    fn crc32c_shift(zeros: &[[u32; 256]; 4], crc: u32) -> u32 {
        zeros[0][crc as usize & 0xff]
            ^ zeros[1][(crc >> 8) as usize & 0xff]
            ^ zeros[2][(crc >> 16) as usize & 0xff]
            ^ zeros[3][(crc >> 24) as usize]
    }

    /// The tables for `crc32c_shift` over `len` zero bytes, a power of two:
    /// the operator, a 32x32 matrix over GF(2), applied to each byte of the
    /// CRC.
    const fn crc32c_zeros(len: usize) -> [[u32; 256]; 4] {
        // The operator for one zero bit, then squared up to `len` bytes.
        let mut op = [0u32; 32];
        op[0] = 0x82f6_3b78;
        let mut n = 1;
        while n < 32 {
            op[n] = 1 << (n - 1);
            n += 1;
        }
        let mut bits = 1;
        while bits < 8 * len {
            op = gf2_square(&op);
            bits *= 2;
        }
        let mut tables = [[0u32; 256]; 4];
        let mut byte = 0;
        while byte < 4 {
            let mut n = 0;
            while n < 256 {
                tables[byte][n] = gf2_times(&op, (n as u32) << (8 * byte));
                n += 1;
            }
            byte += 1;
        }
        tables
    }

    const fn gf2_times(mat: &[u32; 32], mut vec: u32) -> u32 {
        let mut sum = 0;
        let mut i = 0;
        while vec != 0 {
            if vec & 1 != 0 {
                sum ^= mat[i];
            }
            vec >>= 1;
            i += 1;
        }
        sum
    }

    const fn gf2_square(mat: &[u32; 32]) -> [u32; 32] {
        let mut square = [0u32; 32];
        let mut n = 0;
        while n < 32 {
            square[n] = gf2_times(mat, mat[n]);
            n += 1;
        }
        square
    }

    /// CRC-32C eight bytes at a time ("slicing by 8"), for CPUs without
    /// CRC-32C instructions.
    fn crc32c_tables(src: &[u8]) -> u32 {
        let table = &CRC32C_TABLES;
        let mut crc = !0u32;
        let mut chunks = src.chunks_exact(8);
        for chunk in &mut chunks {
            let low = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) ^ crc;
            let high = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            crc = table[7][low as usize & 0xff]
                ^ table[6][(low >> 8) as usize & 0xff]
                ^ table[5][(low >> 16) as usize & 0xff]
                ^ table[4][(low >> 24) as usize]
                ^ table[3][high as usize & 0xff]
                ^ table[2][(high >> 8) as usize & 0xff]
                ^ table[1][(high >> 16) as usize & 0xff]
                ^ table[0][(high >> 24) as usize];
        }
        for &byte in chunks.remainder() {
            crc = (crc >> 8) ^ table[0][(crc as u8 ^ byte) as usize];
        }
        !crc
    }

    /// `CRC32C_TABLES[k][b]` is the CRC of byte `b` followed by `k` zero
    /// bytes, without the initial and final inversion.
    static CRC32C_TABLES: [[u32; 256]; 8] = {
        let mut tables = [[0u32; 256]; 8];
        let mut i = 0;
        while i < 256 {
            let mut crc = i as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = (crc >> 1) ^ (0x82f6_3b78 & 0u32.wrapping_sub(crc & 1));
                bit += 1;
            }
            tables[0][i] = crc;
            i += 1;
        }
        let mut k = 1;
        while k < 8 {
            let mut i = 0;
            while i < 256 {
                let prev = tables[k - 1][i];
                tables[k][i] = (prev >> 8) ^ tables[0][(prev & 0xff) as usize];
                i += 1;
            }
            k += 1;
        }
        tables
    };

    #[cfg(test)]
    #[test]
    fn crc32c_check_value() {
        // The standard check value, and lengths around the 8-byte step.
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        let data: Vec<u8> = (0..100u32).map(|i| (i * 37 + 11) as u8).collect();
        for len in 0..data.len() {
            let mut bitwise = !0u32;
            for &byte in &data[..len] {
                bitwise ^= u32::from(byte);
                for _ in 0..8 {
                    bitwise =
                        (bitwise >> 1) ^ (0x82f6_3b78 & 0u32.wrapping_sub(bitwise & 1));
                }
            }
            assert_eq!(crc32c(&data[..len]), !bitwise, "len {len}");
            assert_eq!(crc32c_tables(&data[..len]), !bitwise, "len {len}");
        }
        // Lengths around the three-way blocks of the CRC instructions.
        // The bytes must not repeat with a period that divides the block
        // lengths, or the three streams' CRCs would be equal.
        let mut x = 0x9e37_79b9u32;
        let data: Vec<u8> = (0..3 * LONG + 1000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        for len in (0..data.len()).step_by(97).chain([
            3 * SHORT,
            3 * LONG,
            3 * LONG + 3 * SHORT + 9,
        ]) {
            assert_eq!(
                crc32c(&data[..len]),
                crc32c_tables(&data[..len]),
                "len {len}"
            );
        }
    }
}

/// Parses the varint decoded length at the start of a raw block, returning
/// it and the length of the varint.
#[inline(always)]
fn read_header(src: &[u8]) -> Result<(usize, usize)> {
    let Some(&first) = src.first() else {
        return Err(Error::Empty);
    };
    if first < 0x80 {
        return Ok((usize::from(first), 1));
    }
    let mut len = 0u64;
    for (i, &byte) in src.iter().take(5).enumerate() {
        len |= u64::from(byte & 0x7f) << (7 * i);
        if byte < 0x80 {
            if len > MAX_BLOCK_LEN {
                return Err(Error::TooBig {
                    given: len,
                    max: MAX_BLOCK_LEN,
                });
            }
            return Ok((len as usize, i + 1));
        }
    }
    Err(Error::Header)
}
