use super::*;

#[test]
fn test_reject_table_with_too_many_empty_cells() {
    // Build a table with 12 columns but ~80% empty cells — should be rejected.
    use crate::structure::table_extractor::{Table, TableCell, TableRow};
    let col_count = 12;
    let mut rows = Vec::new();
    // Header row: only 3 of 12 cells have text
    let mut header = TableRow::new(true);
    for c in 0..col_count {
        header.cells.push(TableCell {
            text: if c < 3 {
                format!("H{c}")
            } else {
                String::new()
            },
            spans: Vec::new(),
            colspan: 1,
            rowspan: 1,
            mcids: vec![],
            bbox: None,
            is_header: true,
        });
    }
    rows.push(header);
    // 4 data rows: only 2 of 12 cells have text each
    for r in 0..4 {
        let mut row = TableRow::new(false);
        for c in 0..col_count {
            row.cells.push(TableCell {
                text: if c < 2 {
                    format!("R{r}C{c}")
                } else {
                    String::new()
                },
                spans: Vec::new(),
                colspan: 1,
                rowspan: 1,
                mcids: vec![],
                bbox: None,
                is_header: false,
            });
        }
        rows.push(row);
    }
    let table = Table {
        rows,
        has_header: true,
        col_count,
        bbox: None,
    };
    // 5 rows * 12 cols = 60 total, 3 + 4*2 = 11 filled, 49 empty → 81.7% empty
    assert!(
        !is_valid_table(&table),
        "Table with >60% empty cells should be rejected"
    );
}

#[test]
fn test_valid_table_passes_validation() {
    use crate::structure::table_extractor::{Table, TableCell, TableRow};
    let col_count = 3;
    let mut rows = Vec::new();
    for r in 0..4 {
        let mut row = TableRow::new(r == 0);
        for c in 0..col_count {
            row.cells.push(TableCell {
                text: format!("R{r}C{c}"),
                spans: Vec::new(),
                colspan: 1,
                rowspan: 1,
                mcids: vec![],
                bbox: None,
                is_header: r == 0,
            });
        }
        rows.push(row);
    }
    let table = Table {
        rows,
        has_header: true,
        col_count,
        bbox: None,
    };
    assert!(
        is_valid_table(&table),
        "Well-populated table should pass validation"
    );
}

/// Product data sheets have label/value rows that look like 2-column
/// tables to the spatial detector (key text on the left, value on
/// the right, with faint cell backgrounds). When the right-hand
/// value wraps, the detector emits a continuation row whose left
/// cell is empty — the hallmark of this false positive. Such tables
/// must be rejected so their rows remain in the flow text.
#[test]
fn test_narrow_shallow_table_rejected_as_false_positive() {
    use crate::structure::table_extractor::{Table, TableCell, TableRow};
    let col_count = 2;
    let rows_data: Vec<(&str, &str)> = vec![
        (
            "Temperature resistance",
            "adhered to aluminium, -56° C to +82° C",
        ),
        (
            "Resistance to cleaning agents",
            "adhered to aluminium, 8 h in solution (0.5% household",
        ),
        // Wrapping continuation → empty left cell.
        ("", "cleaning agents) at room temperature and 65° C, no"),
    ];
    let mut rows = Vec::new();
    for (label, value) in &rows_data {
        let mut row = TableRow::new(false);
        row.cells.push(TableCell {
            text: label.to_string(),
            spans: Vec::new(),
            colspan: 1,
            rowspan: 1,
            mcids: vec![],
            bbox: None,
            is_header: false,
        });
        row.cells.push(TableCell {
            text: value.to_string(),
            spans: Vec::new(),
            colspan: 1,
            rowspan: 1,
            mcids: vec![],
            bbox: None,
            is_header: false,
        });
        rows.push(row);
    }
    let table = Table {
        rows,
        has_header: false,
        col_count,
        bbox: None,
    };
    assert!(
        !is_valid_table(&table),
        "Narrow 2-column 'table' with an empty continuation cell must \
         be rejected so its rows stay in the flow text"
    );
}

/// A 2-column data table with enough filled rows is a real table
/// and must continue to pass validation. Pins the threshold so the
/// narrow-table guard does not regress genuine two-column tables.
#[test]
fn test_narrow_deep_table_still_accepted() {
    use crate::structure::table_extractor::{Table, TableCell, TableRow};
    let col_count = 2;
    let mut rows = Vec::new();
    for i in 0..6 {
        let mut row = TableRow::new(i == 0);
        row.cells.push(TableCell {
            text: format!("Key {i}"),
            spans: Vec::new(),
            colspan: 1,
            rowspan: 1,
            mcids: vec![],
            bbox: None,
            is_header: i == 0,
        });
        row.cells.push(TableCell {
            text: format!("Value {i}"),
            spans: Vec::new(),
            colspan: 1,
            rowspan: 1,
            mcids: vec![],
            bbox: None,
            is_header: i == 0,
        });
        rows.push(row);
    }
    let table = Table {
        rows,
        has_header: true,
        col_count,
        bbox: None,
    };
    assert!(
        is_valid_table(&table),
        "A 2-col × 6-row data table should still be accepted"
    );
}

/// A sparse 2-column table with a missing value on the right is a
/// legitimate pattern (key/value lists, form layouts, "N/A" rows) and
/// must NOT match the narrow-table false-positive signature, which
/// targets empty-LEFT / filled-RIGHT continuation rows specifically.
#[test]
fn test_narrow_sparse_table_with_missing_right_value_accepted() {
    use crate::structure::table_extractor::{Table, TableCell, TableRow};
    let col_count = 2;
    let rows_data: Vec<(&str, &str)> = vec![
        ("Name", "ACME Corp"),
        ("Registration", "12345"),
        ("Fax", ""),
        ("Email", "info@example.com"),
    ];
    let mut rows = Vec::new();
    for (label, value) in &rows_data {
        let mut row = TableRow::new(false);
        row.cells.push(TableCell {
            text: label.to_string(),
            spans: Vec::new(),
            colspan: 1,
            rowspan: 1,
            mcids: vec![],
            bbox: None,
            is_header: false,
        });
        row.cells.push(TableCell {
            text: value.to_string(),
            spans: Vec::new(),
            colspan: 1,
            rowspan: 1,
            mcids: vec![],
            bbox: None,
            is_header: false,
        });
        rows.push(row);
    }
    let table = Table {
        rows,
        has_header: false,
        col_count,
        bbox: None,
    };
    assert!(
        is_valid_table(&table),
        "A 2-col table with a missing right-hand value but no empty-left \
         continuation row must still validate"
    );
}

#[test]
fn test_text_only_tables_capped_at_max_columns() {
    // Build spans that form 8+ columns of text aligned across rows.
    // detect_tables_from_spans should reject when columns exceed max_table_columns.
    let mut spans = Vec::new();
    let col_xs = [50.0_f32, 100.0, 150.0, 200.0, 250.0, 300.0, 350.0, 400.0];
    let row_ys = [700.0_f32, 680.0, 660.0, 640.0, 620.0];

    for &cx in &col_xs {
        for &ry in &row_ys {
            spans.push(create_test_span("val", cx, ry, 30.0, 10.0));
        }
    }

    // Use tight tolerance so each X position becomes its own column,
    // and cap at 6 columns via config.
    let config = TableDetectionConfig {
        column_tolerance: 5.0,
        column_merge_threshold: 8.0,
        max_table_columns: 6,
        ..TableDetectionConfig::default()
    };

    let tables = detect_tables_from_spans(&spans, &config);
    assert!(
        tables.is_empty(),
        "Text-only table with 8 columns should be rejected (max_table_columns=6), got {} table(s)",
        tables.len()
    );
}

#[test]
fn test_extended_grid_when_lines_dont_cross() {
    // H-lines at y=100 and y=50 spanning full width x=0..500
    // V-lines at y=300..350 at x=0, x=100, x=200
    // These lines don't physically cross but should produce
    // a 1-row x 2-col grid via extended intersections.
    let lines = vec![
        make_h_line(0.0, 100.0, 500.0),
        make_h_line(0.0, 50.0, 500.0),
        make_v_line(0.0, 300.0, 50.0),
        make_v_line(100.0, 300.0, 50.0),
        make_v_line(200.0, 300.0, 50.0),
    ];

    // Place text in the cells to satisfy min_table_cells.
    let spans = vec![
        create_test_span("A", 30.0, 70.0, 20.0, 10.0),
        create_test_span("B", 130.0, 70.0, 20.0, 10.0),
    ];

    let config = TableDetectionConfig {
        horizontal_strategy: TableStrategy::Lines,
        vertical_strategy: TableStrategy::Lines,
        min_table_cells: 2,
        min_table_columns: 2,
        ..TableDetectionConfig::default()
    };

    let tables = detect_tables_from_intersections(&spans, &lines, &config);
    assert!(
        !tables.is_empty(),
        "Extended grid should produce at least one table when H and V lines don't cross"
    );
    let table = &tables[0];
    assert!(
        table.col_count >= 2,
        "Extended grid table should have at least 2 columns, got {}",
        table.col_count
    );
}

#[test]
fn test_merge_vertically_adjacent_tables() {
    // Two tables with 3 columns each, bboxes separated by 5pt (< ADJACENT_TABLE_MERGE_GAP).
    let table1 = Table {
        rows: vec![TableRow {
            cells: vec![
                TableCell {
                    text: "A".into(),
                    spans: Vec::new(),
                    colspan: 1,
                    rowspan: 1,
                    mcids: vec![],
                    bbox: None,
                    is_header: false,
                },
                TableCell {
                    text: "B".into(),
                    spans: Vec::new(),
                    colspan: 1,
                    rowspan: 1,
                    mcids: vec![],
                    bbox: None,
                    is_header: false,
                },
                TableCell {
                    text: "C".into(),
                    spans: Vec::new(),
                    colspan: 1,
                    rowspan: 1,
                    mcids: vec![],
                    bbox: None,
                    is_header: false,
                },
            ],
            is_header: false,
        }],
        has_header: false,
        col_count: 3,
        bbox: Some(Rect::new(0.0, 100.0, 300.0, 50.0)),
    };
    let table2 = Table {
        rows: vec![TableRow {
            cells: vec![
                TableCell {
                    text: "D".into(),
                    spans: Vec::new(),
                    colspan: 1,
                    rowspan: 1,
                    mcids: vec![],
                    bbox: None,
                    is_header: false,
                },
                TableCell {
                    text: "E".into(),
                    spans: Vec::new(),
                    colspan: 1,
                    rowspan: 1,
                    mcids: vec![],
                    bbox: None,
                    is_header: false,
                },
                TableCell {
                    text: "F".into(),
                    spans: Vec::new(),
                    colspan: 1,
                    rowspan: 1,
                    mcids: vec![],
                    bbox: None,
                    is_header: false,
                },
            ],
            is_header: false,
        }],
        has_header: false,
        col_count: 3,
        // Top at 155, so gap = 155 - (100+50) = 5pt
        bbox: Some(Rect::new(0.0, 155.0, 300.0, 50.0)),
    };

    let mut tables = vec![table1, table2];
    merge_vertically_adjacent_tables(&mut tables);
    assert_eq!(tables.len(), 1, "Adjacent tables should be merged into one");
    assert_eq!(tables[0].rows.len(), 2, "Merged table should have 2 rows");
    assert_eq!(tables[0].col_count, 3);
}

#[test]
fn test_no_merge_when_gap_too_large() {
    let table1 = Table {
        rows: vec![TableRow {
            cells: vec![TableCell {
                text: "A".into(),
                spans: Vec::new(),
                colspan: 1,
                rowspan: 1,
                mcids: vec![],
                bbox: None,
                is_header: false,
            }],
            is_header: false,
        }],
        has_header: false,
        col_count: 1,
        bbox: Some(Rect::new(0.0, 100.0, 300.0, 50.0)),
    };
    let table2 = Table {
        rows: vec![TableRow {
            cells: vec![TableCell {
                text: "B".into(),
                spans: Vec::new(),
                colspan: 1,
                rowspan: 1,
                mcids: vec![],
                bbox: None,
                is_header: false,
            }],
            is_header: false,
        }],
        has_header: false,
        col_count: 1,
        // Top at 200, gap = 200 - 150 = 50pt >> ADJACENT_TABLE_MERGE_GAP
        bbox: Some(Rect::new(0.0, 200.0, 300.0, 50.0)),
    };

    let mut tables = vec![table1, table2];
    merge_vertically_adjacent_tables(&mut tables);
    assert_eq!(
        tables.len(),
        2,
        "Tables with large gap should NOT be merged"
    );
}

// ---------------------------------------------------------------
// Census / W-2 / 1099 table detection tests
// ---------------------------------------------------------------

#[test]
fn test_census_h_and_v_in_different_regions() {
    // Census-style layout: H-lines at y=100, y=50 spanning full width (x=36..576)
    // V-lines at y=500..550 at positions x=36, 117, 197, 277, 357, 437, 517, 576
    // The H and V lines DON'T physically cross (different Y regions)
    // But they should produce a table via extended grid.
    let lines = vec![
        // H-lines in the lower Y region
        make_h_line(36.0, 100.0, 540.0), // y=100, x=36..576
        make_h_line(36.0, 50.0, 540.0),  // y=50, x=36..576
        // V-lines in a completely different (higher) Y region
        make_v_line(36.0, 500.0, 50.0),  // x=36, y=500..550
        make_v_line(117.0, 500.0, 50.0), // x=117
        make_v_line(197.0, 500.0, 50.0), // x=197
        make_v_line(277.0, 500.0, 50.0), // x=277
        make_v_line(357.0, 500.0, 50.0), // x=357
        make_v_line(437.0, 500.0, 50.0), // x=437
        make_v_line(517.0, 500.0, 50.0), // x=517
        make_v_line(576.0, 500.0, 50.0), // x=576
    ];

    // Place text spans in the cells (between the H-lines).
    let spans = vec![
        create_test_span("A", 60.0, 70.0, 20.0, 10.0),
        create_test_span("B", 140.0, 70.0, 20.0, 10.0),
        create_test_span("C", 220.0, 70.0, 20.0, 10.0),
        create_test_span("D", 300.0, 70.0, 20.0, 10.0),
        create_test_span("E", 380.0, 70.0, 20.0, 10.0),
        create_test_span("F", 460.0, 70.0, 20.0, 10.0),
        create_test_span("G", 540.0, 70.0, 20.0, 10.0),
    ];

    let config = TableDetectionConfig {
        horizontal_strategy: TableStrategy::Lines,
        vertical_strategy: TableStrategy::Lines,
        min_table_cells: 2,
        min_table_columns: 2,
        ..TableDetectionConfig::default()
    };

    let tables = detect_tables_with_lines(&spans, &lines, &config);
    assert!(
        !tables.is_empty(),
        "Census layout with H/V in different Y regions should produce at least 1 table"
    );
    let table = &tables[0];
    // Should have ~7 columns (8 V-lines = 7 column spans) and 1 row (2 H-lines = 1 row)
    assert!(
        table.col_count >= 5,
        "Census table should have at least 5 columns, got {}",
        table.col_count
    );
}

#[test]
fn test_w2_grid_not_fragmented() {
    // W-2 style: V-lines at x=100,200,300 spanning y=100..700 (full form)
    // V-lines at x=350,450 spanning y=300..500 (sub-section only)
    // H-lines at y=100,200,300,400,500,600,700
    // Should produce 1 table (or a few that merge), not 5+ fragments.
    let lines = vec![
        // Full-height V-lines
        make_v_line(100.0, 100.0, 600.0), // x=100, y=100..700
        make_v_line(200.0, 100.0, 600.0), // x=200, y=100..700
        make_v_line(300.0, 100.0, 600.0), // x=300, y=100..700
        // Sub-section V-lines
        make_v_line(350.0, 300.0, 200.0), // x=350, y=300..500
        make_v_line(450.0, 300.0, 200.0), // x=450, y=300..500
        // H-lines across full width
        make_h_line(100.0, 100.0, 350.0),
        make_h_line(100.0, 200.0, 350.0),
        make_h_line(100.0, 300.0, 350.0),
        make_h_line(100.0, 400.0, 350.0),
        make_h_line(100.0, 500.0, 350.0),
        make_h_line(100.0, 600.0, 350.0),
        make_h_line(100.0, 700.0, 350.0),
    ];

    // Place text in cells
    let spans = vec![
        create_test_span("R1C1", 120.0, 150.0, 30.0, 10.0),
        create_test_span("R1C2", 220.0, 150.0, 30.0, 10.0),
        create_test_span("R2C1", 120.0, 250.0, 30.0, 10.0),
        create_test_span("R2C2", 220.0, 250.0, 30.0, 10.0),
        create_test_span("R3C1", 120.0, 350.0, 30.0, 10.0),
        create_test_span("R3C2", 220.0, 350.0, 30.0, 10.0),
        create_test_span("R4C1", 120.0, 450.0, 30.0, 10.0),
        create_test_span("R4C2", 220.0, 450.0, 30.0, 10.0),
        create_test_span("R5C1", 120.0, 550.0, 30.0, 10.0),
        create_test_span("R5C2", 220.0, 550.0, 30.0, 10.0),
        create_test_span("R6C1", 120.0, 650.0, 30.0, 10.0),
        create_test_span("R6C2", 220.0, 650.0, 30.0, 10.0),
    ];

    let config = TableDetectionConfig {
        horizontal_strategy: TableStrategy::Lines,
        vertical_strategy: TableStrategy::Lines,
        min_table_cells: 4,
        min_table_columns: 2,
        ..TableDetectionConfig::default()
    };

    let tables = detect_tables_with_lines(&spans, &lines, &config);
    assert!(
        tables.len() <= 2,
        "W-2 grid should produce at most 2 tables (not fragmented into {})",
        tables.len()
    );
    // The total row count across all tables should be reasonable (not duplicated).
    let total_filled: usize = tables
        .iter()
        .flat_map(|t| &t.rows)
        .flat_map(|r| &r.cells)
        .filter(|c| !c.text.is_empty())
        .count();
    assert!(
        total_filled >= 8,
        "W-2 tables should capture most text spans, got {}",
        total_filled
    );
}

#[test]
fn test_invoice_still_separate_tables() {
    // Invoice layout with header + main table:
    // Header V-lines at x=410,535 spanning y=71..143
    // Main table V-lines at x=22,103,490,541,589 spanning y=150..553
    // H-lines shared near y=150 (header at y=83,142; main at y=150,553)
    // Should produce 2 separate tables (header + main), not 1 merged.
    let lines = vec![
        // Header table
        make_h_line(410.0, 83.0, 125.0),  // y=83
        make_h_line(410.0, 142.0, 125.0), // y=142
        make_v_line(410.0, 71.0, 72.0),   // x=410, y=71..143
        make_v_line(535.0, 71.0, 72.0),   // x=535, y=71..143
        // Main table
        make_h_line(22.0, 150.0, 567.0),  // y=150
        make_h_line(22.0, 553.0, 567.0),  // y=553
        make_v_line(22.0, 150.0, 403.0),  // x=22, y=150..553
        make_v_line(103.0, 150.0, 403.0), // x=103, y=150..553
        make_v_line(490.0, 150.0, 403.0), // x=490, y=150..553
        make_v_line(541.0, 150.0, 403.0), // x=541, y=150..553
        make_v_line(589.0, 150.0, 403.0), // x=589, y=150..553
    ];

    // Spans in header
    let mut spans = vec![
        create_test_span("Balance Due", 420.0, 100.0, 80.0, 10.0),
        create_test_span("$500.00", 420.0, 120.0, 60.0, 10.0),
    ];
    // Spans in main table
    for i in 0..6 {
        let y = 160.0 + i as f32 * 60.0;
        spans.push(create_test_span("Date", 30.0, y, 40.0, 10.0));
        spans.push(create_test_span("Code", 110.0, y, 40.0, 10.0));
        spans.push(create_test_span("Desc", 200.0, y, 200.0, 10.0));
        spans.push(create_test_span("$100", 500.0, y, 30.0, 10.0));
    }

    let config = TableDetectionConfig {
        horizontal_strategy: TableStrategy::Lines,
        vertical_strategy: TableStrategy::Lines,
        min_table_cells: 2,
        min_table_columns: 1,
        ..TableDetectionConfig::default()
    };

    let tables = detect_tables_with_lines(&spans, &lines, &config);
    assert!(
        tables.len() >= 2,
        "Invoice should produce at least 2 separate tables (header + main), got {}",
        tables.len()
    );
}
