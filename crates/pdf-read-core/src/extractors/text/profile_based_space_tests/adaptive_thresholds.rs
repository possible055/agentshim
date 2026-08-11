use super::*;

#[test]
fn test_no_decimal_merge_for_non_digit_spans() {
    // Should NOT merge "Hello" + "72" as decimal
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "Hello".to_string(),
            bbox: Rect::new(382.3, 700.0, 39.6, 12.0),
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
            text: "72".to_string(),
            bbox: Rect::new(432.7, 700.0, 13.2, 12.0), // 10.8pt gap
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
    // Should NOT merge because first span is not all digits
    assert_eq!(
        extractor.spans.len(),
        2,
        "Non-digit spans should not be merged as decimal values"
    );
}

#[test]
fn test_no_decimal_merge_for_long_decimal_part() {
    // Should NOT merge "123456" + "723" (3-digit decimal part is not a cents pattern)
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "123456".to_string(),
            bbox: Rect::new(382.3, 700.0, 39.6, 12.0),
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
            text: "723".to_string(),
            bbox: Rect::new(432.7, 700.0, 18.0, 12.0), // 10.8pt gap
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
    // Should NOT merge because decimal part has 3 digits (not a cents pattern)
    assert_eq!(
        extractor.spans.len(),
        2,
        "3-digit decimal part should not trigger decimal merge"
    );
}

#[test]
fn test_no_decimal_merge_with_intervening_comma_glyph() {
    // Subscript index pairs (`P_{1,0}`, `i_2, i_4`) place two small
    // digit runs a split-box-sized gap apart WITH the separating comma
    // drawn between them — often later in the content stream, so the
    // digit spans are sequence-adjacent. Ink inside the gap proves the
    // digits are separate tokens: a genuine split-box amount has empty
    // space between its boxes. Gap here: 110.0 - 103.5 = 6.5pt at 7pt
    // font = 0.93x — squarely inside the genuine split-box band, so a
    // gap ceiling alone cannot reject it.
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    extractor.spans = vec![
        digit_test_span("1", Rect::new(100.0, 700.0, 3.5, 7.0), 7.0),
        digit_test_span("0", Rect::new(110.0, 700.0, 3.5, 7.0), 7.0),
        // The comma, drawn after both digits in the content stream but
        // sitting geometrically inside the gap.
        digit_test_span(",", Rect::new(104.8, 699.0, 1.8, 3.0), 7.0),
    ];

    extractor.merge_adjacent_spans();
    assert!(
        !extractor.spans.iter().any(|s| s.text.contains("1.0")),
        "digits separated by a drawn comma must not merge into a decimal, got {:?}",
        extractor
            .spans
            .iter()
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_no_decimal_merge_across_font_sizes() {
    // Scientific notation in table rows: the exponent digit of one
    // value ("10^-4" drawn as "10" + superscript "4") and the mantissa
    // digit of the NEXT value ("3 ...") are both pure-digit runs a
    // split-box-sized gap apart, and were fused into a fabricated
    // decimal ("4 . 10-4 3 . 10-4" -> "4 . 10-4.3 . 10-4"). A genuine
    // split-box amount prints both halves at the SAME size; an
    // exponent is markedly smaller than the neighbouring mantissa, so
    // a size mismatch disqualifies the pair.
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    extractor.spans = vec![
        // Exponent "4" of the previous value: 7pt.
        digit_test_span("4", Rect::new(200.0, 700.0, 4.0, 7.0), 7.0),
        // Mantissa "3" of the next value: 12pt, 8pt away (0.67-1.14x
        // either font size -- inside the merge band for both).
        digit_test_span("3", Rect::new(212.0, 700.0, 6.5, 12.0), 12.0),
    ];

    extractor.merge_adjacent_spans();
    assert!(
        !extractor.spans.iter().any(|s| s.text.contains('.')),
        "digit runs at mismatched font sizes must not merge into a decimal, got {:?}",
        extractor
            .spans
            .iter()
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_decimal_merge_with_ink_elsewhere_on_line_still_joins() {
    // Positive control for the intervening-ink test: ink elsewhere on
    // the same line (a comma before the amount) must not block a
    // genuine split-box merge — only ink INSIDE the gap counts.
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    extractor.spans = vec![
        digit_test_span("123456", Rect::new(382.3, 700.0, 39.6, 12.0), 12.0),
        digit_test_span("72", Rect::new(432.7, 700.0, 13.2, 12.0), 12.0), // 10.8pt gap
        digit_test_span(",", Rect::new(300.0, 700.0, 2.5, 4.0), 12.0),    // far left of both
    ];

    extractor.merge_adjacent_spans();
    assert!(
        extractor.spans.iter().any(|s| s.text == "123456.72"),
        "split-box amount must still merge when the line's other ink is outside the gap, got {:?}",
        extractor
            .spans
            .iter()
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_no_decimal_merge_for_wide_subscript_digits() {
    // Subscript index pairs (e.g. `P_{1,0}` in scientific PDFs) draw the
    // two subscript digits in a smaller font (~7pt) spaced far apart
    // (~1.5-1.7x the font size). The decimal-merge rule was joining them
    // into an invented decimal ("1" + "0" -> "1.0"). A real split-box
    // dollar amount clusters near ~0.8-1.0x the font size, so a wide gap is
    // not an integer/cents amount and must stay separate.
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    // 7pt subscript digits: "1" at x=100.0 (w=3.5), "0" at x=114.5 (w=3.5).
    // gap = 114.5 - (100.0 + 3.5) = 11.0pt -> 11.0 / 7.0 = 1.57x font size.
    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "1".to_string(),
            bbox: Rect::new(100.0, 700.0, 3.5, 7.0),
            font_name: "F1".to_string(),
            font_size: 7.0,
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
            text: "0".to_string(),
            bbox: Rect::new(114.5, 700.0, 3.5, 7.0), // 11.0pt gap = 1.57x font
            font_name: "F1".to_string(),
            font_size: 7.0,
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
    // Widely-spaced subscript digits must NOT be joined into a decimal.
    assert_eq!(
        extractor.spans.len(),
        2,
        "Widely-spaced subscript digits should not merge into a decimal value"
    );
    assert!(
        !extractor.spans.iter().any(|s| s.text.contains('.')),
        "No invented decimal point should appear between subscript digits"
    );
}

#[test]
fn test_decimal_merge_just_under_ceiling_still_joins() {
    // The ceiling that stops subscripts must not be so tight that it drops
    // genuine split-box amounts. Real amounts cluster near ~0.8-1.0x the
    // font size; this locks the ceiling by proving an amount at ~1.2x the
    // font size (just under the 1.3x cap) still merges.
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    // 12pt digits: "1234" at x=200.0 (w=24.0), "56" at x=238.4 (w=12.0).
    // gap = 238.4 - (200.0 + 24.0) = 14.4pt -> 14.4 / 12.0 = 1.2x font size.
    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "1234".to_string(),
            bbox: Rect::new(200.0, 700.0, 24.0, 12.0),
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
            text: "56".to_string(),
            bbox: Rect::new(238.4, 700.0, 12.0, 12.0), // 14.4pt gap = 1.2x font
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
        "Amount just under the ceiling should still merge"
    );
    assert_eq!(extractor.spans[0].text, "1234.56");
}

#[test]
fn test_cross_font_word_glue_single_letter_prefix() {
    // A single-letter span in one font, tight-kerned against a
    // multi-letter span in another font, is the drop-cap pattern.
    // These must merge into one word with the longer run's font
    // metadata — emitting per-letter emphasis runs corrupts proper
    // nouns.
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::default();

    extractor.spans = vec![
        TextSpan {
            text: "S".to_string(),
            bbox: Rect::new(72.0, 700.0, 10.0, 12.0),
            font_name: "Helvetica-Bold".to_string(),
            font_weight: FontWeight::Bold,
            font_size: 12.0,
            ..TextSpan::default()
        },
        TextSpan {
            text: "ales".to_string(),
            bbox: Rect::new(82.0, 700.0, 30.0, 12.0),
            font_name: "Helvetica".to_string(),
            font_weight: FontWeight::Normal,
            font_size: 12.0,
            ..TextSpan::default()
        },
    ];

    extractor.merge_adjacent_spans();

    assert_eq!(
        extractor.spans.len(),
        1,
        "cross_font_word_glue should merge 'S' + 'ales' into 'Sales'"
    );
    assert_eq!(extractor.spans[0].text, "Sales");
    // Dominant-font swap: the longer run (regular weight) should win.
    assert_eq!(extractor.spans[0].font_weight, FontWeight::Normal);
}

/// The advance-fold folds a sub-threshold TJ offset into the run's stored
/// advance using the exact ISO 32000-1 §9.4.4 displacement
/// (`-Tj/1000 * Tfs * Th`), keeping `char_widths.last` and
/// `accumulated_width` in lockstep so the reconstructed geometry equals the
/// text-matrix position. An empty buffer is a no-op (the next glyph
/// re-anchors to the matrix).
#[test]
fn test_fold_offset_into_buffer_matches_spec_displacement() {
    let mut extractor = TextExtractor::new();
    {
        let st = extractor.state_stack.current_mut();
        st.font_size = 10.0;
        st.horizontal_scaling = 100.0; // Th = 1.0
    }
    let mut buffer = TjBuffer::new(extractor.state_stack.current(), None, None);
    buffer.char_widths.push(5.0);
    buffer.accumulated_width = 5.0;

    // -120 TJ units => -(-120)/1000 * 10 * (100/100) = 1.2 (text space).
    extractor.fold_offset_into_buffer(&mut buffer, -120.0);
    let expected = 1.2_f32;
    assert!((buffer.char_widths.last().unwrap() - (5.0 + expected)).abs() < 1e-4);
    assert!((buffer.accumulated_width - (5.0 + expected)).abs() < 1e-4);
    // Invariant: sum(char_widths) == accumulated_width by construction.
    let sum: f32 = buffer.char_widths.iter().sum();
    assert!((sum - buffer.accumulated_width).abs() < 1e-4);

    // Empty buffer: nothing to fold into, must not panic or fabricate width.
    let mut empty = TjBuffer::new(extractor.state_stack.current(), None, None);
    extractor.fold_offset_into_buffer(&mut empty, -120.0);
    assert!(empty.char_widths.is_empty());
    assert_eq!(empty.accumulated_width, 0.0);
}

/// The cross-font glue ceiling (0.12em) must NOT glue a real word followed
/// by a single-letter variable set in a different font run across a
/// word-space gap (roman `solution` -> math-italic `U`, gap ~0.24em). This
/// is the mirror of the drop-cap case above (gap ~0): a word space is a
/// genuine boundary poppler/PDFium keep, so the two spans stay separate.
#[test]
fn test_cross_font_word_variable_not_glued() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::default();
    // fs 10; "solution" ends at x=112, "U" starts at x=114.4 => gap 2.4pt = 0.24em.
    extractor.spans = vec![
        TextSpan {
            text: "solution".to_string(),
            bbox: Rect::new(72.0, 700.0, 40.0, 10.0),
            font_name: "NimbusRomNo9L-Regu".to_string(),
            font_size: 10.0,
            ..TextSpan::default()
        },
        TextSpan {
            text: "U".to_string(),
            bbox: Rect::new(114.4, 700.0, 7.0, 10.0),
            font_name: "NimbusRomNo9L-Ital".to_string(),
            font_size: 10.0,
            ..TextSpan::default()
        },
    ];
    extractor.merge_adjacent_spans();
    assert_eq!(
        extractor.spans.len(),
        2,
        "a 0.24em word-space gap across a font change must NOT glue (drop-cap glue is for ~0 gaps)"
    );
}
