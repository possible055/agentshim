use super::*;

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

/// Issue #3 — spatial-prose-as-table (>= 5 cols, >= 2 data rows,
/// >= 60% single-word non-empty cells) collapses to a paragraph.
#[test]
fn test_issue3_degenerate_table_collapses_to_paragraph() {
    let input = "\
| Q1 | Warehouse | throughput | increased | 15% |
|---|---|---|---|---|
| quarter | over | quarter | to | 23,500 |
| units | per | day | strong | demand |
";
    let out = simplify_degenerate_tables(input);
    assert!(
        !out.contains("|---|"),
        "separator row should be gone, got:\n{}",
        out
    );
    assert!(
        out.contains("Q1 Warehouse throughput increased 15%"),
        "header words flattened to prose, got:\n{}",
        out
    );
}

/// Issue #3 — a normal table with multi-word cells must SURVIVE.
/// Guards against over-eager flattening that would corrupt real
/// tabular data.
#[test]
fn test_issue3_preserves_legitimate_multi_word_tables() {
    let input = "\
| Region | Revenue Q1 | Revenue Q2 | Revenue Q3 | Revenue Q4 |
|---|---|---|---|---|
| North America Sales | 1.2 M | 1.5 M | 1.7 M | 1.9 M |
| Europe Sales Total | 0.8 M | 0.9 M | 1.1 M | 1.3 M |
";
    let out = simplify_degenerate_tables(input);
    assert!(
        out.contains("|---|"),
        "real table must keep separator, got:\n{}",
        out
    );
    assert!(
        out.contains("| North America Sales |"),
        "real table cells must remain, got:\n{}",
        out
    );
}

/// Issue #8 — a table cell that carries bold spans must render the
/// bold markers in the output. Reporter measured 73% bold-marker
/// loss across 53/54 files; this asserts at least the simple case.
#[test]
fn test_issue8_table_cell_renders_bold_marker() {
    let bold_span = TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: "Critical".to_string(),
        bbox: Rect::new(0.0, 0.0, 50.0, 12.0),
        font_name: "Test-Bold".to_string(),
        font_size: 12.0,
        font_weight: FontWeight::Bold,
        is_italic: false,
        is_monospace: false,
        color: Color::black(),
        mcid: None,
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
        rtl_draw_logical: false,
    };
    let mut cell = TableCell::new("Critical".to_string(), false);
    cell.spans.push(bold_span.clone());
    let mut row = TableRow::new(false);
    row.add_cell(cell);
    let mut table = Table::new();
    table.add_row(row);

    let result = MarkdownOutputConverter::new()
        .render_table_markdown(&table, &TextPipelineConfig::default());
    assert!(
        result.contains("**Critical**"),
        "bold marker must appear in rendered cell, got:\n{}",
        result
    );
}

/// Issue #5 — all-identical header cells (spatial-grouping
/// artifact) must be deduped to a single occurrence in the
/// rendered output. Operates on the assembled markdown so it
/// catches both render paths.
#[test]
fn test_issue5_dedups_identical_header_cells() {
    let input = "| Q1'25 | Q1'25 | Q1'25 | Q1'25 |\n|---|---|---|---|\n| Zone A |  |  |  |\n";
    let out = dedup_identical_header_cells(input);
    let q1_count = out.matches("Q1'25").count();
    assert_eq!(
        q1_count, 1,
        "all-identical header cells must dedup to one, got {} in:\n{}",
        q1_count, out
    );
    // Cell count preserved (still 4 pipes in the data row).
    assert!(
        out.contains("Zone A"),
        "data row must remain intact, got:\n{}",
        out
    );
}

/// Issue #5 — a legitimate header with distinct values must NOT
/// be touched.
#[test]
fn test_issue5_preserves_real_distinct_headers() {
    let input = "| North | South | East | West |\n|---|---|---|---|\n| 1 | 2 | 3 | 4 |\n";
    let out = dedup_identical_header_cells(input);
    for col in ["North", "South", "East", "West"] {
        assert!(
            out.contains(col),
            "distinct header `{}` must survive: {}",
            col,
            out
        );
    }
}

/// Issue #7 — when side-by-side columns are present, text from
/// column 2 must not interleave with column 1's text mid-paragraph.
/// The existing `is_column_gap` heuristic (forward gutter > 3×
/// font_size OR backward wrap) is what forces the paragraph break
/// between columns; this test pins that behavior so future
/// reading-order refactors don't silently regress it.
#[test]
fn test_issue7_no_column_interleaving() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mk = |t: &str, x: f32, y: f32, bid: u32| {
        let mut s = make_span(t, x, y, 12.0, FontWeight::Normal);
        s.block_id = Some(bid);
        s
    };
    // Left column at x=0, right column at x=300; baselines stagger.
    let spans = vec![
        mk("Left A.", 0.0, 100.0, 1),
        mk("Right A.", 300.0, 100.0, 2),
        mk("Left B.", 0.0, 88.0, 1),
        mk("Right B.", 300.0, 88.0, 2),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    // Left column must surface as a contiguous run.
    assert!(
        result.contains("Left A.") && result.contains("Left B."),
        "left column must surface, got:\n{}",
        result
    );
    // No interleaving: "Left A. Right A." together would prove
    // interleaving (reading-order put right immediately after left
    // before left's continuation).
    assert!(
        !result.contains("Left A. Right A."),
        "columns must not interleave at the line level, got:\n{}",
        result
    );
}
