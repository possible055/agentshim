use super::*;

#[test]
fn test_deduplicate_spans_still_collapses_stroke_fill_narrow_glyphs() {
    // Positive regression: stroke+fill single-glyph narrow spans at
    // ~0 pt offset must still be collapsed by the geometric dedup
    // phase. The ratio (0.30) comfortably catches real duplicates
    // while preserving legitimate doublets.
    let mut extractor = TextExtractor::new();

    let narrow_at = |x: f32, seq: usize| TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: "l".to_string(),
        bbox: Rect::new(x, 700.0, 2.5, 9.0),
        font_name: "Helvetica".to_string(),
        font_size: 9.0,
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

    // Stroke pass + fill pass at ~2 % of advance apart.
    extractor.spans = vec![narrow_at(100.0, 0), narrow_at(100.05, 1)];

    extractor.deduplicate_overlapping_spans();
    assert_eq!(
        extractor.spans.len(),
        1,
        "Stroke+fill narrow-glyph duplicate spans (same text at ~0 pt offset) \
         must still be collapsed"
    );
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Column detection
// ========================================================================

#[test]
fn test_detect_span_columns_empty() {
    let extractor = TextExtractor::new();
    let columns = extractor.detect_span_columns();
    assert!(columns.is_empty());
}

#[test]
fn test_detect_span_columns_single_column() {
    let mut extractor = TextExtractor::new();
    // Create spans all in one column
    for i in 0..10 {
        extractor.spans.push(TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: format!("Line {}", i),
            bbox: Rect::new(50.0, 700.0 - (i as f32 * 14.0), 200.0, 12.0),
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: i,
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
        });
    }

    let columns = extractor.detect_span_columns();
    assert_eq!(columns.len(), 1, "Should detect single column");
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Sort by reading order
// ========================================================================

#[test]
fn test_sort_by_reading_order() {
    let mut extractor = TextExtractor::new();
    // Add chars in wrong order
    extractor.chars = vec![
        TextChar {
            char: 'B',
            bbox: Rect::new(100.0, 680.0, 6.0, 12.0), // Lower on page
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
        TextChar {
            char: 'A',
            bbox: Rect::new(100.0, 700.0, 6.0, 12.0), // Higher on page
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
    ];

    extractor.sort_by_reading_order();
    // PDF Y increases upward, so 700 is higher than 680
    // Reading order: top first, so A (y=700) before B (y=680)
    assert_eq!(extractor.chars[0].char, 'A');
    assert_eq!(extractor.chars[1].char, 'B');
}

#[test]
fn test_sort_by_reading_order_same_line() {
    let mut extractor = TextExtractor::new();
    extractor.chars = vec![
        TextChar {
            char: 'B',
            bbox: Rect::new(200.0, 700.0, 6.0, 12.0), // Right
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            is_italic: false,
            is_monospace: false,
            origin_x: 200.0,
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
            bbox: Rect::new(100.0, 700.0, 6.0, 12.0), // Left
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
    ];

    extractor.sort_by_reading_order();
    // Same line: left to right
    assert_eq!(extractor.chars[0].char, 'A');
    assert_eq!(extractor.chars[1].char, 'B');
}

#[test]
fn test_sort_by_reading_order_nan_values() {
    let mut extractor = TextExtractor::new();
    extractor.chars = vec![
        TextChar {
            char: 'A',
            bbox: Rect::new(f32::NAN, f32::NAN, 6.0, 12.0),
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            is_italic: false,
            is_monospace: false,
            origin_x: 0.0,
            origin_y: 0.0,
            rotation_degrees: 0.0,
            advance_width: 6.0,
            rendered_advance: 6.0,
            ascent: 11.4,
            descent: -4.2,
            matrix: None,
        },
        TextChar {
            char: 'B',
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
    ];

    // Should not panic with NaN values
    extractor.sort_by_reading_order();
    assert_eq!(extractor.chars.len(), 2);
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: merge_adjacent_spans
// ========================================================================

#[test]
fn test_merge_adjacent_spans_same_line() {
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
            text: "World".to_string(),
            bbox: Rect::new(131.0, 700.0, 30.0, 12.0), // 1pt gap
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

    extractor.merge_adjacent_spans();
    assert_eq!(
        extractor.spans.len(),
        1,
        "Adjacent spans on same line should merge"
    );
    assert!(extractor.spans[0].text.contains("Hello"));
    assert!(extractor.spans[0].text.contains("World"));
}

#[test]
fn test_merge_adjacent_spans_different_lines() {
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
            text: "World".to_string(),
            bbox: Rect::new(100.0, 680.0, 30.0, 12.0), // Different line
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

    extractor.merge_adjacent_spans();
    assert_eq!(
        extractor.spans.len(),
        2,
        "Spans on different lines should not merge"
    );
}

#[test]
fn test_merge_adjacent_spans_empty() {
    let mut extractor = TextExtractor::new();
    extractor.merge_adjacent_spans();
    assert!(extractor.spans.is_empty());
}

#[test]
fn test_merge_adjacent_spans_column_boundary() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "Left".to_string(),
            bbox: Rect::new(50.0, 700.0, 30.0, 12.0),
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
            text: "Right".to_string(),
            bbox: Rect::new(300.0, 700.0, 30.0, 12.0), // Large gap (column boundary)
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

    extractor.merge_adjacent_spans();
    assert_eq!(
        extractor.spans.len(),
        2,
        "Spans separated by column boundary should not merge"
    );
}

#[test]
fn test_merge_whitespace_only_span() {
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
            text: " ".to_string(),
            bbox: Rect::new(130.0, 700.0, 2.0, 12.0),
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 1,
            split_boundary_before: false,
            offset_semantic: true, // TJ offset space
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
            text: "World".to_string(),
            bbox: Rect::new(132.0, 700.0, 30.0, 12.0),
            font_name: "F1".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 2,
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

    extractor.merge_adjacent_spans();
    assert_eq!(extractor.spans.len(), 1, "All three spans should merge");
    assert!(
        extractor.spans[0].text.contains("Hello"),
        "Should contain Hello"
    );
    assert!(
        extractor.spans[0].text.contains("World"),
        "Should contain World"
    );
}
