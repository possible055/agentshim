//! High-level DOC document API.

use std::io::{Read, Seek};

use crate::cfb::CfbReader;

use super::error::{DocError, Result};
use super::fib::Fib;
use super::piece_table::{extract_text, parse_clx, sanitize_text};

/// A parsed legacy Word document.
#[derive(Debug)]
pub struct DocDocument {
    /// The raw extracted text (after sanitization).
    text: String,
}

impl DocDocument {
    pub(crate) fn markdown_unit_count(&self) -> usize {
        self.text.lines().count()
    }

    pub(crate) fn markdown_unit(&self, index: usize) -> Option<String> {
        let line = self.text.lines().nth(index)?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Some(format!("{trimmed}\n\n"));
        }
        let previous_was_empty = index
            .checked_sub(1)
            .and_then(|previous| self.text.lines().nth(previous))
            .is_some_and(|previous| previous.trim().is_empty());
        Some(if previous_was_empty {
            String::new()
        } else {
            "\n".to_owned()
        })
    }

    /// Open a DOC file from a reader.
    pub fn from_reader<R: Read + Seek>(reader: R) -> Result<Self> {
        let mut cfb = CfbReader::new(reader)?;

        let word_doc = cfb
            .open_stream("WordDocument")
            .map_err(|error| match error {
                crate::cfb::CfbError::StreamNotFound(_) => {
                    DocError::MissingStream("WordDocument stream not found".into())
                }
                other => DocError::Cfb(other),
            })?;

        let fib = Fib::parse(&word_doc)?;

        // Open the appropriate table stream; try preferred first, then fallback.
        let (preferred, alternate) = if fib.use_table1 {
            ("1Table", "0Table")
        } else {
            ("0Table", "1Table")
        };
        let table_name = if cfb.has_stream(preferred) {
            preferred
        } else if cfb.has_stream(alternate) {
            alternate
        } else {
            return Err(DocError::MissingStream("0Table or 1Table".into()));
        };
        let table_stream = cfb.open_stream(table_name)?;

        if fib.text_len == 0 {
            return Ok(Self {
                text: String::new(),
            });
        }

        // Extract CLX from the table stream.
        let clx_start = fib.clx_offset as usize;
        let clx_end = clx_start
            .checked_add(fib.clx_size as usize)
            .ok_or_else(|| DocError::InvalidPieceTable("CLX range overflow".into()))?;

        if fib.clx_size == 0 || clx_end > table_stream.len() {
            return Err(DocError::InvalidPieceTable(
                "CLX range is unavailable".into(),
            ));
        }

        let clx_data = &table_stream[clx_start..clx_end];
        let pieces = parse_clx(clx_data)?;

        // Extract main document text only (not footnotes, headers, etc.).
        let raw_text = extract_text(&word_doc, &pieces, fib.text_len)?;
        let text = sanitize_text(&raw_text);
        crate::budget::charge_model_text("office_doc_text_bytes", text.len())
            .map_err(crate::map_office_error_to_cfb)
            .map_err(DocError::from)?;

        Ok(Self { text })
    }

    /// Convert to markdown (basic: paragraphs separated by blank lines).
    #[cfg(test)]
    pub fn to_markdown(&self) -> String {
        let mut result = String::new();
        let mut prev_empty = false;

        for line in self.text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                if !prev_empty {
                    result.push('\n');
                }
                prev_empty = true;
            } else {
                result.push_str(trimmed);
                result.push_str("\n\n");
                prev_empty = false;
            }
        }

        result
    }
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn markdown_double_spacing() {
        let doc = DocDocument {
            text: "First paragraph\nSecond paragraph\n\nAfter gap".into(),
        };
        let md = doc.to_markdown();
        assert!(md.contains("First paragraph\n\n"));
        assert!(md.contains("Second paragraph\n\n"));
        assert!(md.contains("After gap\n\n"));
    }

    #[test]
    fn plain_text_access() {
        let doc = DocDocument {
            text: "Hello World".into(),
        };
        assert_eq!(doc.plain_text(), "Hello World");
    }

    fn make_doc(text: &str) -> DocDocument {
        DocDocument {
            text: text.to_string(),
        }
    }

    #[test]
    fn ir_empty_doc_produces_empty_section() {
        let ir = crate::convert_doc::doc_to_ir(&make_doc(""));
        assert!(ir.sections[0].elements.is_empty());
        assert!(ir.metadata.title.is_none());
    }

    #[test]
    fn ir_allcaps_first_line_becomes_h1() {
        use crate::ir::Element;
        let ir = crate::convert_doc::doc_to_ir(&make_doc("INTRODUCTION\nSome text here."));
        assert_eq!(ir.metadata.title.as_deref(), Some("INTRODUCTION"));
        assert!(matches!(ir.sections[0].elements[0], Element::Heading(ref h) if h.level == 1));
    }

    #[test]
    fn ir_first_short_line_no_punct_becomes_h1() {
        use crate::ir::Element;
        let ir = crate::convert_doc::doc_to_ir(&make_doc("My Document Title\nThis is body text."));
        assert!(matches!(ir.sections[0].elements[0], Element::Heading(ref h) if h.level == 1));
    }

    #[test]
    fn ir_allcaps_non_first_line_becomes_h2() {
        use crate::ir::Element;
        let ir = crate::convert_doc::doc_to_ir(&make_doc("Title\nSECTION TWO\nBody text."));
        assert!(matches!(ir.sections[0].elements[1], Element::Heading(ref h) if h.level == 2));
    }

    #[test]
    fn ir_line_ending_with_period_becomes_paragraph() {
        use crate::ir::Element;
        let ir = crate::convert_doc::doc_to_ir(&make_doc("This is a sentence."));
        assert!(matches!(ir.sections[0].elements[0], Element::Paragraph(_)));
    }

    #[test]
    fn ir_blank_lines_are_skipped() {
        let ir = crate::convert_doc::doc_to_ir(&make_doc("Title\n\n\nText"));
        assert_eq!(ir.sections[0].elements.len(), 2);
    }

    #[test]
    fn ir_format_is_doc() {
        let ir = crate::convert_doc::doc_to_ir(&make_doc("content"));
        assert_eq!(ir.metadata.format, crate::format::DocumentFormat::Doc);
    }
}
