use super::*;

/// Detect page column regions from an X-projection histogram of text spans.
///
/// Builds a histogram of horizontal coverage (2pt buckets), then identifies
/// runs of empty buckets as candidate column gutters.  Only gaps wider than
/// 20pt **and** at least 4% of the total page X-extent are treated as true
/// column boundaries, preventing internal table whitespace from being
/// misidentified as page column gutters.
///
/// Returns a list of `(x_min, x_max)` column regions sorted left-to-right.
pub(super) fn detect_page_columns(spans: &[TextSpan]) -> Vec<(f32, f32)> {
    if spans.is_empty() {
        return Vec::new();
    }

    // 1. Find page X extent, excluding degenerate outliers.
    //
    // Per PDF 32000-1:2008 §8.3.2.3, user space is an infinite plane and
    // the CTM can produce arbitrarily large coordinates. The visible region
    // is defined by MediaBox/CropBox. Degenerate CTM transforms (e.g.,
    // rotated dvips pages) can produce span coordinates ~1e16 pt wide,
    // which would cause a multi-petabyte histogram allocation.
    //
    // Strategy: compute the median X center, then exclude any span whose
    // center is more than MAX_EXTENT from the median. This fixed safety
    // bound covers all standard page sizes while rejecting pathological
    // outliers; pages wider than 10,000pt fall back to single column.
    const MAX_EXTENT_FROM_MEDIAN: f32 = 5_000.0;

    let mut x_centers: Vec<f32> = spans
        .iter()
        .map(|s| s.bbox.x + s.bbox.width * 0.5)
        .collect();
    x_centers.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
    let median_x = x_centers[x_centers.len() / 2];

    let mut page_x_min = f32::MAX;
    let mut page_x_max = f32::MIN;
    for s in spans {
        let center = s.bbox.x + s.bbox.width * 0.5;
        if (center - median_x).abs() > MAX_EXTENT_FROM_MEDIAN {
            continue; // skip degenerate outlier
        }
        let left = s.bbox.x;
        let right = s.bbox.x + s.bbox.width;
        if left < page_x_min {
            page_x_min = left;
        }
        if right > page_x_max {
            page_x_max = right;
        }
    }

    if page_x_min >= page_x_max {
        // All spans were outliers or no valid extent
        return vec![(
            spans.iter().map(|s| s.bbox.x).fold(f32::MAX, f32::min),
            spans
                .iter()
                .map(|s| s.bbox.x + s.bbox.width)
                .fold(f32::MIN, f32::max),
        )];
    }

    let page_width = page_x_max - page_x_min;

    // Final safety: if width is still unreasonable after outlier filtering,
    // skip column detection entirely. Typical pages are ≤2400pt (A0).
    if page_width > 10_000.0 {
        log::warn!(
            "detect_page_columns: page_width {:.0} still exceeds safe limit after \
             outlier filtering, falling back to single column",
            page_width,
        );
        return vec![(page_x_min, page_x_max)];
    }

    // 2. Build histogram with 2pt buckets.
    let bucket_size = 2.0_f32;
    let n_buckets = ((page_width) / bucket_size).ceil() as usize + 1;
    let mut histogram = vec![0u32; n_buckets];

    for s in spans {
        let center = s.bbox.x + s.bbox.width * 0.5;
        if (center - median_x).abs() > MAX_EXTENT_FROM_MEDIAN {
            continue;
        }
        let left = s.bbox.x;
        let right = s.bbox.x + s.bbox.width;
        let b_start = ((left - page_x_min) / bucket_size).floor() as usize;
        let b_end = ((right - page_x_min) / bucket_size).ceil() as usize;
        for b in b_start..b_end.min(n_buckets) {
            histogram[b] += 1;
        }
    }

    // 3. Collect all gaps (runs of empty buckets) with their positions and widths.
    let min_gap_pt = 20.0_f32;
    let min_gap_buckets = ((min_gap_pt / bucket_size).ceil() as usize).max(1);

    struct Gap {
        start_bucket: usize,
        len_buckets: usize,
    }

    let mut gaps = Vec::new();
    let mut gap_start: Option<usize> = None;

    for (i, &count) in histogram.iter().enumerate() {
        if count == 0 {
            if gap_start.is_none() {
                gap_start = Some(i);
            }
        } else if let Some(gs) = gap_start {
            let gap_len = i - gs;
            if gap_len >= min_gap_buckets {
                gaps.push(Gap {
                    start_bucket: gs,
                    len_buckets: gap_len,
                });
            }
            gap_start = None;
        }
    }

    // 4. For each gap, determine the "immediate region" on each side
    //    (bounded by adjacent gaps or page edges).  A gap qualifies as a
    //    page column gutter only if at least one of its immediate regions
    //    contains a span wider than `min_paragraph_width` (80pt).  This
    //    prevents inter-cell whitespace in tables from being treated as
    //    column gutters.
    let min_paragraph_width = 80.0_f32;
    let qualifying_indices: Vec<usize> = (0..gaps.len())
        .filter(|&gi| {
            let gap_left_x = page_x_min + gaps[gi].start_bucket as f32 * bucket_size;
            let gap_right_x =
                page_x_min + (gaps[gi].start_bucket + gaps[gi].len_buckets) as f32 * bucket_size;

            // Left boundary: right edge of previous gap, or page_x_min.
            let left_bound = if gi > 0 {
                page_x_min
                    + (gaps[gi - 1].start_bucket + gaps[gi - 1].len_buckets) as f32 * bucket_size
            } else {
                page_x_min
            };
            // Right boundary: left edge of next gap, or page_x_max.
            let right_bound = if gi + 1 < gaps.len() {
                page_x_min + gaps[gi + 1].start_bucket as f32 * bucket_size
            } else {
                page_x_max
            };

            // Check left immediate region [left_bound, gap_left_x].
            let has_wide_left = spans.iter().any(|s| {
                let center = s.bbox.x + s.bbox.width / 2.0;
                center >= left_bound && center <= gap_left_x && s.bbox.width >= min_paragraph_width
            });

            // Check right immediate region [gap_right_x, right_bound].
            let has_wide_right = spans.iter().any(|s| {
                let center = s.bbox.x + s.bbox.width / 2.0;
                center >= gap_right_x
                    && center <= right_bound
                    && s.bbox.width >= min_paragraph_width
            });

            has_wide_left || has_wide_right
        })
        .collect();
    let qualifying_gaps: Vec<&Gap> = qualifying_indices.iter().map(|&i| &gaps[i]).collect();

    if qualifying_gaps.is_empty() {
        // No qualifying gap → single column.
        return vec![(page_x_min, page_x_max)];
    }

    // 5. Build column regions from gaps.
    let mut columns = Vec::new();

    // Find first occupied bucket.
    let first_occ = match histogram.iter().position(|&c| c > 0) {
        Some(b) => b,
        None => return Vec::new(),
    };
    let mut region_start = first_occ;

    for gap in &qualifying_gaps {
        // Close current region at the gap start.
        if gap.start_bucket > region_start {
            let x_min = page_x_min + region_start as f32 * bucket_size;
            let x_max = page_x_min + gap.start_bucket as f32 * bucket_size;
            columns.push((x_min, x_max));
        }
        region_start = gap.start_bucket + gap.len_buckets;
    }

    // Close last region.
    let last_occ = histogram
        .iter()
        .rposition(|&c| c > 0)
        .unwrap_or(n_buckets - 1);
    if region_start <= last_occ {
        let x_min = page_x_min + region_start as f32 * bucket_size;
        let x_max = page_x_min + (last_occ + 1) as f32 * bucket_size;
        columns.push((x_min, x_max));
    }

    columns
}

/// Issue #6/#5: keep only columns that carry content in a meaningful
/// fraction of rows. A real table column appears in most rows; a
/// "phantom" column produced by spaced words inside a single cell (e.g.
/// "Receiving Dock Inspection" with wide inter-word gaps) appears in
/// only one or two rows. Each column's distinct-row coverage is the
/// number of rows in which at least one of its spans falls.
///
/// Threshold: >= ceil(0.6 * num_rows), floored at 2. Phantom columns
/// (coverage 1) are removed; their spans get re-assigned to the nearest
/// surviving column downstream, rejoining the words into one cell.
pub(super) fn filter_columns_by_row_coverage(
    columns: &[ColumnCluster],
    rows: &[RowCluster],
    spans: &[TextSpan],
) -> Vec<ColumnCluster> {
    let num_rows = rows.len();
    if num_rows < 3 {
        return columns.to_vec();
    }
    // Minimum distinct rows a column must touch to be "real".
    let min_cov = (((num_rows as f32) * 0.6).ceil() as usize).max(2);

    // Pre-resolve each span's row index (nearest row center within y-extent).
    let span_row = |sidx: usize| -> Option<usize> {
        let cy = spans[sidx].bbox.center().y;
        rows.iter().position(|r| cy <= r.y_max && cy >= r.y_min)
    };

    let kept: Vec<ColumnCluster> = columns
        .iter()
        .filter(|col| {
            let mut seen: Vec<usize> = col
                .span_indices
                .iter()
                .filter_map(|&s| span_row(s))
                .collect();
            seen.sort_unstable();
            seen.dedup();
            seen.len() >= min_cov
        })
        .cloned()
        .collect();

    // Safety: never return fewer than 2 columns from here — if the
    // coverage filter would collapse the table, fall back to the
    // original columns (the caller's min-columns guard then decides).
    if kept.len() >= 2 {
        kept
    } else {
        columns.to_vec()
    }
}

pub(super) fn detect_columns(
    spans: &[TextSpan],
    column_tolerance: f32,
    merge_threshold: f32,
) -> Vec<ColumnCluster> {
    // Sort span indices by X coordinate before clustering for deterministic results.
    let mut sorted_indices: Vec<usize> = (0..spans.len()).collect();
    sorted_indices
        .sort_by(|&a, &b| crate::utils::safe_float_cmp(spans[a].bbox.left(), spans[b].bbox.left()));

    let mut columns: Vec<ColumnCluster> = Vec::new();
    for idx in sorted_indices {
        let x = spans[idx].bbox.left();
        let mut found = false;
        for col in &mut columns {
            if (x - col.x_center).abs() < column_tolerance {
                col.span_indices.push(idx);
                col.x_min = col.x_min.min(x);
                col.x_max = col.x_max.max(x);
                // Update running average so the cluster center tracks
                // the actual midpoint.
                let n = col.span_indices.len() as f32;
                col.x_center = col.x_center * ((n - 1.0) / n) + x / n;
                found = true;
                break;
            }
        }
        if !found {
            columns.push(ColumnCluster {
                x_center: x,
                x_min: x,
                x_max: x,
                span_indices: vec![idx],
            });
        }
    }

    // Sort columns by center before merge pass.
    columns.sort_by(|a, b| crate::utils::safe_float_cmp(a.x_center, b.x_center));

    // Post-clustering merge pass: merge adjacent columns whose centers are
    // within merge_threshold of each other or whose X ranges overlap.
    let mut merged: Vec<ColumnCluster> = Vec::new();
    for col in columns {
        let should_merge = merged.last().is_some_and(|prev: &ColumnCluster| {
            (col.x_center - prev.x_center).abs() < merge_threshold || col.x_min <= prev.x_max
        });
        if should_merge {
            let prev = merged.last_mut().unwrap();
            prev.x_min = prev.x_min.min(col.x_min);
            prev.x_max = prev.x_max.max(col.x_max);
            let total = prev.span_indices.len() as f32 + col.span_indices.len() as f32;
            prev.x_center = prev.x_center * (prev.span_indices.len() as f32 / total)
                + col.x_center * (col.span_indices.len() as f32 / total);
            prev.span_indices.extend(col.span_indices);
        } else {
            merged.push(col);
        }
    }

    // Final sort by x_center.
    merged.sort_by(|a, b| crate::utils::safe_float_cmp(a.x_center, b.x_center));
    merged
}

/// Text-edge column detection inspired by pdfplumber/Tabula "Stream" mode.
///
/// Instead of greedily clustering span X-centres, this approach:
/// 1. Collects left-edge and right-edge X positions of every span.
/// 2. Snaps nearby X values into clusters (within `snap_tolerance`).
/// 3. Keeps only X positions that appear in `min_row_count` or more distinct
///    text rows — these are consistent alignment edges.
/// 4. Returns [`ColumnCluster`]s whose centres sit at those surviving edges.
///
/// The resulting columns are fewer and more faithful to the visual grid of
/// forms that have no vector lines.
/// A short numeric cell: optional sign, digits with an optional single decimal
/// point, optional trailing `%`. Accepts `0.69`, `100`, `-1.2`, `52%`; rejects
/// words and identifiers. Used to recognise a borderless data grid.
pub(super) fn is_numeric_cell(t: &str) -> bool {
    if t.is_empty() || t.len() > 8 {
        return false;
    }
    let t = t.strip_suffix('%').unwrap_or(t);
    let t = t.strip_prefix(['+', '-', '\u{2212}']).unwrap_or(t);
    let mut seen_dot = false;
    let mut seen_digit = false;
    for c in t.chars() {
        match c {
            '0'..='9' => seen_digit = true,
            '.' if !seen_dot => seen_dot = true,
            _ => return false,
        }
    }
    seen_digit
}

/// True when the column centres sit on a near-constant pitch — the signature of
/// a numeric data lattice rather than prose that happened to align. Requires
/// ≥5 columns and tolerates up to two off-pitch gaps (e.g. a wider row-label
/// column at the left edge).
pub(super) fn is_regular_lattice(cols: &[ColumnCluster]) -> bool {
    if cols.len() < 5 {
        return false;
    }
    let mut centers: Vec<f32> = cols.iter().map(|c| c.x_center).collect();
    centers.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
    let gaps: Vec<f32> = centers.windows(2).map(|w| w[1] - w[0]).collect();
    let mut sorted = gaps.clone();
    sorted.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
    let median = sorted[sorted.len() / 2];
    if median <= 0.0 {
        return false;
    }
    let on_pitch = gaps
        .iter()
        .filter(|&&g| g >= median * 0.6 && g <= median * 1.6)
        .count();
    on_pitch + 2 >= gaps.len()
}

pub(super) fn detect_text_edge_columns(
    spans: &[TextSpan],
    config: &TableDetectionConfig,
) -> Vec<ColumnCluster> {
    if spans.is_empty() {
        return Vec::new();
    }

    let snap_tolerance = config.column_tolerance;
    let min_row_count: usize = 3;

    // --- 1. Collect (x, y_row_key) for left and right edges ----------
    // We bucket Y into rows using row_tolerance so that we can count
    // *distinct* rows per X cluster.
    let row_tol = config.row_tolerance;

    // Assign each span to a row id (simple greedy 1-D clustering on Y).
    let mut row_ids: Vec<usize> = Vec::with_capacity(spans.len());
    let mut row_centres: Vec<f32> = Vec::new();
    for span in spans {
        let y = span.bbox.center().y;
        let mut assigned = None;
        for (rid, rc) in row_centres.iter().enumerate() {
            if (y - rc).abs() < row_tol {
                assigned = Some(rid);
                break;
            }
        }
        match assigned {
            Some(rid) => row_ids.push(rid),
            None => {
                row_ids.push(row_centres.len());
                row_centres.push(y);
            }
        }
    }

    // --- 2. Build edge observations: (x_value, row_id) ---------------
    let mut edge_obs: Vec<(f32, usize)> = Vec::with_capacity(spans.len() * 2);
    for (i, span) in spans.iter().enumerate() {
        edge_obs.push((span.bbox.left(), row_ids[i]));
        edge_obs.push((span.bbox.right(), row_ids[i]));
    }
    // Sort by X for deterministic clustering.
    edge_obs.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));

    // --- 3. Cluster X positions (snap within tolerance) ---------------
    struct XCluster {
        x_center: f32,
        count: usize,
        rows: Vec<usize>, // row ids (may contain duplicates; we dedupe later)
    }

    let mut x_clusters: Vec<XCluster> = Vec::new();
    for &(x, rid) in &edge_obs {
        let mut found = false;
        for cl in &mut x_clusters {
            if (x - cl.x_center).abs() < snap_tolerance {
                let n = cl.count as f32;
                cl.x_center = cl.x_center * (n / (n + 1.0)) + x / (n + 1.0);
                cl.count += 1;
                cl.rows.push(rid);
                found = true;
                break;
            }
        }
        if !found {
            x_clusters.push(XCluster {
                x_center: x,
                count: 1,
                rows: vec![rid],
            });
        }
    }

    // --- 4. Filter: keep edges that appear in >= min_row_count distinct rows
    let mut edges: Vec<f32> = Vec::new();
    for cl in &mut x_clusters {
        cl.rows.sort_unstable();
        cl.rows.dedup();
        if cl.rows.len() >= min_row_count {
            edges.push(cl.x_center);
        }
    }
    edges.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));

    // Deduplicate edges that ended up very close after averaging.
    let mut deduped: Vec<f32> = Vec::new();
    for &e in &edges {
        if deduped
            .last()
            .is_some_and(|prev| (e - prev).abs() < snap_tolerance)
        {
            // merge: keep midpoint
            let prev = deduped.last_mut().unwrap();
            *prev = (*prev + e) / 2.0;
        } else {
            deduped.push(e);
        }
    }

    // --- 5. Convert surviving edges to ColumnClusters ----------------
    // Each edge becomes a column; assign spans whose left-edge is closest.
    let mut columns: Vec<ColumnCluster> = deduped
        .iter()
        .map(|&x| ColumnCluster {
            x_center: x,
            x_min: x,
            x_max: x,
            span_indices: Vec::new(),
        })
        .collect();

    if columns.is_empty() {
        return columns;
    }

    for (idx, span) in spans.iter().enumerate() {
        let sx = span.bbox.left();
        let best = columns
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| ((sx - c.x_center).abs() * 1000.0) as i32)
            .map(|(i, _)| i)
            .unwrap_or(0);
        columns[best].span_indices.push(idx);
        columns[best].x_min = columns[best].x_min.min(sx);
        columns[best].x_max = columns[best].x_max.max(sx);
    }

    // Drop columns that received no spans.
    columns.retain(|c| !c.span_indices.is_empty());

    // Final sort.
    columns.sort_by(|a, b| crate::utils::safe_float_cmp(a.x_center, b.x_center));
    columns
}

pub(super) fn detect_rows(spans: &[TextSpan], row_tolerance: f32) -> Vec<RowCluster> {
    // Sort span indices by Y coordinate before clustering for deterministic results.
    let mut sorted_indices: Vec<usize> = (0..spans.len()).collect();
    sorted_indices.sort_by(|&a, &b| {
        crate::utils::safe_float_cmp(spans[a].bbox.center().y, spans[b].bbox.center().y)
    });

    let mut rows: Vec<RowCluster> = Vec::new();
    for idx in sorted_indices {
        let y = spans[idx].bbox.center().y;
        let mut found = false;
        for row in &mut rows {
            if (y - row.y_center).abs() < row_tolerance {
                row.span_indices.push(idx);
                row.y_min = row.y_min.min(y);
                row.y_max = row.y_max.max(y);
                // Update running average (same rationale as detect_columns)
                let n = row.span_indices.len() as f32;
                row.y_center = row.y_center * ((n - 1.0) / n) + y / n;
                found = true;
                break;
            }
        }
        if !found {
            rows.push(RowCluster {
                y_center: y,
                y_min: y,
                y_max: y,
                span_indices: vec![idx],
            });
        }
    }
    rows.sort_by(|a, b| crate::utils::safe_float_cmp(b.y_center, a.y_center));
    rows
}

pub(super) fn assign_spans_to_cells(
    spans: &[TextSpan],
    columns: &[ColumnCluster],
    rows: &[RowCluster],
) -> GridStructure {
    let num_cols = columns.len();
    let num_rows = rows.len();
    let mut cells: Vec<Vec<Vec<usize>>> = vec![vec![Vec::new(); num_cols]; num_rows];
    for (idx, span) in spans.iter().enumerate() {
        let span_x = span.bbox.center().x;
        let span_y = span.bbox.center().y;
        let col_idx = columns
            .iter()
            .enumerate()
            .min_by_key(|(_, col)| ((span_x - col.x_center).abs() * 1000.0) as i32)
            .map(|(i, _)| i)
            .unwrap_or(0);
        let row_idx = rows
            .iter()
            .enumerate()
            .min_by_key(|(_, row)| ((span_y - row.y_center).abs() * 1000.0) as i32)
            .map(|(i, _)| i)
            .unwrap_or(0);
        cells[row_idx][col_idx].push(idx);
    }
    GridStructure {
        columns: columns.to_vec(),
        rows: rows.to_vec(),
        cells,
    }
}

/// Maximum number of detected columns the split-column detector can
/// analyse. Grids wider than this skip the check; extremely wide
/// candidates are rare and have other defences upstream.
pub(super) const MAX_MASK_COLUMNS: usize = 128;

/// Minimum share of modal rows a column-component must contain to
/// count as "significant" for split-detection purposes. Chosen to
/// admit the original split-flow shape (the DB10 reproducer's modal
/// rows split evenly across two halves) while avoiding obvious
/// overfitting. Heuristic, not corpus-calibrated.
pub(super) const MIN_SPLIT_GROUP_ROW_SHARE: f32 = 0.20;

pub(super) fn validate_table_structure_internal(
    grid: &GridStructure,
    config: &TableDetectionConfig,
) -> bool {
    let num_cols = grid.columns.len();
    let total_cells: usize = grid
        .cells
        .iter()
        .flat_map(|row| row.iter().take(num_cols))
        .map(|cell| if cell.is_empty() { 0 } else { 1 })
        .sum();
    if total_cells < config.min_table_cells {
        return false;
    }
    let cell_counts: Vec<usize> = grid
        .cells
        .iter()
        .map(|row| {
            row.iter()
                .take(num_cols)
                .filter(|cell| !cell.is_empty())
                .count()
        })
        .collect();
    if cell_counts.is_empty() {
        return false;
    }
    let most_common_count = *cell_counts
        .iter()
        .max_by_key(|&&count| cell_counts.iter().filter(|&&c| c == count).count())
        .unwrap_or(&0);
    if most_common_count == 0 {
        return false;
    }
    let regular_rows = cell_counts
        .iter()
        .filter(|&&count| count == most_common_count)
        .count();
    if (regular_rows as f32 / cell_counts.len() as f32) < config.regular_row_ratio {
        return false;
    }

    if has_split_modal_column_groups(grid, most_common_count) {
        return false;
    }

    true
}

/// Returns `true` when the modal rows of `grid` partition into two or
/// more disconnected column-co-occurrence components, each backed by a
/// significant share of modal rows. This signature catches "two prose
/// flows mis-clustered as one table" without rejecting hierarchical
/// tables whose modal data rows are sparse but internally connected.
///
/// The check operates only on rows whose populated-cell count equals
/// `most_common_count`. For each such row, the populated columns form
/// a co-occurrence clique. The union of those cliques forms a graph
/// over columns; its connected components are computed via bitmask
/// flood-fill. If two or more components each contain at least two
/// columns and are supported by at least `MIN_SPLIT_GROUP_ROW_SHARE`
/// of the modal rows, the grid is rejected as split-flow.
///
/// Heuristic, not corpus-calibrated.
pub(super) fn has_split_modal_column_groups(
    grid: &GridStructure,
    most_common_count: usize,
) -> bool {
    let num_cols = grid.columns.len();

    // A meaningful split needs at least 4 columns (two groups of >=2)
    // and at least 2 populated cells per modal row.
    if !(4..=MAX_MASK_COLUMNS).contains(&num_cols) || most_common_count < 2 {
        return false;
    }

    // Collect column-occupancy masks for the modal rows. Bounded by
    // `num_cols` so `most_common_count` (computed over the same bounded
    // slice upstream) and `populated` here share one column universe;
    // also keeps every `1u128 << idx` shift in range of the u128 mask.
    let modal_masks: Vec<u128> = grid
        .cells
        .iter()
        .filter_map(|row| {
            let populated = row
                .iter()
                .take(num_cols)
                .filter(|cell| !cell.is_empty())
                .count();

            if populated != most_common_count {
                return None;
            }

            let mut mask = 0u128;
            for (idx, cell) in row.iter().take(num_cols).enumerate() {
                if !cell.is_empty() {
                    mask |= 1u128 << idx;
                }
            }

            if mask.count_ones() >= 2 {
                Some(mask)
            } else {
                None
            }
        })
        .collect();

    // Need enough modal rows to make the share threshold meaningful.
    if modal_masks.len() < 4 {
        return false;
    }

    // Floor at 2 rows so a single-row outlier with a wide/narrow mask
    // can never be classified as its own "significant" component
    // — when modal_masks.len() == 4 the share alone would round to 1.
    let min_component_rows =
        (((modal_masks.len() as f32) * MIN_SPLIT_GROUP_ROW_SHARE).ceil() as usize).max(2);

    // Build column adjacency: two columns are adjacent iff they ever
    // co-occur in the same modal row.
    let mut adjacency: Vec<u128> = vec![0u128; num_cols];
    let mut active_columns: u128 = 0;

    for &mask in &modal_masks {
        active_columns |= mask;
        let mut bits = mask;
        while bits != 0 {
            let bit = bits & bits.wrapping_neg();
            let col = bit.trailing_zeros() as usize;
            adjacency[col] |= mask;
            bits &= !bit;
        }
    }

    // Walk connected components by bitmask flood-fill. Count how many
    // are "significant" (>=2 columns AND >=min_component_rows modal
    // rows containing at least one of their columns).
    let mut remaining = active_columns;
    let mut significant_components = 0usize;

    while remaining != 0 {
        let seed_bit = remaining & remaining.wrapping_neg();
        let mut component: u128 = 0;
        let mut frontier: u128 = seed_bit;

        while frontier != 0 {
            let bit = frontier & frontier.wrapping_neg();
            frontier &= !bit;

            if component & bit != 0 {
                continue;
            }

            component |= bit;
            let col = bit.trailing_zeros() as usize;
            frontier |= adjacency[col] & !component;
        }

        remaining &= !component;

        let component_cols = component.count_ones() as usize;
        let component_row_support = modal_masks
            .iter()
            .filter(|&&mask| mask & component != 0)
            .count();

        if component_cols >= 2 && component_row_support >= min_component_rows {
            significant_components += 1;
            if significant_components >= 2 {
                return true;
            }
        }
    }

    false
}
