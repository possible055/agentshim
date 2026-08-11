use super::*;

/// Extract horizontal and vertical edges from path content, decomposing rectangles.
pub(super) fn extract_edges(lines: &[crate::elements::PathContent]) -> (Vec<Edge>, Vec<Edge>) {
    const LINE_AXIS_TOL: f32 = 2.0;
    let mut h_edges: Vec<Edge> = Vec::new();
    let mut v_edges: Vec<Edge> = Vec::new();

    for path in lines {
        let bbox = &path.bbox;
        if path.is_horizontal_line(LINE_AXIS_TOL) {
            // Rendered extents so a stroke-width-encoded rule contributes
            // the edge its drawn bar covers, not its geometric speck
            // Identical to `bbox` for ordinary thin rules.
            let rendered = path.rendered_bbox();
            h_edges.push(Edge {
                coord: rendered.center().y,
                start: rendered.left(),
                end: rendered.right(),
            });
        } else if path.is_vertical_line(LINE_AXIS_TOL) {
            let rendered = path.rendered_bbox();
            v_edges.push(Edge {
                coord: rendered.center().x,
                start: rendered.top(),
                end: rendered.bottom(),
            });
        } else if path.is_rectangle() {
            // Decompose rectangle into 4 edges.
            let (l, r, t, b) = (bbox.left(), bbox.right(), bbox.top(), bbox.bottom());
            h_edges.push(Edge {
                coord: t,
                start: l,
                end: r,
            });
            h_edges.push(Edge {
                coord: b,
                start: l,
                end: r,
            });
            v_edges.push(Edge {
                coord: l,
                start: t,
                end: b,
            });
            v_edges.push(Edge {
                coord: r,
                start: t,
                end: b,
            });
        }
    }
    (h_edges, v_edges)
}

/// Snap parallel edges within `SNAP_TOL` to the same coordinate, join collinear
/// segments within `JOIN_TOL`, and discard edges shorter than `MIN_EDGE_LEN`.
pub(super) fn snap_and_merge(edges: &mut Vec<Edge>) {
    snap_edges(edges);
    join_collinear_edges(edges);
    reconstitute_dotted_lines(edges);
}

/// Phase 1: Sort edges by coord and snap nearby coordinates (within `SNAP_TOL`)
/// to the first coordinate in each group.
pub(super) fn snap_edges(edges: &mut [Edge]) {
    if edges.is_empty() {
        return;
    }
    // Sort by coord so nearby lines are adjacent.
    edges.sort_by(|a, b| crate::utils::safe_float_cmp(a.coord, b.coord));

    let mut i = 0;
    while i < edges.len() {
        let base_coord = edges[i].coord;
        let mut j = i + 1;
        while j < edges.len() && (edges[j].coord - base_coord).abs() <= SNAP_TOL {
            edges[j].coord = base_coord;
            j += 1;
        }
        i = j;
    }
}

/// Phase 2: Sort by (coord, start) and merge overlapping or adjacent collinear
/// segments into single edges.
pub(super) fn join_collinear_edges(edges: &mut Vec<Edge>) {
    if edges.is_empty() {
        return;
    }
    // Sort by coord then start so a single sweep handles chains of touching
    // segments regardless of the order they were originally collected.
    edges.sort_by(|a, b| {
        crate::utils::safe_float_cmp(a.coord, b.coord)
            .then_with(|| crate::utils::safe_float_cmp(a.start, b.start))
    });

    let mut merged: Vec<Edge> = Vec::new();
    for &edge in edges.iter() {
        // Use SNAP_TOL for the coord comparison (not f32::EPSILON) so that
        // edges whose coords were snapped from slightly different originals
        // still join correctly.
        let should_merge = merged.last().is_some_and(|prev: &Edge| {
            (prev.coord - edge.coord).abs() <= SNAP_TOL && edge.start <= prev.end + JOIN_TOL
        });
        if should_merge {
            let prev = merged.last_mut().unwrap();
            prev.end = prev.end.max(edge.end);
        } else {
            merged.push(edge);
        }
    }

    *edges = merged;
}

/// Phase 3: Group short segments (below `MIN_EDGE_LEN`) by coordinate.  When a
/// group has >= `DOTTED_MIN_SEGMENTS` members spanning >= `DOTTED_MIN_SPAN`
/// points, replace them with a single long edge. Short segments that do not
/// qualify are discarded.
pub(super) fn reconstitute_dotted_lines(edges: &mut Vec<Edge>) {
    let mut dotted_groups: HashMap<i32, Vec<Edge>> = HashMap::new();
    let mut long_edges: Vec<Edge> = Vec::new();

    for &edge in edges.iter() {
        if (edge.end - edge.start) >= MIN_EDGE_LEN {
            long_edges.push(edge);
        } else {
            let key = (edge.coord * DOTTED_COORD_SNAP).round() as i32;
            dotted_groups.entry(key).or_default().push(edge);
        }
    }

    // Iterate in sorted key order: `dotted_groups` is a HashMap (per-process-
    // randomized), and the reconstituted edges are appended to `long_edges`
    // (which becomes `*edges`), so HashMap order would leak into edge order and,
    // downstream, table-cell/region order. Sorting the snapped-coordinate keys
    // makes it deterministic.
    let mut dotted_keys: Vec<i32> = dotted_groups.keys().copied().collect();
    dotted_keys.sort_unstable();
    for key in dotted_keys {
        let segments = &dotted_groups[&key];
        if segments.len() >= DOTTED_MIN_SEGMENTS {
            let min_start = segments
                .iter()
                .map(|e| e.start)
                .min_by(|a, b| crate::utils::safe_float_cmp(*a, *b))
                .unwrap();
            let max_end = segments
                .iter()
                .map(|e| e.end)
                .max_by(|a, b| crate::utils::safe_float_cmp(*a, *b))
                .unwrap();
            let total_span = max_end - min_start;
            if total_span >= DOTTED_MIN_SPAN {
                // Use the coordinate of the first segment (they are all snapped
                // to the same value within SNAP_TOL anyway).
                long_edges.push(Edge {
                    coord: segments[0].coord,
                    start: min_start,
                    end: max_end,
                });
            }
        }
    }

    // No additional short-edge discard needed: long_edges already excludes
    // short segments that were not reconstituted.
    *edges = long_edges;
}

/// Remove orphan edges that have no plausible counterpart in the other axis.
///
/// For each H-edge, keep it only if at least one V-edge has an X coordinate
/// within the H-edge's X-range (with generous tolerance).
/// For each V-edge, keep it only if at least one H-edge has an X-range that
/// overlaps with the V-edge's X coordinate (with generous tolerance).
///
/// This is purely an X-range overlap check. The Y-axis relationship is
/// intentionally ignored because the extended grid projects V-line X positions
/// across all H-line Y positions regardless of whether they share Y ranges
/// (e.g., Census tables where H-lines and V-lines occupy different Y regions).
pub(super) fn filter_edges_by_coverage(h_edges: &mut Vec<Edge>, v_edges: &mut Vec<Edge>) {
    // Compute a generous X-axis tolerance: 50% of the total X span of all edges.
    let all_x_min = h_edges
        .iter()
        .map(|e| e.start)
        .chain(v_edges.iter().map(|e| e.coord))
        .fold(f32::INFINITY, f32::min);
    let all_x_max = h_edges
        .iter()
        .map(|e| e.end)
        .chain(v_edges.iter().map(|e| e.coord))
        .fold(f32::NEG_INFINITY, f32::max);
    let x_span = (all_x_max - all_x_min).max(1.0);
    let x_tol = x_span * 0.5;

    // Keep H-edges that have at least one V-edge whose X coord falls within
    // [start - x_tol, end + x_tol].
    h_edges.retain(|h| {
        v_edges
            .iter()
            .any(|v| v.coord >= h.start - x_tol && v.coord <= h.end + x_tol)
    });

    // Keep V-edges that have at least one H-edge whose X-range overlaps
    // with this V-edge's X coordinate (within tolerance).
    v_edges.retain(|v| {
        h_edges
            .iter()
            .any(|h| v.coord >= h.start - x_tol && v.coord <= h.end + x_tol)
    });
}

/// Find all intersection points where an H edge and a V edge actually cross.
pub(super) fn find_intersections(h_edges: &[Edge], v_edges: &[Edge]) -> Vec<Intersection> {
    let mut pts: Vec<Intersection> = Vec::new();
    for h in h_edges {
        for v in v_edges {
            // H edge spans x=[h.start, h.end] at y=h.coord
            // V edge spans y=[v.start, v.end] at x=v.coord
            if v.coord >= h.start - SNAP_TOL
                && v.coord <= h.end + SNAP_TOL
                && h.coord >= v.start - SNAP_TOL
                && h.coord <= v.end + SNAP_TOL
            {
                pts.push(Intersection {
                    x: v.coord,
                    y: h.coord,
                });
            }
        }
    }
    // Deduplicate (snap-level)
    pts.sort_by(|a, b| {
        crate::utils::safe_float_cmp(a.x, b.x).then_with(|| crate::utils::safe_float_cmp(a.y, b.y))
    });
    pts.dedup_by(|a, b| (a.x - b.x).abs() <= SNAP_TOL && (a.y - b.y).abs() <= SNAP_TOL);
    pts
}

/// Build cells from intersection points.
/// A cell exists when all four corners (x1,y1), (x2,y1), (x1,y2), (x2,y2) are present
/// and there is no intermediate intersection between them on either axis.
pub(super) fn build_cells_from_intersections(pts: &[Intersection]) -> Vec<IntersectionCell> {
    use std::collections::BTreeSet;

    // Collect unique sorted X and Y coordinates.
    let mut xs: Vec<f32> = pts.iter().map(|p| p.x).collect();
    let mut ys: Vec<f32> = pts.iter().map(|p| p.y).collect();
    xs.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
    xs.dedup_by(|a, b| (*a - *b).abs() <= SNAP_TOL);
    ys.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
    ys.dedup_by(|a, b| (*a - *b).abs() <= SNAP_TOL);

    // Build a fast lookup: quantize to (xi, yi) indices.
    let x_idx = |xv: f32| -> Option<usize> { xs.iter().position(|&c| (c - xv).abs() <= SNAP_TOL) };
    let y_idx = |yv: f32| -> Option<usize> { ys.iter().position(|&c| (c - yv).abs() <= SNAP_TOL) };

    let nx = xs.len();
    let ny = ys.len();
    // present[yi * nx + xi] = true if intersection exists
    let mut present: BTreeSet<usize> = BTreeSet::new();
    for p in pts {
        if let (Some(xi), Some(yi)) = (x_idx(p.x), y_idx(p.y)) {
            present.insert(yi * nx + xi);
        }
    }

    let has = |xi: usize, yi: usize| -> bool { present.contains(&(yi * nx + xi)) };

    let mut cells = Vec::new();
    for yi in 0..ny {
        for xi in 0..nx {
            if !has(xi, yi) {
                continue;
            }
            // Find next X with an intersection on the same Y row.
            let next_xi = ((xi + 1)..nx).find(|&nxi| has(nxi, yi));
            // Find next Y with an intersection on the same X column.
            let next_yi = ((yi + 1)..ny).find(|&nyi| has(xi, nyi));

            if let (Some(nxi), Some(nyi)) = (next_xi, next_yi) {
                // Check diagonal corner exists.
                if has(nxi, nyi) {
                    cells.push(IntersectionCell {
                        x1: xs[xi],
                        y1: ys[yi],
                        x2: xs[nxi],
                        y2: ys[nyi],
                    });
                }
            }
        }
    }
    cells
}

/// Build grid cells from the Cartesian product of H-edge Y-positions and V-edge X-positions.
///
/// This "extended grid" approach handles the case where horizontal and vertical lines
/// don't physically intersect (e.g., H-lines in a header area and V tick marks in a data
/// area). Instead of requiring actual crossings, we project every unique V-line X coordinate
/// across every unique H-line Y coordinate to create virtual grid intersections.
pub(super) fn build_extended_grid_cells(
    h_edges: &[Edge],
    v_edges: &[Edge],
) -> Vec<IntersectionCell> {
    // Collect unique Y positions from H edges (the row boundaries).
    let mut ys: Vec<f32> = h_edges.iter().map(|e| e.coord).collect();
    ys.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
    ys.dedup_by(|a, b| (*a - *b).abs() <= SNAP_TOL);

    // Collect unique X positions from V edges (the column boundaries).
    let mut xs: Vec<f32> = v_edges.iter().map(|e| e.coord).collect();
    xs.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
    xs.dedup_by(|a, b| (*a - *b).abs() <= SNAP_TOL);

    if xs.len() < 2 || ys.len() < 2 {
        return Vec::new();
    }

    // Build cells from every adjacent pair of X and Y values.
    let mut cells = Vec::new();
    for yi in 0..ys.len() - 1 {
        for xi in 0..xs.len() - 1 {
            cells.push(IntersectionCell {
                x1: xs[xi],
                y1: ys[yi],
                x2: xs[xi + 1],
                y2: ys[yi + 1],
            });
        }
    }
    cells
}

/// Group cells that share edges into tables using union-find.
pub(super) fn group_cells_into_tables(cells: &[IntersectionCell]) -> Vec<Vec<usize>> {
    if cells.is_empty() {
        return Vec::new();
    }
    let n = cells.len();
    let mut uf = UnionFind::new(n);

    // Sweep-line prune for the O(n²) edge-adjacency scan (the hot loop on dense
    // ruled pages — CFR regulatory megafiles, #26). BOTH adjacency tests below
    // require the cells' y-extents to touch within SNAP_TOL: horizontal
    // adjacency needs y1≈y1 (so cj.y1 ≤ ci.y2 + SNAP_TOL), and vertical
    // adjacency needs cj.y1 ≈ ci.y2 (also ≤ ci.y2 + SNAP_TOL). Iterating cells
    // in ascending-y1 order lets us `break` the inner loop once a candidate's
    // y1 clears ci.y2 + SNAP_TOL — every later candidate has an even larger y1
    // and cannot share an edge. We `union` by ORIGINAL index, and union is
    // order-independent, so the resulting partition is byte-identical to the
    // full O(n²) scan; only provably-non-adjacent pairs are skipped.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| crate::utils::safe_float_cmp(cells[a].y1, cells[b].y1));
    for a in 0..n {
        let i = order[a];
        let ci = &cells[i];
        let y_limit = ci.y2 + SNAP_TOL;
        for &j in order.iter().skip(a + 1) {
            let cj = &cells[j];
            if cj.y1 > y_limit {
                break; // sorted by y1 → no later cell can touch ci's y-extent
            }
            let shares_edge = // Horizontal adjacency (share a vertical edge)
                (((ci.x2 - cj.x1).abs() <= SNAP_TOL || (ci.x1 - cj.x2).abs() <= SNAP_TOL)
                    && (ci.y1 - cj.y1).abs() <= SNAP_TOL
                    && (ci.y2 - cj.y2).abs() <= SNAP_TOL)
                || // Vertical adjacency (share a horizontal edge)
                (((ci.y2 - cj.y1).abs() <= SNAP_TOL || (ci.y1 - cj.y2).abs() <= SNAP_TOL)
                    && (ci.x1 - cj.x1).abs() <= SNAP_TOL
                    && (ci.x2 - cj.x2).abs() <= SNAP_TOL);
            if shares_edge {
                uf.union(i, j);
            }
        }
    }

    // Collect groups in a DETERMINISTIC order. `groups()` returns a HashMap
    // whose iteration order is randomized per-process (Rust `RandomState`), so
    // `into_values().collect()` would yield table clusters in a different order
    // each run — leaking non-deterministic reading order on multi-table / figure
    // pages (e.g. matrix-figure pages with several detected regions). Each
    // group's `Vec` is already ascending (built `for i in 0..n`); sort the outer
    // list by each group's first (smallest) cell index so table order is stable.
    let mut groups: Vec<Vec<usize>> = uf.groups().into_values().collect();
    groups.sort_by_key(|g| g.first().copied().unwrap_or(usize::MAX));
    groups
}

/// Split table rows that contain text spans at multiple distinct Y positions into sub-rows.
///
/// This handles the hybrid case where column boundaries come from vertical lines but there
/// are no horizontal lines between individual rows. In that scenario the intersection-based
/// pipeline produces a single mega-row; this function detects multiple Y-clusters within
/// each row and splits accordingly.
pub(super) fn split_rows_by_text_positions(
    table_rows: Vec<TableRow>,
    row_cell_span_indices: &[Vec<Vec<usize>>],
    spans: &[TextSpan],
    config: &TableDetectionConfig,
) -> Vec<TableRow> {
    let mut result: Vec<TableRow> = Vec::new();

    for (row_idx, row) in table_rows.into_iter().enumerate() {
        let cell_indices = &row_cell_span_indices[row_idx];

        // Collect all Y-centers from every span in this row across all columns.
        let mut all_ys: Vec<f32> = Vec::new();
        for col_spans in cell_indices {
            for &idx in col_spans {
                if let Some(s) = spans.get(idx) {
                    all_ys.push(s.bbox.center().y);
                }
            }
        }

        if all_ys.len() <= 1 {
            // 0 or 1 span total -- nothing to split.
            result.push(row);
            continue;
        }

        // Cluster Y positions using the configured row_tolerance.
        all_ys.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        let mut y_clusters: Vec<f32> = Vec::new();
        for &y in &all_ys {
            let merged = y_clusters
                .last()
                .is_some_and(|&last| (y - last).abs() < config.row_tolerance);
            if merged {
                // Update cluster center as running average.
                let last = y_clusters.last_mut().unwrap();
                *last = (*last + y) / 2.0;
            } else {
                y_clusters.push(y);
            }
        }

        if y_clusters.len() <= 1 {
            // All spans are on the same Y line -- no split needed.
            result.push(row);
            continue;
        }

        // Sort clusters descending (higher y = top of page in PDF coords, displayed first).
        y_clusters.sort_by(|a, b| crate::utils::safe_float_cmp(*b, *a));

        let num_cols = row.cells.len();

        // Build one new row per Y-cluster.
        for &cluster_y in &y_clusters {
            let mut new_row = TableRow::new(row.is_header);
            for ci in 0..num_cols {
                // Collect spans in this cell that belong to this Y-cluster.
                let matching_indices: Vec<usize> = cell_indices[ci]
                    .iter()
                    .copied()
                    .filter(|&idx| {
                        spans
                            .get(idx)
                            .map(|s| {
                                let sy = s.bbox.center().y;
                                // Assign span to nearest cluster.
                                y_clusters
                                    .iter()
                                    .min_by_key(|&&cy| ((sy - cy).abs() * 1000.0) as i32)
                                    .is_some_and(|&nearest| (nearest - cluster_y).abs() < 0.01)
                            })
                            .unwrap_or(false)
                    })
                    .collect();

                let cell_text = extract_cell_text(&matching_indices, spans);
                let mcids: Vec<u32> = matching_indices
                    .iter()
                    .filter_map(|&idx| spans.get(idx).and_then(|s| s.mcid))
                    .collect();

                // Compute bbox from matching spans, fall back to original cell bbox.
                let cell_bbox = if matching_indices.is_empty() {
                    row.cells[ci].bbox
                } else {
                    let mut b = spans[matching_indices[0]].bbox;
                    for &idx in &matching_indices[1..] {
                        b = b.union(&spans[idx].bbox);
                    }
                    Some(b)
                };

                let cell_spans = matching_indices
                    .iter()
                    .filter_map(|&idx| spans.get(idx).cloned())
                    .collect::<Vec<_>>();

                new_row.cells.push(TableCell {
                    text: cell_text,
                    spans: cell_spans,
                    colspan: 1,
                    rowspan: 1,
                    mcids,
                    bbox: cell_bbox,
                    is_header: row.is_header,
                });
            }
            result.push(new_row);
        }
    }

    result
}

/// Strip form-template numbering artifacts and decorative separators from table rows.
///
/// PDF form templates sometimes embed single-digit numbering (e.g. "1", "5") as
/// separate text spans that get concatenated into cell text as a prefix. They also
/// use rows or cells filled with dashes/underscores as decorative separators.
/// This function:
/// 1. Removes entire rows where every cell is either empty or a lone single digit.
/// 2. Strips a leading single-digit prefix from cell text when the remainder looks
///    like real content (starts with a letter, `$`, or contains `/` or `-`).
/// 3. Clears cells that contain only dashes/underscores (decorative separators).
/// 4. Removes rows where all cells are empty after separator stripping.
pub(super) fn strip_form_numbering_artifacts(table_rows: &mut Vec<TableRow>) {
    // Phase 1: Remove rows where ALL cells are either empty or a lone single
    // digit (1-9), AND at least one cell actually contains a digit.  Rows that
    // are completely empty are left intact so the downstream empty-row splitting
    // logic can use them as table separators.
    table_rows.retain(|row| {
        let all_empty_or_digit = row.cells.iter().all(|c| {
            let t = c.text.trim();
            t.is_empty()
                || (t.len() == 1
                    && t.as_bytes()
                        .first()
                        .is_some_and(|b| b.is_ascii_digit() && *b != b'0'))
        });
        let has_digit = row.cells.iter().any(|c| {
            let t = c.text.trim();
            t.len() == 1
                && t.as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_digit() && *b != b'0')
        });
        !(all_empty_or_digit && has_digit)
    });

    // Phase 2: Strip leading single-digit prefix from individual cells.
    // Track whether any stripping occurred for Phase 3.
    // Only strip when the remainder clearly looks like form data (currency, dates,
    // codes with dashes/slashes), NOT when it could be a natural phrase like
    // "3 items".
    for row in table_rows.iter_mut() {
        let mut stripped_any = false;
        for cell in &mut row.cells {
            let text = cell.text.trim();
            if text.len() < 3 {
                continue; // Need at least digit + space + char
            }
            let bytes = text.as_bytes();
            if bytes[0].is_ascii_digit() && bytes[0] != b'0' && bytes[1] == b' ' {
                let rest = text[2..].trim_start();
                if !rest.is_empty() {
                    let first = rest.as_bytes()[0];
                    // Strip when remainder starts with '$' (currency) or starts
                    // with a digit (date like "Apr 11" won't, but codes like
                    // "12111 - ..." will), or contains '-' or '/' (dates, codes).
                    let looks_like_data = first == b'$'
                        || first.is_ascii_digit()
                        || (first.is_ascii_alphabetic()
                            && (rest.contains('-') || rest.contains('/') || rest.contains(',')));
                    if looks_like_data {
                        cell.text = rest.to_string();
                        stripped_any = true;
                    }
                }
            }
        }

        // Phase 3: In rows where prefixes were stripped, clear remaining
        // lone single-digit cells (they're the same numbering artifact
        // but had no content after the digit).
        if stripped_any {
            for cell in &mut row.cells {
                let t = cell.text.trim();
                if t.len() == 1 && t.as_bytes()[0].is_ascii_digit() {
                    cell.text.clear();
                }
            }
        }
    }

    // Phase 4: Clear cells that contain only dashes and/or underscores
    // (decorative line separators in form templates, e.g. "------", "____").
    for row in table_rows.iter_mut() {
        for cell in &mut row.cells {
            let t = cell.text.trim();
            if !t.is_empty() && t.chars().all(|c| c == '-' || c == '_') {
                cell.text.clear();
            }
        }
    }

    // Note: rows that become fully empty after Phase 4 (e.g. all-dash rows)
    // are intentionally left in place.  The downstream empty-row splitting
    // logic in detect_tables_from_intersections uses them as table separators.
}
