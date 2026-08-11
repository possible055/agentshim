use super::*;

#[test]
fn test_strip_form_numbering_artifacts() {
    use crate::structure::table_extractor::{TableCell, TableRow};

    let make_cell = |text: &str| TableCell {
        text: text.to_string(),
        spans: Vec::new(),
        colspan: 1,
        rowspan: 1,
        mcids: Vec::new(),
        bbox: None,
        is_header: false,
    };

    let mut rows = vec![
        // Row 0: all single-digit -> should be removed entirely
        TableRow {
            cells: vec![make_cell("5"), make_cell(""), make_cell(""), make_cell("")],
            is_header: false,
        },
        // Row 1: digit prefix artifacts -> should be stripped
        TableRow {
            cells: vec![
                make_cell("1 Apr 11, 2025"),
                make_cell("1 12111 - Rinse-Fluoride Treatment"),
                make_cell("1 $14.60"),
                make_cell("1"),
            ],
            is_header: false,
        },
        // Row 2: no artifacts -> unchanged
        TableRow {
            cells: vec![
                make_cell("Apr 11, 2025"),
                make_cell("11101 - One unit of time"),
                make_cell("$47.60"),
                make_cell(""),
            ],
            is_header: false,
        },
        // Row 3: digit prefix but remainder starts with digit -> NOT stripped
        TableRow {
            cells: vec![
                make_cell("3 items"),
                make_cell(""),
                make_cell(""),
                make_cell(""),
            ],
            is_header: false,
        },
    ];

    strip_form_numbering_artifacts(&mut rows);

    // Row 0 (all single-digit) should have been removed.
    assert_eq!(rows.len(), 3, "Single-digit-only row should be removed");

    // Former row 1 is now row 0: digit prefixes stripped.
    let r0: Vec<&str> = rows[0].cells.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(r0[0], "Apr 11, 2025", "Leading '1 ' should be stripped");
    assert_eq!(
        r0[1], "12111 - Rinse-Fluoride Treatment",
        "Leading '1 ' stripped, rest starts with digit but contains '-'"
    );
    assert_eq!(
        r0[2], "$14.60",
        "Leading '1 ' stripped, rest starts with '$'"
    );
    // "1" alone is cleared in Phase 3 because other cells in this row were stripped.
    assert_eq!(
        r0[3], "",
        "Lone '1' cleared when other cells in row were stripped"
    );

    // Former row 2 is now row 1: unchanged.
    let r1: Vec<&str> = rows[1].cells.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(r1[0], "Apr 11, 2025");
    assert_eq!(r1[1], "11101 - One unit of time");
    assert_eq!(r1[2], "$47.60");

    // Former row 3 is now row 2: "3 items" should NOT be stripped because
    // the remainder ("items") is a plain word with no date/code indicators.
    let r2: Vec<&str> = rows[2].cells.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(
        r2[0], "3 items",
        "'3 items' should NOT be stripped (plain word, no date/code/currency)"
    );
}

#[test]
fn test_strip_dash_separator_cells() {
    // T5 summary table: "------" appears as a decorative line separator
    // in a cell. After stripping, that cell should be empty.
    use crate::structure::table_extractor::{TableCell, TableRow};

    let make_cell = |text: &str| TableCell {
        text: text.to_string(),
        spans: Vec::new(),
        colspan: 1,
        rowspan: 1,
        mcids: Vec::new(),
        bbox: None,
        is_header: false,
    };

    let mut rows = vec![
        // Row with a dash-only cell (decorative separator)
        TableRow {
            cells: vec![
                make_cell("------"),
                make_cell("Total"),
                make_cell("$500.00"),
            ],
            is_header: false,
        },
        // Row with underscore-only cell
        TableRow {
            cells: vec![
                make_cell("____"),
                make_cell("Subtotal"),
                make_cell("$200.00"),
            ],
            is_header: false,
        },
        // Row with mixed dashes and underscores
        TableRow {
            cells: vec![make_cell("--__--"), make_cell("Tax"), make_cell("$10.00")],
            is_header: false,
        },
        // Row where ALL cells are dashes -> should be removed entirely
        TableRow {
            cells: vec![make_cell("------"), make_cell("---"), make_cell("------")],
            is_header: false,
        },
        // Row with real data that happens to contain a dash
        TableRow {
            cells: vec![
                make_cell("2025-01-01"),
                make_cell("Payment"),
                make_cell("$100.00"),
            ],
            is_header: false,
        },
    ];

    strip_form_numbering_artifacts(&mut rows);

    // Row 0: "------" cell should be cleared
    assert_eq!(
        rows[0].cells[0].text.trim(),
        "",
        "Dash-only cell should be cleared"
    );
    assert_eq!(rows[0].cells[1].text, "Total");
    assert_eq!(rows[0].cells[2].text, "$500.00");

    // Row 1: "____" cell should be cleared
    assert_eq!(
        rows[1].cells[0].text.trim(),
        "",
        "Underscore-only cell should be cleared"
    );

    // Row 2: "--__--" cell should be cleared
    assert_eq!(
        rows[2].cells[0].text.trim(),
        "",
        "Mixed dash/underscore cell should be cleared"
    );

    // Row with all-dash cells becomes all-empty (kept for downstream
    // empty-row splitting, which uses empty rows as table separators).
    assert_eq!(rows.len(), 5, "All-dash row kept as empty separator");
    assert!(
        rows[3].cells.iter().all(|c| c.text.trim().is_empty()),
        "All-dash row should now be all-empty"
    );

    // Real data with dash in date should be preserved
    assert_eq!(rows[4].cells[0].text, "2025-01-01");
}

#[test]
fn test_separate_small_and_large_table_clusters() {
    // Simulate an invoice layout with header + main table:
    // Header table: H-lines at y=83,142; V-lines at x=409,534 spanning y=71-143
    // Main table:   H-lines at y=150,553; V-lines at x=22,589 spanning y=150-553
    // These should form 2 separate clusters, not 1.
    let lines = vec![
        // Header table horizontal lines (thin rects: large width, tiny height)
        make_rect_path(409.0, 83.0, 125.0, 0.5), // H-line at y=83
        make_rect_path(409.0, 142.0, 125.0, 0.5), // H-line at y=142
        // Header table vertical lines (thin rects: tiny width, spans y=71..143)
        make_rect_path(409.0, 71.0, 0.5, 72.0), // V-line at x=409
        make_rect_path(534.0, 71.0, 0.5, 72.0), // V-line at x=534
        // Main table horizontal lines
        make_rect_path(22.0, 150.0, 567.0, 0.5), // H-line at y=150
        make_rect_path(22.0, 553.0, 567.0, 0.5), // H-line at y=553
        // Main table vertical lines (spans y=150..553)
        make_rect_path(22.0, 150.0, 0.5, 403.0), // V-line at x=22
        make_rect_path(490.0, 150.0, 0.5, 403.0), // V-line at x=490
        make_rect_path(589.0, 150.0, 0.5, 403.0), // V-line at x=589
    ];

    let config = TableDetectionConfig::default();
    let clusters = group_lines_into_clusters(&lines, &config);
    assert!(
        clusters.len() >= 2,
        "Expected at least 2 clusters (header table + main table), got {}",
        clusters.len()
    );

    // Verify that no single cluster contains both the header V-lines (y=71..143)
    // and the main table V-lines (y=150..553).
    for cluster in &clusters {
        let mut has_header_vline = false;
        let mut has_main_vline = false;
        for &idx in &cluster.lines {
            let bbox = &lines[idx].bbox;
            // V-line: width < 2
            if bbox.width.abs() < 2.0 && bbox.height.abs() > 5.0 {
                let y_max = bbox.y + bbox.height;
                if y_max < 145.0 {
                    has_header_vline = true;
                }
                if bbox.y >= 149.0 {
                    has_main_vline = true;
                }
            }
        }
        assert!(
            !(has_header_vline && has_main_vline),
            "A single cluster should not contain both header V-lines (y<145) and main V-lines (y>149)"
        );
    }
}

// -----------------------------------------------------------------------
// WS0.3b (C) header-row inclusion
// -----------------------------------------------------------------------

/// (C) A ruled 3-column grid whose header text row sits just ABOVE the top
/// ruling (unruled header). Its three labels align to the three detected
/// columns and span the table width, so the row must be pulled in as the
/// table's header (cells preserved, not merged, marked as header).
#[test]
fn test_ws03b_header_row_above_included() {
    let lines = vec![
        make_h_line(100.0, 560.0, 300.0), // top ruling
        make_h_line(100.0, 530.0, 300.0),
        make_h_line(100.0, 500.0, 300.0),
        make_v_line(100.0, 500.0, 60.0),
        make_v_line(200.0, 500.0, 60.0),
        make_v_line(300.0, 500.0, 60.0),
        make_v_line(400.0, 500.0, 60.0),
    ];
    let spans = vec![
        // Header row ABOVE the top ruling (y ~568..578), aligned to columns.
        create_test_span("Name", 140.0, 568.0, 20.0, 10.0),
        create_test_span("Age", 240.0, 568.0, 20.0, 10.0),
        create_test_span("City", 340.0, 568.0, 20.0, 10.0),
        // Body rows.
        create_test_span("Ann", 145.0, 545.0, 10.0, 10.0),
        create_test_span("30", 245.0, 545.0, 10.0, 10.0),
        create_test_span("NYC", 345.0, 545.0, 10.0, 10.0),
        create_test_span("Bob", 145.0, 515.0, 10.0, 10.0),
        create_test_span("41", 245.0, 515.0, 10.0, 10.0),
        create_test_span("LA", 345.0, 515.0, 10.0, 10.0),
    ];
    let config = TableDetectionConfig::default();
    let clusters = group_lines_into_clusters(&lines, &config);
    assert_eq!(clusters.len(), 1, "single ruled grid → one cluster");
    let tables = detect_tables_in_cluster(&spans, &lines, &clusters[0], &config);
    assert_eq!(tables.len(), 1, "one table expected");
    let table = &tables[0];
    assert_eq!(
        table.rows.len(),
        3,
        "header row above the top ruling must be included (2 body + 1 header)"
    );
    assert!(table.has_header, "table must be flagged as having a header");
    assert!(table.rows[0].is_header, "row 0 must be the header row");
    // Header cells are preserved (not colspan-merged) and hold the labels.
    let header_text: String = table.rows[0]
        .cells
        .iter()
        .map(|c| c.text.trim())
        .collect::<Vec<_>>()
        .join("|");
    for label in ["Name", "Age", "City"] {
        assert!(
            header_text.contains(label),
            "header row must contain {label:?}, got {header_text:?}"
        );
    }
}

/// (C-negative) Unrelated text above the same grid at an x OUTSIDE the column
/// extent does NOT align to any column, so no header row is added and the
/// stray text is left out of the table (table unchanged: 2 body rows only).
#[test]
fn test_ws03b_unaligned_text_above_not_header() {
    let lines = vec![
        make_h_line(100.0, 560.0, 300.0),
        make_h_line(100.0, 530.0, 300.0),
        make_h_line(100.0, 500.0, 300.0),
        make_v_line(100.0, 500.0, 60.0),
        make_v_line(200.0, 500.0, 60.0),
        make_v_line(300.0, 500.0, 60.0),
        make_v_line(400.0, 500.0, 60.0),
    ];
    let spans = vec![
        // Caption far to the RIGHT of the column extent (100..400) → unaligned.
        create_test_span("Table", 520.0, 568.0, 20.0, 10.0),
        create_test_span("caption", 560.0, 568.0, 40.0, 10.0),
        // Body rows.
        create_test_span("Ann", 145.0, 545.0, 10.0, 10.0),
        create_test_span("30", 245.0, 545.0, 10.0, 10.0),
        create_test_span("NYC", 345.0, 545.0, 10.0, 10.0),
        create_test_span("Bob", 145.0, 515.0, 10.0, 10.0),
        create_test_span("41", 245.0, 515.0, 10.0, 10.0),
        create_test_span("LA", 345.0, 515.0, 10.0, 10.0),
    ];
    let config = TableDetectionConfig::default();

    // The gate itself must reject the unaligned text.
    let mut row_ys = vec![560.0_f32, 530.0, 500.0];
    row_ys.sort_by(|a, b| crate::utils::safe_float_cmp(*b, *a));
    let col_xs = vec![100.0_f32, 200.0, 300.0, 400.0];
    assert!(
        detect_header_row_above(&spans, &row_ys, &col_xs).is_none(),
        "text outside the column extent must not be treated as a header row"
    );

    // End-to-end: table is unchanged (only the 2 ruled body rows) and the
    // caption text is not present in any cell.
    let clusters = group_lines_into_clusters(&lines, &config);
    let tables = detect_tables_in_cluster(&spans, &lines, &clusters[0], &config);
    assert_eq!(tables.len(), 1);
    assert_eq!(
        tables[0].rows.len(),
        2,
        "no header row must be added for unaligned text above the grid"
    );
    let all_text: String = tables[0]
        .rows
        .iter()
        .flat_map(|r| r.cells.iter())
        .map(|c| c.text.clone())
        .collect();
    assert!(
        !all_text.contains("caption"),
        "unaligned caption text must stay out of the table, got {all_text:?}"
    );
}

#[test]
fn test_text_edge_columns_form_layout() {
    // Simulate a form layout: text aligns to specific X positions across
    // many rows, but each row may only use a subset of columns.
    //
    // Column 1 (employer info):  left edge ~48
    // Column 2 (box codes):      left edge ~210
    // Column 3 (values):         left edge ~382
    // Column 4 (values):         left edge ~516
    //
    // We place 5+ spans at each column's X across different Y rows.

    let mut spans = Vec::new();
    let col_xs = [48.0_f32, 210.0, 382.0, 516.0];
    let row_ys = [700.0_f32, 680.0, 660.0, 640.0, 620.0, 600.0];

    for &cx in &col_xs {
        for &ry in &row_ys {
            spans.push(create_test_span("val", cx, ry, 40.0, 10.0));
        }
    }

    // Add some "noise" spans that only appear in 1-2 rows (should NOT
    // create extra columns).
    spans.push(create_test_span("noise", 130.0, 700.0, 20.0, 10.0));
    spans.push(create_test_span("noise", 132.0, 680.0, 20.0, 10.0));

    let config = TableDetectionConfig::default();
    let columns = detect_text_edge_columns(&spans, &config);

    // We expect roughly 4 column clusters (one per alignment edge),
    // possibly a few more for right-edges that also recur, but definitely
    // not 8+ as greedy clustering would produce.
    assert!(
        columns.len() >= 3 && columns.len() <= 6,
        "Expected 3-6 text-edge columns, got {}",
        columns.len()
    );

    // Verify the centres are close to the known left-edge positions.
    let centres: Vec<f32> = columns.iter().map(|c| c.x_center).collect();
    for &expected_x in &col_xs {
        assert!(
            centres
                .iter()
                .any(|&cx| (cx - expected_x).abs() < config.column_tolerance
                    || (cx - (expected_x + 40.0)).abs() < config.column_tolerance),
            "Expected a column near x={expected_x} (or its right edge), centres={centres:?}"
        );
    }
}

#[test]
fn test_text_edge_columns_noise_filtered() {
    // Spans that only appear in 1 row should not produce columns.
    let spans = vec![
        // Only 1 span at x=100 — below the min_row_count=3 threshold
        create_test_span("a", 100.0, 500.0, 30.0, 10.0),
        // 4 spans at x=300 — should survive
        create_test_span("c", 300.0, 500.0, 30.0, 10.0),
        create_test_span("d", 300.0, 480.0, 30.0, 10.0),
        create_test_span("e", 300.0, 460.0, 30.0, 10.0),
        create_test_span("f", 300.0, 440.0, 30.0, 10.0),
    ];

    let config = TableDetectionConfig::default();
    let columns = detect_text_edge_columns(&spans, &config);

    // x=100 has only 1 row, so its left edge should be filtered out.
    // x=300 has 4 rows so its left-edge (and possibly right-edge at 330)
    // should survive.  At most 2 columns.
    assert!(
        !columns.is_empty(),
        "Should produce at least one column from x=300"
    );
    // Make sure we don't get a column centred near 100
    for c in &columns {
        assert!(
            (c.x_center - 100.0).abs() > 15.0,
            "x=100 edge should have been filtered (only 1 row), but got column at {}",
            c.x_center
        );
    }
}

#[test]
fn test_text_edge_fallback_integration() {
    // When greedy detect_columns produces >6 columns, detect_tables_from_spans
    // should fall back to text-edge detection and produce fewer columns.
    //
    // Build a layout with 4 true alignment columns but noisy X offsets
    // that cause greedy clustering (tolerance=15) to split them.
    let mut spans = Vec::new();
    let true_cols = [50.0_f32, 200.0, 350.0, 500.0];
    let row_ys = [700.0_f32, 680.0, 660.0, 640.0, 620.0];

    for (ci, &cx) in true_cols.iter().enumerate() {
        for (ri, &ry) in row_ys.iter().enumerate() {
            // Add slight jitter that stays within snap_tolerance but could
            // push greedy clustering into creating extra columns when
            // combined with different-width spans.
            let jitter = ((ci + ri) % 3) as f32 * 2.0;
            spans.push(create_test_span("v", cx + jitter, ry, 30.0, 10.0));
        }
    }

    // Also add extra scattered spans at unique X positions (each only in
    // 1 row) to bloat the greedy column count past 6.
    for i in 0..10 {
        let x = 80.0 + i as f32 * 30.0;
        spans.push(create_test_span("x", x, 700.0, 15.0, 10.0));
    }

    let config = TableDetectionConfig {
        column_tolerance: 8.0, // tight tolerance to force many greedy columns
        ..TableDetectionConfig::default()
    };

    let greedy_cols = detect_columns(
        &spans,
        config.column_tolerance,
        config.column_merge_threshold,
    );
    // With tight tolerance + scattered spans, greedy should exceed 6.
    assert!(
        greedy_cols.len() > 6,
        "Precondition: greedy should produce >6 columns, got {}",
        greedy_cols.len()
    );

    let te_cols = detect_text_edge_columns(&spans, &config);
    assert!(
        te_cols.len() < greedy_cols.len(),
        "Text-edge should produce fewer columns ({}) than greedy ({})",
        te_cols.len(),
        greedy_cols.len()
    );
}
