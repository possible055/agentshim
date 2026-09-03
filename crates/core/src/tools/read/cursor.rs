use super::request::ReadError;

const SOURCE_ID_LENGTH: usize = 16;
const OFFSET_SEPARATOR: char = '.';
const OFFICE_SEPARATOR: char = ':';

/// A PDF continuation, carrying the source version it belongs to and, when the previous
/// response stopped inside one page, where to resume in that page's Markdown.
///
/// Binding the two together is the point: as separate arguments the caller had to keep
/// "an offset must travel with its source id" true by hand, and a mismatch was only
/// caught by a validation rule written in prose. Here the offset cannot exist without
/// the source id, so the rule is structural.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PdfCursor<'a> {
    pub source_id: &'a str,
    pub text_offset: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OfficeCursor<'a> {
    pub source_id: &'a str,
    pub format: agentshim_office_read::OfficeFormat,
    pub unit_index: usize,
    pub offset: usize,
}

pub fn encode_office(
    source_id: &str,
    format: agentshim_office_read::OfficeFormat,
    unit_index: usize,
    offset: usize,
) -> String {
    format!(
        "2{}{}{}{}{}{}{}{}{}",
        OFFICE_SEPARATOR,
        office_format_code(format),
        OFFICE_SEPARATOR,
        source_id,
        OFFICE_SEPARATOR,
        unit_index,
        OFFICE_SEPARATOR,
        offset,
        OFFICE_SEPARATOR
    )
}

pub fn decode_office(cursor: &str) -> Result<OfficeCursor<'_>, ReadError> {
    let mut parts = cursor.split(OFFICE_SEPARATOR);
    let version = parts.next();
    let format = parts.next().and_then(parse_office_format);
    let source_id = parts.next();
    let unit_index = parts.next().and_then(|value| value.parse::<usize>().ok());
    let offset = parts.next().and_then(|value| value.parse::<usize>().ok());
    let trailing = parts.next();
    if version != Some("2")
        || format.is_none()
        || source_id.is_none_or(|value| !valid_source_id(value))
        || unit_index.is_none()
        || offset.is_none()
        || trailing != Some("")
        || parts.next().is_some()
    {
        return Err(ReadError::Validation(
            "office_cursor must be a value copied from a previous response".to_owned(),
        ));
    }
    Ok(OfficeCursor {
        source_id: source_id.unwrap_or_default(),
        format: format.unwrap_or(agentshim_office_read::OfficeFormat::Docx),
        unit_index: unit_index.unwrap_or_default(),
        offset: offset.unwrap_or_default(),
    })
}

fn valid_source_id(source_id: &str) -> bool {
    source_id.len() == SOURCE_ID_LENGTH
        && source_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

const fn office_format_code(format: agentshim_office_read::OfficeFormat) -> &'static str {
    match format {
        agentshim_office_read::OfficeFormat::Docx => "docx",
        agentshim_office_read::OfficeFormat::Xlsx => "xlsx",
        agentshim_office_read::OfficeFormat::Pptx => "pptx",
        agentshim_office_read::OfficeFormat::Doc => "doc",
        agentshim_office_read::OfficeFormat::Xls => "xls",
        agentshim_office_read::OfficeFormat::Ppt => "ppt",
    }
}

fn parse_office_format(value: &str) -> Option<agentshim_office_read::OfficeFormat> {
    match value {
        "docx" => Some(agentshim_office_read::OfficeFormat::Docx),
        "xlsx" => Some(agentshim_office_read::OfficeFormat::Xlsx),
        "pptx" => Some(agentshim_office_read::OfficeFormat::Pptx),
        "doc" => Some(agentshim_office_read::OfficeFormat::Doc),
        "xls" => Some(agentshim_office_read::OfficeFormat::Xls),
        "ppt" => Some(agentshim_office_read::OfficeFormat::Ppt),
        _ => None,
    }
}

pub fn encode(source_id: &str, text_offset: Option<usize>) -> String {
    match text_offset {
        Some(offset) => format!("{source_id}{OFFSET_SEPARATOR}{offset}"),
        None => source_id.to_owned(),
    }
}

/// Rejects anything the server did not print. The token is opaque to the caller, so a
/// hand-built value is a mistake worth reporting rather than a shape to accommodate.
pub fn decode(cursor: &str) -> Result<PdfCursor<'_>, ReadError> {
    let (source_id, text_offset) = match cursor.split_once(OFFSET_SEPARATOR) {
        Some((source_id, offset)) => {
            let offset = offset.parse::<usize>().map_err(|_| malformed())?;
            (source_id, Some(offset))
        }
        None => (cursor, None),
    };
    if source_id.len() != SOURCE_ID_LENGTH
        || !source_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(malformed());
    }
    Ok(PdfCursor {
        source_id,
        text_offset,
    })
}

fn malformed() -> ReadError {
    ReadError::Validation("pdf_cursor must be a value copied from a previous response".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{decode, decode_office, encode_office};

    /// The round trip itself is covered on real output by the PDF continuation tests,
    /// which decode the cursor the renderer printed. What has no other home is the
    /// rejection boundary: these shapes never come from a response.
    #[test]
    fn hand_built_cursors_are_rejected() {
        for candidate in [
            "",
            "short",
            "0123456789ABCDEF",
            "0123456789abcdeff",
            "0123456789abcde",
            "0123456789abcdef.",
            "0123456789abcdef.-1",
            "0123456789abcdef.x",
            "0123456789abcdef.1.2",
            "not-a-fingerprint",
        ] {
            assert!(decode(candidate).is_err(), "{candidate} must be rejected");
        }
    }

    #[test]
    fn office_cursor_round_trips_and_rejects_other_shapes() {
        let encoded = encode_office(
            "0123456789abcdef",
            agentshim_office_read::OfficeFormat::Xlsx,
            7,
            42,
        );
        let decoded = decode_office(&encoded).expect("round trip");
        assert_eq!(decoded.source_id, "0123456789abcdef");
        assert_eq!(decoded.format, agentshim_office_read::OfficeFormat::Xlsx);
        assert_eq!(decoded.unit_index, 7);
        assert_eq!(decoded.offset, 42);
        for invalid in [
            "",
            "2:xlsx:short:0:0:",
            "1:xlsx:0123456789abcdef:0:0:",
            "2:unknown:0123456789abcdef:0:0:",
            "2:xlsx:0123456789abcdef:-1:0:",
            "2:xlsx:0123456789abcdef:0:-1:",
            "2:xlsx:0123456789abcdef:0:0:extra",
        ] {
            assert!(decode_office(invalid).is_err(), "{invalid}");
        }
    }
}
