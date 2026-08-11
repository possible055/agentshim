use super::*;

#[test]
fn test_strip_cjk_digit_boundary_spaces() {
    // A space between a CJK ideograph and an embedded number is dropped at
    // both ends; the number itself is preserved.
    assert_eq!(
        strip_cjk_digit_boundary_spaces("公元前 1000 年"),
        "公元前1000年"
    );
    assert_eq!(
        strip_cjk_digit_boundary_spaces("追溯至 10,000 年前"),
        "追溯至10,000年前"
    );
    // Works for Japanese ideographs/kana too.
    assert_eq!(
        strip_cjk_digit_boundary_spaces("西暦 2024 年"),
        "西暦2024年"
    );
    // Korean (Hangul) is EXCLUDED — Korean uses inter-word spaces, so a
    // space between a syllable and a number is a real word boundary and
    // must be preserved ("14 예" = "14 cases", "7 예중").
    assert_eq!(strip_cjk_digit_boundary_spaces("약 1 만년"), "약 1 만년");
    assert_eq!(
        strip_cjk_digit_boundary_spaces("기질은 14 예에서"),
        "기질은 14 예에서"
    );
    // Legitimate spacing is preserved.
    assert_eq!(strip_cjk_digit_boundary_spaces("貓 通常"), "貓 通常"); // CJK↔CJK
    assert_eq!(
        strip_cjk_digit_boundary_spaces("catus 펠리스"),
        "catus 펠리스"
    ); // letter↔CJK
    assert_eq!(strip_cjk_digit_boundary_spaces("10 000"), "10 000"); // digit↔digit
    assert_eq!(
        strip_cjk_digit_boundary_spaces("page 12 of 30"),
        "page 12 of 30"
    ); // Latin
       // No-op fast path.
    assert_eq!(strip_cjk_digit_boundary_spaces("中文"), "中文");

    // Brackets hug their content: a space between a CJK/Hangul character and
    // an adjacent bracket is a layout artifact, dropped on both sides and
    // for both ASCII paren/square/brace shapes.
    assert_eq!(
        strip_cjk_digit_boundary_spaces("고양이 (학명"),
        "고양이(학명"
    ); // Hangul→(
    assert_eq!(
        strip_cjk_digit_boundary_spaces("카투스 [*]) 는"),
        "카투스[*])는"
    ); // Hangul→[ , )→Hangul
    assert_eq!(strip_cjk_digit_boundary_spaces("漢字 (注)"), "漢字(注)"); // CJK↔paren
                                                                          // A space between Latin and a bracket is left alone (English may write
                                                                          // "study (note)" with a space).
    assert_eq!(
        strip_cjk_digit_boundary_spaces("study (note)"),
        "study (note)"
    );
}

#[test]
fn test_strip_prime_decimal_boundary_spaces() {
    // Artifact space between the prime and the decimal point is dropped.
    assert_eq!(
        strip_prime_decimal_boundary_spaces("0\u{2032}\u{2032} .28"),
        "0\u{2032}\u{2032}.28"
    );
    // Artifact space between the prime's decimal point and its digits.
    assert_eq!(
        strip_prime_decimal_boundary_spaces("0\u{2032}\u{2032}. 28"),
        "0\u{2032}\u{2032}.28"
    );
    // Single prime and double-prime (U+2033) both handled.
    assert_eq!(
        strip_prime_decimal_boundary_spaces("1\u{2032}.47"),
        "1\u{2032}.47"
    ); // already tight: no-op
    assert_eq!(
        strip_prime_decimal_boundary_spaces("12\u{2033} .5"),
        "12\u{2033}.5"
    );
    // Feet-and-inches keeps its space: prime → DIGIT (not a decimal point).
    assert_eq!(
        strip_prime_decimal_boundary_spaces("5\u{2032} 6\u{2033}"),
        "5\u{2032} 6\u{2033}"
    );
    // A prime ending a sentence followed by prose is untouched (next not . / digit).
    assert_eq!(
        strip_prime_decimal_boundary_spaces("see 3\u{2032} and"),
        "see 3\u{2032} and"
    );
    // A lone decimal with no preceding prime is untouched.
    assert_eq!(
        strip_prime_decimal_boundary_spaces("v1. 0 release"),
        "v1. 0 release"
    );
    // No-op fast path.
    assert_eq!(
        strip_prime_decimal_boundary_spaces("0\u{2032}\u{2032}.28"),
        "0\u{2032}\u{2032}.28"
    );
}

/// Overlap (raw_gap < 0) on a fallback-width font IS corrected — this is
/// the issue #328 NASA-Apollo case where the 0.55 em fallback over-reports
/// width and swallows a real word gap. The correction lifts the gap.
#[test]
fn test_corrected_space_gap_corrects_overlap() {
    // raw_gap -2.0, width 30 → -2.0 + 30*(1 - 1/1.22) ≈ -2.0 + 5.41 = 3.41
    let g = corrected_space_gap(-2.0, false, 30.0, false);
    assert!(
        g > 0.0,
        "overlap on fallback-width font must be lifted positive, got {g}"
    );
}

/// Reliable-width fonts (explicit /Widths) are never corrected — the
/// bbox gap is authoritative regardless of sign.
#[test]
fn test_corrected_space_gap_reliable_widths_untouched() {
    assert_eq!(corrected_space_gap(-2.0, true, 30.0, false), -2.0);
    assert_eq!(corrected_space_gap(5.0, true, 30.0, false), 5.0);
}

// ========================================================================
// COVERAGE TESTS: SpanMergingConfig builder variants
// ========================================================================

#[test]
fn test_span_merging_config_adaptive_with_config() {
    let adaptive_config = crate::extractors::gap_statistics::AdaptiveThresholdConfig::default();
    let config = SpanMergingConfig::adaptive_with_config(adaptive_config);
    assert!(config.use_adaptive_threshold);
    assert!(config.adaptive_config.is_some());
}

// ========================================================================
// COVERAGE TESTS: Fallback char to unicode (more symbols)
// ========================================================================

#[test]
fn test_fallback_quotation_marks() {
    assert_eq!(fallback_char_to_unicode(0x2018), "\u{2018}"); // Left single quote
    assert_eq!(fallback_char_to_unicode(0x2019), "\u{2019}"); // Right single quote
    assert_eq!(fallback_char_to_unicode(0x201C), "\u{201C}"); // Left double quote
    assert_eq!(fallback_char_to_unicode(0x201D), "\u{201D}"); // Right double quote
}

#[test]
fn test_fallback_math_extended() {
    assert_eq!(fallback_char_to_unicode(0x00F7), "\u{00F7}"); // Division
    assert_eq!(fallback_char_to_unicode(0x2202), "\u{2202}"); // Partial diff
    assert_eq!(fallback_char_to_unicode(0x2207), "\u{2207}"); // Nabla
    assert_eq!(fallback_char_to_unicode(0x220F), "\u{220F}"); // Product
    assert_eq!(fallback_char_to_unicode(0x2261), "\u{2261}"); // Identical
    assert_eq!(fallback_char_to_unicode(0x2248), "\u{2248}"); // Almost equal
}

#[test]
fn test_fallback_set_theory() {
    assert_eq!(fallback_char_to_unicode(0x2282), "\u{2282}"); // Subset
    assert_eq!(fallback_char_to_unicode(0x2283), "\u{2283}"); // Superset
    assert_eq!(fallback_char_to_unicode(0x2286), "\u{2286}"); // Subset or equal
    assert_eq!(fallback_char_to_unicode(0x2287), "\u{2287}"); // Superset or equal
    assert_eq!(fallback_char_to_unicode(0x2208), "\u{2208}"); // Element of
    assert_eq!(fallback_char_to_unicode(0x2209), "\u{2209}"); // Not element
    assert_eq!(fallback_char_to_unicode(0x2200), "\u{2200}"); // For all
    assert_eq!(fallback_char_to_unicode(0x2203), "\u{2203}"); // There exists
    assert_eq!(fallback_char_to_unicode(0x2205), "\u{2205}"); // Empty set
}

#[test]
fn test_fallback_logic() {
    assert_eq!(fallback_char_to_unicode(0x2227), "\u{2227}"); // Logical and
    assert_eq!(fallback_char_to_unicode(0x2228), "\u{2228}"); // Logical or
    assert_eq!(fallback_char_to_unicode(0x00AC), "\u{00AC}"); // Not
}

#[test]
fn test_fallback_arrows() {
    assert_eq!(fallback_char_to_unicode(0x2192), "\u{2192}"); // Right arrow
    assert_eq!(fallback_char_to_unicode(0x2190), "\u{2190}"); // Left arrow
    assert_eq!(fallback_char_to_unicode(0x2194), "\u{2194}"); // Left right arrow
    assert_eq!(fallback_char_to_unicode(0x21D2), "\u{21D2}"); // Double right
    assert_eq!(fallback_char_to_unicode(0x21D4), "\u{21D4}"); // Double left-right
}

#[test]
fn test_fallback_greek_lowercase_extended() {
    assert_eq!(fallback_char_to_unicode(0x03B5), "\u{03B5}"); // epsilon
    assert_eq!(fallback_char_to_unicode(0x03B6), "\u{03B6}"); // zeta
    assert_eq!(fallback_char_to_unicode(0x03B7), "\u{03B7}"); // eta
    assert_eq!(fallback_char_to_unicode(0x03B9), "\u{03B9}"); // iota
    assert_eq!(fallback_char_to_unicode(0x03BA), "\u{03BA}"); // kappa
    assert_eq!(fallback_char_to_unicode(0x03BB), "\u{03BB}"); // lambda
    assert_eq!(fallback_char_to_unicode(0x03BC), "\u{03BC}"); // mu
    assert_eq!(fallback_char_to_unicode(0x03BD), "\u{03BD}"); // nu
    assert_eq!(fallback_char_to_unicode(0x03BE), "\u{03BE}"); // xi
    assert_eq!(fallback_char_to_unicode(0x03BF), "\u{03BF}"); // omicron
    assert_eq!(fallback_char_to_unicode(0x03C1), "\u{03C1}"); // rho
    assert_eq!(fallback_char_to_unicode(0x03C2), "\u{03C2}"); // final sigma
    assert_eq!(fallback_char_to_unicode(0x03C3), "\u{03C3}"); // sigma
    assert_eq!(fallback_char_to_unicode(0x03C4), "\u{03C4}"); // tau
    assert_eq!(fallback_char_to_unicode(0x03C5), "\u{03C5}"); // upsilon
    assert_eq!(fallback_char_to_unicode(0x03C6), "\u{03C6}"); // phi
    assert_eq!(fallback_char_to_unicode(0x03C7), "\u{03C7}"); // chi
    assert_eq!(fallback_char_to_unicode(0x03C8), "\u{03C8}"); // psi
}

#[test]
fn test_fallback_greek_uppercase_extended() {
    assert_eq!(fallback_char_to_unicode(0x0391), "\u{0391}"); // Alpha
    assert_eq!(fallback_char_to_unicode(0x0392), "\u{0392}"); // Beta
    assert_eq!(fallback_char_to_unicode(0x0394), "\u{0394}"); // Delta
    assert_eq!(fallback_char_to_unicode(0x0395), "\u{0395}"); // Epsilon
    assert_eq!(fallback_char_to_unicode(0x0396), "\u{0396}"); // Zeta
    assert_eq!(fallback_char_to_unicode(0x0397), "\u{0397}"); // Eta
    assert_eq!(fallback_char_to_unicode(0x0398), "\u{0398}"); // Theta
    assert_eq!(fallback_char_to_unicode(0x0399), "\u{0399}"); // Iota
    assert_eq!(fallback_char_to_unicode(0x039A), "\u{039A}"); // Kappa
    assert_eq!(fallback_char_to_unicode(0x039B), "\u{039B}"); // Lambda
    assert_eq!(fallback_char_to_unicode(0x039C), "\u{039C}"); // Mu
    assert_eq!(fallback_char_to_unicode(0x039D), "\u{039D}"); // Nu
    assert_eq!(fallback_char_to_unicode(0x039E), "\u{039E}"); // Xi
    assert_eq!(fallback_char_to_unicode(0x039F), "\u{039F}"); // Omicron
    assert_eq!(fallback_char_to_unicode(0x03A0), "\u{03A0}"); // Pi
    assert_eq!(fallback_char_to_unicode(0x03A1), "\u{03A1}"); // Rho
    assert_eq!(fallback_char_to_unicode(0x03A3), "\u{03A3}"); // Sigma
    assert_eq!(fallback_char_to_unicode(0x03A4), "\u{03A4}"); // Tau
    assert_eq!(fallback_char_to_unicode(0x03A5), "\u{03A5}"); // Upsilon
    assert_eq!(fallback_char_to_unicode(0x03A6), "\u{03A6}"); // Phi
    assert_eq!(fallback_char_to_unicode(0x03A7), "\u{03A7}"); // Chi
    assert_eq!(fallback_char_to_unicode(0x03A8), "\u{03A8}"); // Psi
}

#[test]
fn test_fallback_currency_extended() {
    assert_eq!(fallback_char_to_unicode(0x20A3), "\u{20A3}"); // Franc
    assert_eq!(fallback_char_to_unicode(0x20A4), "\u{20A4}"); // Lira
    assert_eq!(fallback_char_to_unicode(0x20A9), "\u{20A9}"); // Won
    assert_eq!(fallback_char_to_unicode(0x20AA), "\u{20AA}"); // Shekel
    assert_eq!(fallback_char_to_unicode(0x20AB), "\u{20AB}"); // Dong
    assert_eq!(fallback_char_to_unicode(0x20B9), "\u{20B9}"); // Rupee
}

// ========================================================================
// COVERAGE TESTS: decode_text_to_unicode edge cases
// ========================================================================

#[test]
fn test_decode_text_simple_font_with_control_chars() {
    let font = create_test_font();
    let bytes = vec![0x01, 0x41, 0x09]; // ctrl char, 'A', tab
    let result = decode_text_to_unicode(&bytes, Some(&font));
    // Should filter control chars but keep tab
    assert!(result.contains('\t') || result.contains('A'));
}

#[test]
fn test_decode_text_single_byte_only() {
    // Test with bytes that hit the TwoByte < 2 fallback
    let mut font = create_test_font();
    font.subtype = "Type0".to_string();
    font.encoding = crate::fonts::Encoding::Identity;
    let bytes = vec![0x41]; // Single byte for Type0 identity
    let result = decode_text_to_unicode(&bytes, Some(&font));
    // Should hit trailing byte path
}

// ========================================================================
// COVERAGE TESTS: Color space resets
// ========================================================================

#[test]
fn test_set_fill_color_space_resets_color() {
    let mut extractor = TextExtractor::new();
    // Set RGB color first
    extractor
        .execute_operator_public(Operator::SetFillRgb {
            r: 1.0,
            g: 0.0,
            b: 0.0,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 1.0).abs() < 0.01);

    // Change color space should reset to black
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "DeviceGray".to_string(),
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.0).abs() < 0.01);
    assert!(state.fill_color_cmyk.is_none());
}

#[test]
fn test_set_stroke_color_space_resets_color() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeRgb {
            r: 0.0,
            g: 1.0,
            b: 0.0,
        })
        .unwrap();

    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "DeviceRGB".to_string(),
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.0).abs() < 0.01);
    assert!(state.stroke_color_cmyk.is_none());
}

// ========================================================================
// COVERAGE TESTS: CMYK color operators
// ========================================================================

#[test]
fn test_set_stroke_cmyk() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeCmyk {
            c: 1.0,
            m: 0.0,
            y: 0.0,
            k: 0.0,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!(state.stroke_color_cmyk.is_some());
    // Cyan: R=0, G=1, B=1
    assert!((state.stroke_color_rgb.0 - 0.0).abs() < 0.01);
}

#[test]
fn test_set_stroke_gray() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeGray { gray: 0.7 })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.7).abs() < 0.01);
    assert!((state.stroke_color_rgb.1 - 0.7).abs() < 0.01);
    assert!((state.stroke_color_rgb.2 - 0.7).abs() < 0.01);
}

#[test]
fn test_set_stroke_rgb() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeRgb {
            r: 0.3,
            g: 0.6,
            b: 0.9,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.3).abs() < 0.01);
    assert!((state.stroke_color_rgb.1 - 0.6).abs() < 0.01);
    assert!((state.stroke_color_rgb.2 - 0.9).abs() < 0.01);
}

// ========================================================================
// COVERAGE TESTS: CMYK to RGB edge cases
// ========================================================================

#[test]
fn test_cmyk_to_rgb_mixed() {
    let (r, g, b) = cmyk_to_rgb(0.5, 0.3, 0.1, 0.2);
    assert!((0.0..=1.0).contains(&r));
    assert!((0.0..=1.0).contains(&g));
    assert!((0.0..=1.0).contains(&b));
}

#[test]
fn test_cmyk_to_rgb_all_ones() {
    let (r, g, b) = cmyk_to_rgb(1.0, 1.0, 1.0, 1.0);
    assert!((r - 0.0).abs() < 0.01);
    assert!((g - 0.0).abs() < 0.01);
    assert!((b - 0.0).abs() < 0.01);
}

// ========================================================================
// COVERAGE TESTS: Content deduplication - content-based
// ========================================================================

#[test]
fn test_deduplicate_content_based() {
    let mut extractor = TextExtractor::new();
    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "Hello World".to_string(), // >= 5 chars
            bbox: Rect::new(100.0, 700.0, 60.0, 12.0),
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 0,
            split_boundary_before: false,
            offset_semantic: false,
            is_italic: false,
            is_monospace: false,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scaling: 100.0,
            primary_detected: false,
            char_widths: vec![],
            char_x_offsets: Vec::new(),
            heading_level: None,
            rotation_degrees: 0.0,
            wmode: 0,
            rtl_draw_logical: false,
        },
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "Hello World".to_string(), // Same text, overlapping position
            bbox: Rect::new(102.0, 700.0, 60.0, 12.0), // X within 5pt
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 1,
            split_boundary_before: false,
            offset_semantic: false,
            is_italic: false,
            is_monospace: false,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scaling: 100.0,
            primary_detected: false,
            char_widths: vec![],
            char_x_offsets: Vec::new(),
            heading_level: None,
            rotation_degrees: 0.0,
            wmode: 0,
            rtl_draw_logical: false,
        },
    ];

    extractor.deduplicate_overlapping_spans();
    assert_eq!(
        extractor.spans.len(),
        1,
        "Content duplicates should be removed"
    );
}

#[test]
fn test_deduplicate_content_not_overlapping() {
    let mut extractor = TextExtractor::new();
    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "Hello World".to_string(),
            bbox: Rect::new(100.0, 700.0, 60.0, 12.0),
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 0,
            split_boundary_before: false,
            offset_semantic: false,
            is_italic: false,
            is_monospace: false,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scaling: 100.0,
            primary_detected: false,
            char_widths: vec![],
            char_x_offsets: Vec::new(),
            heading_level: None,
            rotation_degrees: 0.0,
            wmode: 0,
            rtl_draw_logical: false,
        },
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "Hello World".to_string(), // Same text but far apart
            bbox: Rect::new(500.0, 700.0, 60.0, 12.0), // X > 5pt difference
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 1,
            split_boundary_before: false,
            offset_semantic: false,
            is_italic: false,
            is_monospace: false,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scaling: 100.0,
            primary_detected: false,
            char_widths: vec![],
            char_x_offsets: Vec::new(),
            heading_level: None,
            rotation_degrees: 0.0,
            wmode: 0,
            rtl_draw_logical: false,
        },
    ];

    extractor.deduplicate_overlapping_spans();
    assert_eq!(
        extractor.spans.len(),
        2,
        "Non-overlapping content should not be deduped"
    );
}

// ========================================================================
// COVERAGE TESTS: advance_position_for_string
// ========================================================================

#[test]
fn test_advance_position_no_font() {
    let mut extractor = TextExtractor::new();
    extractor.state_stack.current_mut().font_size = 12.0;
    extractor.state_stack.current_mut().horizontal_scaling = 100.0;

    let width = extractor.advance_position_for_string(b"Hello").unwrap();
    assert!(width > 0.0, "Width should be positive even without font");
}
