use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// For every span, the `(max_font_size, anchor_y)` over the spans within
    /// `±band` of its Y, in O(n) via a sliding-window maximum (monotonic deque)
    /// over the Y-sorted order. Replaces a per-span window walk that was O(n²)
    /// when many spans share a Y band (wide table rows).
    ///
    /// Tie-break on equal max font size: the lowest-Y span (deque keeps the
    /// earliest sorted position). A substitution only fires when the span's own
    /// font is strictly smaller than the anchor, so the tie-break merely picks
    /// which equal-sized body span supplies `anchor_y`, all within `band`.
    pub(super) fn compute_band_anchors(
        spans: &[crate::layout::TextSpan],
        sorted_by_y: &[usize],
        band: f32,
    ) -> Vec<(f32, f32)> {
        let n = sorted_by_y.len();
        let mut band_anchor = vec![(0.0f32, 0.0f32); n];
        let y = |p: usize| spans[sorted_by_y[p]].bbox.y;
        let fs = |p: usize| spans[sorted_by_y[p]].font_size;
        // Deque of sorted positions, font size non-increasing front→back;
        // positions are pushed in increasing order so the deque is also
        // position-increasing front→back (front = smallest position = max fs).
        let mut deque: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        let mut lo = 0usize;
        let mut hi = 0usize;
        for pos in 0..n {
            let cy = y(pos);
            while hi < n && y(hi) <= cy + band {
                while let Some(&back) = deque.back() {
                    if fs(back) < fs(hi) {
                        deque.pop_back();
                    } else {
                        break;
                    }
                }
                deque.push_back(hi);
                hi += 1;
            }
            while lo < n && y(lo) < cy - band {
                if deque.front() == Some(&lo) {
                    deque.pop_front();
                }
                lo += 1;
            }
            let best = *deque.front().expect("window always contains pos");
            band_anchor[sorted_by_y[pos]] = (fs(best), y(best));
        }
        band_anchor
    }

    /// Return true when span `i` has a base-sized alphabetic
    /// neighbour both before and after it on the same line band,
    /// within ~1 em horizontally. That captures the "X²Y" /
    /// "H₂O" / "k₁ + …" pattern but excludes footnote markers
    /// that hang off the end of a word with no following body
    /// character.
    /// Bucket span indices by Y-band (`round(y / band)`) so same-line lookups
    /// scan only nearby bands instead of every span. Querying a band `k`'s
    /// `[k-2, k+2]` neighbours is a guaranteed superset of all spans within
    /// `band` points of any Y in band `k`, so an exact `|Δy|` filter on the
    /// result is byte-identical to a full scan.
    pub(super) fn build_y_band_index(
        spans: &[crate::layout::TextSpan],
        band: f32,
    ) -> HashMap<i32, Vec<usize>> {
        let mut idx: HashMap<i32, Vec<usize>> = HashMap::new();
        for (j, s) in spans.iter().enumerate() {
            idx.entry((s.bbox.y / band).round() as i32)
                .or_default()
                .push(j);
        }
        idx
    }

    /// Indices in the Y-bands within ±2 of `y`'s band (superset of `|Δy| ≤ band`).
    pub(super) fn y_band_candidates<'a>(
        y_index: &'a HashMap<i32, Vec<usize>>,
        y: f32,
        band: f32,
    ) -> impl Iterator<Item = usize> + 'a {
        let k = (y / band).round() as i32;
        (k - 2..=k + 2).flat_map(move |b| y_index.get(&b).into_iter().flatten().copied())
    }

    pub(super) fn span_is_token_internal(
        spans: &[crate::layout::TextSpan],
        i: usize,
        y_index: &HashMap<i32, Vec<usize>>,
        band: f32,
    ) -> bool {
        let curr = &spans[i];
        let curr_y = curr.bbox.y;
        let curr_x = curr.bbox.x;
        let curr_right = curr.bbox.x + curr.bbox.width;
        let body_fs = Self::y_band_candidates(y_index, curr_y, band)
            .filter(|&j| (spans[j].bbox.y - curr_y).abs() <= 4.0)
            .map(|j| spans[j].font_size)
            .fold(0f32, f32::max)
            .max(1.0);
        let neighbour_fs_min = body_fs * 0.85;
        let max_em = body_fs;
        let mut has_left = false;
        let mut has_right = false;
        for j in Self::y_band_candidates(y_index, curr_y, band) {
            if j == i {
                continue;
            }
            let s = &spans[j];
            if (s.bbox.y - curr_y).abs() > 4.0 {
                continue;
            }
            if s.font_size < neighbour_fs_min {
                continue;
            }
            // Anchor must start or end with an alphabetic character
            // — a digit or punctuation neighbour does not signal a
            // token-internal context.
            let s_right = s.bbox.x + s.bbox.width;
            // Allow small overlap (super/sub glyphs nest slightly
            // under the body letter's bounding box).
            let dx_left = curr_x - s_right;
            if s_right < curr_right
                && dx_left <= max_em
                && dx_left >= -max_em * 0.5
                && s.text
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphabetic())
            {
                has_left = true;
            }
            let dx_right = s.bbox.x - curr_right;
            if s.bbox.x > curr_x
                && dx_right <= max_em
                && dx_right >= -max_em * 0.5
                && s.text.chars().next().is_some_and(|c| c.is_alphabetic())
            {
                has_right = true;
            }
        }
        has_left && has_right
    }

    /// Return per-page font statistics for use in heading detection and layout analysis.
    ///
    /// [`crate::layout::PageFontStats`] contains:
    /// - `dominant_em`: the mode font size weighted by character count — the body text "1 em"
    /// - `dominant_line_height`: median baseline-to-baseline distance
    /// - `dominant_char_width`: average character advance width
    /// - `body_font_name`: name of the most-used font
    ///
    /// The primary use-case is heading detection in downstream tools: compare
    /// `span.font_size / stats.dominant_em` against a threshold (e.g. 1.4×
    /// for H2, 1.8× for H1) to classify large-font spans as headings without
    /// depending on any hardcoded point sizes.
    ///
    /// ```ignore
    /// let stats = doc.page_font_stats(0)?;
    /// let spans = doc.extract_spans(0)?;
    /// for span in &spans {
    ///     let ratio = span.font_size / stats.dominant_em;
    ///     if ratio >= 1.8 { println!("H1: {}", span.text); }
    ///     else if ratio >= 1.4 { println!("H2: {}", span.text); }
    /// }
    /// ```
    pub fn page_font_stats(&self, page_index: usize) -> Result<crate::layout::PageFontStats> {
        let spans = self.extract_spans(page_index)?;
        Ok(crate::layout::PageFontStats::from_spans(&spans))
    }

    /// Return all extraction warnings accumulated since this document was opened.
    ///
    /// Warnings are recorded when silent fallbacks occur during text extraction
    /// (e.g., missing ToUnicode CMap, font not found, malformed structure tree).
    /// They do NOT consume the warning list — use [`Self::take_warnings`] to drain it.
    ///
    /// This API makes previously invisible extraction degradations programmatically
    /// observable without requiring callers to hook into the `log` crate.
    pub fn warnings(&self) -> Vec<String> {
        self.accumulated_warnings.lock_or_recover().clone()
    }

    /// Drain and return all accumulated extraction warnings, clearing the list.
    ///
    /// After this call, [`Self::warnings`] returns an empty `Vec` until new warnings
    /// are generated. Useful for incremental processing pipelines that want to
    /// inspect warnings on a per-page or per-operation basis.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.accumulated_warnings.lock_or_recover())
    }

    /// Record an extraction warning. Called internally when a silent fallback occurs.
    pub(crate) fn push_warning(&self, msg: impl Into<String>) {
        self.accumulated_warnings.lock_or_recover().push(msg.into());
    }

    /// Return the document's accumulated structured warnings as a
    /// snapshot. Each entry carries the warning's
    /// [`WarningCategory`](crate::extractors::warnings::WarningCategory),
    /// page (if applicable), human-readable message, and PDF spec
    /// section reference (when applicable).
    ///
    /// Unlike [`Self::warnings`] which returns plain strings, this
    /// accessor returns structured records callers can filter, route
    /// to observability dashboards, or assert on in tests without
    /// parsing message text. Pairs with the `pyo3_log` per-target
    /// default-level downgrade to give Python users a clean stderr
    /// experience plus an opt-in structured surface.
    ///
    /// Returns the warnings in insertion order. The vector is
    /// non-destructive: subsequent calls return the same entries
    /// plus any new ones pushed since the last call. Use
    /// [`Self::take_structured_warnings`] to drain.
    ///
    /// Renamed from `flatten_warnings` in to avoid colliding
    /// with the pre-existing `DocumentEditor::flatten_warnings`
    /// (which returns the form-flattening side-effect log, a
    /// `&[String]` — different feature). Both the Rust and Python
    /// (`PyDocument`) surfaces now agree on `structured_warnings`.
    pub fn structured_warnings(&self) -> Vec<crate::extractors::warnings::Warning> {
        self.warning_sink.snapshot()
    }

    /// Drain and return all accumulated structured warnings.
    /// Companion to [`Self::structured_warnings`].
    pub fn take_structured_warnings(&self) -> Vec<crate::extractors::warnings::Warning> {
        self.warning_sink.take()
    }

    /// Record a structured warning. Hook called from migrated
    /// `log::warn!` sites that also want to surface the warning as
    /// structured data.
    ///
    /// Exposed as `pub` so external diagnostic sources (custom
    /// extractors, FFI hooks) can also push warnings into the same
    /// sink that [`Self::structured_warnings`] surfaces.
    pub fn push_structured_warning(&self, warning: crate::extractors::warnings::Warning) {
        self.warning_sink.push(warning);
    }

    /// Heuristic: does this page have two or more vertical text columns?
    ///
    /// Used by `extract_spans` to decide whether to pay the XY-cut cost
    /// (correct but slower on large pages) or stick with the cheap row-
    /// aware sort. The check bins span X-centers into a small histogram
    /// and looks for two dense bands separated by a gutter whose spans
    /// vertically overlap with each other — that's the defining shape
    /// of a multi-column layout (newspaper / academic / dashboard) as
    /// opposed to sparse side-notes that flank a single column.
    ///
    /// False negatives (missed multi-column page) just mean we use the
    /// old reading order. False positives (single column routed through
    /// XY-cut) cost a bit of CPU but produce the same or better result.
    /// Both sides degrade gracefully.
    /// True when the page splits into side-by-side columns separated by a clean
    /// vertical gutter that no text span crosses.
    ///
    /// This is the small-page companion to the histogram detector in
    /// [`Self::is_multi_column_page`]: a two-column page with only a handful of
    /// wrapped lines per column (a short article, a synthetic fixture) carries
    /// too few spans for a projection histogram to classify, yet the gutter is
    /// perfectly unambiguous. We recover it directly:
    ///
    /// 1. Drop spans whose width exceeds 60 % of the content width — full-bleed
    ///    headings/footers legitimately straddle the gutter and must not veto it
    ///    (the recursive XY-Cut handles them with a horizontal cut first).
    /// 2. Sweep the remaining boxes left-to-right merging their X extents; a
    ///    forward jump of ≥ `MIN_GUTTER_PT` between the running right edge and
    ///    the next box's left edge is an empty channel that no span crosses.
    /// 3. Accept only when ≥ 2 spans sit on each side (genuine columns, not a
    ///    stray indent or page number) and the two sides' vertical ranges
    ///    overlap (columns sit beside each other, ruling out stacked blocks).
    pub(super) fn has_clean_column_gutter(spans: &[crate::layout::TextSpan]) -> bool {
        /// Minimum empty-channel width. Real column gutters run ≥ 18pt; ordinary
        /// inter-word/inter-cell gaps are both narrower and crossed by spans on
        /// other lines, so they never survive the sweep.
        const MIN_GUTTER_PT: f32 = 18.0;

        // (x0, x1, y0, y1) for every non-empty, finite span.
        let mut boxes: Vec<(f32, f32, f32, f32)> = spans
            .iter()
            .filter(|s| {
                !s.text.trim().is_empty()
                    && s.bbox.x.is_finite()
                    && s.bbox.y.is_finite()
                    && s.bbox.width.is_finite()
                    && s.bbox.height.is_finite()
                    && s.bbox.width > 0.0
            })
            .map(|s| {
                (
                    s.bbox.x,
                    s.bbox.x + s.bbox.width,
                    s.bbox.y,
                    s.bbox.y + s.bbox.height,
                )
            })
            .collect();
        if boxes.len() < 4 {
            return false;
        }

        let content_min_x = boxes.iter().map(|b| b.0).fold(f32::INFINITY, f32::min);
        let content_max_x = boxes.iter().map(|b| b.1).fold(f32::NEG_INFINITY, f32::max);
        let content_w = content_max_x - content_min_x;
        if content_w < 100.0 {
            return false; // a single narrow column cannot hold a gutter
        }

        // Exclude full-width headings/footers from the gutter sweep.
        boxes.retain(|b| (b.1 - b.0) <= 0.6 * content_w);
        if boxes.len() < 4 {
            return false;
        }
        boxes.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));

        // Grid-row guard. A two-column body has exactly one text run per column
        // on a line, so a row carries at most one wide internal gap (the
        // gutter). A table / form row instead has several cells, i.e. two or
        // more wide internal gaps. Group the (heading-excluded) boxes into rows
        // by baseline and, when the majority of multi-box rows carry ≥ 2
        // significant internal gaps, treat the page as a grid and bail — a
        // single wide middle gap on a 2×N cell grid would otherwise read as a
        // lone gutter. Mirrors the grid-row discriminator on the histogram path.
        const MIN_GAP_PT: f32 = 6.0;
        let mut rows: std::collections::BTreeMap<i32, Vec<(f32, f32)>> =
            std::collections::BTreeMap::new();
        for &(x0, x1, _y0, y1) in &boxes {
            rows.entry(y1.round() as i32).or_default().push((x0, x1));
        }
        let (mut multi_gap_rows, mut counted_rows) = (0usize, 0usize);
        for cells in rows.values() {
            if cells.len() < 2 {
                continue;
            }
            let mut s = cells.clone();
            s.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
            let gaps = s
                .windows(2)
                .filter(|w| w[1].0 - w[0].1 >= MIN_GAP_PT)
                .count();
            counted_rows += 1;
            if gaps >= 2 {
                multi_gap_rows += 1;
            }
        }
        if counted_rows > 0 && multi_gap_rows * 2 >= counted_rows {
            return false; // grid / form, not a two-column body
        }

        // Sweep-merge X extents and collect EVERY ≥ MIN_GUTTER_PT forward jump
        // (a vertical channel no span crosses). A genuine two-column body has
        // exactly ONE such corridor — the gutter; the lines inside each column
        // overlap horizontally, so a column's own extents merge into a single
        // contiguous run. A short-cell table (numeric grid, form) instead leaves
        // a corridor between every cell column, so two or more qualifying gaps
        // means a grid / multi-region layout, not two columns — reject it
        // (matching the grid-row discriminator on the histogram path).
        let mut cover_right = boxes[0].1;
        let mut gutter_splits: Vec<usize> = Vec::new();
        for i in 1..boxes.len() {
            if boxes[i].0 - cover_right >= MIN_GUTTER_PT {
                gutter_splits.push(i);
            }
            cover_right = cover_right.max(boxes[i].1);
        }
        if gutter_splits.len() != 1 {
            return false; // 0 = single column; ≥ 2 = grid / multi-region
        }

        let (left, right) = boxes.split_at(gutter_splits[0]);
        if left.len() < 2 || right.len() < 2 {
            return false;
        }
        // Vertical ranges of the two sides must overlap — otherwise the
        // "columns" are vertically stacked blocks (e.g. a body block above a
        // sidebar), which read fine row-aware.
        let l_y0 = left.iter().map(|b| b.2).fold(f32::INFINITY, f32::min);
        let l_y1 = left.iter().map(|b| b.3).fold(f32::NEG_INFINITY, f32::max);
        let r_y0 = right.iter().map(|b| b.2).fold(f32::INFINITY, f32::min);
        let r_y1 = right.iter().map(|b| b.3).fold(f32::NEG_INFINITY, f32::max);
        let overlap = l_y1.min(r_y1) - l_y0.max(r_y0);
        let min_height = (l_y1 - l_y0).min(r_y1 - r_y0);
        min_height > 0.0 && overlap > 0.5 * min_height
    }

    /// Gutter X for a page that is genuinely **two-column PROSE** (#734), or
    /// `None`. Content-balance discriminator (corpus-measured): rejects forms
    /// (`label:value`), TOCs (`title…page#`), tables and N-up — all of which
    /// share a clean gutter but must read row-wise. A real two-column body has
    /// full-length text on both sides of the gutter.
    /// Measure a single central vertical gutter (a column-separating whitespace
    /// corridor) as a PURE geometric read. Returns the gutter's mid-X when the
    /// page has EXACTLY ONE corridor ≥ `MIN_GUTTER_PT` wide near mid-page
    /// (`0.30..=0.70` of content width); `None` for single-column, multi-corridor
    /// (grid/table/form), off-centre, or too-narrow pages — so a caller that
    /// gates on `Some` is byte-identical on all of those.
    ///
    /// Shared by the marginalia pre-filter (Item 2) and the topological
    /// union-find gutter veto (Item 4). Deliberately NOT a refactor of
    /// `prose_two_column_gutter` / `has_clean_column_gutter`: those use different
    /// corridor thresholds (12 / 18) and additional structural guards, and
    /// unifying them is high blast radius for no benefit. This is a separate,
    /// conservative 18 pt central-corridor probe.
    #[allow(dead_code)] // wired by Items 2 (P3) and 4 (P4)
    pub(super) fn measure_single_central_gutter(spans: &[crate::layout::TextSpan]) -> Option<f32> {
        const MIN_GUTTER_PT: f32 = 18.0;
        let body: Vec<&crate::layout::TextSpan> = spans
            .iter()
            .filter(|s| {
                !s.text.trim().is_empty()
                    && s.bbox.width > 0.0
                    && s.bbox.x.is_finite()
                    && s.bbox.width.is_finite()
            })
            .collect();
        if body.len() < 8 {
            return None;
        }
        let cmin = body.iter().map(|s| s.bbox.x).fold(f32::INFINITY, f32::min);
        let cmax = body
            .iter()
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);
        let content_w = cmax - cmin;
        if content_w < 100.0 {
            return None;
        }
        // Exclude full-width spanning rows (headings/footers) so they don't mask
        // the corridor (same exclusion as the prose/clean-gutter sweeps).
        let mut boxes: Vec<(f32, f32)> = body
            .iter()
            .filter(|s| s.bbox.width <= 0.6 * content_w)
            .map(|s| (s.bbox.x, s.bbox.x + s.bbox.width))
            .collect();
        if boxes.len() < 8 {
            return None;
        }
        boxes.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
        let mut cover = boxes[0].1;
        let (mut corridors, mut gutter_x) = (0usize, 0.0f32);
        for &(l, r) in &boxes[1..] {
            if l - cover >= MIN_GUTTER_PT {
                corridors += 1;
                gutter_x = (cover + l) * 0.5;
            }
            cover = cover.max(r);
        }
        if corridors != 1 || !(0.30..=0.70).contains(&((gutter_x - cmin) / content_w)) {
            return None;
        }
        Some(gutter_x)
    }

    /// Valley-DEPTH central gutter probe. Like `measure_single_central_gutter`,
    /// returns the mid-X of a single central column-separating corridor, but uses
    /// a 2-D span-PROJECTION density (the emptiest vertical channel over the whole
    /// Y-extent) instead of a 1-D running-cover scan. This finds gutters that the
    /// cover scan misses because a full-width header/footer that is NOT quite wide
    /// enough to be band-excluded (it spans, say, 0.55 of the content width) jumps
    /// the running cover past the corridor; the projection only counts spans that
    /// actually straddle a given x, so a single bridging line is absorbed by the
    /// tolerance. It also catches the TIGHT (≈ 10–14 pt) real gutters of dense
    /// two-column journal bodies, below the conservative 18 pt cover threshold.
    ///
    /// A true gutter is a vertical band of near-zero straddle density; a phantom
    /// word/indent gap has moderate density (many lines carry text there). Returns
    /// the gutter mid-X only when EXACTLY ONE such near-empty central corridor of
    /// real width exists; `None` otherwise. Used (OR-ed with the cover scan) as
    /// the topological union-find gutter veto, so it can only PREVENT a
    /// cross-gutter union — never create one — keeping non-2-column pages
    /// byte-identical.
    pub(super) fn density_central_gutter(spans: &[crate::layout::TextSpan]) -> Option<f32> {
        let finite = |s: &crate::layout::TextSpan| {
            !s.text.trim().is_empty()
                && s.bbox.width > 0.0
                && s.bbox.x.is_finite()
                && s.bbox.width.is_finite()
        };
        let body: Vec<&crate::layout::TextSpan> = spans.iter().filter(|s| finite(s)).collect();
        if body.len() < 12 {
            return None;
        }
        let cmin = body.iter().map(|s| s.bbox.x).fold(f32::INFINITY, f32::min);
        let cmax = body
            .iter()
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);
        let content_w = cmax - cmin;
        if !content_w.is_finite() || content_w < 100.0 {
            return None;
        }
        // Column-content spans only (exclude true full-width bands). A real
        // gutter is invisible under titles/abstracts/footers, so they must not
        // count toward straddle density.
        let band_w = 0.6 * content_w;
        let cols: Vec<(f32, f32)> = body
            .iter()
            .filter(|s| s.bbox.width <= band_w)
            .map(|s| (s.bbox.x, s.bbox.x + s.bbox.width))
            .collect();
        if cols.len() < 12 {
            return None;
        }
        // Scan the central band; "empty" tolerates ~1 % stray straddlers (a rare
        // long token or a header just under the band-exclusion width).
        let lo = cmin + 0.30 * content_w;
        let hi = cmin + 0.70 * content_w;
        let step = (content_w / 400.0).clamp(0.5, 3.0);
        let empty_max = (0.01 * cols.len() as f32).ceil() as usize;
        let straddle_at = |x: f32| -> usize {
            cols.iter()
                .filter(|(l, r)| *l + 2.0 < x && *r - 2.0 > x)
                .count()
        };
        // Find ALL near-empty corridors and the widest one; require EXACTLY ONE
        // (a 3-column grid has two, and must stay row-aware).
        let (mut corridors, mut best_w, mut best_mid) = (0usize, 0.0f32, f32::NAN);
        let (mut run_start, mut in_run) = (lo, false);
        let mut x = lo;
        let close = |run_start: f32,
                     end: f32,
                     corridors: &mut usize,
                     best_w: &mut f32,
                     best_mid: &mut f32| {
            let w = end - run_start;
            if w >= 6.0 {
                *corridors += 1;
                if w > *best_w {
                    *best_w = w;
                    *best_mid = (run_start + end) * 0.5;
                }
            }
        };
        while x <= hi {
            if straddle_at(x) <= empty_max {
                if !in_run {
                    run_start = x;
                    in_run = true;
                }
            } else if in_run {
                close(run_start, x, &mut corridors, &mut best_w, &mut best_mid);
                in_run = false;
            }
            x += step;
        }
        if in_run {
            close(run_start, hi, &mut corridors, &mut best_w, &mut best_mid);
        }
        if corridors != 1 || !best_mid.is_finite() {
            return None;
        }
        // Balanced columns: each side carries a real share of the column spans
        // (rejects a single column beside a sparse margin rail).
        let (mut left, mut right) = (0usize, 0usize);
        for (l, r) in &cols {
            if (l + r) * 0.5 < best_mid {
                left += 1;
            } else {
                right += 1;
            }
        }
        let n = left + right;
        if n == 0 || (left * 4 < n) || (right * 4 < n) {
            return None;
        }
        Some(best_mid)
    }

    /// Characters-per-text-line density for a set of spans (≈ chars per line).
    /// Lines are counted by clustering span upper edges (`bbox.bottom()`, larger
    /// y) with a `med_h * 0.6` gap. A page-number rail or a form's value column
    /// is text-SPARSE (a few chars per line); genuine prose columns and metadata
    /// sidebars are text-DENSE. Shared by the topological side-by-side gate
    /// (Item 1) and the marginalia sparsity gate (Item 2) — same formula the
    /// `topological_block_order` `char_density` closure uses.
    #[allow(dead_code)] // wired by Items 1 (P4) and 2 (P3)
    pub(super) fn block_char_density(spans: &[&crate::layout::TextSpan], med_h: f32) -> f32 {
        if spans.is_empty() {
            return 0.0;
        }
        let med_h = med_h.max(1.0);
        let mut ys: Vec<f32> = spans.iter().map(|s| s.bbox.bottom()).collect();
        ys.sort_by(|p, q| crate::utils::safe_float_cmp(*p, *q));
        let mut lines = 1usize;
        for w in ys.windows(2) {
            if (w[1] - w[0]).abs() > med_h * 0.6 {
                lines += 1;
            }
        }
        let chars: usize = spans.iter().map(|s| s.text.trim().chars().count()).sum();
        chars as f32 / lines as f32
    }
}
