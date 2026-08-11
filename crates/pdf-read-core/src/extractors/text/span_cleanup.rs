use super::*;

impl<'doc> TextExtractor<'doc> {
    /// Deduplicate overlapping characters on the same line.
    ///
    /// Some PDFs render text multiple times at slightly different X positions
    /// (e.g., for bold effect or shadowing). This causes garbled text output when
    /// all renders are extracted. We keep only one character when multiple chars
    /// at nearly the same position exist.
    ///
    /// Heuristic: If two consecutive characters on the same line (Y rounded to
    /// integer) overlap by a fraction of their own advance width, keep only the
    /// first one.
    ///
    /// The threshold is expressed as a fraction of the glyph's `advance_width`
    /// (see [`Self::DEDUP_OVERLAP_RATIO`]) rather than an absolute point
    /// value. Real rendering duplicates (stroke+fill, bold shadow,
    /// outline+fill) sit at nearly identical positions — well under 30 % of
    /// one advance apart. Legitimate adjacent doublets of narrow glyphs
    /// (`ll`, `rr`, `II`, `ii` at small font sizes) are separated by one
    /// full advance; an absolute threshold of e.g. 2 pt would wrongly
    /// collapse them on fonts where a narrow glyph's advance drops below
    /// ~2 pt (e.g. Helvetica at ≤ 9 pt).
    ///
    /// Capped at [`Self::DEDUP_OVERLAP_CAP_PT`] to preserve the existing
    /// behaviour for pathologically oversized advance values, and falls
    /// back to `bbox.width` when `advance_width` is missing from the font
    /// dictionary.
    pub(super) fn deduplicate_overlapping_chars(&mut self) {
        if self.chars.is_empty() {
            return;
        }

        let before = self.chars.len();
        let mut prev_y_rounded: Option<i32> = None;
        let mut prev_x: Option<f32> = None;
        let mut prev_char: Option<char> = None;

        // Retained in place: the predicate only looks back at the previously
        // KEPT glyph, which `retain`'s in-order visit preserves. Building a
        // second Vec instead deep-cloned every glyph — and `TextChar` owns a
        // `font_name` String, so that was one malloc per glyph, per page.
        self.chars.retain(|ch| {
            let y_rounded = ch.bbox.y.round() as i32;
            let x = ch.bbox.x;

            // Check if this char overlaps with the previous one
            let should_skip = if let (Some(prev_y), Some(prev_x_val), Some(prev_ch)) =
                (prev_y_rounded, prev_x, prev_char)
            {
                // Reference width: advance_width if known, else bbox.width,
                // else the legacy cap (keeps behaviour for pathological
                // inputs without advance metrics).
                let ref_width = if ch.advance_width > 0.0 {
                    ch.advance_width
                } else if ch.bbox.width > 0.0 {
                    ch.bbox.width
                } else {
                    Self::DEDUP_OVERLAP_CAP_PT
                };
                let threshold =
                    (ref_width * Self::DEDUP_OVERLAP_RATIO).min(Self::DEDUP_OVERLAP_CAP_PT);
                // Same character, same line, and within `threshold` horizontally
                ch.char == prev_ch && y_rounded == prev_y && (x - prev_x_val).abs() < threshold
            } else {
                false
            };

            if !should_skip {
                prev_y_rounded = Some(y_rounded);
                prev_x = Some(x);
                prev_char = Some(ch.char);
            } else {
                log::trace!(
                    "Deduplicating overlapping char '{}' at X={:.1}, Y={:.1} (too close to previous)",
                    ch.char,
                    x,
                    ch.bbox.y
                );
            }
            !should_skip
        });

        log::debug!(
            "Deduplicated {} overlapping characters ({} -> {} chars)",
            before - self.chars.len(),
            before,
            self.chars.len()
        );
    }

    /// Snap super/subscript glyph spans onto the baseline of an
    /// adjacent base span so downstream row-aware sorting keeps
    /// them inline.
    ///
    /// PDF §9.3.7 defines text rise (`Ts`) as a per-text-state
    /// vertical offset added to the rendering position; the
    /// resulting glyphs sit above (super) or below (sub) the
    /// surrounding baseline. The raw extracted bbox preserves
    /// that offset, so sorting by Y descending interprets a
    /// superscript line of affiliation markers (`1,2 ★ 3,4 …`)
    /// as a row that precedes the author names that they actually
    /// annotate. Snapping each candidate's Y to the matched base
    /// puts them back in the same Y-band.
    ///
    /// A span is a snap candidate when:
    /// - its font_size is < 85 % of a nearby larger-font span,
    /// - its Y is above that base by ≤ 50 % of the base's font_size
    ///   (or below it by the same — covers subscript too), and
    /// - its X falls between the base's right edge and one base
    ///   font_size beyond (the position a superscript would
    ///   appear when typeset directly after the base).
    pub(super) fn snap_superscript_baselines(&mut self) {
        let n = self.spans.len();
        if n < 2 {
            return;
        }

        // Snapshot the read-side fields we need so the borrow checker
        // lets us mutate `self.spans[i].bbox.y` inside the loop.
        let snapshot: Vec<(f32, f32, f32, f32)> = self
            .spans
            .iter()
            .map(|s| (s.bbox.x, s.bbox.y, s.bbox.width, s.font_size))
            .collect();

        // A valid base candidate `j` always has `y_offset = sy - by` in
        // `[0, bfs*0.5]` (see the gates below), so `by` lies in
        // `[sy - bfs*0.5, sy] ⊆ [sy - max_fs*0.5, sy]`. Sort span indices by
        // Y once and, per candidate, binary-search that Y-window instead of
        // rescanning all spans — this turns the previous O(n²) double loop
        // (which hung for >30 s on archive.org / Google-Books pages whose
        // invisible hOCR layer emits thousands of spans, #575) into roughly
        // O(n log n + n·window). The window is a strict superset of the
        // acceptable bases, so the result is identical to the full scan.
        let max_fs = snapshot.iter().map(|s| s.3).fold(0.0f32, f32::max);
        let max_half_em = max_fs * 0.5;
        let mut by_order: Vec<usize> = (0..n).collect();
        by_order.sort_by(|&a, &b| crate::utils::safe_float_cmp(snapshot[a].1, snapshot[b].1));
        let ys_sorted: Vec<f32> = by_order.iter().map(|&idx| snapshot[idx].1).collect();

        for i in 0..n {
            let (sx, sy, _sw, sfs) = snapshot[i];
            if sfs <= 0.0 {
                continue;
            }
            // Find the closest base candidate (in Y) that satisfies
            // the super/subscript geometry. Pick the smallest |y_offset|
            // tie-breaker so a candidate sandwiched between two body
            // lines snaps onto the nearer one.
            let mut best_base_y: Option<f32> = None;
            let mut best_abs_offset = f32::MAX;
            // Candidates have `by ∈ [sy - max_half_em, sy]`; restrict the scan
            // to that contiguous slice of the Y-sorted index.
            let lo = ys_sorted.partition_point(|&y| y < sy - max_half_em);
            let hi = ys_sorted.partition_point(|&y| y <= sy);
            for &j in &by_order[lo..hi] {
                if i == j {
                    continue;
                }
                let (bx, by, bw, bfs) = snapshot[j];
                if bfs <= sfs * 1.15 {
                    continue;
                }
                let y_offset = sy - by;
                let half_em = bfs * 0.5;
                if y_offset.abs() > half_em {
                    continue;
                }
                // Skip subscripts (lowered glyphs). The document-level
                // pass `apply_super_sub_script_substitutions` needs to
                // see them at their original lowered baseline so it can
                // substitute ASCII digits with U+2080..U+2089 (e.g.
                // H2O -> H\u{2082}O). Snapping them onto the base
                // baseline would defeat that substitution.
                if y_offset < 0.0 {
                    continue;
                }
                // X adjacency: the candidate's left edge must sit
                // near the base's right edge — within one base
                // font_size to the right and a small slack to the
                // left for kerning. Combining diacritics are
                // excluded by the size-ratio gate above (they
                // typically share font_size with their base
                // letter, failing `bfs > sfs * 1.15`).
                let base_right = bx + bw;
                let dx = sx - base_right;
                if dx < -bfs * 0.25 || dx > bfs {
                    continue;
                }
                let abs_off = y_offset.abs();
                if abs_off < best_abs_offset {
                    best_abs_offset = abs_off;
                    best_base_y = Some(by);
                }
            }
            if let Some(by) = best_base_y {
                self.spans[i].bbox.y = by;
            }
        }
    }

    /// Sort extracted text spans by reading order (top-to-bottom, left-to-right).
    pub(super) fn sort_spans_by_reading_order(&mut self) {
        if self.spans.is_empty() {
            return;
        }

        // Vertical-mode (tategaki) routing. Each span carries the writing
        // mode it was emitted under (`wmode == 1` for vertical text). When
        // the page is *predominantly* vertical we apply column-aware
        // top-to-bottom + right-to-left ordering. When the page is
        // predominantly horizontal we fall through to the existing
        // horizontal sort; the rare mixed-mode case stays governed by the
        // dominant mode here. Per-span wmode is preserved on every span
        // either way, so downstream consumers (export, search) can still
        // distinguish them.
        let vertical_count = self.spans.iter().filter(|s| s.wmode == 1).count();
        let total = self.spans.len();
        if total > 0 && vertical_count * 2 >= total {
            log::trace!(
                "Reading order: {}/{} spans are vertical — using tategaki sort",
                vertical_count,
                total
            );
            self.sort_spans_vertical_tategaki();
            return;
        }

        // Detect columns first
        let columns = self.detect_span_columns();

        log::trace!(
            "Column detection: found {} columns from {} spans",
            columns.len(),
            self.spans.len()
        );
        for (i, (left, right)) in columns.iter().enumerate() {
            log::trace!(
                "  Column {}: X range [{:.1}, {:.1}] (width: {:.1})",
                i,
                left,
                right,
                right - left
            );
        }

        if columns.len() <= 1 {
            // Single column or no columns detected: use simple sort
            log::trace!("Using simple Y-then-X sorting (single column)");
            self.simple_sort_spans();
        } else {
            // Multi-column layout: sort within each column, then across columns
            log::trace!("Using column-aware sorting ({} columns)", columns.len());
            self.sort_spans_by_columns(&columns);
        }
    }

    /// Sort spans in vertical writing (tategaki) order: right-to-left
    /// across columns, top-to-bottom within each column. See
    /// `crate::utils::sort_vertical_tategaki` for the column-clustering
    /// algorithm and the total-order rationale.
    pub(super) fn sort_spans_vertical_tategaki(&mut self) {
        self.spans =
            crate::utils::sort_vertical_tategaki(std::mem::take(&mut self.spans), |s| &s.bbox);
    }

    /// Simple Y-then-X sorting for single-column layouts.
    pub(super) fn simple_sort_spans(&mut self) {
        self.spans.sort_by(|a, b| {
            // Round Y coordinates for stable comparison
            let a_y_rounded = a.bbox.y.round() as i32;
            let b_y_rounded = b.bbox.y.round() as i32;

            match b_y_rounded.cmp(&a_y_rounded) {
                std::cmp::Ordering::Equal => {
                    // Same line: sort by X ascending (left to right)
                    crate::utils::safe_float_cmp(a.bbox.x, b.bbox.x)
                }
                other => other,
            }
        });
    }

    /// Detect columns by analyzing X-coordinate distribution.
    ///
    /// Returns column boundaries as (left_x, right_x) pairs, sorted left-to-right.
    pub(super) fn detect_span_columns(&self) -> Vec<(f32, f32)> {
        if self.spans.is_empty() {
            return vec![];
        }

        // Find page bounds
        let min_x = self
            .spans
            .iter()
            .map(|s| s.bbox.x)
            .fold(f32::INFINITY, f32::min);
        let max_x = self
            .spans
            .iter()
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);

        let page_width = max_x - min_x;

        // Build X-coordinate histogram to find vertical gaps
        let bins = 100;
        let bin_width = page_width / bins as f32;
        let mut histogram = vec![0; bins];

        for span in &self.spans {
            let start_bin = ((span.bbox.x - min_x) / bin_width) as usize;
            let end_bin = ((span.bbox.x + span.bbox.width - min_x) / bin_width) as usize;

            for i in start_bin..=end_bin.min(bins - 1) {
                histogram[i] += 1;
            }
        }

        // Find gaps (bins with zero or very low content)
        let avg_density: f32 = histogram.iter().sum::<i32>() as f32 / bins as f32;
        let gap_threshold = (avg_density * 0.2).max(1.0); // 20% of average or at least 1

        let mut gaps = vec![];
        let mut in_gap = false;
        let mut gap_start = 0;

        for (i, &count) in histogram.iter().enumerate() {
            if count as f32 <= gap_threshold {
                if !in_gap {
                    gap_start = i;
                    in_gap = true;
                }
            } else if in_gap {
                // End of gap - record if significant
                // Use 2% of page width OR absolute 15pt minimum (catches narrow column gutters)
                let gap_width = (i - gap_start) as f32 * bin_width;
                if gap_width > (page_width * 0.02).max(15.0) {
                    let gap_x = min_x + gap_start as f32 * bin_width;
                    gaps.push(gap_x);
                }
                in_gap = false;
            }
        }

        // No significant gaps found - single column
        if gaps.is_empty() {
            return vec![(min_x, max_x)];
        }

        // Build column boundaries from gaps
        let mut columns = vec![];
        let mut left = min_x;

        for gap_x in gaps {
            columns.push((left, gap_x));
            left = gap_x;
        }
        columns.push((left, max_x));

        log::debug!("Detected {} columns: {:?}", columns.len(), columns);

        columns
    }

    /// Sort spans by column-aware reading order.
    ///
    /// Process columns left-to-right, and within each column, top-to-bottom.
    pub(super) fn sort_spans_by_columns(&mut self, columns: &[(f32, f32)]) {
        // Assign each span to a column
        let mut column_spans: Vec<Vec<TextSpan>> = vec![vec![]; columns.len()];

        for span in self.spans.drain(..) {
            let span_center_x = span.bbox.x + span.bbox.width / 2.0;

            // Find which column this span belongs to
            let col_idx = columns
                .iter()
                .position(|&(left, right)| span_center_x >= left && span_center_x <= right)
                .unwrap_or(0); // Default to first column if not found

            column_spans[col_idx].push(span);
        }

        // Sort within each column (top-to-bottom, then left-to-right)
        for col_spans in &mut column_spans {
            col_spans.sort_by(|a, b| {
                let a_y_rounded = a.bbox.y.round() as i32;
                let b_y_rounded = b.bbox.y.round() as i32;

                match b_y_rounded.cmp(&a_y_rounded) {
                    std::cmp::Ordering::Equal => crate::utils::safe_float_cmp(a.bbox.x, b.bbox.x),
                    other => other,
                }
            });
        }

        // Reassemble: read columns left-to-right
        for col_spans in column_spans {
            self.spans.extend(col_spans);
        }
    }

    /// Deduplicate overlapping text spans on the same line.
    ///
    /// Uses hybrid geometric + content-based deduplication:
    /// - Geometric check (same Y, X within a fraction of the span's per-glyph
    ///   advance) — catches identical positions
    /// - Content check (same text, same line Y, different X) — catches
    ///   duplicates across columns
    ///
    /// The geometric threshold is expressed as a fraction of the span's
    /// per-glyph width (bbox.width / char_count), capped by
    /// [`Self::DEDUP_OVERLAP_CAP_PT`] and scaled by
    /// [`Self::DEDUP_OVERLAP_RATIO`]. An absolute threshold would wrongly
    /// collapse legitimate single-glyph spans of adjacent narrow glyphs
    /// (`ll`, `rr`, `II`, `ii` at small font sizes) in PDFs that emit text
    /// glyph-by-glyph with kerning.
    pub(super) fn deduplicate_overlapping_spans(&mut self) {
        if self.spans.is_empty() {
            return;
        }

        // Phase 0 (B7): same-text overlapping spans from stroke+fill render
        // passes. Maps (newspaper / poster) frequently draw every label
        // twice — once stroked for outline, once filled — and both passes
        // land at essentially the same CTM. Without this up-front filter,
        // the merge step later concatenates them into "EverestEverest" /
        // "CentralCentral". We bucket by lowercased text and compare each
        // new span's bbox against prior entries via IoU; any later span
        // whose bbox overlaps an earlier one by >= 70 % is dropped.
        self.dedup_stroke_fill_overlap();

        // Take ownership of spans to avoid cloning during iteration
        let old_len = self.spans.len();
        let spans = std::mem::take(&mut self.spans);
        let mut deduplicated = Vec::with_capacity(old_len);
        let mut prev_y_rounded: Option<i32> = None;
        let mut prev_x: Option<f32> = None;
        let mut prev_text: Option<String> = None;
        let mut seen_content: std::collections::HashMap<String, (f32, f32)> =
            std::collections::HashMap::new();

        let mut geometric_skips = 0;
        let mut content_skips = 0;

        for span in spans {
            let y_rounded = span.bbox.y.round() as i32;
            let x = span.bbox.x;

            // PHASE 1: Geometric deduplication — require BOTH position AND text match
            let geometric_duplicate = if let (Some(prev_y), Some(prev_x_val), Some(ref prev_txt)) =
                (prev_y_rounded, prev_x, &prev_text)
            {
                // Threshold scales with the span's per-glyph advance so that
                // single-glyph narrow spans (`l`, `r`, `I`) are never wrongly
                // treated as overlapping with their legitimate neighbour.
                let char_count = span.text.chars().count().max(1) as f32;
                let per_glyph_width = (span.bbox.width / char_count).max(0.1);
                let threshold =
                    (per_glyph_width * Self::DEDUP_OVERLAP_RATIO).min(Self::DEDUP_OVERLAP_CAP_PT);
                y_rounded == prev_y && (x - prev_x_val).abs() < threshold && span.text == *prev_txt
            } else {
                false
            };

            // PHASE 2: Content-based deduplication — require positions to OVERLAP
            let content_duplicate = if span.text.len() >= 5 {
                if let Some((prev_x_val, prev_y_val)) = seen_content.get(&span.text) {
                    let y_diff = (span.bbox.y - prev_y_val).abs();
                    let x_diff = (span.bbox.x - prev_x_val).abs();

                    // Only dedup when spans overlap geometrically (X within 5pt)
                    // NOT when they're at different positions on the same line
                    let same_line = y_diff < 2.0;
                    let overlapping_position = x_diff < 5.0;

                    same_line && overlapping_position
                } else {
                    false
                }
            } else {
                false
            };

            if geometric_duplicate {
                geometric_skips += 1;
            } else if content_duplicate {
                content_skips += 1;
            } else {
                prev_y_rounded = Some(y_rounded);
                prev_x = Some(x);
                prev_text = Some(span.text.clone());

                // Track content for duplicate detection
                if span.text.len() >= 5 {
                    seen_content.insert(span.text.clone(), (span.bbox.x, span.bbox.y));
                }
                // Move span instead of cloning
                deduplicated.push(span);
            }
        }

        log::debug!(
            "Deduplicated {} spans (geometric: {}, content: {}) ({} -> {} spans)",
            geometric_skips + content_skips,
            geometric_skips,
            content_skips,
            old_len,
            deduplicated.len()
        );

        self.spans = deduplicated;
    }

    /// Drop same-text spans whose bounding boxes overlap heavily with an
    /// earlier span. This is the canonical stroke+fill pattern on maps,
    /// posters, and marketing materials: a label is drawn twice (once
    /// stroked for the outline, once filled for the glyph) at identical
    /// positions. Both passes surface as distinct spans; without this
    /// filter the downstream merge pass concatenates them.
    ///
    /// Keyed by lowercased text + rounded (x, y) bucket to make the
    /// lookup O(1) without quadratic bbox comparisons on large pages.
    /// The actual overlap check falls through to a real IoU on collision.
    pub(super) fn dedup_stroke_fill_overlap(&mut self) {
        use std::collections::HashMap;

        if self.spans.len() < 2 {
            return;
        }
        let old_len = self.spans.len();
        let spans = std::mem::take(&mut self.spans);
        // Bucket each text key into a coarse (cx,cy) grid instead of one flat
        // Vec (O(k²) when a short label repeats N times). The IoU ≥ 0.7 test is
        // unchanged: a partner with that overlap is within ≈0.176·width, so it
        // falls in this cell or a neighbour — querying the 3×3 neighbourhood
        // finds every match the full scan would. Only runs for text ≥ 2 chars
        // (shorter candidates rely on downstream positional dedup).
        const CELL: f32 = 16.0;
        type Grid = HashMap<(i32, i32), Vec<crate::geometry::Rect>>;
        let mut seen: HashMap<String, Grid> = HashMap::new();
        let mut kept: Vec<TextSpan> = Vec::with_capacity(old_len);
        let mut skipped = 0usize;
        for span in spans {
            let trimmed = span.text.trim();
            if trimmed.chars().count() < 2 {
                kept.push(span);
                continue;
            }
            let key = trimmed.to_ascii_lowercase();
            let b = span.bbox;
            let cx = ((b.x + b.width * 0.5) / CELL).floor() as i32;
            let cy = ((b.y + b.height * 0.5) / CELL).floor() as i32;
            let mut is_dup = false;
            if let Some(grid) = seen.get(&key) {
                // Saturating bounds: a span with an extreme/out-of-page bbox can
                // push cx/cy to the i32 limits, where `cx + 1` would overflow in
                // an overflow-checked build (observed on 1008.3918v2.pdf).
                'outer: for gx in cx.saturating_sub(1)..=cx.saturating_add(1) {
                    for gy in cy.saturating_sub(1)..=cy.saturating_add(1) {
                        let Some(others) = grid.get(&(gx, gy)) else {
                            continue;
                        };
                        for other in others {
                            // IoU — intersection over union. >= 0.7 means the
                            // two bboxes are almost the same rectangle, which is
                            // what stroke+fill produces.
                            let ix1 = b.x.max(other.x);
                            let iy1 = b.y.max(other.y);
                            let ix2 = (b.x + b.width).min(other.x + other.width);
                            let iy2 = (b.y + b.height).min(other.y + other.height);
                            if ix2 <= ix1 || iy2 <= iy1 {
                                continue;
                            }
                            let inter = (ix2 - ix1) * (iy2 - iy1);
                            let area_a = b.width * b.height;
                            let area_b = other.width * other.height;
                            let union = area_a + area_b - inter;
                            if union > 0.0 && inter / union >= 0.7 {
                                is_dup = true;
                                break 'outer;
                            }
                        }
                    }
                }
            }
            if is_dup {
                skipped += 1;
            } else {
                seen.entry(key)
                    .or_default()
                    .entry((cx, cy))
                    .or_default()
                    .push(b);
                kept.push(span);
            }
        }
        if skipped > 0 {
            log::debug!("Stroke+fill dedup: dropped {skipped} duplicate spans of {old_len}");
        }
        self.spans = kept;
    }
}
