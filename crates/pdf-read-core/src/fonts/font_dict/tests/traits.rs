use super::*;

#[test]
fn test_glyph_name_to_unicode_math_symbols() {
    assert_eq!(glyph_name_to_unicode("infinity"), Some('∞'));
    assert_eq!(glyph_name_to_unicode("notequal"), Some('≠'));
    assert_eq!(glyph_name_to_unicode("lessequal"), Some('≤'));
    assert_eq!(glyph_name_to_unicode("greaterequal"), Some('≥'));
}

#[test]
fn test_glyph_name_to_unicode_german_sharp_s() {
    assert_eq!(glyph_name_to_unicode("germandbls"), Some('ß'));
}

#[test]
fn test_glyph_name_to_unicode_copyright_registered() {
    assert_eq!(glyph_name_to_unicode("copyright"), Some('©'));
    assert_eq!(glyph_name_to_unicode("registered"), Some('®'));
    assert_eq!(glyph_name_to_unicode("trademark"), Some('™'));
}

// =========================================================================
// char_to_unicode — Type0 Identity-H with non-Identity ordering (CJK)
// =========================================================================

#[test]
fn test_char_to_unicode_type0_identity_h_cjk_ordering() {
    let font = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Standard("Identity-H".to_string());
        f.cid_system_info = Some(CIDSystemInfo {
            registry: "Adobe".to_string(),
            ordering: "Japan1".to_string(),
            supplement: 4,
        });
    });
    // Non-identity ordering with Identity-H: CIDs are NOT Unicode
    // Should use predefined CMap lookup
    // CID 843 → Hiragana あ (U+3042)
    assert_eq!(font.char_to_unicode(843), Some("\u{3042}".to_string()));
}

// =========================================================================
// char_to_unicode — UCS2/UTF16 encoding variant
// =========================================================================

#[test]
fn test_char_to_unicode_type0_ucs2_encoding() {
    let font = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Standard("UniJIS-UCS2-H".to_string());
        f.cid_system_info = Some(CIDSystemInfo {
            registry: "Adobe".to_string(),
            ordering: "Identity".to_string(),
            supplement: 0,
        });
    });
    // UCS2 encoding: char_code IS the Unicode value
    assert_eq!(font.char_to_unicode(0x41), Some("A".to_string()));
}

// =========================================================================
// Standard encoding control char handling
// =========================================================================

#[test]
fn test_standard_encoding_winansi_control_range() {
    // Codes 0-31 are control range — WinAnsi doesn't map these
    assert_eq!(standard_encoding_lookup("WinAnsiEncoding", 0x00), None);
    assert_eq!(standard_encoding_lookup("WinAnsiEncoding", 0x01), None);
    assert_eq!(standard_encoding_lookup("WinAnsiEncoding", 0x1F), None);
}

// =========================================================================
// WinAnsi full extended range spot checks
// =========================================================================

#[test]
fn test_standard_encoding_winansi_full_extended() {
    // 0x85 → Horizontal ellipsis (U+2026)
    assert_eq!(
        standard_encoding_lookup("WinAnsiEncoding", 0x85),
        Some("\u{2026}".to_string())
    );
    // 0x99 → Trade mark sign (U+2122)
    assert_eq!(
        standard_encoding_lookup("WinAnsiEncoding", 0x99),
        Some("\u{2122}".to_string())
    );
    // 0xFF → Latin small letter y with diaeresis
    assert_eq!(
        standard_encoding_lookup("WinAnsiEncoding", 0xFF),
        Some("\u{00FF}".to_string())
    );
}

// ==========================================
// wrap_cff_in_opentype tests
// ==========================================

#[test]
fn test_wrap_cff_in_opentype_header() {
    // Minimal CFF data (version 1.0, hdrSize=4, offSize=1)
    let cff = vec![1, 0, 4, 1, 0, 0, 0, 0];
    let otf = super::wrap_cff_in_opentype(&cff);

    // Must start with 'OTTO' tag
    assert_eq!(&otf[0..4], b"OTTO");
    // numTables = 4 (CFF, head, hhea, maxp)
    assert_eq!(u16::from_be_bytes([otf[4], otf[5]]), 4);
    // Must contain the CFF data at some offset
    assert!(otf.windows(cff.len()).any(|w| w == &cff[..]));
}

#[test]
fn test_wrap_cff_in_opentype_contains_required_tables() {
    let cff = vec![1, 0, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0];
    let otf = super::wrap_cff_in_opentype(&cff);

    // Check all 4 required table tags exist in the table directory
    // Table directory starts at offset 12, each record is 16 bytes
    let mut found_tables = Vec::new();
    for i in 0..4 {
        let offset = 12 + i * 16;
        let tag = std::str::from_utf8(&otf[offset..offset + 4]).unwrap_or("????");
        found_tables.push(tag.to_string());
    }
    found_tables.sort();
    assert_eq!(found_tables, vec!["CFF ", "head", "hhea", "maxp"]);
}

#[test]
fn test_wrap_cff_in_opentype_parseable() {
    // Create a minimal but valid CFF font stub
    let cff = vec![1, 0, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let otf = super::wrap_cff_in_opentype(&cff);

    // ttf-parser should be able to parse the header (head + hhea + maxp)
    // without panicking, even if CFF data is minimal
    let result = ttf_parser::Face::parse(&otf, 0);
    // May fail on CFF content but should not panic on table parsing
    // The fact that it doesn't panic is the test
    let _ = result;
}

// ==========================================
// get_standard_font_width tests
// ==========================================

#[test]
fn test_standard_font_width_times_roman() {
    let font = FontInfo {
        base_font: "Times-Roman".to_string(),
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
        cid_to_gid_map: None,
        cid_system_info: None,
        cid_font_type: None,
        widths: None, // No widths → should use standard metrics
        first_char: None,
        last_char: None,
        font_matrix_a: 0.001,
        default_width: 500.0,
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

    // 'A' = 722 in Times-Roman (not the default 500)
    assert_eq!(font.get_glyph_width(65), 722.0);
    // 'i' = 278 (narrow)
    assert_eq!(font.get_glyph_width(105), 278.0);
    // space = 250
    assert_eq!(font.get_glyph_width(32), 250.0);
    // 'm' = 778 (wide)
    assert_eq!(font.get_glyph_width(109), 778.0);
}

#[test]
fn test_standard_font_width_courier_monospace() {
    let font = FontInfo {
        base_font: "Courier".to_string(),
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
        cid_to_gid_map: None,
        cid_system_info: None,
        cid_font_type: None,
        widths: None,
        first_char: None,
        last_char: None,
        font_matrix_a: 0.001,
        default_width: 500.0,
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

    // Courier is monospace — all chars 600
    assert_eq!(font.get_glyph_width(65), 600.0); // A
    assert_eq!(font.get_glyph_width(105), 600.0); // i
    assert_eq!(font.get_glyph_width(32), 600.0); // space
}

#[test]
fn test_standard_font_width_not_applied_with_widths_array() {
    let font = FontInfo {
        base_font: "Times-Roman".to_string(),
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
        cid_to_gid_map: None,
        cid_system_info: None,
        cid_font_type: None,
        widths: Some(vec![999.0]), // Has explicit widths
        first_char: Some(65),      // Starting at 'A'
        last_char: Some(65),
        font_matrix_a: 0.001,
        default_width: 500.0,
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

    // Should use explicit width (999), not standard Times width (722)
    assert_eq!(font.get_glyph_width(65), 999.0);
}

#[test]
fn test_standard_font_width_not_applied_to_unknown_font() {
    let font = FontInfo {
        base_font: "MyCustomFont".to_string(),
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
        cid_to_gid_map: None,
        cid_system_info: None,
        cid_font_type: None,
        widths: None,
        first_char: None,
        last_char: None,
        font_matrix_a: 0.001,
        default_width: 500.0,
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

    // Unknown font → should fall back to default_width (500)
    assert_eq!(font.get_glyph_width(65), 500.0);
}

/// Pins the standard-14 fallback path in `get_byte_to_width_table`:
/// when `widths` is `None`, the table must be populated from
/// `get_standard_font_width` (PDF spec Appendix D metrics), not
/// from `default_width`. Also pins the fallback-within-the-fallback
/// for byte codes that don't appear in the standard-14 table —
/// those still use `default_width`.
#[test]
fn fallback_uses_standard_14_metrics_when_widths_absent() {
    let font = FontInfo {
        base_font: "Helvetica".to_string(),
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: None,
        font_weight: Some(400),
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

    let table = font.get_byte_to_width_table();

    // Standard-14 Helvetica metrics (PDF spec Appendix D).
    assert_eq!(table[32], 278.0, "space");
    assert_eq!(table[48], 556.0, "digit '0'");
    assert_eq!(table[65], 667.0, "'A'");
    assert_eq!(table[87], 944.0, "'W'");

    // Byte codes not in the standard-14 table fall back to default_width.
    assert_eq!(table[0], 1000.0, "NUL -> default_width fallback");
}

// ────────────────────────────────────────────────────────────────────────────
// Fix A / B / C tests (§9.10.2 Priority-3 guard + control filter + OOB CID)
// ────────────────────────────────────────────────────────────────────────────
