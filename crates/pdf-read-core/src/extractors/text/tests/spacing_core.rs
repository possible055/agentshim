use super::*;

#[test]
fn test_default() {
    let extractor = TextExtractor::default();
    assert_eq!(extractor.char_count(), 0);
}

/// Test unified space decision: Boundary space already present
#[test]
fn test_space_decision_boundary_space() {
    let config = SpanMergingConfig::default();
    let fonts = std::collections::HashMap::new();

    // Preceding text ends with space
    let decision = should_insert_space(
        "word ", "next", 0.0, 12.0, "TestFont", &fonts, false, &config, None, None, 12.0, 12.0,
    );
    assert!(!decision.insert_space);
    assert_eq!(decision.source, SpaceSource::AlreadyPresent);

    // Following text starts with space
    let decision = should_insert_space(
        "word", " next", 0.0, 12.0, "TestFont", &fonts, false, &config, None, None, 12.0, 12.0,
    );
    assert!(!decision.insert_space);
    assert_eq!(decision.source, SpaceSource::AlreadyPresent);
}

/// Regression test for issue flagged in PR #281 review:
/// a long number emitted as multiple digit-only spans with a kerning-sized
/// positive gap must NOT have a space inserted between the digits (would
/// turn "123456" into "123 456"). Adjacent table cell digit values with a
/// larger gap must still be separated.
#[test]
fn test_space_decision_digit_digit_gap_threshold() {
    let config = SpanMergingConfig::default();
    let fonts = std::collections::HashMap::new();

    // Kerning-sized gap (0.3pt) between digit spans — must NOT insert.
    // For 12pt font with no font-info fallback, geometric_threshold is
    // typically around 1.5pt, so half of that is 0.75pt.
    let kerning = should_insert_space(
        "123", "456", 0.3, 12.0, "TestFont", &fonts, false, &config, None, None, 12.0, 12.0,
    );
    assert!(
        !kerning.insert_space,
        "Kerning-sized gap (0.3pt) between digits must not split the number, got: {:?}",
        kerning
    );

    // Larger gap (2pt) between digit spans — adjacent table cell values,
    // must still insert a space.
    let table_cells = should_insert_space(
        "123", "456", 2.0, 12.0, "TestFont", &fonts, false, &config, None, None, 12.0, 12.0,
    );
    assert!(
        table_cells.insert_space,
        "2pt gap between digits should still split adjacent table values, got: {:?}",
        table_cells
    );
}

/// Test split boundary merging with space insertion
///
/// When split_boundary_before=true, it indicates the span is part of a boundary
/// that was previously split (e.g., from CamelCase fusion like "theGeneral").
/// These spans should be merged WITH a space to preserve word separation.
#[test]
fn test_split_boundary_merges_with_space() {
    let spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "the".to_string(),
            bbox: Rect {
                x: 0.0,
                y: 100.0,
                width: 10.0,
                height: 12.0,
            },
            font_name: "Arial".to_string(),
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
            text: "General".to_string(),
            bbox: Rect {
                x: 10.0,
                y: 100.0,
                width: 25.0,
                height: 12.0,
            },
            font_name: "Arial".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 1,
            split_boundary_before: true, // Marks this as part of a split boundary
            offset_semantic: false,
            primary_detected: false,
            is_italic: false,
            is_monospace: false,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scaling: 100.0,
            char_widths: vec![],
            char_x_offsets: Vec::new(),
            heading_level: None,
            rotation_degrees: 0.0,
            wmode: 0,
            rtl_draw_logical: false,
        },
    ];

    // Simulate extraction state
    let mut extractor = TextExtractor::new();
    extractor.spans = spans;
    extractor.merging_config = SpanMergingConfig::default();

    // Merge adjacent spans
    extractor.merge_adjacent_spans();

    // Per PDF Spec ISO 32000-1:2008 Section 9.4.4 and implementation design:
    // split_boundary_before=true means "merge with a space, never without"
    // This ensures "length" + "This" becomes "length This" not "lengthThis"
    // The spans are merged INTO ONE span with space-separated text
    assert_eq!(extractor.spans.len(), 1);
    assert_eq!(extractor.spans[0].text, "the General");
}

// Removed: test_should_insert_space_heuristic - function doesn't exist in current codebase

/// Test boundary space detection
#[test]
fn test_has_boundary_space() {
    // Preceding text with trailing space
    assert!(has_boundary_space("word ", "next"));

    // Following text with leading space
    assert!(has_boundary_space("word", " next"));

    // Both with space
    assert!(has_boundary_space("word ", " next"));

    // Neither
    assert!(!has_boundary_space("word", "next"));

    // Only whitespace characters count
    assert!(has_boundary_space("word\t", "next"));
    assert!(has_boundary_space("word\n", "next"));
    assert!(has_boundary_space("word", "\tnext"));
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: TextExtractionConfig
// ========================================================================

#[test]
fn test_text_extraction_config_new_defaults() {
    let config = TextExtractionConfig::new();
    assert_eq!(config.space_insertion_threshold, -120.0);
    assert_eq!(config.word_margin_ratio, 0.1);
    assert!(!config.use_adaptive_tj_threshold);
    assert!(config.profile.is_none());
}

#[test]
fn test_text_extraction_config_with_space_threshold() {
    let config = TextExtractionConfig::with_space_threshold(-80.0);
    assert_eq!(config.space_insertion_threshold, -80.0);
    assert_eq!(config.word_margin_ratio, 0.1);
    assert!(!config.use_adaptive_tj_threshold);
}

#[test]
fn test_text_extraction_config_with_word_margin_ratio() {
    let config = TextExtractionConfig::with_word_margin_ratio(0.15);
    assert_eq!(config.word_margin_ratio, 0.15);
    assert!(config.use_adaptive_tj_threshold);
    assert_eq!(config.space_insertion_threshold, -120.0); // fallback
}

#[test]
fn test_text_extraction_config_set_word_margin_ratio() {
    let config = TextExtractionConfig::new().set_word_margin_ratio(0.2);
    assert_eq!(config.word_margin_ratio, 0.2);
    assert!(config.use_adaptive_tj_threshold);
}

#[test]
fn test_text_extraction_config_set_adaptive_tj_threshold() {
    let config = TextExtractionConfig::new().set_adaptive_tj_threshold(true);
    assert!(config.use_adaptive_tj_threshold);
    let config2 = config.set_adaptive_tj_threshold(false);
    assert!(!config2.use_adaptive_tj_threshold);
}

#[test]
fn test_text_extraction_config_with_profile() {
    let config =
        TextExtractionConfig::new().with_profile(crate::config::ExtractionProfile::ACADEMIC);
    assert!(config.profile.is_some());
    let profile = config.profile.unwrap();
    assert_eq!(profile.name, "Academic");
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: SpanMergingConfig
// ========================================================================

#[test]
fn test_span_merging_config_defaults() {
    let config = SpanMergingConfig::new();
    assert_eq!(config.space_threshold_em_ratio, 0.25);
    assert_eq!(config.conservative_threshold_pt, 0.1);
    assert_eq!(config.column_boundary_threshold_pt, 5.0);
    assert_eq!(config.severe_overlap_threshold_pt, -0.5);
    assert!(config.use_adaptive_threshold);
    assert!(!config.detect_email_patterns);
    assert!(!config.detect_citation_markers);
}

#[test]
fn test_span_merging_config_aggressive() {
    let config = SpanMergingConfig::aggressive();
    assert_eq!(config.space_threshold_em_ratio, 0.15);
    assert_eq!(config.conservative_threshold_pt, 0.1);
    assert!(!config.use_adaptive_threshold);
}

#[test]
fn test_span_merging_config_conservative() {
    let config = SpanMergingConfig::conservative();
    assert_eq!(config.space_threshold_em_ratio, 0.33);
    assert_eq!(config.conservative_threshold_pt, 0.3);
    assert!(!config.use_adaptive_threshold);
}

#[test]
fn test_span_merging_config_custom() {
    let config = SpanMergingConfig::custom(0.2, 0.2, 6.0, -0.3);
    assert_eq!(config.space_threshold_em_ratio, 0.2);
    assert_eq!(config.conservative_threshold_pt, 0.2);
    assert_eq!(config.column_boundary_threshold_pt, 6.0);
    assert_eq!(config.severe_overlap_threshold_pt, -0.3);
    assert!(!config.use_adaptive_threshold);
}

#[test]
fn test_span_merging_config_adaptive() {
    let config = SpanMergingConfig::adaptive();
    assert!(config.use_adaptive_threshold);
    assert!(config.adaptive_config.is_some());
}

#[test]
fn test_span_merging_config_legacy() {
    let config = SpanMergingConfig::legacy();
    assert!(!config.use_adaptive_threshold);
    assert_eq!(config.conservative_threshold_pt, 0.1);
    assert!(config.adaptive_config.is_none());
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: SpaceDecision
// ========================================================================

#[test]
fn test_space_decision_insert() {
    let d = SpaceDecision::insert(SpaceSource::TjOffset, 0.95);
    assert!(d.insert_space);
    assert_eq!(d.source, SpaceSource::TjOffset);
    assert_eq!(d.confidence, 0.95);
}

#[test]
fn test_space_decision_no_space() {
    let d = SpaceDecision::no_space(SpaceSource::NoSpace, 1.0);
    assert!(!d.insert_space);
    assert_eq!(d.source, SpaceSource::NoSpace);
    assert_eq!(d.confidence, 1.0);
}

#[test]
fn test_space_decision_clamp_confidence() {
    let d = SpaceDecision::insert(SpaceSource::GeometricGap, 1.5);
    assert_eq!(d.confidence, 1.0); // clamped
    let d2 = SpaceDecision::insert(SpaceSource::GeometricGap, -0.5);
    assert_eq!(d2.confidence, 0.0); // clamped
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Text operators via execute_operator
// ========================================================================

#[test]
fn test_operator_td_positioning() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // BT, set font, Td to position (100, 700), show "X", ET
    let stream = b"BT /F1 12 Tf 100 700 Td (X) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert_eq!(chars[0].char, 'X');
    // After Td(100, 700), position should be near (100, 700)
    assert!((chars[0].bbox.x - 100.0).abs() < 2.0);
    assert!((chars[0].bbox.y - 700.0).abs() < 2.0);
}

/// Issue #254: TD Y offset must be scaled by the text matrix.
/// Pattern: `/F1 1 Tf 10 0 0 10 72 700 Tm (Line one) Tj 0 -1.3 TD (Line two) Tj`
/// The Tm sets a 10x scale, so `0 -1.3 TD` should produce a 13pt vertical gap,
/// not 1.3pt. Both lines must appear in extracted text.
#[test]
fn test_issue_254_tm_scale_td_offset() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Font size 1 with Tm scale 10 — effective font size is 10pt.
    // TD(0, -1.3) in text space = 13pt in user space.
    let stream = b"BT /F1 1 Tf 10 0 0 10 72 700 Tm (Line one) Tj 0 -1.3 TD (Line two) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    // Collect unique text
    let text: String = chars.iter().map(|c| c.char).collect();
    assert!(
        text.contains("Line one"),
        "Should contain 'Line one', got: {}",
        text
    );
    assert!(
        text.contains("Line two"),
        "Should contain 'Line two', got: {}",
        text
    );

    // Verify the Y gap is ~13pt (1.3 * 10), not 1.3pt
    let line_one_y = chars.iter().find(|c| c.char == 'L').unwrap().bbox.y;
    let line_two_chars: Vec<_> = chars.iter().filter(|c| c.char == 'L').collect();
    assert!(
        line_two_chars.len() >= 2,
        "Should have at least 2 'L' chars (one per line)"
    );
    let line_two_y = line_two_chars[1].bbox.y;
    let y_gap = (line_one_y - line_two_y).abs();
    assert!(
        y_gap > 5.0,
        "Y gap should be ~13pt (Tm-scaled), got {:.1}pt",
        y_gap
    );
}

#[test]
fn test_operator_td_sets_leading() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // TD sets leading = -ty, then positions text
    let stream = b"BT /F1 12 Tf 100 -14 TD (A) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert_eq!(chars[0].char, 'A');
}

#[test]
fn test_operator_tstar_line_break() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // TL sets leading, then T* moves to next line using leading
    let stream = b"BT /F1 12 Tf 14 TL 100 700 Td (A) Tj T* (B) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 2);
    assert_eq!(chars[0].char, 'A');
    assert_eq!(chars[1].char, 'B');
    // B should be on a different line (different Y)
    assert!((chars[0].bbox.y - chars[1].bbox.y).abs() > 1.0);
}

#[test]
fn test_operator_quote_next_line_show_text() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // ' operator: T* + Tj combined
    let stream = b"BT /F1 12 Tf 14 TL 100 700 Td (A) Tj (B) ' ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 2);
    assert_eq!(chars[0].char, 'A');
    assert_eq!(chars[1].char, 'B');
}

#[test]
fn test_operator_double_quote() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // " operator: set word/char spacing, T*, Tj
    let stream = b"BT /F1 12 Tf 14 TL 100 700 Td 1 2 (Hi) \" ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 2);
    assert_eq!(chars[0].char, 'H');
    assert_eq!(chars[1].char, 'i');
}

#[test]
fn test_operator_tc_char_spacing() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 2 Tc 100 700 Td (AB) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 2);
    assert_eq!(chars[0].char, 'A');
    assert_eq!(chars[1].char, 'B');
}

#[test]
fn test_operator_tw_word_spacing() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 5 Tw 100 700 Td (A B) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert!(chars.len() >= 3); // A, space, B
}

#[test]
fn test_operator_tz_horizontal_scaling() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 150 Tz 100 700 Td (X) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert_eq!(chars[0].char, 'X');
}

#[test]
fn test_operator_tl_leading() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 20 TL 100 700 Td (A) Tj T* (B) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 2);
    // A and B should be 20pt apart vertically (the leading value)
    let y_diff = (chars[0].bbox.y - chars[1].bbox.y).abs();
    assert!(
        y_diff > 10.0,
        "Leading should create vertical gap, got {}",
        y_diff
    );
}

#[test]
fn test_operator_ts_text_rise() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Ts sets text rise (superscript/subscript)
    let stream = b"BT /F1 12 Tf 5 Ts 100 700 Td (X) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert_eq!(chars[0].char, 'X');
}

#[test]
fn test_operator_tr_render_mode() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Tr sets rendering mode
    let stream = b"BT /F1 12 Tf 1 Tr 100 700 Td (X) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert_eq!(chars[0].char, 'X');
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Color operators
// ========================================================================

#[test]
fn test_set_fill_rgb() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT 0.5 0.3 0.8 rg /F1 12 Tf 0 0 Td (C) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert!((chars[0].color.r - 0.5).abs() < 0.01);
    assert!((chars[0].color.g - 0.3).abs() < 0.01);
    assert!((chars[0].color.b - 0.8).abs() < 0.01);
}
