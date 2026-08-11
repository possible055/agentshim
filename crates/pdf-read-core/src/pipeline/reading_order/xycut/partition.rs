use super::*;

impl XYCutStrategy {
    /// Index-based recursive partitioning — returns groups of indices into the input span slice.
    ///
    /// Avoids cloning TextSpan at every recursive split level. Spans are only
    /// read through shared reference; indices are partitioned instead.
    pub(super) fn partition_indexed(
        &self,
        all_spans: &[TextSpan],
        indices: &[usize],
    ) -> Vec<Vec<usize>> {
        self.partition_indexed_depth(all_spans, indices, 0)
    }

    /// Depth-bounded recursive partition. `find_vertical_split_indexed` permits
    /// singleton peels, so without a cap a page with many distinct-Y
    /// header/footer strips can recurse O(n) deep (O(n² log n) work);
    /// `MAX_PARTITION_DEPTH` bounds it.
    pub(super) fn partition_indexed_depth(
        &self,
        all_spans: &[TextSpan],
        indices: &[usize],
        depth: u32,
    ) -> Vec<Vec<usize>> {
        if indices.is_empty() {
            return Vec::new();
        }

        // Base case: small region, don't split further
        if indices.len() < self.min_spans_for_split {
            return vec![self.sort_indices(all_spans, indices)];
        }

        // Depth cap: bail to a flat sort rather than recurse unbounded.
        if depth >= MAX_PARTITION_DEPTH {
            return vec![self.sort_indices(all_spans, indices)];
        }

        // (#534): two-column-prose probe BEFORE the
        // single-column short-circuit. Tight gutters (~10-15pt) that
        // sit below `min_valley_width` defeat the standard projection-
        // valley detector, and the wide+dense heuristic inside
        // `is_single_column_region` mis-classifies the body as one
        // column because each line's bbox spans the narrow gutter.
        // The probe positively identifies the 2-column-prose shape
        // (gutter-radius left-edge clusters + ≥6 narrow lines +
        // classify_region_kind == Prose) and only fires when ALL of
        // those signals agree. Critically, the Prose gate prevents
        // the false positive that reverted v0.3.53's attempts on a
        // 2-column sub-region of the google_doc population table
        // (mean_chars < 8 → Table → bail).
        //
        // **Band-separation first**: when the probe would fire AND a
        // clean vertical band-separation (top header / body / bottom
        // footer) is available, peel the band off BEFORE the column
        // cut. Without this step, full-width header / footer rows
        // get absorbed into one of the two column halves and end up
        // mid-page in reading order — the failure mode on the
        // 1256-page French Bible from #536 where the chapter-header
        // band and page-number footer were full-width and span the
        // gutter. The signal for "band": a vertical split whose
        // smaller side has ≤ 25 % of the region's spans (a tight
        // band relative to the body it sits next to).
        // Two-column-prose detector based on line-start clustering.
        // When it fires, peel any wide Y-band first (title / authors
        // / abstract / footer often span the gutter) before the
        // column cut, so they don't get fragmented across columns.
        // Each peeled band is re-classified inside the recursive
        // call.
        //
        // Classify once and pass to both prose detectors below; each gated on
        // `classify_region_kind == Prose` and re-ran the same line clustering.
        let region_kind = self.classify_region_kind(all_spans, indices);
        if let Some(gutter_x) = self.detect_two_column_prose(all_spans, indices, region_kind) {
            if let Some((above, below)) = self.find_vertical_split_indexed(all_spans, indices) {
                log::debug!(
                    "XY-cut: peeling Y-band before column cut, above={} below={}",
                    above.len(),
                    below.len()
                );
                let mut result = self.partition_indexed_depth(all_spans, &above, depth + 1);
                result.extend(self.partition_indexed_depth(all_spans, &below, depth + 1));
                return result;
            }
            let (left, right): (Vec<usize>, Vec<usize>) = indices
                .iter()
                .copied()
                .partition(|&i| all_spans[i].bbox.left() < gutter_x);
            if !left.is_empty() && !right.is_empty() {
                log::debug!(
                    "XY-cut: two-column-prose detected, gutter_x={:.1}, left={} right={}",
                    gutter_x,
                    left.len(),
                    right.len()
                );
                let mut result = self.partition_indexed_depth(all_spans, &left, depth + 1);
                result.extend(self.partition_indexed_depth(all_spans, &right, depth + 1));
                return result;
            }
        }

        // Narrow-gutter prose detector — second pass for layouts
        // where the line-start cluster shape is masked by outlier
        // singletons (title / caption / equation rows scattering
        // extra clusters that block the primary detector). Cuts
        // directly at the gap-cluster centre WITHOUT peeling a
        // Y-band first: for these pages `find_vertical_split`
        // tends to fire on mid-body paragraph gaps and bisect
        // the body across the peel — both halves then lose
        // enough gutter signal that the column cut never reaches
        // them on recursion.
        if let Some(gutter_x) = self.detect_narrow_gutter_prose(all_spans, indices, region_kind) {
            let (left, right): (Vec<usize>, Vec<usize>) = indices
                .iter()
                .copied()
                .partition(|&i| all_spans[i].bbox.left() < gutter_x);
            if !left.is_empty() && !right.is_empty() {
                log::debug!(
                    "XY-cut: narrow-gutter prose detected, gutter_x={:.1}, left={} right={}",
                    gutter_x,
                    left.len(),
                    right.len()
                );
                let mut result = self.partition_indexed_depth(all_spans, &left, depth + 1);
                result.extend(self.partition_indexed_depth(all_spans, &right, depth + 1));
                return result;
            }
        }

        // Detect single-column body text up-front and skip all spatial
        // splits. Real body text has density dips (indented code, short
        // last-lines, paragraph breaks) that would otherwise trigger
        // spurious horizontal (column) or vertical (row) splits,
        // scrambling reading order. The subsequent sort-by-Y already
        // handles row order within a column.
        if self.is_single_column_region(all_spans, indices) {
            return vec![self.sort_indices(all_spans, indices)];
        }

        let split_h =
            |s: &Self, sp: &[TextSpan], idx: &[usize]| s.find_horizontal_split_indexed(sp, idx);
        let split_v =
            |s: &Self, sp: &[TextSpan], idx: &[usize]| s.find_vertical_split_indexed(sp, idx);

        let first_split = if self.prefer_horizontal {
            split_h
        } else {
            split_v
        };
        let second_split = if self.prefer_horizontal {
            split_v
        } else {
            split_h
        };

        if let Some((a, b)) = first_split(self, all_spans, indices) {
            let mut result = self.partition_indexed_depth(all_spans, &a, depth + 1);
            result.extend(self.partition_indexed_depth(all_spans, &b, depth + 1));
            return result;
        }

        if let Some((a, b)) = second_split(self, all_spans, indices) {
            let mut result = self.partition_indexed_depth(all_spans, &a, depth + 1);
            result.extend(self.partition_indexed_depth(all_spans, &b, depth + 1));
            return result;
        }

        // No split found, return as single group
        vec![self.sort_indices(all_spans, indices)]
    }

    /// Classifier verdict for a region — used to gate the tight-gutter
    /// column-split path (#534) so the same XY-cut recursion no longer
    /// corrupts table cells (the lesson).
    ///
    /// See the inline post-mortem at lines 73–101: two prior attempts at
    /// the multi-column-prose fix were reverted by the 70-PDF sweep when
    /// they accidentally fired on a 2-column sub-region of a real table
    /// and reordered digits. The fix has to *positively identify prose*
    /// before allowing the tight cut — not merely *fail to identify
    /// table*. This classifier is that positive identification.
    pub(super) fn classify_region_kind(
        &self,
        all_spans: &[TextSpan],
        indices: &[usize],
    ) -> RegionKind {
        // Cheap shape check first.
        if indices.len() < 6 {
            return RegionKind::Mixed;
        }

        let mut x_min = f32::MAX;
        let mut x_max = f32::MIN;
        for &i in indices {
            x_min = x_min.min(all_spans[i].bbox.left());
            x_max = x_max.max(all_spans[i].bbox.right());
        }
        let region_width = x_max - x_min;
        if region_width <= 10.0 {
            return RegionKind::Mixed;
        }

        // Cluster spans into lines by rounded Y.
        let mut lines: std::collections::BTreeMap<i32, (f32, f32, usize)> =
            std::collections::BTreeMap::new();
        for &i in indices {
            let s = &all_spans[i];
            let y_key = s.bbox.top().round() as i32;
            let nonws_chars = s.text.chars().filter(|c| !c.is_whitespace()).count();
            let entry = lines.entry(y_key).or_insert((f32::MAX, f32::MIN, 0));
            entry.0 = entry.0.min(s.bbox.left());
            entry.1 = entry.1.max(s.bbox.right());
            entry.2 += nonws_chars;
        }

        let line_count = lines.len();
        if line_count < 6 {
            // Too few lines to be a substantial prose body. Headings,
            // captions, single paragraphs all land here — leave them to
            // the default XY-cut behaviour.
            return RegionKind::Mixed;
        }

        // Per-line statistics: average char count and the count of
        // "narrow" lines whose extent < 0.6 × region_width (a column-half
        // line) and "wide" lines whose extent ≥ 0.6 × region_width (a
        // body-text or table-row line). Table cells are narrow; tables
        // have many such narrow lines but with very short content.
        let mut total_chars = 0usize;
        let mut narrow_lines = 0usize;
        let mut wide_lines = 0usize;
        for (left, right, chars) in lines.values() {
            total_chars += chars;
            let extent = (*right - *left).max(0.0);
            if extent < region_width * 0.6 {
                narrow_lines += 1;
            } else {
                wide_lines += 1;
            }
        }
        let mean_chars = total_chars as f32 / line_count as f32;

        // PROSE: tall stack of wide lines OR tall stack of half-column
        // lines with substantial content per line.
        //   - mean_chars > 20: real prose, not table cells
        //   - line_count ≥ 6: substantial column
        //   - either:
        //     * majority of lines are wide (single-column body), OR
        //     * majority of lines are narrow with mean_chars > 20
        //       (two half-column lines with prose content)
        let mostly_wide = wide_lines * 2 > line_count;
        let mostly_narrow = narrow_lines * 2 > line_count;
        if mean_chars > 20.0 && (mostly_wide || mostly_narrow) {
            return RegionKind::Prose;
        }

        // SHORT-LINE PROSE (#536 short-verse two-column bodies): the
        // `mean_chars > 20` guard above deliberately rejected short-verse
        // two-column bodies (Bible / lexicon editions — a verse fragment
        // per column-line is often < 20 non-whitespace chars) along with
        // short-cell tables. The guard was doing two jobs at once. Here we
        // re-admit ONLY the short-line case that carries a *strong central
        // gutter corridor* a short-cell table cannot fake: a single
        // persistent vertical gutter near the region centre, present on a
        // high fraction of lines, with balanced left/right char mass and
        // ≤ 2 left-edge clusters. A label+data table fails this on
        // concentration/coverage (its gaps scatter across cell
        // boundaries), centre (the dominant gap sits off-centre),
        // char-balance (the label column is tiny), or left-edge clusters
        // (≥ 3 columns). The long-line accept path above is byte-unchanged.
        if mean_chars <= 20.0
            && self.short_line_central_corridor_prose(all_spans, indices, x_min, region_width)
        {
            return RegionKind::Prose;
        }

        // TABLE: lots of narrow lines, short content per line (mean_chars
        // < 8). The google_doc_document.pdf population table —
        // the canonical regression that reverted attempts 1 & 2 — sits
        // squarely here (digit-only cells, ≤ 7 chars each).
        if mean_chars < 8.0 {
            return RegionKind::Table;
        }

        // Anything in between (e.g. captions with headings, mixed
        // figure-and-text bands) → don't risk the tight cut.
        RegionKind::Mixed
    }
}
