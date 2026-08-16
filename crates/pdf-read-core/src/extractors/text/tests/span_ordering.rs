use super::*;

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
// COVERAGE TESTS: Sort spans reading order (single vs multi column)
// ========================================================================

#[test]
fn test_sort_spans_single_column() {
    let mut extractor = TextExtractor::new();
    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "Line2".to_string(),
            bbox: Rect::new(50.0, 680.0, 100.0, 12.0),
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
            text: "Line1".to_string(),
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

    extractor.sort_spans_by_reading_order();
    assert_eq!(extractor.spans[0].text, "Line1"); // higher Y first
    assert_eq!(extractor.spans[1].text, "Line2");
}

/// A scanned vertical-CJK OCR layer can emit hundreds of single-glyph
/// `wmode=1` spans whose X-centers step by a fraction of the median
/// span width: every adjacent pair looks "same column" under a pairwise
/// `|a - b| <= tol` check, but the first and last span are hundreds of
/// points apart, so the comparator claims contradictory orderings
/// (A<B, B<C, C<A) and Rust's `sort_by` panics with "does not correctly
/// implement a total order" instead of returning a reading order.
#[test]
fn test_sort_spans_vertical_tategaki_chained_x_centers_does_not_panic() {
    let mut extractor = TextExtractor::new();
    extractor.spans = (0..240)
        .map(|i| TextSpan {
            text: format!("g{i}"),
            bbox: Rect::new(
                20.0 + i as f32 * 0.8,
                700.0 - ((i * 37) % 96) as f32 * 7.0,
                1.0,
                12.0,
            ),
            font_size: 12.0,
            wmode: 1,
            ..TextSpan::default()
        })
        .collect();

    extractor.sort_spans_by_reading_order(); // must not panic
    assert_eq!(extractor.spans.len(), 240);
}
