use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Extract text using structure tree for Tagged PDFs.
    ///
    /// This method implements PDF spec-compliant text extraction for Tagged PDFs
    /// using the logical structure tree to determine reading order.
    ///
    /// # PDF Spec Reference
    ///
    /// ISO 32000-1:2008 Section 14.8.2.3 - Determining the Text Extraction Sequence
    /// "For a Tagged PDF document, conforming readers shall present the document's
    /// content to the user in the order given by a pre-order traversal of the
    /// structure hierarchy"
    ///
    /// # Algorithm
    /// 1. Extract all text spans with MCIDs from the page
    /// 2. Build a map from MCID → Vec<TextSpan>
    /// 3. Traverse structure tree in pre-order to get MCIDs in reading order
    /// 4. Assemble text by looking up spans for each MCID in order
    ///
    /// # Arguments
    /// * `page_index` - Zero-based page index
    /// * `struct_tree` - The structure tree root from the PDF catalog
    ///
    /// # Returns
    /// Extracted text in logical structure order
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // This is called automatically by extract_text() for Tagged PDFs
    /// let text = doc.extract_text(0)?;
    /// ```
    #[allow(dead_code)]
    pub(super) fn extract_text_structure_order(
        &self,
        page_index: usize,
        struct_tree: &crate::structure::StructTreeRoot,
    ) -> Result<String> {
        log::debug!(
            "Extracting text using structure tree for page {}",
            page_index
        );

        // Step 1: Extract all spans with MCIDs
        let all_spans = self.extract_spans(page_index)?;

        if all_spans.is_empty() {
            let mut text = String::new();
            self.append_non_widget_annotation_text(page_index, &mut text);
            return Ok(text);
        }

        // Step 2: Build MCID → Vec<TextSpan> map
        let mut mcid_map: HashMap<u32, Vec<TextSpan>> = HashMap::new();
        let mut spans_without_mcid: Vec<TextSpan> = Vec::new();

        for span in all_spans {
            if let Some(mcid) = span.mcid {
                mcid_map.entry(mcid).or_default().push(span);
            } else {
                // Collect spans without MCID (shouldn't happen in well-formed Tagged PDFs)
                spans_without_mcid.push(span);
            }
        }

        log::debug!(
            "Found {} MCIDs with spans, {} spans without MCID",
            mcid_map.len(),
            spans_without_mcid.len()
        );

        // Step 3: Traverse structure tree to get MCIDs in reading order
        let ordered_content = traverse_structure_tree(struct_tree, page_index as u32)
            .map_err(|e| Error::InvalidPdf(format!("Failed to traverse structure tree: {}", e)))?;

        log::debug!(
            "Structure tree traversal found {} content items in reading order",
            ordered_content.len()
        );

        // Resolve struct-tree-scope `/ActualText`. The mcid-driven
        // emission walk consults the cached index and assigns at most
        // one action per MCID — either "emit the replacement and
        // suppress this MCID's raw glyphs" or "suppress only".
        let at_index = self.actualtext_index();
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

        // Step 4: Assemble text in structure order
        let mut text = String::with_capacity(mcid_map.len() * 50); // estimate
        let mut prev_span: Option<TextSpan> = None;
        // Whether the content element that emitted `prev_span` sat inside a
        // table — used to collapse a table row boundary to a single newline.
        let mut prev_in_table = false;
        let mut consumed_mcids: std::collections::HashSet<u32> = std::collections::HashSet::new();

        for content in &ordered_content {
            // Handle word break markers by inserting a space
            if content.is_word_break {
                if !text.is_empty() && !text.ends_with(' ') && !text.ends_with('\n') {
                    text.push(' ');
                }
                continue;
            }

            // For regular content with MCID
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

            // ActualText action dispatch. `EmitAndSuppress` is set only
            // on the first visible covered MCID of a consecutive-same-
            // replacement run; subsequent MCIDs in the run carry
            // `Suppress`. MC-scope-wins MCIDs (their BDC carried inline
            // /ActualText) are exempt and walk the raw-span path so
            // the extractor's in-stream replacement reaches output.
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
                            Self::push_line_breaks(
                                &mut text,
                                prev,
                                span,
                                y_diff,
                                content.in_table && prev_in_table,
                            );
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
            } else {
                log::warn!(
                    "Structure tree references MCID {} but no spans found with that MCID",
                    mcid
                );
                self.push_warning(format!(
                    "page {page_index}: structure tree references MCID {mcid} but no content spans found — some text may be missing"
                ));
            }
        }

        // Append spans with MCIDs not referenced by the structure tree.
        // This happens with Form XObjects that lack /StructParents, where
        // their BDC/MCID markers exist in the content stream but are not
        // registered in the page's ParentTree.
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

        // Append any spans without MCID at the end (shouldn't happen in well-formed PDFs)
        if !spans_without_mcid.is_empty() {
            log::warn!(
                "Found {} text spans without MCID - appending to end",
                spans_without_mcid.len()
            );
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

    /// Order one MCID's spans for emission in the structure-order assemblers
    ///. A single marked-content element can carry spans across several
    /// visual lines; emitting them in raw extraction order can mis-order them,
    /// so sort by the canonical reading-order comparator. Skipped for single-
    /// span MCIDs and for any MCID containing RTL text (whose span order is
    /// handled by the bidi passes) — both stay byte-identical.
    pub(super) fn order_mcid_spans(
        spans: &[crate::layout::TextSpan],
    ) -> Vec<&crate::layout::TextSpan> {
        use crate::text::rtl_detector::is_rtl_text;
        let mut ordered: Vec<&crate::layout::TextSpan> = spans.iter().collect();
        if spans.len() <= 1 {
            return ordered;
        }
        let has_rtl = spans
            .iter()
            .any(|s| s.text.chars().any(|c| is_rtl_text(c as u32)));
        let has_latin = spans
            .iter()
            .any(|s| s.text.chars().any(|c| c.is_ascii_alphabetic()));
        if !has_rtl {
            // LTR multi-span MCID: left-to-right row-aware reading order.
            ordered.sort_by(|a, b| {
                crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
            });
        } else if !has_latin {
            // #656/#657: pure-RTL MCID. The tagged struct-tree path never
            // reaches `reverse_rtl_visual_order_runs`, so without an explicit
            // span-order pass the words emerge in visual (reversed) sequence.
            // Emitting each row right-to-left (X descending) reconstructs
            // logical reading order from geometry, independent of whether the
            // producer stored the run visually or logically. Per-span glyph
            // order is corrected separately by `push_span_text_bidi`.
            ordered = Self::order_pure_rtl_spans(spans);
        }
        // Mixed RTL+Latin MCIDs keep raw order (full UAX #9 bidi deferred).
        ordered
    }

    /// Order a pure-RTL MCID's spans into logical reading order: group spans
    /// into visual lines using a **font-relative** vertical tolerance, then
    /// emit each line right-to-left (X descending).
    ///
    /// A fixed quantized row band (`row_aware_span_cmp_rtl` with the global
    /// `ROW_BAND_TOLERANCE_PT`) over-segments Arabic lines. Producers routinely
    /// draw zero-advance glyphs — hamza seats, shadda/kasra marks, and even
    /// whole consonants positioned by a separate zero-width show — 1–3 pt off
    /// the baseline. A coarse fixed band rounds those into adjacent rows, which
    /// then emit before or after the body of the line and scatter the run (the
    /// telltale leading run of stray alef/hamza glyphs). Banding by a tolerance
    /// proportional to the glyph size keeps one jittery line intact while still
    /// separating genuinely distinct lines, whose leading is ~1.2× the font
    /// size — comfortably beyond the tolerance. Per-span glyph order is fixed
    /// separately by [`push_span_text_bidi`]; this function only fixes the order
    /// in which spans are emitted.
    pub(super) fn order_pure_rtl_spans(
        spans: &[crate::layout::TextSpan],
    ) -> Vec<&crate::layout::TextSpan> {
        use crate::utils::safe_float_cmp;
        let mut by_y: Vec<&crate::layout::TextSpan> = spans.iter().collect();
        // Stable sort, Y descending (top of page first). Ties keep extraction
        // (content-stream) order; the X-descending pass below refines each line.
        by_y.sort_by(|a, b| safe_float_cmp(b.bbox.y, a.bbox.y));

        let mut out: Vec<&crate::layout::TextSpan> = Vec::with_capacity(spans.len());
        let mut line: Vec<&crate::layout::TextSpan> = Vec::new();
        let mut anchor_y = f32::NAN;
        let mut tol = 0.0f32;
        for s in by_y {
            let fs = if s.font_size.is_finite() && s.font_size > 1.0 {
                s.font_size
            } else {
                10.0
            };
            let starts_new_line =
                anchor_y.is_finite() && (!s.bbox.y.is_finite() || anchor_y - s.bbox.y > tol);
            if anchor_y.is_nan() || starts_new_line {
                if !line.is_empty() {
                    line.sort_by(|a, b| safe_float_cmp(b.bbox.x, a.bbox.x));
                    out.append(&mut line);
                }
                anchor_y = s.bbox.y;
                tol = 0.5 * fs;
            }
            line.push(s);
        }
        if !line.is_empty() {
            line.sort_by(|a, b| safe_float_cmp(b.bbox.x, a.bbox.x));
            out.append(&mut line);
        }
        out
    }

    /// Pre-pass for the cross-span Arabic GLYPH-interleave defect. Some producers
    /// draw one Arabic word as a multi-glyph body span PLUS separate zero-width
    /// mark / consonant spans positioned by their own show, each at its true x.
    /// The atom-level span sort ([`order_pure_rtl_spans`]) then orders those spans
    /// as whole units and reverses each independently, so a zero-width glyph whose
    /// x falls *inside* a sibling span's x-extent lands at a word edge instead of
    /// interleaved — `الثدييات` extracts as `ثالدييات`.
    ///
    /// Returns `Some(owned spans)` with each affected visual LINE collapsed into a
    /// single visual-order span (so the downstream [`push_span_text_bidi`] reverse
    /// produces correct logical order), or `None` when no line exhibits the defect
    /// (then the caller uses the original spans, byte-identical). Gated tightly:
    /// fires only on a pure-RTL line (no ASCII-alpha) that actually contains a
    /// zero-width span interleaved inside another — a page with no such interleave
    /// (BidiSample, ArabicCIDTrueType-logical, hebrew_mirrored) returns `None`.
    pub(super) fn merge_interleaved_rtl_lines(
        spans: &[crate::layout::TextSpan],
    ) -> Option<Vec<crate::layout::TextSpan>> {
        use crate::utils::safe_float_cmp;
        if spans.len() < 3 {
            return None;
        }
        // Group into visual lines by a font-relative Y tolerance (mirrors
        // order_pure_rtl_spans banding).
        let mut by_y: Vec<&crate::layout::TextSpan> = spans.iter().collect();
        by_y.sort_by(|a, b| safe_float_cmp(b.bbox.y, a.bbox.y));
        let mut lines: Vec<Vec<&crate::layout::TextSpan>> = Vec::new();
        // Roll the line-break reference forward to the PREVIOUS span's y rather
        // than pinning it to the band's first (topmost) span. RTL producers seat
        // a line's glyphs across a few points of vertical jitter — combining
        // marks ride high, a line-final letter can sit a few points low (P2: a
        // width-0 final glyph at dy≈3pt below the baseline). Against a fixed top
        // anchor the line's own span furthest above the baseline sets the band
        // ceiling, so the lowest glyph can exceed `0.5·fs` and split onto its own
        // "line" — which then reverses and lands after the sentence terminator,
        // detaching the line's final letter. Comparing each span to its immediate
        // predecessor keeps a line whose internal step is < tol intact while a
        // real inter-line gap (leading ≈ one full em, well over tol) still opens
        // the next band.
        let mut prev_y = f32::NAN;
        let mut tol = 0.0f32;
        for s in by_y {
            let fs = if s.font_size.is_finite() && s.font_size > 1.0 {
                s.font_size
            } else {
                10.0
            };
            let new_line = prev_y.is_finite() && (!s.bbox.y.is_finite() || prev_y - s.bbox.y > tol);
            if prev_y.is_nan() || new_line {
                lines.push(Vec::new());
                tol = 0.5 * fs;
            }
            prev_y = s.bbox.y;
            lines.last_mut().unwrap().push(s);
        }

        let mut any_gated = false;
        let mut out: Vec<crate::layout::TextSpan> = Vec::with_capacity(spans.len());
        for line in &lines {
            if Self::rtl_line_needs_glyph_reorder(line) {
                any_gated = true;
                out.push(Self::merge_rtl_line_to_visual_span(line));
            } else {
                out.extend(line.iter().map(|s| (*s).clone()));
            }
        }
        if any_gated {
            Some(out)
        } else {
            None
        }
    }

    /// True when a visual line exhibits the zero-width-glyph interleave defect:
    /// (1) pure-RTL — no span carries an ASCII-alphabetic char and at least one
    /// carries an RTL letter; AND (2) a zero-width span's x lies STRICTLY inside
    /// another span's `[x, x+width]` on the line. Both are required so ordinary
    /// pure-RTL text (no interleave) and any mixed-Latin line are left untouched.
    pub(super) fn rtl_line_needs_glyph_reorder(line: &[&crate::layout::TextSpan]) -> bool {
        use crate::text::rtl_detector::is_rtl_text;
        if line.len() < 2 {
            return false;
        }
        let mut has_rtl = false;
        for s in line {
            for c in s.text.chars() {
                if c.is_ascii_alphabetic() {
                    return false; // mixed Latin — not our case
                }
                if is_rtl_text(c as u32) {
                    has_rtl = true;
                }
            }
        }
        if !has_rtl {
            return false;
        }
        line.iter().any(|m| {
            m.bbox.width.abs() < 0.01
                && line.iter().any(|b| {
                    !std::ptr::eq(*m, *b)
                        && b.bbox.width > 0.01
                        && m.bbox.x > b.bbox.x
                        && m.bbox.x < b.bbox.x + b.bbox.width
                })
        })
    }

    /// Collapse a gated RTL visual line into one VISUAL-order span: explode every
    /// span into per-glyph `(x, char)` (reusing the `to_chars` advance arithmetic),
    /// drop producer shatter spaces, sort base letters by ascending x (visual
    /// left-to-right), bind each combining mark to its nearest base, and re-insert
    /// a single space at genuine inter-word x-gaps. The downstream
    /// [`push_span_text_bidi`] then reverses this to correct logical order with
    /// marks kept attached (`reverse_rtl_keeping_marks`).
    /// Private-use sentinel that [`merge_rtl_line_to_visual_span`] emits in place
    /// of a SPACE at an AUTHORITATIVE producer-segmented Arabic word boundary, so
    /// the downstream [`strip_interior_arabic_spaces`] (which strips only U+0020)
    /// leaves it intact instead of mistaking a genuine word break for a
    /// cursive-shatter artefact. Every output site restores it to a SPACE right
    /// after the strip ([`push_span_text_bidi`] for plain text,
    /// [`apply_rtl_logical_order_to_ordered_spans`] for md/html). U+F8FF is in the
    /// Unicode private-use area and never appears in real producer text reaching
    /// the pure-RTL merge path.
    pub(super) const RTL_WORD_BOUNDARY: char = '\u{F8FF}';

    pub(super) fn merge_rtl_line_to_visual_span(
        line: &[&crate::layout::TextSpan],
    ) -> crate::layout::TextSpan {
        use crate::text::rtl_detector::is_rtl_diacritic;
        use crate::utils::safe_float_cmp;
        // Explode to glyphs: split bases from combining marks, DROP shatter spaces
        // that are interior to a multi-glyph span, but record the x of each
        // STANDALONE space span — those are the producer's real word boundaries
        // (geometric gap-thresholding is unreliable for cursive Arabic, so we use
        // the producer's own segmentation instead).
        let mut bases: Vec<(f32, char)> = Vec::new();
        let mut marks: Vec<(f32, char)> = Vec::new();
        let mut word_space_x: Vec<f32> = Vec::new();
        for s in line {
            if !s.text.is_empty() && s.text.chars().all(|c| c.is_whitespace()) {
                word_space_x.push(s.bbox.x + s.bbox.width * 0.5);
                continue;
            }
            // Pre-collect the span's chars so a whitespace glyph can see its
            // non-mark neighbours (to tell a cursive-join shatter space from a
            // genuine word break).
            let span_chars: Vec<char> = s.to_chars().into_iter().map(|t| t.char).collect();
            for (idx, tc) in s.to_chars().into_iter().enumerate() {
                let c = tc.char;
                if c.is_whitespace() {
                    // ISO 32000-1 §14.8.2.3.3: a SPACE that borders a
                    // NON-CURSIVE token (clause punctuation / symbol — not an
                    // Arabic/Hebrew letter and not a digit) is a real word
                    // break, so record its x. A space flanked by cursive letters
                    // is the producer's intra-word shatter (dropped), and a
                    // space between digits is a thousands separator (dropped) —
                    // neither is a word boundary.
                    use crate::text::rtl_detector::{
                        is_arabic_letter, is_arabic_number, is_hebrew_letter,
                    };
                    let neighbour = |it: &mut dyn Iterator<Item = &char>| -> Option<char> {
                        it.copied()
                            .find(|&p| !p.is_whitespace() && !is_rtl_diacritic(p as u32))
                    };
                    let is_boundary_marker = |o: Option<char>| {
                        o.is_some_and(|p| {
                            let u = p as u32;
                            !is_arabic_letter(u)
                                && !is_hebrew_letter(u)
                                && !is_arabic_number(u)
                                && !p.is_ascii_digit()
                        })
                    };
                    let prev = neighbour(&mut span_chars[..idx].iter().rev());
                    let next = neighbour(&mut span_chars[idx + 1..].iter());
                    if is_boundary_marker(prev) || is_boundary_marker(next) {
                        word_space_x.push(tc.bbox.x + tc.bbox.width * 0.5);
                    }
                    continue; // not emitted as a glyph either way
                }
                if is_rtl_diacritic(c as u32) {
                    marks.push((tc.bbox.x, c));
                } else {
                    bases.push((tc.bbox.x, c));
                }
            }
        }
        if bases.is_empty() {
            return (*line[0]).clone();
        }
        bases.sort_by(|a, b| safe_float_cmp(a.0, b.0));
        // Attach each mark to the nearest base by x; build base→trailing-marks.
        let mut trailing: Vec<Vec<char>> = vec![Vec::new(); bases.len()];
        for (mx, mc) in &marks {
            let mut best = 0usize;
            let mut best_d = f32::MAX;
            for (i, (bx, _)) in bases.iter().enumerate() {
                let d = (bx - mx).abs();
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            trailing[best].push(*mc);
        }
        // Emit visual (ascending-x) order: each base then its marks, with a single
        // word-boundary marker wherever a producer word-boundary x falls between two
        // bases. The marker is the private-use sentinel [`Self::RTL_WORD_BOUNDARY`],
        // not a plain SPACE, so the downstream `strip_interior_arabic_spaces` (which
        // only removes U+0020) cannot mistake this AUTHORITATIVE producer-segmented
        // word break for a cursive-shatter artefact and delete it; each output site
        // restores it to a SPACE right after the strip. The downstream reverse maps
        // this to logical order with words intact.
        let mut text = String::new();
        let mut prev_x: Option<f32> = None;
        for (i, (bx, bc)) in bases.iter().enumerate() {
            if let Some(px) = prev_x {
                if word_space_x.iter().any(|sx| *sx > px && *sx < *bx)
                    && !text.ends_with(Self::RTL_WORD_BOUNDARY)
                {
                    text.push(Self::RTL_WORD_BOUNDARY);
                }
            }
            text.push(*bc);
            for m in &trailing[i] {
                text.push(*m);
            }
            prev_x = Some(*bx);
        }
        // Build the merged span from the line's first span, spanning the line.
        let mut merged = (*line[0]).clone();
        let x_min = line.iter().map(|s| s.bbox.x).fold(f32::MAX, f32::min);
        let x_max = line
            .iter()
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::MIN, f32::max);
        merged.text = text;
        merged.bbox.x = x_min;
        merged.bbox.width = (x_max - x_min).max(0.0);
        merged.char_widths = Vec::new();
        merged
    }
}
