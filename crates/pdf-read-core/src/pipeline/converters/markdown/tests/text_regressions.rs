use super::*;

// ============================================================================
// Issue #260: Single-word BT/ET blocks should have spaces between words
// ============================================================================

#[test]
fn test_issue_260_single_word_bt_et_blocks_get_spaces() {
    // PDFKit.NET places each word in its own BT/ET block with absolute positioning.
    // The markdown converter must detect the horizontal gap and insert a space.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    // Simulate: "The" at x=72 w=20, "quick" at x=96 w=30, "brown" at x=130 w=33
    // All same Y=500, font_size=12. Gaps: 96-92=4pt, 130-126=4pt.
    // 4pt gap > 0.15*12=1.8pt threshold → should insert space.
    let spans = vec![
        make_span_with_width("The", 72.0, 500.0, 20.0, 12.0, FontWeight::Normal, 0),
        make_span_with_width("quick", 96.0, 500.0, 30.0, 12.0, FontWeight::Normal, 1),
        make_span_with_width("brown", 130.0, 500.0, 33.0, 12.0, FontWeight::Normal, 2),
    ];

    let result = converter.convert(&spans, &config).unwrap();
    assert!(
        result.contains("The quick brown"),
        "Single-word BT/ET spans with gaps should have spaces inserted: got {:?}",
        result
    );
}

#[test]
fn test_issue_260_no_space_for_tight_spans() {
    // When spans are tightly packed (no significant gap), no extra space should be added.
    // This covers ligature fragments or split characters.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    // "Hel" at x=72 w=18, "lo" at x=90 w=12 — gap = 90-90 = 0pt, no space needed
    let spans = vec![
        make_span_with_width("Hel", 72.0, 500.0, 18.0, 12.0, FontWeight::Normal, 0),
        make_span_with_width("lo", 90.0, 500.0, 12.0, 12.0, FontWeight::Normal, 1),
    ];

    let result = converter.convert(&spans, &config).unwrap();
    assert!(
        result.contains("Hello"),
        "Tight spans should be merged without space: got {:?}",
        result
    );
}

/// D7 — Arabic text with Bold font-weight must NOT produce `**` markers in
/// the markdown output.  Reproduces the right_to_left_02 fixture where
/// contextual glyph forms (initial/medial/final) triggered the bold
/// detector, inserting spurious `**مرح**با` fragments.
#[test]
fn test_arabic_bold_span_no_spurious_bold_markers() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Even when the font is reported as Bold, Arabic text must NOT be
    // wrapped in `**…**` in the final markdown (the bold detector fires on
    // Latin-font-weight heuristics that are unreliable for Arabic glyphs).
    let span = make_span("مرحبا", 0.0, 100.0, 12.0, FontWeight::Bold);
    let result = converter.convert(&[span], &config).unwrap();
    assert!(
        !result.contains("**"),
        "spurious bold markers found in Arabic output: {:?}",
        result
    );
    assert!(
        result.contains("مرحبا"),
        "Arabic text lost in output: {:?}",
        result
    );
}

/// D7 — is_rtl_text / looks_rtl must return true for Arabic Unicode ranges
/// and false for ASCII.  Pins the detector contract used by the converter.
#[test]
fn test_rtl_detection_arabic_and_ascii() {
    // Arabic main block
    assert!(
        crate::text::bidi::looks_rtl("مرحبا"),
        "Arabic U+0600-U+06FF must be RTL"
    );
    // Arabic Presentation Forms-B (common in PDFs using contextual forms)
    assert!(
        crate::text::bidi::looks_rtl("\u{FE80}"),
        "Arabic Presentation Forms-B U+FE80 must be RTL"
    );
    // Hebrew
    assert!(
        crate::text::bidi::looks_rtl("שלום"),
        "Hebrew U+0590-U+05FF must be RTL"
    );
    // Pure ASCII must not trigger the RTL path.
    assert!(
        !crate::text::bidi::looks_rtl("hello world"),
        "ASCII must not be RTL"
    );
    assert!(
        !crate::text::bidi::looks_rtl(""),
        "empty string must not be RTL"
    );
}

/// D7 — strip_inline_emphasis_in_rtl must remove `**…**` and `*…*`
/// markers when the inner content is RTL (Arabic / Hebrew) and preserve
/// them when the inner content is LTR.
#[test]
fn test_strip_inline_emphasis_removes_rtl_markers() {
    // `**bold**` around Arabic text → markers stripped
    let out = strip_inline_emphasis_in_rtl("**مرح**با");
    assert!(
        !out.contains("**"),
        "bold markers must be stripped from Arabic: {:?}",
        out
    );
    assert!(
        out.contains("مرح") && out.contains("با"),
        "Arabic chars must survive stripping: {:?}",
        out
    );

    // `*italic*` around Arabic text → markers stripped
    let out2 = strip_inline_emphasis_in_rtl("*مرحبا*");
    assert!(
        !out2.contains('*'),
        "italic markers must be stripped from Arabic: {:?}",
        out2
    );
    assert!(out2.contains("مرحبا"), "Arabic text lost: {:?}", out2);

    // Emphasis around LTR content must be preserved.
    let out3 = strip_inline_emphasis_in_rtl("*Hello*");
    assert_eq!(
        out3, "*Hello*",
        "LTR emphasis must be preserved: {:?}",
        out3
    );

    // No asterisks → identity.
    let out4 = strip_inline_emphasis_in_rtl("مرحبا");
    assert_eq!(
        out4, "مرحبا",
        "no-asterisk path must be identity: {:?}",
        out4
    );
}

/// D7 — the RTL emphasis cleanup block must preserve the trailing newline
/// that the whitespace-normalisation pass added.  Previously `lines().join()`
/// silently dropped the terminal `\n`, corrupting multi-paragraph documents.
#[test]
fn test_rtl_cleanup_preserves_trailing_newline() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Two Arabic paragraphs separated by `\n\n`.  The result must end with
    // the same suffix the normaliser emits (a single `\n` in default mode).
    let mut s1 = make_span("مرحبا", 0.0, 200.0, 12.0, FontWeight::Normal);
    s1.block_id = Some(1);
    let mut s2 = make_span("عالم", 0.0, 100.0, 12.0, FontWeight::Normal);
    s2.block_id = Some(2);
    let result = converter.convert(&[s1, s2], &config).unwrap();
    // Must contain both words.
    assert!(
        result.contains("مرحبا"),
        "first Arabic word lost: {:?}",
        result
    );
    assert!(
        result.contains("عالم"),
        "second Arabic word lost: {:?}",
        result
    );
    // Result must end with a newline (the document-level trailing `\n`).
    assert!(
        result.ends_with('\n'),
        "trailing newline was dropped by RTL cleanup: {:?}",
        result
    );
}

/// REGRESSION GUARD (70-PDF sweep). Consecutive paragraphs with
/// identical text (e.g. several distinct form widgets that share
/// a label) must NOT be deduped away by the pipeline. The
/// dedup_consecutive_paragraphs step that did this is retired.
#[test]
fn test_regression_repeated_identical_paragraphs_preserved() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span(
            "Radio button, unselected",
            0.0,
            100.0,
            12.0,
            FontWeight::Normal,
        ),
        make_span(
            "Radio button, unselected",
            0.0,
            80.0,
            12.0,
            FontWeight::Normal,
        ),
        make_span(
            "Radio button, unselected",
            0.0,
            60.0,
            12.0,
            FontWeight::Normal,
        ),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    let count = result.matches("Radio button, unselected").count();
    assert_eq!(
        count, 3,
        "three distinct identical-label widgets must all survive, got {}:\n{}",
        count, result
    );
}

/// Issue #9 — page-number-shaped lines (e.g. "Page 1 of 12",
/// "— 5 —", "[12]") MUST be preserved in the markdown output if
/// they appear in the prose stream. Dropping them at this layer
/// would discard legitimate content — the proper fix is upstream
/// artifact (`/Artifact` tag) handling per PDF §14.8.2.2. This
/// test pins that contract: the post-process pipeline does not
/// touch these lines.
#[test]
fn test_issue9_preserves_page_number_shaped_lines() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span("Some text.", 0.0, 100.0, 12.0, FontWeight::Normal),
        make_span("Page 1 of 12", 0.0, 80.0, 10.0, FontWeight::Normal),
        make_span("More text.", 0.0, 60.0, 12.0, FontWeight::Normal),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    assert!(
        result.contains("Page 1 of 12"),
        "page-N text must survive, got:\n{}",
        result
    );
    assert!(
        result.contains("Some text."),
        "prose must survive, got:\n{}",
        result
    );
    assert!(
        result.contains("More text."),
        "prose must survive, got:\n{}",
        result
    );
}

/// Issue #9 — in-prose "Page N" references must obviously also
/// survive (this was the existing guard).
#[test]
fn test_issue9_preserves_page_in_prose() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![make_span(
        "See Page 3 for details about the change.",
        0.0,
        100.0,
        12.0,
        FontWeight::Normal,
    )];
    let result = converter.convert(&spans, &config).unwrap();
    assert!(
        result.contains("See Page 3 for details"),
        "in-prose 'Page N' must not be dropped, got:\n{}",
        result
    );
}

/// Issue #13 — wrong-glyph bullets (`❍`, `◦`, ...) at line start
/// must NOT be silently dropped. The upstream renderer already
/// recognizes these as bullet-glyph variants and emits them as
/// idiomatic markdown `- ` bullets — that preserves the semantic
/// list structure across all glyph variants. What this test
/// pins is content preservation: the text content after the
/// glyph (`First item`, `Second item`) must reach the output;
/// the bullet symbol itself can be normalized to `-` because
/// markdown's bullet semantics are the same.
///
/// What is NOT acceptable (the bug we're guarding against): a
/// post-process layer pattern-matching codepoints and rewriting
/// them in arbitrary text. The pipeline does no such rewriting
/// (see `normalize_bullet_glyphs` no-op doc).
#[test]
fn test_issue13_preserves_bullet_text_content() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span("\u{274D} First item", 0.0, 100.0, 12.0, FontWeight::Normal),
        make_span("\u{25E6} Second item", 0.0, 80.0, 12.0, FontWeight::Normal),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    assert!(
        result.contains("First item"),
        "list-item text must survive: {}",
        result
    );
    assert!(
        result.contains("Second item"),
        "list-item text must survive: {}",
        result
    );
}

/// Issue #13 (mid-prose codepoint preservation). A `❍` that
/// appears in the MIDDLE of body text (not at line start) must
/// be preserved verbatim — at that position the upstream does
/// not treat it as a bullet, so any rewriting would be content
/// corruption.
#[test]
fn test_issue13_preserves_mid_prose_bullet_codepoint() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![make_span(
        "The symbol \u{274D} indicates a shadow circle.",
        0.0,
        100.0,
        12.0,
        FontWeight::Normal,
    )];
    let result = converter.convert(&spans, &config).unwrap();
    assert!(
        result.contains("\u{274D}"),
        "mid-prose U+274D must survive verbatim, got:\n{}",
        result
    );
}

/// Issue #2 — consecutive duplicate paragraphs (structured +
/// plaintext echo) must be deduped down to one.
#[test]
fn test_issue2_dedup_consecutive_duplicate_paragraphs() {
    let input = "Revenue grew by 15%.\n\nRevenue grew by 15%.\n\nNext paragraph here.\n";
    let out = dedup_consecutive_paragraphs(input);
    let occurrences = out.matches("Revenue grew by 15%.").count();
    assert_eq!(
        occurrences, 1,
        "exact-duplicate consecutive paragraph must collapse, got:\n{}",
        out
    );
    assert!(
        out.contains("Next paragraph here."),
        "subsequent paragraph must survive, got:\n{}",
        out
    );
}

/// Issue #2 — non-consecutive duplicates (separated by other
/// content) must NOT be touched: legitimate prose can repeat a
/// phrase later in the document.
#[test]
fn test_issue2_preserves_nonconsecutive_repeats() {
    let input = "Important note.\n\nOther content.\n\nImportant note.\n";
    let out = dedup_consecutive_paragraphs(input);
    let occurrences = out.matches("Important note.").count();
    assert_eq!(
        occurrences, 2,
        "non-consecutive repeat must survive, got:\n{}",
        out
    );
}

// ==========================================================================
// Bidi-isolation markers in markdown output (#537 follow-up — v0.3.55).
// Acceptance tests from
// docs/releases/plans/v0.3.55/fix-537-followup-bidi-isolation-markers.md.
// ==========================================================================

/// Hebrew run in an LTR-dominant line — must be wrapped with
/// U+2067 (RLI) … U+2069 (PDI) so a UAX #9-aware markdown
/// viewer does not let neutrals around the Hebrew bleed across
/// the boundary. Pre-fix output had no markers; this test pins
/// the post-fix behaviour.
#[test]
fn markdown_wraps_rtl_run_with_rli_pdi() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let span = make_span(
        "The article שלום עולם is greetings.",
        0.0,
        100.0,
        12.0,
        FontWeight::Normal,
    );
    let result = converter.convert(&[span], &config).unwrap();
    assert!(
        result.contains('\u{2067}'),
        "expected U+2067 (RLI) in markdown output, got:\n{:?}",
        result
    );
    assert!(
        result.contains('\u{2069}'),
        "expected U+2069 (PDI) in markdown output, got:\n{:?}",
        result
    );
    // Block is LTR-dominant — LTR runs must NOT get LRI.
    assert!(
        !result.contains('\u{2066}'),
        "unexpected U+2066 (LRI) in LTR-block output:\n{:?}",
        result
    );
}

/// English brand-name embedded in a Hebrew (RTL-dominant) line
/// — the English run must be wrapped with U+2066 (LRI) …
/// U+2069 (PDI).
#[test]
fn markdown_wraps_ltr_run_inside_rtl_block_with_lri_pdi() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let span = make_span("הספר Microsoft חדש", 0.0, 100.0, 12.0, FontWeight::Normal);
    let result = converter.convert(&[span], &config).unwrap();
    assert!(
        result.contains('\u{2066}'),
        "expected U+2066 (LRI) wrapping the embedded LTR token, got:\n{:?}",
        result
    );
    assert!(
        result.contains('\u{2069}'),
        "expected U+2069 (PDI) closing the LRI, got:\n{:?}",
        result
    );
    // Block is RTL-dominant — RTL runs must NOT get RLI.
    assert!(
        !result.contains('\u{2067}'),
        "unexpected U+2067 (RLI) in RTL-block output:\n{:?}",
        result
    );
}

/// Regression guard: pure-LTR markdown output must contain
/// ZERO bidi-isolation markers anywhere. This is the "no
/// markers appear in pure-LTR documents" contract from the
/// v0.3.55 plan's acceptance criteria. If this ever fails, the
/// isolation pass leaked into LTR-only output.
#[test]
fn markdown_leaves_pure_ltr_unchanged() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span("The first paragraph.", 0.0, 100.0, 12.0, FontWeight::Normal),
        make_span("A second sentence.", 0.0, 84.0, 12.0, FontWeight::Normal),
        make_span(
            "Numbers 123 and (parens) too.",
            0.0,
            68.0,
            12.0,
            FontWeight::Normal,
        ),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    for marker in ['\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}'] {
        assert!(
            !result.contains(marker),
            "pure-LTR output must not contain U+{:04X}, got:\n{:?}",
            marker as u32,
            result
        );
    }
}
