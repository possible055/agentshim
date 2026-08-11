use super::*;

#[test]
fn test_is_numeric_cell() {
    for ok in ["0.69", "100", "-1.2", "52%", "0", "1.00", "\u{2212}3.5"] {
        assert!(is_numeric_cell(ok), "{ok:?} should be numeric");
    }
    for no in [
        "Ours",
        "GLUE",
        "v3",
        "1e9",
        "0.6.6",
        "",
        "12345678.9",
        "p<0.05",
    ] {
        assert!(!is_numeric_cell(no), "{no:?} should NOT be numeric");
    }
}

#[test]
fn grid_interval_prefers_exact_side_near_internal_boundary() {
    let boundaries = [0.0, 40.0, 70.0, 100.0];

    assert_eq!(grid_interval_for_point(68.5, &boundaries), Some(1));
    assert_eq!(grid_interval_for_point(71.0, &boundaries), Some(2));
    assert_eq!(grid_interval_for_point(70.0, &boundaries), Some(2));
    assert_eq!(grid_interval_for_point(-2.0, &boundaries), Some(0));
    assert_eq!(grid_interval_for_point(102.0, &boundaries), Some(2));
    assert_eq!(grid_interval_for_point(104.0, &boundaries), None);
}

#[test]
fn intersection_grid_keeps_superscripts_and_boundary_text_in_their_cells() {
    let group_cells = [
        IntersectionCell {
            x1: 0.0,
            y1: 0.0,
            x2: 40.0,
            y2: 20.0,
        },
        IntersectionCell {
            x1: 40.0,
            y1: 0.0,
            x2: 70.0,
            y2: 20.0,
        },
        IntersectionCell {
            x1: 70.0,
            y1: 0.0,
            x2: 100.0,
            y2: 20.0,
        },
    ];
    let spans = vec![
        create_test_span("273", 10.0, 5.0, 27.0, 10.0),
        create_test_span("1", 37.0, 5.0, 2.0, 6.0),
        create_test_span("83", 45.0, 5.0, 21.0, 10.0),
        create_test_span("2", 66.0, 5.0, 2.0, 6.0),
        // Its center is one point into the third cell but within SNAP_TOL
        // of the second cell's right boundary.
        create_test_span("Europe", 66.0, 5.0, 10.0, 10.0),
    ];

    let (rows, _) = assign_spans_to_intersection_grid(
        &group_cells,
        &[0.0, 40.0, 70.0, 100.0],
        &[0.0, 20.0],
        3,
        &spans,
    )
    .expect("synthetic grid should be valid");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cells[0].text, "2731");
    assert_eq!(rows[0].cells[1].text, "832");
    assert_eq!(rows[0].cells[2].text, "Europe");
}

#[test]
fn test_is_regular_lattice() {
    // Regular ~20pt pitch with one wider row-label gap on the left.
    let regular: Vec<ColumnCluster> = [113.0, 150.0, 170.0, 190.0, 210.0, 230.0]
        .iter()
        .map(|&x| col_at(x))
        .collect();
    assert!(is_regular_lattice(&regular));

    // Fewer than 5 columns → not a lattice.
    let small: Vec<ColumnCluster> = [100.0, 200.0, 300.0].iter().map(|&x| col_at(x)).collect();
    assert!(!is_regular_lattice(&small));

    // Irregular gaps (prose that happened to align) → rejected.
    let irregular: Vec<ColumnCluster> = [100.0, 140.0, 320.0, 330.0, 500.0, 505.0]
        .iter()
        .map(|&x| col_at(x))
        .collect();
    assert!(!is_regular_lattice(&irregular));
}

#[test]
fn test_is_data_value() {
    for v in ["5,012", "+2%", "240", "-1.5", "1,000.50", "67", "\u{2212}3"] {
        assert!(is_data_value(v), "{v:?} should be a data value");
    }
    for w in ["FY22", "Mercury", "Body", "", "YoY", "$", "+", "Q1"] {
        assert!(!is_data_value(w), "{w:?} should NOT be a data value");
    }
}

#[test]
fn test_quality_gate_admits_numeric_table_rejects_prose_split() {
    let row = |cells: &[&str]| TableRow {
        cells: cells.iter().map(|c| prose_cell(c)).collect(),
        is_header: false,
    };
    // A dense numeric metrics table: ~all single-token numeric cells. Must
    // PASS (the prose ratio excludes data values).
    let mut numeric = Table::new();
    numeric.col_count = 8;
    for r in [
        ["Body", "FY22", "FY23", "FY24", "FY25", "YoY", "Plan", "Var"],
        [
            "Mercury Transits",
            "5,012",
            "5,210",
            "5,488",
            "5,612",
            "+2%",
            "5,600",
            "+12",
        ],
        [
            "Venus Phases",
            "1,840",
            "1,902",
            "1,975",
            "2,041",
            "+3%",
            "2,030",
            "+11",
        ],
    ] {
        numeric.rows.push(row(&r));
    }
    assert!(
        passes_spatial_quality_gate(&numeric),
        "dense numeric table must pass the spatial quality gate"
    );

    // Prose accidentally split into single-word columns must still be REJECTED.
    let mut prose = Table::new();
    prose.col_count = 6;
    for _ in 0..3 {
        prose
            .rows
            .push(row(&["the", "quick", "brown", "fox", "jumps", "over"]));
    }
    assert!(
        !passes_spatial_quality_gate(&prose),
        "word-dominated single-word split must still be rejected"
    );
}

#[test]
fn test_looks_like_cjk_prose() {
    let row = |cells: &[&str]| TableRow {
        cells: cells.iter().map(|c| prose_cell(c)).collect(),
        is_header: false,
    };
    // CJK prose mis-split into columns: a long ideograph/kana run in a cell.
    let mut prose = Table::new();
    prose.col_count = 2;
    prose.rows.push(row(&[
        "\u{30CD}\u{30B3}",
        "\u{72ED}\u{7FA9}\u{306B}\u{306F}\u{98DF}\u{8089}\u{76EE}\u{30CD}\u{30B3}\u{79D1}\u{30CD}\u{30B3}\u{5C5E}",
    ]));
    assert!(looks_like_cjk_prose(&prose));

    // A genuine CJK data table — short label + number cells — is NOT prose.
    let mut table = Table::new();
    table.col_count = 2;
    table.rows.push(row(&["\u{58F2}\u{4E0A}", "1,234"]));
    table.rows.push(row(&["\u{5229}\u{76CA}", "567"]));
    assert!(!looks_like_cjk_prose(&table));

    // Latin prose has no long CJK run.
    let mut latin = Table::new();
    latin.col_count = 2;
    latin.rows.push(row(&["Region", "North"]));
    assert!(!looks_like_cjk_prose(&latin));
}

#[test]
fn test_looks_like_bulleted_list_rejects_bullet_cells() {
    let row = |cells: &[&str]| TableRow {
        cells: cells.iter().map(|c| prose_cell(c)).collect(),
        is_header: false,
    };
    // A bulleted list mis-fused into two columns: lone bullet markers in
    // the first column → recognise as a list, not a table.
    let mut list = Table::new();
    list.col_count = 2;
    list.rows.push(row(&["\u{2022}", "Ship the API."]));
    list.rows.push(row(&["\u{2022}", "Write the docs."]));
    assert!(looks_like_bulleted_list(&list));

    // A genuine two-column data table has no lone-bullet cell.
    let mut table = Table::new();
    table.col_count = 2;
    table.rows.push(row(&["Region", "Q1"]));
    table.rows.push(row(&["North", "120"]));
    assert!(!looks_like_bulleted_list(&table));

    // A cell that merely *starts* with a dash but carries text (a value or
    // a negative number) is not a bullet marker.
    let mut dash = Table::new();
    dash.col_count = 2;
    dash.rows.push(row(&["Delta", "-12"]));
    assert!(!looks_like_bulleted_list(&dash));
}

/// #09 prose gate: a wrapped paragraph mis-split into a table — a row
/// crossing a sentence boundary ("...to 23,500. Stockout rate...") must
/// be recognised as prose and rejected.
#[test]
fn test_looks_like_prose_paragraph_detects_sentence_crossing_row() {
    let mut t = Table::new();
    t.col_count = 4;
    t.rows.push(TableRow {
        cells: vec![
            prose_cell("Total SKU count grew 15%"),
            prose_cell("quarter-over-quarter to"),
            prose_cell("23,500."),
            prose_cell("Stockout rate improved by 200 basis"),
        ],
        is_header: false,
    });
    assert!(looks_like_prose_paragraph(&t));
}

/// Caseless scripts (Bengali here) have no capital-letter signal at all,
/// so their sentence-final danda ('।') must be treated as a terminator
/// in its own right. Real-world PDFs sometimes render the danda with a
/// stray gap before it ("প্রাণী ।"), so the check must tolerate that.
#[test]
fn test_looks_like_prose_paragraph_detects_bengali_danda_crossing_row() {
    let mut t = Table::new();
    t.col_count = 4;
    t.rows.push(TableRow {
        cells: vec![
            prose_cell("বিড়াল একটি গার্হস্থ্য প্রজাতি"),
            prose_cell("স্তন্যপায়ী প্রাণী"),
            prose_cell("। এটি"),
            prose_cell("ফেলিডা পরিবারের একমাত্র গৃহপালিত প্রজাতি"),
        ],
        is_header: false,
    });
    assert!(looks_like_prose_paragraph(&t));
}

/// REGRESSION GUARD: a genuine data table (short value/label cells, no
/// sentence crossing a row) must NOT be flagged as prose.
#[test]
fn test_looks_like_prose_paragraph_keeps_real_table() {
    let mut t = Table::new();
    t.col_count = 4;
    for cells in [
        ["Zone", "Pallets stored", "11,100", "-2.5%"],
        ["A", "Utilization", "87%", "-3pp"],
        ["B", "Damage rate", "0.3%", "-0.2pp"],
    ] {
        t.rows.push(TableRow {
            cells: cells.iter().map(|c| prose_cell(c)).collect(),
            is_header: false,
        });
    }
    assert!(!looks_like_prose_paragraph(&t));
}

#[test]
fn test_line_clustering_multiple_tables() {
    let lines = vec![
        make_rect_path(10.0, 100.0, 50.0, 20.0),
        make_rect_path(10.0, 50.0, 50.0, 20.0), // Far away vertically
    ];
    let config = TableDetectionConfig::default();
    let clusters = group_lines_into_clusters(&lines, &config);
    assert_eq!(
        clusters.len(),
        2,
        "Should find 2 separate table regions with optimized clustering"
    );
}

#[test]
fn test_line_clustering_horizontal_separation() {
    let lines = vec![
        make_rect_path(10.0, 100.0, 50.0, 20.0), // Table 1: x=10..60
        make_rect_path(80.0, 100.0, 50.0, 20.0), // Table 2: x=80..130 (20pt gap)
    ];
    let config = TableDetectionConfig::default();
    let clusters = group_lines_into_clusters(&lines, &config);
    assert_eq!(
        clusters.len(),
        2,
        "Should find 2 separate table regions even if nearby horizontally"
    );
}
