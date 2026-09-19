use std::borrow::Cow;

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::{BIG5, Decoder, DecoderResult, Encoding, GBK};

use super::io::{DecodeError, SourceEncoding};

const MIN_AUTODETECT_NON_ASCII_BYTES: usize = 8;

/// Resolve a caller-supplied WHATWG label to the canonical name `encoding_rs` uses.
///
/// Returning `&'static str` lets a label validated once travel through the `Copy` search
/// plan without allocating again for every candidate file.
///
/// # Errors
///
/// Returns [`DecodeError::UnknownEncoding`] for a label `encoding_rs` does not resolve, or
/// resolves only to the replacement encoding.
pub fn normalize_label(label: &str) -> Result<&'static str, DecodeError> {
    let trimmed = label.trim_matches(char::is_whitespace);
    Encoding::for_label_no_replacement(trimmed.as_bytes())
        .map(Encoding::name)
        .ok_or_else(|| DecodeError::UnknownEncoding(trimmed.to_owned()))
}

pub fn detect_legacy_encoding(
    prefix: &[u8],
    explicit: Option<&str>,
    sample_is_complete: bool,
) -> Result<Option<&'static str>, DecodeError> {
    if explicit.is_some() || starts_with_unicode_bom(prefix) || prefix_is_utf8(prefix) {
        return Ok(None);
    }
    if prefix
        .iter()
        .filter(|byte| **byte >= 0x80)
        .take(MIN_AUTODETECT_NON_ASCII_BYTES)
        .count()
        < MIN_AUTODETECT_NON_ASCII_BYTES
    {
        return Err(DecodeError::UndetectedEncoding);
    }

    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    detector.feed(prefix, sample_is_complete);
    let encoding = detector.guess(None, Utf8Detection::Allow);
    if encoding == BIG5 || encoding == GBK {
        Ok(Some(encoding.name()))
    } else {
        Err(DecodeError::UndetectedEncoding)
    }
}

/// The one BOM table for both the streaming and the chunked decode paths. UTF-32
/// entries must sort before the UTF-16 entries they extend.
#[derive(Clone, Copy)]
pub(crate) enum Bom {
    Utf32,
    Encoding(SourceEncoding, usize),
}

const BOMS: [(&[u8], Bom); 5] = [
    (&[0x00, 0x00, 0xFE, 0xFF], Bom::Utf32),
    (&[0xFF, 0xFE, 0x00, 0x00], Bom::Utf32),
    (&[0xEF, 0xBB, 0xBF], Bom::Encoding(SourceEncoding::Utf8, 3)),
    (&[0xFF, 0xFE], Bom::Encoding(SourceEncoding::Utf16Le, 2)),
    (&[0xFE, 0xFF], Bom::Encoding(SourceEncoding::Utf16Be, 2)),
];

pub(crate) fn detect_bom(prefix: &[u8]) -> Option<Bom> {
    BOMS.iter()
        .find(|(bom, _)| prefix.starts_with(bom))
        .map(|(_, kind)| *kind)
}

fn starts_with_unicode_bom(prefix: &[u8]) -> bool {
    detect_bom(prefix).is_some()
}

fn prefix_is_utf8(prefix: &[u8]) -> bool {
    match std::str::from_utf8(prefix) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

pub fn detect_encoding(
    prefix: &[u8],
    explicit: Option<&str>,
) -> Result<(SourceEncoding, usize), DecodeError> {
    match detect_bom(prefix) {
        Some(Bom::Utf32) => return Err(DecodeError::Utf32),
        Some(Bom::Encoding(source, bom_len)) => return Ok((source, bom_len)),
        None => {}
    }
    let Some(label) = explicit else {
        return Ok((SourceEncoding::Utf8, 0));
    };
    let label = label.trim_matches(char::is_whitespace);
    let encoding = Encoding::for_label_no_replacement(label.as_bytes())
        .ok_or_else(|| DecodeError::UnknownEncoding(label.to_owned()))?;
    Ok((SourceEncoding::for_encoding(encoding), 0))
}

pub enum StrictDecoder {
    Utf8 { carry: Vec<u8> },
    Other(Decoder),
}

impl StrictDecoder {
    pub fn new(source: SourceEncoding) -> Self {
        if source == SourceEncoding::Utf8 {
            Self::Utf8 {
                carry: Vec::with_capacity(3),
            }
        } else {
            Self::Other(source.encoding().new_decoder_without_bom_handling())
        }
    }

    pub fn decode<'input>(
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
        // `bytes` is owned, so the borrowed text must be copied out; `to_owned`
        // reuses the already-validated `&str` instead of re-scanning the buffer.
        Ok(text) => Ok(Cow::Owned(text.to_owned())),
        Err(error) if error.error_len().is_none() && !is_last => {
            let valid_up_to = error.valid_up_to();
            let text = std::str::from_utf8(&bytes[..valid_up_to])
                .map_err(|_| DecodeError::Malformed("UTF-8"))?;
            carry.extend_from_slice(&bytes[valid_up_to..]);
            Ok(Cow::Owned(text.to_owned()))
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
