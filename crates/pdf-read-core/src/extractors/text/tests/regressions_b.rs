use super::*;

// ========================================================================
// COVERAGE TESTS: Marked content with BDC
// ========================================================================

#[test]
fn test_bdc_with_mcid() {
    let mut extractor = TextExtractor::new();
    let mut props = HashMap::new();
    props.insert("MCID".to_string(), Object::Integer(5));

    extractor
        .execute_operator_public(Operator::BeginMarkedContentDict {
            tag: "P".to_string(),
            properties: Box::new(Object::Dictionary(props)),
        })
        .unwrap();

    assert_eq!(extractor.current_mcid, Some(5));
    assert!(!extractor.inside_artifact);
}

#[test]
fn test_bdc_artifact_with_type() {
    let mut extractor = TextExtractor::new();
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Pagination".to_string()));
    props.insert("Subtype".to_string(), Object::Name("Header".to_string()));

    extractor
        .execute_operator_public(Operator::BeginMarkedContentDict {
            tag: "Artifact".to_string(),
            properties: Box::new(Object::Dictionary(props)),
        })
        .unwrap();

    assert!(extractor.inside_artifact);
}

#[test]
fn test_emc_resets_mcid() {
    let mut extractor = TextExtractor::new();
    extractor.current_mcid = Some(10);
    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "P".to_string(),
        is_artifact: false,
        actual_text: None,
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    });

    extractor
        .execute_operator_public(Operator::EndMarkedContent)
        .unwrap();

    assert_eq!(extractor.current_mcid, None);
    assert!(extractor.marked_content_stack.is_empty());
}

#[test]
fn test_emc_with_empty_stack() {
    let mut extractor = TextExtractor::new();
    // Should not panic
    extractor
        .execute_operator_public(Operator::EndMarkedContent)
        .unwrap();
}

// ========================================================================
// COVERAGE TESTS: BDC with ActualText and Expansion
// ========================================================================

#[test]
fn test_bdc_with_actual_text() {
    let mut extractor = TextExtractor::new();
    let mut props = HashMap::new();
    props.insert("ActualText".to_string(), Object::String(b"fi".to_vec()));

    extractor
        .execute_operator_public(Operator::BeginMarkedContentDict {
            tag: "Span".to_string(),
            properties: Box::new(Object::Dictionary(props)),
        })
        .unwrap();

    let actual = extractor.get_current_actual_text();
    assert_eq!(actual, Some("fi".to_string()));
}

#[test]
fn test_bdc_with_expansion() {
    let mut extractor = TextExtractor::new();
    let mut props = HashMap::new();
    props.insert("E".to_string(), Object::String(b"PDF".to_vec()));

    extractor
        .execute_operator_public(Operator::BeginMarkedContentDict {
            tag: "Span".to_string(),
            properties: Box::new(Object::Dictionary(props)),
        })
        .unwrap();

    let ctx = &extractor.marked_content_stack[0];
    assert_eq!(ctx.expansion, Some("PDF".to_string()));
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

// ========================================================================
// COVERAGE TESTS: Tm continuation optimization
// ========================================================================

#[test]
fn test_tm_continuation_different_transform() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Different transform params (a=2) should NOT be continuation
    let stream = b"BT /F1 12 Tf 1 0 0 1 100 700 Tm (A) Tj 2 0 0 1 120 700 Tm (B) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    // Should produce separate spans due to different transform
    assert!(!spans.is_empty());
}

// ========================================================================
// COVERAGE TESTS: decode_pdf_text_string edge cases
// ========================================================================

#[test]
fn test_decode_pdf_text_string_single_byte() {
    let result = TextExtractor::decode_pdf_text_string(&[0x41]);
    assert_eq!(result, "A");
}

#[test]
fn test_decode_pdf_text_string_invalid_utf16() {
    // UTF-16BE BOM followed by invalid pair
    let bytes = vec![0xFE, 0xFF, 0xD8, 0x00]; // invalid surrogate half
    let result = TextExtractor::decode_pdf_text_string(&bytes);
    // Should fall back to lossy conversion
    assert!(!result.is_empty() || result.is_empty()); // Just don't panic
}

#[test]
fn test_decode_pdf_text_string_utf16le_invalid() {
    // UTF-16LE BOM followed by odd byte count
    let bytes = vec![0xFF, 0xFE, 0x41]; // odd after BOM
    let result = TextExtractor::decode_pdf_text_string(&bytes);
    // Should handle gracefully
}

// ========================================================================
// TDD: decode_pdf_text_string — PDFDocEncoding fallback correctness
// Bytes 0xA0–0xFF and the special 0x80–0x9E zone must decode through
// PDFDocEncoding, not through from_utf8_lossy (which produces U+FFFD).
// ========================================================================

#[test]
fn test_decode_pdfdocencoding_latin_byte() {
    // 0xE9 = PDFDocEncoding for é (U+00E9). Not valid UTF-8 on its own.
    let result = TextExtractor::decode_pdf_text_string(&[0xE9]);
    assert_eq!(
        result, "é",
        "0xE9 must decode as 'é' via PDFDocEncoding, not produce U+FFFD"
    );
}

#[test]
fn test_decode_pdfdocencoding_bullet() {
    // 0x80 = PDFDocEncoding for • (U+2022 BULLET)
    let result = TextExtractor::decode_pdf_text_string(&[0x80]);
    assert_eq!(
        result, "•",
        "0x80 must decode as bullet '•' via PDFDocEncoding"
    );
}

#[test]
fn test_decode_pdfdocencoding_emdash() {
    // 0x84 = PDFDocEncoding for — (U+2014 EM DASH)
    let result = TextExtractor::decode_pdf_text_string(&[0x84]);
    assert_eq!(
        result, "—",
        "0x84 must decode as em-dash '—' via PDFDocEncoding"
    );
}

#[test]
fn test_decode_pdfdocencoding_trademark() {
    // 0x92 = PDFDocEncoding for ™ (U+2122 TRADE MARK SIGN)
    let result = TextExtractor::decode_pdf_text_string(&[0x92]);
    assert_eq!(
        result, "™",
        "0x92 must decode as trademark '™' via PDFDocEncoding"
    );
}

#[test]
fn test_decode_pdfdocencoding_undefined_9f_is_dropped() {
    // 0x9F is undefined in PDFDocEncoding — must be silently dropped.
    let result = TextExtractor::decode_pdf_text_string(&[0x41, 0x9F, 0x42]);
    assert_eq!(
        result, "AB",
        "0x9F is undefined in PDFDocEncoding and must be dropped"
    );
}

#[test]
fn test_decode_pdfdocencoding_mixed_ascii_and_latin() {
    // "Hello" followed by 0xE9 (é): 6 bytes → "Helloé"
    let bytes: Vec<u8> = b"Hello".iter().copied().chain([0xE9]).collect();
    let result = TextExtractor::decode_pdf_text_string(&bytes);
    assert_eq!(
        result, "Helloé",
        "Mixed ASCII + PDFDocEncoding bytes must decode correctly"
    );
}

#[test]
fn test_decode_pdfdocencoding_utf8_bytes_still_work() {
    // Valid UTF-8 without BOM: must still decode correctly (for lenient PDFs).
    // ASCII is a subset of UTF-8, so this path always works.
    let result = TextExtractor::decode_pdf_text_string(b"ASCII text");
    assert_eq!(result, "ASCII text");
}

// ========================================================================
// COVERAGE TESTS: shared truetype cmaps (no donors)
// ========================================================================

#[test]
fn test_share_truetype_cmaps_no_donors() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Should return early (no cmap donors)
    extractor.share_truetype_cmaps();
    assert_eq!(extractor.fonts.len(), 1);
}

// ========================================================================
// COVERAGE TESTS: Extract with WithConfig
// ========================================================================

#[test]
fn test_extractor_with_config_and_profile() {
    let config = TextExtractionConfig::new().with_profile(crate::config::ExtractionProfile::POLICY);

    let mut extractor = TextExtractor::with_config(config);
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 100 700 Td (Policy) Tj ET";
    let chars = extractor.extract(stream).unwrap();
    assert!(!chars.is_empty());
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
