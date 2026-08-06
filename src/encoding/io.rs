use std::{
    borrow::Cow,
    io::{self, Read},
};

use encoding_rs::{Decoder, DecoderResult, Encoding, UTF_8, UTF_16BE, UTF_16LE};
use tokio_util::sync::CancellationToken;

const DECODE_CHUNK_BYTES: usize = 64 * 1024;

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

    fn encoding(self) -> &'static Encoding {
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
