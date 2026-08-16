use super::request::ReadError;

const SOURCE_ID_LENGTH: usize = 16;
const OFFSET_SEPARATOR: char = '.';

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
    use super::decode;

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
}
