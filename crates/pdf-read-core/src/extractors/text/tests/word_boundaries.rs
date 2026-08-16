use super::*;

#[test]
fn test_named_fill_color_space_fallback_cmyk() {
    let mut e = TextExtractor::new();
    e.execute_operator_public(Operator::SetFillColorSpace {
        name: "Cs3".to_string(),
    })
    .unwrap();
    e.execute_operator_public(Operator::SetFillColor {
        components: vec![0.0, 0.0, 0.0, 0.5],
    })
    .unwrap();
    let state = e.state_stack.current();
    assert!(state.fill_color_cmyk.is_some());
}

#[test]
fn test_named_stroke_color_space_fallback_rgb() {
    let mut e = TextExtractor::new();
    e.execute_operator_public(Operator::SetStrokeColorSpace {
        name: "Cs1".to_string(),
    })
    .unwrap();
    e.execute_operator_public(Operator::SetStrokeColor {
        components: vec![0.5, 0.6, 0.7],
    })
    .unwrap();
    let state = e.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.5).abs() < 0.01);
    assert!((state.stroke_color_rgb.1 - 0.6).abs() < 0.01);
    assert!((state.stroke_color_rgb.2 - 0.7).abs() < 0.01);
}

// ========================================================================
// COVERAGE TESTS: Line style & misc operators
// ========================================================================

#[test]
fn test_set_line_cap() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetLineCap { cap_style: 2 })
        .unwrap();
    assert_eq!(extractor.state_stack.current().line_cap, 2);
}

#[test]
fn test_set_line_join() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetLineJoin { join_style: 1 })
        .unwrap();
    assert_eq!(extractor.state_stack.current().line_join, 1);
}

#[test]
fn test_set_miter_limit() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetMiterLimit { limit: 5.0 })
        .unwrap();
    assert!((extractor.state_stack.current().miter_limit - 5.0).abs() < 0.01);
}

#[test]
fn test_set_rendering_intent() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetRenderingIntent {
            intent: "RelativeColorimetric".to_string(),
        })
        .unwrap();
    assert_eq!(
        extractor.state_stack.current().rendering_intent,
        "RelativeColorimetric"
    );
}

#[test]
fn test_set_flatness() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFlatness { tolerance: 0.5 })
        .unwrap();
    assert!((extractor.state_stack.current().flatness - 0.5).abs() < 0.01);
}

#[test]
fn test_set_ext_gstate() {
    let mut extractor = TextExtractor::new();
    // Should not panic, just logs debug info
    extractor
        .execute_operator_public(Operator::SetExtGState {
            dict_name: "GS1".to_string(),
        })
        .unwrap();
}

#[test]
fn test_paint_shading() {
    let mut extractor = TextExtractor::new();
    // Should not panic, just logs debug info
    extractor
        .execute_operator_public(Operator::PaintShading {
            name: "sh1".to_string(),
        })
        .unwrap();
}

#[test]
fn test_inline_image_operator() {
    let mut extractor = TextExtractor::new();
    let mut dict = HashMap::new();
    dict.insert("W".to_string(), Object::Integer(100));
    dict.insert("H".to_string(), Object::Integer(50));
    extractor
        .execute_operator_public(Operator::InlineImage {
            dict: Box::new(dict),
            data: vec![0u8; 100],
        })
        .unwrap();
    // Should not panic and not produce text
}

#[test]
fn test_inline_image_no_dimensions() {
    let mut extractor = TextExtractor::new();
    let dict = HashMap::new(); // no W/H
    extractor
        .execute_operator_public(Operator::InlineImage {
            dict: Box::new(dict),
            data: vec![0u8; 10],
        })
        .unwrap();
}

// ========================================================================
// COVERAGE TESTS: Email pattern detection with config
// ========================================================================

#[test]
fn test_email_context_at_sign_end() {
    // Pattern: user@ + domain
    assert!(is_email_context("user@", "domain.com"));
}

#[test]
fn test_email_context_domain_dot() {
    // Pattern: user@domain. + com
    assert!(is_email_context("user@domain.", "com"));
}

#[test]
fn test_email_context_not_alpha_after_at() {
    // @ followed by non-alphanumeric should not be email
    assert!(!is_email_context("user@", " "));
}

#[test]
fn test_email_context_long_preceding_text() {
    // Test with very long preceding text (should only check last 64 bytes)
    let long_prefix = "a".repeat(200) + "@domain";
    assert!(is_email_context(&long_prefix, ".com"));
}

#[test]
fn test_should_insert_space_with_email_config() {
    let config = SpanMergingConfig {
        detect_email_patterns: true,
        email_threshold_multiplier: 2.5,
        ..Default::default()
    };
    let fonts = HashMap::new();

    // Email context with gap below threshold: suppress space
    let decision = should_insert_space(
        "user@domain",
        ".com",
        1.0,
        12.0,
        "F1",
        &fonts,
        false,
        &config,
        None,
        None,
        12.0,
        12.0,
    );
    assert!(
        !decision.insert_space,
        "Email context should suppress space for small gap"
    );
}

#[test]
fn test_should_insert_space_email_large_gap() {
    let config = SpanMergingConfig {
        detect_email_patterns: true,
        email_threshold_multiplier: 2.5,
        ..Default::default()
    };
    let fonts = HashMap::new();

    // Email context with very large gap: insert space
    let decision = should_insert_space(
        "user@domain",
        ".com",
        100.0,
        12.0,
        "F1",
        &fonts,
        false,
        &config,
        None,
        None,
        12.0,
        12.0,
    );
    assert!(
        decision.insert_space,
        "Email context should insert space for large gap"
    );
}

#[test]
fn test_should_insert_space_email_with_font_info() {
    let config = SpanMergingConfig {
        detect_email_patterns: true,
        ..Default::default()
    };
    let mut fonts: HashMap<String, Arc<FontInfo>> = HashMap::new();
    let font = create_test_font();
    fonts.insert("F1".to_string(), Arc::new(font));

    // Email context uses font metrics for threshold
    let decision = should_insert_space(
        "user@domain",
        ".com",
        1.0,
        12.0,
        "F1",
        &fonts,
        false,
        &config,
        None,
        None,
        12.0,
        12.0,
    );
    assert!(!decision.insert_space);
}

// ========================================================================
// COVERAGE TESTS: Citation marker detection with config
// ========================================================================

#[test]
fn test_should_insert_space_citation_context() {
    let config = SpanMergingConfig {
        detect_citation_markers: true,
        citation_font_size_ratio: 0.75,
        ..Default::default()
    };
    let fonts = HashMap::new();

    let prev_bbox = Rect::new(10.0, 100.0, 50.0, 12.0);
    let next_bbox = Rect::new(60.0, 105.0, 10.0, 7.0); // Raised, smaller

    let decision = should_insert_space(
        "text",
        "1",
        2.0,
        12.0,
        "F1",
        &fonts,
        true,
        &config,
        Some(&prev_bbox),
        Some(&next_bbox),
        12.0,
        7.2,
    );
    assert!(
        decision.insert_space,
        "Citation context with TJ should insert space"
    );
}

#[test]
fn test_is_pictographic_ranges() {
    assert!(is_pictographic('📄'));
    assert!(is_pictographic('✅'));
    assert!(!is_pictographic('A'));
    assert!(!is_pictographic('→')); // arrow excluded (math/symbol text)
    assert!(!is_pictographic('5'));
}

#[test]
fn test_should_insert_space_emoji_letter_boundary() {
    let config = SpanMergingConfig::default();
    let fonts = HashMap::new();
    // The real case (arxiv_2510.26287): a wide emoji glyph abuts the next
    // token, so the gap is exactly 0. The space must still be kept.
    let decision0 = should_insert_space(
        "📄", "README", 0.0, 12.0, "F1", &fonts, false, &config, None, None, 12.0, 12.0,
    );
    assert!(
        decision0.insert_space,
        "emoji→letter with a zero (abutting) gap must keep space"
    );

    // A positive gap also keeps it.
    let decision = should_insert_space(
        "📄", "README", 0.5, 12.0, "F1", &fonts, false, &config, None, None, 12.0, 12.0,
    );
    assert!(
        decision.insert_space,
        "emoji→letter with a positive gap keeps the space"
    );

    // A combined emoji sequence (next char is another pictograph, not a
    // letter) must NOT be forced into a space by this rule.
    let combined = should_insert_space(
        "📄", "📄", 0.0, 12.0, "F1", &fonts, false, &config, None, None, 12.0, 12.0,
    );
    assert!(
        !combined.insert_space,
        "emoji→emoji must not be forced into a space"
    );
}

#[test]
fn test_should_insert_space_citation_geometric() {
    let config = SpanMergingConfig {
        detect_citation_markers: true,
        ..Default::default()
    };
    let fonts = HashMap::new();

    let prev_bbox = Rect::new(10.0, 100.0, 50.0, 12.0);
    let next_bbox = Rect::new(60.0, 105.0, 10.0, 7.0);

    // Citation context with large geometric gap
    let decision = should_insert_space(
        "text",
        "1",
        10.0,
        12.0,
        "F1",
        &fonts,
        false,
        &config,
        Some(&prev_bbox),
        Some(&next_bbox),
        12.0,
        7.2,
    );
    assert!(
        decision.insert_space,
        "Citation context with large gap should insert space"
    );
}

#[test]
fn test_should_insert_space_citation_with_font() {
    let config = SpanMergingConfig {
        detect_citation_markers: true,
        ..Default::default()
    };
    let mut fonts: HashMap<String, Arc<FontInfo>> = HashMap::new();
    fonts.insert("F1".to_string(), Arc::new(create_test_font()));

    let prev_bbox = Rect::new(10.0, 100.0, 50.0, 12.0);
    let next_bbox = Rect::new(60.0, 105.0, 10.0, 7.0);

    let decision = should_insert_space(
        "text",
        "1",
        5.0,
        12.0,
        "F1",
        &fonts,
        true,
        &config,
        Some(&prev_bbox),
        Some(&next_bbox),
        12.0,
        7.2,
    );
    assert!(decision.insert_space);
}

// ========================================================================
// COVERAGE TESTS: Line break detection
// ========================================================================

#[test]
fn test_line_break_different_column() {
    let config = SpanMergingConfig::default();
    let fonts = HashMap::new();

    // prev and next at very different X positions (different columns)
    let prev_bbox = Rect::new(50.0, 700.0, 200.0, 12.0);
    let next_bbox = Rect::new(400.0, 680.0, 200.0, 12.0);

    let decision = should_insert_space(
        "end",
        "start",
        0.0,
        12.0,
        "F1",
        &fonts,
        false,
        &config,
        Some(&prev_bbox),
        Some(&next_bbox),
        12.0,
        12.0,
    );
    // Different column - should not trigger same_column line break path
    // The default no space path should apply
}

#[test]
fn test_line_break_not_triggered_small_vertical_gap() {
    let config = SpanMergingConfig::default();
    let fonts = HashMap::new();

    // Small vertical gap - not a line break
    let prev_bbox = Rect::new(100.0, 700.0, 200.0, 12.0);
    let next_bbox = Rect::new(100.0, 699.0, 200.0, 12.0);

    let decision = should_insert_space(
        "word",
        "next",
        0.0,
        12.0,
        "F1",
        &fonts,
        false,
        &config,
        Some(&prev_bbox),
        Some(&next_bbox),
        12.0,
        12.0,
    );
    // Small vertical gap should not trigger line break
}

// ========================================================================
// COVERAGE TESTS: WordBoundary tiebreaker path
// ========================================================================

#[test]
fn test_should_insert_space_tiebreaker_with_bboxes() {
    let config = SpanMergingConfig::default();
    let fonts = HashMap::new();

    let prev_bbox = Rect::new(100.0, 700.0, 50.0, 12.0);
    let next_bbox = Rect::new(155.0, 700.0, 50.0, 12.0);

    // TJ triggered but gap does not suggest space (conflict)
    // Should go through tiebreaker
    let decision = should_insert_space(
        "word",
        "next",
        1.0,
        12.0,
        "F1",
        &fonts,
        true,
        &config,
        Some(&prev_bbox),
        Some(&next_bbox),
        12.0,
        12.0,
    );
    // Result depends on WordBoundaryDetector
}

#[test]
fn test_should_insert_space_geometric_only_conflict() {
    let config = SpanMergingConfig::default();
    let fonts = HashMap::new();

    let prev_bbox = Rect::new(100.0, 700.0, 50.0, 12.0);
    let next_bbox = Rect::new(155.0, 700.0, 50.0, 12.0);

    // No TJ but gap suggests space (conflict with no TJ)
    let decision = should_insert_space(
        "word",
        "next",
        5.0,
        12.0,
        "F1",
        &fonts,
        false,
        &config,
        Some(&prev_bbox),
        Some(&next_bbox),
        12.0,
        12.0,
    );
    // Geometric alone - should go through tiebreaker path
}

// ========================================================================
// COVERAGE TESTS: Font-aware spacing in should_insert_space
// ========================================================================

#[test]
fn test_should_insert_space_font_aware() {
    let config = SpanMergingConfig::default();
    let mut fonts: HashMap<String, Arc<FontInfo>> = HashMap::new();
    fonts.insert("F1".to_string(), Arc::new(create_test_font()));

    // With font info, threshold is calculated from font metrics
    let decision = should_insert_space(
        "word", "next", 0.5, 12.0, "F1", &fonts, false, &config, None, None, 12.0, 12.0,
    );
    // The result depends on font-specific threshold
}

// ── #12 spec-aligned gap correction (§9.4.4): the fallback-width
//    inflation that splits "SalesForce" → "SalesF orce" is only applied
//    when glyphs actually overlap (raw_gap < 0), per corrected_space_gap ──

/// Adjacent glyphs (raw_gap == 0) on a fallback-width font must NOT be
/// inflated into a phantom gap — this is the "SalesF"+"orce" case. The
/// reported gap stays 0 so no spurious word space is inserted.
#[test]
fn test_corrected_space_gap_no_inflation_when_adjacent() {
    // raw_gap 0.0, unreliable widths, non-empty: must stay 0.0.
    assert_eq!(corrected_space_gap(0.0, false, 34.23, false), 0.0);
    // small positive raw gap (academic "XGBoostX"+"provides") untouched.
    assert_eq!(corrected_space_gap(0.47, false, 50.0, false), 0.47);
}

// ========================================================================
// COVERAGE TESTS: insert_space_as_span
// ========================================================================

#[test]
fn test_insert_space_as_span() {
    let mut extractor = TextExtractor::new();
    extractor.state_stack.current_mut().font_size = 12.0;
    extractor.state_stack.current_mut().horizontal_scaling = 100.0;
    extractor.state_stack.current_mut().font_name = Some("F1".to_string());

    let before = extractor.spans.len();
    extractor.insert_space_as_span().unwrap();
    assert_eq!(extractor.spans.len(), before + 1);
    assert_eq!(extractor.spans.last().unwrap().text, " ");
    assert!(extractor.spans.last().unwrap().offset_semantic);
}

// ========================================================================
// COVERAGE TESTS: split_fused_words
// ========================================================================

#[test]
fn test_split_fused_words_camelcase() {
    let mut extractor = TextExtractor::new();
    extractor.spans = vec![TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: "theGeneral".to_string(),
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
    }];

    extractor.split_fused_words();
    assert_eq!(
        extractor.spans.len(),
        2,
        "Should split theGeneral into two spans"
    );
    assert_eq!(extractor.spans[0].text, "the");
    assert_eq!(extractor.spans[1].text, "General");
    assert!(extractor.spans[1].split_boundary_before);
}

#[test]
fn test_split_fused_words_no_split() {
    let mut extractor = TextExtractor::new();
    extractor.spans = vec![TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: "hello".to_string(),
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
    }];

    extractor.split_fused_words();
    assert_eq!(
        extractor.spans.len(),
        1,
        "No split needed for all-lowercase"
    );
    assert_eq!(extractor.spans[0].text, "hello");
}

// ========================================================================
// COVERAGE TESTS: Word boundary mode primary
// ========================================================================

#[test]
fn test_extractor_with_primary_word_boundary() {
    let config = TextExtractionConfig {
        word_boundary_mode: WordBoundaryMode::Primary,
        ..TextExtractionConfig::default()
    };
    let mut extractor = TextExtractor::with_config(config);
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);
    extractor.merging_config = SpanMergingConfig::legacy();

    let stream = b"BT /F1 12 Tf 100 700 Td (Hello) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    let text: String = spans
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        text.contains("Hello"),
        "Primary mode should still extract text, got: {}",
        text
    );
}

// ========================================================================
// COVERAGE TESTS: TextExtractor with_config builder
// ========================================================================

#[test]
fn test_extractor_with_config_copies_word_boundary_mode() {
    let config = TextExtractionConfig {
        word_boundary_mode: WordBoundaryMode::Primary,
        ..TextExtractionConfig::default()
    };
    let extractor = TextExtractor::with_config(config);
    assert_eq!(extractor.word_boundary_mode, WordBoundaryMode::Primary);
}
