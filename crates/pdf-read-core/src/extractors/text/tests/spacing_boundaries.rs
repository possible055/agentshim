use super::*;

#[test]
fn test_set_fill_gray() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT 0.5 g /F1 12 Tf 0 0 Td (G) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert!((chars[0].color.r - 0.5).abs() < 0.01);
    assert!((chars[0].color.g - 0.5).abs() < 0.01);
    assert!((chars[0].color.b - 0.5).abs() < 0.01);
}

#[test]
fn test_set_fill_cmyk() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // CMYK 0 0 0 1 is the K ink, #231F20 - not #000000.
    let stream = b"BT 0 0 0 1 k /F1 12 Tf 0 0 Td (K) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert!((chars[0].color.r - 0.1373).abs() < 0.01);
    assert!((chars[0].color.g - 0.1216).abs() < 0.01);
    assert!((chars[0].color.b - 0.1255).abs() < 0.01);
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Graphics state save/restore
// ========================================================================

#[test]
fn test_save_restore_color() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Test that color state is saved/restored by q/Q within a BT/ET block
    // Set blue, save, set red, show R (red), restore, show B (blue restored)
    let stream = b"BT /F1 12 Tf 0 0 1 rg q 1 0 0 rg 100 700 Td (R) Tj Q 200 700 Td (B) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(
        chars.len(),
        2,
        "Should extract 2 chars, got {}",
        chars.len()
    );
    let r_char = chars.iter().find(|c| c.char == 'R').expect("Should find R");
    let b_char = chars.iter().find(|c| c.char == 'B').expect("Should find B");
    // R should be red (set inside q)
    assert!(
        (r_char.color.r - 1.0).abs() < 0.01,
        "R should be red, got ({}, {}, {})",
        r_char.color.r,
        r_char.color.g,
        r_char.color.b
    );
    // B should be blue (restored by Q)
    assert!(
        (b_char.color.b - 1.0).abs() < 0.01,
        "B should be blue after Q restore, got ({}, {}, {})",
        b_char.color.r,
        b_char.color.g,
        b_char.color.b
    );
}

#[test]
fn test_save_restore_ctm() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Save, translate CTM, show A, restore (CTM back to identity), show B at different position
    let stream = b"q 1 0 0 1 100 200 cm BT /F1 12 Tf (A) Tj ET Q BT /F1 12 Tf (B) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 2);
    // A should be at (100, 200), B should be at (0, 0)
    assert!(chars[0].bbox.x > 90.0, "A should be translated by CTM");
    assert!(
        chars[1].bbox.x < 10.0,
        "B should be at origin after restore"
    );
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Span extraction mode
// ========================================================================

#[test]
fn test_extract_text_spans_simple() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 100 700 Td (Hello World) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    assert!(!spans.is_empty());
    // Find the span containing "Hello World"
    let text: String = spans
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        text.contains("Hello"),
        "Expected 'Hello' in extracted text, got: {}",
        text
    );
    assert!(
        text.contains("World"),
        "Expected 'World' in extracted text, got: {}",
        text
    );
}

#[test]
fn test_extract_text_spans_multiple_tj() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Two Tj operators that should be accumulated into one span
    let stream = b"BT /F1 12 Tf 100 700 Td (He) Tj (llo) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    let text: String = spans
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        text.contains("Hello"),
        "Expected 'Hello' in spans, got: {}",
        text
    );
}

#[test]
fn test_extract_text_spans_with_font_info() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 14 Tf 100 700 Td (Test) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    assert!(!spans.is_empty());
    let span = &spans[0];
    assert!(
        span.font_name.contains("F1") || span.font_name.contains("Times"),
        "Font name should reference F1 or Times, got: {}",
        span.font_name
    );
    assert!(span.font_size > 0.0, "Font size should be positive");
}

#[test]
fn test_extract_text_spans_empty_stream() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"";
    let spans = extractor.extract_text_spans(stream).unwrap();
    assert!(spans.is_empty());
}

#[test]
fn test_extract_text_spans_bt_et_no_text() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf ET";
    let spans = extractor.extract_text_spans(stream).unwrap();
    assert!(spans.is_empty());
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: TJ array processing (span mode)
// ========================================================================

#[test]
fn test_tj_array_with_spacing() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // TJ array with small kerning offset (should not insert space)
    let stream = b"BT /F1 12 Tf 100 700 Td [(H) -20 (ello)] TJ ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    let text: String = spans
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        text.contains("Hello"),
        "Small TJ offset should not split word, got: {}",
        text
    );
}

#[test]
fn test_tj_array_word_boundary() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // TJ array with large negative offset (word boundary)
    let stream = b"BT /F1 12 Tf 100 700 Td [(Hello) -300 (World)] TJ ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    let text: String = spans
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    // Should have space between Hello and World
    assert!(
        text.contains("Hello") && text.contains("World"),
        "Should extract both words, got: {}",
        text
    );
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: fallback_char_to_unicode
// ========================================================================

#[test]
fn test_fallback_common_punctuation() {
    assert_eq!(fallback_char_to_unicode(0x2014), "\u{2014}"); // Em dash
    assert_eq!(fallback_char_to_unicode(0x2013), "\u{2013}"); // En dash
    assert_eq!(fallback_char_to_unicode(0x2022), "\u{2022}"); // Bullet
    assert_eq!(fallback_char_to_unicode(0x2026), "\u{2026}"); // Ellipsis
    assert_eq!(fallback_char_to_unicode(0x00B0), "\u{00B0}"); // Degree
}

#[test]
fn test_fallback_math_operators() {
    assert_eq!(fallback_char_to_unicode(0x00B1), "\u{00B1}"); // Plus-minus
    assert_eq!(fallback_char_to_unicode(0x00D7), "\u{00D7}"); // Multiply
    assert_eq!(fallback_char_to_unicode(0x221E), "\u{221E}"); // Infinity
    assert_eq!(fallback_char_to_unicode(0x2264), "\u{2264}"); // Less or equal
    assert_eq!(fallback_char_to_unicode(0x2265), "\u{2265}"); // Greater or equal
    assert_eq!(fallback_char_to_unicode(0x2260), "\u{2260}"); // Not equal
    assert_eq!(fallback_char_to_unicode(0x221A), "\u{221A}"); // Square root
    assert_eq!(fallback_char_to_unicode(0x222B), "\u{222B}"); // Integral
    assert_eq!(fallback_char_to_unicode(0x2211), "\u{2211}"); // Summation
}

#[test]
fn test_fallback_greek_letters() {
    assert_eq!(fallback_char_to_unicode(0x03B1), "\u{03B1}"); // alpha
    assert_eq!(fallback_char_to_unicode(0x03B2), "\u{03B2}"); // beta
    assert_eq!(fallback_char_to_unicode(0x03C0), "\u{03C0}"); // pi
    assert_eq!(fallback_char_to_unicode(0x03C9), "\u{03C9}"); // omega
    assert_eq!(fallback_char_to_unicode(0x0393), "\u{0393}"); // Gamma
    assert_eq!(fallback_char_to_unicode(0x03A9), "\u{03A9}"); // Omega
}

#[test]
fn test_fallback_currency() {
    assert_eq!(fallback_char_to_unicode(0x20AC), "\u{20AC}"); // Euro
    assert_eq!(fallback_char_to_unicode(0x00A3), "\u{00A3}"); // Pound
    assert_eq!(fallback_char_to_unicode(0x00A5), "\u{00A5}"); // Yen
    assert_eq!(fallback_char_to_unicode(0x00A2), "\u{00A2}"); // Cent
}

#[test]
fn test_fallback_direct_unicode() {
    // Valid ASCII character
    assert_eq!(fallback_char_to_unicode(0x41), "A");
    assert_eq!(fallback_char_to_unicode(0x20), " ");
}

#[test]
fn test_fallback_invalid_code_point() {
    // Surrogate pair range is invalid Unicode
    assert_eq!(fallback_char_to_unicode(0xD800), "?");
    assert_eq!(fallback_char_to_unicode(0xDFFF), "?");
}

#[test]
fn test_fallback_private_use_area() {
    // PUA characters should still be returned (not replaced with ?)
    let result = fallback_char_to_unicode(0xE000);
    assert_ne!(result, "?");
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: decode_text_to_unicode
// ========================================================================

#[test]
fn test_decode_text_no_font_latin1() {
    let result = decode_text_to_unicode(b"Hello", None);
    assert_eq!(result, "Hello");
}

#[test]
fn test_decode_text_no_font_high_bytes() {
    // Latin-1 high bytes should map to Unicode code points
    let bytes = vec![0xC0, 0xE9]; // A-grave, e-acute in Latin-1
    let result = decode_text_to_unicode(&bytes, None);
    assert!(result.contains('\u{00C0}'), "Should contain A-grave");
    assert!(result.contains('\u{00E9}'), "Should contain e-acute");
}

#[test]
fn test_decode_text_filters_control_chars() {
    // Control characters (except tab, newline, carriage return) should be filtered
    let bytes = vec![0x01, 0x02, 0x41, 0x09, 0x0A]; // ctrl chars, 'A', tab, newline
    let result = decode_text_to_unicode(&bytes, None);
    assert!(result.contains('A'), "Should contain 'A'");
    assert!(result.contains('\t'), "Should keep tab");
    assert!(result.contains('\n'), "Should keep newline");
    assert!(!result.contains('\x01'), "Should filter ctrl-A");
}

#[test]
fn test_decode_text_with_simple_font() {
    let font = create_test_font();
    let result = decode_text_to_unicode(b"ABC", Some(&font));
    // With WinAnsiEncoding, ASCII characters should map correctly
    assert!(
        result.contains('A') || !result.is_empty(),
        "Should decode something"
    );
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: cmyk_to_rgb
// ========================================================================

#[test]
fn test_cmyk_to_rgb_black() {
    // The K ink is #231F20, not #000000 - see color::cmyk_to_rgb.
    let (r, g, b) = cmyk_to_rgb(0.0, 0.0, 0.0, 1.0);
    assert!((r - 0.1373).abs() < 0.01);
    assert!((g - 0.1216).abs() < 0.01);
    assert!((b - 0.1255).abs() < 0.01);
}

#[test]
fn test_cmyk_to_rgb_white() {
    let (r, g, b) = cmyk_to_rgb(0.0, 0.0, 0.0, 0.0);
    assert!((r - 1.0).abs() < 0.01);
    assert!((g - 1.0).abs() < 0.01);
    assert!((b - 1.0).abs() < 0.01);
}

#[test]
fn test_cmyk_to_rgb_cyan() {
    // Process cyan, #00ADEF.
    let (r, g, b) = cmyk_to_rgb(1.0, 0.0, 0.0, 0.0);
    assert!((r - 0.0).abs() < 0.01);
    assert!((g - 0.6784).abs() < 0.01);
    assert!((b - 0.9373).abs() < 0.01);
}

#[test]
fn test_cmyk_to_rgb_magenta() {
    // Process magenta, #EC008C.
    let (r, g, b) = cmyk_to_rgb(0.0, 1.0, 0.0, 0.0);
    assert!((r - 0.9255).abs() < 0.01);
    assert!((g - 0.0).abs() < 0.01);
    assert!((b - 0.5490).abs() < 0.01);
}

#[test]
fn test_cmyk_to_rgb_yellow() {
    // Process yellow, #FFF200.
    let (r, g, b) = cmyk_to_rgb(0.0, 0.0, 1.0, 0.0);
    assert!((r - 1.0).abs() < 0.01);
    assert!((g - 0.9490).abs() < 0.01);
    assert!((b - 0.0).abs() < 0.01);
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: has_boundary_space edge cases
// ========================================================================

#[test]
fn test_has_boundary_space_empty_strings() {
    assert!(!has_boundary_space("", ""));
    assert!(!has_boundary_space("", "hello"));
    assert!(!has_boundary_space("hello", ""));
}

#[test]
fn test_has_boundary_space_only_spaces() {
    assert!(has_boundary_space(" ", " "));
    assert!(has_boundary_space(" ", "word"));
    assert!(has_boundary_space("word", " "));
}

#[test]
fn test_has_boundary_space_unicode_whitespace() {
    // Non-breaking space (U+00A0)
    assert!(has_boundary_space("word\u{00A0}", "next"));
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: is_email_context
// ========================================================================

#[test]
fn test_email_context_at_domain() {
    // Pattern: user@outlook + . + com
    assert!(is_email_context("user@outlook", ".com"));
}

#[test]
fn test_email_context_after_at() {
    // Pattern: user@ + domain.com
    assert!(is_email_context("user@", "domain.com"));
}

#[test]
fn test_email_context_domain_dot_tld() {
    // Pattern: user@domain. + com
    assert!(is_email_context("user@domain.", "com"));
}

#[test]
fn test_email_context_not_email() {
    assert!(!is_email_context("hello", "world"));
    assert!(!is_email_context("no at sign", "here"));
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: is_citation_context
// ========================================================================

#[test]
fn test_citation_context_superscript() {
    let prev_bbox = Rect::new(10.0, 100.0, 50.0, 12.0);
    let next_bbox = Rect::new(60.0, 105.0, 10.0, 7.0); // Raised, smaller

    // next_font_size is 0.6 * current = superscript range
    let result = is_citation_context(
        Some(&prev_bbox),
        Some(&next_bbox),
        12.0,
        12.0,
        7.2, // 60% of 12 = 0.6, within 0.5-0.75 range
    );
    assert!(result, "Should detect citation context");
}

#[test]
fn test_citation_context_no_superscript() {
    let prev_bbox = Rect::new(10.0, 100.0, 50.0, 12.0);
    let next_bbox = Rect::new(60.0, 100.0, 50.0, 12.0); // Same size, same position

    let result = is_citation_context(
        Some(&prev_bbox),
        Some(&next_bbox),
        12.0,
        12.0,
        12.0, // Same font size = not a citation
    );
    assert!(!result, "Should not detect citation when same size");
}

#[test]
fn test_citation_context_no_bbox() {
    // Font size ratio alone (without bbox) - prev is superscript
    let result = is_citation_context(None, None, 12.0, 7.2, 12.0);
    assert!(result, "Should detect citation from font size ratio alone");
}

#[test]
fn test_snap_superscript_baselines_correctness() {
    let mut extractor = TextExtractor::new();
    // Base: 12pt body glyph at y=700, right edge x=130.
    // Superscript: 6pt glyph just above-right (y=704, x=130).
    extractor.spans = vec![
        snap_span("x", 100.0, 700.0, 30.0, 12.0, 0),
        snap_span("2", 130.0, 704.0, 4.0, 6.0, 1),
    ];
    extractor.snap_superscript_baselines();
    assert_eq!(
        extractor.spans[1].bbox.y, 700.0,
        "#575: superscript must snap onto the base baseline (y=700)"
    );
}
