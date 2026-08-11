//! Table extraction from PDF structure tree.
//!
//! Implements table detection and reconstruction according to ISO 32000-1:2008 Section 14.8.4.3.4
//! (Table Elements).
//!
//! Table structure hierarchy:
//! - Table: Top-level container
//!   - THead: Optional header row group
//!   - TBody: One or more body row groups
//!   - TFoot: Optional footer row group
//! - TR: Table row (contains TH and/or TD cells)
//!   - TH: Table header cell
//!   - TD: Table data cell

use crate::error::Error;
use crate::geometry::Rect;
use crate::layout::{Color, FontWeight, TextBlock, TextSpan};
use crate::structure::types::{StructChild, StructElem, StructType};

/// A complete extracted table with rows and optional header information.
#[derive(Debug, Clone)]
pub struct Table {
    /// Rows of the table (alternating between header and body rows)
    pub rows: Vec<TableRow>,

    /// Whether the table has an explicit header section
    pub has_header: bool,

    /// Number of columns (inferred from first row)
    pub col_count: usize,

    /// Bounding box of the table region (used to exclude table spans from normal rendering)
    pub bbox: Option<Rect>,
}

/// A single row in a table.
#[derive(Debug, Clone)]
pub struct TableRow {
    /// Cells in this row
    pub cells: Vec<TableCell>,

    /// Whether this is a header row
    pub is_header: bool,
}

/// A single cell in a table.
#[derive(Debug, Clone)]
pub struct TableCell {
    /// Text content of the cell
    pub text: String,

    /// Original text spans that make up this cell's content, with
    /// font/style metadata preserved for format-aware rendering.
    pub spans: Vec<crate::layout::TextSpan>,

    /// Number of columns this cell spans (default 1)
    pub colspan: u32,

    /// Number of rows this cell spans (default 1)
    pub rowspan: u32,

    /// MCID values that make up this cell's content
    pub mcids: Vec<u32>,

    /// Bounding box of the cell (v0.3.14)
    pub bbox: Option<Rect>,

    /// Whether this is a header cell
    pub is_header: bool,
}

/// Find all Table structure elements in the structure tree for a given page.
///
/// Recursively walks the structure tree to collect StructElem nodes where
/// `struct_type == StructType::Table` and the element (or any descendant)
/// has marked content on the specified page.
///
/// # Arguments
/// * `struct_tree` - The structure tree root
/// * `page_num` - Page number to match (0-based)
///
/// # Returns
/// * `Vec<&StructElem>` - Table elements found for the page
pub fn find_table_elements(
    struct_tree: &crate::structure::types::StructTreeRoot,
    page_num: u32,
) -> Vec<&StructElem> {
    let mut tables = Vec::new();
    for elem in &struct_tree.root_elements {
        collect_table_elements(elem, page_num, &mut tables);
    }
    tables
}

/// Recursively collect Table elements that have content on the given page.
fn collect_table_elements<'a>(
    elem: &'a StructElem,
    page_num: u32,
    tables: &mut Vec<&'a StructElem>,
) {
    if elem.struct_type == StructType::Table {
        if element_has_page_content(elem, page_num) {
            tables.push(elem);
        }
        return; // Don't recurse into table children looking for nested tables
    }

    for child in &elem.children {
        if let StructChild::StructElem(child_elem) = child {
            collect_table_elements(child_elem, page_num, tables);
        }
    }
}

/// Walk the structure tree once and bucket every `Table` element by each page
/// it has content on (owned clones), so the converter table path can do an
/// O(1) per-page lookup instead of `find_table_elements`'s per-page walk
/// (≈ O(pages²) on a tagged document). For a given page the result matches
/// `find_table_elements(tree, page)`: same DFS pre-order, and like
/// `collect_table_elements` it does not recurse into a Table's children.
pub fn find_table_elements_all_pages(
    struct_tree: &crate::structure::types::StructTreeRoot,
) -> std::collections::HashMap<u32, Vec<StructElem>> {
    let mut by_page: std::collections::HashMap<u32, Vec<StructElem>> =
        std::collections::HashMap::new();
    for elem in &struct_tree.root_elements {
        collect_table_elements_all_pages(elem, &mut by_page);
    }
    by_page
}

fn collect_table_elements_all_pages(
    elem: &StructElem,
    by_page: &mut std::collections::HashMap<u32, Vec<StructElem>>,
) {
    if elem.struct_type == StructType::Table {
        // Pages this table (or any descendant) has content on — the exact set
        // for which `element_has_page_content(elem, page)` is true.
        let mut pages = std::collections::BTreeSet::new();
        collect_content_pages(elem, &mut pages);
        for p in pages {
            by_page.entry(p).or_default().push(elem.clone());
        }
        return; // mirror collect_table_elements: don't recurse into table children
    }

    for child in &elem.children {
        if let StructChild::StructElem(child_elem) = child {
            collect_table_elements_all_pages(child_elem, by_page);
        }
    }
}

/// Collect the set of pages on which `elem` (or any descendant) has content.
/// Mirrors the truth set of [`element_has_page_content`] across all pages.
fn collect_content_pages(elem: &StructElem, pages: &mut std::collections::BTreeSet<u32>) {
    if let Some(p) = elem.page {
        pages.insert(p);
    }
    for child in &elem.children {
        match child {
            StructChild::MarkedContentRef { page, .. } => {
                pages.insert(*page);
            }
            StructChild::StructElem(child_elem) => {
                collect_content_pages(child_elem, pages);
            }
            StructChild::ObjectRef(_, _) => {}
        }
    }
}

/// Check if a structure element or any descendant has marked content on the given page.
fn element_has_page_content(elem: &StructElem, page_num: u32) -> bool {
    // Check the element's own page attribute
    if elem.page == Some(page_num) {
        return true;
    }

    for child in &elem.children {
        match child {
            StructChild::MarkedContentRef { page, .. } => {
                if *page == page_num {
                    return true;
                }
            }
            StructChild::StructElem(child_elem) => {
                if element_has_page_content(child_elem, page_num) {
                    return true;
                }
            }
            StructChild::ObjectRef(_, _) => {}
        }
    }

    false
}

/// Extract a table from a structure element tree using TextSpans (MCID matching).
///
/// Converts TextSpans to a format suitable for MCID-based cell text extraction,
/// then delegates to the standard `extract_table` function.
///
/// # Arguments
/// * `table_elem` - The Table structure element
/// * `spans` - Text spans from the page (with MCID values)
///
/// # Returns
/// * `Table` containing all rows and cells
pub fn extract_table_from_spans(
    table_elem: &StructElem,
    spans: &[crate::layout::TextSpan],
) -> Result<Table, Error> {
    // Convert spans to TextBlocks for MCID matching, applying column-spanning
    // decimal split so that "12.11" (sailing score columns) becomes "12 11".
    let text_blocks: Vec<TextBlock> = spans
        .iter()
        .filter(|s| s.mcid.is_some())
        .map(|s| {
            let text = span_text_for_cell(s);
            TextBlock {
                chars: Vec::new(),
                bbox: s.bbox,
                text,
                avg_font_size: s.font_size,
                dominant_font: s.font_name.clone(),
                is_bold: s.font_weight.is_bold(),
                is_italic: s.is_italic,
                mcid: s.mcid,
                sequence: s.sequence,
                rotation_degrees: s.rotation_degrees,
            }
        })
        .collect();
    extract_table(table_elem, &text_blocks)
}

/// Return the display text for a span when used as a table cell token.
/// Mirrors `PdfDocument::push_span_text`: splits column-spanning decimals
/// (e.g. "12.11" across adjacent score columns) at the decimal point.
pub(super) fn span_text_for_cell(span: &crate::layout::TextSpan) -> String {
    let text = &span.text;
    // Must be an "N.M" pattern with all-digit parts and a single dot.
    let dot_pos = match text.find('.') {
        Some(p) if p > 0 && p < text.len() - 1 => p,
        _ => return text.clone(),
    };
    if text[dot_pos + 1..].contains('.') {
        return text.clone();
    }
    if !text[..dot_pos].chars().all(|c| c.is_ascii_digit()) {
        return text.clone();
    }
    if !text[dot_pos + 1..].chars().all(|c| c.is_ascii_digit()) {
        return text.clone();
    }
    let char_count = text.chars().count();
    // Signal 1: sparse char_widths array (cw.len < char_count) means the
    // span was assembled from two concatenated Tj runs — see the matching
    // `is_column_spanning_decimal` rule in document.rs.  Catches sailing-
    // score cells emitted as a single Tj like "1.10" (cw=[w]) where the
    // PDF actually means "1" followed by "10" in adjacent score columns
    // (issue 487 nougat_018).  bbox.width can still be tight here, so the
    // bbox-inflation check below isn't sufficient.
    if !span.char_widths.is_empty() && span.char_widths.len() < char_count {
        return format!("{} {}", &text[..dot_pos], &text[dot_pos + 1..]);
    }
    let expected_width = if !span.char_widths.is_empty() {
        let cw_sum: f32 = span.char_widths.iter().sum();
        cw_sum * (char_count as f32 / span.char_widths.len() as f32)
    } else if span.font_size > 0.0 {
        // 0.50em per char: digits are narrower than average; keeps the
        // fallback from producing false negatives on word_spans (char_widths=[]).
        span.font_size * 0.50 * char_count as f32
    } else {
        return text.clone();
    };
    let gap = span.bbox.width - expected_width;
    if span.font_size > 0.0 && gap > span.font_size * 1.0 {
        format!("{} {}", &text[..dot_pos], &text[dot_pos + 1..])
    } else {
        text.clone()
    }
}

/// Extract a table from a structure element tree.
///
/// According to PDF spec Section 14.8.4.3.4, a Table element may contain:
/// - Direct TR (table row) children, OR
/// - THead (optional) + TBody (one or more) + TFoot (optional)
///
/// # Arguments
/// * `table_elem` - The Table structure element
/// * `text_blocks` - All text blocks in the document (for MCID matching)
///
/// # Returns
/// * `Table` containing all rows and cells
pub fn extract_table(table_elem: &StructElem, text_blocks: &[TextBlock]) -> Result<Table, Error> {
    let mut table = Table::new();

    // Check table structure
    let has_thead = table_elem
        .children
        .iter()
        .any(|child| matches!(child, StructChild::StructElem(elem) if elem.struct_type == StructType::THead));

    if has_thead {
        table.has_header = true;
    }

    // Process all children
    for child in &table_elem.children {
        match child {
            StructChild::StructElem(elem) => match elem.struct_type {
                StructType::TR => {
                    // Direct row in table
                    let row = extract_row(elem, text_blocks, false)?;
                    table.add_row(row);
                }
                StructType::THead => {
                    // Header row group
                    extract_row_group(elem, text_blocks, true, &mut table)?;
                }
                StructType::TBody => {
                    // Body row group
                    extract_row_group(elem, text_blocks, false, &mut table)?;
                }
                StructType::TFoot => {
                    // Footer row group
                    extract_row_group(elem, text_blocks, false, &mut table)?;
                }
                _ => {
                    // Skip other elements (caption, etc.)
                }
            },
            StructChild::MarkedContentRef { .. } => {
                // Skip direct content references
            }
            StructChild::ObjectRef(_, _) => {
                // Skip object references
            }
        }
    }

    Ok(table)
}

/// Extract rows from a row group (THead, TBody, TFoot).
fn extract_row_group(
    group_elem: &StructElem,
    text_blocks: &[TextBlock],
    is_header: bool,
    table: &mut Table,
) -> Result<(), Error> {
    for child in &group_elem.children {
        match child {
            StructChild::StructElem(elem) if elem.struct_type == StructType::TR => {
                let row = extract_row(elem, text_blocks, is_header)?;
                table.add_row(row);
            }
            _ => {
                // Skip non-row elements
            }
        }
    }
    Ok(())
}

/// Extract a single row (TR element).
fn extract_row(
    tr_elem: &StructElem,
    text_blocks: &[TextBlock],
    force_header: bool,
) -> Result<TableRow, Error> {
    let mut row = TableRow::new(force_header);

    for child in &tr_elem.children {
        match child {
            StructChild::StructElem(elem) => match elem.struct_type {
                StructType::TH => {
                    // Header cell
                    let cell = extract_cell(elem, text_blocks, true)?;
                    row.add_cell(cell);
                }
                StructType::TD => {
                    // Data cell
                    let cell = extract_cell(elem, text_blocks, false)?;
                    row.add_cell(cell);
                }
                _ => {
                    // Skip other elements
                }
            },
            StructChild::MarkedContentRef { .. } => {
                // Skip direct content references
            }
            StructChild::ObjectRef(_, _) => {
                // Skip object references
            }
        }
    }

    Ok(row)
}

/// Extract a single cell (TH or TD element).
fn extract_cell(
    cell_elem: &StructElem,
    text_blocks: &[TextBlock],
    is_header: bool,
) -> Result<TableCell, Error> {
    // Collect all MCIDs from this cell
    let mut mcids = Vec::new();
    collect_mcids(cell_elem, &mut mcids);

    // Find all text blocks that match these MCIDs, joining them with position-aware
    // spacing: insert a space only when there is a genuine horizontal gap between
    // adjacent spans on the same line, or when spans are on different lines.
    // This prevents spurious spaces inside CJK expressions like "Q（peu/d）" whose
    // glyphs are stored as separate marked-content runs that abut each other.
    let mut cell_text = String::new();
    // Issue #8 fix: also collect per-block style info as synthetic TextSpans
    // so the markdown renderer's `render_table_markdown` can emit bold /
    // italic markers per fragment. Without this, the tagged-PDF path
    // produced cells with empty `spans`, which the markdown renderer
    // falls back from to plain text — losing ~73% of inline formatting
    // in the reporter's 54-PDF corpus.
    let mut cell_spans: Vec<TextSpan> = Vec::new();
    let mut prev_block: Option<&TextBlock> = None;
    for mcid in &mcids {
        for block in text_blocks {
            if let Some(block_mcid) = block.mcid {
                if block_mcid == *mcid {
                    let mut leading_space = false;
                    if !cell_text.is_empty() {
                        let need_space = if let Some(prev) = prev_block {
                            let y_diff = (block.bbox.y - prev.bbox.y).abs();
                            let line_h = prev.bbox.height.max(block.bbox.height);
                            if y_diff > line_h * 0.5 {
                                // Different lines — always insert a space.
                                true
                            } else {
                                // Same line — only insert a space when there is an actual
                                // horizontal gap (> 15% of font size, matching document.rs).
                                let gap = block.bbox.x - (prev.bbox.x + prev.bbox.width);
                                let font_size =
                                    prev.avg_font_size.max(block.avg_font_size).max(1.0);
                                if gap <= font_size * 0.15 {
                                    false
                                } else {
                                    // Suppress space insertion when one side is CJK and the
                                    // other is CJK or a fullwidth/math operator (e.g. ≤, ＜, μ).
                                    // This mirrors the CJK-pair suppression in document.rs and
                                    // converters/mod.rs (Issue #485).
                                    #[inline(always)]
                                    fn is_cjk(c: char) -> bool {
                                        matches!(c,
                                            '\u{3040}'..='\u{309F}' |   // Hiragana
                                            '\u{30A0}'..='\u{30FF}' |   // Katakana
                                            '\u{4E00}'..='\u{9FFF}' |   // CJK Unified Ideographs
                                            '\u{AC00}'..='\u{D7AF}' |   // Hangul
                                            '\u{3400}'..='\u{4DBF}' |   // CJK Extension A
                                            '\u{20000}'..='\u{2A6DF}'   // CJK Extension B
                                        )
                                    }
                                    #[inline(always)]
                                    fn is_fw_math(c: char) -> bool {
                                        matches!(c,
                                            '\u{FF0B}' | '\u{FF0D}' |
                                            '\u{FF1A}' | '\u{FF1B}' |
                                            '\u{FF1C}'..='\u{FF1E}' |
                                            '\u{2260}' | '\u{2248}' |
                                            '\u{2264}'..='\u{2265}' |
                                            '\u{00B5}' | '\u{03BC}' |
                                            '\u{00B1}' | '\u{00D7}' | '\u{00F7}'
                                        )
                                    }
                                    let p_last = prev.text.chars().next_back();
                                    let b_first = block.text.chars().next();
                                    let suppress = if let (Some(p), Some(b)) = (p_last, b_first) {
                                        let p_cjk = is_cjk(p);
                                        let b_cjk = is_cjk(b);
                                        (p_cjk || is_fw_math(p))
                                            && (b_cjk || is_fw_math(b))
                                            && (p_cjk || b_cjk)
                                    } else {
                                        false
                                    };
                                    !suppress
                                }
                            }
                        } else {
                            !cell_text.ends_with(' ')
                        };
                        if need_space {
                            cell_text.push(' ');
                            leading_space = true;
                        }
                    }
                    cell_text.push_str(&block.text);
                    // Synthesize a minimal TextSpan capturing the block's
                    // style. Only the fields the markdown converter
                    // consults (text, font_weight, is_italic, font_size,
                    // bbox) need real values — everything else is filled
                    // from sensible defaults. Carry the inter-block space
                    // into the span text as well: the markdown/HTML table
                    // renderers reconstruct spacing from the spans (not from
                    // cell_text), and their horizontal-gap heuristic cannot
                    // see a line wrap, so without this they glue tokens
                    // across wrapped lines. Both renderers already treat a
                    // leading space in the span text as authoritative
                    // (their `already_has_space` guard), so this never
                    // double-spaces.
                    let span_text = if leading_space {
                        let mut s = String::with_capacity(block.text.len() + 1);
                        s.push(' ');
                        s.push_str(&block.text);
                        s
                    } else {
                        block.text.clone()
                    };
                    cell_spans.push(TextSpan {
                        provenance: None,
                        artifact_type: None,
                        text: span_text,
                        bbox: block.bbox,
                        font_name: block.dominant_font.clone(),
                        font_size: block.avg_font_size,
                        font_weight: if block.is_bold {
                            FontWeight::Bold
                        } else {
                            FontWeight::Normal
                        },
                        is_italic: block.is_italic,
                        is_monospace: false,
                        color: Color::black(),
                        mcid: block.mcid,
                        mcid_scope: None,
                        sequence: 0,
                        offset_semantic: false,
                        split_boundary_before: false,
                        char_spacing: 0.0,
                        word_spacing: 0.0,
                        horizontal_scaling: 100.0,
                        primary_detected: false,
                        char_widths: vec![],
                        char_x_offsets: Vec::new(),
                        heading_level: None,
                        rotation_degrees: 0.0,
                        wmode: 0,
                        text_rise: 0.0,
                        rtl_draw_logical: false,
                    });
                    prev_block = Some(block);
                    break;
                }
            }
        }
    }

    let mut cell = TableCell::new(cell_text.trim().to_string(), is_header);
    cell.mcids = mcids;
    cell.spans = cell_spans;

    Ok(cell)
}

/// Recursively collect all MCIDs from a structure element and its children.
fn collect_mcids(elem: &StructElem, mcids: &mut Vec<u32>) {
    for child in &elem.children {
        match child {
            StructChild::MarkedContentRef { mcid, .. } => {
                mcids.push(*mcid);
            }
            StructChild::StructElem(child_elem) => {
                // Recursively collect from child elements
                collect_mcids(child_elem, mcids);
            }
            StructChild::ObjectRef(_, _) => {
                // Skip object references
            }
        }
    }
}

mod model;

#[cfg(test)]
mod tests;
