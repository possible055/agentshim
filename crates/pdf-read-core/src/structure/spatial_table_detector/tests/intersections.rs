use super::*;

// ---------------------------------------------------------------
// Intersection-based detection tests
// ---------------------------------------------------------------

#[test]
fn test_intersection_basic_2x2_table() {
    // 3 H lines at y=100, y=200, y=300 spanning x=50..400
    // 3 V lines at x=50, x=200, x=400 spanning y=100..300
    // This creates a 2-row x 2-col grid.
    let lines = vec![
        make_h_line(50.0, 100.0, 350.0),  // y=100
        make_h_line(50.0, 200.0, 350.0),  // y=200
        make_h_line(50.0, 300.0, 350.0),  // y=300
        make_v_line(50.0, 100.0, 200.0),  // x=50
        make_v_line(200.0, 100.0, 200.0), // x=200
        make_v_line(400.0, 100.0, 200.0), // x=400
    ];
    // Place text spans in each cell (center of each cell).
    // Cell (row0, col0): x in [50,200], y in [100,200] -> center (125, 150)
    // Cell (row0, col1): x in [200,400], y in [100,200] -> center (300, 150)
    // Cell (row1, col0): x in [50,200], y in [200,300] -> center (125, 250)
    // Cell (row1, col1): x in [200,400], y in [200,300] -> center (300, 250)
    let spans = vec![
        create_test_span("A1", 120.0, 145.0, 20.0, 10.0),
        create_test_span("B1", 295.0, 145.0, 20.0, 10.0),
        create_test_span("A2", 120.0, 245.0, 20.0, 10.0),
        create_test_span("B2", 295.0, 245.0, 20.0, 10.0),
    ];
    let config = TableDetectionConfig {
        horizontal_strategy: TableStrategy::Lines,
        vertical_strategy: TableStrategy::Lines,
        min_table_cells: 4,
        min_table_columns: 2,
        ..TableDetectionConfig::default()
    };
    let tables = detect_tables_with_lines(&spans, &lines, &config);
    assert_eq!(tables.len(), 1, "Should detect exactly 1 table");
    let table = &tables[0];
    assert_eq!(table.rows.len(), 2, "Should have 2 rows");
    assert_eq!(table.col_count, 2, "Should have 2 columns");

    // Higher y = higher on page, so rows sorted descending by y.
    // Row at y=[200,300] (higher) comes first in display order.
    // Row at y=[100,200] (lower) comes second.
    let r0_texts: Vec<&str> = table.rows[0]
        .cells
        .iter()
        .map(|c| c.text.as_str())
        .collect();
    let r1_texts: Vec<&str> = table.rows[1]
        .cells
        .iter()
        .map(|c| c.text.as_str())
        .collect();
    assert_eq!(
        r0_texts,
        vec!["A2", "B2"],
        "Top row (higher y) should be A2, B2"
    );
    assert_eq!(
        r1_texts,
        vec!["A1", "B1"],
        "Bottom row (lower y) should be A1, B1"
    );
}

#[test]
fn test_intersection_snap_and_merge_edges() {
    // Two H edges at y=100 and y=101.5 (within SNAP_TOL=3) should snap.
    let mut edges = vec![
        Edge {
            coord: 100.0,
            start: 0.0,
            end: 50.0,
        },
        Edge {
            coord: 101.5,
            start: 0.0,
            end: 50.0,
        },
    ];
    snap_and_merge(&mut edges);
    assert_eq!(edges.len(), 1, "Snapped edges should merge into 1");
    assert!((edges[0].coord - 100.0).abs() < 0.01);
}

#[test]
fn test_intersection_join_collinear_segments() {
    // Two segments on same coord, gap of 2pt (within JOIN_TOL=3).
    let mut edges = vec![
        Edge {
            coord: 100.0,
            start: 0.0,
            end: 50.0,
        },
        Edge {
            coord: 100.0,
            start: 52.0,
            end: 100.0,
        },
    ];
    snap_and_merge(&mut edges);
    assert_eq!(edges.len(), 1, "Collinear segments within 3pt should join");
    assert!((edges[0].start - 0.0).abs() < 0.01);
    assert!((edges[0].end - 100.0).abs() < 0.01);
}

#[test]
fn test_intersection_discard_short_edges() {
    let mut edges = vec![
        Edge {
            coord: 100.0,
            start: 0.0,
            end: 4.0,
        }, // 4pt < MIN_EDGE_LEN
        Edge {
            coord: 200.0,
            start: 0.0,
            end: 50.0,
        },
    ];
    snap_and_merge(&mut edges);
    assert_eq!(edges.len(), 1, "Short edge should be discarded");
    assert!((edges[0].coord - 200.0).abs() < 0.01);
}

#[test]
fn test_intersection_find_intersections_basic() {
    let h = vec![
        Edge {
            coord: 100.0,
            start: 0.0,
            end: 200.0,
        },
        Edge {
            coord: 200.0,
            start: 0.0,
            end: 200.0,
        },
    ];
    let v = vec![
        Edge {
            coord: 50.0,
            start: 50.0,
            end: 250.0,
        },
        Edge {
            coord: 150.0,
            start: 50.0,
            end: 250.0,
        },
    ];
    let pts = find_intersections(&h, &v);
    assert_eq!(pts.len(), 4, "2 H x 2 V = 4 intersections");
}

#[test]
fn test_intersection_no_crossing_means_no_intersection() {
    // H line at y=100 spanning x=[0,50], V line at x=100 (outside H range).
    let h = vec![Edge {
        coord: 100.0,
        start: 0.0,
        end: 50.0,
    }];
    let v = vec![Edge {
        coord: 100.0,
        start: 0.0,
        end: 200.0,
    }];
    let pts = find_intersections(&h, &v);
    assert!(
        pts.is_empty(),
        "Non-crossing edges should produce no intersection"
    );
}

#[test]
fn test_intersection_build_cells() {
    let pts = vec![
        Intersection { x: 0.0, y: 0.0 },
        Intersection { x: 100.0, y: 0.0 },
        Intersection { x: 0.0, y: 100.0 },
        Intersection { x: 100.0, y: 100.0 },
    ];
    let cells = build_cells_from_intersections(&pts);
    assert_eq!(cells.len(), 1, "4 corners should produce 1 cell");
}

#[test]
fn test_intersection_group_adjacent_cells() {
    // Two horizontally adjacent cells.
    let cells = vec![
        IntersectionCell {
            x1: 0.0,
            y1: 0.0,
            x2: 100.0,
            y2: 100.0,
        },
        IntersectionCell {
            x1: 100.0,
            y1: 0.0,
            x2: 200.0,
            y2: 100.0,
        },
    ];
    let groups = group_cells_into_tables(&cells);
    assert_eq!(groups.len(), 1, "Adjacent cells should be in 1 group");
}

#[test]
fn test_intersection_separate_tables() {
    // Two cells far apart - not sharing any edge.
    let cells = vec![
        IntersectionCell {
            x1: 0.0,
            y1: 0.0,
            x2: 100.0,
            y2: 100.0,
        },
        IntersectionCell {
            x1: 500.0,
            y1: 500.0,
            x2: 600.0,
            y2: 600.0,
        },
    ];
    let groups = group_cells_into_tables(&cells);
    assert_eq!(
        groups.len(),
        2,
        "Distant cells should be in separate groups"
    );
}

#[test]
fn test_intersection_rect_decomposition() {
    // A rectangle should decompose into 4 edges.
    let lines = vec![crate::elements::PathContent::rect(10.0, 10.0, 100.0, 50.0)];
    let (h, v) = extract_edges(&lines);
    assert_eq!(h.len(), 2, "Rectangle should produce 2 horizontal edges");
    assert_eq!(v.len(), 2, "Rectangle should produce 2 vertical edges");
}

#[test]
fn test_intersection_3x3_grid_produces_4_cells() {
    // 3x3 intersection grid = 2x2 = 4 cells.
    let pts = vec![
        Intersection { x: 0.0, y: 0.0 },
        Intersection { x: 50.0, y: 0.0 },
        Intersection { x: 100.0, y: 0.0 },
        Intersection { x: 0.0, y: 50.0 },
        Intersection { x: 50.0, y: 50.0 },
        Intersection { x: 100.0, y: 50.0 },
        Intersection { x: 0.0, y: 100.0 },
        Intersection { x: 50.0, y: 100.0 },
        Intersection { x: 100.0, y: 100.0 },
    ];
    let cells = build_cells_from_intersections(&pts);
    assert_eq!(cells.len(), 4, "3x3 grid should produce 4 cells");
    let groups = group_cells_into_tables(&cells);
    assert_eq!(groups.len(), 1, "All 4 cells should form 1 table");
}

#[test]
fn test_dotted_line_reconstitution() {
    // 10 short H segments at y=300, each 3pt wide, spanning x=50..350
    // Each segment is below MIN_EDGE_LEN (5pt) so would normally be discarded.
    // The reconstitution pass should merge them into one edge from x=50 to x=350.
    let mut edges: Vec<Edge> = (0..10)
        .map(|i| Edge {
            coord: 300.0,
            start: 50.0 + i as f32 * 30.0,
            end: 53.0 + i as f32 * 30.0, // 3pt each
        })
        .collect();

    snap_and_merge(&mut edges);

    assert_eq!(
        edges.len(),
        1,
        "Dotted segments should reconstitute into 1 edge"
    );
    assert!(
        (edges[0].coord - 300.0).abs() < 0.01,
        "Reconstituted edge should be at y=300"
    );
    assert!(
        (edges[0].start - 50.0).abs() < 0.01,
        "Reconstituted edge should start at x=50"
    );
    // Last segment ends at 53 + 9*30 = 323
    assert!(
        (edges[0].end - 323.0).abs() < 0.01,
        "Reconstituted edge should end at x=323"
    );
}

#[test]
fn test_dotted_line_too_few_segments_discarded() {
    // Only 2 short segments — below DOTTED_MIN_SEGMENTS threshold.
    // Should be discarded entirely (not reconstituted, not kept individually).
    let mut edges = vec![
        Edge {
            coord: 200.0,
            start: 10.0,
            end: 13.0,
        },
        Edge {
            coord: 200.0,
            start: 20.0,
            end: 23.0,
        },
    ];
    snap_and_merge(&mut edges);
    assert!(
        edges.is_empty(),
        "Two short segments should not be reconstituted or kept"
    );
}

#[test]
fn test_dotted_line_narrow_span_discarded() {
    // 5 short segments at same coord but total span < DOTTED_MIN_SPAN (50pt).
    // Gaps between segments are > JOIN_TOL (3pt) so they won't be joined.
    let mut edges: Vec<Edge> = (0..5)
        .map(|i| Edge {
            coord: 400.0,
            start: 10.0 + i as f32 * 8.0,
            end: 13.0 + i as f32 * 8.0,
        })
        .collect();
    // span = (13+4*8) - 10 = 45 - 10 = 35pt < 50pt
    snap_and_merge(&mut edges);
    assert!(
        edges.is_empty(),
        "Short segments with narrow total span should be discarded"
    );
}

#[test]
fn test_dotted_line_mixed_with_long_edges() {
    // One long edge + several dotted segments at a different coord.
    // Both should survive.
    let mut edges = vec![
        Edge {
            coord: 100.0,
            start: 0.0,
            end: 200.0,
        }, // long, survives normally
    ];
    // Add 10 short segments at y=300
    for i in 0..10 {
        edges.push(Edge {
            coord: 300.0,
            start: 50.0 + i as f32 * 30.0,
            end: 53.0 + i as f32 * 30.0,
        });
    }
    snap_and_merge(&mut edges);
    assert_eq!(
        edges.len(),
        2,
        "Long edge + reconstituted dotted line = 2 edges"
    );
}

#[test]
fn test_join_chain_of_short_segments() {
    // 10 H segments at y=100, each 25pt wide, touching end-to-end
    // x: 0-25, 25-50, 50-75, ..., 225-250
    // Should join into 1 segment x=0..250
    let mut edges: Vec<Edge> = (0..10)
        .map(|i| Edge {
            coord: 100.0,
            start: i as f32 * 25.0,
            end: (i + 1) as f32 * 25.0,
        })
        .collect();

    snap_and_merge(&mut edges);

    assert_eq!(
        edges.len(),
        1,
        "Chain of 10 touching H segments should join into 1"
    );
    assert!(
        (edges[0].start - 0.0).abs() < 0.01,
        "Joined edge should start at 0"
    );
    assert!(
        (edges[0].end - 250.0).abs() < 0.01,
        "Joined edge should end at 250"
    );
}

#[test]
fn test_join_tiny_vertical_segments() {
    // 10 V segments at x=50, each 6pt tall, touching
    // y: 0-6, 6-12, 12-18, ..., 54-60
    // Should join into 1 segment y=0..60
    let mut edges: Vec<Edge> = (0..10)
        .map(|i| Edge {
            coord: 50.0,
            start: i as f32 * 6.0,
            end: (i + 1) as f32 * 6.0,
        })
        .collect();

    snap_and_merge(&mut edges);

    assert_eq!(
        edges.len(),
        1,
        "Chain of 10 touching V segments should join into 1"
    );
    assert!(
        (edges[0].start - 0.0).abs() < 0.01,
        "Joined edge should start at 0"
    );
    assert!(
        (edges[0].end - 60.0).abs() < 0.01,
        "Joined edge should end at 60"
    );
}

#[test]
fn test_join_segments_with_slightly_different_coords() {
    // Segments at very close but not identical coords (within SNAP_TOL)
    // should snap to the same coord and then join.
    let mut edges = vec![
        Edge {
            coord: 87.4,
            start: 36.0,
            end: 117.0,
        },
        Edge {
            coord: 87.41,
            start: 117.0,
            end: 143.0,
        },
        Edge {
            coord: 87.39,
            start: 143.0,
            end: 170.0,
        },
    ];

    snap_and_merge(&mut edges);

    assert_eq!(
        edges.len(),
        1,
        "Segments at near-identical coords should snap and join"
    );
    assert!(
        (edges[0].start - 36.0).abs() < 0.01,
        "Joined edge should start at 36"
    );
    assert!(
        (edges[0].end - 170.0).abs() < 0.01,
        "Joined edge should end at 170"
    );
}

#[test]
fn test_hybrid_line_cols_text_rows() {
    // V lines at x=50, 200, 400 (2 columns) spanning y=100..300
    // H lines at y=100 and y=300 only (top and bottom, NO middle rows)
    // This creates a single intersection-based row, but text lives at 3 Y positions.
    let lines = vec![
        make_h_line(50.0, 100.0, 350.0),  // y=100 (bottom)
        make_h_line(50.0, 300.0, 350.0),  // y=300 (top)
        make_v_line(50.0, 100.0, 200.0),  // x=50
        make_v_line(200.0, 100.0, 200.0), // x=200
        make_v_line(400.0, 100.0, 200.0), // x=400
    ];
    // Text spans at three distinct Y positions within the single row:
    //   Row 1 (y~270): "A" in col0, "B" in col1
    //   Row 2 (y~210): "C" in col0, "D" in col1
    //   Row 3 (y~150): "E" in col0, "F" in col1
    let spans = vec![
        create_test_span("A", 60.0, 265.0, 20.0, 10.0), // col0, y=270
        create_test_span("B", 210.0, 265.0, 20.0, 10.0), // col1, y=270
        create_test_span("C", 60.0, 205.0, 20.0, 10.0), // col0, y=210
        create_test_span("D", 210.0, 205.0, 20.0, 10.0), // col1, y=210
        create_test_span("E", 60.0, 145.0, 20.0, 10.0), // col0, y=150
        create_test_span("F", 210.0, 145.0, 20.0, 10.0), // col1, y=150
    ];
    let config = TableDetectionConfig {
        horizontal_strategy: TableStrategy::Lines,
        vertical_strategy: TableStrategy::Lines,
        min_table_cells: 4,
        min_table_columns: 2,
        ..TableDetectionConfig::default()
    };
    let tables = detect_tables_with_lines(&spans, &lines, &config);
    assert_eq!(tables.len(), 1, "Should detect exactly 1 table");
    let table = &tables[0];
    assert_eq!(
        table.rows.len(),
        3,
        "Should have 3 rows (split from text Y positions), got {}",
        table.rows.len()
    );
    assert_eq!(table.col_count, 2, "Should have 2 columns");

    // Rows sorted top-to-bottom (descending Y in PDF coords).
    let r0: Vec<&str> = table.rows[0]
        .cells
        .iter()
        .map(|c| c.text.as_str())
        .collect();
    let r1: Vec<&str> = table.rows[1]
        .cells
        .iter()
        .map(|c| c.text.as_str())
        .collect();
    let r2: Vec<&str> = table.rows[2]
        .cells
        .iter()
        .map(|c| c.text.as_str())
        .collect();
    assert_eq!(r0, vec!["A", "B"], "Top row should be A, B");
    assert_eq!(r1, vec!["C", "D"], "Middle row should be C, D");
    assert_eq!(r2, vec!["E", "F"], "Bottom row should be E, F");
}
