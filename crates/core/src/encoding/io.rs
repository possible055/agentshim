use std::io::{self, Read};

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};
use tokio_util::sync::CancellationToken;

use super::decoder::{StrictDecoder, detect_encoding};

const DECODE_CHUNK_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscodeFailure {
    Cancelled,
    Io,
    Malformed,
    Binary,
}

/// Thin `Read` adapter over [`StrictDecoder`]: one strict decode stack, one BOM
/// table, one NUL binary detection for both the chunked and the streaming paths.
pub struct StrictTranscodingReader<'a, R> {
    reader: R,
    decoder: StrictDecoder,
    encoding: &'static Encoding,
    cancellation: &'a CancellationToken,
    input: Box<[u8]>,
    input_start: usize,
    input_end: usize,
    decoded: String,
    decoded_start: usize,
    bom_checked: bool,
    eof: bool,
    finished: bool,
    failure: Option<TranscodeFailure>,
}

impl<'a, R> StrictTranscodingReader<'a, R> {
    pub fn new(
        reader: R,
        encoding: &'static Encoding,
        cancellation: &'a CancellationToken,
        input_bytes: usize,
    ) -> Self {
        Self {
            reader,
            decoder: StrictDecoder::new(SourceEncoding::for_encoding(encoding)),
            encoding,
            cancellation,
            input: vec![0; input_bytes.max(4)].into_boxed_slice(),
            input_start: 0,
            input_end: 0,
            decoded: String::new(),
            decoded_start: 0,
            bom_checked: false,
            eof: false,
            finished: false,
            failure: None,
        }
    }

    pub fn into_parts(self) -> (R, Option<TranscodeFailure>) {
        (self.reader, self.failure)
    }

    fn fail(&mut self, failure: TranscodeFailure) -> io::Error {
        self.failure = Some(failure);
        io::Error::other("strict transcoding failed")
    }
}

/// Mirror the WHATWG decode algorithm's BOM sniffing that `encoding_rs`' BOM-sniffing
/// `new_decoder` performs: a leading UTF BOM overrides the requested encoding
/// entirely, and legacy encodings keep their requested decoder.
fn sniff_bom(requested: &'static Encoding, chunk: &[u8]) -> (&'static Encoding, usize) {
    if chunk.starts_with(&[0xEF, 0xBB, 0xBF]) {
        (UTF_8, 3)
    } else if chunk.starts_with(&[0xFE, 0xFF]) {
        (UTF_16BE, 2)
    } else if chunk.starts_with(&[0xFF, 0xFE]) {
        (UTF_16LE, 2)
    } else {
        (requested, 0)
    }
}

impl<R: Read> Read for StrictTranscodingReader<'_, R> {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        if destination.is_empty() {
            return Ok(0);
        }
        loop {
            if self.decoded_start < self.decoded.len() {
                let count = destination
                    .len()
                    .min(self.decoded.len() - self.decoded_start);
                destination[..count].copy_from_slice(
                    &self.decoded.as_bytes()
                        [self.decoded_start..self.decoded_start.saturating_add(count)],
                );
                self.decoded_start += count;
                return Ok(count);
            }
            if self.finished {
                return Ok(0);
            }
            if self.cancellation.is_cancelled() {
                return Err(self.fail(TranscodeFailure::Cancelled));
            }
            if self.input_start == self.input_end && !self.eof {
                match self.reader.read(&mut self.input) {
                    Ok(0) => self.eof = true,
                    Ok(count) => {
                        self.input_start = 0;
                        self.input_end = count;
                    }
                    Err(error) => {
                        self.failure = Some(TranscodeFailure::Io);
                        return Err(error);
                    }
                }
            }
            if !self.bom_checked {
                self.bom_checked = true;
                let chunk = &self.input[..self.input_end];
                let (encoding, bom_len) = sniff_bom(self.encoding, chunk);
                if encoding != self.encoding {
                    self.encoding = encoding;
                    self.decoder = StrictDecoder::new(SourceEncoding::for_encoding(encoding));
                }
                self.input_start = bom_len;
            }
            let chunk = &self.input[self.input_start..self.input_end];
            let decoded = match self.decoder.decode(chunk, self.eof) {
                Ok(decoded) => decoded,
                Err(super::DecodeError::Io(io_error)) => {
                    self.failure = Some(TranscodeFailure::Io);
                    return Err(io_error);
                }
                Err(super::DecodeError::Cancelled) => {
                    return Err(self.fail(TranscodeFailure::Cancelled));
                }
                Err(_) => {
                    return Err(self.fail(TranscodeFailure::Malformed));
                }
            };
            self.input_start = self.input_end;
            if let Some(decoded) = decoded {
                if decoded.contains('\0') {
                    return Err(self.fail(TranscodeFailure::Binary));
                }
                self.decoded = decoded.into_owned();
                self.decoded_start = 0;
            }
            if self.eof {
                self.finished = true;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Other(&'static Encoding),
}

impl SourceEncoding {
    /// The canonical source encoding for an `encoding_rs` encoding, as the
    /// BOM-aware detection path would classify it.
    #[must_use]
    pub fn for_encoding(encoding: &'static Encoding) -> Self {
        if encoding == UTF_8 {
            Self::Utf8
        } else if encoding == UTF_16LE {
            Self::Utf16Le
        } else if encoding == UTF_16BE {
            Self::Utf16Be
        } else {
            Self::Other(encoding)
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16LE",
            Self::Utf16Be => "UTF-16BE",
            Self::Other(encoding) => encoding.name(),
        }
    }

    pub fn encoding(self) -> &'static Encoding {
        match self {
            Self::Utf8 => UTF_8,
            Self::Utf16Le => UTF_16LE,
            Self::Utf16Be => UTF_16BE,
            Self::Other(encoding) => encoding,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeSummary {
    pub source_encoding: SourceEncoding,
    pub decoded_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeControl {
    Continue,
    Stop,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("unsupported UTF-32 encoding")]
    Utf32,
    #[error("unknown or replacement-only encoding label: {0}")]
    UnknownEncoding(String),
    #[error(
        "cannot reliably detect text encoding; supported automatic legacy encodings are Big5 and GBK"
    )]
    UndetectedEncoding,
    #[error("input is not valid {0}")]
    Malformed(&'static str),
    #[error("decoded content contains NUL and is treated as binary")]
    Binary,
    #[error("decoded content exceeds the configured memory limit")]
    TooLarge,
    #[error("decode cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Strictly decode one byte stream and deliver bounded UTF-8 chunks to `sink`.
///
/// BOM detection takes precedence over `explicit_encoding`; without either, input
/// must be valid UTF-8. No malformed sequence is replaced.
///
/// # Errors
///
/// Returns a decode error for I/O, cancellation, malformed input, binary NUL,
/// unsupported encoding, or decoded output above `max_decoded_bytes`.
pub fn decode_stream<R, F>(
    mut reader: R,
    explicit_encoding: Option<&str>,
    max_decoded_bytes: usize,
    cancellation: &CancellationToken,
    mut sink: F,
) -> Result<DecodeSummary, DecodeError>
where
    R: Read,
    F: FnMut(&str) -> Result<DecodeControl, DecodeError>,
{
    let first = read_prefix(&mut reader, cancellation)?;
    let first_len = first.len();
    let (source_encoding, bom_len) = detect_encoding(&first, explicit_encoding)?;
    let mut decoder = StrictDecoder::new(source_encoding);
    let mut decoded_bytes = 0_usize;

    let mut stopped = false;
    if let Some(decoded) = decoder.decode(&first[bom_len..], first_len == 0)? {
        stopped = deliver(
            &decoded,
            &mut decoded_bytes,
            max_decoded_bytes,
            cancellation,
            &mut sink,
        )? == DecodeControl::Stop;
    }

    if first_len != 0 && !stopped {
        let mut input = vec![0_u8; DECODE_CHUNK_BYTES];
        loop {
            let count = read_chunk(&mut reader, &mut input, cancellation)?;
            let is_last = count == 0;
            if let Some(decoded) = decoder.decode(&input[..count], is_last)? {
                if deliver(
                    &decoded,
                    &mut decoded_bytes,
                    max_decoded_bytes,
                    cancellation,
                    &mut sink,
                )? == DecodeControl::Stop
                {
                    break;
                }
            }
            if is_last {
                break;
            }
        }
    }

    Ok(DecodeSummary {
        source_encoding,
        decoded_bytes,
    })
}

/// Decode a bounded stream into one string.
///
/// # Errors
///
/// Returns the same errors as [`decode_stream`].
#[cfg(test)]
pub fn decode_to_string<R: Read>(
    reader: R,
    explicit_encoding: Option<&str>,
    max_decoded_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<(String, DecodeSummary), DecodeError> {
    let mut text = String::new();
    let summary = decode_stream(
        reader,
        explicit_encoding,
        max_decoded_bytes,
        cancellation,
        |chunk| {
            text.push_str(chunk);
            Ok(DecodeControl::Continue)
        },
    )?;
    Ok((text, summary))
}

fn read_chunk<R: Read>(
    reader: &mut R,
    output: &mut [u8],
    cancellation: &CancellationToken,
) -> Result<usize, DecodeError> {
    if cancellation.is_cancelled() {
        return Err(DecodeError::Cancelled);
    }
    reader.read(output).map_err(DecodeError::Io)
}

fn read_prefix<R: Read>(
    reader: &mut R,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, DecodeError> {
    let mut prefix = Vec::with_capacity(4);
    while prefix.len() < 4 {
        if cancellation.is_cancelled() {
            return Err(DecodeError::Cancelled);
        }
        let mut byte = [0_u8; 1];
        if reader.read(&mut byte)? == 0 {
            break;
        }
        prefix.push(byte[0]);
    }
    Ok(prefix)
}

fn deliver<F>(
    decoded: &str,
    total: &mut usize,
    maximum: usize,
    cancellation: &CancellationToken,
    sink: &mut F,
) -> Result<DecodeControl, DecodeError>
where
    F: FnMut(&str) -> Result<DecodeControl, DecodeError>,
{
    if cancellation.is_cancelled() {
        return Err(DecodeError::Cancelled);
    }
    if decoded.contains('\0') {
        return Err(DecodeError::Binary);
    }
    *total = total
        .checked_add(decoded.len())
        .ok_or(DecodeError::TooLarge)?;
    if *total > maximum {
        return Err(DecodeError::TooLarge);
    }
    if !decoded.is_empty() {
        return sink(decoded);
    }
    Ok(DecodeControl::Continue)
}
