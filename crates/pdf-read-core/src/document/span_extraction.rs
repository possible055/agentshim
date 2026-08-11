use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Extract text from a Tagged PDF page using pre-computed structure traversal cache.
    ///
    /// This is the optimized version of `extract_text_structure_order` that uses
    /// the pre-built `structure_content_cache` for O(1) page content lookup instead
    /// of re-traversing the entire structure tree for each page.
    pub(super) fn extract_text_structure_order_cached_with_spans(
        &self,
        page_index: usize,
        all_spans: Vec<TextSpan>,
        include_artifacts: bool,
    ) -> Result<String> {
        log::debug!(
            "Extracting text using cached structure order for page {}",
            page_index
        );

        if all_spans.is_empty() {
            let mut text = String::new();
            self.append_non_widget_annotation_text(page_index, &mut text);
            return Ok(text);
        }

        // Drop content marked /Artifact (PDF Spec ISO 32000-1:2008
        // §14.8.2.2 — headers, footers, page numbers, decorations) —
        // unless the caller opted in via `include_artifacts` (default
        // true). The geometric branch in `assemble_text_from_spans`
        // applies the same filter; tagged PDFs taking the structure-order
        // path must honour it too, otherwise artifact spans (including
        // any MC-scope `/ActualText` replacements inside an `/Artifact`
        // BDC) leak into output. Untagged-PDF running-header
        // detection runs at document level and feeds the same flag.
        let all_spans: Vec<TextSpan> = if include_artifacts {
            all_spans
        } else {
            all_spans
                .into_iter()
                .filter(|s| s.artifact_type.is_none())
                .collect()
        };

        // Step 2: Build MCID → Vec<TextSpan> map
        let mut mcid_map: HashMap<u32, Vec<TextSpan>> = HashMap::new();
        let mut spans_without_mcid: Vec<TextSpan> = Vec::new();

        for span in all_spans {
            if let Some(mcid) = span.mcid {
                mcid_map.entry(mcid).or_default().push(span);
            } else {
                spans_without_mcid.push(span);
            }
        }

        // Step 3: Get pre-computed ordered content for this page (O(1) lookup)
        let ordered_content_owned: Vec<crate::structure::OrderedContent>;
        let ordered_content = {
            let cache = self.structure_content_cache.lock_or_recover();
            ordered_content_owned = cache
                .as_ref()
                .and_then(|c| c.get(&(page_index as u32)))
                .cloned()
                .unwrap_or_default();
            &ordered_content_owned as &[crate::structure::OrderedContent]
        };

        // Resolve struct-tree-scope `/ActualText` via the mcid-driven
        // action map (see `actualtext_actions_for_page`). The index is
        // built once per document (cached). For untagged documents the
        // map stays empty and the assembler behaves exactly as before.
        let at_index = self.actualtext_index();
        // MC-scope-wins precedence set: MCIDs whose BDC carried inline
        // `/ActualText` keep the in-stream replacement (most specific
        // declaration) and are exempt from ancestor struct-tree
        // emissions.
        let mc_wins: HashSet<u32> = self
            .mc_actualtext_mcids
            .lock_or_recover()
            .get(&page_index)
            .cloned()
            .unwrap_or_default();
        let default_scope = crate::structure::McidScope::Page(page_index as u32);
        let mcid_order: Vec<(crate::structure::McidScope, u32)> = ordered_content
            .iter()
            .filter_map(|c| {
                c.mcid
                    .map(|m| (c.mcid_scope.clone().unwrap_or(default_scope.clone()), m))
            })
            .collect();
        // Per-key rendered glyph text for the §14.9.4 conformance gate.
        let mut glyph_text: HashMap<(crate::structure::McidScope, u32), String> = HashMap::new();
        for (scope, m) in &mcid_order {
            if let Some(sp) = mcid_map.get(m) {
                let joined: String = sp.iter().map(|s| s.text.as_str()).collect();
                glyph_text
                    .entry((scope.clone(), *m))
                    .or_default()
                    .push_str(&joined);
            }
        }
        let actions = Self::actualtext_actions_for_page(
            at_index.as_deref(),
            &mcid_order,
            |_scope, m| mcid_map.contains_key(&m),
            &mc_wins,
            &glyph_text,
        );

        log::debug!(
            "Cached structure content: {} items for page {}, {} MCIDs with spans, {} ActualText actions on this page",
            ordered_content.len(),
            page_index,
            mcid_map.len(),
            actions.len()
        );

        // Step 4: Assemble text in structure order
        let mut text = String::with_capacity(mcid_map.len() * 50);
        let mut prev_span: Option<TextSpan> = None;
        let mut prev_in_table = false;
        let mut consumed_mcids: HashSet<u32> = HashSet::new();

        for content in ordered_content {
            if content.is_word_break {
                if !text.is_empty() && !text.ends_with(' ') && !text.ends_with('\n') {
                    text.push(' ');
                }
                continue;
            }

            let Some(mcid) = content.mcid else {
                continue;
            };
            // ISO 32000-1 §14.7: a marked-content sequence's MCID is unique
            // within its content stream and is referenced from the structure
            // hierarchy at most once. A malformed struct tree that re-references
            // the same MCID multiple times would otherwise emit that MCID's
            // glyphs once per reference. Emit each MCID once. (A destructive
            // /ActualText replacement — now declined by the §14.9.4 conformance
            // gate — can mask this by collapsing each consecutive run to a
            // single emit.)
            if !consumed_mcids.insert(mcid) {
                continue;
            }
            let mcid_scope_key = content.mcid_scope.clone().unwrap_or(default_scope.clone());

            match actions.get(&(mcid_scope_key, mcid)) {
                Some(ActualTextAction::EmitAndSuppress(repl)) => {
                    consumed_mcids.insert(mcid);
                    if !text.is_empty() && !text.ends_with(' ') && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(repl);
                    continue;
                }
                Some(ActualTextAction::Suppress) => {
                    consumed_mcids.insert(mcid);
                    continue;
                }
                None => {}
            }

            if let Some(spans) = mcid_map.get(&mcid) {
                consumed_mcids.insert(mcid);
                let rtl_run = Self::mcid_run_is_pure_rtl(spans);
                // Repair the cross-span Arabic glyph-interleave defect (zero-width
                // mark/consonant spans landing at word edges) before ordering.
                let merged_rtl = Self::merge_interleaved_rtl_lines(spans);
                let use_spans: &[crate::layout::TextSpan] = merged_rtl.as_deref().unwrap_or(spans);
                for span in Self::order_mcid_spans(use_spans) {
                    if let Some(prev) = &prev_span {
                        let y_diff = (prev.bbox.y - span.bbox.y).abs();
                        if y_diff > Self::same_line_threshold(prev, span) {
                            // Suppress the break when a Hangul eojeol wrapped
                            // mid-syllable (no inter-eojeol space at the wrap), so
                            // the word stays whole for word-segmentation scoring.
                            if !Self::hangul_midword_line_wrap(&text, prev, span) {
                                Self::push_line_breaks(
                                    &mut text,
                                    prev,
                                    span,
                                    y_diff,
                                    content.in_table && prev_in_table,
                                );
                            }
                        } else if Self::should_insert_space(prev, span)
                            || Self::stacked_cell_needs_space(prev, span)
                        {
                            text.push(' ');
                        }
                    }

                    Self::push_span_text_bidi(&mut text, span, rtl_run);
                    prev_span = Some(span.clone());
                    prev_in_table = content.in_table;
                }
            }
        }

        // Append spans with MCIDs not referenced by the structure tree
        let mut unconsumed: Vec<(&u32, &Vec<TextSpan>)> = mcid_map
            .iter()
            .filter(|(mcid, _)| !consumed_mcids.contains(mcid))
            .collect();
        unconsumed.sort_by_key(|(mcid, _)| **mcid);
        if !unconsumed.is_empty() {
            log::debug!(
                "Appending {} unreferenced MCIDs (e.g., from Form XObjects without StructParents)",
                unconsumed.len()
            );
            for (_mcid, spans) in &unconsumed {
                let rtl_run = Self::mcid_run_is_pure_rtl(spans);
                for span in *spans {
                    if let Some(prev) = &prev_span {
                        let y_diff = (prev.bbox.y - span.bbox.y).abs();
                        if y_diff > Self::same_line_threshold(prev, span) {
                            text.push('\n');
                        } else if Self::should_insert_space(prev, span) {
                            text.push(' ');
                        }
                    }
                    Self::push_span_text_bidi(&mut text, span, rtl_run);
                    prev_span = Some(span.clone());
                }
            }
        }

        // Append any spans without MCID (including widget/form field spans) sorted by position
        if !spans_without_mcid.is_empty() {
            log::debug!(
                "Found {} text spans without MCID (including form field widgets) - appending sorted by position",
                spans_without_mcid.len()
            );
            // Row-aware sort: Y-band descending (top→bottom), then X ascending.
            crate::utils::sort_by_row_band(&mut spans_without_mcid, |s| s.bbox.y, |s| s.bbox.x);
            for span in &spans_without_mcid {
                if let Some(prev) = &prev_span {
                    let y_diff = (prev.bbox.y - span.bbox.y).abs();
                    if y_diff > Self::same_line_threshold(prev, span) {
                        text.push('\n');
                    } else if Self::should_insert_space(prev, span) {
                        text.push(' ');
                    }
                }
                Self::push_span_text_bidi(&mut text, span, false);
                prev_span = Some(span.clone());
            }
        }

        // Annotation text is already included via annotation_content_spans() in
        // extract_spans() — do NOT call append_non_widget_annotation_text() here
        // (would cause double-emission of all annotation text).

        Ok(text)
    }

    /// Extract text spans from a page (PDF spec compliant - RECOMMENDED).
    ///
    /// This is the recommended method for text extraction. It extracts complete
    /// text strings as the PDF provides them via Tj/TJ operators, following the
    /// PDF specification ISO 32000-1:2008.
    ///
    /// # Benefits over extract_chars
    /// - Avoids overlapping character issues
    /// - Preserves PDF's text positioning intent
    /// - More robust for complex layouts
    /// - Matches industry best practices (PyMuPDF, etc.)
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    ///
    /// # Returns
    ///
    /// Vector of TextSpan objects in reading order
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::PdfDocument;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("document.pdf")?;
    /// let spans = doc.extract_spans(0)?;
    /// for span in spans {
    ///     println!("Text: {} at ({}, {})", span.text, span.bbox.x, span.bbox.y);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_spans(&self, page_index: usize) -> Result<Vec<crate::layout::TextSpan>> {
        // Serve repeat per-page extractions from cache (the converters reach
        // here twice per page; see `page_spans_cache`).
        if let Some(cached) = self.page_spans_cache.lock_or_recover().get(&page_index) {
            return Ok((**cached).clone());
        }
        let spans = self.extract_spans_raw(page_index)?;
        let spans = self.postprocess_spans(page_index, spans)?;
        self.page_spans_cache
            .lock_or_recover()
            .insert(page_index, std::sync::Arc::new(spans.clone()));
        Ok(spans)
    }

    pub(super) fn extract_spans_filtered(
        &self,
        page_index: usize,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        let spans = self.extract_spans_raw_filtered(page_index, excluded_layers, excluded_inks)?;
        self.postprocess_spans(page_index, spans)
    }

    /// Map a span rectangle (already translated so the page origin is at
    /// `(0, 0)`) through a clockwise page `/Rotate` of `rot` degrees, returning
    /// the axis-aligned bounding box in the displayed coordinate frame.
    ///
    /// `page_w` / `page_h` are the unrotated page dimensions; for 90° / 270° the
    /// displayed page is `page_h × page_w`. Per ISO 32000-1:2008 §7.7.3.3 the
    /// rotation is clockwise and §8.3.3 gives the point transform. `rot` must be
    /// a normalised multiple of 90 (`0/90/180/270`); any other value returns the
    /// rectangle unchanged. `rot == 0` is the identity and `rot == 180` is
    /// numerically identical to the legacy mirror, preserving byte-for-byte
    /// output for unrotated and 180° pages.
    pub(crate) fn rotate_span_bbox(
        bbox: crate::geometry::Rect,
        rot: i32,
        page_w: f32,
        page_h: f32,
    ) -> crate::geometry::Rect {
        // Map a point (y-up) by the clockwise display rotation.
        let map = |x: f32, y: f32| -> (f32, f32) {
            match rot {
                90 => (y, page_w - x),
                180 => (page_w - x, page_h - y),
                270 => (page_h - y, x),
                _ => (x, y),
            }
        };
        let (ax, ay) = map(bbox.x, bbox.y);
        let (bx, by) = map(bbox.x + bbox.width, bbox.y + bbox.height);
        crate::geometry::Rect::new(ax.min(bx), ay.min(by), (ax - bx).abs(), (ay - by).abs())
    }

    /// Map a single span's bbox into the displayed frame for a `/Rotate`d page
    /// (translate to origin → [`rotate_span_bbox`] → translate back).
    pub(super) fn map_span_into_rotated_frame(
        s: &mut crate::layout::TextSpan,
        rot: i32,
        llx: f32,
        lly: f32,
        w: f32,
        h: f32,
    ) {
        let rel =
            crate::geometry::Rect::new(s.bbox.x - llx, s.bbox.y - lly, s.bbox.width, s.bbox.height);
        let m = Self::rotate_span_bbox(rel, rot, w, h);
        s.bbox.x = llx + m.x;
        s.bbox.y = lly + m.y;
        s.bbox.width = m.width;
        s.bbox.height = m.height;
    }

    /// Order rotated runs that were segregated out of the horizontal reading
    /// flow. Spans drawn with a rotated text matrix (`rotation_degrees != 0`)
    /// break the axis-aligned assumptions of the row-band / XY-cut sort, so they
    /// are pulled out, ordered here, and appended as their own blocks. Runs are
    /// grouped by rotation (first-seen group order preserved); within a group
    /// each span's origin is rotated back into an upright frame and the standard
    /// row-aware comparator (top→bottom, left→right) is applied there.
    pub(crate) fn order_rotated_blocks(
        spans: Vec<crate::layout::TextSpan>,
    ) -> Vec<crate::layout::TextSpan> {
        let mut groups: Vec<(f32, Vec<crate::layout::TextSpan>)> = Vec::new();
        for s in spans {
            let key = s.rotation_degrees;
            match groups.iter_mut().find(|(k, _)| (*k - key).abs() < 0.5) {
                Some(g) => g.1.push(s),
                None => groups.push((key, vec![s])),
            }
        }
        let mut out = Vec::new();
        for (deg, mut group) in groups {
            let (sin, cos) = (-deg).to_radians().sin_cos();
            // Upright frame: rotate each origin by -deg, then read top→bottom,
            // left→right exactly as horizontal text.
            group.sort_by(|a, b| {
                let ax = a.bbox.x * cos - a.bbox.y * sin;
                let ay = a.bbox.x * sin + a.bbox.y * cos;
                let bx = b.bbox.x * cos - b.bbox.y * sin;
                let by = b.bbox.x * sin + b.bbox.y * cos;
                crate::utils::row_aware_span_cmp(ay, ax, by, bx)
            });
            out.extend(group);
        }
        out
    }

    /// Re-attach an oversized lone leading capital (a drop-cap / table-title
    /// initial that the producer set in a larger font, so it became its own
    /// span) to the body run immediately to its right on the same line —
    /// otherwise reading-order strands it (`TABLE` → `T` … `ABLE`).
    ///
    /// Conservative gates so prose drop-caps / standalone capitals aren't glued
    /// to the wrong word: the candidate must be a single uppercase ASCII letter
    /// at ≥1.5× the body run's font size, its right edge within ~0.3 em of the
    /// body's left edge, vertically overlapping it, and the body must start with
    /// a letter. Runs in raw span order before reading-order sorting.
    pub(super) fn merge_drop_cap_initials(spans: &mut Vec<crate::layout::TextSpan>) {
        let n = spans.len();
        if n < 2 {
            return;
        }
        // A genuine drop cap is oversized relative to the page's *normal* body
        // text, not merely relative to its right-hand neighbor. Inline math such
        // as "A_st" pairs a normal-size capital with a shrunken subscript; gating
        // on the neighbor alone would treat that capital as oversized and glue
        // "A" + "st" into "Ast". Anchor the size gate to the median size of
        // multi-character spans (real words) so a body-size capital cannot
        // qualify.
        let mut body_sizes: Vec<f32> = spans
            .iter()
            .filter(|s| s.font_size > 0.0 && s.text.chars().nth(1).is_some())
            .map(|s| s.font_size)
            .collect();
        if body_sizes.is_empty() {
            return;
        }
        body_sizes.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        let body_size = body_sizes[body_sizes.len() / 2];

        // Span indices sorted by left edge, and the widest font on the page, so
        // each initial only probes spans whose left edge falls in its narrow
        // candidate x-window (was a full O(n) rescan per initial). A continuation
        // satisfies `gap in [-fs*0.5, fs*0.12]`, i.e. its left edge is within
        // [init_right - max_fs*0.5, init_right + max_fs*0.12]; using the page max
        // font widens the window conservatively, and the exact per-candidate gap
        // test below reproduces the original filter — so this is byte-identical.
        let order: Vec<usize> = {
            let mut o: Vec<usize> = (0..n).collect();
            o.sort_by(|&a, &b| crate::utils::safe_float_cmp(spans[a].bbox.x, spans[b].bbox.x));
            o
        };
        let max_fs = spans.iter().map(|s| s.font_size).fold(0.0_f32, f32::max);

        // For each initial candidate, the closest qualifying body span to its right.
        let mut target: Vec<Option<usize>> = vec![None; n];
        for i in 0..n {
            let init = &spans[i];
            if init.text.chars().count() != 1 || init.font_size <= 0.0 {
                continue;
            }
            if !init
                .text
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
            {
                continue;
            }
            if init.font_size < body_size * 1.5 {
                continue; // initial must be clearly oversized vs normal body text
            }
            let init_right = init.bbox.x + init.bbox.width;
            // Candidates: spans whose left edge is in the conservative window.
            // Collect their indices and visit in ASCENDING ORIGINAL ORDER so the
            // strict-`<` min keeps the same first-wins tie-break as the old scan.
            let lo_x = init_right - max_fs * 0.5;
            let hi_x = init_right + max_fs * 0.12;
            let lo = order.partition_point(|&k| spans[k].bbox.x < lo_x);
            let hi = order.partition_point(|&k| spans[k].bbox.x <= hi_x);
            let mut cands: Vec<usize> = order[lo..hi].to_vec();
            cands.sort_unstable();
            let mut best: Option<usize> = None;
            let mut best_gap = f32::MAX;
            for &j in &cands {
                let body = &spans[j];
                if j == i || body.font_size <= 0.0 {
                    continue;
                }
                if !body.text.chars().next().is_some_and(|c| c.is_alphabetic()) {
                    continue;
                }
                // Continuation shares the initial's baseline (same text line). A
                // tall oversized initial also vertically overlaps the line *above*
                // it, so a raw bbox-overlap test would let it reach up and steal a
                // word from the previous line (alice_old: the 16.8pt "A" of "A very
                // heavy weight" overlapping "Or if" → "OrAif"). Baseline proximity
                // (≈ bbox bottom) keeps the merge on the initial's own line.
                if (init.bbox.y - body.bbox.y).abs() > body.font_size * 0.5 {
                    continue;
                }
                // Body immediately to the right, essentially touching. A genuine
                // oversized initial is the first glyph of one word ("T" of
                // "TABLE", "P" of "PENALTY"), so its continuation begins within a
                // hair of the initial's advance — never across a word space. A
                // word-space gap (~0.25 em) would wrongly glue a standalone "A"
                // or "I" onto the next word ("A Perspective" → "APerspective"),
                // so the upper bound stays well below it.
                let gap = body.bbox.x - init_right;
                if gap < -body.font_size * 0.5 || gap > body.font_size * 0.12 {
                    continue;
                }
                if gap.abs() < best_gap {
                    best_gap = gap.abs();
                    best = Some(j);
                }
            }
            target[i] = best;
        }

        let mut taken = vec![false; n];
        let mut remove = vec![false; n];
        for i in 0..n {
            let Some(j) = target[i] else { continue };
            if taken[j] || remove[j] || remove[i] {
                continue; // a body receives at most one initial
            }
            taken[j] = true;
            remove[i] = true;
            let init_text = spans[i].text.clone();
            let init_left = spans[i].bbox.x;
            let body = &mut spans[j];
            body.text = format!("{init_text}{}", body.text);
            let right = body.bbox.x + body.bbox.width;
            body.bbox.x = init_left.min(body.bbox.x);
            body.bbox.width = right - body.bbox.x;
        }
        let mut k = 0;
        spans.retain(|_| {
            let keep = !remove[k];
            k += 1;
            keep
        });
    }

    /// True for Computer-Modern (`CM*`) or symbol font names, after stripping a
    /// `ABCDEF+` subset tag. Used to scope the `¬`→`.` decimal recovery.
    pub(super) fn is_cm_or_symbol_font(font_name: &str) -> bool {
        let base = font_name.split('+').next_back().unwrap_or(font_name);
        let lower = base.to_ascii_lowercase();
        lower.starts_with("cm") || lower.contains("symbol")
    }

    /// Replace a `¬` (U+00AC) that a math subset drew from its `logicalnot`
    /// slot as a decimal point. Two shapes are recovered:
    ///
    ///   - `digit ¬ digit`         → `digit.digit` (e.g. `1¬00` → `1.00`)
    ///   - `digit ¬ <space> digit` → `digit.digit` (e.g. `1¬ 00` → `1.00`)
    ///
    /// The second form covers subsets that emit a single space between the
    /// decimal glyph and the fractional digits; the lone separating space is
    /// dropped so the number reads as one token. The leading digit must abut
    /// `¬` directly in both shapes, so a genuinely spaced negation (`5 ¬ 3`,
    /// `A ¬ B`) is left untouched. Every other `¬` is preserved.
    pub(super) fn fix_digit_logicalnot_decimal(text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\u{00AC}' && i > 0 && chars[i - 1].is_ascii_digit() {
                // Unspaced: digit ¬ digit.
                if chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()) {
                    out.push('.');
                    i += 1;
                    continue;
                }
                // Spaced: digit ¬ <single space> digit — drop the lone space.
                if chars.get(i + 1) == Some(&' ')
                    && chars.get(i + 2).is_some_and(|n| n.is_ascii_digit())
                {
                    out.push('.');
                    i += 2; // skip the ¬ and the single separating space
                    continue;
                }
            }
            out.push(c);
            i += 1;
        }
        out
    }

    /// Drop spans whose bbox lies ENTIRELY outside the page's MediaBox.
    ///
    /// PDFs that reuse one big Form XObject across pages (ExpertPdf and similar
    /// tools - see issue B1 / nougat_005.pdf) rely on the content stream's `W n`
    /// clip rectangle to hide the off-page portion. The text extractor does not
    /// honour `W n` yet, so without this filter a page emits every page's worth of
    /// spans at distinct but out-of-bounds Y coordinates. Spans that even
    /// PARTIALLY overlap the MediaBox are kept, so legitimate bleed / trim-mark
    /// content is never dropped.
    ///
    /// `get_page_media_box` returns `(llx, lly, urx, ury)` - absolute corner
    /// coordinates per ISO 32000-1 s7.7.3.3, NOT `(x, y, width, height)`.
    pub(super) fn drop_offpage_spans(
        &self,
        page_index: usize,
        spans: &mut Vec<crate::layout::TextSpan>,
    ) {
        if let Ok((llx, lly, urx, ury)) = self.get_page_media_box(page_index) {
            const EDGE_TOLERANCE_PT: f32 = 2.0;
            // Normalise corners: some producers write the MediaBox with swapped
            // corners (e.g. `[0 792 612 0]`, ury < lly). Taking min/max makes the
            // bounds correct either way - without this a swapped box inverts the
            // test below and drops the whole page's legitimate text.
            let left = llx.min(urx) - EDGE_TOLERANCE_PT;
            let right = llx.max(urx) + EDGE_TOLERANCE_PT;
            let bottom = lly.min(ury) - EDGE_TOLERANCE_PT;
            let top = lly.max(ury) + EDGE_TOLERANCE_PT;
            spans.retain(|span| {
                let sx1 = span.bbox.x;
                let sx2 = span.bbox.x + span.bbox.width;
                let sy1 = span.bbox.y;
                let sy2 = span.bbox.y + span.bbox.height;
                sx2 > left && sx1 < right && sy2 > bottom && sy1 < top
            });
        }
    }
}
