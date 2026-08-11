use super::*;

impl<'doc> TextExtractor<'doc> {
    /// Merge adjacent text spans on the same line to reconstruct complete words.
    ///
    /// PDF content streams often break words into multiple Tj operators for precise
    /// kerning/positioning. This causes word fragmentation like "Intr oduction" instead
    /// of "Introduction". We merge spans that are:
    /// - On the same line (Y coordinates within 1pt)
    /// - Very close horizontally (gap < 3pt, approximately average char width)
    ///
    /// Mark spans whose RTL glyphs were drawn **right-to-left** — the producer
    /// stored the text in LOGICAL order and positioned each glyph individually at
    /// decreasing x (ISO 32000-1 §14.8.2.3.3 method 1). Such spans' characters are
    /// already logical and must NOT be character-reversed by the structure-path
    /// `push_span_text_bidi`. VISUAL storage (glyphs drawn left-to-right) is never
    /// marked, so it keeps the default character-reversal and stays byte-identical.
    ///
    /// MUST run on the raw stream order (right after the content stream is parsed),
    /// before `sort_spans_by_reading_order`, which reorders the spans into
    /// left-to-right and erases the draw direction.
    ///
    /// The draw direction is the only signal that separates logical-stored RTL
    /// from visual-stored RTL when both use base-form characters with no Arabic
    /// presentation forms and no `/ReversedChars` (the two are otherwise
    /// indistinguishable yet need opposite treatment).
    pub(super) fn detect_rtl_draw_direction(&mut self) {
        use crate::text::rtl_detector::is_rtl_text;
        fn is_rtl_span(s: &TextSpan) -> bool {
            let mut rtl = false;
            for c in s.text.chars() {
                if c.is_ascii_alphabetic() {
                    return false;
                }
                if is_rtl_text(c as u32) {
                    rtl = true;
                }
            }
            rtl
        }
        let n = self.spans.len();
        // Index of the previous RTL span in stream order; a pure-whitespace span
        // between two RTL glyphs (a word break) does not break the run.
        let mut prev: Option<usize> = None;
        for i in 0..n {
            if self.spans[i].text.chars().all(char::is_whitespace) {
                continue;
            }
            if !is_rtl_span(&self.spans[i]) {
                prev = None;
                continue;
            }
            if let Some(p) = prev {
                let same_line = (self.spans[i].bbox.y - self.spans[p].bbox.y).abs()
                    < self.spans[p].font_size.max(1.0) * 0.6;
                // The incoming glyph sits to the LEFT of the previous one on the
                // same baseline ⇒ right-to-left placement ⇒ logical storage.
                if same_line && self.spans[i].bbox.x < self.spans[p].bbox.x - 0.5 {
                    self.spans[i].rtl_draw_logical = true;
                    self.spans[p].rtl_draw_logical = true;
                }
            }
            prev = Some(i);
        }
    }

    /// Per-line bimodal word-gap thresholds for the narrow-space rescue (#847).
    ///
    /// The fixed intra-word kerning guard in `should_insert_space`
    /// (0.75× the space-glyph advance) suppresses genuine but *narrow* word
    /// gaps on condensed/tracked lines — a bold heading or a running footer
    /// typeset with NO space glyph, whose inter-word gaps are ~0.18 em, just
    /// under the guard. A fixed magnitude cannot separate a 0.18 em word gap
    /// from ~0.15 em intra-word kerning. But within one line the intra-word
    /// glyph gaps cluster near zero (tight/slightly-overlapping side-bearings)
    /// while the inter-word gaps form a distinct larger cluster: a clean
    /// bimodal split that pins the word boundary *regardless of absolute
    /// magnitude*.
    ///
    /// This walks the content-order span list, groups it into baseline runs,
    /// and for each run whose inter-span gaps are clearly bimodal returns the
    /// gap value separating the two clusters (indexed per span). Spans on
    /// unimodal or too-short lines get `None` and keep the default guard. The
    /// merge loop uses a returned threshold only to *rescue* a suppressed word
    /// gap — it never removes a space the default logic already inserts.
    pub(super) fn bimodal_line_gap_thresholds(spans: &[TextSpan]) -> Vec<Option<f32>> {
        let n = spans.len();
        let mut out = vec![None; n];
        let mut i = 0;
        while i < n {
            // Extend a run of consecutive same-baseline spans.
            let mut j = i;
            while j + 1 < n && (spans[j].bbox.y - spans[j + 1].bbox.y).abs() < 1.0 {
                j += 1;
            }
            if j > i {
                let fs = spans[i..=j]
                    .iter()
                    .map(|s| s.font_size)
                    .fold(0.0f32, f32::max)
                    .max(1.0);
                // ALL consecutive gaps (intra-word gaps are near-zero or
                // slightly negative, so they must be kept, not filtered) — but
                // ONLY between glyphs sharing a baseline. A super/subscript sits
                // at a baseline shift (~0.15 em) and its horizontal gap to the
                // base is the same ~0.10 em magnitude as a condensed footer's
                // word gap; including it would let the narrow-gap rescue split a
                // math subscript from its variable (`λᵢ` → `λ i`), which the
                // advance-aware extractors correctly do NOT do. Excluding
                // baseline-shifted pairs keeps the footer word gap (same
                // baseline) while leaving dense math untouched.
                let gaps: Vec<f32> = (i..j)
                    .filter(|&k| (spans[k].bbox.y - spans[k + 1].bbox.y).abs() < fs * 0.04)
                    .map(|k| spans[k + 1].bbox.x - (spans[k].bbox.x + spans[k].bbox.width))
                    .collect();
                if let Some(split) = Self::bimodal_gap_split(&gaps, fs) {
                    for slot in out.iter_mut().take(j + 1).skip(i) {
                        *slot = Some(split);
                    }
                }
            }
            i = j + 1;
        }
        out
    }

    /// Given the consecutive inter-span gaps of one baseline run, return the
    /// threshold separating an intra-word cluster from an inter-word cluster
    /// when the distribution is clearly bimodal, else `None`. `fs` is the
    /// run's font size; all bounds are expressed as em fractions so headings
    /// and body calibrate independently.
    pub(super) fn bimodal_gap_split(gaps: &[f32], fs: f32) -> Option<f32> {
        if gaps.len() < 3 {
            return None;
        }
        let mut sorted = gaps.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // Return the LOWEST cluster border, not the widest jump: walking the
        // sorted gaps from the bottom, the first jump that leaves the intra-word
        // cluster for a real word gap. A qualifying border needs
        //   * an intra-word-sized low side (< 0.10 em — kerning, tight
        //     side-bearings, or overlap),
        //   * a high side that is a real (if narrow) word gap (>= 0.09 em) —
        //     reaching the ~0.10 em gaps of condensed running footers that
        //     pymupdf/pdfplumber's fixed thresholds miss (an explicit positive
        //     advance IS a word-boundary signal, ISO 32000-1 §9.4.4), and
        //   * a real separation between them (>= 0.08 em), not a smooth spread.
        // Taking the LOWEST such border handles a *multi-level* condensed line —
        // tight intra-word gaps, a narrow ~0.10 em word gap, AND a wide real
        // space glyph — splitting at every level above intra-word, matching the
        // advance-aware extractors (pdfminer, poppler). A single-word line (all
        // gaps low) yields no qualifying border and returns None. The caller
        // feeds only SAME-BASELINE gaps, so a math subscript gap of the same
        // magnitude (which sits at a baseline shift) never enters this
        // distribution and is not split.
        for w in sorted.windows(2) {
            let (lo, hi) = (w[0], w[1]);
            if lo < fs * 0.10 && hi >= fs * 0.09 && (hi - lo) >= fs * 0.08 {
                return Some((lo + hi) * 0.5);
            }
        }
        None
    }
}
