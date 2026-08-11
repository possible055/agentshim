use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Port of the plain-text RTL reading-order correction to the converter
    /// pipeline's [`crate::pipeline::OrderedTextSpan`] sequence, so `to_markdown`
    /// and `to_html` produce logical-order Arabic/Hebrew.
    ///
    /// The plain-text path fixes RTL during structure assembly
    /// ([`order_pure_rtl_spans`] for span order, [`push_span_text_bidi`] for
    /// per-glyph order). The converter pipeline never reaches those — it orders
    /// spans by MCID/geometry and the converters emit each span's text verbatim
    /// — so visual-order Arabic leaked through reversed and scrambled. This pass
    /// groups the reading-order sequence into visual lines by a font-relative Y
    /// tolerance (matching the converters' own line-break test) and, for each
    /// line that is purely right-to-left, emits the spans rightmost-first and
    /// rewrites each span's text to logical order:
    /// - a pure-RTL span: strip interior cursive-join spaces (§14.8.2.3.3) then
    ///   reverse keeping combining marks attached ([`reverse_rtl_keeping_marks`]);
    /// - a neutral-only span: reverse so trailing punctuation re-attaches to the
    ///   preceding word (UAX #9 N1/N2, [`is_reversible_rtl_neutral_span`]).
    ///
    /// Mixed RTL+Latin lines are left untouched (full UAX #9 deferred), and the
    /// whole pass is skipped when the page has no RTL characters.
    ///
    /// KNOWN LIMITATION (md/html only): this pass orders a pure-RTL line by
    /// sorting whole SPANS by `bbox.x` (rightmost first), so it cannot place a
    /// standalone zero-width glyph (e.g. an Arabic qaf whose x falls inside an
    /// already-merged word span) at its correct *intra-word* slot — the glyph
    /// sorts before/after the word instead (`القهوة` → `قالهوة`). The plain-text
    /// path avoids this by reconstructing at the GLYPH level
    /// ([`merge_interleaved_rtl_lines`] / [`merge_rtl_line_to_visual_span`],
    /// which explode each span to per-glyph bases via `to_chars`). To make the
    /// md/html path match the text path, either run `merge_interleaved_rtl_lines`
    /// on the raw spans before the pipeline orders them, or rebuild each gated
    /// line here at the glyph level instead of the whole-span x-sort.
    pub(super) fn apply_rtl_logical_order_to_ordered_spans(
        spans: &mut [crate::pipeline::OrderedTextSpan],
    ) {
        use crate::text::rtl_detector::is_rtl_text;
        use crate::utils::safe_float_cmp;

        let has_any_rtl = |s: &crate::pipeline::OrderedTextSpan| {
            s.span.text.chars().any(|c| is_rtl_text(c as u32))
        };
        if !spans.iter().any(has_any_rtl) {
            return; // fast path: pure-LTR page is byte-identical
        }

        // The converter pipeline's reading order can interleave the spans of a
        // single RTL line (the MCID/XYCut order is not strictly top-to-bottom),
        // which would break the consecutive line-grouping below — a display
        // heading then scatters into one `#` per fragment. On a page that is
        // dominantly right-to-left, re-sort by Y first so each visual line is
        // contiguous; Y-descending IS the reading order there. Mixed / LTR-
        // dominant pages keep the pipeline order so LTR runs are not disturbed.
        let rtl_letters = spans
            .iter()
            .flat_map(|s| s.span.text.chars())
            .filter(|c| is_rtl_text(*c as u32))
            .count();
        let latin_letters = spans
            .iter()
            .flat_map(|s| s.span.text.chars())
            .filter(|c| c.is_ascii_alphabetic())
            .count();
        if rtl_letters > latin_letters.max(1) * 2 {
            spans.sort_by(|a, b| safe_float_cmp(b.span.bbox.y, a.span.bbox.y));
        }

        let n = spans.len();
        let mut i = 0;
        while i < n {
            // Group the maximal run of spans on the same visual line, using the
            // same font-relative tolerance the converters use for line breaks.
            let anchor_y = spans[i].span.bbox.y;
            let mut j = i + 1;
            while j < n {
                let fs = spans[i]
                    .span
                    .font_size
                    .max(spans[j].span.font_size)
                    .max(1.0);
                if !spans[j].span.bbox.y.is_finite()
                    || (spans[j].span.bbox.y - anchor_y).abs() > 0.5 * fs
                {
                    break;
                }
                j += 1;
            }
            let line = &mut spans[i..j];

            let has_rtl = line.iter().any(has_any_rtl);
            let has_latin = line
                .iter()
                .any(|s| s.span.text.chars().any(|c| c.is_ascii_alphabetic()));
            if has_rtl && !has_latin {
                // Logical RTL order is rightmost glyph first.
                line.sort_by(|a, b| safe_float_cmp(b.span.bbox.x, a.span.bbox.x));
                // Snap every span on this line to a single baseline. RTL
                // producers jitter glyphs a few points off the baseline (hamza
                // seats, marks), and the converters break a line whenever the
                // Y delta between consecutive spans exceeds ~0.5em — which,
                // after the X-descending reorder, would shatter one line into
                // many spurious one-word "lines" (each then mis-promoted to a
                // heading). Collapsing the jitter keeps the line intact.
                for s in line.iter_mut() {
                    s.span.bbox.y = anchor_y;
                }
                for s in line.iter_mut() {
                    let mut rtl = 0usize;
                    let mut latin = false;
                    for c in s.span.text.chars() {
                        if c.is_whitespace() {
                            continue;
                        }
                        if c.is_ascii_alphabetic() {
                            latin = true;
                            break;
                        }
                        if is_rtl_text(c as u32) {
                            rtl += 1;
                        }
                    }
                    if rtl >= 2 && !latin {
                        s.span.text = Self::reverse_rtl_keeping_marks(
                            &Self::strip_interior_arabic_spaces(&s.span.text),
                        )
                        .replace(Self::RTL_WORD_BOUNDARY, " ");
                    } else if Self::is_reversible_rtl_neutral_span(&s.span.text) {
                        s.span.text = s.span.text.chars().rev().collect();
                    }
                }
            }
            i = j;
        }

        // The converters re-sort by `reading_order` before emitting, so the
        // span-order changes above (the Y pre-sort and the per-line X-descending
        // reorder) only take effect once that field reflects the new sequence.
        for (idx, s) in spans.iter_mut().enumerate() {
            s.reading_order = idx;
            // Defensive: restore any word-boundary sentinel that the reorder
            // branch above did not reach (e.g. a merged pure-RTL span that ended
            // up Y-banded with a Latin neighbour so its line took the LTR path),
            // so the marker can never leak into md/html output.
            if s.span.text.contains(Self::RTL_WORD_BOUNDARY) {
                s.span.text = s.span.text.replace(Self::RTL_WORD_BOUNDARY, " ");
            }
        }
    }

    ///
    /// Used by paths that operate on raw spans rather than ordered
    /// spans (`extract_page_text`, `extract_structured`,
    /// `extract_spans_with_reading_order`). Mutates each covered span's
    /// text to the replacement (run-first only) or clears it
    /// (continuation / suppress-only / non-first-page coverage); fully
    /// suppressed spans are removed.
    ///
    /// Untagged documents and pages with no coverage are no-ops.
    pub(crate) fn apply_actualtext_to_spans(
        &self,
        page_index: usize,
        spans: &mut Vec<crate::layout::TextSpan>,
    ) {
        let Some(idx) = self.actualtext_index() else {
            return;
        };
        if idx.covered_mcids.is_empty() {
            return;
        }
        let mc_wins: HashSet<u32> = self
            .mc_actualtext_mcids
            .lock_or_recover()
            .get(&page_index)
            .cloned()
            .unwrap_or_default();

        let default_scope = crate::structure::McidScope::Page(page_index as u32);
        // Visibility = "has at least one raw span at this (scope, mcid)".
        // glyph_text accumulates each key's rendered text for the §14.9.4
        // conformance gate (decline destructive replacements).
        let mut present: HashSet<(crate::structure::McidScope, u32)> = HashSet::new();
        let mut glyph_text: HashMap<(crate::structure::McidScope, u32), String> = HashMap::new();
        for s in spans.iter() {
            if let Some(m) = s.mcid {
                let scope = s.mcid_scope.clone().unwrap_or(default_scope.clone());
                present.insert((scope.clone(), m));
                glyph_text.entry((scope, m)).or_default().push_str(&s.text);
            }
        }
        // Walk the structure-tree's per-page MCID order so the
        // consecutive-run dedup matches the assemblers'.
        let mcid_order = self
            .struct_tree_marked()
            .map(|t| self.cached_mcid_order_for_page(&t, page_index as u32))
            .unwrap_or_default();
        let actions = Self::actualtext_actions_for_page(
            Some(&idx),
            &mcid_order,
            |scope, m| present.contains(&(scope.clone(), m)),
            &mc_wins,
            &glyph_text,
        );
        if actions.is_empty() {
            return;
        }

        // Apply actions to the raw spans. EmitAndSuppress mutates the
        // first span of the (scope, mcid) key; subsequent spans for
        // the same key are dropped (so a key with multiple spans
        // collapses to one span carrying the replacement). Suppress
        // drops every span with that key.
        let mut emit_used: HashSet<(crate::structure::McidScope, u32)> = HashSet::new();
        let mut drop_idx: Vec<usize> = Vec::new();
        for (i, s) in spans.iter_mut().enumerate() {
            let Some(m) = s.mcid else { continue };
            let scope = s.mcid_scope.clone().unwrap_or(default_scope.clone());
            let key = (scope, m);
            match actions.get(&key) {
                Some(ActualTextAction::EmitAndSuppress(repl)) => {
                    if emit_used.insert(key) {
                        s.text = repl.to_string();
                    } else {
                        s.text.clear();
                        drop_idx.push(i);
                    }
                }
                Some(ActualTextAction::Suppress) => {
                    s.text.clear();
                    drop_idx.push(i);
                }
                None => {}
            }
        }
        for &i in drop_idx.iter().rev() {
            spans.remove(i);
        }
    }

    /// Apply struct-tree-scope `/ActualText` to a vector of ordered
    /// spans, in place. Mirrors [`Self::apply_actualtext_to_spans`]
    /// over the converters' [`crate::pipeline::OrderedTextSpan`]
    /// shape; renumbers `reading_order` after dropping suppressed
    /// spans so downstream converters see a contiguous sequence.
    /// Tag ordered spans that fall within a `/Link` annotation's rectangle
    /// with its resolved URI, so the markdown/HTML converters can emit
    /// hyperlinks (ISO 32000-1 §12.5.6.5 Link annotations + §12.6.4.7 URI
    /// actions). Spans and link rectangles share PDF user-space coordinates,
    /// so a span is linked when its bbox centre lies inside the rectangle.
    pub(crate) fn apply_link_annotations_to_ordered_spans(
        &self,
        page_index: usize,
        ordered: &mut [crate::pipeline::OrderedTextSpan],
    ) {
        use crate::annotation_types::AnnotationSubtype;
        use crate::annotations::LinkAction;

        let annots = match self.get_annotations(page_index) {
            Ok(a) => a,
            Err(_) => return,
        };
        let mut links: Vec<([f32; 4], std::sync::Arc<str>)> = Vec::new();
        for a in &annots {
            if a.subtype_enum != AnnotationSubtype::Link {
                continue;
            }
            let uri = match &a.action {
                Some(LinkAction::Uri(u)) if !u.is_empty() => {
                    std::sync::Arc::<str>::from(u.as_str())
                }
                _ => continue,
            };
            if let Some(r) = a.rect {
                links.push((
                    [
                        r[0].min(r[2]) as f32,
                        r[1].min(r[3]) as f32,
                        r[0].max(r[2]) as f32,
                        r[1].max(r[3]) as f32,
                    ],
                    uri,
                ));
            }
        }
        if links.is_empty() {
            return;
        }
        for s in ordered.iter_mut() {
            let b = &s.span.bbox;
            let (sx0, sy0, sx1, sy1) = (b.x, b.y, b.x + b.width, b.y + b.height);
            // A span is linked when its bbox overlaps the annotation rectangle.
            // Overlap (rather than centre-in-rect) keeps the link when adjacent
            // runs are merged into one wide span the small link rect only
            // partially covers — the URL is preserved rather than lost.
            if let Some((_, uri)) = links
                .iter()
                .find(|(r, _)| sx0 < r[2] && sx1 > r[0] && sy0 < r[3] && sy1 > r[1])
            {
                s.link_uri = Some(uri.clone());
            }
        }
    }

    pub(crate) fn apply_actualtext_to_ordered_spans(
        &self,
        page_index: usize,
        ordered: &mut Vec<crate::pipeline::OrderedTextSpan>,
    ) {
        let Some(idx) = self.actualtext_index() else {
            return;
        };
        if idx.covered_mcids.is_empty() {
            return;
        }
        let mc_wins: HashSet<u32> = self
            .mc_actualtext_mcids
            .lock_or_recover()
            .get(&page_index)
            .cloned()
            .unwrap_or_default();

        let default_scope = crate::structure::McidScope::Page(page_index as u32);
        let mut present: HashSet<(crate::structure::McidScope, u32)> = HashSet::new();
        let mut glyph_text: HashMap<(crate::structure::McidScope, u32), String> = HashMap::new();
        for o in ordered.iter() {
            if let Some(m) = o.span.mcid {
                let scope = o.span.mcid_scope.clone().unwrap_or(default_scope.clone());
                present.insert((scope.clone(), m));
                glyph_text
                    .entry((scope, m))
                    .or_default()
                    .push_str(&o.span.text);
            }
        }
        let mcid_order = self
            .struct_tree_marked()
            .map(|t| self.cached_mcid_order_for_page(&t, page_index as u32))
            .unwrap_or_default();
        let actions = Self::actualtext_actions_for_page(
            Some(&idx),
            &mcid_order,
            |scope, m| present.contains(&(scope.clone(), m)),
            &mc_wins,
            &glyph_text,
        );
        if actions.is_empty() {
            return;
        }

        let mut emit_used: HashSet<(crate::structure::McidScope, u32)> = HashSet::new();
        for o in ordered.iter_mut() {
            let Some(m) = o.span.mcid else { continue };
            let scope = o.span.mcid_scope.clone().unwrap_or(default_scope.clone());
            let key = (scope, m);
            match actions.get(&key) {
                Some(ActualTextAction::EmitAndSuppress(repl)) => {
                    if emit_used.insert(key) {
                        o.span.text = repl.to_string();
                        o.actualtext_replacement = Some(repl.clone());
                    } else {
                        o.span.text.clear();
                        o.actualtext_replacement = Some(std::sync::Arc::from(""));
                    }
                }
                Some(ActualTextAction::Suppress) => {
                    o.span.text.clear();
                    o.actualtext_replacement = Some(std::sync::Arc::from(""));
                }
                None => {}
            }
        }

        ordered.retain(|o| !o.is_suppressed());
        for (i, o) in ordered.iter_mut().enumerate() {
            o.reading_order = i;
        }
    }

    /// Compute the per-page `MCID → ActualTextAction` map.
    ///
    /// Walks `mcid_order` (the structure-tree's per-page MCID sequence
    /// in pre-order) and groups consecutive covered MCIDs by the
    /// replacement text they share. Each group emits ONE replacement at
    /// the first visible-and-not-MC-scope-wins MCID; the rest of the
    /// group is marked `Suppress` (raw glyphs dropped). MCIDs whose
    /// `(page, mcid)` lands in `suppress_only` are always `Suppress`
    /// (their replacement already fired on a different page).
    ///
    /// `visible(mcid)` returns `true` when at least one span carries
    /// the MCID and survives all upstream filters (artifact / OCG /
    /// region). A run with zero visible MCIDs is dropped entirely (no
    /// emission, no suppression — nothing to drop).
    ///
    /// MCIDs in `mc_wins` keep the in-stream MC-scope `/ActualText`
    /// replacement applied by the extractor and are exempt from the
    /// ancestor struct-tree scope; they do not break the run dedup —
    /// the run can still find a non-MC-wins MCID to emit at.
    /// §14.9.4 conformance test for a struct-tree `/ActualText` replacement.
    ///
    /// Per ISO 32000-1 §14.9.4 (pdf.md:39253) an `/ActualText` value "shall be
    /// used as a replacement … providing text that is *equivalent to what a
    /// person would see when viewing the content*"; per §14.8.2.4 NOTE 2
    /// (pdf.md:37380) a conforming reader *may choose* whether to use it. We
    /// decline a replacement that is **destructive**: it would suppress glyphs
    /// carrying alphanumeric (letter/digit, any script) content while itself
    /// carrying none — e.g. a producer tagging whole words with `" "` or `"-"`.
    /// Such a value is not "equivalent to what a person would see", so we keep
    /// the rendered glyphs (extracted via ToUnicode, §14.8.2.4) instead.
    /// Legitimate ActualText — the spec's hyphenation EXAMPLE `(c)`→`k-`,
    /// ligature/soft-hyphen substitution (NOTE 3), any real-character
    /// replacement — is alphanumeric and passes.
    pub(super) fn actual_text_is_destructive(replacement: &str, covered_glyphs: &str) -> bool {
        covered_glyphs.chars().any(char::is_alphanumeric)
            && !replacement.chars().any(char::is_alphanumeric)
    }

    pub(super) fn actualtext_actions_for_page<F: Fn(&crate::structure::McidScope, u32) -> bool>(
        idx: Option<&crate::structure::ActualTextIndex>,
        mcid_order: &[(crate::structure::McidScope, u32)],
        visible: F,
        mc_wins: &HashSet<u32>,
        glyph_text: &HashMap<(crate::structure::McidScope, u32), String>,
    ) -> HashMap<(crate::structure::McidScope, u32), ActualTextAction> {
        let mut out: HashMap<(crate::structure::McidScope, u32), ActualTextAction> = HashMap::new();
        let Some(idx) = idx else {
            return out;
        };
        if idx.covered_mcids.is_empty() {
            return out;
        }

        // Two-pass walk to support runs that span the input order
        // perfectly: collect (scope, mcid, replacement?) tuples for
        // covered MCIDs on this page (across all scopes that render on
        // it), then group consecutive equal-replacement entries into
        // runs.
        //
        // Replacement = None for `suppress_only` entries and for
        // covered keys with no text (defensive — shouldn't happen
        // given the builder invariants).
        let mut entries: Vec<(crate::structure::McidScope, u32, Option<&str>)> = Vec::new();
        for (scope, m) in mcid_order {
            let key = (scope.clone(), *m);
            if !idx.covered_mcids.contains(&key) {
                continue;
            }
            if idx.suppress_only.contains(&key) {
                entries.push((scope.clone(), *m, None));
                continue;
            }
            let text = idx.mcid_to_actual_text.get(&key).map(|s| &**s);
            entries.push((scope.clone(), *m, text));
        }

        // Walk entries and assign actions per consecutive same-
        // replacement run.
        let mut i = 0usize;
        while i < entries.len() {
            let repl_opt = entries[i].2;
            // Find the end of the consecutive run sharing this
            // replacement (None matches None — i.e. suppress-only runs
            // also collapse).
            let mut j = i;
            while j < entries.len() && entries[j].2 == repl_opt {
                j += 1;
            }

            if let Some(repl) = repl_opt {
                // §14.9.4 conformance gate (pdf.md:39253 + NOTE 2 pdf.md:37380):
                // if this replacement would suppress alphanumeric glyphs while
                // carrying none itself, it is not "equivalent to what a person
                // would see" — decline it (emit no action for the run) so the
                // rendered glyphs survive. See `actual_text_is_destructive`.
                let run_glyphs: String = entries[i..j]
                    .iter()
                    .filter_map(|e| glyph_text.get(&(e.0.clone(), e.1)))
                    .map(String::as_str)
                    .collect();
                if Self::actual_text_is_destructive(repl, &run_glyphs) {
                    i = j;
                    continue;
                }
                // Find first emit-eligible entry (visible, not MC-wins).
                // MC-wins keys are skipped because their replacement
                // came from the extractor's in-stream BDC /ActualText.
                let mut emit_pick: Option<(crate::structure::McidScope, u32)> = None;
                for entry in &entries[i..j] {
                    if visible(&entry.0, entry.1) && !mc_wins.contains(&entry.1) {
                        emit_pick = Some((entry.0.clone(), entry.1));
                        break;
                    }
                }
                let repl_arc: std::sync::Arc<str> = std::sync::Arc::from(repl);
                for entry in &entries[i..j] {
                    if mc_wins.contains(&entry.1) {
                        // MC-scope wins: do not touch this MCID at all.
                        // The extractor's inline replacement reaches
                        // output unmodified.
                        continue;
                    }
                    let key = (entry.0.clone(), entry.1);
                    if emit_pick.as_ref() == Some(&key) {
                        out.insert(key, ActualTextAction::EmitAndSuppress(repl_arc.clone()));
                    } else {
                        out.insert(key, ActualTextAction::Suppress);
                    }
                }
            } else {
                // suppress_only run: every key is suppressed (no
                // emission). MC-wins MCIDs stay untouched.
                for entry in &entries[i..j] {
                    if mc_wins.contains(&entry.1) {
                        continue;
                    }
                    out.insert((entry.0.clone(), entry.1), ActualTextAction::Suppress);
                }
            }

            i = j;
        }
        out
    }

    /// Page's MCID reading order from the all-pages traversal cache
    /// (`structure_content_cache`, populated once). `build_context` previously
    /// re-walked the whole tree per page (≈ O(pages²) on a tagged document);
    /// the cached all-pages walk (#608) yields the same per-page order.
    pub(crate) fn cached_mcid_order_for_page(
        &self,
        struct_tree: &crate::structure::StructTreeRoot,
        page_index: u32,
    ) -> Vec<(crate::structure::McidScope, u32)> {
        if self.structure_content_cache.lock_or_recover().is_none() {
            let all_content = crate::structure::traverse_structure_tree_all_pages(struct_tree);
            *self.structure_content_cache.lock_or_recover() = Some(all_content);
        }
        self.structure_content_cache
            .lock_or_recover()
            .as_ref()
            .and_then(|c| c.get(&page_index))
            .map(|content| {
                content
                    .iter()
                    .filter_map(|c| {
                        // Word break markers have mcid=None; skip.
                        let m = c.mcid?;
                        // Page-scoped MCIDs default to Page(c.page) when
                        // the parser didn't capture a scope. New parses
                        // always populate `mcid_scope`; the unwrap_or
                        // is for legacy traversals only.
                        let scope = c
                            .mcid_scope
                            .clone()
                            .unwrap_or(crate::structure::McidScope::Page(c.page));
                        Some((scope, m))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}
