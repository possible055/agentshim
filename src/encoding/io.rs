use std::io::{self, Read};

use encoding_rs::{Decoder, DecoderResult, Encoding, UTF_8, UTF_16BE, UTF_16LE};
use tokio_util::sync::CancellationToken;

use super::decoder::{StrictDecoder, detect_encoding};

const DECODE_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranscodeFailure {
    Cancelled,
    Io,
    Malformed,
    Binary,
}

pub(crate) struct StrictTranscodingReader<'a, R> {
    reader: R,
    decoder: Decoder,
    cancellation: &'a CancellationToken,
    input: Box<[u8]>,
    input_start: usize,
    input_end: usize,
    output: Box<[u8]>,
    output_start: usize,
    output_end: usize,
    eof: bool,
    finished: bool,
    failure: Option<TranscodeFailure>,
}

impl<'a, R> StrictTranscodingReader<'a, R> {
    pub(crate) fn new(
        reader: R,
        encoding: &'static Encoding,
        cancellation: &'a CancellationToken,
        input_bytes: usize,
        output_bytes: usize,
    ) -> Self {
        Self {
            reader,
            decoder: encoding.new_decoder(),
            cancellation,
            input: vec![0; input_bytes.max(4)].into_boxed_slice(),
            input_start: 0,
            input_end: 0,
            output: vec![0; output_bytes.max(4)].into_boxed_slice(),
            output_start: 0,
            output_end: 0,
            eof: false,
            finished: false,
            failure: None,
        }
    }

    pub(crate) fn into_parts(self) -> (R, Option<TranscodeFailure>) {
        (self.reader, self.failure)
    }

    fn fail(&mut self, failure: TranscodeFailure) -> io::Error {
        self.failure = Some(failure);
        io::Error::other("strict transcoding failed")
    }
}

impl<R: Read> Read for StrictTranscodingReader<'_, R> {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        if destination.is_empty() {
            return Ok(0);
        }
        loop {
            if self.output_start < self.output_end {
                let count = destination
                    .len()
                    .min(self.output_end.saturating_sub(self.output_start));
                destination[..count].copy_from_slice(
                    &self.output[self.output_start..self.output_start.saturating_add(count)],
                );
                self.output_start += count;
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
            let (result, read, written) = self.decoder.decode_to_utf8_without_replacement(
                &self.input[self.input_start..self.input_end],
                &mut self.output,
                self.eof,
            );
            self.input_start = self.input_start.saturating_add(read);
            self.output_start = 0;
            self.output_end = written;
            if self.output[..written].contains(&0) {
                return Err(self.fail(TranscodeFailure::Binary));
            }
            match result {
                DecoderResult::InputEmpty if self.eof => self.finished = true,
                DecoderResult::InputEmpty | DecoderResult::OutputFull => {}
                DecoderResult::Malformed(_, _) => {
                    return Err(self.fail(TranscodeFailure::Malformed));
                }
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
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16LE",
            Self::Utf16Be => "UTF-16BE",
            Self::Other(encoding) => encoding.name(),
        }
    }

    pub(super) fn encoding(self) -> &'static Encoding {
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
