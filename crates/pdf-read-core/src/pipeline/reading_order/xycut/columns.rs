use super::*;

impl XYCutStrategy {
    /// Short-line two-column-prose admission (#536, v0.3.58 Part 1a).
    ///
    /// Called from `classify_region_kind` ONLY for the short-line case
    /// (`mean_chars <= 20`) that the long-line prose guard rejects. A
    /// short-verse two-column body (verse-per-line bibles/lexicons) has
    /// short lines yet a strong, table-independent central gutter; a
    /// short-cell numeric table has short lines and NO such corridor.
    ///
    /// Returns `true` only when ALL of the following hold — each one a
    /// length-independent discriminator a short-cell label+data table
    /// cannot satisfy:
    ///   - a single persistent vertical gutter exists: per-line largest
    ///     within-line gap clusters at one X (10 pt radius) covering
    ///     **≥ 70 %** of gap-bearing lines (concentration) and present on
    ///     **≥ 60 %** of all lines (coverage) — a table's dominant gap
    ///     scatters across cell boundaries and appears on a minority of
    ///     rows;
    ///   - that gutter sits near the region centre: offset ∈
    ///     **[0.30, 0.70]·region_width** — a label+data table's dominant
    ///     gap sits off-centre;
    ///   - **left/right char balance:** non-whitespace char mass on each
    ///     side of the gutter is **≥ 35 %** of the total — a label column
    ///     is lopsided (one side is tiny numeric labels);
    ///   - **≤ 2 left-edge clusters** left of the gutter (30 pt radius) —
    ///     a real two-column body starts each column at one X; an
    ///     N-column table has ≥ 3 left-edge clusters (the fix-534
    ///     `left_edge_clusters >= 3 → Mixed` rule).
    pub(super) fn short_line_central_corridor_prose(
        &self,
        all_spans: &[TextSpan],
        indices: &[usize],
        x_min: f32,
        region_width: f32,
    ) -> bool {
        if region_width <= 0.0 {
            return false;
        }

        // Re-cluster spans into lines, keeping PER-SPAN (left, right, chars)
        // so we can find the within-line gutter gap and split char mass.
        let mut lines: std::collections::BTreeMap<i32, Vec<(f32, f32, usize)>> =
            std::collections::BTreeMap::new();
        for &i in indices {
            let s = &all_spans[i];
            let y_key = s.bbox.top().round() as i32;
            let nonws = s.text.chars().filter(|c| !c.is_whitespace()).count();
            lines
                .entry(y_key)
                .or_default()
                .push((s.bbox.left(), s.bbox.right(), nonws));
        }
        let total_lines = lines.len();
        if total_lines == 0 {
            return false;
        }

        // Per-line: largest within-line gap and its midpoint X. A gap of
        // ≥ 6 pt suppresses ordinary 2–5 pt word spacing.
        const MIN_GAP_PT: f32 = 6.0;
        let mut gap_positions: Vec<f32> = Vec::new();
        for line_spans in lines.values() {
            if line_spans.len() < 2 {
                continue;
            }
            let mut sorted = line_spans.clone();
            sorted.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
            let mut largest_gap = 0.0_f32;
            let mut largest_mid = 0.0_f32;
            for w in sorted.windows(2) {
                let gap = w[1].0 - w[0].1;
                if gap > largest_gap {
                    largest_gap = gap;
                    largest_mid = (w[0].1 + w[1].0) * 0.5;
                }
            }
            if largest_gap >= MIN_GAP_PT {
                gap_positions.push(largest_mid);
            }
        }
        if gap_positions.is_empty() {
            return false;
        }

        // Cluster gap positions (10 pt radius) → dominant corridor.
        const CLUSTER_RADIUS_PT: f32 = 10.0;
        let mut sorted_gaps = gap_positions.clone();
        sorted_gaps.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        let mut best_size = 0usize;
        let mut best_center = 0.0_f32;
        for &pivot in &sorted_gaps {
            let lo = pivot - CLUSTER_RADIUS_PT;
            let hi = pivot + CLUSTER_RADIUS_PT;
            let mut count = 0usize;
            let mut sum = 0.0_f32;
            for &g in &sorted_gaps {
                if g >= lo && g <= hi {
                    count += 1;
                    sum += g;
                }
            }
            if count > best_size {
                best_size = count;
                best_center = sum / count as f32;
            }
        }
        if best_size == 0 {
            return false;
        }

        // Concentration ≥ 70 % of gap-bearing lines at one X.
        if best_size * 10 < gap_positions.len() * 7 {
            return false;
        }
        // Coverage ≥ 60 % of ALL lines carry the corridor.
        if best_size * 10 < total_lines * 6 {
            return false;
        }
        // Centre: gutter offset ∈ [0.30, 0.70]·region_width.
        let gutter_offset = best_center - x_min;
        if gutter_offset < region_width * 0.30 || gutter_offset > region_width * 0.70 {
            return false;
        }

        // Left/right non-whitespace char balance about the corridor:
        // each side ≥ 35 % of total. A label-column table is lopsided.
        let mut left_chars = 0usize;
        let mut right_chars = 0usize;
        for line_spans in lines.values() {
            for &(l, r, chars) in line_spans {
                let mid = (l + r) * 0.5;
                if mid < best_center {
                    left_chars += chars;
                } else {
                    right_chars += chars;
                }
            }
        }
        let total_chars = left_chars + right_chars;
        if total_chars == 0 {
            return false;
        }
        if (left_chars as f32) < total_chars as f32 * 0.35
            || (right_chars as f32) < total_chars as f32 * 0.35
        {
            return false;
        }

        // ≤ 2 left-edge clusters left of the corridor (30 pt radius). A
        // real two-column body starts its left column at one X (one
        // cluster, maybe two counting a paragraph indent); an N-column
        // table left of the corridor has several cell-start X's → ≥ 3
        // clusters. Cluster EVERY span left-edge that lies left of the
        // corridor (not just each line's minimum) so multi-column cell
        // starts are not collapsed into one cluster.
        const LEFT_CLUSTER_RADIUS_PT: f32 = 30.0;
        let mut clusters: Vec<(f32, usize)> = Vec::new();
        for line_spans in lines.values() {
            for &(l, _, _) in line_spans {
                if l >= best_center {
                    continue;
                }
                if let Some(c) = clusters
                    .iter_mut()
                    .find(|(c, _)| (*c - l).abs() <= LEFT_CLUSTER_RADIUS_PT)
                {
                    let count = c.1 as f32;
                    c.0 = (c.0 * count + l) / (count + 1.0);
                    c.1 += 1;
                } else {
                    clusters.push((l, 1));
                }
            }
        }
        // Drop singleton/noise clusters (< 2 lines) before counting, so a
        // lone outlier left-edge doesn't inflate the count.
        let dominant_left_clusters = clusters.iter().filter(|(_, n)| *n >= 2).count();
        if dominant_left_clusters >= 3 {
            return false;
        }

        true
    }

    /// Two-column-prose probe (#534) — does this region look like two
    /// side-by-side columns of prose with a tight gutter (~10-15pt)?
    ///
    /// Called from `is_single_column_region` when the wide+dense
    /// heuristic would otherwise short-circuit the region as
    /// single-column. Distinguishing signal: most lines fit inside
    /// **one** half of the region width (column-half lines), and the
    /// left edges cluster into exactly **two** groups separated by
    /// approximately half the region width.
    ///
    /// Gated on `classify_region_kind == Prose` so the same machinery
    /// doesn't fire on a 2-column sub-region of a table (the v0.3.53
    /// failure mode).
    ///
    /// Returns `Some(gutter_x)` when a 2-column prose layout is
    /// detected — the caller treats that as a non-single-column verdict
    /// and lets `find_horizontal_split_indexed` cut at the gutter.
    pub(super) fn detect_two_column_prose(
        &self,
        all_spans: &[TextSpan],
        indices: &[usize],
        region_kind: RegionKind,
    ) -> Option<f32> {
        // Cheap shape check first.
        if indices.len() < 8 {
            return None;
        }

        let mut x_min = f32::MAX;
        let mut x_max = f32::MIN;
        for &i in indices {
            x_min = x_min.min(all_spans[i].bbox.left());
            x_max = x_max.max(all_spans[i].bbox.right());
        }
        let region_width = x_max - x_min;
        if region_width < 200.0 {
            // Real two-column bodies span at least ~200pt (the
            // narrowest two-column layout in the corpus is ~250pt for a
            // letter-page body inside ~250pt margins).
            return None;
        }

        // Cluster spans into lines by rounded Y. Keep PER-SPAN
        // (left, right) data so we can detect within-line gaps —
        // the canonical multi-column interleave on issue_07 puts
        // a left-col span (left=82) and a right-col span (left=312)
        // on the same Y baseline. The whole-line bbox.right -
        // bbox.left = 358 pt looks "wide" (358 > 0.6 × 500 = 300)
        // even though each side is a narrow column half.
        let mut lines_spans: std::collections::BTreeMap<i32, Vec<(f32, f32)>> =
            std::collections::BTreeMap::new();
        for &i in indices {
            let s = &all_spans[i];
            let y_key = s.bbox.top().round() as i32;
            lines_spans
                .entry(y_key)
                .or_default()
                .push((s.bbox.left(), s.bbox.right()));
        }
        if lines_spans.len() < 6 {
            return None;
        }

        // For each line, find the largest gap between adjacent spans.
        // A line is treated as multiple "half-lines" if a gap ≥ 10 pt
        // splits it; each side of the gap contributes its leftmost-x
        // to `narrow_lefts`. This is the lesson: the row-by-
        // row interleave shape on issue_07 spans the gutter as bbox
        // but has a clear gap within each line.
        let narrow_threshold = region_width * 0.6;
        let intra_line_gap_threshold = 10.0_f32;
        let mut narrow_lefts: Vec<f32> = Vec::new();
        // Count "narrow" lines for the majority check — a line with
        // a within-line gap contributes 1 to this count regardless of
        // how many half-lines it produces, so the majority threshold
        // stays comparable to single-column reasoning.
        let mut narrow_line_count = 0usize;
        for line_spans in lines_spans.values() {
            let mut sorted = line_spans.clone();
            sorted.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
            // Detect largest within-line gap.
            let mut largest_gap = 0.0_f32;
            let mut split_idx: Option<usize> = None;
            for (i, w) in sorted.windows(2).enumerate() {
                let gap = w[1].0 - w[0].1;
                if gap > largest_gap {
                    largest_gap = gap;
                    split_idx = Some(i);
                }
            }
            let line_left = sorted.first().map(|(l, _)| *l).unwrap_or(0.0);
            let line_right = sorted.last().map(|(_, r)| *r).unwrap_or(0.0);
            let line_extent = (line_right - line_left).max(0.0);

            if let Some(si) = split_idx {
                if largest_gap >= intra_line_gap_threshold {
                    // Within-line gap detected — treat each side as
                    // its own narrow half-line.
                    narrow_lefts.push(line_left);
                    // The right-side starts at sorted[si + 1].0
                    if let Some(&(right_side_left, _)) = sorted.get(si + 1) {
                        narrow_lefts.push(right_side_left);
                    }
                    narrow_line_count += 1;
                    continue;
                }
            }

            if line_extent < narrow_threshold {
                narrow_lefts.push(line_left);
                narrow_line_count += 1;
            }
        }
        // Majority of lines must be narrow — otherwise this isn't a
        // 2-column body, it's a single-column body with a few short
        // last-lines.
        if narrow_line_count * 2 < lines_spans.len() {
            return None;
        }

        // Cluster the narrow left-edges. Two clusters separated by
        // approximately half the region width = 2-column prose.
        let cluster_radius = 30.0_f32;
        let mut clusters: Vec<(f32, usize)> = Vec::new();
        for &x in &narrow_lefts {
            if let Some(c) = clusters
                .iter_mut()
                .find(|(c, _)| (*c - x).abs() <= cluster_radius)
            {
                // Running mean
                let count = c.1 as f32;
                c.0 = (c.0 * count + x) / (count + 1.0);
                c.1 += 1;
            } else {
                clusters.push((x, 1));
            }
        }

        // Want exactly 2 substantial clusters separated by ~half-width.
        // ≥ 3 clusters = either a table or a band-mixed region — bail.
        if clusters.len() != 2 {
            return None;
        }
        // Sort by x.
        clusters.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
        let (c1_x, c1_n) = clusters[0];
        let (c2_x, c2_n) = clusters[1];

        // Each cluster needs substantial coverage — ≥ 3 lines, or 20 %
        // of the line count, whichever is larger. Reject lopsided
        // shapes (header + body-paragraph).
        let min_cluster = 3usize.max(narrow_lefts.len() / 5);
        if c1_n < min_cluster || c2_n < min_cluster {
            return None;
        }

        // Gap between cluster centres ≥ 30 % of region width (the
        // gutter + right-column left-margin). For a tight gutter of
        // ~12pt with two ~250pt columns the gap is ~250pt out of 512pt
        // → ~49 %, well above the floor.
        let gap = c2_x - c1_x;
        if gap < region_width * 0.30 {
            return None;
        }

        // Positive identification of prose — required by the
        // classifier to avoid the google_doc 2-col table
        // sub-region false positive.
        if region_kind != RegionKind::Prose {
            return None;
        }

        // Gutter midpoint as the cut. The cluster centres are the left
        // edges of the two columns; the gutter sits between the right
        // edge of column 1 and the left edge of column 2. We don't
        // track right edges per cluster, so approximate the gutter
        // centre as halfway between the two cluster centres — that's
        // close enough; the actual partition uses `bbox.left()` per
        // span so individual spans land cleanly on either side.
        let gutter_x = (c1_x + c2_x) * 0.5;
        Some(gutter_x)
    }

    /// Second-pass 2-column-prose detector for the narrow-gutter case
    /// that `detect_two_column_prose` (the line-start-cluster detector)
    /// misses.
    ///
    /// Two-column papers that emit body text at character-cluster
    /// granularity (each glyph its own span) confuse the line-start
    /// detector: titles, captions, and equation labels contribute
    /// outlier singleton clusters in addition to the two body
    /// columns, so the `clusters.len() != 2` gate rejects. Their
    /// gutters are also often narrower than `min_valley_width` so
    /// the primary projection-valley path in
    /// `find_horizontal_split_indexed` rejects as well.
    ///
    /// Distinguishing signal that works regardless of outlier rows:
    /// the **largest within-line gap** on each body line lives at
    /// roughly the same X coordinate (the gutter) across a strong
    /// majority of lines. Cluster those gap positions; if one cluster
    /// covers ≥ 60 % of the body lines AND the region classifies as
    /// `Prose`, the page is two-column prose and the cluster centre
    /// is the gutter X.
    ///
    /// Returns the gutter X coordinate (an actual gap position, not
    /// a midpoint estimate) when the pattern is detected.
    ///
    /// The Prose-classifier gate keeps tables out: table rows have
    /// their largest gap at variable X across rows (different cell
    /// widths), so the gap-position cluster never dominates.
    pub(super) fn detect_narrow_gutter_prose(
        &self,
        all_spans: &[TextSpan],
        indices: &[usize],
        region_kind: RegionKind,
    ) -> Option<f32> {
        if indices.len() < 24 {
            return None;
        }
        let mut x_min = f32::MAX;
        let mut x_max = f32::MIN;
        for &i in indices {
            x_min = x_min.min(all_spans[i].bbox.left());
            x_max = x_max.max(all_spans[i].bbox.right());
        }
        let region_width = x_max - x_min;
        if region_width < 200.0 {
            return None;
        }

        // Cluster spans into lines by rounded Y.
        let mut lines: std::collections::BTreeMap<i32, Vec<(f32, f32)>> =
            std::collections::BTreeMap::new();
        for &i in indices {
            let s = &all_spans[i];
            let y_key = s.bbox.top().round() as i32;
            lines
                .entry(y_key)
                .or_default()
                .push((s.bbox.left(), s.bbox.right()));
        }
        if lines.len() < 12 {
            return None;
        }

        // For each line, find the largest within-line gap (≥ 6 pt
        // suppresses ordinary word-spacing of 2–5 pt). Record the gap's
        // midpoint X.
        const MIN_GAP_PT: f32 = 6.0;
        let mut gap_positions: Vec<f32> = Vec::new();
        for line_spans in lines.values() {
            if line_spans.len() < 2 {
                continue;
            }
            let mut sorted = line_spans.clone();
            sorted.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
            let mut largest_gap = 0.0_f32;
            let mut largest_mid = 0.0_f32;
            for w in sorted.windows(2) {
                let gap = w[1].0 - w[0].1;
                if gap > largest_gap {
                    largest_gap = gap;
                    largest_mid = (w[0].1 + w[1].0) * 0.5;
                }
            }
            if largest_gap >= MIN_GAP_PT {
                gap_positions.push(largest_mid);
            }
        }

        // Need at least 12 gap-bearing lines to cluster — fewer is
        // statistical noise.
        if gap_positions.len() < 12 {
            return None;
        }

        // Cluster the gap positions with a 10 pt radius (tight; the
        // gutter is at one specific X with minor line-to-line drift).
        // Sliding-window two-pointer scan over the sorted positions —
        // both `left` and `right` only advance forward, so total
        // work is O(n) instead of the previous O(n²) pivot scan
        // (thesis-style PDFs with hundreds of gap-bearing rows pay
        // visibly in that nested loop).
        const CLUSTER_RADIUS_PT: f32 = 10.0;
        let mut sorted_gaps = gap_positions.clone();
        sorted_gaps.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        // Prefix sums let us read window-sum in O(1) given (left, right).
        let mut prefix: Vec<f32> = Vec::with_capacity(sorted_gaps.len() + 1);
        prefix.push(0.0);
        for &x in &sorted_gaps {
            prefix.push(prefix.last().unwrap() + x);
        }
        let mut best_size = 0usize;
        let mut best_center = 0.0_f32;
        let mut left = 0usize;
        let mut right = 0usize;
        for &pivot in &sorted_gaps {
            while left < sorted_gaps.len() && sorted_gaps[left] < pivot - CLUSTER_RADIUS_PT {
                left += 1;
            }
            while right < sorted_gaps.len() && sorted_gaps[right] <= pivot + CLUSTER_RADIUS_PT {
                right += 1;
            }
            let count = right - left;
            let sum = prefix[right] - prefix[left];
            if count > best_size {
                best_size = count;
                best_center = sum / count as f32;
            }
        }

        // Concentration: ≥ 70 % of gap-bearing lines cluster at the
        // same X. Distinguishes 2-col prose (one gutter) from
        // tables (gaps at several cell boundaries, lower
        // concentration).
        if best_size * 10 < gap_positions.len() * 7 {
            return None;
        }
        if best_size < 12 {
            return None;
        }
        if best_size * 5 < lines.len() {
            return None;
        }

        // Sanity: the gutter must lie comfortably inside the region.
        let gutter_offset = best_center - x_min;
        if gutter_offset < region_width * 0.2 || gutter_offset > region_width * 0.8 {
            return None;
        }

        // Prose gate — same safety as `detect_two_column_prose`.
        // Tables with narrow cell gaps fail the classifier
        // (`mean_chars < 8` → `Table`), preventing the gap-cluster
        // signal from misfiring on tabular content. Short-verse
        // two-column bodies (#536) now also pass this gate: although
        // their `mean_chars <= 20`, `classify_region_kind`'s short-line
        // central-corridor admission arm returns `Prose` for them, so a
        // routed short-verse body is cut here rather than re-collapsed.
        //
        if region_kind != RegionKind::Prose {
            return None;
        }

        Some(best_center)
    }

    /// Heuristic: does the region look like a single column of body text?
    ///
    /// Called **before** horizontal split attempts. When true, the region
    /// is returned as a single sorted group, bypassing both horizontal
    /// (column) and vertical (row) splits. This prevents XY-Cut from
    /// fragmenting body text at density dips caused by indentation or
    /// short last-lines.
    ///
    /// Detection: cluster spans into lines by rounded top-Y, then count
    /// lines that are both **wide** (extent ≥ 60% region width) and
    /// **dense** (covered ratio ≥ 80%). Body-text lines satisfy both.
    /// Aligned multi-column rows look "wide" because their extent spans
    /// the gutter, but fail the density check because the gutter is empty.
    pub(super) fn is_single_column_region(
        &self,
        all_spans: &[TextSpan],
        indices: &[usize],
    ) -> bool {
        if indices.len() < 3 {
            return false;
        }
        let mut x_min = f32::MAX;
        let mut x_max = f32::MIN;
        for &i in indices {
            x_min = x_min.min(all_spans[i].bbox.left());
            x_max = x_max.max(all_spans[i].bbox.right());
        }
        let region_width = x_max - x_min;
        if region_width <= 10.0 {
            return true;
        }

        // Store both bbox.right and core_right for each span. bbox.right
        // can be over-estimated by extractors (trailing whitespace,
        // stretched advance widths) which makes multi-column lines look
        // like one wide continuous run; core_right (char_count × em) is
        // a conservative fallback used ONLY when adjacent bbox edges
        // overlap (a signal of bbox inflation).
        //
        let mut lines: std::collections::BTreeMap<i32, Vec<(f32, f32, f32)>> =
            std::collections::BTreeMap::new();
        for &i in indices {
            let s = &all_spans[i];
            let y_key = s.bbox.top().round() as i32;
            let char_count = s.text.chars().filter(|c| !c.is_whitespace()).count().max(1) as f32;
            let approx_char_width = (s.font_size * 0.45).max(2.5);
            let core_right = s.bbox.left() + char_count * approx_char_width;
            lines
                .entry(y_key)
                .or_default()
                .push((s.bbox.left(), s.bbox.right(), core_right));
        }
        if lines.len() < 3 {
            return false;
        }

        // A real column gutter recurs at roughly the SAME X position
        // across multiple lines. Sparse title-page layouts (Title /
        // Subtitle / Byline) also have wide inter-word gaps, but their
        // gap positions are scattered — not a gutter. Collect all gap
        // positions (mid-gap X), then check whether a consistent cluster
        // of gap positions appears on ≥30% of lines.
        //
        // Gap uses bbox.right, but if adjacent bboxes OVERLAP (classic
        // signature of extractor-inflated bbox widths), re-check with
        // conservative core_right estimates so column detection is not
        // defeated by trailing whitespace inflation.
        let max_gap = self.min_valley_width;
        let mut gap_positions: Vec<f32> = Vec::new();
        for line_spans in lines.values() {
            let mut sorted = line_spans.clone();
            sorted.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
            for w in sorted.windows(2) {
                let bbox_gap = w[1].0 - w[0].1;
                let (effective_gap, gap_end_left) = if bbox_gap < 0.0 {
                    (w[1].0 - w[0].2, w[0].2)
                } else {
                    (bbox_gap, w[0].1)
                };
                if effective_gap >= max_gap {
                    gap_positions.push((gap_end_left + w[1].0) * 0.5);
                }
            }
        }
        // Centered-block guard: a CENTERED title/subtitle/
        // byline block (each line horizontally centered, varying widths)
        // produces accidental gap clusters that look like a column
        // gutter — but it is NOT columnar, and treating it as columns
        // scrambles reading order ("Quarterly Inventory Review" centered
        // title read as 3 columns → "Quarterly" / "Spring" / ... ).
        //
        // The distinguishing signal: a REAL multi-column layout has the
        // left column starting at a consistent left edge across rows
        // (low variance of per-line leftmost x). Centered text has its
        // leftmost x scattered (each line centered with a different
        // width). Compute the spread of per-line leftmost edges; if it
        // is large relative to the region width, the block is centered,
        // not columnar, so do NOT treat the gap cluster as a gutter.
        // Centered iff the per-line leftmost edges do NOT share a common
        // left margin. A left-aligned layout (single column OR real
        // multi-column) has most rows starting at the same x (the left
        // margin), so the largest cluster of leftmost edges covers a
        // majority of lines. Centered text has each line's leftmost edge
        // scattered (different per line), so no cluster dominates.
        //
        // Using a cluster fraction (not raw spread) is robust to rows
        // that only contain right-column content — those push the spread
        // up but do not change the fact that the left margin still
        // dominates the remaining rows. (Raw spread mis-classified the
        // two-column test where the last row held only a right cell.)
        let looks_centered = {
            let mins: Vec<f32> = lines
                .values()
                .map(|ls| ls.iter().map(|(l, _, _)| *l).fold(f32::MAX, f32::min))
                .collect();
            if mins.len() < 2 {
                false
            } else {
                let tol = 10.0_f32;
                // Largest count of leftmost-edges within ±tol of any single edge.
                // Sort once + binary-search the window instead of the O(k^2)
                // all-pairs scan; the max count is a multiset property so this is
                // identical to the pairwise version.
                let largest = {
                    let mut sorted = mins.clone();
                    sorted.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
                    sorted
                        .iter()
                        .map(|&a| {
                            let lo = sorted.partition_point(|&x| x < a - tol);
                            let hi = sorted.partition_point(|&x| x <= a + tol);
                            hi - lo
                        })
                        .max()
                        .unwrap_or(0)
                };
                // Centered when no left-margin cluster covers a majority.
                (largest as f32) < (mins.len() as f32) * 0.5
            }
        };

        // A SMALL centered block (title / subtitle / byline — few lines,
        // scattered leftmost edges) is treated as a single column so its
        // lines stay in top-to-bottom order and a centered multi-word
        // title is not split into per-word "columns". Gated
        // to <= 6 lines so it only catches title-page-style blocks: a
        // real multi-column body has many lines and is never classified
        // centered here (its left column starts at a consistent margin,
        // giving a small leftmost-spread anyway).
        if looks_centered && lines.len() <= 6 {
            return true;
        }

        // Cluster gap positions: count, for each observed gap, how many
        // other gaps fall within ±20pt. If any cluster contains gaps
        // from ≥30% of lines, it's a genuine column gutter.
        if !gap_positions.is_empty() && !looks_centered {
            let cluster_radius = 20.0_f32;
            // Require ≥3 gap positions (or 20% of lines, whichever is
            // larger) clustered within ±20pt. 20% accommodates pages
            // where header/footer/title rows dilute the body-line count
            // but a real multi-column body still dominates.
            let min_cluster = (3usize).max(lines.len() / 5);
            // Sort once + binary-search each gap's ±radius window instead of the
            // O(k^2) all-pairs scan. Returns false iff some gap's window holds
            // >= min_cluster gaps — identical to the pairwise version.
            let mut sorted_gaps = gap_positions.clone();
            sorted_gaps.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
            for &pos in &sorted_gaps {
                let lo = sorted_gaps.partition_point(|&p| p < pos - cluster_radius);
                let hi = sorted_gaps.partition_point(|&p| p <= pos + cluster_radius);
                if hi - lo >= min_cluster {
                    return false;
                }
            }
        }

        // With no column gutter found on any line, check that the majority
        // of lines are wide AND densely covered. This catches clean body
        // text where every line covers most of the region width.
        let width_threshold = region_width * 0.6;
        let mut wide_dense_lines = 0usize;
        for line_spans in lines.values() {
            let mut sorted = line_spans.clone();
            sorted.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
            let extent_left = sorted.first().unwrap().0;
            let extent_right = sorted.iter().map(|(_, r, _)| *r).fold(f32::MIN, f32::max);
            let extent = extent_right - extent_left;
            if extent < width_threshold {
                continue;
            }
            // Use core_right (char-count estimate) rather than bbox.right
            // for coverage. bbox.right is inflated by tab characters and
            // trailing whitespace — tab-expanded table rows would otherwise
            // score 100% coverage and be misidentified as dense body text.
            let mut covered = 0.0f32;
            let mut last_end = f32::MIN;
            for &(l, _, cr) in &sorted {
                let effective_right = cr.min(extent_right);
                let start = l.max(last_end);
                if effective_right > start {
                    covered += effective_right - start;
                    last_end = effective_right;
                }
            }
            if covered >= extent * 0.8 {
                wide_dense_lines += 1;
            }
        }
        wide_dense_lines * 2 >= lines.len()
    }
}
