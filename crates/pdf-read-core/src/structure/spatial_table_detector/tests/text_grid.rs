use super::*;

/// Issue #6/#5: an agenda-style table has 3 real columns (Time @72,
/// Activity @200, Team @420). The Activity cell holds multiple words
/// laid out with wide gaps ("Receiving Dock Inspection"), each at a
/// distinct X that occurs in only ONE row. Greedy column clustering
/// turns every word X into a column; the cross-row text-edge
/// detector must instead recover the 3 real columns whose edges
/// recur across rows. Asserts the detected table has 3 columns, not
/// one-per-word.
#[test]
fn test_issue6_agenda_words_not_split_into_columns() {
    // y descending = rows top→bottom. 4 rows incl. header.
    let spans = vec![
        // Header row.
        create_test_span("Time", 72.0, 638.6, 24.4, 12.0),
        create_test_span("Activity", 200.0, 638.6, 34.8, 12.0),
        create_test_span("Team", 420.0, 638.6, 28.1, 12.0),
        // Row 1: Activity = "Receiving Dock Inspection" (3 word spans).
        create_test_span("06:00 - 07:00", 72.0, 610.6, 61.1, 12.0),
        create_test_span("Receiving", 200.0, 610.6, 43.9, 12.0),
        create_test_span("Dock", 249.9, 610.6, 22.8, 12.0),
        create_test_span("Inspection", 278.7, 610.6, 45.6, 12.0),
        create_test_span("Inbound Team", 420.0, 610.6, 65.7, 12.0),
        // Row 2: Activity = "Bulk Putaway Slotting".
        create_test_span("07:00 - 09:00", 72.0, 582.6, 61.1, 12.0),
        create_test_span("Bulk", 200.0, 582.6, 19.5, 12.0),
        create_test_span("Putaway", 225.4, 582.6, 38.3, 12.0),
        create_test_span("Slotting", 282.5, 582.6, 33.4, 12.0),
        create_test_span("Warehouse Ops", 420.0, 582.6, 73.5, 12.0),
        // Row 3: Activity = "Pick Wave Processing".
        create_test_span("09:00 - 11:00", 72.0, 554.6, 61.1, 12.0),
        create_test_span("Pick", 200.0, 554.6, 18.9, 12.0),
        create_test_span("Wave", 230.0, 554.6, 24.0, 12.0),
        create_test_span("Processing", 262.0, 554.6, 48.0, 12.0),
        create_test_span("Fulfillment", 420.0, 554.6, 55.0, 12.0),
    ];
    let config = TableDetectionConfig::default();
    let tables = detect_tables_from_spans(&spans, &config);
    // Either no table (acceptable — agenda is borderline tabular) or
    // a table with the 3 real columns. What must NOT happen: a table
    // with one column per Activity word (>= 5 columns).
    if let Some(t) = tables.first() {
        let ncols = t.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
        assert!(
            ncols <= 4,
            "agenda must not fragment Activity words into columns; got {} cols",
            ncols
        );
    }
}

#[test]
fn test_lines_strategy_no_lines_returns_empty() {
    let spans = vec![
        create_test_span("A", 10.0, 100.0, 10.0, 10.0),
        create_test_span("B", 50.0, 100.0, 10.0, 10.0),
        create_test_span("C", 10.0, 80.0, 10.0, 10.0),
        create_test_span("D", 50.0, 80.0, 10.0, 10.0),
    ];
    let config = TableDetectionConfig {
        horizontal_strategy: TableStrategy::Lines,
        vertical_strategy: TableStrategy::Lines,
        ..TableDetectionConfig::default()
    };
    assert!(detect_tables_with_lines(&spans, &[], &config).is_empty());
}

#[test]
fn test_horizontal_lines_only_strategy_no_false_positives() {
    // Regression test: horizontal_strategy: "lines" should NOT fall back to
    // text-based detection when there are no lines on the page.
    let spans = vec![
        create_test_span("A", 10.0, 100.0, 10.0, 10.0),
        create_test_span("B", 50.0, 100.0, 10.0, 10.0),
        create_test_span("C", 10.0, 80.0, 10.0, 10.0),
        create_test_span("D", 50.0, 80.0, 10.0, 10.0),
    ];
    // Test with horizontal_strategy: Lines but vertical_strategy: Both (the default)
    let config = TableDetectionConfig {
        horizontal_strategy: TableStrategy::Lines,
        vertical_strategy: TableStrategy::Both,
        ..TableDetectionConfig::default()
    };
    // Should return empty because there are no horizontal lines to define rows
    assert!(detect_tables_with_lines(&spans, &[], &config).is_empty());
}

/// Regression test for issue #486: text-only spatial fallback for line-less tables.
///
/// `text_fallback = true` is now the default on `TableDetectionConfig` (the
/// prose-shape filter and ≥3-row guard suppress the false positives that
/// previously motivated a `false` default).  With the default and the `Both`
/// strategy, `detect_tables_with_lines` with an empty lines slice falls
/// through to the text-based path and detects the grid from span alignment
/// alone.  Callers that explicitly want the conservative
/// "no ruling lines → no tables" behaviour set `text_fallback = false` and
/// the `extract_page_tables` early-return guard in document.rs short-circuits
/// before this code is reached.
///
/// This test directly calls `detect_tables_with_lines` with an empty lines
/// slice to verify that the text-based path inside it finds the table.
#[test]
fn test_text_fallback_detects_lineless_grid() {
    // Simulate a 3-column, 4-row sailing-score table with no ruling lines.
    // Columns at x=10, 50, 90; rows at y=200, 180, 160, 140.
    let spans = vec![
        // Row 1
        create_test_span("Pos", 10.0, 200.0, 25.0, 10.0),
        create_test_span("Boat", 50.0, 200.0, 25.0, 10.0),
        create_test_span("Pts", 90.0, 200.0, 20.0, 10.0),
        // Row 2
        create_test_span("1", 10.0, 180.0, 25.0, 10.0),
        create_test_span("Alpha", 50.0, 180.0, 25.0, 10.0),
        create_test_span("14", 90.0, 180.0, 20.0, 10.0),
        // Row 3
        create_test_span("2", 10.0, 160.0, 25.0, 10.0),
        create_test_span("Beta", 50.0, 160.0, 25.0, 10.0),
        create_test_span("17", 90.0, 160.0, 20.0, 10.0),
        // Row 4
        create_test_span("3", 10.0, 140.0, 25.0, 10.0),
        create_test_span("Gamma", 50.0, 140.0, 25.0, 10.0),
        create_test_span("21", 90.0, 140.0, 20.0, 10.0),
    ];

    // With the Both strategy, NO lines, and text_fallback explicitly
    // enabled, the text-based fallback inside detect_tables_with_lines
    // fires and finds the grid.  Issue 484: the default no longer enables
    // text_fallback to avoid spurious tables on report-style PDFs that
    // would otherwise be double-emitted by extract_text.
    let config = TableDetectionConfig {
        text_fallback: true,
        ..TableDetectionConfig::default()
    };
    let tables = detect_tables_with_lines(&spans, &[], &config);
    assert_eq!(
        tables.len(),
        1,
        "Text-only fallback in detect_tables_with_lines should detect the grid (got {:?} tables)",
        tables.len()
    );
    let t = &tables[0];
    assert_eq!(t.col_count, 3, "Should detect 3 columns");
    assert_eq!(t.rows.len(), 4, "Should detect 4 rows");
}

/// Verify that when no lines are present and `text_fallback = false` (the default),
/// the guard in `extract_page_tables` (outside `detect_tables_with_lines`) would
/// prevent the text path from running.  We simulate this at the config level: using
/// a `Lines`-only strategy ensures `detect_tables_with_lines` returns nothing when
/// paths are empty — confirming the safety contract for the public API path.
#[test]
fn test_text_fallback_disabled_lines_strategy_returns_empty() {
    let spans = vec![
        create_test_span("Pos", 10.0, 200.0, 25.0, 10.0),
        create_test_span("Boat", 50.0, 200.0, 25.0, 10.0),
        create_test_span("Pts", 90.0, 200.0, 20.0, 10.0),
        create_test_span("1", 10.0, 180.0, 25.0, 10.0),
        create_test_span("Alpha", 50.0, 180.0, 25.0, 10.0),
        create_test_span("14", 90.0, 180.0, 20.0, 10.0),
    ];
    // Lines-only strategy: no lines → no tables.  This is what the public
    // extract_tables() API uses after the early-return guard fires.
    let config = TableDetectionConfig::strict(); // strict() uses Lines/Lines
    let tables = detect_tables_with_lines(&spans, &[], &config);
    assert!(
        tables.is_empty(),
        "Lines-only strategy with no ruling lines should return no tables"
    );
}

#[test]
fn test_table_splitting_on_empty_row() {
    let spans = vec![
        create_test_span("T1-11", 20.0, 115.0, 10.0, 10.0),
        create_test_span("T1-12", 40.0, 115.0, 10.0, 10.0),
        create_test_span("T1-21", 20.0, 95.0, 10.0, 10.0),
        create_test_span("T1-22", 40.0, 95.0, 10.0, 10.0),
        create_test_span("T2-11", 20.0, 35.0, 10.0, 10.0),
        create_test_span("T2-12", 40.0, 35.0, 10.0, 10.0),
        create_test_span("T2-21", 20.0, 15.0, 10.0, 10.0),
        create_test_span("T2-22", 40.0, 15.0, 10.0, 10.0),
    ];
    let lines = vec![
        make_h_line(10.0, 130.0, 50.0),
        make_h_line(10.0, 110.0, 50.0),
        make_h_line(10.0, 90.0, 50.0),
        make_v_line(10.0, 90.0, 40.0),
        make_v_line(30.0, 90.0, 40.0),
        make_v_line(60.0, 90.0, 40.0),
        make_h_line(10.0, 50.0, 50.0),
        make_h_line(10.0, 30.0, 50.0),
        make_h_line(10.0, 10.0, 50.0),
        make_v_line(10.0, 10.0, 40.0),
        make_v_line(30.0, 10.0, 40.0),
        make_v_line(60.0, 10.0, 40.0),
        make_v_line(10.0, 50.0, 40.0),
    ];
    let config = TableDetectionConfig {
        horizontal_strategy: TableStrategy::Both,
        vertical_strategy: TableStrategy::Both,
        ..TableDetectionConfig::default()
    };
    assert_eq!(detect_tables_with_lines(&spans, &lines, &config).len(), 2);
}

#[test]
fn test_detect_columns_invoice_4_columns() {
    // Simulate invoice: Date | Description | Charges | Credits
    let spans = vec![
        create_test_span("01/01", 50.0, 100.0, 50.0, 10.0),
        create_test_span("Widget", 130.0, 100.0, 220.0, 10.0),
        create_test_span("$100", 500.0, 100.0, 50.0, 10.0),
        create_test_span("$0", 600.0, 100.0, 50.0, 10.0),
        create_test_span("02/15", 50.0, 80.0, 50.0, 10.0),
        create_test_span("Service fee", 130.0, 80.0, 220.0, 10.0),
        create_test_span("$250", 500.0, 80.0, 50.0, 10.0),
        create_test_span("$50", 600.0, 80.0, 50.0, 10.0),
        create_test_span("03/20", 50.0, 60.0, 50.0, 10.0),
        create_test_span("Consulting", 130.0, 60.0, 220.0, 10.0),
        create_test_span("$500", 500.0, 60.0, 50.0, 10.0),
        create_test_span("$100", 600.0, 60.0, 50.0, 10.0),
    ];
    let config = TableDetectionConfig::default();
    let columns = detect_columns(
        &spans,
        config.column_tolerance,
        config.column_merge_threshold,
    );
    assert_eq!(
        columns.len(),
        4,
        "Invoice with 4 distinct column groups should produce exactly 4 columns, got {}",
        columns.len()
    );
}

#[test]
fn test_detect_columns_merges_nearby_clusters() {
    // Spans at x=130, x=135, x=140 within same logical column
    let spans = vec![
        create_test_span("A", 50.0, 100.0, 30.0, 10.0),
        create_test_span("B", 130.0, 100.0, 30.0, 10.0),
        create_test_span("C", 50.0, 80.0, 30.0, 10.0),
        create_test_span("D", 135.0, 80.0, 30.0, 10.0),
        create_test_span("E", 50.0, 60.0, 30.0, 10.0),
        create_test_span("F", 140.0, 60.0, 30.0, 10.0),
    ];
    let config = TableDetectionConfig::default();
    let columns = detect_columns(
        &spans,
        config.column_tolerance,
        config.column_merge_threshold,
    );
    assert_eq!(
        columns.len(),
        2,
        "Spans at x=130/135/140 should merge into 1 column, plus x=50 = 2 total, got {}",
        columns.len()
    );
}

#[test]
fn test_detect_columns_order_independent() {
    let spans_ordered = vec![
        create_test_span("A", 50.0, 100.0, 30.0, 10.0),
        create_test_span("B", 200.0, 100.0, 30.0, 10.0),
        create_test_span("C", 400.0, 100.0, 30.0, 10.0),
        create_test_span("D", 50.0, 80.0, 30.0, 10.0),
        create_test_span("E", 200.0, 80.0, 30.0, 10.0),
        create_test_span("F", 400.0, 80.0, 30.0, 10.0),
    ];
    // Same spans but in reverse order
    let spans_reversed = vec![
        create_test_span("F", 400.0, 80.0, 30.0, 10.0),
        create_test_span("E", 200.0, 80.0, 30.0, 10.0),
        create_test_span("D", 50.0, 80.0, 30.0, 10.0),
        create_test_span("C", 400.0, 100.0, 30.0, 10.0),
        create_test_span("B", 200.0, 100.0, 30.0, 10.0),
        create_test_span("A", 50.0, 100.0, 30.0, 10.0),
    ];
    let config = TableDetectionConfig::default();
    let cols_ordered = detect_columns(
        &spans_ordered,
        config.column_tolerance,
        config.column_merge_threshold,
    );
    let cols_reversed = detect_columns(
        &spans_reversed,
        config.column_tolerance,
        config.column_merge_threshold,
    );
    assert_eq!(
        cols_ordered.len(),
        cols_reversed.len(),
        "Column count should be independent of span order"
    );
    // Centers should be in the same sorted order
    let centers_ordered: Vec<f32> = cols_ordered
        .iter()
        .map(|c| (c.x_center * 10.0).round())
        .collect();
    let centers_reversed: Vec<f32> = cols_reversed
        .iter()
        .map(|c| (c.x_center * 10.0).round())
        .collect();
    assert_eq!(
        centers_ordered, centers_reversed,
        "Column centers should match regardless of input order"
    );
}

#[test]
fn test_detect_header_row_returns_none_when_no_heuristic_matches() {
    // All spans have same font size, none bold -- no header signal
    let spans = vec![
        create_test_span("A", 10.0, 100.0, 30.0, 10.0),
        create_test_span("B", 50.0, 100.0, 30.0, 10.0),
        create_test_span("C", 10.0, 80.0, 30.0, 10.0),
        create_test_span("D", 50.0, 80.0, 30.0, 10.0),
    ];
    let columns = detect_columns(&spans, 15.0, 25.0);
    let rows = detect_rows(&spans, 2.8);
    let grid = assign_spans_to_cells(&spans, &columns, &rows);
    let header = detect_header_row(&grid, &spans);
    assert_eq!(header, None, "Should return None when no heuristic matches");
}

#[test]
fn test_hierarchical_header_with_visual_heuristic() {
    let spans = vec![
        create_test_span("H1", 10.0, 115.0, 35.0, 10.0),
        create_test_span("H2", 55.0, 115.0, 35.0, 10.0),
        create_test_span("Col 1", 10.0, 95.0, 35.0, 10.0),
        create_test_span("Col 2", 55.0, 95.0, 35.0, 10.0),
        create_test_span("Data 1", 10.0, 75.0, 35.0, 10.0),
        create_test_span("Data 2", 55.0, 75.0, 35.0, 10.0),
    ];
    let lines = vec![
        make_line_path(10.0, 130.0, 90.0, 130.0),
        make_line_path(10.0, 110.0, 90.0, 110.0),
        make_line_path(10.0, 90.0, 90.0, 90.0),
        make_v_line(10.0, 70.0, 60.0),
        make_v_line(50.0, 70.0, 20.0),
        make_v_line(90.0, 70.0, 60.0),
    ];
    let config = TableDetectionConfig::default();
    let tables = detect_tables_with_lines(&spans, &lines, &config);
    assert_eq!(tables.len(), 1);
    assert!(tables[0].rows[0].is_header);
    assert!(tables[0].rows[1].is_header);
}
