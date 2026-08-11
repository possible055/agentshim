use super::*;

#[test]
fn test_geometric_gap_basic() {
    let detector = WordBoundaryDetector::new();
    let context = BoundaryContext::new(12.0);

    let prev = CharacterInfo {
        code: 't' as u32,
        glyph_id: None,
        width: 5.0,
        x_position: 100.0,
        tj_offset: None,
        font_size: 12.0,
        is_ligature: false,
        original_ligature: None,
        protected_from_split: false,
    };

    // Large gap (10 units > 9.6 = 12*0.8 threshold)
    let curr = CharacterInfo {
        code: 'h' as u32,
        glyph_id: None,
        width: 5.0,
        x_position: 115.0, // 115 - 105 = 10 unit gap
        tj_offset: None,
        font_size: 12.0,
        is_ligature: false,
        original_ligature: None,
        protected_from_split: false,
    };

    assert!(
        detector.has_significant_geometric_gap(&prev, &curr, &context),
        "Gap of 10 units should exceed threshold of 9.6 (12pt * 0.8)"
    );
}

#[test]
fn test_geometric_gap_with_char_spacing() {
    let detector = WordBoundaryDetector::new();
    let mut context = BoundaryContext::new(12.0);
    context.char_spacing = 2.0; // Tc = 2.0

    let prev = CharacterInfo {
        code: 'a' as u32,
        glyph_id: None,
        width: 5.0,
        x_position: 100.0,
        tj_offset: None,
        font_size: 12.0,
        is_ligature: false,
        original_ligature: None,
        protected_from_split: false,
    };

    // Raw gap = 10, but Tc = 2.0 reduces it to 8.0
    // 8.0 < 9.6 (threshold), so NO boundary
    let curr = CharacterInfo {
        code: 'b' as u32,
        glyph_id: None,
        width: 5.0,
        x_position: 115.0, // 115 - 105 = 10 unit gap
        tj_offset: None,
        font_size: 12.0,
        is_ligature: false,
        original_ligature: None,
        protected_from_split: false,
    };

    assert!(
        !detector.has_significant_geometric_gap(&prev, &curr, &context),
        "Gap of 10 - 2.0 (Tc) = 8.0 should NOT exceed threshold of 9.6"
    );
}

#[test]
fn test_ligature_internal_gap_fi() {
    let detector = WordBoundaryDetector::new();
    let context = BoundaryContext::new(12.0);

    // 'f' component from expanded 'fi' ligature
    let prev = CharacterInfo {
        code: 'f' as u32,
        glyph_id: None,
        width: 5.0,
        x_position: 100.0,
        tj_offset: None,
        font_size: 12.0,
        is_ligature: true, // This is from ligature expansion
        original_ligature: Some('ﬁ'),
        protected_from_split: false,
    };

    // Large gap but prev is from ligature
    let curr = CharacterInfo {
        code: 'i' as u32,
        glyph_id: None,
        width: 3.0,
        x_position: 120.0, // 120 - 105 = 15 unit gap (would normally be boundary)
        tj_offset: None,
        font_size: 12.0,
        is_ligature: true,
        original_ligature: Some('ﬁ'),
        protected_from_split: false,
    };

    assert!(
        !detector.has_significant_geometric_gap(&prev, &curr, &context),
        "Ligature internal gap should NOT create boundary even with large gap"
    );
}

#[test]
fn test_punctuation_reduced_threshold() {
    let detector = WordBoundaryDetector::new();
    let context = BoundaryContext::new(12.0);
    // Base threshold: 12.0 * 0.8 = 9.6
    // Punctuation threshold: 9.6 * 0.5 = 4.8

    let prev = CharacterInfo {
        code: 'd' as u32,
        glyph_id: None,
        width: 5.0,
        x_position: 100.0,
        tj_offset: None,
        font_size: 12.0,
        is_ligature: false,
        original_ligature: None,
        protected_from_split: false,
    };

    // Gap of 6.0 units
    // Normal threshold would be 9.6 (no boundary)
    // Punctuation threshold is 4.8 (YES boundary)
    let curr_period = CharacterInfo {
        code: '.' as u32, // Period is punctuation
        glyph_id: None,
        width: 2.0,
        x_position: 111.0, // 111 - 105 = 6 unit gap
        tj_offset: None,
        font_size: 12.0,
        is_ligature: false,
        original_ligature: None,
        protected_from_split: false,
    };

    assert!(
        detector.has_significant_geometric_gap(&prev, &curr_period, &context),
        "Gap of 6 units should exceed punctuation threshold of 4.8 (50% of 9.6)"
    );
}

#[test]
fn test_punctuation_does_not_trigger_on_normal_text() {
    let detector = WordBoundaryDetector::new();
    let context = BoundaryContext::new(12.0);

    let prev = CharacterInfo {
        code: 'd' as u32,
        glyph_id: None,
        width: 5.0,
        x_position: 100.0,
        tj_offset: None,
        font_size: 12.0,
        is_ligature: false,
        original_ligature: None,
        protected_from_split: false,
    };

    // Same gap (6.0) but current character is 'e', not punctuation
    let curr = CharacterInfo {
        code: 'e' as u32, // 'e' is NOT punctuation
        glyph_id: None,
        width: 5.0,
        x_position: 111.0,
        tj_offset: None,
        font_size: 12.0,
        is_ligature: false,
        original_ligature: None,
        protected_from_split: false,
    };

    assert!(
        !detector.has_significant_geometric_gap(&prev, &curr, &context),
        "Gap of 6 units should NOT exceed normal threshold of 9.6"
    );
}

#[test]
fn test_is_punctuation_ascii() {
    assert!(WordBoundaryDetector::is_punctuation('.' as u32));
    assert!(WordBoundaryDetector::is_punctuation(',' as u32));
    assert!(WordBoundaryDetector::is_punctuation('!' as u32));
    assert!(WordBoundaryDetector::is_punctuation('?' as u32));
    assert!(WordBoundaryDetector::is_punctuation(':' as u32));
    assert!(WordBoundaryDetector::is_punctuation(';' as u32));
}

#[test]
fn test_is_punctuation_non_punctuation() {
    assert!(!WordBoundaryDetector::is_punctuation('a' as u32));
    assert!(!WordBoundaryDetector::is_punctuation('1' as u32));
    assert!(!WordBoundaryDetector::is_punctuation(' ' as u32));
}

#[test]
fn test_is_ligature_internal_gap_ffi() {
    let detector = WordBoundaryDetector::new();

    // 'f' from 'ffi' ligature
    let prev = CharacterInfo {
        code: 'f' as u32,
        glyph_id: None,
        width: 5.0,
        x_position: 100.0,
        tj_offset: None,
        font_size: 12.0,
        is_ligature: true,
        original_ligature: Some('ﬄ'), // ffi ligature U+FB04
        protected_from_split: false,
    };

    let curr = CharacterInfo {
        code: 'f' as u32,
        glyph_id: None,
        width: 5.0,
        x_position: 110.0,
        tj_offset: None,
        font_size: 12.0,
        is_ligature: true,
        original_ligature: Some('ﬄ'),
        protected_from_split: false,
    };

    assert!(
        detector.is_ligature_internal_gap(&prev, &curr),
        "Should detect ligature internal gap when both have is_ligature=true"
    );
}

#[test]
fn test_is_ligature_internal_gap_actual_ligature_code() {
    let detector = WordBoundaryDetector::new();

    // Previous character IS the ligature U+FB00 ('ff')
    let prev = CharacterInfo {
        code: 0xFB00, // 'ff' ligature
        glyph_id: None,
        width: 10.0,
        x_position: 100.0,
        tj_offset: None,
        font_size: 12.0,
        is_ligature: false, // Not expanded, still the ligature
        original_ligature: None,
        protected_from_split: false,
    };

    let curr = CharacterInfo {
        code: 'i' as u32,
        glyph_id: None,
        width: 3.0,
        x_position: 115.0,
        tj_offset: None,
        font_size: 12.0,
        is_ligature: false,
        original_ligature: None,
        protected_from_split: false,
    };

    assert!(
        detector.is_ligature_internal_gap(&prev, &curr),
        "Should detect ligature internal gap when prev code is U+FB00"
    );
}
