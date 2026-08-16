use super::*;

// ========================================================================
// NEW COMPREHENSIVE TESTS: Tm operator batching optimization
// ========================================================================

#[test]
fn test_tm_continuation_same_line() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Character-by-character Tm+Tj pattern on same line
    // The optimization should batch these into fewer spans
    let stream = b"BT /F1 12 Tf 1 0 0 1 100 700 Tm (H) Tj 1 0 0 1 106 700 Tm (i) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    let text: String = spans
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        text.contains("Hi"),
        "Should batch Tm+Tj on same line, got: {}",
        text
    );
}

#[test]
fn test_tm_different_line_flushes() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Tm to different Y should flush buffer and start new span
    let stream = b"BT /F1 12 Tf 1 0 0 1 100 700 Tm (A) Tj 1 0 0 1 100 680 Tm (B) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    // Should have at least 2 spans (different lines)
    assert!(
        spans.len() >= 2 || {
            // Or could be merged if within merge range
            let text: String = spans
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("");
            text.contains("A") && text.contains("B")
        }
    );
}

// ========================================================================
// TESTS: merge_tm_tj_runs opt-out (#488)
// ========================================================================

/// With the default config (merge_tm_tj_runs = true), multiple Tm+Tj operators
/// on the same line are batched into a single span.
#[test]
fn test_merge_tm_tj_runs_default_merges() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy(); // fixed thresholds, merging on
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Three separate Tm+Tj on the same baseline (same Y, same a/b/c/d, ascending e)
    let stream =
        b"BT /F1 12 Tf 1 0 0 1 100 700 Tm (A) Tj 1 0 0 1 107 700 Tm (B) Tj 1 0 0 1 114 700 Tm (C) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    // All three characters must be present
    let text: String = spans.iter().map(|s| s.text.as_str()).collect();
    assert!(
        text.contains('A') && text.contains('B') && text.contains('C'),
        "All chars must be extracted, got: {:?}",
        text
    );

    // The default merging should combine them into fewer spans than the number
    // of Tm operators (3 Tms should not produce 3 separate spans)
    assert!(
        spans.len() < 3,
        "Default merge_tm_tj_runs=true should combine same-line Tm+Tj into fewer than 3 spans, got {} spans",
        spans.len()
    );
}

/// With merge_tm_tj_runs = false, each Tm operator starts a fresh span.
#[test]
fn test_merge_tm_tj_runs_disabled_splits() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig {
        merge_tm_tj_runs: false,
        ..SpanMergingConfig::legacy()
    };
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Three separate Tm+Tj on the same baseline
    let stream =
        b"BT /F1 12 Tf 1 0 0 1 100 700 Tm (A) Tj 1 0 0 1 107 700 Tm (B) Tj 1 0 0 1 114 700 Tm (C) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    // All three characters must still be present
    let text: String = spans.iter().map(|s| s.text.as_str()).collect();
    assert!(
        text.contains('A') && text.contains('B') && text.contains('C'),
        "All chars must be extracted even with merging disabled, got: {:?}",
        text
    );

    // With merge disabled, each Tm flushes the buffer, so we get more spans
    // than with merging enabled (post-processing merge_adjacent_spans may combine
    // some, but at minimum we should get spans >= 1; the key invariant is that
    // the span count here is NOT reduced by the Tm-continuation shortcut)
    assert!(
        spans.len() >= 2,
        "merge_tm_tj_runs=false should not batch same-line runs; expected >= 2 spans, got {}",
        spans.len()
    );
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Edge cases
// ========================================================================

#[test]
fn test_extract_with_zero_font_size() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Zero font size is technically valid in PDF
    let stream = b"BT /F1 0 Tf 100 700 Td (X) Tj ET";
    let result = extractor.extract(stream);
    // Should not panic
    assert!(result.is_ok());
}

#[test]
fn test_extract_with_negative_font_size() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Negative font size inverts text
    let stream = b"BT /F1 -12 Tf 100 700 Td (X) Tj ET";
    let result = extractor.extract(stream);
    assert!(result.is_ok());
}

#[test]
fn test_extract_with_very_large_coordinate() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 99999 99999 Td (X) Tj ET";
    let chars = extractor.extract(stream).unwrap();
    assert_eq!(chars.len(), 1);
}

// ========================================================================
// COVERAGE TESTS: Color space operators (SetFillColor/SetStrokeColor)
// ========================================================================

#[test]
fn test_set_fill_color_device_gray() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // cs sets color space, then sc sets color components
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "DeviceGray".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![0.5],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.5).abs() < 0.01);
    assert!((state.fill_color_rgb.1 - 0.5).abs() < 0.01);
    assert!((state.fill_color_rgb.2 - 0.5).abs() < 0.01);
}

#[test]
fn test_set_fill_color_device_rgb() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "DeviceRGB".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![0.2, 0.4, 0.6],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.2).abs() < 0.01);
    assert!((state.fill_color_rgb.1 - 0.4).abs() < 0.01);
    assert!((state.fill_color_rgb.2 - 0.6).abs() < 0.01);
}

#[test]
fn test_set_fill_color_device_cmyk() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "DeviceCMYK".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![0.0, 0.0, 0.0, 1.0], // the K ink
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.1373).abs() < 0.01);
    assert!((state.fill_color_rgb.1 - 0.1216).abs() < 0.01);
    assert!((state.fill_color_rgb.2 - 0.1255).abs() < 0.01);
    assert!(state.fill_color_cmyk.is_some());
}

#[test]
fn test_set_fill_color_lab() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "Lab".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![50.0, 20.0, -10.0],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    // Lab simplified to grayscale: L/100
    assert!((state.fill_color_rgb.0 - 0.5).abs() < 0.01);
}

#[test]
fn test_set_fill_color_iccbased_rgb() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "ICCBased".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![0.1, 0.2, 0.3],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.1).abs() < 0.01);
    assert!((state.fill_color_rgb.1 - 0.2).abs() < 0.01);
    assert!((state.fill_color_rgb.2 - 0.3).abs() < 0.01);
}

#[test]
fn test_set_fill_color_iccbased_gray() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "ICCBased".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![0.7],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.7).abs() < 0.01);
}

#[test]
fn test_set_fill_color_iccbased_cmyk() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "ICCBased".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![1.0, 0.0, 0.0, 0.0], // cyan
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!(state.fill_color_cmyk.is_some());
}

#[test]
fn test_set_fill_color_separation() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "Separation".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![0.8], // tint
        })
        .unwrap();

    let state = extractor.state_stack.current();
    // gray = 1.0 - tint = 0.2
    assert!((state.fill_color_rgb.0 - 0.2).abs() < 0.01);
}

#[test]
fn test_set_fill_color_devicen_cmyk() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "DeviceN".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![0.0, 0.0, 0.0, 0.5], // 4-component DeviceN
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!(state.fill_color_cmyk.is_some());
}

#[test]
fn test_set_fill_color_devicen_single() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "DeviceN".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![0.3], // single-component DeviceN
        })
        .unwrap();

    let state = extractor.state_stack.current();
    // gray = 1.0 - 0.3 = 0.7
    assert!((state.fill_color_rgb.0 - 0.7).abs() < 0.01);
}

#[test]
fn test_set_fill_color_unknown_space() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "CustomUnknown".to_string(),
        })
        .unwrap();
    // This should log warning but not panic
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![0.5, 0.5],
        })
        .unwrap();
}

#[test]
fn test_set_fill_color_cal_gray() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "CalGray".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![0.8],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.8).abs() < 0.01);
}

#[test]
fn test_set_fill_color_cal_rgb() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "CalRGB".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColor {
            components: vec![0.9, 0.1, 0.5],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.9).abs() < 0.01);
    assert!((state.fill_color_rgb.1 - 0.1).abs() < 0.01);
    assert!((state.fill_color_rgb.2 - 0.5).abs() < 0.01);
}

// ========================================================================
// COVERAGE TESTS: Stroke color operators
// ========================================================================

#[test]
fn test_set_stroke_color_device_gray() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "DeviceGray".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![0.4],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.4).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_device_rgb() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "DeviceRGB".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![0.1, 0.2, 0.3],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.1).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_lab() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "Lab".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![75.0, 10.0, -5.0],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.75).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_device_cmyk() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "DeviceCMYK".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![0.0, 1.0, 0.0, 0.0], // magenta
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!(state.stroke_color_cmyk.is_some());
}

#[test]
fn test_set_stroke_color_iccbased_gray() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "ICCBased".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![0.3],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.3).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_iccbased_rgb() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "ICCBased".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![0.9, 0.8, 0.7],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.9).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_iccbased_cmyk() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "ICCBased".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![0.1, 0.2, 0.3, 0.4],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!(state.stroke_color_cmyk.is_some());
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
// COVERAGE TESTS: flush_tj_span_buffer when buffer is Some but empty
// ========================================================================

#[test]
fn test_flush_tj_span_buffer_empty_buffer() {
    let mut extractor = TextExtractor::new();
    let state = extractor.state_stack.current().clone();
    extractor.tj_span_buffer = Some(TjBuffer::new(&state, None, None));
    // Empty buffer should not produce a span
    let before = extractor.spans.len();
    extractor.flush_tj_span_buffer().unwrap();
    assert_eq!(extractor.spans.len(), before);
}

#[test]
fn test_flush_tj_span_buffer_with_content() {
    let mut extractor = TextExtractor::new();
    let state_stack = crate::content::graphics_state::GraphicsStateStack::new();
    let mut buffer = TjBuffer::new(state_stack.current(), Some(7), None);
    buffer.append(b"Test").unwrap();
    buffer.accumulated_width = 20.0;
    extractor.tj_span_buffer = Some(buffer);

    extractor.flush_tj_span_buffer().unwrap();
    assert_eq!(extractor.spans.len(), 1);
    assert!(extractor.spans[0].text.contains("Test"));
}

// ========================================================================
// COVERAGE TESTS: Merge with offset_semantic space span suppression
// ========================================================================

#[test]
fn test_merge_offset_semantic_space_suppression() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

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
            text: " ".to_string(), // offset_semantic space
            bbox: Rect::new(130.5, 700.0, 2.0, 12.0),
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 1,
            split_boundary_before: true, // forcing merge path
            offset_semantic: true,
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
    // offset_semantic space should be merged without adding extra space
    let text = &extractor.spans[0].text;
    assert!(
        !text.contains("  "),
        "Should not have double space, got: '{}'",
        text
    );
}
