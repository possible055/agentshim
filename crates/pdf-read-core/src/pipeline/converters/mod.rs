//! Output converters for the text extraction pipeline.
//!
//! This module provides the OutputConverter trait and implementations for
//! converting ordered text spans to various output formats.
//!
//! # Available Converters
//!
//! - [`MarkdownOutputConverter`]: Convert to Markdown format
//! - [`HtmlOutputConverter`]: Convert to HTML format
//! - [`PlainTextConverter`]: Convert to plain text
//!
//! # Example
//!
//! ```ignore
//! use pdf_oxide::pipeline::converters::{OutputConverter, MarkdownOutputConverter};
//! use pdf_oxide::pipeline::TextPipelineConfig;
//!
//! let converter = MarkdownOutputConverter::new();
//! let config = TextPipelineConfig::default();
//! let output = converter.convert(&ordered_spans, &config)?;
//! ```

mod markdown;

pub use markdown::MarkdownOutputConverter;

use crate::error::Result;
use crate::layout::TextSpan;
use crate::pipeline::{OrderedTextSpan, StructRole, TextPipelineConfig};
use crate::structure::table_extractor::Table;

/// Bullet glyphs commonly used as list markers in PDFs.
const BULLET_CHARS: &[char] = &[
    '►', '•', '▪', '▸', '‣', '◦', '●', '■', '◆', '○', '□', '❍', '❖', '✓', '✔', '➢', '➤', '\x7f',
];

/// Whether `text` is a lone bullet glyph (the whole span is the marker).
pub(crate) fn is_bullet_span(text: &str) -> bool {
    let t = text.trim();
    let mut chars = t.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if BULLET_CHARS.contains(&c))
}

/// Whether `text` begins with a bullet glyph (inline bullet + body).
pub(crate) fn starts_with_bullet(text: &str) -> bool {
    text.trim_start()
        .chars()
        .next()
        .is_some_and(|c| BULLET_CHARS.contains(&c))
}

/// Parse an ordered-list marker (`1.`, `a)`, `iv.`) at the start of `text`,
/// returning the numeric position when it is numeric. `Some(_)` means "this
/// text starts a numbered/lettered list item".
pub(crate) fn is_ordered_list_marker(text: &str) -> Option<u32> {
    let t = text.trim_start();
    let bytes = t.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut idx = 0;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() && idx < 3 {
        idx += 1;
    }
    let numeric_n = if idx > 0 {
        std::str::from_utf8(&bytes[..idx])
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
    } else {
        None
    };
    // Single ASCII letter / roman-numeral form (a) / b. / iv.).
    if idx == 0 && bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() {
        let mut roman_end = 0;
        while roman_end < bytes.len().min(4)
            && matches!(bytes[roman_end], b'i' | b'v' | b'x' | b'I' | b'V' | b'X')
        {
            roman_end += 1;
        }
        if roman_end >= 1 && bytes.len() > roman_end {
            let punct = bytes[roman_end];
            if matches!(punct, b'.' | b')') && bytes.get(roman_end + 1).copied() == Some(b' ') {
                return Some(1);
            }
        }
        if bytes.len() >= 3 && matches!(bytes[1], b'.' | b')') && bytes[2] == b' ' {
            return Some(1);
        }
        return None;
    }
    if idx > 0 && bytes.len() > idx {
        let punct = bytes[idx];
        if matches!(punct, b'.' | b')') && bytes.get(idx + 1).copied() == Some(b' ') {
            return numeric_n;
        }
    }
    None
}

/// The base (body) font size used to derive heading levels from font ratios.
///
/// Uses the mode of spans ≥9pt (excluding bullet glyphs / subscripts / footnote
/// markers that would skew it down), capped at 12pt. Shared by the markdown and
/// HTML converters so both agree on heading levels for untagged documents.
pub(crate) fn base_heading_font_size(spans: &[&OrderedTextSpan], detect_headings: bool) -> f32 {
    if !detect_headings {
        return 12.0;
    }
    let mut size_counts: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for s in spans {
        let sz = s.span.font_size;
        if sz < 9.0 {
            continue;
        }
        *size_counts.entry((sz * 2.0).round() as u32).or_insert(0) += 1;
    }
    size_counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .map(|(bucket, _)| bucket as f32 / 2.0)
        .unwrap_or(12.0)
        .min(12.0)
}

/// Whether a `/Link` annotation URI is safe to emit as a hyperlink.
///
/// Only well-known navigable schemes are allowed; `javascript:`, `data:`,
/// `vbscript:`, `file:` and any other/unknown scheme are rejected so a
/// malicious PDF cannot inject an active-content link into the HTML/markdown
/// output (XSS). Anchor text is still emitted for rejected URIs — only the
/// link target is dropped.
pub(crate) fn is_safe_link_uri(uri: &str) -> bool {
    let lower = uri.trim_start().to_ascii_lowercase();
    [
        "http://", "https://", "mailto:", "tel:", "ftp://", "ftps://",
    ]
    .iter()
    .any(|scheme| lower.starts_with(scheme))
}

/// Whether a structure-tree role marks the span as part of a list item.
pub(crate) fn is_list_item_role(role: Option<StructRole>) -> bool {
    matches!(
        role,
        Some(StructRole::ListItem | StructRole::ListItemLabel | StructRole::ListItemBody)
    )
}

/// Trait for converting ordered text spans to output formats.
///
/// Implementations transform a sequence of ordered text spans into a specific
/// output format (Markdown, HTML, plain text, etc.).
///
/// This trait provides a clean abstraction layer between the PDF extraction
/// pipeline and the output generation, following the PDF spec compliance goal
/// of separating PDF representation from output formatting.
pub trait OutputConverter: Send + Sync {
    /// Convert ordered spans to the target format.
    ///
    /// # Arguments
    ///
    /// * `spans` - Ordered text spans from the reading order strategy
    /// * `config` - Pipeline configuration affecting output formatting
    ///
    /// # Returns
    ///
    /// The formatted output string.
    fn convert(&self, spans: &[OrderedTextSpan], config: &TextPipelineConfig) -> Result<String>;

    /// Convert ordered spans to the target format, with pre-detected tables.
    ///
    /// Table regions are rendered using the converter's table formatting
    /// (markdown tables, HTML tables, or tab-delimited text). Spans that
    /// fall within table bounding boxes are excluded from normal rendering.
    ///
    /// Default implementation ignores tables and falls back to `convert()`.
    fn convert_with_tables(
        &self,
        spans: &[OrderedTextSpan],
        tables: &[Table],
        config: &TextPipelineConfig,
    ) -> Result<String> {
        let _ = tables;
        self.convert(spans, config)
    }

    /// Return the name of this converter for debugging.
    fn name(&self) -> &'static str;

    /// Return the MIME type for the output format.
    fn mime_type(&self) -> &'static str;
}

/// Returns `true` if `c` is a CJK character (Chinese, Japanese, or Korean).
fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{309F}' |   // Hiragana
        '\u{30A0}'..='\u{30FF}' |   // Katakana
        '\u{4E00}'..='\u{9FFF}' |   // CJK Unified Ideographs
        '\u{AC00}'..='\u{D7AF}' |   // Hangul
        '\u{3400}'..='\u{4DBF}' |   // CJK Extension A
        '\u{20000}'..='\u{2A6DF}'   // CJK Extension B
    )
}

/// Returns `true` if `c` is a fullwidth or mathematical operator that is
/// commonly embedded inside CJK text without surrounding spaces.
///
/// These characters have slightly wider advances than typical ASCII characters,
/// which can trigger the gap heuristic and insert a spurious space when they
/// appear between CJK glyphs (e.g. `25000≤Q＜40000`).
fn is_fullwidth_or_math_op(c: char) -> bool {
    matches!(c,
        '\u{FF0B}' |                // ＋
        '\u{FF0D}' |                // －
        '\u{FF1A}' |                // ：
        '\u{FF1B}' |                // ；
        '\u{FF1C}'..='\u{FF1E}' |  // ＜ ＝ ＞
        '\u{2260}' |               // ≠
        '\u{2248}' |               // ≈
        '\u{2264}'..='\u{2265}' |  // ≤ ≥
        '\u{00B5}' |               // µ
        '\u{03BC}' |               // μ
        '\u{00B1}' |               // ±
        '\u{00D7}' |               // ×
        '\u{00F7}'                 // ÷
    )
}

/// Check whether two horizontally adjacent spans have a visible gap between them.
///
/// Returns `true` when the horizontal distance between the end of `prev` and
/// the start of `current` exceeds a small fraction of the font size but is not
/// unreasonably large (which would indicate a column break rather than a word
/// gap).
///
/// CJK scripts do not use spaces between words.  When one side of the boundary
/// is a CJK character and the other side is CJK or a fullwidth/math operator
/// (e.g. `≤`, `＜`, `μ`), no space is inserted even if the geometric gap
/// exceeds the threshold.  This mirrors the CJK-pair suppression in the text
/// extraction path (`document.rs`).
pub(crate) fn has_horizontal_gap(prev: &TextSpan, current: &TextSpan) -> bool {
    let font_size = prev.font_size.max(current.font_size).max(1.0);
    let prev_end_x = prev.bbox.x + prev.bbox.width;
    let gap = current.bbox.x - prev_end_x;
    let threshold = font_size * 0.15;
    // Sub-em gaps are inter-glyph kerning — no space needed.  ANY gap
    // larger than that, including gaps >5 em (column boundaries on
    // wide tables — issue 487 pr-138-example.pdf), must result in a
    // space.  The previous `gap < 5 em` upper bound made the caller
    // concatenate without separator for huge gaps, gluing tokens like
    // `3.80%` + `4.41%` into `3.80%4.41%` when the rate-table cells
    // sit ~265 pt apart and the table detector wasn't able to capture
    // them as a real grid.
    if gap <= threshold {
        return false;
    }

    // Suppress space insertion when one side is CJK and the other is CJK or a
    // fullwidth/math operator.  This mirrors the CJK-pair suppression in the
    // text extraction path (document.rs:5587-5605).
    let prev_last = prev.text.chars().next_back();
    let curr_first = current.text.chars().next();
    if let (Some(p), Some(c)) = (prev_last, curr_first) {
        let p_cjk = is_cjk_char(p);
        let c_cjk = is_cjk_char(c);
        if (p_cjk || is_fullwidth_or_math_op(p)) && (c_cjk || is_fullwidth_or_math_op(c)) {
            // At least one side must actually be CJK (not two pure math ops).
            if p_cjk || c_cjk {
                return false;
            }
        }
    }

    true
}

/// Return the index of the table whose bounding box contains the span's
/// origin AND that has a cell whose bbox also contains the span — i.e.
/// the table is actually going to render this span as part of a cell.
///
/// Returning `Some(idx)` causes `convert_semantic_mode` (md/html) to skip
/// the span from paragraph flow on the assumption that the table render
/// will emit it.  If the span sits inside the table's *outer* bbox but
/// the spatial column-clustering missed the column it belongs to (a
/// sparse / variable-width score column on wide sailing-results grids
/// — issue 486 / 487), no cell will contain it and the table render
/// drops the content.  Treating that span as "outside the table" lets
/// the paragraph flow pick it up so the text is not lost.
pub(crate) fn span_in_table(span: &OrderedTextSpan, tables: &[Table]) -> Option<usize> {
    let sx = span.span.bbox.x;
    let sy = span.span.bbox.y;

    for (i, table) in tables.iter().enumerate() {
        let Some(ref bbox) = table.bbox else { continue };
        let tolerance = 2.0;
        let in_outer_bbox = sx >= bbox.x - tolerance
            && sx <= bbox.x + bbox.width + tolerance
            && sy >= bbox.y - tolerance
            && sy <= bbox.y + bbox.height + tolerance;
        if !in_outer_bbox {
            continue;
        }
        // Span is geometrically inside the table — verify a cell will
        // own it.  Walks all rows / cells once; tables that get through
        // is_real_grid are typically small enough (≤30 rows × ≤25 cols)
        // that this is negligible vs. the cost of running the conversion.
        //
        // Special case: a Table with no cell bboxes at all (e.g. when
        // built from MCID-based tagged-PDF extraction, or in unit-test
        // fixtures) carries the rendering responsibility wholesale —
        // there is no per-cell layout to consult.  Fall back to the
        // outer-bbox containment for that case so we don't silently
        // skip the table rendering.
        let has_any_cell_bbox = table
            .rows
            .iter()
            .any(|row| row.cells.iter().any(|c| c.bbox.is_some()));
        if !has_any_cell_bbox {
            return Some(i);
        }
        let span_owned = table.rows.iter().any(|row| {
            row.cells.iter().any(|cell| {
                let Some(cb) = cell.bbox else { return false };
                sx >= cb.x - tolerance
                    && sx <= cb.x + cb.width + tolerance
                    && sy >= cb.y - tolerance
                    && sy <= cb.y + cb.height + tolerance
            })
        });
        if span_owned {
            return Some(i);
        }
        // Span sits in the outer bbox but no cell claims it; fall through
        // to paragraph flow so the content is not silently dropped.
    }
    None
}

/// Post-process rendered text to merge key-value pairs that were split across
/// lines due to column-based reading order.
///
/// Detects the pattern where a text label (e.g. "Grand Total") appears on one
/// line and its corresponding value (e.g. "$750.00") appears alone on the next
/// line.  When detected, the two lines are merged into one with a separating
/// space (e.g. "Grand Total $750.00").
///
/// A line is considered a "value" if it is short (< 30 chars), starts with a
/// digit, currency symbol, or parenthesized number, and does not look like a
/// sentence continuation.  A line is considered a "label" if it ends with
/// alphabetic text (no trailing punctuation that would indicate a complete
/// sentence).
pub(crate) fn merge_key_value_pairs(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 2 {
        return text.to_string();
    }

    // Determine which lines are "value-only" lines that should merge upward.
    // A value line is short and starts with a digit, $, (, -, or similar
    // numeric indicator.
    fn is_value_line(line: &str) -> bool {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.len() > 30 {
            return false;
        }
        // A markdown list item (bullet or ordered marker like "1. Finalize")
        // is never a value — merging it into the line above would glue a list
        // item onto a heading or label.
        if is_ordered_list_marker(trimmed).is_some() || starts_with_bullet(trimmed) {
            return false;
        }
        let mut chars = trimmed.chars();
        let first = chars.next().unwrap();
        match first {
            '0'..='9' | '$' | '€' | '£' | '¥' | '(' => true,
            // '-' / '.' indicate a value ("-$42.50", "-50", ".50") unless
            // followed by a space — i.e. a markdown list bullet "- ", which must
            // NOT merge a list item into the line above it (e.g. a heading).
            '-' | '.' => !matches!(chars.next(), Some(' ') | None),
            _ => false,
        }
    }

    // A label line: non-empty, ends with a word character (letter or digit),
    // does not end with sentence-terminal punctuation.  We also reject lines
    // that are themselves value-only (to avoid merging two values).
    fn is_label_line(line: &str) -> bool {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return false;
        }
        // Must not itself be a value-only line
        if is_value_line(line) {
            return false;
        }
        // Last non-whitespace character should be alphanumeric or ')' or ':'
        // (not sentence-ending like '.', '!', '?')
        let last = trimmed.chars().next_back().unwrap();
        last.is_alphanumeric() || last == ')' || last == ':'
    }

    let mut result = String::with_capacity(text.len());
    let mut i = 0;
    while i < lines.len() {
        // Pattern 1: label immediately followed by value (no blank line)
        if i + 1 < lines.len() && is_label_line(lines[i]) && is_value_line(lines[i + 1]) {
            result.push_str(lines[i].trim_end());
            result.push(' ');
            result.push_str(lines[i + 1].trim_start());
            result.push('\n');
            i += 2;
        }
        // Pattern 2: label, blank line, value (paragraph break between them)
        else if i + 2 < lines.len()
            && is_label_line(lines[i])
            && lines[i + 1].trim().is_empty()
            && is_value_line(lines[i + 2])
        {
            result.push_str(lines[i].trim_end());
            result.push(' ');
            result.push_str(lines[i + 2].trim_start());
            result.push('\n');
            i += 3;
        } else {
            result.push_str(lines[i]);
            result.push('\n');
            i += 1;
        }
    }

    // Restore the exact trailing-newline count of the original input.
    // `text.lines()` strips all trailing empty lines, so we count them here
    // and re-append them after processing.
    let orig_trailing_newlines = text.chars().rev().take_while(|&c| c == '\n').count();
    // Strip any trailing newlines we added, then re-append the original count.
    while result.ends_with('\n') {
        result.pop();
    }
    for _ in 0..orig_trailing_newlines {
        result.push('\n');
    }

    result
}
