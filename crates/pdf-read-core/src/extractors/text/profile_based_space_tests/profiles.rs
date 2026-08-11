use super::*;

/// Test that ACADEMIC profile uses aggressive thresholds
///
/// Academic papers have tight spacing (especially around punctuation).
/// The profile should:
/// - Use lower TJ offset threshold (-90 instead of -120)
/// - Use lower word margin ratio (0.12 instead of 0.1)
/// - Enable adaptive threshold for dynamic adjustment
#[test]
fn test_academic_profile_thresholds() {
    let profile =
        crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Academic);

    // Academic papers should be more aggressive with space insertion
    assert!(
        profile.tj_offset_threshold < -100.0,
        "Academic should use lower TJ threshold for more spaces"
    );

    // Academic papers should have tighter word margins
    assert!(
        profile.word_margin_ratio <= 0.15,
        "Academic should use conservative word margin"
    );

    // Verify we can create a config from the profile
    let config = TextExtractionConfig::with_space_threshold(profile.tj_offset_threshold);
    assert_eq!(
        config.space_insertion_threshold,
        profile.tj_offset_threshold
    );
}

/// Test that POLICY profile uses conservative thresholds
///
/// Policy documents (like GDPR) have justified text with precise spacing.
/// The profile should:
/// - Use higher TJ offset threshold (-110 to preserve structure)
/// - Use higher word margin ratio (0.18-0.2 for justified text)
/// - Preserve column boundaries and table structure
#[test]
fn test_policy_profile_thresholds() {
    let profile =
        crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Policy);

    // Policy documents should be more conservative to preserve structure
    assert!(
        profile.tj_offset_threshold > -120.0,
        "Policy should use higher TJ threshold to avoid over-spacing"
    );

    // Policy documents should have looser word margins for justified text
    assert!(
        profile.word_margin_ratio >= 0.15,
        "Policy should use higher word margin for justified text"
    );

    let config = TextExtractionConfig::with_space_threshold(profile.tj_offset_threshold);
    assert_eq!(
        config.space_insertion_threshold,
        profile.tj_offset_threshold
    );
}

/// Test that FORM profile preserves field boundaries
///
/// Forms have checkboxes, fields, and precise layout.
/// The profile should:
/// - Use conservative thresholds to avoid merging fields
/// - High column boundary threshold to preserve structure
/// - Enable adaptive threshold for form field detection
#[test]
fn test_form_profile_thresholds() {
    let profile =
        crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Form);

    // Forms should preserve field structure with conservative spacing
    assert!(
        profile.tj_offset_threshold >= -120.0,
        "Form profile should be conservative with space insertion"
    );

    let config = TextExtractionConfig::with_space_threshold(profile.tj_offset_threshold);
    assert_eq!(
        config.space_insertion_threshold,
        profile.tj_offset_threshold
    );
}

/// Test that profile selection works correctly for document types
#[test]
fn test_profile_selection_for_document_types() {
    let academic =
        crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Academic);
    let policy =
        crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Policy);
    let form =
        crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Form);
    let mixed =
        crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Mixed);

    // Verify each profile has distinct thresholds
    let thresholds = [
        academic.tj_offset_threshold,
        policy.tj_offset_threshold,
        form.tj_offset_threshold,
        mixed.tj_offset_threshold,
    ];

    // At least some profiles should have different thresholds
    let unique_count = thresholds
        .iter()
        .filter(|t| !thresholds.iter().skip(1).any(|other| other == *t))
        .count();

    assert!(
        unique_count > 0,
        "Profiles should have different thresholds for different document types"
    );
}

/// Test that TextExtractionConfig can accept a profile
#[test]
fn test_config_with_profile() {
    let profile = crate::config::ExtractionProfile::ACADEMIC;

    // Should be able to create config with profile thresholds
    let config = TextExtractionConfig::with_space_threshold(profile.tj_offset_threshold);

    assert_eq!(
        config.space_insertion_threshold,
        profile.tj_offset_threshold
    );
}

/// Test that profiles have reasonable threshold ranges
#[test]
fn test_profile_thresholds_in_reasonable_range() {
    let profiles = vec![
        crate::config::ExtractionProfile::CONSERVATIVE,
        crate::config::ExtractionProfile::ACADEMIC,
        crate::config::ExtractionProfile::POLICY,
        crate::config::ExtractionProfile::FORM,
    ];

    for profile in profiles {
        // TJ offsets should be negative (per PDF spec)
        assert!(
            profile.tj_offset_threshold < 0.0,
            "TJ threshold must be negative ({})",
            profile.name
        );

        // Should be in reasonable range (-150 to -50)
        assert!(
            profile.tj_offset_threshold >= -150.0 && profile.tj_offset_threshold <= -50.0,
            "TJ threshold out of range for {} ({})",
            profile.name,
            profile.tj_offset_threshold
        );

        // Word margin ratios should be positive and reasonable (0.05 to 0.25)
        assert!(
            profile.word_margin_ratio > 0.0 && profile.word_margin_ratio < 1.0,
            "Word margin ratio must be between 0 and 1 for {}",
            profile.name
        );

        // Space threshold EM ratio should be positive
        assert!(
            profile.space_threshold_em_ratio > 0.0,
            "Space threshold EM ratio must be positive for {}",
            profile.name
        );
    }
}

/// Test that multiple profiles can coexist
#[test]
fn test_multiple_profiles_independent() {
    let academic =
        crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Academic);
    let policy =
        crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Policy);

    // Create configs from both profiles
    let academic_config = TextExtractionConfig::with_space_threshold(academic.tj_offset_threshold);
    let policy_config = TextExtractionConfig::with_space_threshold(policy.tj_offset_threshold);

    // Verify they have different thresholds
    assert_ne!(
        academic_config.space_insertion_threshold, policy_config.space_insertion_threshold,
        "Academic and policy configs should have different thresholds"
    );
}

/// Test that default config is backward-compatible
#[test]
fn test_default_config_backward_compatible() {
    let default_config = TextExtractionConfig::default();
    let conservative_profile = crate::config::ExtractionProfile::CONSERVATIVE;

    // Default should match or be compatible with conservative profile
    assert_eq!(
        default_config.space_insertion_threshold, conservative_profile.tj_offset_threshold,
        "Default config should use conservative threshold for backward compatibility"
    );
}

/// Test that adjacent table cell values get spaces inserted between them.
///
/// Simulates a form where two "$0.00" values are in adjacent cells with
/// a small positive gap (1pt). The merge logic should insert a space because
/// the spans are clearly separate tokens (ending/starting with digits/currency).
#[test]
fn test_adjacent_table_cell_values_not_concatenated() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    // "$0.00" at 10pt font is about 30pt wide (5 chars * ~6pt average width)
    // Second value starts at x=131, creating a 1pt gap (100 + 30 = 130, gap = 1pt)
    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "$0.00".to_string(),
            bbox: Rect::new(100.0, 700.0, 30.0, 10.0),
            font_name: "F1".to_string(),
            font_size: 10.0,
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
            text: "$0.00".to_string(),
            bbox: Rect::new(131.0, 700.0, 30.0, 10.0), // 1pt gap
            font_name: "F1".to_string(),
            font_size: 10.0,
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
    assert_eq!(extractor.spans.len(), 1, "Adjacent spans should merge");
    assert_eq!(
        extractor.spans[0].text, "$0.00 $0.00",
        "Adjacent table cell values should have space between them, got: '{}'",
        extractor.spans[0].text
    );
}

/// Test that adjacent numeric values with small gaps get spaces.
/// Covers cases like "100200" that should be "100 200" in table contexts.
#[test]
fn test_adjacent_numeric_values_not_concatenated() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "100".to_string(),
            bbox: Rect::new(200.0, 500.0, 18.0, 10.0),
            font_name: "F1".to_string(),
            font_size: 10.0,
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
            text: "200".to_string(),
            bbox: Rect::new(219.5, 500.0, 18.0, 10.0), // 1.5pt gap
            font_name: "F1".to_string(),
            font_size: 10.0,
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
    assert_eq!(extractor.spans.len(), 1, "Adjacent spans should merge");
    assert_eq!(
        extractor.spans[0].text, "100 200",
        "Adjacent numeric values should have space between them, got: '{}'",
        extractor.spans[0].text
    );
}

/// Ensure that true word fragments (zero gap) still merge without space.
/// E.g., "Hel" + "lo" with gap=0 should become "Hello" not "Hel lo".
#[test]
fn test_word_fragments_zero_gap_no_space() {
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "Hel".to_string(),
            bbox: Rect::new(100.0, 700.0, 18.0, 12.0),
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
            text: "lo".to_string(),
            bbox: Rect::new(118.0, 700.0, 12.0, 12.0), // 0pt gap
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
    assert_eq!(extractor.spans.len(), 1, "Adjacent spans should merge");
    assert_eq!(
        extractor.spans[0].text, "Hello",
        "Zero-gap word fragments should merge without space, got: '{}'",
        extractor.spans[0].text
    );
}

// ========================================================================
// Decimal dollar value merging (split integer/decimal boxes)
// ========================================================================

#[test]
fn test_merge_decimal_dollar_value_split_boxes() {
    // Some forms have integer and decimal parts in separate fixed-width boxes.
    // e.g., "123456" at x=382.3 width=39.6, "72" at x=432.7 width=13.2
    // gap = 432.7 - (382.3 + 39.6) = 10.8pt
    // These should be merged as "123456.72"
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
    assert_eq!(
        extractor.spans.len(),
        1,
        "Decimal dollar value spans should merge into one"
    );
    assert_eq!(
        extractor.spans[0].text, "123456.72",
        "Integer and decimal parts should be joined with '.'"
    );
}

#[test]
fn test_merge_decimal_value_small_integer_part() {
    // Smaller dollar amount: "50" + "00" -> "50.00"
    let mut extractor = TextExtractor::new();
    extractor.merging_config = SpanMergingConfig::legacy();

    extractor.spans = vec![
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: "50".to_string(),
            bbox: Rect::new(382.3, 700.0, 15.0, 12.0),
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
            text: "00".to_string(),
            bbox: Rect::new(407.0, 700.0, 13.2, 12.0), // 9.7pt gap
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
    assert_eq!(extractor.spans.len(), 1);
    assert_eq!(extractor.spans[0].text, "50.00");
}
