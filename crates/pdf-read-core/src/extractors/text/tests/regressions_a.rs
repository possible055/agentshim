use super::*;

#[test]
fn test_advance_position_with_font() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);
    extractor.cached_current_font = extractor.fonts.get("F1").cloned();
    extractor.state_stack.current_mut().font_size = 12.0;
    extractor.state_stack.current_mut().font_name = Some("F1".to_string());
    extractor.state_stack.current_mut().horizontal_scaling = 100.0;

    let width = extractor.advance_position_for_string(b"Hi").unwrap();
    assert!(width > 0.0, "Width should be positive with font");
}

#[test]
fn test_advance_position_with_word_space() {
    let mut extractor = TextExtractor::new();
    extractor.state_stack.current_mut().font_size = 12.0;
    extractor.state_stack.current_mut().horizontal_scaling = 100.0;
    extractor.state_stack.current_mut().word_space = 5.0;

    let width = extractor.advance_position_for_string(b"A B").unwrap();
    assert!(width > 0.0);
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
// COVERAGE TESTS: calculate_average_glyph_width
// ========================================================================

#[test]
fn test_calculate_average_glyph_width_no_widths() {
    let extractor = TextExtractor::new();
    let font = create_test_font(); // No widths array
    let avg = extractor.calculate_average_glyph_width(&font);
    assert_eq!(avg, font.default_width);
}

#[test]
fn test_calculate_average_glyph_width_with_widths() {
    let extractor = TextExtractor::new();
    let mut font = create_test_font();
    font.first_char = Some(32);
    font.last_char = Some(126);
    font.widths = Some(vec![500.0; 95]); // 95 printable chars

    let avg = extractor.calculate_average_glyph_width(&font);
    assert!((avg - 500.0).abs() < 0.01);
}

#[test]
fn test_calculate_average_glyph_width_no_first_char() {
    let extractor = TextExtractor::new();
    let mut font = create_test_font();
    font.widths = Some(vec![500.0; 95]);
    font.first_char = None;

    let avg = extractor.calculate_average_glyph_width(&font);
    assert_eq!(avg, font.default_width);
}

#[test]
fn test_calculate_average_glyph_width_no_last_char() {
    let extractor = TextExtractor::new();
    let mut font = create_test_font();
    font.widths = Some(vec![500.0; 95]);
    font.first_char = Some(32);
    font.last_char = None;

    let avg = extractor.calculate_average_glyph_width(&font);
    assert_eq!(avg, font.default_width);
}

// ========================================================================
// COVERAGE TESTS: Adaptive TJ threshold with justified text
// ========================================================================

#[test]
fn test_adaptive_threshold_with_justified_text() {
    let config = TextExtractionConfig {
        use_adaptive_tj_threshold: true,
        word_margin_ratio: 0.1,
        ..TextExtractionConfig::default()
    };
    let mut extractor = TextExtractor::with_config(config);
    extractor.state_stack.current_mut().font_size = 12.0;

    // Simulate justified text (high CV)
    for i in 0..100 {
        extractor
            .tj_offset_history
            .push(if i % 2 == 0 { -50.0 } else { -200.0 });
    }

    let threshold = extractor.calculate_adaptive_tj_threshold();
    // Justified text uses 3x ratio, so threshold should be more negative
    assert!(threshold < 0.0);
}

#[test]
fn test_adaptive_threshold_with_font_name() {
    let config = TextExtractionConfig {
        use_adaptive_tj_threshold: true,
        word_margin_ratio: 0.1,
        ..TextExtractionConfig::default()
    };
    let mut extractor = TextExtractor::with_config(config);
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);
    extractor.state_stack.current_mut().font_size = 12.0;
    extractor.state_stack.current_mut().font_name = Some("F1".to_string());

    let threshold = extractor.calculate_adaptive_tj_threshold();
    assert!(threshold < 0.0);
}

#[test]
fn test_analyze_tj_distribution_zero_mean() {
    let mut extractor = TextExtractor::new();
    // Push offsets that average to near zero
    extractor.tj_offset_history = vec![100.0, -100.0, 100.0, -100.0];
    let (is_justified, cv) = extractor.analyze_tj_distribution();
    // Mean ~0, so CV should be 0 (avoid division by zero)
    assert_eq!(cv, 0.0);
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
// COVERAGE TESTS: Sort spans by columns (multi-column)
// ========================================================================

#[test]
fn test_sort_spans_by_columns() {
    let mut extractor = TextExtractor::new();
    // Create spans in two distinct columns
    let columns = vec![(0.0, 250.0), (300.0, 550.0)];

    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "Right Col".to_string(),
            bbox: Rect::new(350.0, 700.0, 100.0, 12.0),
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
            text: "Left Col".to_string(),
            bbox: Rect::new(50.0, 700.0, 100.0, 12.0),
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

    extractor.sort_spans_by_columns(&columns);
    // Left column should come first
    assert_eq!(extractor.spans[0].text, "Left Col");
    assert_eq!(extractor.spans[1].text, "Right Col");
}

// ========================================================================
// COVERAGE TESTS: TJ buffer with MCID
// ========================================================================

#[test]
fn test_tj_buffer_with_mcid() {
    let state = crate::content::graphics_state::GraphicsStateStack::new();
    let buffer = TjBuffer::new(state.current(), Some(42), None);
    assert!(buffer.is_empty());
    assert_eq!(buffer.mcid, Some(42));
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
// COVERAGE TESTS: Merge adjacent spans - double space prevention
// ========================================================================

#[test]
fn test_merge_prevents_double_space() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "Hello ".to_string(), // ends with space
            bbox: Rect::new(100.0, 700.0, 35.0, 12.0),
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
            text: " World".to_string(),                // starts with space
            bbox: Rect::new(136.0, 700.0, 35.0, 12.0), // 1pt gap
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 1,
            split_boundary_before: true, // forces merge-with-space path
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

    extractor.merge_adjacent_spans();
    assert_eq!(extractor.spans.len(), 1);
    // Should not have "Hello World" (triple space)
    assert!(
        !extractor.spans[0].text.contains("   "),
        "Should prevent triple space"
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

// ========================================================================
// COVERAGE TESTS: Partition characters boundary at start
// ========================================================================

#[test]
fn test_partition_boundary_at_start() {
    let extractor = TextExtractor::new();
    let chars = vec![
        CharacterInfo {
            code: 65,
            glyph_id: None,
            width: 10.0,
            x_position: 0.0,
            tj_offset: None,
            font_size: 12.0,
            is_ligature: false,
            original_ligature: None,
            protected_from_split: false,
        },
        CharacterInfo {
            code: 66,
            glyph_id: None,
            width: 10.0,
            x_position: 10.0,
            tj_offset: None,
            font_size: 12.0,
            is_ligature: false,
            original_ligature: None,
            protected_from_split: false,
        },
    ];

    // Boundary at 0 means empty first cluster
    let clusters = extractor.partition_characters_by_boundaries(&chars, vec![0]);
    // Should have just one cluster (boundary at 0 produces no items before it)
    assert!(!clusters.is_empty());
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
