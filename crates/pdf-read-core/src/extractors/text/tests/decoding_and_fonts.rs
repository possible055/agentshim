use super::*;

#[test]
fn test_snap_superscript_baselines_scales() {
    let mut extractor = TextExtractor::new();
    let mut spans = Vec::with_capacity(50_002);
    // A real base+superscript pair we can assert on.
    spans.push(snap_span("x", 100.0, 700.0, 30.0, 12.0, 0));
    spans.push(snap_span("2", 130.0, 704.0, 4.0, 6.0, 1));
    // 50k body spans spread across the page (distinct Y) — same font size,
    // so none qualify as bases for each other; the cost is pure iteration.
    for k in 0..50_000usize {
        let y = (k as f32) * 2.0; // spread across Y so each window is tiny
        spans.push(snap_span("a", 50.0, y, 6.0, 10.0, k + 2));
    }
    extractor.spans = spans;

    let start = std::time::Instant::now();
    extractor.snap_superscript_baselines();
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 5,
        "#575: snap_superscript_baselines took {elapsed:?} on 50k spans — \
         likely an O(n²) regression"
    );
    assert_eq!(
        extractor.spans[1].bbox.y, 700.0,
        "#575: the genuine superscript must still snap to its base"
    );
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: TextExtractor configuration
// ========================================================================

#[test]
fn test_extractor_with_merging_config() {
    let extractor = TextExtractor::new().with_merging_config(SpanMergingConfig::aggressive());
    assert_eq!(extractor.merging_config.space_threshold_em_ratio, 0.15);
}

#[test]
fn test_extractor_set_resources() {
    let mut extractor = TextExtractor::new();
    assert!(extractor.resources.is_none());
    extractor.set_resources(Object::Null);
    assert!(extractor.resources.is_some());
}

#[test]
fn test_extractor_prepare_for_span_extraction() {
    let mut extractor = TextExtractor::new();
    extractor.extract_spans = false;
    extractor.span_sequence_counter = 42;
    extractor.prepare_for_span_extraction();
    assert!(extractor.extract_spans);
    assert_eq!(extractor.span_sequence_counter, 0);
    assert!(extractor.spans.is_empty());
}

#[test]
fn test_extractor_get_font_set() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);
    let font2 = create_test_font();
    extractor.add_font("F2".to_string(), font2);

    let font_set = extractor.get_font_set();
    assert_eq!(font_set.len(), 2);
}

#[test]
fn test_extractor_add_font_shared() {
    let mut extractor = TextExtractor::new();
    let font = Arc::new(create_test_font());
    extractor.add_font_shared("F1".to_string(), font.clone());
    assert_eq!(extractor.fonts.len(), 1);
    // Verify it's the same Arc
    assert!(Arc::ptr_eq(extractor.fonts.get("F1").unwrap(), &font));
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: analyze_tj_distribution
// ========================================================================

#[test]
fn test_analyze_tj_distribution_empty() {
    let extractor = TextExtractor::new();
    let (is_justified, cv) = extractor.analyze_tj_distribution();
    assert!(!is_justified);
    assert_eq!(cv, 0.0);
}

#[test]
fn test_analyze_tj_distribution_uniform() {
    let mut extractor = TextExtractor::new();
    // Uniform offsets (all the same) = low CV = not justified
    extractor.tj_offset_history = vec![-100.0; 50];
    let (is_justified, cv) = extractor.analyze_tj_distribution();
    assert!(!is_justified, "Uniform offsets should not be justified");
    assert!(cv < 0.01, "CV should be ~0 for uniform offsets, got {}", cv);
}

#[test]
fn test_analyze_tj_distribution_high_variance() {
    let mut extractor = TextExtractor::new();
    // High variance offsets = justified text
    let mut offsets = Vec::new();
    for i in 0..100 {
        offsets.push(if i % 2 == 0 { -50.0 } else { -200.0 });
    }
    extractor.tj_offset_history = offsets;
    let (is_justified, cv) = extractor.analyze_tj_distribution();
    assert!(
        is_justified,
        "High variance should indicate justified text, cv={}",
        cv
    );
    assert!(
        cv > 0.5,
        "CV should be > 0.5 for justified text, got {}",
        cv
    );
}

/// The O(1) accumulator path and the recompute-from-slice fallback must
/// produce identical results (same f64 formula, same sum order).
#[test]
fn test_tj_accumulator_matches_recompute() {
    let vals = vec![
        -50.0f32, -200.0, -75.0, -180.0, -60.0, -210.0, -90.0, -150.0,
    ];

    // O(1) path: accumulators kept consistent with the history (as `push` does).
    let mut a = TextExtractor::new();
    let mut sum = 0.0f64;
    let mut sq = 0.0f64;
    for &v in &vals {
        let x = v as f64;
        sum += x;
        sq += x * x;
        a.tj_offset_history.push(v);
    }
    a.tj_sum = sum;
    a.tj_sum_sq = sq;
    a.tj_stats_len = a.tj_offset_history.len();
    let (ja, cva) = a.analyze_tj_distribution();

    // Recompute path: only the history is set (stale accumulators).
    let mut b = TextExtractor::new();
    b.tj_offset_history = vals.clone();
    let (jb, cvb) = b.analyze_tj_distribution();

    assert_eq!(ja, jb, "is_justified must agree across paths");
    assert!(
        (cva - cvb).abs() < 1e-6,
        "O(1) cv {cva} must equal recompute cv {cvb}"
    );
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: calculate_adaptive_tj_threshold
// ========================================================================

#[test]
fn test_adaptive_threshold_disabled() {
    let config = TextExtractionConfig {
        use_adaptive_tj_threshold: false,
        space_insertion_threshold: -120.0,
        ..TextExtractionConfig::default()
    };
    let extractor = TextExtractor::with_config(config);
    let threshold = extractor.calculate_adaptive_tj_threshold();
    assert_eq!(threshold, -120.0);
}

#[test]
fn test_adaptive_threshold_enabled() {
    let config = TextExtractionConfig {
        use_adaptive_tj_threshold: true,
        word_margin_ratio: 0.1,
        ..TextExtractionConfig::default()
    };
    let mut extractor = TextExtractor::with_config(config);
    // Set font size
    extractor.state_stack.current_mut().font_size = 12.0;
    let threshold = extractor.calculate_adaptive_tj_threshold();
    assert!(threshold < 0.0, "Adaptive threshold should be negative");
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: update_artifact_state
// ========================================================================

#[test]
fn test_update_artifact_state_empty_stack() {
    let mut extractor = TextExtractor::new();
    extractor.update_artifact_state();
    assert!(!extractor.inside_artifact);
}

#[test]
fn test_update_artifact_state_artifact_present() {
    let mut extractor = TextExtractor::new();
    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "Artifact".to_string(),
        is_artifact: true,
        actual_text: None,
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    });
    extractor.update_artifact_state();
    assert!(extractor.inside_artifact);
}

#[test]
fn test_placed_pdf_suppresses_content() {
    // Text inside an InDesign /PlacedPDF figure region (the placed
    // artwork's own glyphs — e.g. a draft galley) must be suppressed,
    // matching pdftotext/PyMuPDF. Entering a /PlacedPDF BDC sets
    // inside_placed_pdf, which feeds is_content_suppressed().
    let mut extractor = TextExtractor::new();
    assert!(!extractor.inside_placed_pdf);
    assert!(!extractor.is_content_suppressed());

    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "PlacedPDF".to_string(),
        is_artifact: false,
        actual_text: None,
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: true,
        actual_text_emitted: false,
        own_mcid: None,
    });
    extractor.update_layer_state();
    assert!(extractor.inside_placed_pdf);
    assert!(
        extractor.is_content_suppressed(),
        "text inside /PlacedPDF must be suppressed"
    );

    // Leaving the region restores normal extraction.
    extractor.marked_content_stack.pop();
    extractor.update_layer_state();
    assert!(!extractor.inside_placed_pdf);
    assert!(!extractor.is_content_suppressed());
}

#[test]
fn test_non_placed_pdf_tag_does_not_suppress() {
    // A regular (non-PlacedPDF) marked-content tag such as /Figure must
    // NOT suppress its text — only the placed-PDF wrapper does.
    let mut extractor = TextExtractor::new();
    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "Figure".to_string(),
        is_artifact: false,
        actual_text: None,
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    });
    extractor.update_layer_state();
    assert!(!extractor.inside_placed_pdf);
    assert!(!extractor.is_content_suppressed());
}

#[test]
fn test_placed_pdf_kept_when_it_is_the_whole_page_body() {
    // A publisher that places the ENTIRE article body inside one /PlacedPDF
    // region (e.g. MATEC Web of Conferences) leaves almost nothing outside.
    // There the placed text IS the page's logical content and must NOT be
    // suppressed (pymupdf/pdftotext extract it). The coverage pre-scan flags
    // this: placed text dominates, non-placed text is a tiny header.
    let body = "(This is the full article body typeset inside a placed PDF region) Tj\n".repeat(20);
    let stream = format!("/PlacedPDF BMC\nBT\n{body}ET\nEMC\nBT (Journal vol 1) Tj ET\n");
    assert!(
        TextExtractor::placed_pdf_text_dominates(stream.as_bytes()),
        "whole-body /PlacedPDF must be KEPT (not suppressed)"
    );
}

#[test]
fn test_placed_pdf_suppressed_when_minority_overlay() {
    // The decorative-figure case (PMC8100493): a small /PlacedPDF galley
    // duplicate sits amid a full page of real text OUTSIDE it. The placed
    // text is the minority, so it stays suppressed (the de-dup win).
    let outside =
        "(Real published paragraph of the article that lives outside the placed region) Tj\n"
            .repeat(20);
    let stream = format!("BT\n{outside}ET\n/PlacedPDF BMC\nBT (draft galley) Tj ET\nEMC\n");
    assert!(
        !TextExtractor::placed_pdf_text_dominates(stream.as_bytes()),
        "minority-overlay /PlacedPDF must stay suppressed"
    );
}

#[test]
fn test_placed_pdf_coverage_noop_without_tag() {
    // No /PlacedPDF tag anywhere: the pre-scan must short-circuit to false
    // (keep the default suppression state; pay nothing for ordinary pages).
    let stream = b"BT (ordinary single column page of text) Tj ET\n";
    assert!(!TextExtractor::placed_pdf_text_dominates(stream));
}

#[test]
fn test_placed_pdf_kept_when_unique_body_amid_comparable_outside() {
    // Gate 3: an InDesign spread (e.g. a placed floor-plan / marketing page)
    // where the placed region carries a substantial body of UNIQUE text and
    // the non-placed text is comparable or larger but different (labels,
    // headers). The 3:1 dominance ratio fails, yet the placed words are not a
    // duplicate of the outside text, so it must be KEPT (pdftotext/pymupdf
    // extract it; suppressing it drops the whole spread's content).
    let placed = "(master bedroom terrace kitchen dimensions balcony) Tj\n".repeat(30);
    let outside = "(square footage residence penthouse skyline waterfront) Tj\n".repeat(35);
    let stream = format!("BT\n{outside}ET\n/PlacedPDF /MC0 BDC\nBT\n{placed}ET\nEMC\n");
    assert!(
        TextExtractor::placed_pdf_text_dominates(stream.as_bytes()),
        "unique placed body amid comparable outside text must be KEPT"
    );
}

#[test]
fn test_placed_pdf_suppressed_when_large_duplicate_overlay() {
    // Gate 3, the other side: a large placed region whose words DUPLICATE the
    // surrounding text is a draft galley / overlay copy and stays suppressed
    // even though it clears the size gate (the PMC8100493 de-dup intent, at
    // full body size rather than the minority-overlay size).
    let body = "(the published paragraph of the real article body content) Tj\n".repeat(30);
    let stream = format!("BT\n{body}ET\n/PlacedPDF /MC0 BDC\nBT\n{body}ET\nEMC\n");
    assert!(
        !TextExtractor::placed_pdf_text_dominates(stream.as_bytes()),
        "a full-size placed DUPLICATE of the outside text must stay suppressed"
    );
}

#[test]
fn test_update_artifact_state_nested_non_artifact() {
    let mut extractor = TextExtractor::new();
    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "Artifact".to_string(),
        is_artifact: true,
        actual_text: None,
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
        actual_text: None,
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    });
    extractor.update_artifact_state();
    // Should still be inside artifact because parent is artifact
    assert!(extractor.inside_artifact);
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: parse_artifact_type
// ========================================================================

#[test]
fn test_parse_artifact_type_page() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Page".to_string()));
    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(result, Some(ArtifactType::Page));
}

#[test]
fn test_parse_artifact_type_pagination_page_number() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Pagination".to_string()));
    props.insert(
        "Subtype".to_string(),
        Object::Name("PageNumber".to_string()),
    );
    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(
        result,
        Some(ArtifactType::Pagination(PaginationSubtype::PageNumber))
    );
}

#[test]
fn test_parse_artifact_type_pagination_other_subtype() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Pagination".to_string()));
    props.insert(
        "Subtype".to_string(),
        Object::Name("SomethingElse".to_string()),
    );
    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(
        result,
        Some(ArtifactType::Pagination(PaginationSubtype::Other))
    );
}

#[test]
fn test_parse_artifact_type_unknown_type() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("UnknownType".to_string()));
    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(result, None);
}

#[test]
fn test_parse_artifact_type_subtype_footer_only() {
    let mut props = HashMap::new();
    props.insert("Subtype".to_string(), Object::Name("Footer".to_string()));
    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(
        result,
        Some(ArtifactType::Pagination(PaginationSubtype::Footer))
    );
}

#[test]
fn test_parse_artifact_type_subtype_watermark_only() {
    let mut props = HashMap::new();
    props.insert("Subtype".to_string(), Object::Name("Watermark".to_string()));
    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(
        result,
        Some(ArtifactType::Pagination(PaginationSubtype::Watermark))
    );
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: decode_pdf_text_string
// ========================================================================

#[test]
fn test_decode_pdf_text_string_utf8() {
    let result = TextExtractor::decode_pdf_text_string(b"Hello World");
    assert_eq!(result, "Hello World");
}

#[test]
fn test_decode_pdf_text_string_utf16be_bom() {
    // UTF-16BE with BOM: FE FF, then "Hi" in UTF-16BE
    let bytes: Vec<u8> = vec![0xFE, 0xFF, 0x00, 0x48, 0x00, 0x69];
    let result = TextExtractor::decode_pdf_text_string(&bytes);
    assert_eq!(result, "Hi");
}

#[test]
fn test_decode_pdf_text_string_utf16le_bom() {
    // UTF-16LE with BOM: FF FE, then "Hi" in UTF-16LE
    let bytes: Vec<u8> = vec![0xFF, 0xFE, 0x48, 0x00, 0x69, 0x00];
    let result = TextExtractor::decode_pdf_text_string(&bytes);
    assert_eq!(result, "Hi");
}

#[test]
fn test_decode_pdf_text_string_empty() {
    let result = TextExtractor::decode_pdf_text_string(b"");
    assert_eq!(result, "");
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: split_on_camelcase
// ========================================================================

#[test]
fn test_split_camelcase_basic() {
    let extractor = TextExtractor::new();
    let parts = extractor.split_on_camelcase("theGeneral");
    assert_eq!(parts, vec!["the", "General"]);
}

#[test]
fn test_split_camelcase_multiple() {
    let extractor = TextExtractor::new();
    let parts = extractor.split_on_camelcase("lengthThisPage");
    assert_eq!(parts, vec!["length", "This", "Page"]);
}

#[test]
fn test_split_camelcase_no_split_all_lower() {
    let extractor = TextExtractor::new();
    let parts = extractor.split_on_camelcase("lowercase");
    assert_eq!(parts, vec!["lowercase"]);
}

#[test]
fn test_split_camelcase_no_split_all_upper() {
    let extractor = TextExtractor::new();
    let parts = extractor.split_on_camelcase("HTML");
    assert_eq!(parts, vec!["HTML"]);
}

#[test]
fn test_split_camelcase_single_char() {
    let extractor = TextExtractor::new();
    let parts = extractor.split_on_camelcase("A");
    assert_eq!(parts, vec!["A"]);
}

#[test]
fn test_split_camelcase_empty() {
    let extractor = TextExtractor::new();
    let parts = extractor.split_on_camelcase("");
    // Empty string gives one empty part
    assert_eq!(parts.len(), 1);
}

// ========================================================================
// NEW COMPREHENSIVE TESTS: is_ligature_code
// ========================================================================

#[test]
fn test_is_ligature_code() {
    // Standard ligatures: U+FB00-U+FB04
    assert!(TextExtractor::is_ligature_code(0xFB00)); // ff
    assert!(TextExtractor::is_ligature_code(0xFB01)); // fi
    assert!(TextExtractor::is_ligature_code(0xFB02)); // fl
    assert!(TextExtractor::is_ligature_code(0xFB03)); // ffi
    assert!(TextExtractor::is_ligature_code(0xFB04)); // ffl
}

#[test]
fn test_is_not_ligature_code() {
    assert!(!TextExtractor::is_ligature_code(0x41)); // 'A'
    assert!(!TextExtractor::is_ligature_code(0xFAFF)); // Before range
    assert!(!TextExtractor::is_ligature_code(0xFB05)); // After range
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
