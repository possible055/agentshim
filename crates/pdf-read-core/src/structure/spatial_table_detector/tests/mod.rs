use super::*;
use crate::geometry::Rect;
use crate::layout::text_block::{Color, FontWeight};

mod consolidation;
mod grid_validation;
mod intersections;
mod line_grid;
mod quality_heuristics;
mod strategies;
mod text_grid;
mod validation;

fn col_at(x: f32) -> ColumnCluster {
    ColumnCluster {
        x_center: x,
        x_min: x - 3.0,
        x_max: x + 3.0,
        span_indices: Vec::new(),
    }
}

fn prose_cell(text: &str) -> TableCell {
    TableCell {
        text: text.to_string(),
        spans: Vec::new(),
        colspan: 1,
        rowspan: 1,
        mcids: Vec::new(),
        bbox: None,
        is_header: false,
    }
}

fn create_test_span(text: &str, x: f32, y: f32, width: f32, height: f32) -> TextSpan {
    TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: text.to_string(),
        bbox: Rect::new(x, y, width, height),
        font_name: "TestFont".to_string(),
        font_size: 12.0,
        font_weight: FontWeight::Normal,
        is_italic: false,
        is_monospace: false,
        color: Color::black(),
        mcid: None,
        mcid_scope: None,
        sequence: 0,
        split_boundary_before: false,
        offset_semantic: false,
        char_spacing: 0.0,
        word_spacing: 0.0,
        horizontal_scaling: 1.0,
        primary_detected: false,
        char_widths: vec![],
        char_x_offsets: Vec::new(),
        heading_level: None,
        rotation_degrees: 0.0,
        wmode: 0,
        rtl_draw_logical: false,
    }
}

fn make_h_line(x: f32, y: f32, width: f32) -> crate::elements::PathContent {
    crate::elements::PathContent::line(x, y, x + width, y)
}

fn make_v_line(x: f32, y: f32, height: f32) -> crate::elements::PathContent {
    crate::elements::PathContent::line(x, y, x, y + height)
}

fn make_line_path(x1: f32, y1: f32, x2: f32, y2: f32) -> crate::elements::PathContent {
    crate::elements::PathContent::line(x1, y1, x2, y2)
}

fn make_rect_path(x: f32, y: f32, w: f32, h: f32) -> crate::elements::PathContent {
    crate::elements::PathContent::rect(x, y, w, h)
}

// -----------------------------------------------------------------
// validate_table_structure_internal: split-column-group tests
//
// These exercise has_split_modal_column_groups, the structural
// check that replaced the row-density gate. The detector rejects
// grids whose modal rows partition into two or more disconnected
// column-co-occurrence components. make_split_grid models the
// false-positive shape (two prose flows mis-clustered into one
// grid); make_grouped_grid models the sparse grouped-row-header
// shape from the scientific-table regression class; the real
// failure may also involve upstream column over-counting, while
// this unit fixture pins the validator-level property that sparse
// modal rows with connected populated columns are accepted.
// -----------------------------------------------------------------

/// Build a minimal GridStructure with `num_rows` rows and
/// `num_cols` columns, where every row populates exactly
/// `populated_per_row` cells (the first N columns). Numeric
/// fields of `ColumnCluster` / `RowCluster` are arbitrary —
/// `validate_table_structure_internal` reads only
/// `grid.columns.len()` and the emptiness of each cell.
fn make_uniform_grid(num_cols: usize, num_rows: usize, populated_per_row: usize) -> GridStructure {
    let columns = (0..num_cols)
        .map(|_| ColumnCluster {
            x_center: 0.0,
            x_min: 0.0,
            x_max: 0.0,
            span_indices: vec![],
        })
        .collect();
    let rows = (0..num_rows)
        .map(|_| RowCluster {
            y_center: 0.0,
            y_min: 0.0,
            y_max: 0.0,
            span_indices: vec![],
        })
        .collect();
    let cells = (0..num_rows)
        .map(|_| {
            (0..num_cols)
                .map(|c| {
                    if c < populated_per_row {
                        vec![0usize]
                    } else {
                        vec![]
                    }
                })
                .collect()
        })
        .collect();
    GridStructure {
        columns,
        rows,
        cells,
    }
}

/// Build a GridStructure modelling two adjacent text flows
/// mis-clustered into one candidate grid. The first `num_cols / 2`
/// columns form the "left flow"; the remaining columns form the
/// "right flow". Rows alternate between populating only the left
/// flow and only the right flow. All rows have the same populated
/// cardinality (`num_cols / 2`), so the regular-row-ratio gate
/// passes at 1.00. `num_cols` must be even.
fn make_split_grid(num_cols: usize, num_rows: usize) -> GridStructure {
    assert!(
        num_cols.is_multiple_of(2),
        "make_split_grid requires even num_cols"
    );
    let half = num_cols / 2;
    let columns = (0..num_cols)
        .map(|_| ColumnCluster {
            x_center: 0.0,
            x_min: 0.0,
            x_max: 0.0,
            span_indices: vec![],
        })
        .collect();
    let rows = (0..num_rows)
        .map(|_| RowCluster {
            y_center: 0.0,
            y_min: 0.0,
            y_max: 0.0,
            span_indices: vec![],
        })
        .collect();
    let cells = (0..num_rows)
        .map(|r| {
            let left_row = r % 2 == 0;
            (0..num_cols)
                .map(|c| {
                    let in_left_half = c < half;
                    if left_row == in_left_half {
                        vec![0usize]
                    } else {
                        vec![]
                    }
                })
                .collect()
        })
        .collect();
    GridStructure {
        columns,
        rows,
        cells,
    }
}

/// Build a GridStructure modelling a hierarchical scientific table.
/// `total_cols` columns; the first `group_cols` are populated only
/// in the first row of each group of `group_size` consecutive rows;
/// the remaining columns are populated in every row. Models the
/// failure shape from arxiv_2510.24670v2: grouped row-headers above
/// dense data columns. The over-counting that the maintainer
/// described occurs upstream of this fixture; here we model the
/// post-clustering grid the validator actually sees. Numeric
/// cluster fields are arbitrary, matching the convention used by
/// `make_uniform_grid` and `make_split_grid`.
fn make_grouped_grid(
    total_cols: usize,
    num_rows: usize,
    group_cols: usize,
    group_size: usize,
) -> GridStructure {
    assert!(group_cols < total_cols, "group_cols must be < total_cols");
    assert!(group_size > 0, "group_size must be positive");
    let columns = (0..total_cols)
        .map(|_| ColumnCluster {
            x_center: 0.0,
            x_min: 0.0,
            x_max: 0.0,
            span_indices: vec![],
        })
        .collect();
    let rows = (0..num_rows)
        .map(|_| RowCluster {
            y_center: 0.0,
            y_min: 0.0,
            y_max: 0.0,
            span_indices: vec![],
        })
        .collect();
    let cells = (0..num_rows)
        .map(|r| {
            let is_group_header = r % group_size == 0;
            (0..total_cols)
                .map(|c| {
                    let populated = if c < group_cols {
                        is_group_header
                    } else {
                        true
                    };
                    if populated {
                        vec![0usize]
                    } else {
                        vec![]
                    }
                })
                .collect()
        })
        .collect();
    GridStructure {
        columns,
        rows,
        cells,
    }
}

// ========================================================================
// consolidate_adjacent_table_fragments (#485 / #486 / #487 regression)
// ========================================================================

/// Build a minimal Table with a bbox and col_count for consolidation tests.
fn make_fragment(x: f32, y: f32, width: f32, height: f32, cols: u32) -> Table {
    let mut t = Table::new();
    t.bbox = Some(Rect::new(x, y, width, height));
    t.col_count = cols as usize;
    // Push one empty row so consolidation has something to extend; the
    // row count grows as fragments get merged.
    t.rows.push(TableRow::new(false));
    t
}

// ========================================================================
// cell_span_separator (#485 / #487 regression)
// ========================================================================

/// Helper to construct a TextSpan with just the fields the separator
/// rule actually reads: bbox, font_size, and text.
fn ts(text: &str, x: f32, y: f32, width: f32, fs: f32) -> TextSpan {
    TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: text.to_string(),
        bbox: Rect::new(x, y, width, fs),
        font_size: fs,
        font_name: String::new(),
        font_weight: crate::layout::text_block::FontWeight::Normal,
        color: Color::black(),
        mcid: None,
        mcid_scope: None,
        sequence: 0,
        split_boundary_before: false,
        offset_semantic: false,
        is_italic: false,
        is_monospace: false,
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
    }
}
