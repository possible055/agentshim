use super::*;

/// #847 M2: a condensed bold heading typeset with no space glyph — the
/// intra-word glyph gaps cluster near zero (tight/overlapping side-bearings)
/// while inter-word gaps sit at ~0.18 em. The split must land between the
/// clusters so a gap of ~0.18 em reads as a word boundary.
#[test]
fn test_bimodal_gap_split_heading() {
    // fs = 20.5; intra-word ~0/negative, inter-word ~3.7pt (0.18 em).
    let gaps = [-0.5, -0.7, -0.3, 3.72, 3.70, 3.68, -0.4, 3.71];
    let split = TextExtractor::bimodal_gap_split(&gaps, 20.5);
    let split = split.expect("clearly bimodal line must yield a split");
    assert!(
        split > 0.0 && split < 3.5,
        "split {split} must separate the ~0 and ~3.7pt clusters"
    );
}

/// A normally-spaced line (all gaps already a full word-space) is NOT
/// bimodal — there is no narrow-gap rescue to perform, so `None`.
#[test]
fn test_bimodal_gap_split_uniform_word_spacing_none() {
    let gaps = [6.0, 6.1, 5.9, 6.05, 5.95];
    assert!(TextExtractor::bimodal_gap_split(&gaps, 12.0).is_none());
}

/// A single word (all gaps intra-word, near zero) has no inter-word
/// cluster — must return `None`, never fabricate a boundary.
#[test]
fn test_bimodal_gap_split_single_word_none() {
    let gaps = [-0.5, -0.7, -0.3, 0.1, -0.4];
    assert!(TextExtractor::bimodal_gap_split(&gaps, 20.5).is_none());
}

/// Multi-level condensed footer: near-zero/overlapping intra-word gaps, a
/// NARROW ~0.10 em word gap (1.14 pt @ 11 pt), AND a wide ~0.25 em real
/// space (2.75 pt) on one line. The split must land just above the
/// intra-word cluster — below the narrow gap — so BOTH the narrow word gap
/// and the wide space read as boundaries (recovering `All` / `rights` in
/// `© ISO 2021 - All rights…`, matching pdfminer/poppler).
#[test]
fn test_bimodal_gap_split_multilevel_footer() {
    let gaps = [-0.1, -0.2, -0.15, 1.14, -0.1, -0.05, 2.75, -0.2];
    let split = TextExtractor::bimodal_gap_split(&gaps, 11.0)
        .expect("a multi-level line must yield a split");
    assert!(
        split > 0.0 && split < 1.14,
        "split {split} must sit below the narrow 1.14pt word gap so both it and the wide space split"
    );
}

/// The narrow-gap rescue's math guard: a full subscript glyph occupying the
/// gap between a variable and the next symbol must be detected (suppress the
/// split, `λᵢr` stays whole), while a mere descender/ascender edge clipping
/// the gap band must NOT (so ordinary prose word gaps are still recovered).
#[test]
fn test_gap_has_intervening_glyph() {
    let r = |x, y, w, h| crate::geometry::Rect {
        x,
        y,
        width: w,
        height: h,
    };
    // `left` ends at x=10, `right` starts at x=24: a 14-unit gap on the
    // baseline band [0, 10].
    let left = r(0.0, 0.0, 10.0, 10.0);
    let right = r(24.0, 0.0, 10.0, 10.0);
    // A subscript glyph centred in the gap (x 13..21 = 8 units ≈ 57% of the
    // 14-unit gap), shifted down but overlapping the band.
    let subscript = r(13.0, -3.0, 8.0, 8.0);
    assert!(
        gap_has_intervening_glyph(&[left, right, subscript], &left, &right),
        "a full subscript occupying the gap must be detected"
    );
    // A descender edge just clipping the gap (x 9..12 = only ~2 units into
    // the 14-unit gap, < 35%) must NOT count.
    let descender_edge = r(9.0, -4.0, 3.0, 6.0);
    assert!(
        !gap_has_intervening_glyph(&[left, right, descender_edge], &left, &right),
        "a descender edge clipping the gap must not be treated as an intervening glyph"
    );
}

#[test]
fn test_snap_run_rotation() {
    let m = |a, b, c, d| Matrix {
        a,
        b,
        c,
        d,
        e: 0.0,
        f: 0.0,
    };
    // Horizontal identity-scale → 0.0 (byte-identical path).
    assert_eq!(snap_run_rotation(&m(12.0, 0.0, 0.0, 12.0)), 0.0);
    // Tiny float noise still counts as horizontal.
    assert_eq!(snap_run_rotation(&m(12.0, 1e-5, -1e-5, 12.0)), 0.0);
    // 90° CCW (a=0, b=+s, c=-s, d=0).
    assert_eq!(snap_run_rotation(&m(0.0, 12.0, -12.0, 0.0)), 90.0);
    // 270° / -90° (a=0, b=-s, c=+s, d=0).
    assert_eq!(snap_run_rotation(&m(0.0, -12.0, 12.0, 0.0)), -90.0);
    // ~88° snaps to 90.
    let r = 12.0_f32;
    let th = 88.0_f32.to_radians();
    assert_eq!(
        snap_run_rotation(&m(r * th.cos(), r * th.sin(), -r * th.sin(), r * th.cos())),
        90.0
    );
    // 45° watermark is NOT snapped (kept as its own block downstream).
    let th = 45.0_f32.to_radians();
    let got = snap_run_rotation(&m(r * th.cos(), r * th.sin(), -r * th.sin(), r * th.cos()));
    assert!(
        (got - 45.0).abs() < 0.5,
        "45° should pass through, got {got}"
    );
}

#[test]
fn test_text_extractor_new() {
    let extractor = TextExtractor::new();
    assert_eq!(extractor.char_count(), 0);
}

#[test]
fn test_text_extractor_add_font() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);
    assert_eq!(extractor.fonts.len(), 1);
}

#[test]
fn test_extract_simple_text() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 100 700 Td (Hello) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 5); // "Hello"
    assert_eq!(chars[0].char, 'H');
    assert_eq!(chars[1].char, 'e');
    assert_eq!(chars[2].char, 'l');
    assert_eq!(chars[3].char, 'l');
    assert_eq!(chars[4].char, 'o');
}

#[test]
fn test_extract_with_matrix() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 1 0 0 1 100 700 Tm (Hi) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 2);
    assert_eq!(chars[0].char, 'H');
    assert_eq!(chars[1].char, 'i');
    // Position should be around (100, 700)
    assert!(chars[0].bbox.x >= 99.0 && chars[0].bbox.x <= 101.0);
}

/// Regression test for Issue #11: CTM must be applied to text positions
///
/// Per PDF Spec ISO 32000-1:2008 Section 9.4.4, the text rendering matrix is:
/// T_rm = [font_matrix] × T_m × CTM
///
/// This test verifies that when CTM contains a translation, text positions
/// are correctly transformed from text space to user space.
#[test]
fn test_ctm_applied_to_text_position() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // CTM translates by (100, 200), text matrix at origin
    // Final position should be (100, 200), not (0, 0)
    let stream = b"q 1 0 0 1 100 200 cm BT /F1 12 Tf (A) Tj ET Q";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert_eq!(chars[0].char, 'A');
    // Position should be translated by CTM: (100, 200)
    assert!(
        chars[0].bbox.x >= 99.0 && chars[0].bbox.x <= 101.0,
        "X position should be ~100 (got {})",
        chars[0].bbox.x
    );
    assert!(
        chars[0].bbox.y >= 199.0 && chars[0].bbox.y <= 201.0,
        "Y position should be ~200 (got {})",
        chars[0].bbox.y
    );
}

/// Regression test for Issue #11: CTM scaling must affect text positions
///
/// This test verifies that CTM scaling is correctly applied to text positions.
#[test]
fn test_ctm_scaling_applied_to_text_position() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // CTM scales by 2x, text at position (50, 100) in text space
    // Final position should be (100, 200) in user space
    let stream = b"q 2 0 0 2 0 0 cm BT /F1 12 Tf 1 0 0 1 50 100 Tm (B) Tj ET Q";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert_eq!(chars[0].char, 'B');
    // Position should be scaled: (50*2, 100*2) = (100, 200)
    assert!(
        chars[0].bbox.x >= 99.0 && chars[0].bbox.x <= 101.0,
        "X position should be ~100 (got {})",
        chars[0].bbox.x
    );
    assert!(
        chars[0].bbox.y >= 199.0 && chars[0].bbox.y <= 201.0,
        "Y position should be ~200 (got {})",
        chars[0].bbox.y
    );
}

/// Regression test for Issue #11: Combined CTM translation and text matrix
///
/// This test verifies the complete transformation chain works correctly.
#[test]
fn test_ctm_combined_with_text_matrix() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // CTM translates by (50, 50), text matrix positions at (25, 25)
    // Final position should be (75, 75)
    let stream = b"q 1 0 0 1 50 50 cm BT /F1 12 Tf 1 0 0 1 25 25 Tm (C) Tj ET Q";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert_eq!(chars[0].char, 'C');
    // Position: text_matrix(25,25) + CTM_translation(50,50) = (75, 75)
    assert!(
        chars[0].bbox.x >= 74.0 && chars[0].bbox.x <= 76.0,
        "X position should be ~75 (got {})",
        chars[0].bbox.x
    );
    assert!(
        chars[0].bbox.y >= 74.0 && chars[0].bbox.y <= 76.0,
        "Y position should be ~75 (got {})",
        chars[0].bbox.y
    );
}

#[test]
fn test_extract_with_tj_array() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 0 0 Td [(H)(i)] TJ ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 2);
    assert_eq!(chars[0].char, 'H');
    assert_eq!(chars[1].char, 'i');
}

/// Test extraction of multi-byte characters from Type0 fonts (Identity-H)
/// This verifies the fix for Issue #186 where extract_chars() was garbling CJK text.
#[test]
fn test_extract_type0_multibyte_character_extraction() {
    let mut extractor = TextExtractor::new();

    // Create a mock Type0 font with Identity-H encoding
    let mut font = create_test_font();
    font.subtype = "Type0".to_string();
    font.encoding = Encoding::Standard("Identity-H".to_string());

    // Create a valid ToUnicode CMap stream that maps CID 0x4E2D to '中' and 0x6587 to '文'
    let cmap_data = b"
        /CIDInit /ProcSet findresource begin
        12 dict begin
        begincmap
        /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def
        /CMapName /Adobe-Identity-UCS def
        /CMapType 2 def
        1 begincodespacerange <0000> <FFFF> endcodespacerange
        2 beginbfchar
        <4E2D> <4E2D>
        <6587> <6587>
        endbfchar
        endcmap
        CMapName currentdict /CMap defineresource pop
        end
        end
    ";

    // Use public parse_tounicode_cmap to create CMap, then wrap in LazyCMap
    let lazy_cmap = LazyCMap::new(cmap_data.to_vec());
    font.to_unicode = Some(lazy_cmap);

    extractor.add_font("F1".to_string(), font);

    // Content stream with 2-byte CIDs for "中文" (0x4E2D 0x6587)
    let stream = b"BT /F1 12 Tf 0 0 Td <4E2D6587> Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 2);
    assert_eq!(chars[0].char, '中');
    assert_eq!(chars[1].char, '文');
}

#[test]
fn test_extract_color() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT 1 0 0 rg /F1 12 Tf 0 0 Td (R) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 1);
    assert_eq!(chars[0].char, 'R');
    assert_eq!(chars[0].color.r, 1.0);
    assert_eq!(chars[0].color.g, 0.0);
    assert_eq!(chars[0].color.b, 0.0);
}

/// Regression test for a text-only-parser bug where a fill colour set by
/// `scn` *before* the enclosing `BT` was silently dropped, leaving the
/// text drawn in the GraphicsState default (black) instead of the
/// colour the content stream actually requested.
///
/// Root cause: `scan_graphics_region()` (src/content/parser.rs) is used
/// by `parse_and_execute_text_only()` to fast-scan non-text regions
/// looking for the next `BT`. It classified `scn`/`cs`/`sc`/`rg`/`g`/`k`
/// (and friends) as unconditionally "skippable" - correct only when a
/// matching `Q` is guaranteed to revert the change before any `BT`, but
/// wrong at the top level (outside any q/Q scope), where the colour
/// change legitimately persists into the next text object per
/// ISO 32000-1:2008 SS8.4. Reproduces the exact operator sequence found
/// on a real-world govdocs1 slide-deck PDF: a marked-content BDC opens,
/// `scn` sets a blue fill colour *outside* any text object, then `BT`
/// opens the text object that draws the (should-be-blue) heading.
#[test]
fn test_fill_color_scn_before_bt_after_bdc_not_dropped() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"/Shape <</MCID 3 >>BDC \
                    0.2 0.2 0.604 scn \
                    BT /F1 12 Tf 100 700 Td (Blue Heading) Tj ET \
                    EMC";
    let spans = extractor.extract_text_spans(stream).unwrap();

    assert_eq!(spans.len(), 1);
    assert!(
        (spans[0].color.r - 0.2).abs() < 0.01,
        "expected blue fill (0.2, 0.2, 0.604), got {:?}",
        spans[0].color
    );
    assert!((spans[0].color.g - 0.2).abs() < 0.01);
    assert!((spans[0].color.b - 0.604).abs() < 0.01);
}

/// Same bug, second real-world pattern: a `Q` (RestoreGraphicsState)
/// immediately precedes the out-of-text-object `scn`. Reproduces the
/// gold author-block sequence from the same source PDF.
#[test]
fn test_fill_color_scn_after_q_before_bt_not_dropped() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"q 1 0 0 1 0 0 cm Q \
                    1 1 0 scn \
                    BT /F1 12 Tf 100 700 Td (Gold Author) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    assert_eq!(spans.len(), 1);
    assert!(
        (spans[0].color.r - 1.0).abs() < 0.01,
        "expected gold fill (1, 1, 0), got {:?}",
        spans[0].color
    );
    assert!((spans[0].color.g - 1.0).abs() < 0.01);
    assert!((spans[0].color.b - 0.0).abs() < 0.01);
}

/// Must-not-regress guard: `scn` issued *inside* an already-open text
/// object (continuing after a prior `Tj`, still within the same BT/ET)
/// always worked correctly - it goes through the ordinary text-operator
/// parse path, not the non-text `scan_graphics_region` fast scanner.
/// Confirms the fix above did not disturb this working case.
#[test]
fn test_fill_color_scn_inside_open_text_object_still_works() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf 100 700 Td (Black Text) Tj \
                    0.2 0.2 0.604 scn \
                    0 -20 Td (Blue Text) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    assert_eq!(spans.len(), 2);
    assert!(
        (spans[0].color.r - 0.0).abs() < 0.01 && (spans[0].color.b - 0.0).abs() < 0.01,
        "first run should still be default black, got {:?}",
        spans[0].color
    );
    assert!(
        (spans[1].color.r - 0.2).abs() < 0.01,
        "second run should be blue (0.2, 0.2, 0.604), got {:?}",
        spans[1].color
    );
    assert!((spans[1].color.g - 0.2).abs() < 0.01);
    assert!((spans[1].color.b - 0.604).abs() < 0.01);
}

/// Regression test: is_monospace flag must propagate from FontInfo flags
/// through TjBuffer into the final TextSpan.
///
/// When font descriptor flags have bit 0 (FixedPitch) set, spans produced
/// by extract_text_spans() must report is_monospace == true.
/// Conversely, a proportional font (e.g. Helvetica) must yield false.
#[test]
fn test_is_monospace_from_font_flags() {
    // --- Monospace font: flags bit 0 (FixedPitch) set ---
    let mut mono_font = create_test_font();
    mono_font.base_font = "Courier".to_string();
    mono_font.flags = Some(1); // bit 0 = FixedPitch

    let mut extractor = TextExtractor::new();
    extractor.add_font("F1".to_string(), mono_font);

    let stream = b"BT /F1 12 Tf 100 700 Td (Code) Tj ET";
    let spans = extractor.extract_text_spans(stream).unwrap();

    assert!(!spans.is_empty(), "should produce at least one span");
    assert!(
        spans[0].is_monospace,
        "Courier with FixedPitch flag should be monospace, got is_monospace=false"
    );

    // --- Proportional font: no FixedPitch flag ---
    let mut prop_font = create_test_font();
    prop_font.base_font = "Helvetica".to_string();
    prop_font.flags = Some(0); // no FixedPitch

    let mut extractor2 = TextExtractor::new();
    extractor2.add_font("F2".to_string(), prop_font);

    let stream2 = b"BT /F2 12 Tf 100 700 Td (Text) Tj ET";
    let spans2 = extractor2.extract_text_spans(stream2).unwrap();

    assert!(!spans2.is_empty(), "should produce at least one span");
    assert!(
        !spans2[0].is_monospace,
        "Helvetica without FixedPitch flag should not be monospace"
    );

    // --- Name-based heuristic: font name containing MONO ---
    let mut mono_name_font = create_test_font();
    mono_name_font.base_font = "DejaVuSansMono".to_string();
    mono_name_font.flags = None; // no flags at all

    let mut extractor3 = TextExtractor::new();
    extractor3.add_font("F3".to_string(), mono_name_font);

    let stream3 = b"BT /F3 12 Tf 100 700 Td (Mono) Tj ET";
    let spans3 = extractor3.extract_text_spans(stream3).unwrap();

    assert!(!spans3.is_empty(), "should produce at least one span");
    assert!(
        spans3[0].is_monospace,
        "Font named DejaVuSansMono should be detected as monospace via name heuristic"
    );
}

#[test]
fn test_extract_save_restore() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    // Valid PDF: q saves state, Tf changes font size inside, Q restores
    let stream = b"BT /F1 12 Tf q /F1 14 Tf (A) Tj Q (B) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    assert_eq!(chars.len(), 2);
    assert_eq!(chars[0].font_size, 14.0); // Inside q/Q
    assert_eq!(chars[1].font_size, 12.0); // After Q, restored to 12
}

#[test]
fn test_extract_no_font() {
    let mut extractor = TextExtractor::new();
    // Don't add any fonts

    let stream = b"BT /F1 12 Tf (ABC) Tj ET";
    let chars = extractor.extract(stream).unwrap();

    // Should still extract, using identity mapping
    assert_eq!(chars.len(), 3);
}

#[test]
fn test_char_count() {
    let mut extractor = TextExtractor::new();
    assert_eq!(extractor.char_count(), 0);

    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf (Test) Tj ET";
    extractor.extract(stream).unwrap();
    assert_eq!(extractor.char_count(), 4);
}

#[test]
fn test_clear() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);

    let stream = b"BT /F1 12 Tf (Test) Tj ET";
    extractor.extract(stream).unwrap();
    assert_eq!(extractor.char_count(), 4);

    extractor.clear();
    assert_eq!(extractor.char_count(), 0);
}

// ========================================================================
// COVERAGE TESTS: calculate_average_glyph_width
// ========================================================================

#[test]
fn test_calculate_average_glyph_width_no_widths() {
    let extractor = TextExtractor::new();
    let font = create_test_font(); // No widths array
    let avg = extractor.calculate_average_glyph_width(&font);
    assert_eq!(avg, font.default_width);
}

#[test]
fn test_calculate_average_glyph_width_with_widths() {
    let extractor = TextExtractor::new();
    let mut font = create_test_font();
    font.first_char = Some(32);
    font.last_char = Some(126);
    font.widths = Some(vec![500.0; 95]); // 95 printable chars

    let avg = extractor.calculate_average_glyph_width(&font);
    assert!((avg - 500.0).abs() < 0.01);
}

#[test]
fn test_calculate_average_glyph_width_no_first_char() {
    let extractor = TextExtractor::new();
    let mut font = create_test_font();
    font.widths = Some(vec![500.0; 95]);
    font.first_char = None;

    let avg = extractor.calculate_average_glyph_width(&font);
    assert_eq!(avg, font.default_width);
}

#[test]
fn test_calculate_average_glyph_width_no_last_char() {
    let extractor = TextExtractor::new();
    let mut font = create_test_font();
    font.widths = Some(vec![500.0; 95]);
    font.first_char = Some(32);
    font.last_char = None;

    let avg = extractor.calculate_average_glyph_width(&font);
    assert_eq!(avg, font.default_width);
}

// ========================================================================
// COVERAGE TESTS: Adaptive TJ threshold with justified text
// ========================================================================

#[test]
fn test_adaptive_threshold_with_justified_text() {
    let config = TextExtractionConfig {
        use_adaptive_tj_threshold: true,
        word_margin_ratio: 0.1,
        ..TextExtractionConfig::default()
    };
    let mut extractor = TextExtractor::with_config(config);
    extractor.state_stack.current_mut().font_size = 12.0;

    // Simulate justified text (high CV)
    for i in 0..100 {
        extractor
            .tj_offset_history
            .push(if i % 2 == 0 { -50.0 } else { -200.0 });
    }

    let threshold = extractor.calculate_adaptive_tj_threshold();
    // Justified text uses 3x ratio, so threshold should be more negative
    assert!(threshold < 0.0);
}

#[test]
fn test_adaptive_threshold_with_font_name() {
    let config = TextExtractionConfig {
        use_adaptive_tj_threshold: true,
        word_margin_ratio: 0.1,
        ..TextExtractionConfig::default()
    };
    let mut extractor = TextExtractor::with_config(config);
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);
    extractor.state_stack.current_mut().font_size = 12.0;
    extractor.state_stack.current_mut().font_name = Some("F1".to_string());

    let threshold = extractor.calculate_adaptive_tj_threshold();
    assert!(threshold < 0.0);
}

#[test]
fn test_analyze_tj_distribution_zero_mean() {
    let mut extractor = TextExtractor::new();
    // Push offsets that average to near zero
    extractor.tj_offset_history = vec![100.0, -100.0, 100.0, -100.0];
    let (is_justified, cv) = extractor.analyze_tj_distribution();
    // Mean ~0, so CV should be 0 (avoid division by zero)
    assert_eq!(cv, 0.0);
}
