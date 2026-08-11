use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Walk `input` and repair runs of Latin-1 Supplement characters
    /// whose raw byte values form a valid UTF-8 sequence whose decoded
    /// codepoints include at least one non-Latin-1 character.
    ///
    /// This undoes the most common shape of "Cyrillic served as
    /// Latin-1" mojibake that surfaces on PDFs whose fonts have no
    /// ToUnicode CMap. The decoded-codepoint gate (≥ U+0100 somewhere
    /// in the decoded run) ensures genuine Latin-1 content like
    /// "Résumé" — which also decodes as valid UTF-8 but stays entirely
    /// within U+0000..U+00FF — is left alone.
    pub(super) fn repair_utf8_mojibake(input: &str) -> String {
        // Fast-path: if the string contains no Latin-1 Supplement codepoints
        // (U+0080..=U+00FF), there is nothing to repair. This avoids the
        // O(n) `Vec<char>` allocation on every ASCII-only page.
        if !input.chars().any(|c| matches!(c as u32, 0x80..=0xFF)) {
            return input.to_string();
        }
        let mut out = String::with_capacity(input.len());
        let chars: Vec<char> = input.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let mut j = i;
            while j < chars.len() {
                let cc = chars[j] as u32;
                if (0x80..=0xFF).contains(&cc) {
                    j += 1;
                } else {
                    break;
                }
            }
            if j - i >= 2 {
                let bytes: Vec<u8> = chars[i..j].iter().map(|&c| c as u8).collect();
                if let Ok(decoded) = std::str::from_utf8(&bytes) {
                    if decoded.chars().any(|c| c as u32 > 0xFF) {
                        out.push_str(decoded);
                        i = j;
                        continue;
                    }
                }
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    /// Extract text from all pages of the document.
    ///
    /// Concatenates text from every page, separated by form feed characters (`\x0c`).
    /// This is a convenience method equivalent to calling `extract_text()` for each page.
    ///
    /// # Returns
    ///
    /// The combined text from all pages.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("paper.pdf")?;
    /// let all_text = doc.extract_all_text()?;
    /// println!("Full document: {} chars", all_text.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_all_text(&self) -> Result<String> {
        let num_pages = self.page_count()?;
        let mut result = String::new();

        for i in 0..num_pages {
            if i > 0 {
                result.push('\x0c'); // Form feed page separator
            }
            match self.extract_text(i) {
                Ok(text) => result.push_str(&text),
                Err(e) => {
                    log::warn!("Failed to extract text from page {}: {}", i, e);
                }
            }
        }

        Ok(result)
    }

    /// Filter leaked PDF internal metadata from extracted text.
    ///
    /// Some PDFs embed inline ColorSpace definitions (CalRGB, CalGray, Lab) that
    /// get parsed as text content. This removes known metadata patterns like
    /// "WhitePoint [ ... ]", "BlackPoint [ ... ]", "Gamma [ ... ]", "Matrix [ ... ]".
    pub(super) fn filter_leaked_metadata(text: &str) -> String {
        // Known PDF metadata keys that should never appear in extracted text.
        // These come from CalRGB/CalGray/Lab color space dictionaries.
        const METADATA_PATTERNS: &[&str] = &[
            "WhitePoint",
            "BlackPoint",
            "Gamma",
            "Matrix",
            "CalRGB",
            "CalGray",
        ];

        // Quick check: if none of the patterns appear, return as-is
        if !METADATA_PATTERNS.iter().any(|p| text.contains(p)) {
            return text.to_string();
        }

        // Filter line-by-line: remove lines that look like PDF metadata
        let mut result = String::with_capacity(text.len());
        for line in text.lines() {
            let trimmed = line.trim();
            // Skip lines matching "MetadataKey [ ... ]" or "MetadataKey [ ... ] ..."
            let is_metadata = METADATA_PATTERNS.iter().any(|pattern| {
                if let Some(rest) = trimmed.strip_prefix(pattern) {
                    // Must be followed by whitespace and bracket, or end of line
                    let rest = rest.trim_start();
                    rest.is_empty()
                        || rest.starts_with('[')
                        || rest.starts_with('/')
                        || rest.starts_with('<')
                } else {
                    false
                }
            });

            if !is_metadata {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(line);
            }
        }

        result
    }

    /// Normalize Kangxi Radical characters to CJK Unified Ideographs.
    ///
    /// Some PDF fonts/CMaps emit Kangxi Radicals (U+2F00–U+2FD5) or CJK Radicals
    /// Supplement (U+2E80–U+2EFF) instead of the standard CJK Unified Ideographs.
    /// While visually similar, these are different Unicode codepoints and will break
    /// text search, string matching, and NLP pipelines.
    pub(super) fn normalize_kangxi_radicals(text: &str) -> String {
        // Quick check: if no characters in the Kangxi/Supplement range, return as-is
        if !text.chars().any(|c| {
            let cp = c as u32;
            (0x2E80..=0x2EFF).contains(&cp) || (0x2F00..=0x2FD5).contains(&cp)
        }) {
            return text.to_string();
        }

        text.chars()
            .map(|c| crate::text::kangxi::kangxi_to_unified(c).unwrap_or(c))
            .collect()
    }

    /// Reverse visual-order RTL character runs to logical reading order.
    ///
    /// Some PDFs position Arabic/Hebrew characters individually left-to-right
    /// (visual order). For correct text extraction, runs of single-character
    /// RTL spans on the same line are collected, reversed, and merged into
    /// a single span to produce correct logical reading order.
    pub(super) fn reverse_rtl_visual_order_runs(spans: &mut Vec<TextSpan>) {
        use crate::text::rtl_detector::is_rtl_text;

        // Pass 0: reverse visual-order characters inside a single span
        // when the producer clearly emitted pre-shaped Arabic.
        //
        // Some PDFs (e.g. `ArabicCIDTrueType.pdf` in the pdfjs regression
        // corpus) emit Arabic with an entire line as a single Tj-produced
        // span whose `text` is stored in *visual* order — rightmost
        // rendered glyph first. That matches what the content stream
        // literally drew on the page, but downstream consumers expect
        // reading-order (logical) text.
        //
        // The gate for reversal is the presence of **Arabic Presentation
        // Forms A or B** (U+FB50-U+FDFF, U+FE70-U+FEFF). Those code points
        // only appear when the PDF producer has explicitly pre-shaped the
        // glyphs, and producers that pre-shape almost universally also
        // store them in visual order because that's the order the content
        // stream draws them. Plain base-Arabic text (U+0600-U+06FF) is
        // left alone because those files are usually already in logical
        // order — the PDF viewer applies shaping and bidi reordering at
        // render time, so reversing would produce a wrong result.
        //
        // We still require at least 4 characters and >50 % non-whitespace
        // RTL ratio so that punctuation or stray markers adjacent to
        // Arabic do not trigger a reversal.
        //
        // Pass 1 below handles the other common shape where each Arabic
        // character is emitted as its own short span and the reversal is
        // a span-granularity concern. The two passes are independent:
        // a span either fires Pass 0 (pre-shaped, reverse in place) or
        // Pass 1 (per-glyph spans, reverse span order), never both.
        //
        // This is separate from `normalize_arabic_presentation_forms`,
        // which runs later on the assembled output string and unshapes
        // contextual glyphs back to their base Unicode letters.
        for span in spans.iter_mut() {
            let mut total = 0usize;
            let mut rtl_count = 0usize;
            let mut has_presentation_form = false;
            for c in span.text.chars() {
                if c.is_whitespace() {
                    continue;
                }
                total += 1;
                let cp = c as u32;
                if is_rtl_text(cp) {
                    rtl_count += 1;
                }
                if (0xFB50..=0xFDFF).contains(&cp) || (0xFE70..=0xFEFF).contains(&cp) {
                    has_presentation_form = true;
                }
            }
            // #557: Pass 0 only applies to a *whole-line* visual-order span —
            // one span holding several words separated by internal whitespace,
            // in the order the content stream drew them (rightmost first). When
            // the extractor instead emits one span PER WORD (the common
            // CID-TrueType case, e.g. ArabicCIDTrueType.pdf), each word's
            // characters are already in logical order, so char-reversing them
            // here corrupts them. Their right-to-left *word* order is fixed
            // separately by the span-run reversal pass below. Gate on internal
            // whitespace so per-word logical spans are left untouched.
            let has_internal_whitespace = span.text.trim().chars().any(|c| c.is_whitespace());
            if has_presentation_form
                && has_internal_whitespace
                && total >= 4
                && rtl_count * 2 > total
            {
                let reversed: String = span.text.chars().rev().collect();
                span.text = reversed;
            }
        }

        // #557 Pass 0.5: per-word RTL span ORDER. The row-aware sort placed
        // spans left-to-right (x ascending), but a right-to-left script reads
        // the words in the opposite direction. For each maximal run of
        // consecutive same-line spans that is purely RTL (every non-space span
        // holds RTL letters and no Latin letters), reverse the run's order so
        // the words come out in logical reading order. Each word's characters
        // are left as-is (they are already logical — see Pass 0's gate).
        let is_space = |s: &TextSpan| s.text.trim().is_empty();
        let is_rtl_word = |s: &TextSpan| {
            let mut has_rtl = false;
            for c in s.text.chars() {
                if c.is_ascii_alphabetic() {
                    return false; // Latin letter → not a pure-RTL word
                }
                if is_rtl_text(c as u32) {
                    has_rtl = true;
                }
            }
            has_rtl
        };
        let mut i = 0;
        while i < spans.len() {
            if !is_rtl_word(&spans[i]) {
                i += 1;
                continue;
            }
            let y = spans[i].bbox.y;
            let start = i;
            let mut end = i + 1;
            while end < spans.len()
                && (spans[end].bbox.y - y).abs() < 2.0
                && (is_rtl_word(&spans[end]) || is_space(&spans[end]))
            {
                end += 1;
            }
            // Trim trailing space spans so separators stay between words.
            let mut last = end;
            while last > start + 1 && is_space(&spans[last - 1]) {
                last -= 1;
            }
            if last - start >= 2 {
                spans[start..last].reverse();
            }
            i = end;
        }

        if spans.len() < 4 {
            return;
        }

        // Iterate forward; drain consumed runs so subsequent indices stay valid
        let mut i = 0;
        while i < spans.len() {
            // Check if this span starts an RTL single-char run
            let is_short_rtl = spans[i].text.chars().count() <= 2
                && spans[i].text.chars().any(|c| is_rtl_text(c as u32));

            if !is_short_rtl {
                i += 1;
                continue;
            }

            // Find the end of this RTL run (consecutive short spans on same line)
            let run_start = i;
            let y = spans[i].bbox.y;
            let mut j = i + 1;
            while j < spans.len() {
                let y_same = (spans[j].bbox.y - y).abs() < 2.0;
                let is_short = spans[j].text.chars().count() <= 2;
                let has_rtl_or_space = spans[j]
                    .text
                    .chars()
                    .all(|c| is_rtl_text(c as u32) || c == ' ');
                if y_same && is_short && has_rtl_or_space {
                    j += 1;
                } else {
                    break;
                }
            }
            let run_end = j;
            let run_len = run_end - run_start;

            // Only process runs of 4+ spans (avoid false positives)
            if run_len >= 4 {
                // Collect span texts in reverse order (visual LTR → logical RTL).
                // Preserve space spans as word separators.
                let mut reversed_text = String::new();
                for span in spans[run_start..run_end].iter().rev() {
                    reversed_text.push_str(&span.text);
                }

                // Merge into first span, expand bbox to cover entire run
                let last_span = &spans[run_end - 1];
                let new_width = (last_span.bbox.x + last_span.bbox.width) - spans[run_start].bbox.x;
                spans[run_start].text = reversed_text;
                spans[run_start].bbox.width = new_width;

                // Remove the rest of the run
                spans.drain(run_start + 1..run_end);

                i = run_start + 1;
            } else {
                i = run_end;
            }
        }
    }

    /// Normalize Arabic Presentation Forms to base Unicode characters.
    ///
    /// Arabic PDFs often use presentation forms (U+FE70-U+FEFF for Forms-B,
    /// U+FB50-U+FDFF for Forms-A) which represent contextual glyph shapes.
    /// For text extraction, these should be normalized to base characters.
    pub(super) fn normalize_arabic_presentation_forms(text: &str) -> String {
        // Quick check: skip if no Arabic presentation form characters
        if !text.chars().any(|c| {
            let cp = c as u32;
            (0xFB50..=0xFDFF).contains(&cp) || (0xFE70..=0xFEFF).contains(&cp)
        }) {
            return text.to_string();
        }

        text.chars()
            .map(|c| {
                let cp = c as u32;
                // Arabic Presentation Forms-B (U+FE70-U+FEFF): contextual forms
                // Each base letter has isolated/final/initial/medial forms
                let base = match cp {
                    // Hamza forms
                    0xFE80 => 0x0621,
                    // Alef with Madda
                    0xFE81 | 0xFE82 => 0x0622,
                    // Alef with Hamza Above
                    0xFE83 | 0xFE84 => 0x0623,
                    // Waw with Hamza
                    0xFE85 | 0xFE86 => 0x0624,
                    // Alef with Hamza Below
                    0xFE87 | 0xFE88 => 0x0625,
                    // Yeh with Hamza
                    0xFE89..=0xFE8C => 0x0626,
                    // Alef
                    0xFE8D | 0xFE8E => 0x0627,
                    // Beh
                    0xFE8F..=0xFE92 => 0x0628,
                    // Teh Marbuta
                    0xFE93 | 0xFE94 => 0x0629,
                    // Teh
                    0xFE95..=0xFE98 => 0x062A,
                    // Theh
                    0xFE99..=0xFE9C => 0x062B,
                    // Jeem
                    0xFE9D..=0xFEA0 => 0x062C,
                    // Hah
                    0xFEA1..=0xFEA4 => 0x062D,
                    // Khah
                    0xFEA5..=0xFEA8 => 0x062E,
                    // Dal
                    0xFEA9 | 0xFEAA => 0x062F,
                    // Thal
                    0xFEAB | 0xFEAC => 0x0630,
                    // Reh
                    0xFEAD | 0xFEAE => 0x0631,
                    // Zain
                    0xFEAF | 0xFEB0 => 0x0632,
                    // Seen
                    0xFEB1..=0xFEB4 => 0x0633,
                    // Sheen
                    0xFEB5..=0xFEB8 => 0x0634,
                    // Sad
                    0xFEB9..=0xFEBC => 0x0635,
                    // Dad
                    0xFEBD..=0xFEC0 => 0x0636,
                    // Tah
                    0xFEC1..=0xFEC4 => 0x0637,
                    // Zah
                    0xFEC5..=0xFEC8 => 0x0638,
                    // Ain
                    0xFEC9..=0xFECC => 0x0639,
                    // Ghain
                    0xFECD..=0xFED0 => 0x063A,
                    // Feh
                    0xFED1..=0xFED4 => 0x0641,
                    // Qaf
                    0xFED5..=0xFED8 => 0x0642,
                    // Kaf
                    0xFED9..=0xFEDC => 0x0643,
                    // Lam
                    0xFEDD..=0xFEE0 => 0x0644,
                    // Meem
                    0xFEE1..=0xFEE4 => 0x0645,
                    // Noon
                    0xFEE5..=0xFEE8 => 0x0646,
                    // Heh
                    0xFEE9..=0xFEEC => 0x0647,
                    // Waw
                    0xFEED | 0xFEEE => 0x0648,
                    // Alef Maksura
                    0xFEEF | 0xFEF0 => 0x0649,
                    // Yeh
                    0xFEF1..=0xFEF4 => 0x064A,
                    // Lam-Alef ligatures → expand to two characters
                    0xFEF5 | 0xFEF6 => {
                        // Lam + Alef with Madda
                        return '\u{0644}'; // Just return Lam; Alef is separate
                    }
                    0xFEF7 | 0xFEF8 => {
                        return '\u{0644}'; // Lam + Alef with Hamza Above
                    }
                    0xFEF9 | 0xFEFA => {
                        return '\u{0644}'; // Lam + Alef with Hamza Below
                    }
                    0xFEFB | 0xFEFC => {
                        return '\u{0644}'; // Lam + Alef
                    }
                    // Tatweel (kashida)
                    0xFE70 => 0x064B, // Fathatan isolated
                    0xFE71 => 0x064B, // Tatweel + Fathatan
                    0xFE72 => 0x064C, // Dammatan isolated
                    0xFE74 => 0x064D, // Kasratan isolated
                    0xFE76 => 0x064E, // Fatha isolated
                    0xFE77 => 0x064E, // Fatha medial
                    0xFE78 => 0x064F, // Damma isolated
                    0xFE79 => 0x064F, // Damma medial
                    0xFE7A => 0x0650, // Kasra isolated
                    0xFE7B => 0x0650, // Kasra medial
                    0xFE7C => 0x0651, // Shadda isolated
                    0xFE7D => 0x0651, // Shadda medial
                    0xFE7E => 0x0652, // Sukun isolated
                    0xFE7F => 0x0652, // Sukun medial
                    _ => cp,          // Pass through unchanged
                };
                char::from_u32(base).unwrap_or(c)
            })
            .collect()
    }

    /// Returns the Y tolerance (in points) for treating two spans as
    /// belonging to the same visual line during text assembly.
    ///
    /// The threshold scales with the larger font size so mixed-size runs
    /// (for example superscripts and subscripts) are not split by a fixed
    /// absolute tolerance.
    pub(super) fn same_line_threshold(prev: &TextSpan, current: &TextSpan) -> f32 {
        let max_fs = prev.font_size.max(current.font_size).max(1.0);
        let min_fs = prev.font_size.min(current.font_size).max(1.0);
        // Continuous formula — avoids the step discontinuity at the 4×
        // ratio boundary. Examples:
        //   same-size 12 pt body: max(12×1.2, 12×0.3) = 14.4 pt ← 1.2× leading
        //   heading+body 24+10 pt: max(10×1.2, 24×0.3) = 12.0 pt ← keeps para break
        //   superscript 12+6 pt: max(6×1.2, 12×0.3) = 7.2 pt ← same line
        // Prior formula was max_fs×0.5 for normal ratios; new formula uses 1.2× of the
        // smaller font, which is wider and reduces false newlines for normal leading.
        // Formula: max(min_fs * 1.2, max_fs * 0.3)
        (min_fs * 1.2).max(max_fs * 0.3)
    }

    /// True when a line break falls *inside* a Hangul word (eojeol) that wrapped
    /// mid-syllable — Korean breaks anywhere, not only at word boundaries, so a
    /// mid-eojeol wrap carries no separator in the source and the two halves
    /// must rejoin with nothing ("집고양" ⏎ "이의" → "집고양이의"). An
    /// eojeol-BOUNDARY wrap keeps its explicit inter-eojeol space, so `text`
    /// ends with ' ' and this returns false (the break still separates).
    /// Scoped to Hangul (not Chinese/Japanese) to avoid the CJK
    /// line-break-collapse regressions seen in v0.3.62.
    pub(super) fn hangul_midword_line_wrap(text: &str, prev: &TextSpan, span: &TextSpan) -> bool {
        let is_hangul = |c: char| (0xAC00..=0xD7AF).contains(&(c as u32));
        !text.ends_with(' ')
            && prev.text.chars().next_back().is_some_and(is_hangul)
            && span.text.chars().next().is_some_and(is_hangul)
    }

    /// Returns `true` if `inner` is contained within `outer`,
    /// allowing `eps` points of floating-point slack on all four
    /// edges. Used at the table-retain sites to absorb ~0.02pt drift
    /// in span right-edges relative to table bboxes computed from
    /// min/max reductions over many cell edges.
    pub(super) fn contains_rect_with_tolerance(
        outer: &crate::geometry::Rect,
        inner: &crate::geometry::Rect,
        eps: f32,
    ) -> bool {
        inner.left() >= outer.left() - eps
            && inner.right() <= outer.right() + eps
            && inner.top() >= outer.top() - eps
            && inner.bottom() <= outer.bottom() + eps
    }

    /// Returns `true` if a tentative left-to-right X-ordering of `run`
    /// contains a horizontal gap exceeding
    /// `SAME_LINE_REORDER_MAX_GAP_FACTOR * max(font_size)` between any
    /// two consecutive spans. Used by [`reorder_same_line_runs`] to
    /// reject candidate runs that are vertically close but horizontally
    /// disjoint (e.g. tightly-set footer/header rows split across the
    /// page).
    ///
    /// The slice is not mutated; the X-order is computed on a local
    /// copy of `(left_x, right_x, font_size)` triples.
    pub(super) fn run_has_large_x_gap(run: &[TextSpan]) -> bool {
        if run.len() < 2 {
            return false;
        }

        let mut edges: Vec<(f32, f32, f32)> = run
            .iter()
            .map(|s| (s.bbox.x, s.bbox.x + s.bbox.width, s.font_size))
            .collect();

        edges.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));

        for pair in edges.windows(2) {
            let prev = pair[0];
            let cur = pair[1];

            let gap = cur.0 - prev.1;
            if gap <= 0.0 {
                continue;
            }

            let max_fs = prev.2.max(cur.2).max(1.0);
            if gap > SAME_LINE_REORDER_MAX_GAP_FACTOR * max_fs {
                return true;
            }
        }

        false
    }

    /// True when a candidate run contains spans whose X-extents OVERLAP — the
    /// signature of two (or more) distinct text lines that the same-line Y
    /// tolerance merged into one band, NOT a single line. A real line lays its
    /// spans out left-to-right with non-overlapping advances; only stacked lines
    /// (leading just under `same_line_threshold`, e.g. a two-line title or a
    /// running head sitting above the line below it) put two spans at the same
    /// horizontal position. X-sorting such a band interleaves the two lines word
    /// by word, so the caller must leave it in row order instead. Mirrors
    /// [`run_has_large_x_gap`] for the opposite defect.
    pub(super) fn run_has_x_overlap(run: &[TextSpan]) -> bool {
        if run.len() < 2 {
            return false;
        }

        let mut edges: Vec<(f32, f32, f32)> = run
            .iter()
            .map(|s| (s.bbox.x, s.bbox.x + s.bbox.width, s.font_size))
            .collect();

        edges.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));

        for pair in edges.windows(2) {
            let prev = pair[0];
            let cur = pair[1];

            // prev.right - cur.left > 0 ⇒ the next span starts before the previous
            // one ends (horizontal overlap). Half an em of overlap is well beyond
            // kerning/italic side-bearing and only happens across stacked lines.
            let overlap = prev.1 - cur.0;
            let max_fs = prev.2.max(cur.2).max(1.0);
            if overlap > 0.5 * max_fs {
                return true;
            }
        }

        false
    }

    /// True when a run is structurally two-or-more stacked text LINES: it has at
    /// least two distinct Y levels that EACH carry at least two spans. This
    /// separates a real two-line title / running-head block (many words on each
    /// of two baselines) — where de-interleaving is correct — from a single span
    /// that merely overlaps a line in X (a drop cap, a `©`/`c` mark, a lone
    /// super-script), where the existing X-sort already does the right thing and
    /// reordering by Y would misplace the stray glyph.
    pub(super) fn run_is_stacked_lines(run: &[TextSpan]) -> bool {
        if run.len() < 4 {
            return false; // need ≥2 lines × ≥2 spans
        }
        let mut rows: Vec<(f32, f32)> = run.iter().map(|s| (s.bbox.y, s.font_size)).collect();
        rows.sort_by(|a, b| crate::utils::safe_float_cmp(b.0, a.0));

        let mut multi_rows = 0usize;
        let mut anchor_y = f32::NAN;
        let mut count = 0usize;
        for (y, fs) in rows {
            if anchor_y.is_nan() || (anchor_y - y).abs() <= 0.5 * fs.max(1.0) {
                if anchor_y.is_nan() {
                    anchor_y = y;
                }
                count += 1;
            } else {
                if count >= 2 {
                    multi_rows += 1;
                }
                anchor_y = y;
                count = 1;
            }
        }
        if count >= 2 {
            multi_rows += 1;
        }
        multi_rows >= 2
    }
}
