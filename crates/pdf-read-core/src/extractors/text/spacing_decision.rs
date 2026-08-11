use super::*;

pub(super) fn should_insert_space(
    preceding_text: &str,
    following_text: &str,
    gap_pt: f32,
    font_size: f32,
    font_name: &str,
    fonts: &std::collections::HashMap<String, std::sync::Arc<crate::fonts::FontInfo>>,
    tj_offset_triggered: bool,
    config: &SpanMergingConfig,
    prev_bbox: Option<&crate::geometry::Rect>,
    next_bbox: Option<&crate::geometry::Rect>,
    prev_font_size: f32,
    next_font_size: f32,
) -> SpaceDecision {
    // PHASE 10: PDF Spec-Compliant Space Detection
    // Per ISO 32000-1:2008 Section 9.4.3 and 9.4.4
    //
    // Text positioning is determined by the text matrix and glyph positioning.
    // Only spec-defined signals are used; linguistic heuristics are excluded.
    //
    // Allowed signals (from PDF Spec):
    // 1. Boundary whitespace: spaces already present in text strings
    // 2. TJ array offsets: negative offsets < -100 thousandths of em
    // 3. Geometric gaps: gaps between character bounding boxes vs font metrics

    // Rule 0: Boundary Space (Section 9.4.3 - Text Showing)
    // Spaces already present in text strings should not be duplicated
    if has_boundary_space(preceding_text, following_text) {
        return SpaceDecision::no_space(SpaceSource::AlreadyPresent, 1.0);
    }

    // Rule 0.3: Complex-script combining-mark guard (#656-class Indic gap).
    // A Brahmic/Thai/Khmer dependent vowel sign, virama, or tone mark followed
    // by another character of a complex script is intra-word — the mark carries
    // its own advance, so the geometric gap and consensus paths below would
    // otherwise emit a spurious word space (the dominant matra→consonant error
    // for Tamil/Bengali/Devanagari). Genuine word breaks carry an explicit
    // space glyph, already handled by Rule 0. This guards the strong-geometric
    // and consensus branches, which never consult `WordBoundaryDetector`.
    if let (Some(pc), Some(nc)) = (
        preceding_text.chars().next_back(),
        following_text.chars().next(),
    ) {
        use crate::text::complex_script_detector::{detect_complex_script, is_complex_script_mark};
        if is_complex_script_mark(pc as u32) && detect_complex_script(nc as u32).is_some() {
            return SpaceDecision::no_space(SpaceSource::NoSpace, 0.9);
        }
    }

    // Rule 0.4: Emoji / pictographic → letter boundary.
    // A wide pictographic glyph (e.g. 📄) advances far, so the residual gap to
    // the next token falls below the proportional-font space threshold and the
    // inter-token space would otherwise be dropped (`📄README` instead of
    // `📄 README`). In practice the emoji glyph's right edge abuts the next
    // token (gap ≈ 0). Word boundaries are reader latitude (§9.10), so when an
    // emoji is immediately followed by a letter, keep the space. The
    // `is_alphabetic` requirement on the following char already excludes combined
    // ZWJ/VS emoji sequences (whose next char is a selector or another pictograph,
    // never a letter), so a non-negative gap is the correct gate.
    if gap_pt >= 0.0
        && preceding_text
            .chars()
            .next_back()
            .is_some_and(is_pictographic)
        && following_text
            .chars()
            .next()
            .is_some_and(char::is_alphabetic)
    {
        return SpaceDecision::insert(SpaceSource::GeometricGap, 0.85);
    }

    // Rule 0.5: Email Pattern Detection
    // Per ISO 32000-1:2008 Section 9.10, email formatting preservation
    if config.detect_email_patterns && is_email_context(preceding_text, following_text) {
        let geometric_threshold = if let Some(font_info) = fonts.get(font_name) {
            let space_width_units = font_info.get_space_glyph_width();
            let space_width_pt = (space_width_units / 1000.0) * font_size;
            let word_margin_ratio = 0.5;
            space_width_pt * word_margin_ratio
        } else {
            font_size * 0.25
        };

        let email_threshold = geometric_threshold * config.email_threshold_multiplier;

        if gap_pt > email_threshold {
            log::debug!(
                "Email context detected: gap={:.2}pt > {:.2}pt email threshold - inserting space",
                gap_pt,
                email_threshold
            );
            return SpaceDecision::insert(SpaceSource::GeometricGap, 0.85);
        }

        log::debug!(
            "Email context detected: gap={:.2}pt <= {:.2}pt email threshold - suppressing space",
            gap_pt,
            email_threshold
        );
        return SpaceDecision::no_space(SpaceSource::NoSpace, 1.0);
    }

    // Line Break Handling
    // ==============================================================================
    // Per ISO 32000-1:2008 Section 5.2 (geometric positioning):
    // Line breaks are detected using bbox Y-coordinates (vertical positioning).
    // Words split across lines need special handling:
    // - Soft hyphen breaks: Previous text ends with '-' → NO space (word continuation)
    // - Hard line breaks: Normal breaks → INSERT space (new word on next line)
    //
    // Spec Reference: Section 5.2 states coordinates are in user space units.
    // Font size is used as reference for vertical gap detection threshold.

    if let (Some(prev_box), Some(next_box)) = (prev_bbox, next_bbox) {
        // Calculate vertical positioning for line break detection.
        // Use Y-coordinate difference (not bottom-to-top gap) to detect actual line breaks.
        // Two spans on the same line have nearly identical Y positions regardless of height.
        let y_diff = (prev_box.y - next_box.y).abs();

        // Line break threshold: if Y positions differ by more than 0.5× font size
        let line_break_threshold = font_size * 0.5;
        let is_line_break = y_diff > line_break_threshold;

        if is_line_break {
            // Verify same-column layout: X-positions within 2× font width
            let same_column = (prev_box.left() - next_box.left()).abs() < (font_size * 2.0);

            if same_column {
                log::debug!(
                    "Detected line break: y_diff={:.2}pt > {:.2}pt threshold, same_column=true",
                    y_diff,
                    line_break_threshold
                );

                // Check if previous text ends with hyphen (soft line break)
                if preceding_text.ends_with('-') {
                    log::debug!(
                        "Soft hyphen detected: '{}' ends with '-', suppressing space insertion",
                        preceding_text
                    );
                    return SpaceDecision::no_space(SpaceSource::SoftHyphen, 1.0);
                } else {
                    log::debug!("Hard line break detected: inserting space for word continuation");
                    return SpaceDecision::insert(SpaceSource::GeometricGap, 0.9);
                }
            }
        }
    }

    // NEW: Rule 1.5: Citation Marker Detection
    // ==============================================================================
    // Per ISO 32000-1:2008 Section 9.3, citation markers have distinct visual properties
    if config.detect_citation_markers
        && is_citation_context(
            prev_bbox,
            next_bbox,
            font_size,
            prev_font_size,
            next_font_size,
        )
    {
        // For citations, use single-signal detection (don't require consensus)
        // Compute geometric threshold for citation context
        let citation_geometric_threshold = if let Some(font_info) = fonts.get(font_name) {
            let space_width_units = font_info.get_space_glyph_width();
            let space_width_pt = (space_width_units / 1000.0) * font_size;
            space_width_pt * 0.5
        } else {
            font_size * 0.25
        };

        if tj_offset_triggered || gap_pt > citation_geometric_threshold {
            log::debug!(
                "Citation context detected: using relaxed spacing rules (gap={:.2}pt, tj={})",
                gap_pt,
                tj_offset_triggered
            );
            return SpaceDecision::insert(SpaceSource::TjOffset, 0.90);
        }
    }

    // Consensus-Based Spacing Logic
    // ==============================================================================
    // Per ISO 32000-1:2008 Section 9.4.4 and 9.10:
    // "Determining word boundaries is not specified by PDF."
    // TJ offsets are typographic hints only, not definitive word boundaries.
    //
    // Solution: Require CONSENSUS between multiple PDF-spec-defined signals:
    // - TJ offset signal (explicit typography positioning)
    // - Geometric signal (bounding box analysis)
    // - Strong geometric signal alone is sufficient (gap > 2× threshold)

    // Rule 1: TJ Offset Signal (Section 9.4.3) - PDF-spec explicit signal
    // Calculate font-aware geometric threshold for consensus checking
    let geometric_threshold = if let Some(font_info) = fonts.get(font_name) {
        // Font found: use space glyph width for calculation
        let space_width_units = font_info.get_space_glyph_width(); // in 1000ths of em
        let space_width_pt = (space_width_units / 1000.0) * font_size;
        // monospace fonts emit one show-text
        // op per glyph at one-em-advance positioning, so the gap
        // between glyphs in normal tokens briefly exceeds the
        // proportional-font threshold. Use a 1.2× ratio for monospace
        // so spurious spaces around punctuation in code listings
        // (`function add (a , b )` → `function add(a, b)`) don't fire.
        let mut word_margin_ratio = if is_monospace_font(font_name) {
            1.2
        } else {
            0.5 // 50% of space width (proportional default)
        };
        // when prev_font_size
        // next_font_size differ significantly, we're at a font-run
        // boundary (italic → roman, bold → regular, or a font-family
        // switch). PdfTeX-typeset titles like
        // `Astronomy & Astrophysicsmanuscript no.` exhibit this when
        // the writer doesn't emit an explicit space-glyph at the font
        // switch. Reduce the threshold by 30% at boundaries so a
        // smaller gap suffices to trigger space insertion. The full
        // fix (font-name plumbing for italic→roman within same size)
        // is tracked in — many italic transitions
        // share font_size, so this only catches the size-changing
        // subset.
        if (prev_font_size - next_font_size).abs() > 0.5 {
            word_margin_ratio *= 0.7;
        }
        let threshold = space_width_pt * word_margin_ratio;

        log::debug!(
            "Font-aware spacing for '{}' @ {:.1}pt: space_width={:.1}pt, threshold={:.1}pt (mono={})",
            font_name,
            font_size,
            space_width_pt,
            threshold,
            is_monospace_font(font_name),
        );

        threshold
    } else {
        // Font not found: fallback to fixed 0.25em threshold
        log::debug!(
            "Font '{}' not found in font map, using default 0.25em threshold for {:.1}pt",
            font_name,
            font_size
        );
        font_size * 0.25
    };

    // suppress space insertion at AGL-
    // ligature boundaries. When the preceding or following text
    // starts with one of the Latin ligature codepoints (U+FB00..U+FB04)
    // or matches the multi-char AGL ligature names, the small kerning
    // gap that surrounds the ligature glyph is NOT a word boundary —
    // it's an intra-word position artefact from pdfTeX-style ligature
    // emission. Inflating the threshold by 1.5× at these positions
    // catches the `di ff cult` → `difficult` repro from issue .
    let ligature_boundary = starts_with_agl_ligature(following_text)
        || preceding_text
            .chars()
            .last()
            .map(|c| ('\u{FB00}'..='\u{FB06}').contains(&c))
            .unwrap_or(false);
    let geometric_threshold = if ligature_boundary {
        geometric_threshold * 1.5
    } else {
        geometric_threshold
    };

    let geometric_suggests_space = gap_pt > geometric_threshold;

    // #365 / B8b: Intra-word kerning guard (letter-letter branch).
    //
    // On TJ-heavy producers (LaTeX, MS Word → PDF) the Primary
    // word-boundary detector hands `should_insert_space` two adjacent
    // clusters like "cha"→"nge", "diffe"→"rent", "equivalen"→"t"
    // whose gap sits just above `geometric_threshold` (= 0.5 ×
    // space-glyph width) but well below a real word gap. The
    // consensus rule below would then emit a spurious space mid-word.
    // Real word gaps in real producers reach one full space-glyph
    // width or sit next to punctuation/digits, both of which fall
    // through this guard.
    //
    // The guard fires regardless of `tj_offset_triggered` because the
    // gap can also be geometric-only (when WordBoundaryDetector splits
    // the cluster but no explicit TJ offset crossed the threshold).
    // See the sibling guard in `process_tj_array_tiebreaker` for the
    // upstream space-as-span insertion path.
    //
    // Ceiling = 1.5 × `geometric_threshold` (= 0.75 × space-glyph width,
    // ≈ 0.2 em for a typical 0.25-em space). Inter-letter kerning is a
    // property of font size — realistic microtype / Word letter-spacing
    // is a few percent of the em and sits just above the 0.5-space-width
    // threshold, never far beyond it. The previous 2.4× ceiling
    // (≈ 1.2 × a full space-glyph advance, ≈ 0.33 em for Helvetica) was
    // far wider than any real kerning and swallowed genuine *tight* word
    // gaps between lowercase words — the dominant cause of
    // "MasterofScience" / "Resultsdriven" gluing on resume-style PDFs
    // that position words via small Td offsets. 1.5× still clears
    // worst-case ~0.19-em intra-word kerning (including the ~0.15-em
    // LaTeX/microtype letter-spacing case) while letting a
    // 0.2-em-and-wider word gap through to the consensus path — the same
    // ~0.18-0.2-em word-break point PyMuPDF / poppler use. Gaps in the
    // overlap zone (wide letter-tracking in titles, ~0.28 em) are not
    // separable from real word gaps by magnitude alone and fall through.
    //
    // Only fires when the font is available so the threshold is
    // computed from the font's own space-glyph advance — the no-font
    // fallback (`font_size * 0.25`) is a wider, deliberately
    // conservative value that already separates real word gaps from
    // kerning at the consensus level.
    let kerning_guard_threshold = if fonts.contains_key(font_name) {
        Some(geometric_threshold * 1.5)
    } else {
        None
    };
    if let Some(thr) = kerning_guard_threshold {
        if gap_pt < thr {
            let prev_last = preceding_text.chars().last();
            let next_first = following_text.chars().next();
            if let (Some(pc), Some(nc)) = (prev_last, next_first) {
                // Use is_lowercase on both sides: LaTeX/microtype intra-word kerning
                // occurs within lowercase letter runs. Real word boundaries in
                // professional PDFs frequently involve uppercase letters (headings,
                // abbreviations, proper nouns) — those fall through to the consensus
                // path, avoiding word-gluing like "APPENDIXA" or "OLIVERA.".
                if pc.is_lowercase() && nc.is_lowercase() {
                    log::debug!(
                        "intra-word kerning guard: suppressing space between '{pc}' and '{nc}' (gap={gap_pt:.2}pt < {thr:.2}pt, threshold = 0.75× space-glyph width)"
                    );
                    return SpaceDecision::no_space(SpaceSource::IntraWordKerning, 0.9);
                }
            }
        }
    }

    // Consensus checking
    // Only insert space if BOTH signals agree OR geometric signal is very strong
    // This reduces false positives in justified text where TJ offsets are arbitrary
    if tj_offset_triggered && geometric_suggests_space {
        // HIGH CONFIDENCE: Both TJ and geometric signals agree
        log::debug!(
            "Space decision: CONSENSUS - both TJ and geometric signals triggered (gap={:.2}pt > {:.2}pt) - inserting space",
            gap_pt,
            geometric_threshold
        );
        return SpaceDecision::insert(SpaceSource::TjOffset, 1.0);
    }

    // TJ offset with relaxed geometric confirmation
    // In tight typesetting (e.g., LaTeX academic papers), word gaps are narrower than
    // the standard 50% space-width threshold. When the PDF producer explicitly encoded
    // a TJ offset, accept a lower geometric bar (25% of space width).
    if tj_offset_triggered && gap_pt > geometric_threshold * 0.5 {
        log::debug!(
            "Space decision: TJ + relaxed geometric (gap={:.2}pt > {:.2}pt relaxed threshold) - inserting space",
            gap_pt,
            geometric_threshold * 0.5
        );
        return SpaceDecision::insert(SpaceSource::TjOffset, 0.9);
    }

    // WordBoundaryDetector tiebreaker when TJ and geometric signals conflict
    // Per ISO 32000-1:2008 Section 9.4.4, use multiple signals to determine word boundaries
    if tj_offset_triggered != geometric_suggests_space {
        if let (Some(prev_box), Some(next_box)) = (prev_bbox, next_bbox) {
            let (characters, context) = build_boundary_characters(
                preceding_text,
                following_text,
                prev_box,
                next_box,
                font_size,
                tj_offset_triggered,
            );

            // Use WordBoundaryDetector with geometric gap ratio matching our threshold
            // OPTIMIZATION: Detect document script profile to skip unnecessary detectors
            let script = DocumentScript::detect_from_characters(&characters);
            let detector = WordBoundaryDetector::new()
                .with_document_script(script)
                .with_geometric_gap_ratio(0.5);
            let boundaries = detector.detect_word_boundaries(&characters, &context);

            if !boundaries.is_empty() {
                log::debug!(
                    "Space decision: WordBoundaryDetector resolved conflict (TJ={}, geo={}) - inserting space",
                    tj_offset_triggered,
                    geometric_suggests_space
                );
                return SpaceDecision::insert(SpaceSource::WordBoundaryAnalysis, 0.85);
            }
        }
    }

    // Strong geometric signal alone.
    //
    // `geometric_threshold` is already `space_width_pt * 0.5`. A gap that
    // clears this threshold is >= 50 % of the font's own space-glyph
    // advance, which is what pdfium (Chrome/pypdfium2) uses as the
    // word-break heuristic in its default text-extraction path —
    // the reason pdf_oxide was glueing adjacent words like
    // "atBirmingham", "LIFESCIENCESRESEARCH", "STATIONFREEDOM",
    // "proteincrystals" before this change. The previous 2× multiplier
    // required gaps >= 100 % of a full space glyph, which is stricter
    // than the gaps modern tightly-kerned typesetters emit between
    // real words (often 60-80 % of a space glyph).
    //
    // Intra-word kerning and letter-spacing adjustments are well below
    // 50 % of a space glyph (typically under 5 % of font-size), so
    // lowering this threshold does not produce false word breaks
    // inside words. Pure digit-digit sequences are separately protected
    // in the value/token branch below via `digit_digit_gap_ok`.
    //
    // See issue #326 for the corpus-wide measurement that motivated
    // this change (NASA Apollo 11 jaccard 0.449 → target >= 0.90 vs
    // pypdfium2 on the 60-PDF regression corpus).
    if gap_pt > geometric_threshold {
        log::debug!(
            "Space decision: STRONG GEOMETRIC - gap={:.2}pt > {:.2}pt threshold - inserting space",
            gap_pt,
            geometric_threshold
        );
        return SpaceDecision::insert(SpaceSource::GeometricGap, 0.95);
    }

    // Separate token detection: when two spans have a positive gap and look like
    // distinct values (not fragments of the same word), insert a space.
    //
    // This catches adjacent table cell values like "$0.00" "$0.00" that have small
    // gaps (1-2pt) which fall below the standard geometric threshold but are clearly
    // separate tokens. Word fragments within the same word have zero or near-zero
    // gaps; any meaningful positive gap between non-fragment tokens indicates a
    // word boundary.
    //
    // Heuristic: gap > 0 AND spans look like separate tokens based on boundary characters.
    // Use near-zero threshold for currency boundaries (any positive gap = separate)
    let min_token_gap = 0.01; // Essentially any positive gap triggers token check
    if gap_pt > min_token_gap {
        let prev_last = preceding_text.chars().last();
        let next_first = following_text.chars().next();

        if let (Some(pc), Some(nc)) = (prev_last, next_first) {
            // Separate value tokens: digit/currency/punctuation boundaries that
            // indicate two distinct values rather than fragments of one word.
            // Examples: "$0.00" + "$0.00", "100" + "200", "Subtotal" + "$500.00"
            let prev_is_value_end = pc.is_ascii_digit() || pc == '%' || pc == ')' || pc == ']';

            // Pure digit→digit boundaries require a larger gap than the
            // global `min_token_gap`: a long number emitted as multiple
            // spans (e.g. due to glyph-level kerning or TJ positioning
            // rounding) can have a tiny positive gap between adjacent
            // digit spans, which must NOT become "123 456". Anything less
            // than half the font-aware geometric threshold is treated as
            // intra-number kerning, not a token boundary.
            let digit_digit = nc.is_ascii_digit() && pc.is_ascii_digit();
            let digit_digit_gap_ok = !digit_digit || gap_pt > geometric_threshold * 0.5;

            let next_is_value_start = nc == '$'
                || nc == '('
                || nc == '['
                || (nc == '-' && following_text.len() > 1)
                || (nc.is_ascii_digit() && prev_is_value_end && digit_digit_gap_ok);

            // Also detect: any text followed by currency symbol
            // e.g., "Subtotal" + "$500.00" or "49" + "$0.00"
            let text_then_currency = (pc.is_ascii_alphabetic() || pc.is_ascii_digit())
                && (nc == '$' || nc == '€' || nc == '£');

            if (prev_is_value_end && next_is_value_start) || text_then_currency {
                log::debug!(
                    "Space decision: SEPARATE VALUES - gap={:.2}pt > {:.2}pt min, prev='{}', next='{}' - inserting space",
                    gap_pt,
                    min_token_gap,
                    crate::utils::safe_suffix(preceding_text, 5),
                    crate::utils::safe_prefix(following_text, 5),
                );
                return SpaceDecision::insert(SpaceSource::GeometricGap, 0.85);
            }
        }
    }

    // Default: No space
    // Per ISO 32000-1:2008 Section 9.10, when PDF doesn't encode a clear word boundary,
    // we cannot reliably recover it. Requiring consensus prevents false positives in justified text.
    log::trace!(
        "Space decision: Insufficient consensus (TJ={}, gap={:.2}pt <= {:.2}pt) - no space",
        tj_offset_triggered,
        gap_pt,
        geometric_threshold
    );
    SpaceDecision::no_space(SpaceSource::NoSpace, 1.0)
}

/// Check if a boundary between spans already has whitespace.
///
/// Returns true if:
/// - The preceding text ends with whitespace, OR
/// - The following text starts with whitespace
///
/// This prevents double-spacing when text already contains space characters.
pub(super) fn has_boundary_space(preceding: &str, following: &str) -> bool {
    // Use ends_with/starts_with patterns instead of .chars().last() to avoid
    // O(n) iteration over the entire accumulated text
    let has_trailing_space = preceding.ends_with(|c: char| c.is_whitespace());
    let has_leading_space = following.starts_with(|c: char| c.is_whitespace());

    has_trailing_space || has_leading_space
}

/// Build CharacterInfo for word boundary analysis between two text segments.
///
/// Creates minimal character info for the last character of the preceding text
/// and the first character of the following text. This allows WordBoundaryDetector
/// to determine if a word boundary exists between two spans.
///
/// Per ISO 32000-1:2008 Section 9.4.4, word boundaries can be identified through:
/// - TJ array offsets (passed via tj_offset_triggered)
/// - Geometric gaps between glyphs (calculated from bbox positions)
/// - Space characters in the text stream
/// - CJK character transitions
pub(super) fn build_boundary_characters(
    prev_text: &str,
    next_text: &str,
    prev_bbox: &Rect,
    next_bbox: &Rect,
    font_size: f32,
    tj_offset_triggered: bool,
) -> (Vec<CharacterInfo>, BoundaryContext) {
    let prev_last_char = prev_text.chars().last().unwrap_or(' ');
    let next_first_char = next_text.chars().next().unwrap_or(' ');

    // Estimate character widths from bbox and character count
    // Use byte length as fast O(1) approximation (accurate for ASCII, close for UTF-8)
    // to avoid O(n) char counting on the accumulated merge text
    let prev_char_count = prev_text.len().max(1) as f32;
    let prev_char_width = prev_bbox.width / prev_char_count;
    let prev_last_x = prev_bbox.x + prev_bbox.width - prev_char_width;

    let next_char_count = next_text.len().max(1) as f32;
    let next_char_width = next_bbox.width / next_char_count;

    // Build CharacterInfo for boundary analysis
    let characters = vec![
        CharacterInfo {
            code: prev_last_char as u32,
            glyph_id: None,
            width: prev_char_width,
            x_position: prev_last_x,
            // Convert TJ trigger to offset value: -200 indicates word boundary
            tj_offset: if tj_offset_triggered {
                Some(-200)
            } else {
                None
            },
            font_size,
            is_ligature: false, // Not relevant for tiebreaker mode
            original_ligature: None,
            protected_from_split: false,
        },
        CharacterInfo {
            code: next_first_char as u32,
            glyph_id: None,
            width: next_char_width,
            x_position: next_bbox.x,
            tj_offset: None,
            font_size,
            is_ligature: false, // Not relevant for tiebreaker mode
            original_ligature: None,
            protected_from_split: false,
        },
    ];

    let context = BoundaryContext {
        font_size,
        horizontal_scaling: 100.0, // Default; actual value not available at span level
        word_spacing: 0.0,
        char_spacing: 0.0,
    };

    (characters, context)
}

/// Check if surrounding text forms an email-like pattern.
/// Per PDF spec, uses only extracted text pattern matching.
///
/// Patterns detected:
/// - "user@outlook" + "." + "com" (space before TLD)
/// - "user@" + "domain.com" (space after @)
pub(super) fn is_email_context(preceding_text: &str, following_text: &str) -> bool {
    // Only check the last ~64 bytes for email patterns to avoid O(n) scan
    // of the entire accumulated text (which would cause O(n²) in merge loop)
    let prev_start = preceding_text.len().saturating_sub(64);
    // Round up to the next UTF-8 char boundary. `str::ceil_char_boundary`
    // would do this in one line but it's only stable since Rust 1.91,
    // above our MSRV (1.88 — pinned by transitive deps).
    let prev_start = {
        let mut i = prev_start;
        while i < preceding_text.len() && !preceding_text.is_char_boundary(i) {
            i += 1;
        }
        i
    };
    let prev = preceding_text[prev_start..].trim_end();
    let next = following_text.trim_start();

    // Pattern 1: @ followed by domain part
    if prev.contains('@') {
        let after_at = prev.split('@').next_back().unwrap_or("");

        // Pattern 1a: "outlook" + "." → likely email
        if !after_at.is_empty() && next.starts_with('.') {
            return true;
        }

        // Pattern 1b: "outlook." + "com" → likely email
        if after_at.ends_with('.') && next.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            return true;
        }
    }

    // Pattern 2: Previous ends with @ (immediate after @)
    if prev.ends_with('@')
        && next
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return true;
    }

    false
}

/// Detect if bounding boxes indicate citation marker context.
/// Per PDF spec Section 9.3, citation markers have distinct visual properties:
/// - Smaller font size (typically 50-75% of body text)
/// - Raised position (superscript)
pub(super) fn is_citation_context(
    prev_bbox: Option<&crate::geometry::Rect>,
    next_bbox: Option<&crate::geometry::Rect>,
    current_font_size: f32,
    prev_font_size: f32,
    next_font_size: f32,
) -> bool {
    let prev_ratio = prev_font_size / current_font_size;
    let next_ratio = next_font_size / current_font_size;

    // Superscript range: 50-75% of body text size
    const SUPERSCRIPT_MIN: f32 = 0.5;
    const SUPERSCRIPT_MAX: f32 = 0.75;

    let prev_is_superscript = (SUPERSCRIPT_MIN..=SUPERSCRIPT_MAX).contains(&prev_ratio);
    let next_is_superscript = (SUPERSCRIPT_MIN..=SUPERSCRIPT_MAX).contains(&next_ratio);

    if let (Some(prev_box), Some(next_box)) = (prev_bbox, next_bbox) {
        let vertical_offset = (prev_box.y - next_box.y).abs();
        let is_raised = vertical_offset > (current_font_size * 0.2);

        // Either previous OR next is superscript + raised
        if (prev_is_superscript || next_is_superscript) && is_raised {
            return true;
        }
    }

    // Fallback: just font size check if bbox unavailable
    prev_is_superscript || next_is_superscript
}
