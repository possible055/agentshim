const MIN_AUTODETECT_NON_ASCII_BYTES: usize = 8;

pub(crate) fn detect_legacy_encoding(
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

fn starts_with_unicode_bom(prefix: &[u8]) -> bool {
    prefix.starts_with(&[0x00, 0x00, 0xFE, 0xFF])
        || prefix.starts_with(&[0xFF, 0xFE, 0x00, 0x00])
        || prefix.starts_with(&[0xEF, 0xBB, 0xBF])
        || prefix.starts_with(&[0xFF, 0xFE])
        || prefix.starts_with(&[0xFE, 0xFF])
}

fn prefix_is_utf8(prefix: &[u8]) -> bool {
    match std::str::from_utf8(prefix) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
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

include!("tests.rs");
