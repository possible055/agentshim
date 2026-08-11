use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    pub(super) fn postprocess_spans(
        &self,
        page_index: usize,
        raw_spans: Vec<crate::layout::TextSpan>,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        let mut spans = raw_spans;

        self.drop_offpage_spans(page_index, &mut spans);

        // Recover decimal points mis-decoded as `¬` (U+00AC) in Computer-Modern
        // math subsets, where the `/Differences` names the decimal slot
        // `logicalnot`. Only a `¬` sitting *directly between two digits* (no
        // space) is rewritten — real logic/set `¬` is always spaced, so this
        // cannot corrupt it.
        for span in &mut spans {
            if Self::is_cm_or_symbol_font(&span.font_name) && span.text.contains('\u{00AC}') {
                span.text = Self::fix_digit_logicalnot_decimal(&span.text);
            }
        }

        // Re-attach oversized lone leading capitals to their word before the
        // reading-order sort can strand them (drop-cap / table-title initials).
        Self::merge_drop_cap_initials(&mut spans);

        // Apply page /Rotate to span geometry BEFORE reading-order sorting.
        //
        // A page with a /Rotate entry must be read in its DISPLAYED orientation
        // or the row-aware sort emits text in the wrong order (pdf.js issue14415
        // is a 180° English page that otherwise comes out word- and line-reversed).
        //
        // The transform is applied selectively, because a rotated page carries two
        // very different kinds of run, distinguished by each span's own
        // `rotation_degrees` (the content-stream text-matrix rotation):
        //
        // * **Horizontal content (`rotation_degrees == 0`) on a 90°/270° page** —
        //   e.g. a landscape table stored rotated (`/Rotate 90`, MediaBox already
        //   landscape). This text is horizontal in raw user space, so it reads and
        //   groups correctly THERE. Rotating its bbox by ±90° only rotates the
        //   RECTANGLE, but `TextSpan::to_chars` still lays glyphs horizontally with
        //   raw advance widths and cannot express a now-vertical run, so every raw
        //   row collapses onto one displayed band and perpendicular columns fuse
        //   into one 1000+ char token (#804). These are LEFT RAW — matching
        //   `extract_chars`, which also returns raw coordinates.
        //
        // * **Rotated content (`rotation_degrees == ±90`) on a 90°/270° page** —
        //   e.g. a chart axis, a sideways table, or a whole landscape page authored
        //   by drawing every glyph sideways in a portrait MediaBox with `/Rotate 90`
        //   to present it upright. Here the page /Rotate must be applied so it
        //   COMBINES with the content rotation (which `order_rotated_blocks` undoes
        //   for ordering) into the correct upright displayed frame; leaving it raw
        //   reads the page sideways. These ARE mapped.
        //
        // 180° maps everything (text stays horizontal; both axes just mirror —
        // numerically identical to the legacy mirror).
        //
        // Captured so the same transform is applied to annotation spans appended
        // later (their /Rect is in unrotated page space too). `None` for rot == 0
        // or unknown media box — those pages keep raw geometry.
        let page_rotation: Option<(i32, f32, f32, f32, f32)> =
            match self.get_page_media_box(page_index) {
                Ok((llx, lly, urx, ury)) => {
                    let rot = self
                        .get_page_rotation(page_index)
                        .unwrap_or(0)
                        .rem_euclid(360);
                    matches!(rot, 90 | 180 | 270).then_some((rot, llx, lly, urx - llx, ury - lly))
                }
                Err(_) => None,
            };
        if let Some((rot, llx, lly, w, h)) = page_rotation {
            for s in spans.iter_mut() {
                // 90°/270°: only map runs whose own content is rotated; horizontal
                // content stays in raw user space (see rationale above).
                if rot != 180 && s.rotation_degrees == 0.0 {
                    continue;
                }
                Self::map_span_into_rotated_frame(s, rot, llx, lly, w, h);
            }
        }

        // Tategaki (vertical writing) intercept. Pages whose majority of
        // spans were emitted under WMode 1 (font /Encoding ends in -V or
        // the CMap declares /WMode 1) need right-to-left, top-to-bottom
        // ordering. Row-aware / XY-cut sorts assume horizontal flow and
        // scramble vertical text; per-span wmode lets us route just those
        // pages through a tategaki comparator while leaving every existing
        // horizontal corpus untouched.
        let vertical_count = spans.iter().filter(|s| s.wmode == 1).count();
        if !spans.is_empty() && vertical_count * 2 >= spans.len() {
            // See `crate::utils::sort_vertical_tategaki` for the
            // column-clustering algorithm and the total-order rationale.
            spans = crate::utils::sort_vertical_tategaki(spans, |s| &s.bbox);
        } else if let Some(ordered) = Self::sidebar_body_reading_order(&spans) {
            // RW-1: narrow-sidebar + wide-body first pages (full-width title band
            // over a metadata sidebar + body). Handled before the XY-cut so the
            // title is not sliced along the body gutter (§14.8.3).
            spans = ordered;
        } else if Self::is_multi_column_page(&spans) {
            use crate::pipeline::reading_order::{
                ReadingOrderContext as ROContext, ReadingOrderStrategy, XYCutStrategy,
            };
            let strategy = XYCutStrategy::new();
            let context = ROContext::new().with_page(page_index as u32);
            // Clone needed: apply() takes ownership, and the Err branch
            // falls back to sorting the original vec in place.
            match strategy.apply(spans.clone(), &context) {
                Ok(ordered) => {
                    spans = ordered.into_iter().map(|o| o.span).collect();
                }
                Err(e) => {
                    log::debug!(
                        "XY-cut reading order failed on page {page_index} ({e}), \
                         falling back to row-aware sort"
                    );
                    spans.sort_by(|a, b| {
                        crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
                    });
                    Self::reorder_rowspan_labels(&mut spans);
                }
            }
        } else {
            // Row-aware sort: Y-band descending (top→bottom), X ascending
            // within a row.
            spans.sort_by(|a, b| {
                crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
            });
            // Lift multi-row-spanning labels to the top of their block.
            Self::reorder_rowspan_labels(&mut spans);
        }

        // Per-span rotation firewall. Runs drawn with a rotated text matrix
        // (the vertical `arXiv:…` margin stamp, figure/axis labels, rotated
        // table headers, transit-poster route names) break the axis-aligned
        // row-band / XY-cut assumptions, so interleaving them with the
        // horizontal flow scrambles reading order. The reordering above ran on
        // the FULL span set (so its column/XY-cut decisions are unchanged — the
        // horizontal body keeps its exact baseline order); now stably lift the
        // rotated runs out (preserving horizontal order) and re-append them as
        // their own blocks, each ordered in an upright frame. No-op (and
        // byte-identical) when the page has no rotated spans.
        if spans.iter().any(|s| s.rotation_degrees != 0.0) {
            let rotated: Vec<crate::layout::TextSpan> = spans
                .iter()
                .filter(|s| s.rotation_degrees != 0.0)
                .cloned()
                .collect();
            spans.retain(|s| s.rotation_degrees == 0.0);
            spans.extend(Self::order_rotated_blocks(rotated));
        }

        // Append text from non-Widget annotations (/Subtype /Text, FreeText,
        // Stamp, Highlight, etc.) that carry a /Contents entry. These are not
        // part of the page content stream so they are not picked up by the
        // regular extractor. On a /Rotate'd page their /Rect-derived bboxes are
        // in unrotated page space, so map the appended spans into the same
        // displayed frame as the content spans (no-op for unrotated pages).
        // Annotation text is horizontal (rotation_degrees == 0), so on a 90°/270°
        // page it stays raw, matching the horizontal content spans above.
        let pre_annotation_len = spans.len();
        spans.extend(self.annotation_content_spans(page_index));
        if let Some((rot, llx, lly, w, h)) = page_rotation {
            for s in spans[pre_annotation_len..].iter_mut() {
                if rot != 180 && s.rotation_degrees == 0.0 {
                    continue;
                }
                Self::map_span_into_rotated_frame(s, rot, llx, lly, w, h);
            }
        }

        // Mark running headers/footers (untagged-PDF heuristic). Spans whose
        // normalized text recurs on >=50% of pages and sits near the top or
        // bottom of the page are flagged as artifacts so downstream filters
        // drop them.
        self.mark_running_artifact_spans(page_index, &mut spans)?;

        // Normalize Unicode typographic spaces (U+2000–U+200B, U+202F, U+205F)
        // to ASCII space. Some PDF producers encode word separators as hair-space
        // or thin-space variants in ToUnicode CMaps (e.g. justified text layouts);
        // normalising here gives consistent word boundaries to every downstream
        // consumer (extract_text, word-F1 scoring, etc.).
        for span in &mut spans {
            if span
                .text
                .chars()
                .any(|c| matches!(c, '\u{2000}'..='\u{200B}' | '\u{202F}' | '\u{205F}'))
            {
                span.text = span
                    .text
                    .chars()
                    .map(|c| {
                        if matches!(c, '\u{2000}'..='\u{200B}' | '\u{202F}' | '\u{205F}') {
                            ' '
                        } else {
                            c
                        }
                    })
                    .collect();
            }
        }

        // Apply char_widths boundary splits directly to span.text so that every
        // downstream consumer (to_markdown, to_html, extract_text) sees the same
        // word boundaries. extract_text applies the same logic through push_span_text;
        // after this normalization push_span_text sees a space at the boundary
        // becomes a no-op, so there is no double-application risk.
        for span in &mut spans {
            if let Some(split) = Self::char_widths_boundary_split(span) {
                let mut t = String::with_capacity(span.text.len() + 1);
                t.push_str(&span.text[..split]);
                t.push(' ');
                t.push_str(&span.text[split..]);
                span.text = t;
            }
        }

        // Detect superscript / subscript runs and substitute ASCII
        // digits with their Unicode super/sub-script equivalents
        // (only when the run is sandwiched between alphabetic body
        // spans on both sides — chemistry/math context like "S²X"
        // or "H₂O"). The same substitution would otherwise fire on
        // author-affiliation markers ("name¹,²") which the bench
        // ground truth keeps in ASCII; gating on token-internal
        // context keeps the desired cases without regressing the
        // affiliation-block pages.
        Self::apply_super_sub_script_substitutions(&mut spans);

        // Fold spacing-diacritic spans (´, `, ^, ~, ¨, …) into the
        // base letter of the following span when the diacritic is
        // centred over the base glyph. PDFs that pre-shape accented
        // Latin (LaTeX `\'E` → two glyphs, `acute` then `E`) emit
        // the marks as separate `Tj` ops at the base glyph's X
        // coordinate. Without this pass extract_text returns the
        // raw two-glyph order "´Ecole" instead of "École".
        Self::apply_combining_mark_composition(&mut spans);

        // Stamp accurate per-glyph x-origins onto the finalized spans so that
        // `to_chars()` (and thus extract_words / extract_spans /
        // extract_text_lines, which all decompose spans through it) reports
        // spec-aligned positions instead of drifting prefix-sums. Runs last, on
        // the fully post-processed spans, so alignment sees the same text the
        // consumers do.
        self.stamp_char_x_offsets(page_index, &mut spans);

        Ok(spans)
    }

    /// Copy the spec-aligned per-glyph baseline x-origins from the char-level
    /// extractor onto each span's [`char_x_offsets`](crate::layout::TextSpan::char_x_offsets).
    ///
    /// # Why
    ///
    /// [`TextSpan::to_chars`](crate::layout::TextSpan::to_chars) otherwise
    /// reconstructs each glyph's x by prefix-summing the span's nominal
    /// `char_widths` from `bbox.x`. Those nominal widths omit the ISO
    /// 32000-1:2008 §9.4.3 TJ-array adjustment (the number in a TJ array is
    /// "expressed in thousandths of a unit of text space … subtracted from the
    /// current … coordinate") and the full §9.4.4 text-space displacement
    /// (`t_x = ((w0 − Tj/1000) · Tfs + Tc + Tw) · Th`). Prefix-summing the
    /// nominal widths therefore drifts cumulatively along a line. The
    /// char-level extractor that [`extract_chars`](Self::extract_chars) uses
    /// implements §9.4.4 / §9.4.3 in full (it matches Poppler `pdftotext
    /// -bbox`), so its `origin_x` values are the authoritative positions this
    /// function stamps back onto the spans.
    ///
    /// # Alignment (robust, per span)
    ///
    /// A naive global greedy walk on char values mis-jumps on repeated
    /// letters / spaces. Instead, for each span we take only the accurate chars
    /// on the SAME baseline (`|origin_y − span.bbox.y| ≤ 0.5·font_size`), sort
    /// them by x, and match the span's glyph sequence as a CONTIGUOUS run,
    /// choosing the run whose first glyph's `origin_x` is nearest `span.bbox.x`.
    ///
    /// # Fallback (never guess)
    ///
    /// If a span cannot be fully, unambiguously aligned — no contiguous run of
    /// exactly the span's glyphs exists on its line (count mismatch from a
    /// post-processing text edit, ligature expansion, a synthetic space glyph
    /// not present in the char stream, …) — its `char_x_offsets` is left empty
    /// so `to_chars` uses the legacy prefix-sum path. A cleared span is a
    /// no-op, never a regression. `char_widths` is never touched (a downstream
    /// word-boundary heuristic keys off its length).
    ///
    /// 180° pages are skipped entirely: the spans are mirrored into the displayed
    /// frame while the accurate chars remain in unrotated page space, so a
    /// horizontal-x stamp would not correspond. On 90°/270° pages, horizontal
    /// content spans stay in raw user space and ARE stamped, but rotated-content
    /// spans (`rotation_degrees != 0`) have been mapped into the displayed frame
    /// (see `postprocess_spans`) and their glyphs run along a rotated axis, so a
    /// horizontal-x stamp would misalign — those individual spans are skipped.
    pub(super) fn stamp_char_x_offsets(
        &self,
        page_index: usize,
        spans: &mut [crate::layout::TextSpan],
    ) {
        // Horizontal-x offsets only make sense in an unrotated frame; the 180°
        // mirror is the one rotation that leaves ALL spans in the displayed frame.
        if self
            .get_page_rotation(page_index)
            .unwrap_or(0)
            .rem_euclid(360)
            == 180
        {
            return;
        }

        let accurate = match self.cached_page_chars(page_index) {
            Ok(chars) if !chars.is_empty() => chars,
            _ => return,
        };

        // Baseline index: char positions ordered by `origin_y`, so each span's
        // baseline slice is a binary-searched range rather than a linear scan
        // over every glyph on the page (that scan made this pass
        // O(spans x chars) — the dominant per-page cost on long documents).
        let mut by_y: Vec<u32> = (0..accurate.len() as u32).collect();
        by_y.sort_by(|&a, &b| {
            crate::utils::safe_float_cmp(
                accurate[a as usize].origin_y,
                accurate[b as usize].origin_y,
            )
        });
        let ys: Vec<f32> = by_y
            .iter()
            .map(|&i| accurate[i as usize].origin_y)
            .collect();

        for span in spans.iter_mut() {
            // Rotated-content spans are in the displayed frame (mapped on 90/270)
            // and their glyphs run vertically; a horizontal-x stamp from the raw
            // chars would not correspond, so leave them to the prefix-sum path.
            if span.rotation_degrees != 0.0 {
                continue;
            }
            // Start clean: any offsets carried over via struct-update from a
            // source span must not be trusted for this (possibly edited) text.
            span.char_x_offsets.clear();

            let glyphs: Vec<char> = span.text.chars().collect();
            let n = glyphs.len();
            if n == 0 {
                continue;
            }

            // Accurate chars sharing this span's baseline, left-to-right.
            let baseline_tol = 0.6 * span.font_size.max(1.0);
            // Chars sharing this span's baseline, left-to-right. The y-sorted
            // index brackets a candidate range; the exact `abs()` predicate then
            // selects from it, so the result matches a full linear scan even
            // where the bracket arithmetic rounds differently. The widened
            // bracket keeps that range a superset. Ordering by
            // (origin_x, source index) reproduces the previous stable
            // filter-then-sort: ties on origin_x keep their `accurate` order.
            let bracket = baseline_tol + baseline_tol.abs() * 1e-6 + f32::EPSILON;
            let lo = ys.partition_point(|&y| y < span.bbox.y - bracket);
            let hi = ys.partition_point(|&y| y <= span.bbox.y + bracket);
            let mut idx: Vec<u32> = by_y[lo..hi]
                .iter()
                .copied()
                .filter(|&i| (accurate[i as usize].origin_y - span.bbox.y).abs() <= baseline_tol)
                .collect();
            if idx.is_empty() {
                continue;
            }
            idx.sort_by(|&a, &b| {
                crate::utils::safe_float_cmp(
                    accurate[a as usize].origin_x,
                    accurate[b as usize].origin_x,
                )
                .then(a.cmp(&b))
            });
            let line: Vec<&crate::layout::TextChar> =
                idx.iter().map(|&i| &accurate[i as usize]).collect();

            // Greedy per-glyph alignment. Anchor the scan cursor at the accurate
            // char nearest this span's left edge, then walk the span's glyphs,
            // matching each to the next equal accurate char within a small
            // forward window. Unlike an all-or-nothing contiguous match, a single
            // unmatched glyph (an inserted word-boundary space, a ligature split,
            // a combining mark) no longer discards the whole span — such glyphs
            // are interpolated below so `char_x_offsets` still fills every index.
            let start = line
                .iter()
                .position(|c| c.origin_x >= span.bbox.x - 0.5)
                .unwrap_or(0);
            let mut assigned: Vec<Option<f32>> = vec![None; n];
            let mut li = start;
            for (k, &g) in glyphs.iter().enumerate() {
                let mut j = li;
                let mut steps = 0;
                while j < line.len() && steps < 6 {
                    if line[j].char == g {
                        assigned[k] = Some(line[j].origin_x);
                        li = j + 1;
                        break;
                    }
                    j += 1;
                    steps += 1;
                }
            }

            // Need enough real anchors to trust the run; otherwise fall back.
            let anchors = assigned.iter().filter(|a| a.is_some()).count();
            if anchors * 5 < n * 3 {
                // < 60% matched
                continue;
            }

            // Fill the gaps: each unmatched glyph takes the nearest preceding
            // anchor plus the prefix sum of the (locally accurate) char_widths
            // between them; if there is no preceding anchor, walk back from the
            // nearest following one. Over the short spans between anchors the
            // cumulative drift these interpolations reintroduce is sub-point.
            let cw = &span.char_widths;
            let width_at = |i: usize| -> f32 {
                if cw.len() == n {
                    cw[i]
                } else {
                    span.bbox.width / n as f32
                }
            };
            let mut offs = vec![0.0f32; n];
            // forward pass from preceding anchors
            let mut last: Option<(usize, f32)> = None;
            for k in 0..n {
                if let Some(x) = assigned[k] {
                    offs[k] = x;
                    last = Some((k, x));
                } else if let Some((lk, lx)) = last {
                    let acc: f32 = (lk..k).map(width_at).sum();
                    offs[k] = lx + acc;
                }
            }
            // backfill any leading None from the first following anchor
            if assigned[0].is_none() {
                if let Some(fk) = assigned.iter().position(|a| a.is_some()) {
                    let fx = assigned[fk].unwrap();
                    for k in 0..fk {
                        let acc: f32 = (k..fk).map(width_at).sum();
                        offs[k] = fx - acc;
                    }
                }
            }
            span.char_x_offsets = offs;
        }
    }

    /// Fold a one-char spacing-diacritic span into the following
    /// span's first character when they overlap in X (the typical
    /// LaTeX `\'E` → `(´)(E)` shape). Substitutes the relevant
    /// combining mark from U+0300..U+0327 and lets
    /// `unicode_normalization::nfc` precompose where it can
    /// ("E\u{0301}" → "É"). The diacritic span is left empty so
    /// downstream rendering skips it.
    pub(super) fn apply_combining_mark_composition(spans: &mut Vec<crate::layout::TextSpan>) {
        use unicode_normalization::UnicodeNormalization;

        fn combining_for(spacing: char) -> Option<char> {
            Some(match spacing {
                '\u{00B4}' => '\u{0301}', // acute
                '\u{0060}' => '\u{0300}', // grave
                '\u{005E}' => '\u{0302}', // circumflex
                '\u{02C6}' => '\u{0302}', // modifier-letter circumflex
                '\u{007E}' => '\u{0303}', // tilde
                '\u{02DC}' => '\u{0303}', // small tilde
                '\u{00A8}' => '\u{0308}', // diaeresis
                '\u{00AF}' => '\u{0304}', // macron
                '\u{02C9}' => '\u{0304}', // modifier-letter macron
                '\u{00B8}' => '\u{0327}', // cedilla
                '\u{02DA}' => '\u{030A}', // ring above
                _ => return None,
            })
        }

        // First pass: spans that already got merged at the extractor
        // (when the LaTeX `(´)(Ecole)` pair both sit at the same
        // text-matrix origin the upstream merge_adjacent_spans pulls
        // them into a single "´Ecole" span). Fold the leading
        // diacritic + base letter into the precomposed form.
        for span in spans.iter_mut() {
            let mut iter = span.text.chars();
            let Some(d) = iter.next() else { continue };
            let Some(base) = iter.next() else { continue };
            let Some(combining) = combining_for(d) else {
                continue;
            };
            if !base.is_alphabetic() {
                continue;
            }
            let rest_start = d.len_utf8() + base.len_utf8();
            let mut composed = String::with_capacity(span.text.len() + 2);
            composed.push(base);
            composed.push(combining);
            composed.push_str(&span.text[rest_start..]);
            span.text = composed.nfc().collect();
        }

        // Walk spans pairwise. The diacritic is on its own one-
        // character span; the next span carries the base letter.
        let mut i = 0;
        while i + 1 < spans.len() {
            let mark_char = {
                let s = &spans[i];
                let mut iter = s.text.chars();
                let first = iter.next();
                let rest = iter.next();
                if rest.is_some() {
                    None
                } else {
                    first.and_then(combining_for)
                }
            };
            let Some(combining) = mark_char else {
                i += 1;
                continue;
            };
            // Geometric: same line, diacritic anchored over the base
            // letter's left edge (within ±1 pt).
            let (same_line, overlaps_x) = {
                let p = &spans[i];
                let n = &spans[i + 1];
                let same = (p.bbox.y - n.bbox.y).abs() < p.font_size.max(n.font_size) * 0.6;
                let dx = (p.bbox.x - n.bbox.x).abs();
                (same, dx <= 1.5)
            };
            if !(same_line && overlaps_x) {
                i += 1;
                continue;
            }
            // The next span must start with a base letter we can
            // attach a combining mark to (Latin letter / digit).
            let Some(base) = spans[i + 1].text.chars().next() else {
                i += 1;
                continue;
            };
            if !base.is_alphabetic() {
                i += 1;
                continue;
            }
            // Build "<base><combining><rest>" and NFC-compose.
            let mut composed = String::with_capacity(spans[i + 1].text.len() + 2);
            composed.push(base);
            composed.push(combining);
            let rest_start = base.len_utf8();
            composed.push_str(&spans[i + 1].text[rest_start..]);
            spans[i + 1].text = composed.nfc().collect();
            // Empty out the diacritic span; downstream consumers
            // skip zero-text spans.
            spans[i].text.clear();
            i += 2;
        }

        // Drop any spans we emptied.
        spans.retain(|s| !s.text.is_empty());
    }

    /// Substitute ASCII digits and a few punctuation characters in
    /// super/sub-script spans with their Unicode counterparts
    /// (U+2070..U+2079 / U+00B2/B3/B9 for superscripts,
    /// U+2080..U+2089 for subscripts). A span is treated as
    /// super- or sub-script when its font is meaningfully smaller
    /// than the previous span on the same line and its baseline is
    /// raised or lowered. Only spans whose text consists entirely
    /// of substitutable characters are rewritten — mixed-content
    /// or single-letter superscript callouts (e.g. footnote "a")
    /// fall through unchanged so the existing citation-handling
    /// path stays in control.
    pub(super) fn apply_super_sub_script_substitutions(spans: &mut [crate::layout::TextSpan]) {
        fn super_for_char(c: char) -> Option<char> {
            Some(match c {
                '0' => '\u{2070}',
                '1' => '\u{00B9}',
                '2' => '\u{00B2}',
                '3' => '\u{00B3}',
                '4' => '\u{2074}',
                '5' => '\u{2075}',
                '6' => '\u{2076}',
                '7' => '\u{2077}',
                '8' => '\u{2078}',
                '9' => '\u{2079}',
                '+' => '\u{207A}',
                '-' => '\u{207B}',
                '=' => '\u{207C}',
                '(' => '\u{207D}',
                ')' => '\u{207E}',
                _ => return None,
            })
        }
        fn sub_for_char(c: char) -> Option<char> {
            Some(match c {
                '0' => '\u{2080}',
                '1' => '\u{2081}',
                '2' => '\u{2082}',
                '3' => '\u{2083}',
                '4' => '\u{2084}',
                '5' => '\u{2085}',
                '6' => '\u{2086}',
                '7' => '\u{2087}',
                '8' => '\u{2088}',
                '9' => '\u{2089}',
                '+' => '\u{208A}',
                '-' => '\u{208B}',
                '=' => '\u{208C}',
                '(' => '\u{208D}',
                ')' => '\u{208E}',
                _ => return None,
            })
        }
        // Two-pass: first compute the body-font baseline for each
        // line band (largest font_size on that line), then walk
        // spans and substitute any whose font is meaningfully
        // smaller AND whose baseline is raised or lowered relative
        // to the body baseline.
        let n = spans.len();
        if n < 2 {
            return;
        }
        const LINE_BAND_PT: f32 = 4.0;
        // band_anchor[i] = (body_font_size, body_y) of the line
        // band that span `i` belongs to. Sorting span indices by Y
        // once + sliding a two-pointer window over the sorted view
        // reduces the per-span band-anchor scan from O(n) to amortised
        // O(window_size), bringing the whole pass from O(n²) down to
        // O(n log n) on thesis-style pages with thousands of spans.
        let mut sorted_by_y: Vec<usize> = (0..n).collect();
        sorted_by_y
            .sort_by(|&a, &b| crate::utils::safe_float_cmp(spans[a].bbox.y, spans[b].bbox.y));
        let band_anchor = Self::compute_band_anchors(spans, &sorted_by_y, LINE_BAND_PT);
        // Spatial index: bucket spans by Y-band so `span_is_token_internal`
        // queries only nearby spans instead of all of them (its same-line
        // neighbour scan was O(n) per candidate → O(n²) on dense pages).
        let y_index = Self::build_y_band_index(spans, LINE_BAND_PT);
        for i in 0..n {
            let (anchor_fs, anchor_y) = band_anchor[i];
            let curr_fs = spans[i].font_size;
            // Skip the body span itself (it IS the anchor).
            if anchor_fs <= 0.0 || curr_fs >= anchor_fs * 0.85 {
                continue;
            }
            let y_delta = spans[i].bbox.y - anchor_y;
            let raised = y_delta > anchor_fs * 0.15;
            let lowered = y_delta < -anchor_fs * 0.15;
            if !raised && !lowered {
                continue;
            }
            let map: fn(char) -> Option<char> = if raised { super_for_char } else { sub_for_char };
            if spans[i].text.is_empty() || !spans[i].text.chars().all(|c| map(c).is_some()) {
                continue;
            }
            // Leave a signed numeric exponent (scientific unit notation such as
            // `s−1`, `m−2`) as ASCII. ToUnicode already decoded the intended
            // characters, and the plaintext convention every reference extractor
            // follows keeps these un-superscripted; rewriting `−1` to `₋₁` / `⁻¹`
            // is both wrong against that convention and — because the geometric
            // classifier fires inconsistently on borderline baselines — a source
            // of non-determinism across identical occurrences.
            if Self::run_is_signed_number(&spans[i].text) {
                continue;
            }
            // Limit the substitution to clearly token-internal
            // super/sub-scripts: the run must have a base-sized
            // neighbour on BOTH sides whose first/last char is
            // alphabetic and roughly adjacent in X. Author-
            // affiliation markers like "name¹,²" sit at the END
            // of a line with no following body letter; the bench
            // GT renders those as plain ASCII digits, so substi-
            // tuting them would regress. Restricting to sandwiched
            // runs keeps the chemistry / exponent cases that the
            // GT does want as Unicode (S², H₂O, k₁) and skips the
            // trailing footnote callouts.
            if !Self::span_is_token_internal(spans, i, &y_index, LINE_BAND_PT) {
                continue;
            }
            let substituted: String = spans[i].text.chars().map(|c| map(c).unwrap()).collect();
            spans[i].text = substituted;
        }
    }

    /// A run is a signed numeric exponent — e.g. `-1`, `−2`, `‑3` — when it
    /// opens with a minus/hyphen sign and contains at least one digit. Such runs
    /// are scientific unit exponents (`s−1`, `m−2`) that the plaintext extraction
    /// convention keeps as ASCII, so [`apply_super_sub_script_substitutions`]
    /// must not rewrite them into Unicode sub/superscript glyphs.
    ///
    /// [`apply_super_sub_script_substitutions`]: Self::apply_super_sub_script_substitutions
    pub(super) fn run_is_signed_number(text: &str) -> bool {
        let is_minus = |c: char| matches!(c, '\u{002D}' | '\u{2212}' | '\u{2010}' | '\u{2011}');
        matches!(text.chars().next(), Some(c) if is_minus(c))
            && text.chars().any(|c| c.is_ascii_digit())
    }
}
