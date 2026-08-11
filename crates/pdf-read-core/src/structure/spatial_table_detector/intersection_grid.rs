use super::*;

/// Detect tables from intersections of horizontal and vertical edges, then assign text.
///
/// This implements the universal pipeline used by Tabula, pdfplumber, and PyMuPDF:
/// `Edges -> Snap/Merge -> Intersections -> Cells -> Table Groups`
pub(super) fn detect_tables_from_intersections(
    spans: &[TextSpan],
    lines: &[crate::elements::PathContent],
    config: &TableDetectionConfig,
) -> Vec<Table> {
    let groups = build_grid_from_lines(lines, config);

    let mut tables = Vec::new();
    for (group_cells, xs, ys, num_cols) in &groups {
        let Some((table_rows, row_cell_span_indices)) =
            assign_spans_to_intersection_grid(group_cells, xs, ys, *num_cols, spans)
        else {
            continue;
        };
        let sub_tables = finalize_intersection_tables(
            table_rows,
            &row_cell_span_indices,
            spans,
            config,
            *num_cols,
        );
        tables.extend(sub_tables);
    }

    merge_vertically_adjacent_tables(&mut tables);

    // Post-merge: split tables at section dividers — full-width horizontal
    // lines that indicate separate form sections within a single grid.
    // Use merged H-edges (to detect full-width lines) but only snap (do NOT
    // join) V-edges — joining would merge separate per-section V-segments
    // into a single long edge, hiding the section boundary discontinuity.
    let (mut h_edges, mut v_edges) = extract_edges(lines);
    snap_and_merge(&mut h_edges);
    snap_edges(&mut v_edges); // snap only, don't join
    tables = split_tables_at_section_dividers(tables, &h_edges, &v_edges, config);

    tables
}

/// Steps 1-4: extract edges, find intersections, build cells, and group them
/// into per-table cell groups with their grid boundaries.
///
/// Returns one `(group_cells, xs, ys, num_cols)` tuple per table group.
pub(super) fn build_grid_from_lines(
    lines: &[crate::elements::PathContent],
    config: &TableDetectionConfig,
) -> Vec<(Vec<IntersectionCell>, Vec<f32>, Vec<f32>, usize)> {
    // Step 1: Extract and preprocess edges.
    let (mut h_edges, mut v_edges) = extract_edges(lines);
    snap_and_merge(&mut h_edges);
    snap_and_merge(&mut v_edges);

    if h_edges.len() < 2 || v_edges.len() < 2 {
        return Vec::new();
    }

    // Step 2: Find intersections.
    let intersections = find_intersections(&h_edges, &v_edges);

    // Step 2b: When intersections are sparse (< 4), filter out orphan edges
    // that have no plausible counterpart before building the extended grid.
    // This prevents unrelated edges (e.g., decorative lines far from the table)
    // from polluting the grid.
    if intersections.len() < 4 {
        filter_edges_by_coverage(&mut h_edges, &mut v_edges);
        if h_edges.len() < 2 || v_edges.len() < 2 {
            return Vec::new();
        }
    }

    // Step 3: Build cells.
    let cells = if intersections.len() >= 4 {
        let c = build_cells_from_intersections(&intersections);
        if c.is_empty() {
            // Lines exist but don't form real intersection cells — try extended grid.
            build_extended_grid_cells(&h_edges, &v_edges)
        } else {
            c
        }
    } else {
        // H and V lines don't physically cross (e.g. Census table: H-lines in
        // header area, V tick marks in data area). Build a virtual grid by
        // projecting all V-line X positions across all H-line Y positions.
        build_extended_grid_cells(&h_edges, &v_edges)
    };
    if cells.is_empty() {
        return Vec::new();
    }

    // Step 4: Group cells into tables and compute grid boundaries per group.
    let table_groups = group_cells_into_tables(&cells);
    let mut result = Vec::new();
    for group in &table_groups {
        let group_cells: Vec<IntersectionCell> = group.iter().map(|&i| cells[i]).collect();

        // Determine unique sorted X and Y boundaries for this table.
        let mut xs: Vec<f32> = Vec::new();
        let mut ys: Vec<f32> = Vec::new();
        for c in &group_cells {
            xs.push(c.x1);
            xs.push(c.x2);
            ys.push(c.y1);
            ys.push(c.y2);
        }
        xs.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        xs.dedup_by(|a, b| (*a - *b).abs() <= SNAP_TOL);
        ys.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        ys.dedup_by(|a, b| (*a - *b).abs() <= SNAP_TOL);

        let num_cols = if xs.len() >= 2 {
            xs.len() - 1
        } else {
            continue;
        };
        if ys.len() < 2 {
            continue;
        }

        if num_cols < config.min_table_columns || num_cols > config.max_table_columns {
            continue;
        }

        result.push((group_cells, xs, ys, num_cols));
    }
    result
}

/// Assign text spans to grid cells and build table rows with per-cell span
/// indices. Returns `None` when the grid is degenerate.
pub(super) fn assign_spans_to_intersection_grid(
    group_cells: &[IntersectionCell],
    xs: &[f32],
    ys: &[f32],
    num_cols: usize,
    spans: &[TextSpan],
) -> Option<(Vec<TableRow>, Vec<Vec<Vec<usize>>>)> {
    let num_rows = if ys.len() >= 2 {
        ys.len() - 1
    } else {
        return None;
    };

    // Map each cell to a (row, col) position.
    let col_of =
        |x: f32| -> Option<usize> { (0..num_cols).find(|&c| (xs[c] - x).abs() <= SNAP_TOL) };
    let row_of =
        |y: f32| -> Option<usize> { (0..num_rows).find(|&r| (ys[r] - y).abs() <= SNAP_TOL) };

    // Track which grid positions have cells.
    let mut grid_has_cell = vec![vec![false; num_cols]; num_rows];
    for c in group_cells {
        if let (Some(ci), Some(ri)) = (col_of(c.x1), row_of(c.y1)) {
            grid_has_cell[ri][ci] = true;
        }
    }

    // Assign text spans to grid cells based on center point. Prefer the exact
    // interval before applying snap tolerance: expanding every interval makes
    // points near an internal boundary match both neighbours, and `find()`
    // would always bias them into the earlier row or column.
    let mut grid_spans: Vec<Vec<Vec<usize>>> = vec![vec![Vec::new(); num_cols]; num_rows];
    for (idx, span) in spans.iter().enumerate() {
        let cx = span.bbox.center().x;
        let cy = span.bbox.center().y;
        let col_idx = grid_interval_for_point(cx, xs);
        let row_idx = grid_interval_for_point(cy, ys);
        if let (Some(ci), Some(ri)) = (col_idx, row_idx) {
            if grid_has_cell[ri][ci] {
                grid_spans[ri][ci].push(idx);
            }
        }
    }

    // Build rows sorted top-to-bottom for the table.
    // In PDF coordinates, higher y = higher on page, so sort rows descending by y.
    let mut row_order: Vec<usize> = (0..num_rows).collect();
    row_order.sort_by(|&a, &b| crate::utils::safe_float_cmp(ys[b], ys[a]));

    let mut table_rows = Vec::new();
    // Track span indices per cell alongside table_rows for text-based row splitting.
    let mut row_cell_span_indices: Vec<Vec<Vec<usize>>> = Vec::new();
    for &ri in &row_order {
        let mut row = TableRow::new(false);
        let mut cell_indices_for_row: Vec<Vec<usize>> = Vec::new();
        for ci in 0..num_cols {
            if !grid_has_cell[ri][ci] {
                // Still emit empty cell so column count stays consistent.
                row.cells.push(TableCell {
                    text: String::new(),
                    spans: Vec::new(),
                    colspan: 1,
                    rowspan: 1,
                    mcids: Vec::new(),
                    bbox: Some(crate::geometry::Rect::new(
                        xs[ci],
                        ys[ri],
                        xs[ci + 1] - xs[ci],
                        ys[ri + 1] - ys[ri],
                    )),
                    is_header: false,
                });
                cell_indices_for_row.push(Vec::new());
                continue;
            }
            let cell_text = extract_cell_text(&grid_spans[ri][ci], spans);
            let mcids: Vec<u32> = grid_spans[ri][ci]
                .iter()
                .filter_map(|&idx| spans.get(idx).and_then(|s| s.mcid))
                .collect();
            let cell_bbox = crate::geometry::Rect::new(
                xs[ci],
                ys[ri],
                xs[ci + 1] - xs[ci],
                ys[ri + 1] - ys[ri],
            );
            let cell_spans = grid_spans[ri][ci]
                .iter()
                .filter_map(|&idx| spans.get(idx).cloned())
                .collect::<Vec<_>>();

            row.cells.push(TableCell {
                text: cell_text,
                spans: cell_spans,
                colspan: 1,
                rowspan: 1,
                mcids,
                bbox: Some(cell_bbox),
                is_header: false,
            });
            cell_indices_for_row.push(grid_spans[ri][ci].clone());
        }
        table_rows.push(row);
        row_cell_span_indices.push(cell_indices_for_row);
    }

    Some((table_rows, row_cell_span_indices))
}

/// Return the grid interval containing `point`.
///
/// Internal boundaries are half-open and belong to the interval on their
/// right. Snap tolerance is reserved for points just outside the grid, where
/// there is no neighbouring interval to compete for ownership.
pub(super) fn grid_interval_for_point(point: f32, boundaries: &[f32]) -> Option<usize> {
    let interval_count = boundaries.len().checked_sub(1)?;
    if interval_count == 0 || !point.is_finite() {
        return None;
    }

    if point < boundaries[0] {
        return (boundaries[0] - point <= SNAP_TOL).then_some(0);
    }
    if point > boundaries[interval_count] {
        return (point - boundaries[interval_count] <= SNAP_TOL).then_some(interval_count - 1);
    }

    (0..interval_count).find(|&index| {
        point >= boundaries[index]
            && (point < boundaries[index + 1]
                || (index + 1 == interval_count && point <= boundaries[index + 1]))
    })
}

/// Row splitting, form-artifact stripping, empty-row splitting, and bbox
/// computation. Produces the final `Table` entries for one table group.
pub(super) fn finalize_intersection_tables(
    table_rows: Vec<TableRow>,
    row_cell_span_indices: &[Vec<Vec<usize>>],
    spans: &[TextSpan],
    config: &TableDetectionConfig,
    num_cols: usize,
) -> Vec<Table> {
    // Hybrid row splitting: if a row contains text spans at multiple distinct
    // Y positions (no horizontal lines between them), split into sub-rows
    // based on text Y-clustering.
    let mut table_rows =
        split_rows_by_text_positions(table_rows, row_cell_span_indices, spans, config);

    // Post-process: strip form template numbering artifacts.
    // Form templates sometimes embed single-digit numbering (e.g. "1", "5") as
    // separate text spans that get concatenated to cell text as a prefix.
    strip_form_numbering_artifacts(&mut table_rows);

    // Split on completely empty rows (same strategy as cluster-based approach).
    let mut tables = Vec::new();
    let mut sub_start = 0;
    while sub_start < table_rows.len() {
        // Skip leading empty rows.
        let row_is_empty = |r: &TableRow| r.cells.iter().all(|c| c.text.is_empty());
        if row_is_empty(&table_rows[sub_start]) {
            sub_start += 1;
            continue;
        }
        let mut sub_end = sub_start + 1;
        while sub_end < table_rows.len() && !row_is_empty(&table_rows[sub_end]) {
            sub_end += 1;
        }
        let sub_rows: Vec<TableRow> = table_rows[sub_start..sub_end].to_vec();
        let filled: usize = sub_rows
            .iter()
            .flat_map(|r| r.cells.iter())
            .filter(|c| !c.text.is_empty())
            .count();
        if filled >= config.min_table_cells {
            // Compute bbox from the cells in this sub-table.
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for r in &sub_rows {
                for c in &r.cells {
                    if let Some(b) = c.bbox {
                        min_x = min_x.min(b.left());
                        min_y = min_y.min(b.top());
                        max_x = max_x.max(b.right());
                        max_y = max_y.max(b.bottom());
                    }
                }
            }
            let sub_bbox = if min_x.is_finite() {
                Some(crate::geometry::Rect::new(
                    min_x,
                    min_y,
                    max_x - min_x,
                    max_y - min_y,
                ))
            } else {
                None
            };
            tables.push(Table {
                rows: sub_rows,
                has_header: false,
                col_count: num_cols,
                bbox: sub_bbox,
            });
        }
        sub_start = sub_end;
    }
    tables
}

/// Minimum fraction of the table width that an H-edge must span to qualify
/// as a section divider.
pub(super) const SECTION_DIVIDER_WIDTH_RATIO: f32 = 0.80;

/// Split each table at interior horizontal edges that span nearly the full
/// table width ("section dividers").  Returns a new list of tables where each
/// original table may have been broken into multiple smaller ones.
pub(super) fn split_tables_at_section_dividers(
    tables: Vec<Table>,
    h_edges: &[Edge],
    v_edges: &[Edge],
    config: &TableDetectionConfig,
) -> Vec<Table> {
    let mut result = Vec::new();
    for table in tables {
        let parts = split_table_at_section_dividers(table, h_edges, v_edges, config);
        result.extend(parts);
    }
    result
}

/// Split a single table at section divider lines.
///
/// A section divider is a full-width H-edge at a Y position where few or no
/// V-edges cross through — indicating that the vertical lines stop at that
/// boundary (separate bordered sections stacked vertically).
pub(super) fn split_table_at_section_dividers(
    table: Table,
    h_edges: &[Edge],
    v_edges: &[Edge],
    config: &TableDetectionConfig,
) -> Vec<Table> {
    let Some(bbox) = table.bbox else {
        return vec![table];
    };
    if table.rows.len() < 2 {
        return vec![table];
    }

    let table_width = bbox.right() - bbox.left();
    if table_width <= 0.0 {
        return vec![table];
    }

    // Collect Y-coordinates of H-edges that qualify as section dividers:
    // - span >= SECTION_DIVIDER_WIDTH_RATIO of the table width
    // - fall within the table's vertical range (not at the very top or bottom)
    // - few V-edges cross through that Y (sections have separate vertical grids)
    let top = bbox.top();
    let bottom = bbox.bottom();
    let margin = 2.0; // pts – ignore edges at the very top/bottom boundary

    // Count how many V-edges (within the table's X-range) cross each candidate Y.
    // V-edges have coord=X, start=minY, end=maxY.
    let table_left = bbox.left();
    let table_right = bbox.right();
    let relevant_v_edges: Vec<&Edge> = v_edges
        .iter()
        .filter(|e| e.coord >= table_left - SNAP_TOL && e.coord <= table_right + SNAP_TOL)
        .collect();

    let mut divider_ys: Vec<f32> = Vec::new();
    for edge in h_edges {
        // Edge must overlap the table's horizontal extent significantly.
        let overlap_start = edge.start.max(table_left);
        let overlap_end = edge.end.min(table_right);
        let overlap = overlap_end - overlap_start;
        if overlap < table_width * SECTION_DIVIDER_WIDTH_RATIO {
            continue;
        }
        // Edge must be interior (not the top or bottom border of the table).
        let y = edge.coord;
        if y <= top + margin || y >= bottom - margin {
            continue;
        }
        // Count V-edges that cross through this Y-coordinate (i.e., their
        // vertical span straddles it with clearance on both sides).
        let cross_margin = SNAP_TOL + 1.0;
        let crossings = relevant_v_edges
            .iter()
            .filter(|v| v.start < y - cross_margin && v.end > y + cross_margin)
            .count();
        // A true section divider has no (or very few) V-edges crossing through.
        // Regular grid row boundaries have many V-edges crossing.
        if crossings <= 1 {
            divider_ys.push(y);
        }
    }

    if divider_ys.is_empty() {
        return vec![table];
    }

    divider_ys.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
    divider_ys.dedup_by(|a, b| (*a - *b).abs() <= SNAP_TOL);

    // Find which row indices the dividers fall between.
    // A divider Y falls "between row i and row i+1" if it sits between the
    // bottom of row i and the top of row i+1 (with tolerance).
    //
    // Build a list of row bboxes (top, bottom) for matching.
    let row_bounds: Vec<Option<(f32, f32)>> = table
        .rows
        .iter()
        .map(|row| {
            let mut rmin = f32::INFINITY;
            let mut rmax = f32::NEG_INFINITY;
            for c in &row.cells {
                if let Some(b) = c.bbox {
                    rmin = rmin.min(b.top());
                    rmax = rmax.max(b.bottom());
                }
            }
            if rmin.is_finite() {
                Some((rmin, rmax))
            } else {
                None
            }
        })
        .collect();

    // Determine split-after indices: row indices after which to split.
    // A divider at Y should split after the row whose bottom (max Y) is at
    // or near that Y, OR before the row whose top (min Y) is at or near Y.
    let mut split_after: Vec<usize> = Vec::new();
    let tol = SNAP_TOL + 2.0; // generous tolerance for matching divider to row boundary
    for &dy in &divider_ys {
        // Find the row whose bottom edge is closest to dy (from above or at dy).
        let mut best_idx: Option<usize> = None;
        let mut best_dist = f32::INFINITY;
        for (i, bounds) in row_bounds.iter().enumerate() {
            if i >= table.rows.len().saturating_sub(1) {
                continue; // don't split after the last row
            }
            let Some((row_top, row_bot)) = bounds else {
                continue;
            };
            // Check if divider is near this row's bottom or near the next
            // row's top.
            let dist_to_bot = (dy - row_bot).abs();
            let dist_to_top = (dy - row_top).abs();
            let min_dist = dist_to_bot.min(dist_to_top);
            if min_dist <= tol && min_dist < best_dist {
                // Split after this row if divider is at its bottom,
                // or split after (i-1) if divider is at its top.
                if dist_to_bot <= dist_to_top {
                    best_idx = Some(i);
                } else if i > 0 {
                    best_idx = Some(i - 1);
                }
                best_dist = min_dist;
            }
        }
        if let Some(idx) = best_idx {
            split_after.push(idx);
        }
    }
    split_after.sort_unstable();
    split_after.dedup();

    if split_after.is_empty() {
        return vec![table];
    }

    // Perform the splits.
    let num_cols = table.col_count;
    let all_rows = table.rows;
    let mut sub_tables = Vec::new();
    let mut start = 0;
    for &split_idx in &split_after {
        let end = split_idx + 1;
        if end > start {
            sub_tables.push(&all_rows[start..end]);
        }
        start = end;
    }
    if start < all_rows.len() {
        sub_tables.push(&all_rows[start..]);
    }

    let mut result = Vec::new();
    for sub_rows_slice in sub_tables {
        let sub_rows: Vec<TableRow> = sub_rows_slice.to_vec();
        let filled: usize = sub_rows
            .iter()
            .flat_map(|r| r.cells.iter())
            .filter(|c| !c.text.is_empty())
            .count();
        if filled < config.min_table_cells {
            continue;
        }
        // Compute bbox for sub-table.
        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for r in &sub_rows {
            for c in &r.cells {
                if let Some(b) = c.bbox {
                    min_x = min_x.min(b.left());
                    min_y = min_y.min(b.top());
                    max_x = max_x.max(b.right());
                    max_y = max_y.max(b.bottom());
                }
            }
        }
        let sub_bbox = if min_x.is_finite() {
            Some(crate::geometry::Rect::new(
                min_x,
                min_y,
                max_x - min_x,
                max_y - min_y,
            ))
        } else {
            None
        };
        result.push(Table {
            rows: sub_rows,
            has_header: false,
            col_count: num_cols,
            bbox: sub_bbox,
        });
    }

    if result.is_empty() {
        // Don't lose data; return original if all sub-tables were too small.
        return vec![Table {
            rows: all_rows,
            has_header: false,
            col_count: num_cols,
            bbox: Some(bbox),
        }];
    }

    result
}

/// Maximum vertical gap (in points) between two table bboxes to consider them
/// adjacent and merge them into a single table.
pub(super) const ADJACENT_TABLE_MERGE_GAP: f32 = 20.0;

/// Maximum allowed column count difference for merging vertically adjacent tables.
/// Tables whose column counts differ by more than this are not merged.
pub(super) const MERGE_COL_DIFF_TOLERANCE: usize = 2;
