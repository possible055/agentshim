use super::*;

impl XYCutStrategy {
    /// Find vertical line (X-axis) split using index-based partitioning.
    ///
    /// Rejects lopsided splits where one side contains fewer than ~10% of
    /// the region's spans — those come from single-column pages where
    /// indentation or stray content creates a spurious density dip at
    /// one edge of the projection, not from a real column boundary.
    pub(super) fn find_horizontal_split_indexed(
        &self,
        all_spans: &[TextSpan],
        indices: &[usize],
    ) -> Option<(Vec<usize>, Vec<usize>)> {
        let profile = self.horizontal_projection_indexed(all_spans, indices)?;

        let split_x = if let Some((vs, ve, vw)) = self.find_valley(&profile) {
            if vw < self.min_valley_width {
                return None;
            }
            profile.x_min + (vs + ve) as f32 / 2.0
        } else {
            self.find_split_between_peaks(&profile)?
        };

        // Reject splits where either resulting sub-column would be
        // narrower than ~60 pt (about 6 body-text characters at
        // 10 pt). Without this check, XY-cut recursion sub-splits
        // a single body column into sliver sub-blocks at internal
        // whitespace valleys (paragraph indentation, justified-line
        // trailing gaps, isolated short words), turning what should
        // be a clean column-major emit of a multi-column page into
        // a band-chunked stream. PDF spec §9.4.4 mentions "natural
        // reading order" but does not mandate a
        // minimum column width; this is a descriptive heuristic —
        // a real body column holds at least ~6 characters.
        const MIN_RESULT_WIDTH_PT: f32 = 60.0;
        let mut left_x_min = f32::MAX;
        let mut left_x_max = f32::MIN;
        let mut right_x_min = f32::MAX;
        let mut right_x_max = f32::MIN;
        for &i in indices {
            let l = all_spans[i].bbox.left();
            let r = all_spans[i].bbox.right();
            if l < split_x {
                left_x_min = left_x_min.min(l);
                left_x_max = left_x_max.max(r);
            } else {
                right_x_min = right_x_min.min(l);
                right_x_max = right_x_max.max(r);
            }
        }
        let left_w = left_x_max - left_x_min;
        let right_w = right_x_max - right_x_min;
        if left_w < MIN_RESULT_WIDTH_PT || right_w < MIN_RESULT_WIDTH_PT {
            return None;
        }

        // Partition by span LEFT EDGE (where the glyphs actually start),
        // not bbox.right() and not center. Extractor bboxes overreach to
        // the right (trailing whitespace / stretched advance widths), and
        // for wide single-column body spans the center can also drift
        // past the split. Left edge is anchored to the true glyph start
        // and reliably places each span into its actual column.
        let (left, right): (Vec<usize>, Vec<usize>) = indices
            .iter()
            .partition(|&&i| all_spans[i].bbox.left() < split_x);

        if left.is_empty() || right.is_empty() {
            return None;
        }

        // Real column splits produce balanced partitions. A 95/5 split is
        // almost always from edge dips or stray content, not a column.
        let min_side = (indices.len() / 10).max(2);
        if left.len() < min_side || right.len() < min_side {
            return None;
        }

        // Table-row guard (#7 / PMC8025747). A genuine column gutter is a
        // vertical CORRIDOR: the left column's glyphs END before the gutter
        // and the right column's glyphs BEGIN after it, so the two sides are
        // X-disjoint. A data-table row, by contrast, starts at the left
        // margin but its cells run the FULL width of the region; partitioning
        // such rows by left edge throws the wide rows into `left` while the
        // right-hand cells (their own spans) land in `right`. Taking the cut
        // anyway slices the table's rows into shattered left/right cell groups
        // — the canonical PMC8025747 p2 failure (a prose column stacked above
        // a full-width data table), and the google_doc population-table hazard
        // the post-mortem at lines 73–101 records.
        //
        // Table-row SIGNATURE: SEVERAL left-side rows each span the ENTIRE
        // right column — their glyph content reaches past the right column's
        // far edge (`right_x_max`). A data table has MANY full-width rows (the
        // header and every data row run the whole region width), so when rows
        // are bucketed into `left` by their left edge, multiple of them blanket
        // the whole right column. By contrast:
        //   * a genuine left prose / reference column ENDS before the gutter,
        //     so its lines stop well short of `right_x_max` (never counted);
        //   * a single wide mis-split OCR line (alice) yields at most one or
        //     two straddling spans — never the recurring full-width pattern;
        //   * the single-column google_doc population table short-circuits at
        //     `is_single_column_region` and never reaches here.
        // Requiring ≥ 3 such rows is what isolates the real table from those
        // cases, so the guard is SUBTRACTIVE: it only ever REJECTS a column
        // cut that would shred a table (the recursion then falls back to a row
        // cut and reads the table row-major), never adds or reorders anything.
        //
        // `core_right` (left edge + non-whitespace-char count × ~0.5 em) is
        // used instead of `bbox.right` so trailing-whitespace / advance-width
        // bbox inflation on a real left column's last word is not mistaken for
        // a glyph crossing the gutter. `overlap_tol` (~ one body em) lets a
        // single straddling glyph slip past.
        let mut right_x_max = f32::MIN;
        let mut max_font = 0.0f32;
        for &i in &right {
            right_x_max = right_x_max.max(all_spans[i].bbox.right());
            max_font = max_font.max(all_spans[i].bbox.height.abs());
        }
        let overlap_tol = max_font.max(10.0);
        let full_width_left_rows = left
            .iter()
            .filter(|&&i| {
                let s = &all_spans[i];
                let nonws = s.text.chars().filter(|c| !c.is_whitespace()).count().max(1) as f32;
                let approx_char_width = (s.font_size * 0.45).max(2.5);
                s.bbox.left() + nonws * approx_char_width >= right_x_max - overlap_tol
            })
            .count();
        if full_width_left_rows >= 3 {
            // ≥ 3 left rows each blanket the right column ⇒ this is a table-row
            // slice, not a column gutter. Don't take the column cut; the
            // recursion falls back to a row (horizontal) split and reads the
            // table row-major.
            return None;
        }

        Some((left, right))
    }

    /// Fallback column split: find the deepest trough between the two
    /// strongest density peaks. Used when the standard valley detection
    /// fails because narrow table-cell spans partially fill the gutter.
    ///
    /// Returns the split X coordinate (absolute, not relative to x_min) if
    /// a genuine trough exists — i.e., the minimum between the peaks is ≤
    /// 50% of the weaker peak density.
    pub(super) fn find_split_between_peaks(&self, profile: &ProjectionProfile) -> Option<f32> {
        let density = &profile.density;
        let n = density.len();
        if n < 3 {
            return None;
        }

        // Smooth with a small box filter (window = min_valley_width) to
        // average out individual narrow peaks before finding mass centres.
        let smooth_window = (self.min_valley_width as usize).max(3);
        let half = smooth_window / 2;

        // Smooth into a reused thread-local buffer instead of a fresh `Vec` per
        // failed-valley node. Window-mean is unchanged. (Confirmed not a source
        // of the p.692 non-determinism: the buffer is cleared+refilled to exactly
        // `n` each call and never read out of range.)
        thread_local! {
            static SMOOTH_SCRATCH: std::cell::RefCell<Vec<f32>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }
        SMOOTH_SCRATCH.with(|cell| {
            let mut smoothed = cell.borrow_mut();
            smoothed.clear();
            smoothed.extend((0..n).map(|i| {
                let s = i.saturating_sub(half);
                let e = (i + half + 1).min(n);
                let sum: f32 = density[s..e].iter().sum();
                sum / (e - s) as f32
            }));

            // Find the strongest peak in each half. Use `safe_float_cmp` for
            // NaN-safe total ordering — matches the comparator used elsewhere
            // in the reading-order code so `density` sentinel values can't
            // reach a `partial_cmp` that maps them to `Equal`.
            let mid = n / 2;
            let left_peak =
                (0..mid).max_by(|&a, &b| crate::utils::safe_float_cmp(smoothed[a], smoothed[b]))?;
            let right_peak =
                (mid..n).max_by(|&a, &b| crate::utils::safe_float_cmp(smoothed[a], smoothed[b]))?;

            if smoothed[left_peak] == 0.0 || smoothed[right_peak] == 0.0 {
                return None;
            }

            // Find the minimum density in the interior between the two peaks.
            let search_start = left_peak.min(right_peak) + 1;
            let search_end = left_peak.max(right_peak);
            if search_start >= search_end {
                return None;
            }

            let trough_pos = (search_start..search_end)
                .min_by(|&a, &b| crate::utils::safe_float_cmp(smoothed[a], smoothed[b]))?;

            // Only use if trough is a genuine valley: ≤ 50% of the weaker peak.
            let weaker_peak = smoothed[left_peak].min(smoothed[right_peak]);
            if smoothed[trough_pos] > weaker_peak * 0.5 {
                return None;
            }

            // Trough must be at least min_valley_width from both edges.
            if trough_pos < self.min_valley_width as usize
                || trough_pos + self.min_valley_width as usize > n
            {
                return None;
            }

            Some(profile.x_min + trough_pos as f32)
        })
    }

    /// Find horizontal line (Y-axis) split using index-based partitioning.
    ///
    /// Returns `(above, below)` where `above` holds spans whose rectangle
    /// edge is at larger Y (higher on page in PDF coordinates) and must be
    /// processed first in reading order. PDF Spec ISO 32000-1:2008 §8.3.2.3
    /// defines the default user-space coordinate system with origin at the
    /// lower-left corner and Y increasing upward.
    pub(super) fn find_vertical_split_indexed(
        &self,
        all_spans: &[TextSpan],
        indices: &[usize],
    ) -> Option<(Vec<usize>, Vec<usize>)> {
        let profile = self.vertical_projection_indexed(all_spans, indices)?;
        let (valley_start, valley_end, valley_width) = self.find_valley(&profile)?;

        if valley_width < self.min_valley_width {
            return None;
        }

        let split_y = profile.y_min + (valley_start + valley_end) as f32 / 2.0;

        // `Rect::top()` returns `self.y`, the SMALLER Y coordinate of the
        // normalized rectangle — the method name follows a screen-coordinate
        // convention (Y grows downward) but PDF user space has Y growing
        // upward, so in PDF terms `bbox.top()` is actually the LOWER edge of
        // the glyph's bounding box. The predicate `bbox.top() >= split_y`
        // therefore classifies a span into `above` only when its *lowest*
        // point is already above the split line, i.e. the entire span sits
        // above the cut. Since `split_y` is the midpoint of a horizontal
        // projection valley (an empty band by construction), spans should
        // not straddle it in practice; any that do (e.g. a tall header
        // glyph whose ascenders dip into the valley) fall into `below`.
        let (above, below): (Vec<usize>, Vec<usize>) = indices
            .iter()
            .partition(|&&i| all_spans[i].bbox.top() >= split_y);

        if above.is_empty() || below.is_empty() {
            return None;
        }

        // Row (vertical) splits legitimately produce singleton top
        // partitions for lone headers/titles, so we accept down to 1
        // span per side. The column (horizontal) split is stricter since
        // single-span columns are almost always spurious.
        let min_side = (indices.len() / 10).max(1);
        if above.len() < min_side || below.len() < min_side {
            return None;
        }

        Some((above, below))
    }

    /// Calculate horizontal projection profile from indexed spans.
    pub(super) fn horizontal_projection_indexed(
        &self,
        all_spans: &[TextSpan],
        indices: &[usize],
    ) -> Option<ProjectionProfile> {
        if indices.is_empty() {
            return None;
        }

        let mut x_min = f32::MAX;
        let mut x_max = f32::MIN;
        let mut y_min = f32::MAX;
        let mut y_max = f32::MIN;

        for &i in indices {
            let span = &all_spans[i];
            x_min = x_min.min(span.bbox.left());
            x_max = x_max.max(span.bbox.right());
            y_min = y_min.min(span.bbox.top());
            y_max = y_max.max(span.bbox.bottom());
        }

        let width = (x_max - x_min).ceil() as usize;
        if width > MAX_PROJECTION_SIZE {
            log::warn!(
                "XY-cut: horizontal projection width {} exceeds MAX_PROJECTION_SIZE {}, skipping region (degenerate CTM?)",
                width,
                MAX_PROJECTION_SIZE
            );
            return None;
        }
        let mut density = vec![0.0; width];

        // Text extractors frequently over-estimate span bbox widths
        // (trailing whitespace, stretched advance widths). That makes a
        // full-width projection falsely fill the inter-column gutter on
        // multi-column pages. We project each span's TEXT CORE footprint
        // anchored to its LEFT edge (where glyphs actually start), with
        // length proportional to character count. The left edge is
        // reliable; the right edge is not.
        //
        // Additionally, spans whose core width exceeds 55% of the region
        // width are full-width elements (section headers, figure captions,
        // table titles) that span both columns. Including them fills the
        // inter-column gutter in the density array and prevents valley
        // detection. They are excluded from the projection; the column
        // split boundary will still assign them correctly by left edge.
        let region_width = (x_max - x_min).max(1.0);
        for &i in indices {
            let span = &all_spans[i];
            let height = span.bbox.bottom() - span.bbox.top();
            let char_count = span
                .text
                .chars()
                .filter(|c| !c.is_whitespace())
                .count()
                .max(1);
            // 0.45em per char is a reasonable average across common PDF
            // fonts (Helvetica/Times/Arial at body size) and narrower
            // than the 0.5em advance used for monospace.
            let approx_char_width = (span.font_size * 0.45).max(2.5);
            let core_width = char_count as f32 * approx_char_width;
            let span_width = span.bbox.right() - span.bbox.left();
            // Skip full-width elements (captions, headers, table rows) whose
            // bbox spans more than 55% of the region — they fill the gutter.
            if span_width > region_width * 0.55 {
                continue;
            }
            // Skip isolated single-character/digit spans (table cell values
            // like 'G', 'T', '1', 'A') that scatter across the full X range
            // and fill the column gutter in the density profile. Body text
            // spans always contain multiple characters.
            if char_count < 2 {
                continue;
            }
            let core_left = span.bbox.left();
            let core_right = (core_left + core_width).min(span.bbox.right());
            let x_start = (core_left - x_min).max(0.0).ceil() as usize;
            let x_end = (core_right - x_min).ceil() as usize;

            for j in x_start..x_end.min(width) {
                density[j] += height;
            }
        }

        Some(ProjectionProfile {
            density,
            x_min,
            y_min,
        })
    }

    /// Calculate vertical projection profile from indexed spans.
    pub(super) fn vertical_projection_indexed(
        &self,
        all_spans: &[TextSpan],
        indices: &[usize],
    ) -> Option<ProjectionProfile> {
        if indices.is_empty() {
            return None;
        }

        let mut x_min = f32::MAX;
        let mut x_max = f32::MIN;
        let mut y_min = f32::MAX;
        let mut y_max = f32::MIN;

        for &i in indices {
            let span = &all_spans[i];
            x_min = x_min.min(span.bbox.left());
            x_max = x_max.max(span.bbox.right());
            y_min = y_min.min(span.bbox.top());
            y_max = y_max.max(span.bbox.bottom());
        }

        let height = (y_max - y_min).ceil() as usize;
        if height > MAX_PROJECTION_SIZE {
            log::warn!(
                "XY-cut: vertical projection height {} exceeds MAX_PROJECTION_SIZE {}, skipping region (degenerate CTM?)",
                height,
                MAX_PROJECTION_SIZE
            );
            return None;
        }
        let mut density = vec![0.0; height];

        for &i in indices {
            let span = &all_spans[i];
            let y_start = (span.bbox.top() - y_min).max(0.0).ceil() as usize;
            let y_end = (span.bbox.bottom() - y_min).ceil() as usize;
            let w = span.bbox.right() - span.bbox.left();

            for j in y_start..y_end.min(height) {
                density[j] += w;
            }
        }

        Some(ProjectionProfile {
            density,
            x_min,
            y_min,
        })
    }

    /// Find the widest valley (white space gap) in projection profile.
    ///
    /// Only considers INTERIOR valleys — gaps sandwiched between two
    /// non-empty regions. Leading/trailing empty bands (margin space
    /// outside the actual content extent) are ignored; they represent
    /// page margins, not column gutters, and picking them would produce
    /// meaningless splits.
    pub(super) fn find_valley(&self, profile: &ProjectionProfile) -> Option<(usize, usize, f32)> {
        if profile.density.is_empty() {
            return None;
        }

        // Find peak density
        let peak = profile.density.iter().copied().fold(0.0, f32::max);

        if peak == 0.0 {
            return None;
        }

        // Find the content extent (first and last non-empty positions).
        // Valleys outside this extent are leading/trailing margins.
        let first_nonzero = profile.density.iter().position(|&d| d > 0.0)?;
        let last_nonzero = profile.density.iter().rposition(|&d| d > 0.0)?;

        // Find valleys (regions below threshold)
        let threshold = peak * self.valley_threshold;
        let mut valleys = Vec::new();
        let mut in_valley = false;
        let mut valley_start = 0;

        for (i, &density) in profile.density.iter().enumerate() {
            if density < threshold {
                if !in_valley {
                    valley_start = i;
                    in_valley = true;
                }
            } else if in_valley {
                valleys.push((valley_start, i));
                in_valley = false;
            }
        }

        if in_valley {
            valleys.push((valley_start, profile.density.len()));
        }

        // Merge adjacent interior valley segments separated by a narrow
        // bridge (≤ half the minimum valley width). A callout box or small
        // figure positioned in the column gutter creates a density bump
        // that splits what should be a single valley into two fragments.
        // Bridging re-joins them so the gap is still recognised as a
        // column boundary.
        let bridge_limit = (self.min_valley_width / 2.0).ceil() as usize;
        let interior: Vec<(usize, usize)> = valleys
            .into_iter()
            .filter(|&(start, end)| start > first_nonzero && end <= last_nonzero + 1)
            .collect();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(interior.len());
        for seg in interior {
            if let Some(last) = merged.last_mut() {
                if seg.0 <= last.1 + bridge_limit {
                    last.1 = last.1.max(seg.1);
                    continue;
                }
            }
            merged.push(seg);
        }
        merged
            .into_iter()
            .map(|(start, end)| (start, end, (end - start) as f32))
            .max_by(|a, b| crate::utils::safe_float_cmp(a.2, b.2))
    }

    /// Test-only wrapper for horizontal projection on a contiguous slice.
    #[cfg(test)]
    pub(super) fn horizontal_projection(&self, spans: &[TextSpan]) -> Option<ProjectionProfile> {
        let indices: Vec<usize> = (0..spans.len()).collect();
        self.horizontal_projection_indexed(spans, &indices)
    }

    /// Test-only wrapper for vertical projection on a contiguous slice.
    #[cfg(test)]
    pub(super) fn vertical_projection(&self, spans: &[TextSpan]) -> Option<ProjectionProfile> {
        let indices: Vec<usize> = (0..spans.len()).collect();
        self.vertical_projection_indexed(spans, &indices)
    }

    /// Sort spans in reading order (top-to-bottom, left-to-right).
    #[cfg(test)]
    pub(super) fn sort_spans<'a>(&self, spans: &'a [TextSpan]) -> Vec<&'a TextSpan> {
        let mut sorted: Vec<_> = spans.iter().collect();

        sorted.sort_by(|a, b| {
            // Sort by Y (top) first, descending (top of page first)
            let y_cmp = crate::utils::safe_float_cmp(b.bbox.top(), a.bbox.top());
            if y_cmp != std::cmp::Ordering::Equal {
                return y_cmp;
            }
            // Same Y level, sort by X (left) ascending
            crate::utils::safe_float_cmp(a.bbox.left(), b.bbox.left())
        });

        sorted
    }

    /// Sort indices in reading order (top-to-bottom, left-to-right).
    pub(super) fn sort_indices(&self, all_spans: &[TextSpan], indices: &[usize]) -> Vec<usize> {
        let mut sorted: Vec<usize> = indices.to_vec();
        sorted.sort_by(|&a, &b| {
            let y_cmp =
                crate::utils::safe_float_cmp(all_spans[b].bbox.top(), all_spans[a].bbox.top());
            if y_cmp != std::cmp::Ordering::Equal {
                return y_cmp;
            }
            crate::utils::safe_float_cmp(all_spans[a].bbox.left(), all_spans[b].bbox.left())
        });
        sorted
    }
}
