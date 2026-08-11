use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Re-sort same-line spans by X after row-aware band sorting.
    ///
    /// Row-aware sorting can place off-baseline glyphs such as superscripts or
    /// subscripts in adjacent Y bands before their base glyphs. This helper finds
    /// candidate runs with the existing same-line threshold, then tentatively views
    /// each candidate in X order. If that tentative X order contains a large gap,
    /// the candidate is treated as disjoint footer/header/field content and is
    /// left in the existing row-aware order.
    ///
    /// At the slice level no spans are merged or dropped; successful candidates are
    /// only permuted. Downstream text assembly may then emit the reordered spans
    /// into one visual line, which is the user-observable effect.
    pub(super) fn reorder_same_line_runs(spans: &mut [TextSpan]) {
        let mut i = 0;

        while i < spans.len() {
            let mut j = i + 1;

            while j < spans.len() {
                let anchor = &spans[i];
                let prev = &spans[j - 1];
                let cur = &spans[j];

                let to_prev = (cur.bbox.y - prev.bbox.y).abs();
                let to_anchor = (cur.bbox.y - anchor.bbox.y).abs();

                let tol_prev = Self::same_line_threshold(prev, cur);
                let tol_anchor = Self::same_line_threshold(anchor, cur);

                if to_prev > tol_prev || to_anchor > tol_anchor {
                    break;
                }

                j += 1;
            }

            if j - i > 1 {
                if Self::run_has_large_x_gap(&spans[i..j]) {
                    // Candidate spans are vertically close but not horizontally
                    // contiguous (disjoint header/footer columns). Do not X-sort
                    // them into a fake line; preserve the row-aware order.
                    i = j;
                    continue;
                }

                if Self::run_has_x_overlap(&spans[i..j]) && Self::run_is_stacked_lines(&spans[i..j])
                {
                    // Spans OVERLAP horizontally AND form ≥2 lines of ≥2 spans each:
                    // two stacked lines the Y tolerance merged into one band (a
                    // two-line title, a running head above the line below it). A
                    // flat X-sort interleaves them word by word. De-interleave by
                    // ordering on (Y-descending, then X) so each real line stays
                    // contiguous and in reading order. The stacked-lines gate keeps
                    // a lone overlapping glyph (drop cap, `©`, super-script) on the
                    // normal X-sort path below.
                    spans[i..j].sort_by(|a, b| {
                        crate::utils::safe_float_cmp(b.bbox.y, a.bbox.y)
                            .then(crate::utils::safe_float_cmp(a.bbox.x, b.bbox.x))
                            .then(a.sequence.cmp(&b.sequence))
                    });
                    i = j;
                    continue;
                }

                spans[i..j].sort_by(|a, b| {
                    let cmp = crate::utils::safe_float_cmp(a.bbox.x, b.bbox.x);
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                    a.sequence.cmp(&b.sequence)
                });
            }

            i = j;
        }
    }

    /// Distinguish a genuine tight-kerning overlap of a single word drawn as
    /// two same-font runs ("PLANAL"+"TINA") from an inflated-width artifact.
    ///
    /// A font with no `/Widths` array falls back to a uniform 550/1000-em
    /// advance for every glyph, which over-reports each glyph's width and drags
    /// the previous span's right edge past where the next span really starts —
    /// a fake overlap that the assembler must break with a space (the NASA
    /// "STATION"+"FREEDOM" header case). A genuine kerning overlap, by
    /// contrast, has real per-glyph metrics that VARY across the run, a modest
    /// overlap (well under one em), the same font on both sides, and word
    /// characters at the join. When those hold the two runs are one word and no
    /// space must be synthesized. This works purely on the assembled text — the
    /// spans are left unmerged, so page layout and table detection are
    /// unaffected (a span merge here would shift XY-cut/table statistics).
    pub(super) fn is_reliable_kerning_overlap(prev: &TextSpan, span: &TextSpan, gap: f32) -> bool {
        let fs = prev.font_size.max(span.font_size).max(1.0);
        let prev_last = prev.text.chars().next_back();
        let next_first = span.text.chars().next();
        gap < 0.0
            && gap > -fs
            && prev.font_name == span.font_name
            && prev.font_weight == span.font_weight
            && prev.is_italic == span.is_italic
            && prev_last.is_some_and(|c| c.is_alphanumeric())
            && next_first.is_some_and(|c| c.is_alphanumeric())
            // A lowercase→uppercase transition at the join is a word/sentence
            // boundary ("...with"+"Gp53", "Alg"+"The"), never the middle of a
            // single word split by kerning — real intra-word splits continue in
            // the same case tier ("PLANAL"+"TINA", "eigenv"+"alue"). Excluding
            // it keeps the two overlapping runs as separate words with a space.
            && !(prev_last.is_some_and(|c| c.is_lowercase())
                && next_first.is_some_and(|c| c.is_uppercase()))
            && {
                // Real proportional font metrics take many distinct per-glyph
                // advances; a missing-/Widths fallback emits ONE uniform
                // advance, and coarse/artifact width tables only a couple.
                // Require at least THREE distinct advances so a genuine
                // proportional run ("PLANAL": 6.67/5.56/7.22) is accepted while
                // a 1- or 2-value fallback table (which manufactures fake
                // overlaps between separate words) is not.
                let mut distinct: [i32; 3] = [i32::MIN, i32::MIN, i32::MIN];
                let mut n = 0usize;
                for w in prev.char_widths.iter().map(|w| (w * 100.0).round() as i32) {
                    if !distinct[..n].contains(&w) {
                        if n < 3 {
                            distinct[n] = w;
                        }
                        n += 1;
                        if n >= 3 {
                            break;
                        }
                    }
                }
                n >= 3
            }
    }

    /// # Returns
    /// `true` if a space should be inserted between the spans
    pub(super) fn should_insert_space(prev: &TextSpan, current: &TextSpan) -> bool {
        // Get font size (use the larger of the two)
        let font_size = prev.font_size.max(current.font_size).max(1.0);

        // Same-line gate. Uses the shared threshold so the assembly
        // loop's same-line decision and the space-insertion decision
        // cannot disagree about where a line ends.
        let y_diff = (prev.bbox.y - current.bbox.y).abs();
        if y_diff > Self::same_line_threshold(prev, current) {
            return false; // Different lines - no space needed
        }

        // CJK scripts (Chinese, Japanese, Korean) do not use spaces between
        // words. If both the tail of prev and the head of current are CJK characters,
        // inserting a space would produce incorrect tokenisation.
        let prev_tail = prev.text.chars().next_back();
        let curr_head = current.text.chars().next();
        let is_cjk = |c: char| {
            matches!(
                c as u32,
                0x3040..=0x309F   // Hiragana
                | 0x30A0..=0x30FF // Katakana
                | 0x3400..=0x4DBF // CJK Unified Ideographs Extension A
                | 0x4E00..=0x9FFF // CJK Unified Ideographs
                | 0xAC00..=0xD7AF // Hangul Syllables
                | 0x20000..=0x2A6DF // CJK Unified Ideographs Extension B
                | 0xFF00..=0xFFEF // Halfwidth and Fullwidth Forms
                | 0x3000..=0x303F // CJK Symbols and Punctuation
            )
        };
        if prev_tail.is_some_and(is_cjk) && curr_head.is_some_and(is_cjk) {
            return false;
        }

        // Complex Brahmic / South-East-Asian scripts (Devanagari, Bengali,
        // Tamil, Telugu, …, Thai, Khmer): an inter-glyph gap *inside* a word is
        // not a word break. These scripts render dependent vowel signs
        // (matras), conjuncts, and reordered glyphs with their own positional
        // advances, so the Latin-tuned proportional-gap test below fires inside
        // a syllable cluster (e.g. a Bengali consonant following a wide matra
        // sits ~0.7em from it). Word boundaries in conforming text are carried
        // by an explicit SPACE glyph — ISO 32000-1 §14.8.2.5 requires the
        // spacing characters that separate words to be present — so a heuristic
        // space here only double-counts a boundary the explicit space already
        // marks. Suppress it when both sides are the *same* complex script;
        // this mirrors the CJK guard above (CJK uses no inter-word space at
        // all, these scripts carry it explicitly).
        {
            use crate::text::complex_script_detector::detect_complex_script;
            let prev_script = prev_tail.and_then(|c| detect_complex_script(c as u32));
            let curr_script = curr_head.and_then(|c| detect_complex_script(c as u32));
            if let (Some(p), Some(c)) = (prev_script, curr_script) {
                if p == c {
                    return false;
                }
            }
        }

        // Emoji / pictographic → letter boundary: a wide pictographic glyph
        // (e.g. 📄) abuts the next token, so the proportional-gap test below
        // would drop the inter-token space (`📄README` instead of `📄 README`).
        // Word boundaries are reader latitude (ISO 32000-1:2008 §9.10); keep the
        // space. The alphabetic-follower requirement excludes combined ZWJ/VS
        // emoji sequences (whose next char is a selector or another pictograph).
        if prev_tail.is_some_and(crate::extractors::text::is_pictographic)
            && curr_head.is_some_and(char::is_alphabetic)
        {
            return true;
        }

        // Calculate horizontal gap
        let prev_end_x = prev.bbox.x + prev.bbox.width;
        let gap = current.bbox.x - prev_end_x;

        // CJK script ↔ non-CJK boundary: pdftotext (and the GT it produces)
        // inserts a space wherever a CJK *script* glyph (ideograph, kana, or
        // hangul) meets a Latin/digit character on the same line, regardless
        // of how tightly the two were typeset. Without this, mixed-script
        // content like "神鹰集团" + "2015" collapses into one token
        // "神鹰集团2015", which never matches GT's separate "神鹰集团"
        // "2015" tokens (issue 484, pr-136).
        //
        // IMPORTANT: this MUST exclude fullwidth ASCII variants (U+FF01..FF5E
        // — ＜＞＝＠ etc.) and CJK Symbols and Punctuation (U+3000..303F) even
        // though they are technically "CJK characters". Those are *operator*
        // glyphs that sit inline with adjacent digits and Latin in CJK
        // technical documents — pdftotext keeps "60000≤Q＜80000"
        // "20＜μ≤30" as compound tokens (issue 484, issue-336). Forcing a
        // boundary space there destroys the compound and regresses Jaccard.
        let is_cjk_script = |c: char| {
            matches!(
                c as u32,
                0x3040..=0x309F      // Hiragana
                | 0x30A0..=0x30FF    // Katakana
                | 0x3400..=0x4DBF    // CJK Unified Ideographs Extension A
                | 0x4E00..=0x9FFF    // CJK Unified Ideographs
                | 0xAC00..=0xD7AF    // Hangul Syllables
                | 0x20000..=0x2A6DF  // CJK Unified Ideographs Extension B
                | 0xFF66..=0xFF9F    // Halfwidth Katakana
            )
        };
        let crosses_cjk_boundary = match (prev_tail, curr_head) {
            (Some(p), Some(c)) => is_cjk_script(p) != is_cjk_script(c),
            _ => false,
        };
        // ASCII punctuation hugs the preceding token in every script —
        // pdftotext's GT renders "する." with no space and "神鹰，2015"
        // with no space before the comma either. Suppress the boundary
        // forced-space when the transitioning glyph IS the punctuation;
        // the space-threshold path below still handles real gaps.
        let is_clause_punct = |c: char| {
            matches!(
                c,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}'
                // Indic danda / double danda (sentence terminators that hug the
                // preceding token like a Latin full stop). The danda lives in the
                // Devanagari block but is shared by Bengali, Gurmukhi, etc., so a
                // Bengali sentence + danda would otherwise read as a script
                // transition and take a geometric space ("প্রাণী ।").
                | '\u{0964}' | '\u{0965}'
                // Arabic comma / semicolon / question mark (RTL clause punctuation).
                | '\u{060C}' | '\u{061B}' | '\u{061F}'
            )
        };
        let punct_at_boundary = curr_head.is_some_and(is_clause_punct)
            || prev_tail.is_some_and(|c| matches!(c, '(' | '[' | '{'));
        // Hangul↔digit is NOT a word boundary: a Korean numeral hugs its
        // Sino-Korean counter ("1만년" = 10,000 years, "약 1만년"). Unlike a
        // Chinese ideograph meeting a Latin year ("神鹰集团" + "2015", which
        // pdftotext splits, issue 484), Korean keeps the digit and counter as
        // one token, so forcing a space here over-segments the eojeol.
        let is_hangul = |c: char| (0xAC00..=0xD7AF).contains(&(c as u32));
        let hangul_digit_boundary = match (prev_tail, curr_head) {
            (Some(p), Some(c)) => {
                (is_hangul(p) && c.is_ascii_digit()) || (p.is_ascii_digit() && is_hangul(c))
            }
            _ => false,
        };
        if crosses_cjk_boundary
            && !punct_at_boundary
            && !hangul_digit_boundary
            && gap > -0.5
            && gap < font_size * 5.0
        {
            return true;
        }

        // Space threshold: 0.15 × font size
        // Typical space width is ~0.25em, so 0.15em catches gaps > 60% of a space.
        // This aligns with the text extractor's font-aware threshold (~50% of space width).
        let space_threshold = font_size * 0.15;

        // Insert space if gap is significant. Previously the upper bound was
        // `gap < font_size * 5.0` on the rationale that very large gaps mean
        // "column boundary, no space needed" — but downstream the caller
        // concatenates the two spans together when this returns false, so
        // "column boundary" actually rendered as `3.80%4.41%` on wide rate
        // tables (issue 487 pr-138-example.pdf). Drop the upper bound so any
        // gap above the inter-glyph threshold gets at least a single space.
        //
        // Clause punctuation hugs the preceding word in Brahmic scripts. The
        // producer leaves a wide advance after a Bengali/Devanagari syllable
        // (matra/akhand positioning), so the geometric test would float a danda
        // ("প্রাণী ।") or a comma ("रोशनी ,") off as its own token. Scope this to
        // a *complex-script* previous glyph (or an Indic danda) so the universal
        // Latin/math/form paths — where the same suppression interacts badly with
        // the forward-gap line-break heuristic — stay byte-for-byte unchanged.
        {
            use crate::text::complex_script_detector::detect_complex_script;
            let prev_is_complex = prev_tail
                .and_then(|c| detect_complex_script(c as u32))
                .is_some();
            let curr_is_indic_punct =
                curr_head.is_some_and(|c| matches!(c, '\u{0964}' | '\u{0965}'));
            if curr_head.is_some_and(is_clause_punct) && (prev_is_complex || curr_is_indic_punct) {
                return false;
            }
        }
        gap > space_threshold
    }

    /// Stacked two-line column/table-header cell detector, applied ONLY on the
    /// structure-tree (tagged-content) assembly path — never the main flow.
    ///
    /// A tagged table can draw a header cell as two stacked rows ("Comparison"
    /// over "rate"). When the structure-tree assembler linearizes the cell's
    /// spans it sees them as consecutive, horizontally OVERLAPPING (negative
    /// gap) spans whose baseline drop stayed just under `same_line_threshold`,
    /// so it treats them as one line and — because the gap is negative —
    /// `should_insert_space` returns false and they fuse ("Comparisonrate").
    /// A negative gap combined with a genuine baseline shift is two stacked
    /// tokens, never intra-word kerning (which shares a baseline), so a space
    /// is warranted.
    ///
    /// This deliberately lives OUTSIDE `should_insert_space`: the main flow
    /// (untagged PDFs, e.g. LaTeX math) already routes backtracking
    /// baseline-shifted runs — a fraction's numerator over its denominator —
    /// through dedicated newline branches before the space decision, and adding
    /// this rule there fragments equations. Scoping it to the tagged path keeps
    /// those inputs byte-identical while fixing stacked header cells.
    pub(super) fn stacked_cell_needs_space(prev: &TextSpan, current: &TextSpan) -> bool {
        let font_size = prev.font_size.max(current.font_size).max(1.0);
        let y_diff = (prev.bbox.y - current.bbox.y).abs();
        let gap = current.bbox.x - (prev.bbox.x + prev.bbox.width);
        // Under the caller's same-line band (else the caller line-breaks), a
        // real baseline shift (> 0.5 em) with horizontal overlap (negative gap)
        // is a stacked cell. Both sides must be alphanumeric word content, not
        // punctuation/symbol runs.
        gap < -0.5
            && y_diff > font_size * 0.5
            && prev
                .text
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric())
            && current
                .text
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric())
    }

    /// Detect a span whose text is `N.M` (all-digit groups around one dot) and whose
    /// bbox.width is >40% larger than char_widths imply. This pattern occurs in
    /// sailing-score / competition-table PDFs where two adjacent columns (e.g. Q8=1,
    /// F9=10) are stored as a single Tj text run "1.10" spanning both column cells.
    /// Reference ground truth tokenises them as separate words; we must split at the dot.
    pub(crate) fn is_column_spanning_decimal(span: &TextSpan) -> bool {
        let text = &span.text;
        let dot_pos = match text.find('.') {
            Some(p) if p > 0 && p < text.len() - 1 => p,
            _ => return false,
        };
        if text[dot_pos + 1..].contains('.') {
            return false;
        }
        if !text[..dot_pos].chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        if !text[dot_pos + 1..].chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        let char_count = text.chars().count();
        // Signal 1: sparse char_widths array. When the font's glyph
        // iteration produces fewer advance-width entries than there are
        // characters in the decoded string, the span was assembled from two
        // (or more) concatenated Tj runs whose widths come from different
        // points in the glyph table. This is the exact pattern issue 487
        // nougat_018 sailing-score grids hit: each score cell is emitted as
        // a single Tj like `1.10` with `char_widths=[w]` while the PDF
        // semantically means "1" followed by "10" in adjacent score
        // columns. bbox.width can still be tight here (the producer set
        // it to cover just the rendered glyph run), so the existing
        // bbox-inflation check below misses these. Catch them via the
        // sparse-cw signal directly.
        if !span.char_widths.is_empty() && span.char_widths.len() < char_count {
            return true;
        }
        let expected_width = if !span.char_widths.is_empty() {
            let cw_sum: f32 = span.char_widths.iter().sum();
            cw_sum * (char_count as f32 / span.char_widths.len() as f32)
        } else if span.font_size > 0.0 {
            // Digits are narrower than average; 0.50em per char is a safe
            // upper bound for all-digit strings (avoids the 0.60 fallback
            // producing false negatives on column-spanning sailing scores
            // when char_widths is empty, e.g. word_spans from extract_words).
            span.font_size * 0.50 * char_count as f32
        } else {
            return false;
        };
        // Use absolute gap (bbox_w - expected) rather than a ratio so that
        // 5-char spans like "12.11" (gap ≈ 1.1×fs) are caught along with
        // 4-char spans like "1.10" (gap ≈ 1.4×fs). 1.0×font_size is a safe
        // lower bound: normal text rarely has >1em of hidden whitespace.
        let gap = span.bbox.width - expected_width;
        span.font_size > 0.0 && gap > span.font_size * 1.0
    }

    /// When a CID font's glyph iteration produces fewer advance-width entries than
    /// `decode_text_to_unicode` produces unicode chars, `char_widths.len()` < char count.
    /// This indicates two concatenated text runs stored in one Tj operator (e.g. "Theorem1.7"
    /// where "Theorem" widths come from the font's glyph table and "1.7" doesn't have
    /// matching glyph entries). Return the byte offset at which to insert a space,
    /// or None if no split is appropriate.
    pub(crate) fn char_widths_boundary_split(span: &TextSpan) -> Option<usize> {
        let cw_len = span.char_widths.len();
        if cw_len == 0 {
            return None;
        }
        let char_count = span.text.chars().count();
        if cw_len >= char_count {
            return None;
        }
        // Find the byte offset of the (cw_len)-th character
        let (boundary_byte, boundary_char) = span.text.char_indices().nth(cw_len)?;
        let prev_char = span.text[..boundary_byte].chars().next_back()?;
        // Don't insert if either side is already a space
        if boundary_char == ' ' || prev_char == ' ' {
            return None;
        }
        // Non-ASCII chars at the boundary are encoding artifacts (e.g. Polish diacritics
        // in Latin-2 / CP1250 fonts producing one fewer char_width entry). Only split
        // when the boundary char is ASCII, indicating a genuine text-run concatenation.
        if !boundary_char.is_ascii() {
            return None;
        }
        // Split at letter→digit boundary (e.g. "Theorem1.7") or lower→upper ASCII
        // case boundary (e.g. "BigText" from concatenated CID runs "Big"+"Text").
        // Upper→lower transitions are excluded: a ligature spanning an upper→lower
        // boundary within a compound word (e.g. "officeMax" with "fl" ligature)
        // would otherwise produce a false split.
        if (prev_char.is_alphabetic() && boundary_char.is_ascii_digit())
            || (prev_char.is_ascii_lowercase() && boundary_char.is_ascii_uppercase())
        {
            Some(boundary_byte)
        } else {
            None
        }
    }
}
