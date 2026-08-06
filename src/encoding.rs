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

fn detect_encoding(
    prefix: &[u8],
    explicit: Option<&str>,
) -> Result<(SourceEncoding, usize), DecodeError> {
    if prefix.starts_with(&[0x00, 0x00, 0xFE, 0xFF])
        || prefix.starts_with(&[0xFF, 0xFE, 0x00, 0x00])
    {
        return Err(DecodeError::Utf32);
    }
    if prefix.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok((SourceEncoding::Utf8, 3));
    }
    if prefix.starts_with(&[0xFF, 0xFE]) {
        return Ok((SourceEncoding::Utf16Le, 2));
    }
    if prefix.starts_with(&[0xFE, 0xFF]) {
        return Ok((SourceEncoding::Utf16Be, 2));
    }
    let Some(label) = explicit else {
        return Ok((SourceEncoding::Utf8, 0));
    };
    let label = label.trim_matches(char::is_whitespace);
    let encoding = Encoding::for_label_no_replacement(label.as_bytes())
        .ok_or_else(|| DecodeError::UnknownEncoding(label.to_owned()))?;
    let source = if encoding == UTF_8 {
        SourceEncoding::Utf8
    } else if encoding == UTF_16LE {
        SourceEncoding::Utf16Le
    } else if encoding == UTF_16BE {
        SourceEncoding::Utf16Be
    } else {
        SourceEncoding::Other(encoding)
    };
    Ok((source, 0))
}

enum StrictDecoder {
    Utf8 { carry: Vec<u8> },
    Other(Decoder),
}

impl StrictDecoder {
    fn new(source: SourceEncoding) -> Self {
        if source == SourceEncoding::Utf8 {
            Self::Utf8 {
                carry: Vec::with_capacity(3),
            }
        } else {
            Self::Other(source.encoding().new_decoder_without_bom_handling())
        }
    }

    fn decode<'input>(
        &mut self,
        input: &'input [u8],
        is_last: bool,
    ) -> Result<Option<Cow<'input, str>>, DecodeError> {
        let decoded = match self {
            Self::Utf8 { carry } => decode_utf8(carry, input, is_last)?,
            Self::Other(decoder) => Cow::Owned(decode_other(decoder, input, is_last)?),
        };
        Ok((!decoded.is_empty()).then_some(decoded))
    }
}

fn decode_utf8<'input>(
    carry: &mut Vec<u8>,
    input: &'input [u8],
    is_last: bool,
) -> Result<Cow<'input, str>, DecodeError> {
    if carry.is_empty() {
        match std::str::from_utf8(input) {
            Ok(text) => return Ok(Cow::Borrowed(text)),
            Err(error) if error.error_len().is_none() && !is_last => {
                let valid_up_to = error.valid_up_to();
                carry.extend_from_slice(&input[valid_up_to..]);
                return std::str::from_utf8(&input[..valid_up_to])
                    .map(Cow::Borrowed)
                    .map_err(|_| DecodeError::Malformed("UTF-8"));
            }
            Err(_) => return Err(DecodeError::Malformed("UTF-8")),
        }
    }
    let mut bytes = std::mem::take(carry);
    bytes.extend_from_slice(input);
    match std::str::from_utf8(&bytes) {
        Ok(_) => String::from_utf8(bytes)
            .map(Cow::Owned)
            .map_err(|_| DecodeError::Malformed("UTF-8")),
        Err(error) if error.error_len().is_none() && !is_last => {
            let valid_up_to = error.valid_up_to();
            carry.extend_from_slice(&bytes[valid_up_to..]);
            bytes.truncate(valid_up_to);
            String::from_utf8(bytes)
                .map(Cow::Owned)
                .map_err(|_| DecodeError::Malformed("UTF-8"))
        }
        Err(_) => Err(DecodeError::Malformed("UTF-8")),
    }
}

fn decode_other(decoder: &mut Decoder, input: &[u8], is_last: bool) -> Result<String, DecodeError> {
    let mut consumed = 0_usize;
    let mut output = String::new();
    loop {
        let remaining = &input[consumed..];
        let capacity = decoder
            .max_utf8_buffer_length_without_replacement(remaining.len())
            .ok_or(DecodeError::TooLarge)?
            .max(4);
        let mut buffer = String::with_capacity(capacity);
        let (result, read) =
            decoder.decode_to_string_without_replacement(remaining, &mut buffer, is_last);
        consumed = consumed.checked_add(read).ok_or(DecodeError::TooLarge)?;
        output.push_str(&buffer);
        match result {
            DecoderResult::InputEmpty => return Ok(output),
            DecoderResult::OutputFull => {}
            DecoderResult::Malformed(_, _) => {
                return Err(DecodeError::Malformed(decoder.encoding().name()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};

    use tokio_util::sync::CancellationToken;

    use super::{DecodeError, SourceEncoding, decode_to_string};

    struct ByteReader {
        bytes: Vec<u8>,
        offset: usize,
    }

    impl Read for ByteReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let Some(byte) = self.bytes.get(self.offset) else {
                return Ok(0);
            };
            output[0] = *byte;
            self.offset += 1;
            Ok(1)
        }
    }

    fn utf16(text: &str, little_endian: bool) -> Vec<u8> {
        let mut bytes = if little_endian {
            vec![0xFF, 0xFE]
        } else {
            vec![0xFE, 0xFF]
        };
        for unit in text.encode_utf16() {
            bytes.extend(if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        bytes
    }

    #[test]
    fn strict_streaming_handles_split_utf8_and_utf16() {
        let cancellation = CancellationToken::new();
        let utf8 = ByteReader {
            bytes: "alpha界".as_bytes().to_vec(),
            offset: 0,
        };
        let (text, summary) =
            decode_to_string(utf8, None, 100, &cancellation).expect("decode UTF-8");
        assert_eq!(text, "alpha界");
        assert_eq!(summary.source_encoding, SourceEncoding::Utf8);

        for little_endian in [true, false] {
            let reader = ByteReader {
                bytes: utf16("alpha\r\nbeta", little_endian),
                offset: 0,
            };
            let (text, _) = decode_to_string(reader, Some("windows-1252"), 100, &cancellation)
                .expect("BOM takes priority");
            assert_eq!(text, "alpha\r\nbeta");
        }
    }

    #[test]
    fn malformed_binary_unknown_and_oversized_content_fail() {
        let cancellation = CancellationToken::new();
        assert!(matches!(
            decode_to_string(&[0xFF_u8][..], None, 100, &cancellation),
            Err(DecodeError::Malformed("UTF-8"))
        ));
        assert!(matches!(
            decode_to_string(&b"a\0b"[..], None, 100, &cancellation),
            Err(DecodeError::Binary)
        ));
        assert!(matches!(
            decode_to_string(&b"text"[..], Some("not-an-encoding"), 100, &cancellation),
            Err(DecodeError::UnknownEncoding(_))
        ));
        assert!(matches!(
            decode_to_string(&b"too large"[..], None, 3, &cancellation),
            Err(DecodeError::TooLarge)
        ));
    }

    #[test]
    fn cancellation_stops_before_reading() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            decode_to_string(&b"text"[..], None, 100, &cancellation),
            Err(DecodeError::Cancelled)
        ));
    }
}
