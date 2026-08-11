use super::*;

#[test]
fn test_space_threshold_default() {
    // Test that default configuration uses -120.0 threshold
    let config = TextExtractionConfig::new();
    assert_eq!(config.space_insertion_threshold, -120.0);

    // Test that default extractor has default config
    let extractor = TextExtractor::new();
    assert_eq!(extractor.config.space_insertion_threshold, -120.0);
}

#[test]
fn test_space_threshold_custom() {
    // Test custom threshold configuration
    let config = TextExtractionConfig::with_space_threshold(-80.0);
    assert_eq!(config.space_insertion_threshold, -80.0);

    let extractor = TextExtractor::with_config(config);
    assert_eq!(extractor.config.space_insertion_threshold, -80.0);
}

#[test]
fn test_space_threshold_disabled() {
    // Test that threshold can be disabled with NEG_INFINITY
    let config = TextExtractionConfig::with_space_threshold(f32::NEG_INFINITY);
    assert_eq!(config.space_insertion_threshold, f32::NEG_INFINITY);

    let extractor = TextExtractor::with_config(config);
    assert_eq!(
        extractor.config.space_insertion_threshold,
        f32::NEG_INFINITY
    );
}

#[test]
fn test_adaptive_enabled_by_default() {
    // Test that adaptive threshold is enabled by default
    let config = SpanMergingConfig::default();
    assert!(
        config.use_adaptive_threshold,
        "Adaptive threshold should be enabled by default"
    );
}

#[test]
fn test_legacy_mode_disables_adaptive() {
    // Test that legacy() constructor provides backward-compatible behavior
    let legacy = SpanMergingConfig::legacy();
    assert!(
        !legacy.use_adaptive_threshold,
        "Legacy mode should disable adaptive threshold"
    );
    assert_eq!(legacy.conservative_threshold_pt, 0.1);
}

#[test]
fn test_adaptive_constructor_enables_adaptive() {
    // Test that adaptive() constructor enables adaptive threshold
    let adaptive = SpanMergingConfig::adaptive();
    assert!(
        adaptive.use_adaptive_threshold,
        "Adaptive constructor should enable adaptive threshold"
    );
    assert!(
        adaptive.adaptive_config.is_some(),
        "Adaptive constructor should set adaptive_config"
    );
}

// ============================================================================
// Artifact Type Parsing Tests (PDF Spec Section 14.8.2.2)
// ============================================================================

#[test]
fn test_parse_artifact_type_pagination_header() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Pagination".to_string()));
    props.insert("Subtype".to_string(), Object::Name("Header".to_string()));

    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(
        result,
        Some(ArtifactType::Pagination(PaginationSubtype::Header))
    );
}

#[test]
fn test_parse_artifact_type_pagination_footer() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Pagination".to_string()));
    props.insert("Subtype".to_string(), Object::Name("Footer".to_string()));

    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(
        result,
        Some(ArtifactType::Pagination(PaginationSubtype::Footer))
    );
}

#[test]
fn test_parse_artifact_type_pagination_watermark() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Pagination".to_string()));
    props.insert("Subtype".to_string(), Object::Name("Watermark".to_string()));

    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(
        result,
        Some(ArtifactType::Pagination(PaginationSubtype::Watermark))
    );
}

#[test]
fn test_parse_artifact_type_layout() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Layout".to_string()));

    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(result, Some(ArtifactType::Layout));
}

#[test]
fn test_parse_artifact_type_background() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Background".to_string()));

    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(result, Some(ArtifactType::Background));
}

#[test]
fn test_parse_artifact_type_subtype_only() {
    // Some PDFs use /Subtype without /Type
    let mut props = HashMap::new();
    props.insert("Subtype".to_string(), Object::Name("Header".to_string()));

    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(
        result,
        Some(ArtifactType::Pagination(PaginationSubtype::Header))
    );
}

#[test]
fn test_parse_artifact_type_empty() {
    let props = HashMap::new();
    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(result, None);
}

// ============================================================================
// ActualText Verification Tests (PDF Spec Section 14.9.4)
// ============================================================================
//
// ActualText provides replacement text for content that cannot be accurately
// represented by the content stream (ligatures, decorated glyphs, formulas).
// Per ISO 32000-1:2008 Section 14.9.4, ActualText takes precedence over
// character extraction.

#[test]
fn test_marked_content_context_with_actual_text() {
    // Verify MarkedContentContext correctly stores ActualText
    let ctx = MarkedContentContext {
        artifact_type: None,
        tag: "Span".to_string(),
        is_artifact: false,
        actual_text: Some("fi".to_string()), // Ligature expansion
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    };

    assert_eq!(ctx.actual_text, Some("fi".to_string()));
    assert!(!ctx.is_artifact);
}

#[test]
fn test_marked_content_context_with_expansion() {
    // Verify MarkedContentContext correctly stores /E expansion
    let ctx = MarkedContentContext {
        artifact_type: None,
        tag: "Span".to_string(),
        is_artifact: false,
        actual_text: None,
        expansion: Some("Portable Document Format".to_string()),
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    };

    assert_eq!(ctx.expansion, Some("Portable Document Format".to_string()));
}

#[test]
fn test_marked_content_context_artifact_with_actual_text() {
    // Verify artifacts can have ActualText (though typically they don't)
    let ctx = MarkedContentContext {
        tag: "Artifact".to_string(),
        is_artifact: true,
        artifact_type: Some(ArtifactType::Pagination(PaginationSubtype::Header)),
        actual_text: Some("Header text".to_string()),
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    };

    assert!(ctx.is_artifact);
    assert_eq!(ctx.actual_text, Some("Header text".to_string()));
}

#[test]
fn test_get_current_actual_text_finds_first() {
    // Verify get_current_actual_text returns first ActualText in stack
    let mut extractor = TextExtractor::new();

    // Push contexts with ActualText
    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "Span".to_string(),
        is_artifact: false,
        actual_text: Some("outer text".to_string()),
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    });

    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "Span".to_string(),
        is_artifact: false,
        actual_text: Some("inner text".to_string()),
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    });

    // Should return innermost (most recent) ActualText
    let result = extractor.get_current_actual_text();
    assert_eq!(result, Some("inner text".to_string()));
}

#[test]
fn test_get_current_actual_text_skips_none() {
    // Verify get_current_actual_text skips contexts without ActualText
    let mut extractor = TextExtractor::new();

    // Push context with ActualText
    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "Span".to_string(),
        is_artifact: false,
        actual_text: Some("replacement text".to_string()),
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    });

    // Push context without ActualText
    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "Span".to_string(),
        is_artifact: false,
        actual_text: None,
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    });

    // Should find the ActualText from outer context
    let result = extractor.get_current_actual_text();
    assert_eq!(result, Some("replacement text".to_string()));
}

#[test]
fn test_get_current_actual_text_returns_none_when_empty() {
    // Verify get_current_actual_text returns None when no ActualText
    let extractor = TextExtractor::new();

    let result = extractor.get_current_actual_text();
    assert_eq!(result, None);
}
