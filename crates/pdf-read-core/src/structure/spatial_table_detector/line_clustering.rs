use super::*;

pub(super) fn cluster_values(values: &[f32], tolerance: f32) -> Vec<f32> {
    let mut clusters: Vec<f32> = Vec::new();
    let mut counts: Vec<u32> = Vec::new();
    for &v in values {
        if let Some(idx) = clusters.iter().position(|&c| (v - c).abs() < tolerance) {
            counts[idx] += 1;
            clusters[idx] += (v - clusters[idx]) / counts[idx] as f32;
        } else {
            clusters.push(v);
            counts.push(1);
        }
    }
    clusters
}

pub(super) fn group_lines_into_clusters(
    lines: &[crate::elements::PathContent],
    config: &TableDetectionConfig,
) -> Vec<LineCluster> {
    if lines.is_empty() {
        return Vec::new();
    }
    // All clustering geometry below works on RENDERED extents: a table rule
    // encoded as a 1 pt segment with a table-height stroke width must
    // cluster with (and span) the rules its drawn bar actually touches, not
    // the ones near its geometric speck. Computed once — pure
    // arithmetic per path.
    let rendered: Vec<crate::geometry::Rect> = lines.iter().map(|p| p.rendered_bbox()).collect();
    let mut uf = UnionFind::new(lines.len());
    let mut valid_indices: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, path)| path.is_table_primitive())
        .map(|(i, _)| i)
        .collect();

    // Optimization: Sort by X-coordinate to enable sweep-line early exit (O(n log n))
    valid_indices.sort_by(|&a, &b| crate::utils::safe_float_cmp(rendered[a].x, rendered[b].x));

    const EXPANSION: f32 = 3.0;
    for i in 0..valid_indices.len() {
        let idx_a = valid_indices[i];
        let bbox_a = &rendered[idx_a];
        let expanded_a = crate::geometry::Rect::new(
            bbox_a.x - EXPANSION,
            bbox_a.y - EXPANSION,
            bbox_a.width + EXPANSION * 2.0,
            bbox_a.height + EXPANSION * 2.0,
        );

        for j in (i + 1)..valid_indices.len() {
            let idx_b = valid_indices[j];
            let bbox_b = &rendered[idx_b];

            // Optimization: If the next path's X-start is beyond our search threshold,
            // no subsequent paths in the sorted list can possibly intersect.
            if bbox_b.x > expanded_a.x + expanded_a.width {
                break;
            }

            let expanded_b = crate::geometry::Rect::new(
                bbox_b.x - EXPANSION,
                bbox_b.y - EXPANSION,
                bbox_b.width + EXPANSION * 2.0,
                bbox_b.height + EXPANSION * 2.0,
            );

            if expanded_a.intersects(&expanded_b) {
                uf.union(idx_a, idx_b);
            }
        }
    }
    let mut cluster_map: HashMap<usize, LineCluster> = HashMap::new();
    for i in valid_indices {
        let root = uf.find(i);
        let bbox = rendered[i];
        cluster_map
            .entry(root)
            .and_modify(|c| c.add(i, bbox))
            .or_insert_with(|| LineCluster::new(i, bbox));
    }

    // Post-processing: split clusters whose vertical lines occupy distinct Y-ranges.
    // This prevents a small bordered table (e.g. an invoice header) from merging
    // with a large main table that happens to be nearby vertically.
    // Deterministic order: `cluster_map` is a HashMap (per-process-randomized
    // iteration), so sort clusters by their first (smallest) line index — each
    // cluster's `lines` Vec is already ascending — to keep downstream table
    // boundary order stable across runs.
    let mut raw_clusters: Vec<LineCluster> = cluster_map.into_values().collect();
    raw_clusters.sort_by_key(|c| c.lines.first().copied().unwrap_or(usize::MAX));
    let mut result: Vec<LineCluster> = Vec::with_capacity(raw_clusters.len());
    const LINE_AXIS_TOL: f32 = 2.0;
    let v_split_gap = config.v_split_gap;

    for cluster in raw_clusters {
        // Collect Y-ranges of vertical lines in this cluster.
        let mut v_ranges: Vec<(usize, f32, f32)> = Vec::new(); // (line_idx, y_min, y_max)
        for &idx in &cluster.lines {
            let path = &lines[idx];
            if path.is_vertical_line(LINE_AXIS_TOL) && rendered[idx].height.abs() > 5.0 {
                let y_min = rendered[idx].y;
                let y_max = rendered[idx].y + rendered[idx].height;
                let (y_min, y_max) = if y_min <= y_max {
                    (y_min, y_max)
                } else {
                    (y_max, y_min)
                };
                v_ranges.push((idx, y_min, y_max));
            }
        }

        // Need at least 2 V-lines in different ranges to consider splitting.
        if v_ranges.len() < 2 {
            result.push(cluster);
            continue;
        }

        // Sort V-lines by their y_min and group into non-overlapping Y-range bands.
        v_ranges.sort_by(|a, b| crate::utils::safe_float_cmp(a.1, b.1));
        let mut bands: Vec<(f32, f32)> = Vec::new(); // merged Y-range bands
        let mut band_start = v_ranges[0].1;
        let mut band_end = v_ranges[0].2;
        for &(_, y_min, y_max) in &v_ranges[1..] {
            if y_min > band_end + v_split_gap {
                bands.push((band_start, band_end));
                band_start = y_min;
                band_end = y_max;
            } else {
                band_end = band_end.max(y_max);
            }
        }
        bands.push((band_start, band_end));

        if bands.len() < 2 {
            // All V-lines share one contiguous Y-range; no split needed.
            result.push(cluster);
            continue;
        }

        // Split: assign each line in the cluster to the band it best fits.
        let mut sub_clusters: Vec<Vec<usize>> = vec![Vec::new(); bands.len()];
        for &idx in &cluster.lines {
            let bbox = &rendered[idx];
            let line_y_mid = bbox.y + bbox.height * 0.5;
            // Find the band whose range contains (or is closest to) the line's midpoint.
            let mut best_band = 0;
            let mut best_dist = f32::MAX;
            for (bi, &(b_min, b_max)) in bands.iter().enumerate() {
                let dist = if line_y_mid >= b_min && line_y_mid <= b_max {
                    0.0
                } else {
                    (line_y_mid - b_min).abs().min((line_y_mid - b_max).abs())
                };
                if dist < best_dist {
                    best_dist = dist;
                    best_band = bi;
                }
            }
            sub_clusters[best_band].push(idx);
        }

        // Build LineCluster from each non-empty sub-cluster.
        for sub in sub_clusters {
            if sub.is_empty() {
                continue;
            }
            let first_bbox = rendered[sub[0]];
            let mut lc = LineCluster::new(sub[0], first_bbox);
            for &idx in &sub[1..] {
                lc.add(idx, rendered[idx]);
            }
            result.push(lc);
        }
    }

    result
}

/// (C) WS0.3b — detect a header text row sitting just ABOVE the grid's top
/// ruling (a header boxed only by the page-top, or unruled above the grid).
///
/// `row_ys` is sorted descending (`row_ys[0]` = the grid's top edge) and
/// `col_xs` sorted ascending (the detected column boundaries). If a tight band
/// immediately above the top ruling — up to ~1.5x the median grid row height —
/// holds text whose cells align to the EXISTING columns and that both spans
/// at least 2 distinct columns and reaches across at least half the extent, this
/// returns the y of a new top boundary that brackets exactly that header row.
/// Otherwise `None` (table unchanged).
///
/// The distinct-columns + horizontal-span gates keep a centred title or a
/// left-aligned caption above an already-correct table from being mistaken for
/// a header row (their words cluster together instead of spanning the columns),
/// so this never alters a table that already detects correctly.
pub(super) fn detect_header_row_above(
    spans: &[TextSpan],
    row_ys: &[f32],
    col_xs: &[f32],
) -> Option<f32> {
    /// Header band reaches at most this multiple of the median row height above
    /// the grid's top ruling.
    const WINDOW_ROWS: f32 = 1.5;
    /// Header cells must reach across at least this fraction of the column
    /// extent (rejects titles/captions whose words cluster together).
    const MIN_SPAN_FRAC: f32 = 0.5;

    if row_ys.len() < 2 || col_xs.len() < 2 {
        return None;
    }
    let grid_top = row_ys[0];
    let col_lo = *col_xs.first().unwrap();
    let col_hi = *col_xs.last().unwrap();
    let col_extent = col_hi - col_lo;
    if col_extent <= 0.0 {
        return None;
    }

    // Median grid row height from consecutive H-ruling gaps.
    let mut gaps: Vec<f32> = row_ys.windows(2).map(|w| (w[0] - w[1]).abs()).collect();
    gaps.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
    let median_row_h = gaps[gaps.len() / 2];
    if median_row_h <= 0.0 {
        return None;
    }
    let window_top = grid_top + WINDOW_ROWS * median_row_h;

    // Gather aligned candidate spans in the band immediately above the top ruling.
    let mut cols_hit: Vec<usize> = Vec::new();
    let mut header_max_top = f32::NEG_INFINITY;
    let mut cx_min = f32::INFINITY;
    let mut cx_max = f32::NEG_INFINITY;
    let mut cy_min = f32::INFINITY;
    let mut cy_max = f32::NEG_INFINITY;
    for span in spans {
        let cy = span.bbox.center().y;
        if cy <= grid_top || cy > window_top {
            continue;
        }
        let cx = span.bbox.center().x;
        if cx < col_lo || cx > col_hi {
            continue; // outside the column extent → not aligned to a column
        }
        let Some(ci) = (0..col_xs.len() - 1).find(|&c| cx >= col_xs[c] && cx <= col_xs[c + 1])
        else {
            continue;
        };
        if !cols_hit.contains(&ci) {
            cols_hit.push(ci);
        }
        header_max_top = header_max_top.max(span.bbox.y + span.bbox.height);
        cx_min = cx_min.min(cx);
        cx_max = cx_max.max(cx);
        cy_min = cy_min.min(cy);
        cy_max = cy_max.max(cy);
    }

    // Gate: >= 2 distinct aligned columns, spanning >= half the column extent,
    // and forming a single tight row.
    if cols_hit.len() < 2 {
        return None;
    }
    if (cx_max - cx_min) < MIN_SPAN_FRAC * col_extent {
        return None;
    }
    if (cy_max - cy_min) > median_row_h {
        return None;
    }

    // New top boundary sits just above the header text so its row band brackets
    // exactly the header spans.
    Some(header_max_top + 1.0)
}

pub(super) fn detect_tables_in_cluster(
    spans: &[TextSpan],
    all_lines: &[crate::elements::PathContent],
    cluster: &LineCluster,
    config: &TableDetectionConfig,
) -> Vec<Table> {
    const MIN_LINE_LENGTH: f32 = 5.0;
    const LINE_AXIS_TOL: f32 = 2.0;
    let mut h_ys: Vec<f32> = Vec::new();
    let mut v_xs: Vec<f32> = Vec::new();
    for &idx in &cluster.lines {
        let path = &all_lines[idx];
        // Rendered extents: a stroke-width-encoded rule's center and length
        // come from the drawn bar, not the geometric speck.
        let bbox = path.rendered_bbox();
        if path.is_horizontal_line(LINE_AXIS_TOL) && bbox.width > MIN_LINE_LENGTH {
            h_ys.push(bbox.center().y);
        }
        if path.is_vertical_line(LINE_AXIS_TOL) && bbox.height.abs() > MIN_LINE_LENGTH {
            v_xs.push(bbox.center().x);
        }
    }
    let mut row_ys = cluster_values(&h_ys, config.row_tolerance);
    let mut col_xs = cluster_values(&v_xs, config.column_tolerance);
    if row_ys.len() < 2 || col_xs.len() < 2 {
        return Vec::new();
    }
    row_ys.sort_by(|a, b| crate::utils::safe_float_cmp(*b, *a));
    col_xs.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
    // (C) WS0.3b — include a header text row sitting just ABOVE the grid's top
    // ruling (boxed by page-top / unruled above the grid) when its cells align
    // to the already-detected columns. Adds ONE row boundary above the top and
    // widens the span-assignment region to reach it; never adds columns. Returns
    // `None` (unchanged) unless the tight gate in `detect_header_row_above` holds.
    let mut assign_bbox = cluster.bbox;
    let mut inserted_header_row = false;
    if let Some(header_top) = detect_header_row_above(spans, &row_ys, &col_xs) {
        row_ys.insert(0, header_top);
        inserted_header_row = true;
        let new_height = (header_top - assign_bbox.y).max(assign_bbox.height);
        assign_bbox =
            crate::geometry::Rect::new(assign_bbox.x, assign_bbox.y, assign_bbox.width, new_height);
    }
    let num_rows = row_ys.len() - 1;
    let num_cols = col_xs.len() - 1;
    if num_cols < config.min_table_columns || num_cols > config.max_table_columns {
        return Vec::new();
    }
    let mut cells: Vec<Vec<Vec<usize>>> = vec![vec![Vec::new(); num_cols]; num_rows];
    let mut assigned_any = false;
    for (orig_idx, span) in spans.iter().enumerate() {
        if !assign_bbox.intersects(&span.bbox) {
            continue;
        }
        let cx = span.bbox.center().x;
        let cy = span.bbox.center().y;
        let row_idx = (0..num_rows).find(|&r| cy <= row_ys[r] && cy >= row_ys[r + 1]);
        let col_idx = (0..num_cols).find(|&c| cx >= col_xs[c] && cx <= col_xs[c + 1]);
        if let (Some(r), Some(c)) = (row_idx, col_idx) {
            cells[r][c].push(orig_idx);
            assigned_any = true;
        }
    }
    if !assigned_any {
        return Vec::new();
    }
    let columns: Vec<ColumnCluster> = (0..num_cols)
        .map(|c| ColumnCluster {
            x_center: (col_xs[c] + col_xs[c + 1]) / 2.0,
            x_min: col_xs[c],
            x_max: col_xs[c + 1],
            span_indices: Vec::new(),
        })
        .collect();
    let all_rows: Vec<RowCluster> = (0..num_rows)
        .map(|r| RowCluster {
            y_center: (row_ys[r] + row_ys[r + 1]) / 2.0,
            y_min: row_ys[r + 1],
            y_max: row_ys[r],
            span_indices: Vec::new(),
        })
        .collect();
    let grid_full = GridStructure {
        columns: columns.clone(),
        rows: all_rows.clone(),
        cells: cells.clone(),
    };
    let mut tables = Vec::new();
    let mut current_start_row = 0;
    while current_start_row < num_rows {
        if grid_full.is_row_empty(current_start_row) {
            current_start_row += 1;
            continue;
        }
        let mut current_end_row = current_start_row;
        while current_end_row < num_rows {
            if grid_full.is_row_empty(current_end_row) {
                break;
            }
            current_end_row += 1;
        }
        if current_end_row > current_start_row {
            let sub_cells = cells[current_start_row..current_end_row].to_vec();
            let sub_rows = all_rows[current_start_row..current_end_row].to_vec();
            let mut grid = GridStructure {
                columns: columns.clone(),
                rows: sub_rows,
                cells: sub_cells,
            };
            grid = grid.trim_empty_columns();
            if validate_table_structure_internal(&grid, config) {
                // (C) WS0.3b — the inserted header row (global row 0) lives in the
                // first non-empty run, so it is local row 0 of this sub-table when
                // `current_start_row == 0`. Protect it from colspan merging.
                let protected_header_rows =
                    usize::from(inserted_header_row && current_start_row == 0);
                let mut table = grid_to_table(
                    &grid,
                    spans,
                    Some(detect_merged_cells_visually(
                        &grid,
                        spans,
                        cluster,
                        all_lines,
                        protected_header_rows,
                    )),
                );
                let mut min_y = f32::INFINITY;
                let mut max_y = f32::NEG_INFINITY;
                for r in &grid.rows {
                    min_y = min_y.min(r.y_min);
                    max_y = max_y.max(r.y_max);
                }
                table.bbox = Some(crate::geometry::Rect::new(
                    cluster.bbox.x,
                    min_y,
                    cluster.bbox.width,
                    max_y - min_y,
                ));
                let mut header_rows_detected = 0;
                let table_width = cluster.bbox.width;
                for r in 0..table.rows.len().min(3) {
                    let row_bottom = grid.rows[r].y_min;
                    let has_separator = cluster.lines.iter().any(|&idx| {
                        let path = &all_lines[idx];
                        let rendered = path.rendered_bbox();
                        path.is_horizontal_line(LINE_AXIS_TOL)
                            && rendered.width > table_width * 0.8
                            && (rendered.center().y - row_bottom).abs() < config.row_tolerance
                    });
                    if has_separator {
                        header_rows_detected = r + 1;
                    } else if r == 0 && table.rows[r].has_colspan() {
                        header_rows_detected = 1;
                    } else {
                        break;
                    }
                }
                if header_rows_detected > 0 {
                    table.has_header = true;
                    for r in 0..header_rows_detected {
                        if r < table.rows.len() {
                            table.rows[r].is_header = true;
                            for cell in &mut table.rows[r].cells {
                                cell.is_header = true;
                            }
                        }
                    }
                }
                tables.push(table);
            }
        }
        current_start_row = current_end_row + 1;
    }
    tables
}

pub(super) fn detect_merged_cells_visually(
    grid: &GridStructure,
    spans: &[TextSpan],
    cluster: &LineCluster,
    all_lines: &[crate::elements::PathContent],
    protected_header_rows: usize,
) -> Vec<Vec<CellMergeInfo>> {
    let num_rows = grid.cells.len();
    let num_cols = grid.columns.len();
    const LINE_TOLERANCE: f32 = 2.0;
    let mut merge_info: Vec<Vec<CellMergeInfo>> = (0..num_rows)
        .map(|_| {
            (0..num_cols)
                .map(|_| CellMergeInfo {
                    colspan: 1,
                    rowspan: 1,
                    covered: false,
                })
                .collect()
        })
        .collect();
    for r in 0..num_rows {
        // (C) WS0.3b — a header row reconstructed from the unruled strip ABOVE
        // the grid has no vertical rulings in its band, which would otherwise
        // colspan-merge its distinct column cells into one (dropping every cell
        // but the first). Its cells were already verified to align to separate
        // columns, so skip colspan merging for these leading rows.
        if r < protected_header_rows {
            continue;
        }
        let mut c = 0;
        while c < num_cols {
            if merge_info[r][c].covered {
                c += 1;
                continue;
            }
            let mut colspan = 1;
            let mut cell_text_width: f32 = 0.0;
            for &idx in &grid.cells[r][c] {
                cell_text_width = cell_text_width.max(spans[idx].bbox.width);
            }
            let mut total_cell_width = grid.columns[c].x_max - grid.columns[c].x_min;
            for next_c in (c + 1)..num_cols {
                let separator_x = grid.columns[next_c].x_min;
                let y_min = grid.rows[r].y_min;
                let y_max = grid.rows[r].y_max;
                let has_separator = cluster.lines.iter().any(|&idx| {
                    let path = &all_lines[idx];
                    // Rendered extents: a stroke-width-encoded column rule
                    // crosses every row its drawn bar spans, not just the
                    // band around its geometric midline.
                    let rendered = path.rendered_bbox();
                    path.is_vertical_line(LINE_TOLERANCE)
                        && (rendered.center().x - separator_x).abs() < LINE_TOLERANCE
                        && rendered.y < y_max
                        && (rendered.y + rendered.height) > y_min
                });
                if !has_separator || (cell_text_width > total_cell_width + 2.0) {
                    colspan += 1;
                    total_cell_width += grid.columns[next_c].x_max - grid.columns[next_c].x_min;
                } else {
                    break;
                }
            }
            if colspan > 1 {
                merge_info[r][c].colspan = colspan;
                for i in 1..colspan {
                    merge_info[r][c + i as usize].covered = true;
                }
            }
            c += colspan as usize;
        }
    }
    for c in 0..num_cols {
        let mut r = 0;
        while r < num_rows {
            if merge_info[r][c].covered {
                r += 1;
                continue;
            }
            let mut rowspan = 1;
            let current_colspan = merge_info[r][c].colspan;
            for next_r in (r + 1)..num_rows {
                let separator_y = grid.rows[next_r].y_max;
                let x_min = grid.columns[c].x_min;
                let x_max = grid.columns[c + current_colspan as usize - 1].x_max;
                let has_separator = cluster.lines.iter().any(|&idx| {
                    let path = &all_lines[idx];
                    // Rendered extents, mirroring the colspan check above: a
                    // row rule encoded as a short vertical segment with a
                    // table-width stroke spans every column its drawn bar
                    // crosses.
                    let rendered = path.rendered_bbox();
                    path.is_horizontal_line(LINE_TOLERANCE)
                        && (rendered.center().y - separator_y).abs() < LINE_TOLERANCE
                        && rendered.x < x_max
                        && (rendered.x + rendered.width) > x_min
                });
                if !has_separator {
                    rowspan += 1;
                } else {
                    break;
                }
            }
            if rowspan > 1 {
                merge_info[r][c].rowspan = rowspan;
                for i in 1..rowspan {
                    merge_info[r + i as usize][c].covered = true;
                    for j in 1..current_colspan {
                        merge_info[r + i as usize][c + j as usize].covered = true;
                    }
                }
            }
            r += rowspan as usize;
        }
    }
    merge_info
}

// ---------------------------------------------------------------------------
// Intersection-based table detection (Tabula/pdfplumber/PyMuPDF pipeline)
// ---------------------------------------------------------------------------

/// Snap tolerance: parallel lines within this distance share a coordinate.
pub(super) const SNAP_TOL: f32 = 3.0;
/// Join tolerance: collinear segments within this gap are merged.
pub(super) const JOIN_TOL: f32 = 3.0;
/// Minimum edge length after merging; shorter edges are discarded.
pub(super) const MIN_EDGE_LEN: f32 = 5.0;
/// Minimum number of short segments at the same coordinate to consider them a
/// dotted/dashed line candidate.
pub(super) const DOTTED_MIN_SEGMENTS: usize = 3;
/// Minimum total span (in pt) of collinear short segments to reconstitute them
/// as a single continuous edge.
pub(super) const DOTTED_MIN_SPAN: f32 = 50.0;
/// Snap precision for grouping dotted-line segments by coordinate (0.1 pt).
pub(super) const DOTTED_COORD_SNAP: f32 = 10.0; // multiplier: coord * DOTTED_COORD_SNAP → i32 key
