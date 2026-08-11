use super::*;

#[test]
fn test_two_column_table_detection() {
    // Left column: paragraph text spans at x=50..280
    let mut spans = vec![
        create_test_span("Abstract", 50.0, 700.0, 60.0, 12.0),
        create_test_span(
            "We present a novel approach to language",
            50.0,
            680.0,
            230.0,
            12.0,
        ),
        create_test_span(
            "Results show improvements across all",
            50.0,
            660.0,
            230.0,
            12.0,
        ),
        create_test_span(
            "benchmarks with significant gains on",
            50.0,
            640.0,
            230.0,
            12.0,
        ),
        create_test_span("standard evaluation metrics.", 50.0, 620.0, 180.0, 12.0),
    ];

    // Right column: a 3x3 table at x=320..550
    // Header row
    spans.push(create_test_span("Model", 320.0, 700.0, 40.0, 12.0));
    spans.push(create_test_span("F1", 420.0, 700.0, 15.0, 12.0));
    spans.push(create_test_span("Acc", 500.0, 700.0, 20.0, 12.0));
    // Data row 1
    spans.push(create_test_span("BERT", 320.0, 680.0, 30.0, 12.0));
    spans.push(create_test_span("92.4", 420.0, 680.0, 25.0, 12.0));
    spans.push(create_test_span("89.1", 500.0, 680.0, 25.0, 12.0));
    // Data row 2
    spans.push(create_test_span("GPT", 320.0, 660.0, 25.0, 12.0));
    spans.push(create_test_span("91.2", 420.0, 660.0, 25.0, 12.0));
    spans.push(create_test_span("88.3", 500.0, 660.0, 25.0, 12.0));

    let config = TableDetectionConfig::default();
    let tables = detect_tables_from_spans_column_aware(&spans, &config);

    // Should detect 1 table (the 3x3 in the right column)
    assert_eq!(
        tables.len(),
        1,
        "Should detect exactly 1 table in the right column, got {}",
        tables.len()
    );
    // The table should have 3 columns
    assert_eq!(
        tables[0].col_count, 3,
        "Table should have 3 columns, got {}",
        tables[0].col_count
    );
}

#[test]
fn test_single_column_no_regression() {
    // Standard single-column page with no tables — just flowing paragraph text
    let spans = vec![
        create_test_span("Introduction", 50.0, 700.0, 80.0, 14.0),
        create_test_span(
            "This paper presents a comprehensive study of natural language",
            50.0,
            680.0,
            450.0,
            12.0,
        ),
        create_test_span(
            "processing techniques applied to large-scale document analysis.",
            50.0,
            660.0,
            430.0,
            12.0,
        ),
        create_test_span(
            "Our approach builds on recent advances in transformer architectures",
            50.0,
            640.0,
            460.0,
            12.0,
        ),
        create_test_span(
            "and demonstrates improvements across multiple benchmarks.",
            50.0,
            620.0,
            400.0,
            12.0,
        ),
        create_test_span(
            "We evaluate our method on standard datasets and report results.",
            50.0,
            600.0,
            420.0,
            12.0,
        ),
    ];

    let config = TableDetectionConfig::default();
    let tables = detect_tables_from_spans_column_aware(&spans, &config);
    assert!(
        tables.is_empty(),
        "Single-column paragraph text should not be detected as a table, got {} table(s)",
        tables.len()
    );
}

#[test]
fn test_h_rule_bounded_text_table() {
    // Two H-lines spanning x=50..400 at y=750 and y=700 (no V-lines).
    let lines = vec![
        make_h_line(50.0, 750.0, 350.0), // top rule
        make_h_line(50.0, 700.0, 350.0), // bottom rule
    ];

    // Header row just below the top rule, then 3 data rows with aligned columns.
    let spans = vec![
        // Header row (y=740)
        create_test_span("Model", 60.0, 740.0, 50.0, 10.0),
        create_test_span("Acc", 180.0, 740.0, 30.0, 10.0),
        create_test_span("F1", 280.0, 740.0, 20.0, 10.0),
        // Data row 1 (y=728)
        create_test_span("BERT", 60.0, 728.0, 40.0, 10.0),
        create_test_span("84.6", 180.0, 728.0, 30.0, 10.0),
        create_test_span("83.4", 280.0, 728.0, 30.0, 10.0),
        // Data row 2 (y=716)
        create_test_span("GPT", 60.0, 716.0, 35.0, 10.0),
        create_test_span("82.1", 180.0, 716.0, 30.0, 10.0),
        create_test_span("81.0", 280.0, 716.0, 30.0, 10.0),
        // Data row 3 (y=704)
        create_test_span("XLNet", 60.0, 704.0, 45.0, 10.0),
        create_test_span("85.2", 180.0, 704.0, 30.0, 10.0),
        create_test_span("84.1", 280.0, 704.0, 30.0, 10.0),
    ];

    let config = TableDetectionConfig::default();
    let tables = detect_tables_with_lines(&spans, &lines, &config);
    assert!(
        !tables.is_empty(),
        "Should detect at least 1 table from text within H-line boundaries"
    );
    let table = &tables[0];
    assert!(
        table.col_count >= 3,
        "Expected at least 3 columns, got {}",
        table.col_count
    );
    assert!(
        table.rows.len() >= 3,
        "Expected at least 3 rows, got {}",
        table.rows.len()
    );
}

#[test]
fn test_split_table_at_section_dividers() {
    // Simulate a multi-section form with 3 sections separated by
    // full-width H-lines.  Each section has its OWN vertical grid lines
    // (they don't span across section boundaries).
    //
    // Section 1: y=10..40   (3 rows, H-lines at y=10,20,30,40)
    // Section 2: y=40..70   (3 rows, H-lines at y=40,50,60,70)
    // Section 3: y=70..100  (3 rows, H-lines at y=70,80,90,100)
    //
    // V-lines per section: x=10,40,70,100 but only within each section's
    // Y-range, so no V-edge crosses y=40 or y=70.

    let mut lines: Vec<crate::elements::PathContent> = Vec::new();

    // Full-width H-lines for every row boundary (y=10,20,...,100)
    for i in 0..=9 {
        let y = 10.0 + i as f32 * 10.0;
        lines.push(make_h_line(10.0, y, 90.0)); // x=10..100
    }

    // V-lines per section (NOT spanning across section dividers)
    // Section 1: y=10..40
    for &x in &[10.0, 40.0, 70.0, 100.0] {
        lines.push(make_v_line(x, 10.0, 30.0)); // y=10..40
    }
    // Section 2: y=40..70
    for &x in &[10.0, 40.0, 70.0, 100.0] {
        lines.push(make_v_line(x, 40.0, 30.0)); // y=40..70
    }
    // Section 3: y=70..100
    for &x in &[10.0, 40.0, 70.0, 100.0] {
        lines.push(make_v_line(x, 70.0, 30.0)); // y=70..100
    }

    // Place text spans in each cell (3 cols x 9 rows = 27 spans)
    let mut spans = Vec::new();
    for row in 0..9 {
        let y = 15.0 + row as f32 * 10.0;
        for col in 0..3 {
            let x = 15.0 + col as f32 * 30.0;
            let label = format!("S{}-R{}-C{}", row / 3 + 1, row % 3 + 1, col + 1);
            spans.push(create_test_span(&label, x, y, 20.0, 8.0));
        }
    }

    let config = TableDetectionConfig {
        horizontal_strategy: TableStrategy::Lines,
        vertical_strategy: TableStrategy::Lines,
        ..TableDetectionConfig::default()
    };

    let tables = detect_tables_with_lines(&spans, &lines, &config);

    // The full-width H-lines at y=40 and y=70 have no V-edges crossing
    // through them, so they should be detected as section dividers.
    // We expect 3 tables (one per section).
    assert!(
        tables.len() >= 3,
        "Expected at least 3 tables after section-divider splitting, got {}",
        tables.len()
    );
    // Each sub-table should have 3 columns.
    for (i, t) in tables.iter().enumerate() {
        assert_eq!(
            t.col_count, 3,
            "Table {} should have 3 columns, got {}",
            i, t.col_count
        );
    }
}
