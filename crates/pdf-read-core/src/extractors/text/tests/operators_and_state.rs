use super::*;

// ========================================================================
// NEW COMPREHENSIVE TESTS: BT/ET operators
// ========================================================================

#[test]
fn test_bt_resets_text_matrix() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // First BT/ET block at (100, 700)
    // Second BT should reset text matrix to identity
    let stream = b"BT /F1 12 Tf 100 700 Td (A) Tj ET BT /F1 12 Tf (B) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 2);
    assert_eq!(chars[0].char, 'A');
    assert_eq!(chars[1].char, 'B');
    // B should be at origin (BT resets text matrix)
    assert!(
        chars[1].bbox.x < 10.0,
        "Second BT should reset text matrix, x={}",
        chars[1].bbox.x
    );
}

#[test]
fn test_multiple_bt_et_blocks() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 100 700 Td (Hello) Tj ET BT /F1 12 Tf 100 680 Td (World) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    let text: String = chars.iter().map(|c| c.char).collect();
    assert!(text.contains("Hello"), "Should contain Hello");
    assert!(text.contains("World"), "Should contain World");
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Marked content operators
// ========================================================================

#[test]
fn test_bmc_artifact_tracking() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Use execute_operator_public for fine-grained testing
    extractor
        .execute_operator_public(crate::content::operators::Operator::BeginMarkedContent {
            tag: "Artifact".to_string(),
        })
        .unwrap();

    assert!(
        extractor.inside_artifact,
        "Should be inside artifact after BMC Artifact"
    );

    extractor
        .execute_operator_public(crate::content::operators::Operator::EndMarkedContent)
        .unwrap();

    assert!(
        !extractor.inside_artifact,
        "Should be outside artifact after EMC"
    );
}

#[test]
fn test_bmc_non_artifact() {
    let mut extractor = TextExtractor::new();

    extractor
        .execute_operator_public(crate::content::operators::Operator::BeginMarkedContent {
            tag: "Span".to_string(),
        })
        .unwrap();

    assert!(
        !extractor.inside_artifact,
        "Non-artifact BMC should not set inside_artifact"
    );
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Font switching
// ========================================================================

#[test]
fn test_font_switch_mid_stream() {
    let mut extractor = TextExtractor::new();
    let font1 = create_test_font();
    let mut font2_data = create_test_font();
    font2_data.base_font = "Helvetica".to_string();
    extractor.add_font("F1".to_string(), font1);
    extractor.add_font("F2".to_string(), font2_data);

    let stream = b"BT /F1 12 Tf 100 700 Td (Hello) Tj /F2 14 Tf (World) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    // All characters should be extracted
    let text: String = chars.iter().map(|c| c.char).collect();
    assert!(text.contains("Hello"), "Should contain Hello");
    assert!(text.contains("World"), "Should contain World");
}

#[test]
fn test_font_switch_same_font_no_flush() {
    // Setting the same font twice should be a no-op (optimization)
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf /F1 12 Tf 100 700 Td (Test) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    let text: String = spans
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(text.contains("Test"), "Should extract text, got: {}", text);
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Cm operator (CTM modification)
// ========================================================================

#[test]
fn test_cm_operator_translation() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Apply translation via Cm operator
    let stream = b"1 0 0 1 50 100 cm BT /F1 12 Tf (X) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert!((chars[0].bbox.x - 50.0).abs() < 2.0, "X should be ~50");
    assert!((chars[0].bbox.y - 100.0).abs() < 2.0, "Y should be ~100");
}

#[test]
fn test_cm_operator_scaling() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Scale by 2x via CTM
    let stream = b"2 0 0 2 0 0 cm BT /F1 12 Tf 1 0 0 1 50 100 Tm (Y) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    // Position should be scaled: (50*2, 100*2) = (100, 200)
    assert!(
        (chars[0].bbox.x - 100.0).abs() < 2.0,
        "X should be ~100 (got {})",
        chars[0].bbox.x
    );
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Deduplication
// ========================================================================

#[test]
fn test_deduplicate_overlapping_chars() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Create overlapping chars (simulating bold rendering with duplicate glyphs)
    extractor.chars = vec![
        TextChar {
            char: 'A',
            bbox: Rect::new(100.0, 700.0, 6.0, 12.0),
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            is_italic: false,
            is_monospace: false,
            origin_x: 100.0,
            origin_y: 700.0,
            rotation_degrees: 0.0,
            advance_width: 6.0,
            rendered_advance: 6.0,
            ascent: 11.4,
            descent: -4.2,
            matrix: None,
        },
        TextChar {
            char: 'A',
            bbox: Rect::new(100.5, 700.0, 6.0, 12.0), // Very close X
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            is_italic: false,
            is_monospace: false,
            origin_x: 100.5,
            origin_y: 700.0,
            rotation_degrees: 0.0,
            advance_width: 6.0,
            rendered_advance: 6.0,
            ascent: 11.4,
            descent: -4.2,
            matrix: None,
        },
    ];

    extractor.deduplicate_overlapping_chars();
    assert_eq!(
        extractor.chars.len(),
        1,
        "Overlapping chars should be deduplicated"
    );
}

#[test]
fn test_deduplicate_overlapping_chars_different_lines() {
    let mut extractor = TextExtractor::new();

    // Chars on different lines should NOT be deduplicated
    extractor.chars = vec![
        TextChar {
            char: 'A',
            bbox: Rect::new(100.0, 700.0, 6.0, 12.0),
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            is_italic: false,
            is_monospace: false,
            origin_x: 100.0,
            origin_y: 700.0,
            rotation_degrees: 0.0,
            advance_width: 6.0,
            rendered_advance: 6.0,
            ascent: 11.4,
            descent: -4.2,
            matrix: None,
        },
        TextChar {
            char: 'A',
            bbox: Rect::new(100.0, 680.0, 6.0, 12.0), // Different Y
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            is_italic: false,
            is_monospace: false,
            origin_x: 100.0,
            origin_y: 680.0,
            rotation_degrees: 0.0,
            advance_width: 6.0,
            rendered_advance: 6.0,
            ascent: 11.4,
            descent: -4.2,
            matrix: None,
        },
    ];

    extractor.deduplicate_overlapping_chars();
    assert_eq!(
        extractor.chars.len(),
        2,
        "Chars on different lines should not be deduplicated"
    );
}

#[test]
fn test_deduplicate_overlapping_chars_empty() {
    let mut extractor = TextExtractor::new();
    extractor.deduplicate_overlapping_chars();
    assert!(extractor.chars.is_empty());
}

#[test]
fn test_deduplicate_keeps_distinct_close_chars() {
    // Issue #253: distinct characters close together should NOT be dropped
    let mut extractor = TextExtractor::new();

    let make_char = |c: char, x: f32| TextChar {
        char: c,
        bbox: Rect::new(x, 700.0, 6.0, 12.0),
        font_name: "F1".to_string(),
        font_size: 12.0,
        font_weight: FontWeight::Normal,
        color: Color::black(),
        mcid: None,
        is_italic: false,
        is_monospace: false,
        origin_x: x,
        origin_y: 700.0,
        rotation_degrees: 0.0,
        advance_width: 6.0,
        rendered_advance: 6.0,
        ascent: 11.4,
        descent: -4.2,
        matrix: None,
    };

    // 't' at x=100, ' ' at x=105, 'r' at x=106.5 (within 2pt of ' ' but different char)
    extractor.chars = vec![
        make_char('t', 100.0),
        make_char(' ', 105.0),
        make_char('r', 106.5),
    ];

    extractor.deduplicate_overlapping_chars();
    assert_eq!(
        extractor.chars.len(),
        3,
        "Distinct characters close together must not be dropped"
    );
    assert_eq!(extractor.chars[0].char, 't');
    assert_eq!(extractor.chars[1].char, ' ');
    assert_eq!(extractor.chars[2].char, 'r');
}

#[test]
fn test_deduplicate_still_removes_same_char_duplicates() {
    // Duplicate same character at nearly the same position should still be deduped
    let mut extractor = TextExtractor::new();

    let make_char = |c: char, x: f32| TextChar {
        char: c,
        bbox: Rect::new(x, 700.0, 6.0, 12.0),
        font_name: "F1".to_string(),
        font_size: 12.0,
        font_weight: FontWeight::Normal,
        color: Color::black(),
        mcid: None,
        is_italic: false,
        is_monospace: false,
        origin_x: x,
        origin_y: 700.0,
        rotation_degrees: 0.0,
        advance_width: 6.0,
        rendered_advance: 6.0,
        ascent: 11.4,
        descent: -4.2,
        matrix: None,
    };

    extractor.chars = vec![make_char('A', 100.0), make_char('A', 100.5)];

    extractor.deduplicate_overlapping_chars();
    assert_eq!(
        extractor.chars.len(),
        1,
        "Duplicate same char should still be deduped"
    );
    assert_eq!(extractor.chars[0].char, 'A');
}

#[test]
fn test_deduplicate_keeps_narrow_glyph_doublets() {
    // Regression: `ll`, `rr`, `II`, `ii` in small-font body text were
    // wrongly collapsed to a single glyph because the dedup threshold
    // was a hardcoded 2 pt — larger than the advance width of narrow
    // glyphs at ≤ 9 pt in most fonts (Helvetica `l` ≈ 2.5 pt at 9 pt,
    // smaller below). This caused visible corruption like
    // `controller → controler` and `billed → biled`.
    //
    // Exercises the matrix of four narrow glyphs across three small
    // body-text sizes. Advance widths are the real Helvetica per-em
    // values (0.278 em for `l`/`i`, 0.333 em for `r`, 0.278 em for `I`).
    let narrow_char = |c: char, x: f32, font_size: f32, advance_em: f32| TextChar {
        char: c,
        bbox: Rect::new(x, 700.0, advance_em * font_size * 0.6, font_size),
        font_name: "Helvetica".to_string(),
        font_size,
        font_weight: FontWeight::Normal,
        color: Color::black(),
        mcid: None,
        is_italic: false,
        is_monospace: false,
        origin_x: x,
        origin_y: 700.0,
        rotation_degrees: 0.0,
        advance_width: advance_em * font_size,
        rendered_advance: advance_em * font_size,
        ascent: 11.4,
        descent: -4.2,
        matrix: None,
    };

    // (glyph, Helvetica per-em advance width)
    let cases: &[(char, f32)] = &[('l', 0.278), ('r', 0.333), ('I', 0.278), ('i', 0.278)];
    // Body-text sizes where narrow-glyph advance falls at or below 2 pt.
    let sizes: &[f32] = &[7.0, 9.0, 11.0];

    for &(glyph, advance_em) in cases {
        for &font_size in sizes {
            let advance = advance_em * font_size;
            let mut extractor = TextExtractor::new();
            extractor.chars = vec![
                narrow_char(glyph, 100.0, font_size, advance_em),
                narrow_char(glyph, 100.0 + advance, font_size, advance_em),
            ];

            extractor.deduplicate_overlapping_chars();
            assert_eq!(
                extractor.chars.len(),
                2,
                "Adjacent narrow-glyph doublet ('{glyph}{glyph}') at {font_size} pt \
                 (advance = {advance:.2} pt) must not be collapsed",
            );
        }
    }
}

#[test]
fn test_deduplicate_still_collapses_narrow_glyph_stroke_fill_duplicates() {
    // Positive regression: even with the advance-scaled threshold,
    // stroke+fill render passes on narrow glyphs (two `l`s at ~0 pt
    // offset) must still be collapsed. The ratio (0.30) comfortably
    // catches real duplicates (< 5 % of one advance apart) while
    // staying below typical heaviest kerning (~20 %).
    let mut extractor = TextExtractor::new();

    let narrow_at = |x: f32| TextChar {
        char: 'l',
        bbox: Rect::new(x, 700.0, 1.5, 9.0),
        font_name: "Helvetica".to_string(),
        font_size: 9.0,
        font_weight: FontWeight::Normal,
        color: Color::black(),
        mcid: None,
        is_italic: false,
        is_monospace: false,
        origin_x: x,
        origin_y: 700.0,
        rotation_degrees: 0.0,
        advance_width: 2.5, // 0.278 em × 9 pt
        rendered_advance: 2.5,
        ascent: 11.4,
        descent: -4.2,
        matrix: None,
    };

    // Stroke pass and fill pass typically land within 0.05 pt of each
    // other (2 % of advance at 9 pt Helvetica `l`).
    extractor.chars = vec![narrow_at(100.0), narrow_at(100.05)];

    extractor.deduplicate_overlapping_chars();
    assert_eq!(
        extractor.chars.len(),
        1,
        "Stroke+fill narrow-glyph duplicates (same char at ~0 pt offset) \
         must still be collapsed"
    );
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Span deduplication
// ========================================================================

#[test]
fn test_deduplicate_overlapping_spans_geometric() {
    let mut extractor = TextExtractor::new();
    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "Hello".to_string(),
            bbox: Rect::new(100.0, 700.0, 30.0, 12.0),
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
            text: "Hello".to_string(),
            bbox: Rect::new(101.0, 700.0, 30.0, 12.0), // Very close
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
        "Geometric duplicates should be removed"
    );
}

#[test]
fn test_deduplicate_overlapping_spans_empty() {
    let mut extractor = TextExtractor::new();
    extractor.deduplicate_overlapping_spans();
    assert!(extractor.spans.is_empty());
}

#[test]
fn test_deduplicate_spans_keeps_narrow_glyph_doublets() {
    // Regression: PDFs that emit kerned text glyph-by-glyph produce
    // consecutive single-character spans. Two adjacent narrow-glyph
    // spans (`l`, `r`, `I`, `i` at ≤ 9 pt) sit roughly one advance-width
    // apart, which used to fall under the hardcoded 2 pt geometric
    // threshold and get collapsed. The threshold now scales with each
    // span's per-glyph width so legitimate doublets survive.
    //
    // Exercises the matrix of four narrow glyphs across three small
    // body-text sizes.
    let narrow_span = |glyph: char, x: f32, font_size: f32, advance: f32, seq: usize| TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: glyph.to_string(),
        bbox: Rect::new(x, 700.0, advance, font_size),
        font_name: "Helvetica".to_string(),
        font_size,
        font_weight: FontWeight::Normal,
        color: Color::black(),
        mcid: None,
        mcid_scope: None,
        sequence: seq,
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
    };

    // (glyph, Helvetica per-em advance width)
    let cases: &[(char, f32)] = &[('l', 0.278), ('r', 0.333), ('I', 0.278), ('i', 0.278)];
    let sizes: &[f32] = &[7.0, 9.0, 11.0];

    for &(glyph, advance_em) in cases {
        for &font_size in sizes {
            let advance = advance_em * font_size;
            let mut extractor = TextExtractor::new();
            extractor.spans = vec![
                narrow_span(glyph, 100.0, font_size, advance, 0),
                narrow_span(glyph, 100.0 + advance, font_size, advance, 1),
            ];

            extractor.deduplicate_overlapping_spans();
            assert_eq!(
                extractor.spans.len(),
                2,
                "Adjacent single-glyph narrow-doublet spans ('{glyph}{glyph}') \
                 at {font_size} pt (advance = {advance:.2} pt) must not be collapsed",
            );
        }
    }
}

// ========================================================================
// COVERAGE TESTS: Quote and DoubleQuote operators in span mode
// ========================================================================

#[test]
fn test_quote_operator_span_mode() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 14 TL 100 700 Td (Line1) Tj (Line2) ' ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    let text: String = spans
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        text.contains("Line1"),
        "Should contain Line1, got: {}",
        text
    );
    assert!(
        text.contains("Line2"),
        "Should contain Line2, got: {}",
        text
    );
}

#[test]
fn test_double_quote_operator_span_mode() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 14 TL 100 700 Td 1 2 (Text) \" ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    let text: String = spans
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(text.contains("Text"), "Should extract text, got: {}", text);
}

// ========================================================================
// COVERAGE TESTS: Color space resets (fill_color_cmyk cleared)
// ========================================================================

#[test]
fn test_fill_cmyk_then_change_color_space() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillCmyk {
            c: 0.5,
            m: 0.5,
            y: 0.5,
            k: 0.5,
        })
        .unwrap();
    assert!(extractor.state_stack.current().fill_color_cmyk.is_some());

    // Changing color space should reset CMYK
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "DeviceRGB".to_string(),
        })
        .unwrap();
    assert!(extractor.state_stack.current().fill_color_cmyk.is_none());
}

// ========================================================================
// COVERAGE TESTS: Do operator without document
// ========================================================================

#[test]
fn test_do_operator_without_document() {
    let mut extractor = TextExtractor::new();
    // Do without document set should not panic
    extractor
        .execute_operator_public(Operator::Do {
            name: "Im1".to_string(),
        })
        .unwrap();
}

// ========================================================================
// COVERAGE TESTS: TJ array with adaptive threshold - full pipeline
// ========================================================================

#[test]
fn test_tj_array_span_mode_with_space_insertion() {
    let config = TextExtractionConfig {
        use_adaptive_tj_threshold: false,
        space_insertion_threshold: -120.0,
        ..TextExtractionConfig::default()
    };
    let mut extractor = TextExtractor::with_config(config);
    extractor.merging_config = SpanMergingConfig::legacy();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // TJ array with large offset that triggers space
    let stream = b"BT /F1 12 Tf 100 700 Td [(Word1) -500 (Word2)] TJ ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    let text: String = spans
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(text.contains("Word1"), "Should contain Word1");
    assert!(text.contains("Word2"), "Should contain Word2");
}
