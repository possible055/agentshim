use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Merge subscript and superscript spans into their base span.
    ///
    /// In math-heavy untagged PDFs, subscript glyphs (e.g. the "1" in "k₁") are
    /// stored as separate `TextSpan` entries at a slightly lower/higher baseline than
    /// the base character, and non-adjacent in reading order. The text assembly loop
    /// emits them as isolated tokens ("k … 1") rather than the expected word ("k1").
    ///
    /// A span is classified as a subscript/superscript when ALL of the following hold:
    ///  - 1–3 ASCII alphanumeric chars (digit or letter, no punctuation)
    ///  - font_size < 85 % of the page's maximum font size
    ///  - There exists a preceding "base" span whose right edge (x + width) is within
    ///    ±0.6 × sub_fs of the subscript's left edge (x-adjacent)
    ///  - The vertical offset between base and sub is in [8 %, 85 %] of base_fs
    ///    (distinguishes true sub/superscripts from same-line small caps)
    ///
    /// Matched subscript/superscript spans have their text appended to the base
    /// are removed from `spans`.
    pub(super) fn merge_sub_superscript_spans(spans: &mut Vec<TextSpan>) {
        let n = spans.len();
        if n < 2 {
            return;
        }
        let max_fs = spans.iter().map(|s| s.font_size).fold(0f32, f32::max);
        if max_fs <= 0.0 {
            return;
        }

        // Item 5b (M4): an INDEX CLUSTER is a comma-joined run of digits that the
        // producer set as a single subscript/superscript — an F-statistic's
        // degrees of freedom (`4,176` in `F4,176`) or a multi-affiliation marker
        // (`1,2`). These exceed the 3-char limit and contain a comma, so the plain
        // sub-char gate rejected them, stranding `F`, `4`, `176` as separate
        // tokens. Recognised here so the comma cluster merges back into its base.
        let is_index_cluster = |t: &str| -> bool {
            t.chars().count() >= 3
                && t.contains(',')
                && t.chars().all(|c| c.is_ascii_digit() || c == ',')
                && !t.starts_with(',')
                && !t.ends_with(',')
        };

        // For each candidate sub/superscript span, record which base span to merge into.
        let mut to_merge: Vec<(usize, usize)> = Vec::new(); // (base_idx, sub_idx)
        let mut already_sub: std::collections::HashSet<usize> = std::collections::HashSet::new();

        for i in 0..n {
            let sub = &spans[i];
            // Char-count gate (not byte-count): U+00B2/B3/B9 are 2-byte
            // UTF-8 sequences and U+2070..U+209F are 3-byte, so the
            // earlier byte-length check would have dropped a legitimate
            // 3-digit Unicode subscript like "₁₂₃" (9 bytes).
            if sub.text.is_empty() || (sub.text.chars().count() > 3 && !is_index_cluster(&sub.text))
            {
                continue;
            }
            // Accept the raw ASCII the extractor produces AND the
            // already-substituted Unicode super/subscript codepoints
            // (apply_super_sub_script_substitutions runs upstream).
            // Without the U+00B2/B3/B9 + U+2070..U+209F gate, a
            // chemistry formula like "H₂O" would lose the subscript
            // span from this merge, leaving "H ₂ O" in the output.
            let is_sub_char = |c: char| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '\u{00B2}' | '\u{00B3}' | '\u{00B9}')
                    || ('\u{2070}'..='\u{209F}').contains(&c)
            };
            // M4 (item 5c): a span the producer explicitly raised/lowered with the
            // Text Rise operator (ISO 32000-1 §9.3.7 `Ts`) is an authoritative
            // sub/superscript even when it is NOT shrunk and is not in the ASCII /
            // Unicode sub-glyph set (e.g. a math operator superscript). `text_rise`
            // is stored as the Ts/font-size ratio, so |ratio| ≥ 0.10 marks a real
            // shift. Such a span bypasses the charset and font-size gates below; the
            // x/y proximity gates in the base search still apply, so a genuinely
            // detached different-row marker is not over-merged.
            let ts_flagged = sub.text_rise.abs() >= 0.10;
            if !ts_flagged && !sub.text.chars().all(is_sub_char) && !is_index_cluster(&sub.text) {
                continue;
            }
            // Must be clearly smaller than the dominant font on this page (unless
            // the producer flagged it via Ts).
            if !ts_flagged && sub.font_size >= max_fs * 0.80 {
                continue;
            }
            let sub_fs = sub.font_size;
            let sub_x = sub.bbox.x;
            let sub_y = sub.bbox.y;

            // A purely NUMERIC sub-run (digits, optionally comma-joined) at a
            // base's advance edge is an inline super/subscript even when its
            // bbox shares the base's baseline. Some producers raise a glyph with
            // a small font but emit it on the SAME text-line baseline (the
            // visual rise lives in the glyph's own bbox, not the line's), so the
            // extractor records y_diff_abs ≈ 0. The 12 %-of-em vertical lower
            // bound (which screens out same-line small caps) would then strand
            // the marker — e.g. the isotope label `123` in `[123I]FP-CIT`, or an
            // author-affiliation marker `1,2`. Small caps are never bare digits,
            // so dropping the lower bound for numeric subs is safe; the smaller-
            // font, x-edge, valid-base, and upper-y gates still apply.
            let sub_is_numeric =
                sub.text.chars().all(|c| c.is_ascii_digit() || c == ',') && !ts_flagged;

            // Search backwards for the best-matching base span.
            let search_limit = 30.min(i);
            let mut best: Option<(usize, f32)> = None; // (idx, |x_dist|)

            for j in (i.saturating_sub(search_limit)..i).rev() {
                if already_sub.contains(&j) {
                    continue;
                }
                let base = &spans[j];
                // Base must be at least 25 % larger than the sub (sub_fs ≤ 0.80×base_fs),
                // UNLESS the producer flagged the sub via Ts (then it may be the same
                // size as its base — the rise itself, not the size, marks it).
                if !ts_flagged && base.font_size < sub_fs * 1.25 {
                    continue;
                }
                // Base span must be a valid subscript host:
                //   • 1-char bases (single math variable: k, γ, ρ, H, ∆, …)
                //   • 2-char bases that are NOT two lowercase-ASCII letters
                //     (accepts "Pr", "εp", "ρε" but rejects "of", "to")
                //   • longer bases ENDING in an acronym — a run of ≥2 trailing
                //     uppercase ASCII letters (e.g. a wide body span
                //     "…activation of VPAC", or "CA1"'s "CA"). Receptor/region
                //     names (VPAC, CA, PAC, GABA, NMDA, …) carry a subscript on
                //     their trailing acronym, but the producer emits the whole
                //     wrapped line as one span, so the ≤2-char gate stranded the
                //     subscript and the row-band sort glued it onto a later word
                //     ("…of VPAC receptors … pyramidal1"). The subscript text is
                //     appended to the base's END, which is exactly the acronym, so
                //     it reconstructs "VPAC1". The x-edge gate below still requires
                //     the sub to sit at the base's advance edge, and the trailing
                //     run being UPPERCASE keeps ordinary prose (which ends in a
                //     lowercase letter or punctuation) from ever matching.
                // Multi-char lowercase-only strings like "and", "let", "sup"
                // are English words or common operators; their adjacent digit
                // spans are handled by the assembly loop and char_widths_boundary_split.
                let chars: Vec<char> = base.text.chars().collect();
                let ends_in_acronym = || {
                    let trailing_upper = chars
                        .iter()
                        .rev()
                        .take_while(|c| c.is_ascii_uppercase())
                        .count();
                    trailing_upper >= 2
                };
                let is_valid_base = match chars.len() {
                    1 => true,
                    2 => chars.iter().any(|c| !c.is_ascii_lowercase()),
                    _ => ends_in_acronym(),
                };
                if !is_valid_base {
                    continue;
                }
                let base_right = base.bbox.x + base.bbox.width;
                let x_dist = sub_x - base_right;
                let y_diff_abs = (base.bbox.y - sub_y).abs();

                // Use em-relative x_dist thresholds.
                // Real sub/superscript glyphs land within ±[−0.1×base_fs, 0.25×base_fs]
                // of the base's advance edge; absolute bounds were wrong for non-12pt fonts.
                let base_fs = base.font_size.max(1.0);
                let x_lo = -0.1 * base_fs;
                let x_hi = 0.25 * base_fs;
                if x_dist < x_lo || x_dist > x_hi {
                    continue;
                }
                // Vertical offset must be in the sub/superscript range.
                // Lower bound 12 % of base_fs ensures same-line small caps are excluded.
                // Upper bound 75 % excludes large line-to-line y differences (e.g.
                // author affiliation numbers on a different baseline row).
                // Numeric subs (digits/commas) may sit on the base baseline, so
                // skip the small-caps lower bound for them; all other subs keep it.
                let y_lo = if sub_is_numeric {
                    0.0
                } else {
                    base.font_size * 0.12
                };
                if y_diff_abs < y_lo || y_diff_abs > base.font_size * 0.75 {
                    continue;
                }
                let score = x_dist.abs();
                if best.is_none() || score < best.unwrap().1 {
                    best = Some((j, score));
                }
            }

            if let Some((base_idx, _)) = best {
                to_merge.push((base_idx, i));
                already_sub.insert(i);
            }
        }

        if to_merge.is_empty() {
            return;
        }

        // Collect (base_idx, sub_idx, sub_text, sub_right_edge, sub_char_widths, sub_fs)
        // before mutating spans.
        let ops: Vec<(usize, usize, String, f32, Vec<f32>, f32)> = to_merge
            .iter()
            .map(|pair| {
                let (bi, si) = *pair;
                let sub = &spans[si];
                (
                    bi,
                    si,
                    sub.text.clone(),
                    sub.bbox.x + sub.bbox.width,
                    sub.char_widths.clone(),
                    sub.font_size,
                )
            })
            .collect();

        // Apply: append sub text to base; extend bbox and char_widths to cover the sub.
        //
        // Extending bbox: the assembly loop uses span widths for gap calculations — keeping
        // the original width would make the gap to the following span appear too large.
        //
        // Extending char_widths: char_widths_boundary_split fires whenever cw_len < char_count.
        // After merging sub text, char_count grows but cw_len stays the same, which would
        // cause the split to re-separate the merged token (e.g. "k1" → "k 1"). Adding
        // estimated widths for the sub characters prevents this.
        for (base_idx, _, sub_text, sub_right, sub_cw, sub_fs) in &ops {
            let base = &mut spans[*base_idx];
            base.text.push_str(sub_text);
            let base_right = base.bbox.x + base.bbox.width;
            if *sub_right > base_right {
                base.bbox.width = sub_right - base.bbox.x;
            }
            if !base.char_widths.is_empty() {
                let sub_char_count = sub_text.chars().count();
                if !sub_cw.is_empty() {
                    base.char_widths.extend_from_slice(sub_cw);
                } else {
                    // Estimate sub char widths at 0.50 em per character.
                    let w = sub_fs * 0.50;
                    for _ in 0..sub_char_count {
                        base.char_widths.push(w);
                    }
                }
            }
        }

        // Drop the merged sub/superscript spans in one pass.
        let to_remove: std::collections::HashSet<usize> =
            ops.iter().map(|(_, si, _, _, _, _)| *si).collect();
        let mut idx = 0usize;
        spans.retain(|_| {
            let keep = !to_remove.contains(&idx);
            idx += 1;
            keep
        });
    }

    /// Append span text to `out`, splitting merged runs for cleaner word tokenisation.
    /// Priority 0: spans whose text is entirely `\n`/`\r` are line-break signals.
    /// Priority 1: column-spanning decimal (nougat_018 sailing tables).
    /// Priority 2: char_widths boundary split (pdfa_004 CID-font merge artifacts).
    #[inline]
    pub(crate) fn push_span_text(out: &mut String, span: &TextSpan) {
        // A span whose entire text is one or more newline/CR characters is a
        // ToUnicode line-break signal. Treat it as a logical newline separator rather
        // than emitting the raw control characters verbatim as visible content.
        if !span.text.is_empty() && span.text.chars().all(|c| c == '\n' || c == '\r') {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            return;
        }
        if Self::is_column_spanning_decimal(span) {
            let dot = span.text.find('.').unwrap();
            out.push_str(&span.text[..dot]);
            out.push(' ');
            out.push_str(&span.text[dot + 1..]);
        } else if let Some(split) = Self::char_widths_boundary_split(span) {
            out.push_str(&span.text[..split]);
            out.push(' ');
            out.push_str(&span.text[split..]);
        } else {
            out.push_str(&span.text);
        }
    }

    /// #557a: append a span's text to the structure-tree assembly, reversing a
    /// PURE-RTL run (every non-space char is an Arabic/Hebrew letter, no Latin)
    /// from visual to logical order. The tagged/struct-tree path collapses each
    /// run to a single span and never reaches `reverse_rtl_visual_order_runs`,
    /// so visually-stored RTL (e.g. issue10301 Hebrew "גבא") otherwise leaked
    /// out reversed. A single-direction run's logical order is just its reverse,
    /// so no glyph geometry is needed for the pure-RTL case.
    pub(super) fn push_span_text_bidi(out: &mut String, span: &TextSpan, rtl_run: bool) {
        use crate::text::rtl_detector::is_rtl_text;
        // A span whose glyphs were drawn right-to-left (logical storage — the
        // producer positioned each glyph individually at decreasing x, ISO
        // 32000-1 §14.8.2.3.3 method 1; detected by `detect_rtl_draw_direction`)
        // already carries its CHARACTERS in LOGICAL order. The visual→logical
        // character reversal below assumes VISUAL storage and would corrupt
        // them, so emit the text verbatim — the letters are already correct.
        // (Word ORDER within such a span is left as the reading-order sort
        // produced it: a producer may emit words logically or visually within
        // the same document, so a blanket word reversal would corrupt the
        // logical ones.) Visual storage — the default — is never flagged and
        // keeps the character-reversal below, so it stays byte-identical.
        if span.rtl_draw_logical {
            Self::push_span_text(out, span);
            return;
        }
        let mut rtl = 0usize;
        let mut has_latin = false;
        for c in span.text.chars() {
            if c.is_whitespace() {
                continue;
            }
            if c.is_ascii_alphabetic() {
                has_latin = true;
                break;
            }
            if is_rtl_text(c as u32) {
                rtl += 1;
            }
        }
        if rtl >= 2 && !has_latin {
            let mut tmp = span.clone();
            // Strip producer-inserted SPACEs that fall *between two Arabic
            // letters* inside a single show string. ISO 32000-1 §14.8.2.3.3
            // states a reverse-order show string "shall not contain interior
            // SPACEs" — a word break is signalled by a SPACE at the string
            // boundary (here, a separate span), never inside it. Arabic is
            // cursive, so an interior space splits letters that the script
            // joins; it is never a word boundary in the pure-text
            // representation (§14.8.2.5). Restricted to Arabic (cursive): a
            // non-cursive script such as Hebrew can legitimately carry a
            // space-separated pair in one show string, so it is left alone.
            tmp.text =
                Self::reverse_rtl_keeping_marks(&Self::strip_interior_arabic_spaces(&span.text))
                    .replace(Self::RTL_WORD_BOUNDARY, " ");
            Self::push_span_text(out, &tmp);
        } else if rtl_run && Self::is_reversible_rtl_neutral_span(&span.text) {
            // A neutral-only span (separator / terminator punctuation plus
            // spaces — no strong letters and no digits) embedded in a pure-RTL
            // run carries its glyphs in *visual* (content-stream draw) order.
            // Per UAX #9 the neutrals inherit the surrounding right-to-left
            // direction (rules N1/N2), so their logical order is the reverse of
            // the visual sequence: a visual "<space><comma>" drawn between two
            // Hebrew words becomes "<comma><space>", re-attaching the comma to
            // the preceding word. The pure-RTL words around it are reversed by
            // the branch above; without this the punctuation stayed stranded on
            // the wrong side of the inter-word space.
            let mut tmp = span.clone();
            tmp.text = span.text.chars().rev().collect();
            Self::push_span_text(out, &tmp);
        } else if rtl_run && Self::is_reversible_rtl_numeric_span(&span.text) {
            // A neutral+numeric span (e.g. a Hebrew-context " ,2009-" or " 600-")
            // embedded in a pure-RTL run carries its glyphs in *visual*
            // (content-stream draw) order. Reverse it to logical order while
            // keeping each digit run forward (UAX #9 rule L2): visual " ,2009-"
            // → logical "-2009, ", re-attaching the hyphen to the number and the
            // comma to the preceding word, without ever flipping 2009 → 9002.
            let mut tmp = span.clone();
            tmp.text = crate::text::bidi::reverse_rtl_keep_numbers(&span.text);
            Self::push_span_text(out, &tmp);
        } else {
            Self::push_span_text(out, span);
        }
    }

    /// Whether `text` is a neutral+numeric span eligible for number-preserving
    /// RTL visual→logical reversal in [`push_span_text_bidi`]: every non-space
    /// char is a [reorderable neutral](Self::is_rtl_reorderable_neutral), an
    /// ASCII hyphen-minus, or a digit (ASCII / Arabic-Indic U+0660–0669 /
    /// Extended Arabic-Indic U+06F0–06F9); it contains **exactly one** maximal
    /// digit run (so a date range `2009-2010` or an ORCID is never reversed),
    /// at least one movable neutral/hyphen, and the number-preserving reversal
    /// actually changes it (else the cheaper verbatim path is byte-identical).
    pub(super) fn is_reversible_rtl_numeric_span(text: &str) -> bool {
        let is_digit = |c: char| {
            c.is_ascii_digit()
                || ('\u{0660}'..='\u{0669}').contains(&c)
                || ('\u{06F0}'..='\u{06F9}').contains(&c)
        };
        let mut has_movable = false;
        let mut digit_runs = 0usize;
        let mut in_digit = false;
        for c in text.chars() {
            if is_digit(c) {
                if !in_digit {
                    digit_runs += 1;
                    in_digit = true;
                }
                continue;
            }
            in_digit = false;
            if c.is_whitespace() {
                continue;
            }
            if c == '-' || Self::is_rtl_reorderable_neutral(c) {
                has_movable = true;
                continue;
            }
            return false; // strong letter, bracket, quote, etc. → not eligible
        }
        digit_runs == 1 && has_movable && crate::text::bidi::reverse_rtl_keep_numbers(text) != text
    }

    /// Remove ASCII SPACE (U+0020) characters that sit *between two Arabic
    /// letters* within a single show string — producer-inserted spurious
    /// spaces that split a cursive word (e.g. `قِ ل` inside `القِطّ`).
    ///
    /// Per ISO 32000-1 §14.8.2.3.3 a show string "shall not contain interior
    /// SPACEs"; a genuine word break is a SPACE at a string boundary (a
    /// separate span in this pipeline). Combining marks between the space and
    /// its neighbouring base letter are seen through, so a mark sitting next to
    /// the space does not hide the Arabic letter on that side. Leading and
    /// trailing spaces (real word-break candidates) and spaces flanked by
    /// anything other than two Arabic letters are preserved verbatim, so the
    /// fast path returns the input unchanged when there is nothing to strip.
    pub(super) fn strip_interior_arabic_spaces(text: &str) -> String {
        use crate::text::rtl_detector::{is_arabic_letter, is_rtl_diacritic};
        if !text.contains(' ') {
            return text.to_string();
        }
        // First non-mark char in `it` is an Arabic letter? (marks are seen
        // through so a diacritic next to the space does not hide its base.)
        fn arabic_letter_past_marks<'a>(it: impl Iterator<Item = &'a char>) -> bool {
            for &c in it {
                if is_rtl_diacritic(c as u32) {
                    continue;
                }
                return is_arabic_letter(c as u32);
            }
            false
        }
        let chars: Vec<char> = text.chars().collect();
        // Interior spaces flanked by Arabic letters on both sides.
        let qualifying: Vec<usize> = (0..chars.len())
            .filter(|&i| {
                chars[i] == ' '
                    && arabic_letter_past_marks(chars[..i].iter().rev())
                    && arabic_letter_past_marks(chars[i + 1..].iter())
            })
            .collect();
        if qualifying.is_empty() {
            return text.to_string();
        }
        // SHATTER case (§14.8.2.3.3: a show-string must not contain interior
        // SPACEs). When a space sits between a MAJORITY of adjacent Arabic-letter
        // pairs, the producer exploded one cursive word into separate glyphs
        // (e.g. `فصيلة` drawn as `ة لي ص ف`); every interior space is spurious, so
        // strip them all. The density test (qualifying ≥ half the inter-letter
        // gaps) tells this apart from ordinary multi-word text, whose spaces are
        // sparse real word breaks (the right_to_left_01 class) — those stay.
        let arabic_letters = chars
            .iter()
            .filter(|&&c| is_arabic_letter(c as u32))
            .count();
        let gaps = arabic_letters.saturating_sub(1).max(1);
        if qualifying.len() >= 2 && qualifying.len() * 2 >= gaps {
            let drop: std::collections::HashSet<usize> = qualifying.iter().copied().collect();
            return chars
                .iter()
                .enumerate()
                .filter_map(|(i, &c)| (!drop.contains(&i)).then_some(c))
                .collect();
        }
        // Sparse case: a lone spurious cursive-join space. A span with several
        // sparse Arabic-flanked spaces is ordinary multi-word text whose spaces
        // are real word breaks — leave them intact.
        if qualifying.len() != 1 {
            return text.to_string();
        }
        let drop = qualifying[0];
        // Joining-type discriminator (§14.8.2.3.3). The cursive join already
        // breaks AFTER a right-joining-only letter (ا د ذ ر ز و …), so a space
        // there renders identically whether it is a genuine word break or a
        // producer artefact — the two are indistinguishable. Stripping it would
        // risk concatenating two real words (`دار اب` → `داراب`). Only a space
        // after a dual-joining letter unambiguously broke a join that should
        // not break, so restrict the strip to that case and keep the space when
        // the preceding base letter (seen past any combining marks) is
        // right-joining.
        let preceding_right_joining = chars[..drop]
            .iter()
            .rev()
            .find(|&&c| !is_rtl_diacritic(c as u32))
            .is_some_and(|&c| crate::text::rtl_detector::is_right_joining_arabic(c as u32));
        if preceding_right_joining {
            return text.to_string();
        }
        chars
            .iter()
            .enumerate()
            .filter_map(|(i, &c)| (i != drop).then_some(c))
            .collect()
    }

    /// Emit the inter-line newline(s) between two vertically separated spans in
    /// the struct-order assembler. A normal line gap maps to one to three
    /// newlines proportional to the vertical distance (`y_diff / line_height`,
    /// clamped) so multi-line paragraph spacing survives. When `single_break`
    /// is set — two consecutive cells of a tagged table on different rows — a
    /// single newline is emitted instead: table rows are stacked block rows,
    /// not free-leading paragraphs (ISO 32000-1 §14.8.4.3.4), and the geometric
    /// row pitch (~1.7× leading) would otherwise insert a spurious blank line
    /// between every row.
    pub(super) fn push_line_breaks(
        text: &mut String,
        prev: &TextSpan,
        span: &TextSpan,
        y_diff: f32,
        single_break: bool,
    ) {
        if single_break {
            text.push('\n');
            return;
        }
        let font_size = prev.font_size.max(span.font_size).max(10.0);
        let line_height = font_size * 1.2;
        let num_breaks = (y_diff / line_height).round() as usize;
        for _ in 0..num_breaks.clamp(1, 3) {
            text.push('\n');
        }
    }

    /// Whether every span in this marked-content element is part of a *pure*
    /// right-to-left run: at least one Arabic/Hebrew letter is present and no
    /// Latin letter is. Mirrors the gating in [`order_mcid_spans`] (the branch
    /// that sorts pure-RTL spans right-to-left). Used to decide whether
    /// neutral-only punctuation spans inside the run must be reversed from
    /// visual to logical order by [`push_span_text_bidi`].
    pub(super) fn mcid_run_is_pure_rtl(spans: &[crate::layout::TextSpan]) -> bool {
        use crate::text::rtl_detector::is_rtl_text;
        let has_rtl = spans
            .iter()
            .any(|s| s.text.chars().any(|c| is_rtl_text(c as u32)));
        let has_latin = spans
            .iter()
            .any(|s| s.text.chars().any(|c| c.is_ascii_alphabetic()));
        has_rtl && !has_latin
    }

    /// Is `c` a direction-neutral punctuation mark whose order inside an RTL
    /// run is a pure transposition — safe to reverse with the surrounding RTL
    /// neutrals? Restricted to separators and terminators (comma, full stop,
    /// semicolon, colon, exclamation, question, and their Arabic/Hebrew
    /// equivalents). Deliberately excludes paired brackets and quotation marks
    /// (which need UAX #9 L4 mirroring, handled elsewhere), digits, and any
    /// character that anchors an embedded left-to-right sub-run.
    pub(super) fn is_rtl_reorderable_neutral(c: char) -> bool {
        matches!(
            c,
            ',' | '.' | ';' | ':' | '!' | '?'
                | '\u{05BE}' // Hebrew maqaf
                | '\u{05C3}' // Hebrew sof pasuq
                | '\u{060C}' // Arabic comma
                | '\u{061B}' // Arabic semicolon
                | '\u{061F}' // Arabic question mark
                | '\u{06D4}' // Arabic full stop
        )
    }

    /// Whether `text` is a neutral-only span eligible for the RTL visual→logical
    /// reversal in [`push_span_text_bidi`]: every character is whitespace or a
    /// [reorderable neutral](Self::is_rtl_reorderable_neutral), it contains at
    /// least one such punctuation mark, and it is at least two characters long
    /// (so there is an order to fix). A lone punctuation glyph or a bare space
    /// run reverses to itself and is left untouched.
    pub(super) fn is_reversible_rtl_neutral_span(text: &str) -> bool {
        let mut has_punct = false;
        let mut count = 0usize;
        for c in text.chars() {
            count += 1;
            if c.is_whitespace() {
                continue;
            }
            if Self::is_rtl_reorderable_neutral(c) {
                has_punct = true;
                continue;
            }
            return false; // letter, digit, bracket, quote, or other → not eligible
        }
        has_punct && count >= 2
    }

    /// Reverse a pure-RTL run from visual to logical order while keeping each
    /// Arabic/Hebrew combining mark attached to its base letter.
    ///
    /// A naive `chars().rev()` reverses by Unicode scalar value, so a base
    /// letter's diacritics (which follow it in logical order — kasra/shadda
    /// U+0650/U+0651, Hebrew points U+05B0..) jump *in front* of the base and
    /// float off as standalone marks. Grouping each base char with the
    /// combining marks that trail it, then reversing the group order (each
    /// group's internal order preserved), keeps marks bound to their base.
    pub(super) fn reverse_rtl_keeping_marks(text: &str) -> String {
        use crate::text::rtl_detector::is_rtl_diacritic;
        let mut groups: Vec<Vec<char>> = Vec::new();
        for c in text.chars() {
            if is_rtl_diacritic(c as u32) && !groups.is_empty() {
                groups.last_mut().unwrap().push(c);
            } else {
                groups.push(vec![c]);
            }
        }
        groups.iter().rev().flatten().collect()
    }

    /// Parse font size from a /DA (Default Appearance) string.
    ///
    /// DA strings follow the format: `"/FontName size Tf ..."` (e.g., `"/Helv 12 Tf 0 g"`).
    /// Returns the font size preceding the `Tf` operator, or a default of 10.0 if not found.
    pub(super) fn parse_font_size_from_da(da: &str) -> f32 {
        let tokens: Vec<&str> = da.split_whitespace().collect();
        for i in 0..tokens.len() {
            if tokens[i] == "Tf" && i > 0 {
                if let Ok(size) = tokens[i - 1].parse::<f32>() {
                    if size > 0.0 {
                        return size;
                    }
                }
            }
        }
        10.0 // default
    }
}
