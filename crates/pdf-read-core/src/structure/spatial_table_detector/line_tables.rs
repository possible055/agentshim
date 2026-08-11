use super::*;

/// Merge tables that are vertically adjacent (small gap between bottom of one
/// and top of another) and have similar column counts (difference <= `MERGE_COL_DIFF_TOLERANCE`).
/// When column counts differ, the narrower table's rows are padded with empty cells.
pub(super) fn merge_vertically_adjacent_tables(tables: &mut Vec<Table>) {
    if tables.len() < 2 {
        return;
    }

    // Sort tables by the top-Y of their bbox (highest Y first in PDF coords).
    tables.sort_by(|a, b| {
        let ay = a.bbox.map_or(f32::NEG_INFINITY, |bb| bb.top());
        let by = b.bbox.map_or(f32::NEG_INFINITY, |bb| bb.top());
        crate::utils::safe_float_cmp(ay, by)
    });

    let mut merged: Vec<Table> = Vec::new();
    for table in tables.drain(..) {
        let should_merge = merged.last().is_some_and(|prev: &Table| {
            let col_diff = (prev.col_count as isize - table.col_count as isize).unsigned_abs();
            if col_diff > MERGE_COL_DIFF_TOLERANCE {
                return false;
            }
            match (prev.bbox, table.bbox) {
                (Some(pb), Some(tb)) => {
                    // Vertical gap: distance between bottom of prev and top of current.
                    let gap = (tb.top() - pb.bottom())
                        .abs()
                        .min((pb.top() - tb.bottom()).abs());
                    gap <= ADJACENT_TABLE_MERGE_GAP
                }
                _ => false,
            }
        });

        if should_merge {
            let prev = merged.last_mut().unwrap();
            let target_cols = prev.col_count.max(table.col_count);

            // Pad existing rows in prev if the new table has more columns.
            if prev.col_count < target_cols {
                let pad = target_cols - prev.col_count;
                for row in &mut prev.rows {
                    for _ in 0..pad {
                        row.cells.push(TableCell {
                            text: String::new(),
                            spans: Vec::new(),
                            colspan: 1,
                            rowspan: 1,
                            mcids: Vec::new(),
                            bbox: None,
                            is_header: row.is_header,
                        });
                    }
                }
            }

            // Pad incoming rows if they have fewer columns.
            let mut incoming_rows = table.rows;
            if table.col_count < target_cols {
                let pad = target_cols - table.col_count;
                for row in &mut incoming_rows {
                    for _ in 0..pad {
                        row.cells.push(TableCell {
                            text: String::new(),
                            spans: Vec::new(),
                            colspan: 1,
                            rowspan: 1,
                            mcids: Vec::new(),
                            bbox: None,
                            is_header: row.is_header,
                        });
                    }
                }
            }

            prev.rows.extend(incoming_rows);
            prev.col_count = target_cols;
            // Update bbox to encompass both.
            if let (Some(pb), Some(tb)) = (prev.bbox, table.bbox) {
                let min_x = pb.left().min(tb.left());
                let min_y = pb.top().min(tb.top());
                let max_x = pb.right().max(tb.right());
                let max_y = pb.bottom().max(tb.bottom());
                prev.bbox = Some(crate::geometry::Rect::new(
                    min_x,
                    min_y,
                    max_x - min_x,
                    max_y - min_y,
                ));
            }
            prev.has_header = prev.has_header || table.has_header;
        } else {
            merged.push(table);
        }
    }

    *tables = merged;
}

/// True when the page carries vertical-ruling evidence that should route
/// table detection through the grid pipelines instead of the
/// horizontal-rule-bounded fallback.
///
/// A vertical line counts as RULING evidence only when its drawn bar
/// crosses at least TWO of the horizontal rules: a ruling vertical bounds
/// cells BETWEEN row rules, so it spans from one rule to another (a
/// stroke-width-encoded column bar crosses every row rule of its table).
/// Anything that crosses fewer says nothing about how the page's tables
/// are ruled: an isolated heavy-stroked speck (tick mark, list dash)
/// crosses nothing, and the short dash segments of a decorative dashed
/// BOX border cross at most the one rule their box happens to overlap.
/// Both were disabling the horizontal-rule fallback page-wide, which is
/// precisely what scattered booktabs tables on pages carrying a
/// dash-bordered affiliation box. Rectangles (decomposed into edges) keep
/// their pre-existing veto.
pub(super) fn has_vertical_ruling_evidence(
    lines: &[crate::elements::PathContent],
    h_edges: &[Edge],
) -> bool {
    const LINE_AXIS_TOL: f32 = 2.0;
    lines.iter().any(|path| {
        if path.is_horizontal_line(LINE_AXIS_TOL) {
            return false;
        }
        if path.is_vertical_line(LINE_AXIS_TOL) {
            let r = path.rendered_bbox();
            // Count DISTINCT rule levels crossed, not raw edges: one dashed
            // rule is several collinear edges at the same y, and a border
            // "joint" speck sitting on it would otherwise read as crossing
            // two rules while touching only one.
            let mut crossed_ys: Vec<f32> = h_edges
                .iter()
                .filter(|h| {
                    r.y <= h.coord
                        && (r.y + r.height) >= h.coord
                        && (r.x + r.width) >= h.start
                        && r.x <= h.end
                })
                .map(|h| h.coord)
                .collect();
            crossed_ys.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
            crossed_ys.dedup_by(|a, b| (*a - *b).abs() <= LINE_AXIS_TOL);
            return crossed_ys.len() >= 2;
        }
        path.is_rectangle()
    })
}

/// Detect tables in regions bounded by horizontal rules (H-lines) when no vertical
/// lines are present.  Groups H-edges by Y-position to find horizontal table
/// boundaries, then runs text-edge detection on the spans within each bounded
/// region.  This is the "H-lines define regions, text defines columns" hybrid.
pub(super) fn detect_tables_from_horizontal_rules(
    spans: &[TextSpan],
    h_edges: &[Edge],
    config: &TableDetectionConfig,
) -> Vec<Table> {
    const MIN_RULE_WIDTH: f32 = 100.0;
    const Y_SNAP: f32 = 4.0;
    // A table's boundary rules line up: booktabs top/sub-header/bottom
    // rules share their x-range to within a point (overlap/union ≈ 1.0),
    // while unrelated wide strokes — displayed-equation fraction bars,
    // decorative borders — share at most a common left margin
    // (overlap/union ≲ 0.8 even when x-starts coincide, since widths
    // differ). 0.85 splits the two populations with margin on both sides.
    const X_COHERENCE: f32 = 0.85;

    // Keep only wide H-edges.
    let wide: Vec<&Edge> = h_edges
        .iter()
        .filter(|e| (e.end - e.start) >= MIN_RULE_WIDTH)
        .collect();
    if wide.len() < 2 {
        return Vec::new();
    }

    // Group the wide edges into x-range-coherent FAMILIES and window
    // within each family, so only rules that could bound the same table
    // ever pair up. This fixes two failure shapes in one move: a page of
    // scattered fraction bars never forms a family (no fake table from
    // unrelated equation rules), and a decorative dashed border whose
    // y-position interleaves a real table's rules lands in its own family
    // instead of splitting the table's rules apart in the adjacent-pair
    // walk (which silently dropped the table on real dash-boxed pages).
    let mut uf = UnionFind::new(wide.len());
    for i in 0..wide.len() {
        for j in (i + 1)..wide.len() {
            let (a, b) = (wide[i], wide[j]);
            let overlap = a.end.min(b.end) - a.start.max(b.start);
            let union = a.end.max(b.end) - a.start.min(b.start);
            if union > 0.0 && overlap / union >= X_COHERENCE {
                uf.union(i, j);
            }
        }
    }
    let mut families: HashMap<usize, Vec<&Edge>> = HashMap::new();
    for (i, e) in wide.iter().enumerate() {
        families.entry(uf.find(i)).or_default().push(e);
    }
    // Deterministic family order (HashMap iteration is randomized).
    let mut families: Vec<Vec<&Edge>> = families.into_values().collect();
    families.sort_by(|a, b| crate::utils::safe_float_cmp(a[0].coord, b[0].coord));

    let mut tables = Vec::new();

    for family in &families {
        // Cluster this family's edges by Y-coordinate (snap within Y_SNAP).
        let mut y_coords: Vec<f32> = Vec::new();
        for e in family {
            let merged = y_coords
                .iter_mut()
                .find(|y| (e.coord - **y).abs() <= Y_SNAP);
            if merged.is_none() {
                y_coords.push(e.coord);
            }
        }
        y_coords.sort_by(|a, b| crate::utils::safe_float_cmp(*b, *a)); // descending (top first in PDF coords)

        if y_coords.len() < 2 {
            continue;
        }

        // For each cluster, compute the X-range (union of the FAMILY's
        // edges in that cluster).
        let x_range_for_y = |target_y: f32| -> (f32, f32) {
            let mut min_x = f32::MAX;
            let mut max_x = f32::MIN;
            for e in family {
                if (e.coord - target_y).abs() <= Y_SNAP {
                    if e.start < min_x {
                        min_x = e.start;
                    }
                    if e.end > max_x {
                        max_x = e.end;
                    }
                }
            }
            (min_x, max_x)
        };

        // Consider adjacent Y-pairs within the family as potential table regions.
        for pair in y_coords.windows(2) {
            let y_top = pair[0];
            let y_bot = pair[1];
            // Both H-lines must span significant width and overlap in X.
            let (x1_start, x1_end) = x_range_for_y(y_top);
            let (x2_start, x2_end) = x_range_for_y(y_bot);
            let x_overlap_start = x1_start.max(x2_start);
            let x_overlap_end = x1_end.min(x2_end);
            if x_overlap_end - x_overlap_start < MIN_RULE_WIDTH {
                continue;
            }

            // Collect spans within this Y-range and X-range (with small padding).
            let pad = 2.0;
            let mut region_spans: Vec<TextSpan> = Vec::new();
            let mut outside_width = 0.0f32;
            let mut inside_width = 0.0f32;
            for s in spans {
                let cy = s.bbox.center().y;
                if cy > y_top + pad || cy < y_bot - pad {
                    continue;
                }
                let cx = s.bbox.center().x;
                if cx >= x_overlap_start - pad && cx <= x_overlap_end + pad {
                    inside_width += s.bbox.width.max(0.0);
                    region_spans.push(s.clone());
                } else {
                    outside_width += s.bbox.width.max(0.0);
                }
            }

            if region_spans.is_empty() {
                continue;
            }

            // A pair of rules bounds a table only if the band's text is
            // horizontally CONTAINED by the rules: a table's boundary
            // rules span the rows they rule, while a fraction bar floats
            // inside surrounding math that continues to its left and
            // right (relation symbols, equation numbers). X-range-coherent
            // vinculums from an aligned multi-step derivation pass the
            // family check above, but the text spilling past the bars
            // gives them away — when a third of the band's text mass lies
            // outside the rules, they don't bound anything. (Division-free
            // so a band of zero-width spans compares 0 > 0 instead of
            // taking a NaN branch.)
            if outside_width > (outside_width + inside_width) * 0.3 {
                continue;
            }

            // Letter-spaced monospace guard: framed code and console
            // listings (zines, technical reports) draw each glyph on a
            // terminal-font grid, so the band's "words" are mostly single
            // characters whose aligned x positions look exactly like
            // column boundaries — identifiers shatter into single letters
            // (`s e g f a u l t`), addresses into single digits
            // (`0 0 : 1 4`). A real table's cells are words and numbers:
            // one-third single LETTERS or one-half single characters of
            // any kind is spread-out text, not a grid. (The digit
            // threshold is the looser of the two so genuine single-digit
            // table columns, which sit among multi-char label cells, stay
            // under it.)
            let word_count = region_spans.len();
            let mut single_any = 0usize;
            let mut single_alpha = 0usize;
            for rs in &region_spans {
                let mut chars = rs.text.trim().chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    single_any += 1;
                    if c.is_alphabetic() {
                        single_alpha += 1;
                    }
                }
            }
            if word_count > 0 && (single_alpha * 3 >= word_count || single_any * 2 >= word_count) {
                continue;
            }

            let mut detected = detect_tables_from_spans(&region_spans, config);
            tables.append(&mut detected);
        }
    }

    // Two families can bracket the same text — a dash-bordered decorative
    // box drawn around (or through) a ruled table gives both the box's
    // border family and the table's rule family a region over the same
    // rows, and each detects its own copy. Keep the TIGHTER detection when
    // two overlap: the looser region also swallows neighbouring lines
    // (footnotes, captions) as junk rows.
    let mut keep: Vec<bool> = vec![true; tables.len()];
    for i in 0..tables.len() {
        for j in (i + 1)..tables.len() {
            if !keep[i] || !keep[j] {
                continue;
            }
            let (Some(a), Some(b)) = (tables[i].bbox, tables[j].bbox) else {
                continue;
            };
            let ov_w = (a.x + a.width).min(b.x + b.width) - a.x.max(b.x);
            let ov_h = (a.y + a.height).min(b.y + b.height) - a.y.max(b.y);
            if ov_w <= 0.0 || ov_h <= 0.0 {
                continue;
            }
            let ov_area = ov_w * ov_h;
            let min_area = (a.width * a.height).min(b.width * b.height);
            if min_area > 0.0 && ov_area / min_area > 0.5 {
                // Overlapping duplicates: drop the larger (looser) one.
                if a.width * a.height >= b.width * b.height {
                    keep[i] = false;
                } else {
                    keep[j] = false;
                }
            }
        }
    }
    let mut it = keep.iter();
    tables.retain(|_| *it.next().unwrap_or(&true));

    tables
}
