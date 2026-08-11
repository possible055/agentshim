use super::*;

#[test]
fn test_is_bullet_span() {
    assert!(MarkdownOutputConverter::is_bullet_span("►"));
    assert!(MarkdownOutputConverter::is_bullet_span("•"));
    assert!(MarkdownOutputConverter::is_bullet_span("▪"));
    assert!(MarkdownOutputConverter::is_bullet_span(" ► "));
    assert!(!MarkdownOutputConverter::is_bullet_span("text"));
    assert!(!MarkdownOutputConverter::is_bullet_span("►text"));
    assert!(!MarkdownOutputConverter::is_bullet_span(""));
}

#[test]
fn test_starts_with_bullet() {
    assert!(MarkdownOutputConverter::starts_with_bullet("►text"));
    assert!(MarkdownOutputConverter::starts_with_bullet("• item"));
    assert!(MarkdownOutputConverter::starts_with_bullet("  ► indented"));
    assert!(!MarkdownOutputConverter::starts_with_bullet("text"));
    assert!(!MarkdownOutputConverter::starts_with_bullet(""));
}

#[test]
fn test_strip_bullet() {
    assert_eq!(MarkdownOutputConverter::strip_bullet("► text"), "text");
    assert_eq!(MarkdownOutputConverter::strip_bullet("•item"), "item");
    assert_eq!(
        MarkdownOutputConverter::strip_bullet("no bullet"),
        "no bullet"
    );
}

#[test]
fn test_bullet_spans_become_list_items() {
    // Simulates: ► (separate span) + "Analog input" (next span, same Y)
    // on a new line from previous content
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    let mut title = make_span("FEATURES", 50.0, 660.0, 11.0, FontWeight::Bold);
    title.reading_order = 0;

    let mut bullet = make_span("►", 50.0, 640.0, 8.8, FontWeight::Normal);
    bullet.reading_order = 1;

    let mut text = make_span("Analog input", 60.0, 640.0, 11.0, FontWeight::Normal);
    text.reading_order = 2;

    let mut bullet2 = make_span("►", 50.0, 626.0, 8.8, FontWeight::Normal);
    bullet2.reading_order = 3;

    let mut text2 = make_span("16-bit ADC", 60.0, 626.0, 11.0, FontWeight::Normal);
    text2.reading_order = 4;

    let spans = vec![title, bullet, text, bullet2, text2];
    let result = converter.convert(&spans, &config).unwrap();

    assert!(
        result.contains("- Analog input"),
        "Should convert bullet to list item: {}",
        result
    );
    assert!(
        result.contains("- 16-bit ADC"),
        "Should convert second bullet: {}",
        result
    );
    assert!(
        !result.contains("►"),
        "Should not contain raw bullet character: {}",
        result
    );
}

#[test]
fn test_inline_bullet_becomes_list_item() {
    // Simulates: "► Analog input" as a single span (inline bullet)
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    let mut title = make_span("TITLE", 50.0, 660.0, 11.0, FontWeight::Bold);
    title.reading_order = 0;

    let mut bullet_text = make_span("► Analog input", 50.0, 640.0, 11.0, FontWeight::Normal);
    bullet_text.reading_order = 1;

    let spans = vec![title, bullet_text];
    let result = converter.convert(&spans, &config).unwrap();

    assert!(
        result.contains("- Analog input"),
        "Should convert inline bullet to list item: {}",
        result
    );
}

#[test]
fn test_first_span_inline_bullet() {
    // First span on page starts with bullet — no prev_span exists.
    // Should still be converted to a markdown list item.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    let mut bullet_text = make_span("► First item", 50.0, 660.0, 11.0, FontWeight::Normal);
    bullet_text.reading_order = 0;

    let mut bullet_text2 = make_span("► Second item", 50.0, 646.0, 11.0, FontWeight::Normal);
    bullet_text2.reading_order = 1;

    let spans = vec![bullet_text, bullet_text2];
    let result = converter.convert(&spans, &config).unwrap();

    assert!(
        result.contains("- First item"),
        "First-span inline bullet should become list item: {}",
        result
    );
    assert!(
        result.contains("- Second item"),
        "Second inline bullet should become list item: {}",
        result
    );
}

// ============================================================================
// Issue #182: Heading over-detection prevention
// ============================================================================

#[test]
fn test_heading_base_font_excludes_small_spans() {
    // When page has many 8.8pt ► spans, the base font size should
    // still be ~11pt (excluding small spans), not 8.8pt
    let converter = MarkdownOutputConverter::new();
    let config = config_with_headings();

    let mut spans = Vec::new();
    let mut order = 0;

    // 10 bullet spans at 8.8pt (should be excluded from median)
    for i in 0..10 {
        let mut s = make_span(
            "►",
            50.0,
            600.0 - (i as f32) * 14.0,
            8.8,
            FontWeight::Normal,
        );
        s.reading_order = order;
        order += 1;
        spans.push(s);
    }

    // 10 text spans at 11pt (should be the median)
    for i in 0..10 {
        let mut s = make_span(
            "body text content",
            60.0,
            600.0 - (i as f32) * 14.0,
            11.0,
            FontWeight::Bold,
        );
        s.reading_order = order;
        order += 1;
        spans.push(s);
    }

    let result = converter.convert(&spans, &config).unwrap();

    // "body text content" at 11pt should NOT be detected as heading
    // because base_font_size should be ~11pt (ratio 1.0)
    assert!(
        !result.contains("### body text content"),
        "11pt bold text should not be heading when base is 11pt: {}",
        result
    );
}

// ============================================================================
// Issue #260: Single-word BT/ET blocks should have spaces between words
// ============================================================================

#[test]
fn test_issue_260_single_word_bt_et_blocks_get_spaces() {
    // PDFKit.NET places each word in its own BT/ET block with absolute positioning.
    // The markdown converter must detect the horizontal gap and insert a space.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    // Simulate: "The" at x=72 w=20, "quick" at x=96 w=30, "brown" at x=130 w=33
    // All same Y=500, font_size=12. Gaps: 96-92=4pt, 130-126=4pt.
    // 4pt gap > 0.15*12=1.8pt threshold → should insert space.
    let spans = vec![
        make_span_with_width("The", 72.0, 500.0, 20.0, 12.0, FontWeight::Normal, 0),
        make_span_with_width("quick", 96.0, 500.0, 30.0, 12.0, FontWeight::Normal, 1),
        make_span_with_width("brown", 130.0, 500.0, 33.0, 12.0, FontWeight::Normal, 2),
    ];

    let result = converter.convert(&spans, &config).unwrap();
    assert!(
        result.contains("The quick brown"),
        "Single-word BT/ET spans with gaps should have spaces inserted: got {:?}",
        result
    );
}

#[test]
fn test_issue_260_no_space_for_tight_spans() {
    // When spans are tightly packed (no significant gap), no extra space should be added.
    // This covers ligature fragments or split characters.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    // "Hel" at x=72 w=18, "lo" at x=90 w=12 — gap = 90-90 = 0pt, no space needed
    let spans = vec![
        make_span_with_width("Hel", 72.0, 500.0, 18.0, 12.0, FontWeight::Normal, 0),
        make_span_with_width("lo", 90.0, 500.0, 12.0, 12.0, FontWeight::Normal, 1),
    ];

    let result = converter.convert(&spans, &config).unwrap();
    assert!(
        result.contains("Hello"),
        "Tight spans should be merged without space: got {:?}",
        result
    );
}

#[test]
fn test_heading_detection_still_works_for_large_fonts() {
    let converter = MarkdownOutputConverter::new();
    let config = config_with_headings();

    let mut heading = make_span("BIG HEADING", 50.0, 100.0, 24.0, FontWeight::Bold);
    heading.reading_order = 0;

    let mut body = make_span("Body text", 50.0, 70.0, 11.0, FontWeight::Normal);
    body.reading_order = 1;

    let spans = vec![heading, body];
    let result = converter.convert(&spans, &config).unwrap();

    assert!(
        result.contains("# BIG HEADING"),
        "24pt text should be H1: {}",
        result
    );
}

// ============================================================================
// Bold consolidation tests
// ============================================================================

#[test]
fn test_bold_consolidation_adjacent_bold_spans() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    // Three adjacent bold spans on the same line — each word is a separate span.
    // Use realistic bbox widths so that horizontal gap detection inserts spaces.
    let mut s1 = make_span_w("ACME", 72.0, 700.0, 55.0, 12.0, FontWeight::Bold);
    s1.reading_order = 0;

    let mut s2 = make_span_w("GLOBAL", 130.0, 700.0, 42.0, 12.0, FontWeight::Bold);
    s2.reading_order = 1;

    let mut s3 = make_span_w("LTD.", 175.0, 700.0, 24.0, 12.0, FontWeight::Bold);
    s3.reading_order = 2;

    let spans = vec![s1, s2, s3];
    let result = converter.convert(&spans, &config).unwrap();

    // Should consolidate into a single bold block
    assert!(
        result.contains("**ACME GLOBAL LTD.**"),
        "Adjacent bold spans should be consolidated into one bold block, got: {}",
        result
    );
    // Should NOT have per-word bold markers
    assert!(
        !result.contains("**ACME** **GLOBAL**"),
        "Should not wrap each word individually in bold markers, got: {}",
        result
    );
}

// ============================================================================
// Issue: table cell dropping during markdown conversion
// ============================================================================

#[test]
fn test_render_table_markdown_all_cells_present() {
    // Simulates a financial statement table:
    //   Row 1 (header): "Account No." | "Reference" | "Tax ID" | "Confirmation"
    //   Row 2 (data):   "20003035"    | "403852"    | "123 456 789" | "4351966"
    let mut table = Table::new();
    table.has_header = true;
    table.col_count = 4;

    let mut header = TableRow::new(true);
    header.add_cell(TableCell::new("Account No.".to_string(), true));
    header.add_cell(TableCell::new("Reference".to_string(), true));
    header.add_cell(TableCell::new("Tax ID".to_string(), true));
    header.add_cell(TableCell::new("Confirmation".to_string(), true));
    table.add_row(header);

    let mut data = TableRow::new(false);
    data.add_cell(TableCell::new("20003035".to_string(), false));
    data.add_cell(TableCell::new("403852".to_string(), false));
    data.add_cell(TableCell::new("123 456 789".to_string(), false));
    data.add_cell(TableCell::new("4351966".to_string(), false));
    table.add_row(data);

    let result = MarkdownOutputConverter::new()
        .render_table_markdown(&table, &crate::pipeline::TextPipelineConfig::default());

    // All cells must be present
    assert!(
        result.contains("403852"),
        "Reference value '403852' must be present in markdown table: {}",
        result
    );
    assert!(
        result.contains("20003035"),
        "Account No. value must be present: {}",
        result
    );
    assert!(
        result.contains("123 456 789"),
        "Tax ID value must be present: {}",
        result
    );
    assert!(
        result.contains("4351966"),
        "Confirmation value must be present: {}",
        result
    );
    assert!(
        result.contains("Reference"),
        "Header must be present: {}",
        result
    );

    // Must have pipe separators (markdown table format)
    assert!(
        result.contains("|"),
        "Must be markdown table format with pipe separators"
    );
}

#[test]
fn test_render_table_markdown_short_row_padded() {
    // When a data row has fewer cells than the header, the markdown table
    // must pad with empty cells so every row has the same column count.
    // Otherwise markdown parsers silently drop trailing columns.
    let mut table = Table::new();
    table.has_header = true;
    table.col_count = 4;

    let mut header = TableRow::new(true);
    header.add_cell(TableCell::new("A".to_string(), true));
    header.add_cell(TableCell::new("B".to_string(), true));
    header.add_cell(TableCell::new("C".to_string(), true));
    header.add_cell(TableCell::new("D".to_string(), true));
    table.add_row(header);

    // Data row with only 2 cells (e.g., merge detection removed 2 cells)
    let mut data = TableRow::new(false);
    data.add_cell(TableCell::new("1".to_string(), false));
    data.add_cell(TableCell::new("2".to_string(), false));
    table.add_row(data);

    let result = MarkdownOutputConverter::new()
        .render_table_markdown(&table, &crate::pipeline::TextPipelineConfig::default());

    // Count pipes in header vs data row — they must match
    let lines: Vec<&str> = result.lines().collect();
    assert!(
        lines.len() >= 3,
        "Must have header, separator, and data row: {}",
        result
    );

    let header_pipes = lines[0].matches('|').count();
    let data_pipes = lines[2].matches('|').count();
    assert_eq!(
            header_pipes, data_pipes,
            "Header and data rows must have same number of pipe separators.\nHeader ({}): {}\nData   ({}): {}",
            header_pipes, lines[0], data_pipes, lines[2]
        );
}

#[test]
fn test_render_table_markdown_short_header_padded() {
    // When the header has fewer cells than the widest data row, the header
    // must also be padded.
    let mut table = Table::new();
    table.has_header = true;
    table.col_count = 3;

    let mut header = TableRow::new(true);
    header.add_cell(TableCell::new("X".to_string(), true));
    header.add_cell(TableCell::new("Y".to_string(), true));
    table.add_row(header);

    let mut data = TableRow::new(false);
    data.add_cell(TableCell::new("1".to_string(), false));
    data.add_cell(TableCell::new("2".to_string(), false));
    data.add_cell(TableCell::new("3".to_string(), false));
    table.add_row(data);

    let result = MarkdownOutputConverter::new()
        .render_table_markdown(&table, &crate::pipeline::TextPipelineConfig::default());

    let lines: Vec<&str> = result.lines().collect();
    assert!(
        lines.len() >= 3,
        "Must have header, separator, and data row: {}",
        result
    );

    let header_pipes = lines[0].matches('|').count();
    let data_pipes = lines[2].matches('|').count();
    assert_eq!(
            header_pipes, data_pipes,
            "Header and data rows must have same number of pipe separators.\nHeader ({}): {}\nData   ({}): {}",
            header_pipes, lines[0], data_pipes, lines[2]
        );

    // All data values must be present
    assert!(
        result.contains("| 3 |"),
        "Third cell in data row must be present: {}",
        result
    );
}

#[test]
fn test_key_value_pair_merging_in_markdown() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    // Simulate a single label on one line followed by its value on the next.
    // This happens when spans from different groups produce separate lines.
    let mut s0 = make_span("Grand Total", 50.0, 200.0, 12.0, FontWeight::Normal);
    s0.reading_order = 0;
    s0.group_id = Some(0);

    // Value on a different line (different Y), next in reading order, different group
    let mut s1 = make_span("$750.00", 300.0, 185.0, 12.0, FontWeight::Normal);
    s1.reading_order = 1;
    s1.group_id = Some(1);

    let spans = vec![s0, s1];
    let result = converter.convert(&spans, &config).unwrap();

    assert!(
        result.contains("Grand Total $750.00"),
        "Should merge label with value on same line: {:?}",
        result,
    );
}

/// D7 — Arabic text with Bold font-weight must NOT produce `**` markers in
/// the markdown output.  Reproduces the right_to_left_02 fixture where
/// contextual glyph forms (initial/medial/final) triggered the bold
/// detector, inserting spurious `**مرح**با` fragments.
#[test]
fn test_arabic_bold_span_no_spurious_bold_markers() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Even when the font is reported as Bold, Arabic text must NOT be
    // wrapped in `**…**` in the final markdown (the bold detector fires on
    // Latin-font-weight heuristics that are unreliable for Arabic glyphs).
    let span = make_span("مرحبا", 0.0, 100.0, 12.0, FontWeight::Bold);
    let result = converter.convert(&[span], &config).unwrap();
    assert!(
        !result.contains("**"),
        "spurious bold markers found in Arabic output: {:?}",
        result
    );
    assert!(
        result.contains("مرحبا"),
        "Arabic text lost in output: {:?}",
        result
    );
}

/// D7 — is_rtl_text / looks_rtl must return true for Arabic Unicode ranges
/// and false for ASCII.  Pins the detector contract used by the converter.
#[test]
fn test_rtl_detection_arabic_and_ascii() {
    // Arabic main block
    assert!(
        crate::text::bidi::looks_rtl("مرحبا"),
        "Arabic U+0600-U+06FF must be RTL"
    );
    // Arabic Presentation Forms-B (common in PDFs using contextual forms)
    assert!(
        crate::text::bidi::looks_rtl("\u{FE80}"),
        "Arabic Presentation Forms-B U+FE80 must be RTL"
    );
    // Hebrew
    assert!(
        crate::text::bidi::looks_rtl("שלום"),
        "Hebrew U+0590-U+05FF must be RTL"
    );
    // Pure ASCII must not trigger the RTL path.
    assert!(
        !crate::text::bidi::looks_rtl("hello world"),
        "ASCII must not be RTL"
    );
    assert!(
        !crate::text::bidi::looks_rtl(""),
        "empty string must not be RTL"
    );
}

/// D7 — strip_inline_emphasis_in_rtl must remove `**…**` and `*…*`
/// markers when the inner content is RTL (Arabic / Hebrew) and preserve
/// them when the inner content is LTR.
#[test]
fn test_strip_inline_emphasis_removes_rtl_markers() {
    // `**bold**` around Arabic text → markers stripped
    let out = strip_inline_emphasis_in_rtl("**مرح**با");
    assert!(
        !out.contains("**"),
        "bold markers must be stripped from Arabic: {:?}",
        out
    );
    assert!(
        out.contains("مرح") && out.contains("با"),
        "Arabic chars must survive stripping: {:?}",
        out
    );

    // `*italic*` around Arabic text → markers stripped
    let out2 = strip_inline_emphasis_in_rtl("*مرحبا*");
    assert!(
        !out2.contains('*'),
        "italic markers must be stripped from Arabic: {:?}",
        out2
    );
    assert!(out2.contains("مرحبا"), "Arabic text lost: {:?}", out2);

    // Emphasis around LTR content must be preserved.
    let out3 = strip_inline_emphasis_in_rtl("*Hello*");
    assert_eq!(
        out3, "*Hello*",
        "LTR emphasis must be preserved: {:?}",
        out3
    );

    // No asterisks → identity.
    let out4 = strip_inline_emphasis_in_rtl("مرحبا");
    assert_eq!(
        out4, "مرحبا",
        "no-asterisk path must be identity: {:?}",
        out4
    );
}

/// D7 — the RTL emphasis cleanup block must preserve the trailing newline
/// that the whitespace-normalisation pass added.  Previously `lines().join()`
/// silently dropped the terminal `\n`, corrupting multi-paragraph documents.
#[test]
fn test_rtl_cleanup_preserves_trailing_newline() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Two Arabic paragraphs separated by `\n\n`.  The result must end with
    // the same suffix the normaliser emits (a single `\n` in default mode).
    let mut s1 = make_span("مرحبا", 0.0, 200.0, 12.0, FontWeight::Normal);
    s1.block_id = Some(1);
    let mut s2 = make_span("عالم", 0.0, 100.0, 12.0, FontWeight::Normal);
    s2.block_id = Some(2);
    let result = converter.convert(&[s1, s2], &config).unwrap();
    // Must contain both words.
    assert!(
        result.contains("مرحبا"),
        "first Arabic word lost: {:?}",
        result
    );
    assert!(
        result.contains("عالم"),
        "second Arabic word lost: {:?}",
        result
    );
    // Result must end with a newline (the document-level trailing `\n`).
    assert!(
        result.ends_with('\n'),
        "trailing newline was dropped by RTL cleanup: {:?}",
        result
    );
}

// ─────────────────────────────────────────────────────────────────
// Regression suite for the v0.3.51/v0.3.52 markdown-extraction
// quality issues (external reporter, 54-PDF corpus). Each test
// exercises ONE issue with synthetic input — no external PDF
// dependency — so the harness stays deterministic and survives
// upstream re-extractor changes. Where a fix is post-process only,
// the helper function is invoked directly; where the fix is
// structural, a full `convert()` pass is used.
// ─────────────────────────────────────────────────────────────────

/// Issue #10 — stray leading `|` outside a table block must be
/// escaped so downstream renderers do not misread it as a malformed
/// table row.
#[test]
fn test_issue10_escape_stray_leading_pipes_basic() {
    let input = "| Finished Goods\n| Internal Use Only\nPage 1 of 12\n";
    let out = escape_stray_leading_pipes(input);
    assert!(
        out.contains("\\| Finished Goods"),
        "stray pipe must be escaped, got:\n{}",
        out
    );
    assert!(
        out.contains("\\| Internal Use Only"),
        "second stray pipe must be escaped, got:\n{}",
        out
    );
}

/// Issue #10 — a real markdown table block must NOT be escaped.
/// Guards against over-eager pipe escaping that would corrupt
/// legitimate tables.
#[test]
fn test_issue10_preserves_real_tables() {
    let input = "| Col A | Col B |\n|---|---|\n| 1 | 2 |\n";
    let out = escape_stray_leading_pipes(input);
    assert!(
        !out.contains("\\|"),
        "real table rows must not be escaped, got:\n{}",
        out
    );
}

/// REGRESSION GUARD (70-PDF sweep). A real markdown table with
/// mostly single-word cells (e.g. countries × Continent/Capital/
/// Currency) must NOT be flattened to prose by the pipeline. The
/// simplify_degenerate_tables heuristic that did this is retired
/// from the active path; this test pins the table survives a full
/// convert_with_tables() pass.
#[test]
fn test_regression_real_sparse_table_not_flattened() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mut table = Table::new();
    let mut header = TableRow::new(true);
    for h in ["", "Indonesia", "Germany", "Austria", "France", "Vatican"] {
        header.add_cell(TableCell::new(h.to_string(), true));
    }
    table.add_row(header);
    for (label, vals) in [
        ("Continent", ["Asia", "", "Europe", "", ""]),
        (
            "Capital",
            ["Jakarta", "Berlin", "Vienna", "Paris", "Vatican City"],
        ),
    ] {
        let mut row = TableRow::new(false);
        row.add_cell(TableCell::new(label.to_string(), false));
        for v in vals {
            row.add_cell(TableCell::new(v.to_string(), false));
        }
        table.add_row(row);
    }
    let result = converter
        .convert_with_tables(&[], &[table], &config)
        .unwrap();
    assert!(
        result.contains("|---|") || result.contains("| Indonesia |"),
        "real sparse table must survive as a table, got:\n{}",
        result
    );
}

/// REGRESSION GUARD (70-PDF sweep). Consecutive paragraphs with
/// identical text (e.g. several distinct form widgets that share
/// a label) must NOT be deduped away by the pipeline. The
/// dedup_consecutive_paragraphs step that did this is retired.
#[test]
fn test_regression_repeated_identical_paragraphs_preserved() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span(
            "Radio button, unselected",
            0.0,
            100.0,
            12.0,
            FontWeight::Normal,
        ),
        make_span(
            "Radio button, unselected",
            0.0,
            80.0,
            12.0,
            FontWeight::Normal,
        ),
        make_span(
            "Radio button, unselected",
            0.0,
            60.0,
            12.0,
            FontWeight::Normal,
        ),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    let count = result.matches("Radio button, unselected").count();
    assert_eq!(
        count, 3,
        "three distinct identical-label widgets must all survive, got {}:\n{}",
        count, result
    );
}
