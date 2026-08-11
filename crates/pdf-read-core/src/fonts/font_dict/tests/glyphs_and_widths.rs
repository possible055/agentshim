use super::*;

/// Test CIDToGIDMap Identity mapping
/// Per PDF Spec ISO 32000-1:2008, Section 9.7.4.2
#[test]
fn test_cid_to_gid_identity() {
    let identity_map = CIDToGIDMap::Identity;

    // In identity mapping, CID == GID
    assert_eq!(identity_map.get_gid(0), 0);
    assert_eq!(identity_map.get_gid(100), 100);
    assert_eq!(identity_map.get_gid(0xFFFF), 0xFFFF);
}

/// Test CIDToGIDMap Explicit mapping
/// Verifies that explicit GID arrays are looked up correctly
#[test]
fn test_cid_to_gid_explicit() {
    // Create explicit mapping: CID 0→10, CID 1→20, CID 2→30
    let gid_array = vec![10, 20, 30];
    let explicit_map = CIDToGIDMap::Explicit(gid_array);

    assert_eq!(explicit_map.get_gid(0), 10);
    assert_eq!(explicit_map.get_gid(1), 20);
    assert_eq!(explicit_map.get_gid(2), 30);

    // Out of range - falls back to identity
    assert_eq!(explicit_map.get_gid(3), 3);
    assert_eq!(explicit_map.get_gid(100), 100);
}

// ==================================================================================
// Extended Latin AGL Fallback Tests
// ==================================================================================
// These tests verify that Type0 fonts with Identity CMap can recover unmapped
// characters using the Adobe Glyph List fallback for extended Latin characters
// (0x80-0xFF range).

#[test]
fn test_gid_to_glyph_name_ascii_range() {
    // Verify ASCII printable range (0x20-0x7E) is still working
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x20), Some("space"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x41), Some("A"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x61), Some("a"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x30), Some("zero"));
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0x7E),
        Some("asciitilde")
    );
}

#[test]
fn test_gid_to_glyph_name_windows1252_symbols() {
    // Test Windows-1252 extended symbols (0x80-0x9F range)
    // These are commonly found in Western European PDFs

    // Currency and special symbols
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x80), Some("euro"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x83), Some("florin"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x85), Some("ellipsis"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x8C), Some("OE"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x9C), Some("oe"));

    // Diacritical marks
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x8A), Some("Scaron"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x9A), Some("scaron"));

    // Smart quotes and dashes
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0x91),
        Some("quoteleft")
    );
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0x92),
        Some("quoteright")
    );
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0x93),
        Some("quotedblleft")
    );
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0x94),
        Some("quotedblright")
    );
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x96), Some("endash"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x97), Some("emdash"));
}

#[test]
fn test_gid_to_glyph_name_latin1_supplement() {
    // Test Latin-1 Supplement range (0xA0-0xFF)
    // These cover Western European languages (French, Spanish, German, etc.)

    // Currency and symbols
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xA2), Some("cent"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xA3), Some("sterling"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xA4), Some("currency"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xA5), Some("yen"));
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xA9),
        Some("copyright")
    );
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xAE),
        Some("registered")
    );

    // Math symbols
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xB0), Some("degree"));
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xB1),
        Some("plusminus")
    );
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xD7), Some("multiply"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xF7), Some("divide"));
}

#[test]
fn test_gid_to_glyph_name_uppercase_accented() {
    // Test uppercase Latin letters with diacritical marks
    // These are essential for French (accented A, E), Spanish (N with tilde), German (Umlaut)
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xC0), Some("Agrave"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xC1), Some("Aacute"));
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xC2),
        Some("Acircumflex")
    );
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xC3), Some("Atilde"));
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xC4),
        Some("Adieresis")
    );
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xC5), Some("Aring"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xC6), Some("AE"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xC7), Some("Ccedilla"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xD1), Some("Ntilde"));
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xD6),
        Some("Odieresis")
    );
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xDC),
        Some("Udieresis")
    );
}

#[test]
fn test_gid_to_glyph_name_lowercase_accented() {
    // Test lowercase Latin letters with diacritical marks
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xE0), Some("agrave"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xE1), Some("aacute"));
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xE2),
        Some("acircumflex")
    );
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xE3), Some("atilde"));
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xE4),
        Some("adieresis")
    );
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xE5), Some("aring"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xE6), Some("ae"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xE7), Some("ccedilla"));
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xF1), Some("ntilde"));
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xF6),
        Some("odieresis")
    );
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xFC),
        Some("udieresis")
    );
}

#[test]
fn test_gid_to_glyph_name_special_characters() {
    // Test ordinal indicators and special characters
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xAA),
        Some("ordfeminine")
    );
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xBA),
        Some("ordmasculine")
    );
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xB2),
        Some("twosuperior")
    );
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xB3),
        Some("threesuperior")
    );
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xB9),
        Some("onesuperior")
    );
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xBC),
        Some("onequarter")
    );
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xBD), Some("onehalf"));
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xBE),
        Some("threequarters")
    );
}

#[test]
fn test_gid_to_glyph_name_undefined_codes() {
    // Test that undefined codes in Windows-1252 return None
    // (0x81, 0x8D, 0x8F, 0x90, 0x9D are undefined)
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x81), None);
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x8D), None);
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x8F), None);
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x90), None);
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x9D), None);
}

#[test]
fn test_gid_to_glyph_name_out_of_range() {
    // Test that GIDs outside supported ranges return None
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x100), None);
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x1000), None);
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0xFFFF), None);
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x0000), None);
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x0001), None);
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x001F), None);
}

#[test]
fn test_agl_fallback_euro_sign() {
    // Test that CID 0x80 (Euro sign) maps through AGL correctly
    // This is a real-world case: Type0 fonts without ToUnicode often need Euro mapping
    let glyph_name = FontInfo::gid_to_standard_glyph_name(0x80).expect("0x80 should map to euro");
    assert_eq!(glyph_name, "euro");

    // Verify the glyph exists in AGL
    assert!(ADOBE_GLYPH_LIST.get(glyph_name).is_some());

    // Verify it maps to the correct Unicode
    if let Some(&unicode_char) = ADOBE_GLYPH_LIST.get(glyph_name) {
        assert_eq!(unicode_char as u32, 0x20AC); // Euro sign U+20AC
    }
}

#[test]
fn test_agl_fallback_extended_latin_coverage() {
    // Test that all common extended Latin characters have AGL mappings
    // This ensures the implementation works end-to-end through AGL lookup
    let test_cases = vec![
        (0x80, "euro", 0x20AC),           // Euro sign
        (0x82, "quotesinglbase", 0x201A), // Single low quote
        (0x83, "florin", 0x0192),         // f with hook
        (0x84, "quotedblbase", 0x201E),   // Double low quote
        (0x85, "ellipsis", 0x2026),       // Ellipsis
        (0xA9, "copyright", 0x00A9),      // Copyright
        (0xAE, "registered", 0x00AE),     // Registered
        (0xB0, "degree", 0x00B0),         // Degree
        (0xC1, "Aacute", 0x00C1),         // A acute
        (0xE1, "aacute", 0x00E1),         // a acute
    ];

    for (gid, expected_glyph, expected_unicode) in test_cases {
        // Step 1: GID -> Glyph name
        let glyph_name = FontInfo::gid_to_standard_glyph_name(gid as u16)
            .unwrap_or_else(|| panic!("GID 0x{:02X} should map to a glyph name", gid));
        assert_eq!(glyph_name, expected_glyph);

        // Step 2: Glyph name -> Unicode (via AGL)
        if let Some(&unicode_char) = ADOBE_GLYPH_LIST.get(glyph_name) {
            assert_eq!(unicode_char as u32, expected_unicode);
        } else {
            panic!("Glyph '{}' should exist in Adobe Glyph List", glyph_name);
        }
    }
}

#[test]
fn test_agl_fallback_priority_integration() {
    // Integration test: Verify AGL fallback would activate for unmapped Type0 CIDs
    // This simulates the Priority 5 fallback in char_to_unicode()
    //
    // Scenario:
    // 1. Type0 font with Identity-H CMap
    // 2. No ToUnicode CMap
    // 3. No TrueType cmap
    // 4. CID 0xC1 (Á - A with acute accent) - common in Spanish/French documents
    //
    // Expected: CID 0xC1 -> GID 0xC1 -> "Aacute" -> U+00C1

    let glyph_name =
        FontInfo::gid_to_standard_glyph_name(0xC1).expect("GID 0xC1 should map to Aacute");
    assert_eq!(glyph_name, "Aacute");

    // Verify AGL has the mapping
    assert!(ADOBE_GLYPH_LIST.get("Aacute").is_some());

    // Verify correct Unicode
    if let Some(&unicode_char) = ADOBE_GLYPH_LIST.get("Aacute") {
        let result = unicode_char.to_string();
        assert_eq!(unicode_char as u32, 0x00C1);
        assert!(!result.is_empty());
    }
}

// =============================================================================
// Type 0 /W Array (CID Width) Tests - PDF Spec 9.7.4.3
// =============================================================================

#[test]
fn test_get_glyph_width_uses_cid_widths() {
    // Test that get_glyph_width properly uses cid_widths for Type0 fonts
    let mut cid_widths = HashMap::new();
    cid_widths.insert(1u16, 500.0f32);
    cid_widths.insert(2u16, 600.0f32);
    cid_widths.insert(3u16, 700.0f32);

    let font = FontInfo {
        base_font: "CIDFont".to_string(),
        subtype: "Type0".to_string(),
        encoding: Encoding::Identity,
        to_unicode: None,
        font_weight: None,
        flags: None,
        stem_v: None,
        ascent: 0.95,
        descent: -0.35,
        embedded_font_data: None,
        truetype_cmap: std::sync::OnceLock::new(),
        embedded_glyph_names: std::sync::OnceLock::new(),
        is_truetype_font: false,
        widths: None,
        first_char: None,
        last_char: None,
        font_matrix_a: 0.001,
        default_width: 1000.0,
        cid_to_gid_map: None,
        cid_system_info: None,
        cid_font_type: None,
        cid_widths: Some(cid_widths),
        cid_default_width: 1000.0,
        has_explicit_dw: false,
        cff_gid_map: None,
        multi_char_map: HashMap::new(),
        byte_to_char_table: std::sync::OnceLock::new(),
        type0_unicode_memo: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        byte_to_width_table: std::sync::OnceLock::new(),
        weight_memo: std::sync::OnceLock::new(),
        italic_memo: std::sync::OnceLock::new(),
        std14_memo: std::sync::OnceLock::new(),
        diff_glyph_names: std::collections::HashMap::new(),
        wmode: 0,
        cid_vertical_metrics: None,
        cid_default_vertical_metrics: VerticalMetrics::SPEC_DEFAULT,
        cjk_substitution: None,
    };

    // Widths from cid_widths
    assert_eq!(font.get_glyph_width(1), 500.0);
    assert_eq!(font.get_glyph_width(2), 600.0);
    assert_eq!(font.get_glyph_width(3), 700.0);

    // CID not in cid_widths should return cid_default_width
    assert_eq!(font.get_glyph_width(100), 1000.0);
}

#[test]
fn test_get_glyph_width_cid_default_width() {
    // Test that cid_default_width is used when CID is not in cid_widths
    let mut cid_widths = HashMap::new();
    cid_widths.insert(1u16, 500.0f32);

    let font = FontInfo {
        base_font: "CIDFont".to_string(),
        subtype: "Type0".to_string(),
        encoding: Encoding::Identity,
        to_unicode: None,
        font_weight: None,
        flags: None,
        stem_v: None,
        ascent: 0.95,
        descent: -0.35,
        embedded_font_data: None,
        truetype_cmap: std::sync::OnceLock::new(),
        embedded_glyph_names: std::sync::OnceLock::new(),
        is_truetype_font: false,
        widths: None,
        first_char: None,
        last_char: None,
        font_matrix_a: 0.001,
        default_width: 500.0, // Simple font default
        cid_to_gid_map: None,
        cid_system_info: None,
        cid_font_type: None,
        cid_widths: Some(cid_widths),
        cid_default_width: 800.0, // CID default width
        has_explicit_dw: true,    // F15: /DW was explicitly set
        cff_gid_map: None,
        multi_char_map: HashMap::new(),
        byte_to_char_table: std::sync::OnceLock::new(),
        type0_unicode_memo: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        byte_to_width_table: std::sync::OnceLock::new(),
        weight_memo: std::sync::OnceLock::new(),
        italic_memo: std::sync::OnceLock::new(),
        std14_memo: std::sync::OnceLock::new(),
        diff_glyph_names: std::collections::HashMap::new(),
        wmode: 0,
        cid_vertical_metrics: None,
        cid_default_vertical_metrics: VerticalMetrics::SPEC_DEFAULT,
        cjk_substitution: None,
    };

    // CID 1 has explicit width
    assert_eq!(font.get_glyph_width(1), 500.0);

    // Other CIDs use cid_default_width (not default_width) when has_explicit_dw=true
    assert_eq!(font.get_glyph_width(2), 800.0);
    assert_eq!(font.get_glyph_width(999), 800.0);
}

#[test]
fn test_get_glyph_width_no_cid_widths_uses_default() {
    // Test that fonts without cid_widths fall back to default_width
    let font = FontInfo {
        base_font: "SimpleFont".to_string(),
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: None,
        font_weight: None,
        flags: None,
        stem_v: None,
        ascent: 0.95,
        descent: -0.35,
        embedded_font_data: None,
        truetype_cmap: std::sync::OnceLock::new(),
        embedded_glyph_names: std::sync::OnceLock::new(),
        is_truetype_font: false,
        widths: None,
        first_char: None,
        last_char: None,
        font_matrix_a: 0.001,
        default_width: 600.0,
        cid_to_gid_map: None,
        cid_system_info: None,
        cid_font_type: None,
        cid_widths: None,
        cid_default_width: 1000.0,
        has_explicit_dw: false,
        cff_gid_map: None,
        multi_char_map: HashMap::new(),
        byte_to_char_table: std::sync::OnceLock::new(),
        type0_unicode_memo: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        byte_to_width_table: std::sync::OnceLock::new(),
        weight_memo: std::sync::OnceLock::new(),
        italic_memo: std::sync::OnceLock::new(),
        std14_memo: std::sync::OnceLock::new(),
        diff_glyph_names: std::collections::HashMap::new(),
        wmode: 0,
        cid_vertical_metrics: None,
        cid_default_vertical_metrics: VerticalMetrics::SPEC_DEFAULT,
        cjk_substitution: None,
    };

    // All CIDs use default_width when no cid_widths and no widths array
    assert_eq!(font.get_glyph_width(1), 600.0);
    assert_eq!(font.get_glyph_width(65), 600.0);
}

#[test]
fn test_cid_widths_large_range() {
    // Test CID widths with a large range of values (simulating real CJK fonts)
    let mut cid_widths = HashMap::new();
    // Simulate /W array: [1 100 1000] - CIDs 1-100 all have width 1000
    for cid in 1u16..=100 {
        cid_widths.insert(cid, 1000.0f32);
    }
    // Add some individual widths
    cid_widths.insert(200, 500.0);
    cid_widths.insert(201, 600.0);

    let font = FontInfo {
        base_font: "CJKFont".to_string(),
        subtype: "Type0".to_string(),
        encoding: Encoding::Identity,
        to_unicode: None,
        font_weight: None,
        flags: None,
        stem_v: None,
        ascent: 0.95,
        descent: -0.35,
        embedded_font_data: None,
        truetype_cmap: std::sync::OnceLock::new(),
        embedded_glyph_names: std::sync::OnceLock::new(),
        is_truetype_font: false,
        widths: None,
        first_char: None,
        last_char: None,
        font_matrix_a: 0.001,
        default_width: 500.0,
        cid_to_gid_map: None,
        cid_system_info: Some(CIDSystemInfo {
            registry: "Adobe".to_string(),
            ordering: "Japan1".to_string(),
            supplement: 4,
        }),
        cid_font_type: Some("CIDFontType2".to_string()),
        cid_widths: Some(cid_widths),
        cid_default_width: 1000.0,
        has_explicit_dw: false,
        cff_gid_map: None,
        multi_char_map: HashMap::new(),
        byte_to_char_table: std::sync::OnceLock::new(),
        type0_unicode_memo: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        byte_to_width_table: std::sync::OnceLock::new(),
        weight_memo: std::sync::OnceLock::new(),
        italic_memo: std::sync::OnceLock::new(),
        std14_memo: std::sync::OnceLock::new(),
        diff_glyph_names: std::collections::HashMap::new(),
        wmode: 0,
        cid_vertical_metrics: None,
        cid_default_vertical_metrics: VerticalMetrics::SPEC_DEFAULT,
        cjk_substitution: None,
    };

    // Range test
    assert_eq!(font.get_glyph_width(1), 1000.0);
    assert_eq!(font.get_glyph_width(50), 1000.0);
    assert_eq!(font.get_glyph_width(100), 1000.0);

    // Individual widths
    assert_eq!(font.get_glyph_width(200), 500.0);
    assert_eq!(font.get_glyph_width(201), 600.0);

    // F15 fix: has_explicit_dw=false → fall back to default_width (500.0), not cid_default_width.
    // When /DW is not explicit in the PDF, we cannot trust cid_default_width as authoritative.
    assert_eq!(font.get_glyph_width(300), 500.0);
}
