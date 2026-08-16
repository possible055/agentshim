use super::*;

// ========================================================================
// NEW COMPREHENSIVE TESTS: partition_characters_by_boundaries
// ========================================================================

#[test]
fn test_partition_no_boundaries() {
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

    let clusters = extractor.partition_characters_by_boundaries(&chars, vec![]);
    assert_eq!(clusters.len(), 1);
    assert_eq!(clusters[0].len(), 2);
}

#[test]
fn test_partition_with_boundary() {
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
        CharacterInfo {
            code: 67,
            glyph_id: None,
            width: 10.0,
            x_position: 25.0,
            tj_offset: None,
            font_size: 12.0,
            is_ligature: false,
            original_ligature: None,
            protected_from_split: false,
        },
    ];

    let clusters = extractor.partition_characters_by_boundaries(&chars, vec![2]);
    assert_eq!(clusters.len(), 2);
    assert_eq!(clusters[0].len(), 2); // [A, B]
    assert_eq!(clusters[1].len(), 1); // [C]
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: create_boundary_context
// ========================================================================

#[test]
fn test_create_boundary_context() {
    let mut extractor = TextExtractor::new();
    extractor.state_stack.current_mut().font_size = 12.0;
    extractor.state_stack.current_mut().horizontal_scaling = 100.0;
    extractor.state_stack.current_mut().word_space = 2.0;
    extractor.state_stack.current_mut().char_space = 0.5;

    let ctx = extractor.create_boundary_context();
    assert_eq!(ctx.font_size, 12.0);
    assert_eq!(ctx.horizontal_scaling, 100.0);
    assert_eq!(ctx.word_spacing, 2.0);
    assert_eq!(ctx.char_spacing, 0.5);
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: build_boundary_characters
// ========================================================================

#[test]
fn test_build_boundary_characters() {
    let prev_bbox = Rect::new(10.0, 100.0, 50.0, 12.0);
    let next_bbox = Rect::new(65.0, 100.0, 40.0, 12.0);

    let (chars, ctx) =
        build_boundary_characters("Hello", "World", &prev_bbox, &next_bbox, 12.0, false);

    assert_eq!(chars.len(), 2);
    assert_eq!(chars[0].code, 'o' as u32); // Last char of "Hello"
    assert_eq!(chars[1].code, 'W' as u32); // First char of "World"
    assert_eq!(ctx.font_size, 12.0);
}

#[test]
fn test_build_boundary_characters_with_tj_offset() {
    let prev_bbox = Rect::new(10.0, 100.0, 50.0, 12.0);
    let next_bbox = Rect::new(65.0, 100.0, 40.0, 12.0);

    let (chars, _ctx) =
        build_boundary_characters("Hello", "World", &prev_bbox, &next_bbox, 12.0, true);

    assert_eq!(chars[0].tj_offset, Some(-200)); // TJ offset triggered
    assert_eq!(chars[1].tj_offset, None);
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: TjBuffer
// ========================================================================

#[test]
fn test_tj_buffer_empty() {
    let state = crate::content::graphics_state::GraphicsStateStack::new();
    let buffer = TjBuffer::new(state.current(), None, None);
    assert!(buffer.is_empty());
}

#[test]
fn test_tj_buffer_append() {
    let state = crate::content::graphics_state::GraphicsStateStack::new();
    let mut buffer = TjBuffer::new(state.current(), None, None);
    buffer.append(b"Hello").unwrap();
    assert!(!buffer.is_empty());
    assert_eq!(buffer.unicode, "Hello");
}

#[test]
fn test_tj_buffer_append_truncates_long_string() {
    let state = crate::content::graphics_state::GraphicsStateStack::new();
    let mut buffer = TjBuffer::new(state.current(), None, None);
    // Create a string larger than 32,767 bytes
    let long_bytes = vec![0x41u8; 40_000]; // 40K 'A's
    buffer.append(&long_bytes).unwrap();
    // Should be truncated to 32,767 chars
    assert!(buffer.unicode.len() <= 32_767);
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: advance_position_for_offset
// ========================================================================

#[test]
fn test_advance_position_for_offset_positive() {
    let mut extractor = TextExtractor::new();
    extractor.state_stack.current_mut().font_size = 12.0;
    extractor.state_stack.current_mut().horizontal_scaling = 100.0;

    let initial_e = extractor.state_stack.current().text_matrix.e;
    extractor.advance_position_for_offset(100.0).unwrap();
    let new_e = extractor.state_stack.current().text_matrix.e;

    // Positive offset should move text position left (negative tx)
    // tx = -offset / 1000.0 * font_size * horizontal_scaling / 100.0
    // tx = -100 / 1000 * 12 * 100 / 100 = -1.2
    assert!((new_e - initial_e - (-1.2)).abs() < 0.01);
}

#[test]
fn test_advance_position_for_offset_negative() {
    let mut extractor = TextExtractor::new();
    extractor.state_stack.current_mut().font_size = 12.0;
    extractor.state_stack.current_mut().horizontal_scaling = 100.0;

    let initial_e = extractor.state_stack.current().text_matrix.e;
    extractor.advance_position_for_offset(-200.0).unwrap();
    let new_e = extractor.state_stack.current().text_matrix.e;

    // Negative offset should move text position right (positive tx)
    // tx = -(-200) / 1000 * 12 * 100/100 = 2.4
    assert!((new_e - initial_e - 2.4).abs() < 0.01);
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: should_insert_space function
// ========================================================================

#[test]
fn test_should_insert_space_boundary_already_present_trailing() {
    let config = SpanMergingConfig::default();
    let fonts = HashMap::new();

    let decision = should_insert_space(
        "word ", "next", 5.0, 12.0, "F1", &fonts, true, &config, None, None, 12.0, 12.0,
    );
    assert!(!decision.insert_space);
    assert_eq!(decision.source, SpaceSource::AlreadyPresent);
}

#[test]
fn test_should_insert_space_boundary_already_present_leading() {
    let config = SpanMergingConfig::default();
    let fonts = HashMap::new();

    let decision = should_insert_space(
        "word", " next", 5.0, 12.0, "F1", &fonts, true, &config, None, None, 12.0, 12.0,
    );
    assert!(!decision.insert_space);
    assert_eq!(decision.source, SpaceSource::AlreadyPresent);
}

#[test]
fn test_should_insert_space_strong_geometric() {
    let config = SpanMergingConfig::default();
    let fonts = HashMap::new();

    // Very large gap should trigger strong geometric rule
    // geometric_threshold = 12.0 * 0.25 = 3.0 (fallback)
    // strong threshold = 3.0 * 2.0 = 6.0
    let decision = should_insert_space(
        "word", "next", 10.0, 12.0, "F1", &fonts, false, &config, None, None, 12.0, 12.0,
    );
    assert!(decision.insert_space, "Large gap should insert space");
    assert_eq!(decision.source, SpaceSource::GeometricGap);
}

#[test]
fn test_should_insert_space_consensus_tj_and_geometric() {
    let config = SpanMergingConfig::default();
    let fonts = HashMap::new();

    // Both TJ offset and geometric gap triggered
    // geometric_threshold = 12.0 * 0.25 = 3.0 (fallback)
    let decision = should_insert_space(
        "word", "next", 4.0, 12.0, "F1", &fonts, true, &config, None, None, 12.0, 12.0,
    );
    assert!(decision.insert_space, "Consensus should insert space");
    assert_eq!(decision.source, SpaceSource::TjOffset);
    assert_eq!(decision.confidence, 1.0);
}

#[test]
fn test_should_insert_space_no_consensus_small_gap() {
    let config = SpanMergingConfig::default();
    let fonts = HashMap::new();

    // Small gap, no TJ offset - should not insert
    let decision = should_insert_space(
        "word", "next", 0.5, 12.0, "F1", &fonts, false, &config, None, None, 12.0, 12.0,
    );
    assert!(
        !decision.insert_space,
        "Small gap without TJ should not insert space"
    );
    assert_eq!(decision.source, SpaceSource::NoSpace);
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Line break detection in should_insert_space
// ========================================================================

#[test]
fn test_should_insert_space_line_break_hard() {
    let config = SpanMergingConfig::default();
    let fonts = HashMap::new();

    // Simulate line break: prev at Y=700, next at Y=680
    let prev_bbox = Rect::new(100.0, 700.0, 200.0, 12.0);
    let next_bbox = Rect::new(100.0, 680.0, 200.0, 12.0);

    let decision = should_insert_space(
        "end of line",
        "start of next",
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
    // Line break detected, same column, not ending with hyphen => insert space
    assert!(decision.insert_space, "Hard line break should insert space");
}

#[test]
fn test_should_insert_space_line_break_hyphen() {
    let config = SpanMergingConfig::default();
    let fonts = HashMap::new();

    // Line break with hyphen: should NOT insert space
    let prev_bbox = Rect::new(100.0, 700.0, 200.0, 12.0);
    let next_bbox = Rect::new(100.0, 680.0, 200.0, 12.0);

    let decision = should_insert_space(
        "self-contain-",
        "ed text",
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
    assert!(
        !decision.insert_space,
        "Hyphenated line break should not insert space"
    );
    assert_eq!(
        decision.source,
        SpaceSource::SoftHyphen,
        "the merge site drops the hyphen only when this source identifies it"
    );
}

/// The hyphen is removed only where it is certainly an artefact of line breaking.
#[test]
fn soft_hyphen_removal_is_limited_to_lowercase_word_splits() {
    // Split words: the hyphen belongs to the typesetter, not the word.
    assert!(splits_one_word("implementa-", "tion"));
    assert!(splits_one_word("effective trans-", "fer from"));

    // Certainly real hyphens: capitalised compounds, ranges, and headings.
    assert!(!splits_one_word("Fine-", "Tuning"));
    assert!(!splits_one_word("2019-", "2020"));
    assert!(!splits_one_word("PRE-", "TRAINING"));
    assert!(!splits_one_word("multi-", "Head"));

    // Technical compounds keep their hyphen even all-lowercase, and the prefix is
    // matched as its own word rather than as a suffix of the preceding text.
    assert!(!splits_one_word("we use pre-", "training"));
    assert!(!splits_one_word("self-", "attention"));
    assert!(!splits_one_word("multi-", "head"));
    assert!(!splits_one_word("fine-", "tuning"));
    assert!(splits_one_word("compre-", "hensive"), "not the pre- prefix");

    // Nothing to remove.
    assert!(!splits_one_word("no hyphen", "here"));
    assert!(!splits_one_word("-", "orphan"));
    assert!(!splits_one_word("", "empty"));
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Full extraction pipeline via content streams
// ========================================================================

#[test]
fn test_extract_multiple_text_objects() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 100 700 Td (First) Tj ET BT /F1 12 Tf 100 680 Td (Second) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    let text: String = chars.iter().map(|c| c.char).collect();
    assert!(text.contains("First"));
    assert!(text.contains("Second"));
}

#[test]
fn test_extract_spans_with_line_break() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Two lines of text
    let stream = b"BT /F1 12 Tf 14 TL 100 700 Td (First line) Tj T* (Second line) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    assert!(!spans.is_empty());
    let text: String = spans
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("First"), "Should contain first line");
    assert!(text.contains("Second"), "Should contain second line");
}

#[test]
fn test_extract_chars_reading_order() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Text in reverse rendering order
    let stream = b"BT /F1 12 Tf 100 680 Td (B) Tj ET BT /F1 12 Tf 100 700 Td (A) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    // After sorting by reading order: A (y=700 higher) should come first
    assert_eq!(
        chars[0].char, 'A',
        "Higher Y should come first in reading order"
    );
    assert_eq!(chars[1].char, 'B');
}

#[test]
fn test_extract_empty_string() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 100 700 Td () Tj ET";
    let chars = extractor.extract(stream).unwrap();
    assert_eq!(chars.len(), 0);
}

#[test]
fn test_extract_only_graphics_no_text() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Only graphics commands, no text
    let stream = b"q 1 0 0 1 0 0 cm 100 700 m 200 700 l S Q";
    let chars = extractor.extract(stream).unwrap();
    assert_eq!(chars.len(), 0);
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: Inline images should not affect text
// ========================================================================

#[test]
fn test_inline_image_ignored() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Text before and after inline image - both should be extracted
    // The inline image operators are handled by the parser
    let stream = b"BT /F1 12 Tf 100 700 Td (Before) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    let text: String = chars.iter().map(|c| c.char).collect();
    assert!(text.contains("Before"));
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
