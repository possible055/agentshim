use super::*;

#[test]
fn test_char_to_unicode_custom_encoding() {
    // Create a custom encoding map
    let mut custom_map = HashMap::new();
    custom_map.insert(0x41, 'X'); // A -> X
    custom_map.insert(0x42, '•'); // B -> bullet

    let font = FontInfo {
        base_font: "CustomFont".to_string(),
        subtype: "Type1".to_string(),
        encoding: Encoding::Custom(custom_map),
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

    // Should use custom encoding
    assert_eq!(font.char_to_unicode(0x41), Some("X".to_string()));
    assert_eq!(font.char_to_unicode(0x42), Some("•".to_string()));
    // Unmapped character should return None
    assert_eq!(font.char_to_unicode(0x43), None);
}

// =========================================================================
// get_encoded_char
// =========================================================================

#[test]
fn test_get_encoded_char_custom() {
    let mut map = HashMap::new();
    map.insert(0x41, 'X');
    map.insert(0x42, 'Y');
    let font = make_font(|f| {
        f.encoding = Encoding::Custom(map);
    });
    assert_eq!(font.get_encoded_char(0x41), Some('X'));
    assert_eq!(font.get_encoded_char(0x42), Some('Y'));
    assert_eq!(font.get_encoded_char(0x43), None);
}

#[test]
fn test_get_encoded_char_standard_ascii() {
    let font = make_font(|f| {
        f.encoding = Encoding::Standard("WinAnsiEncoding".to_string());
    });
    assert_eq!(font.get_encoded_char(0x41), Some('A'));
    assert_eq!(font.get_encoded_char(0x20), Some(' '));
    // High byte → None (>= 128)
    assert_eq!(font.get_encoded_char(0x80), None);
}

#[test]
fn test_get_encoded_char_identity_ascii() {
    let font = make_font(|f| {
        f.encoding = Encoding::Identity;
    });
    assert_eq!(font.get_encoded_char(0x41), Some('A'));
    assert_eq!(font.get_encoded_char(0x80), None);
}

// =========================================================================
// has_custom_encoding
// =========================================================================

#[test]
fn test_has_custom_encoding_true() {
    let font = make_font(|f| {
        f.encoding = Encoding::Custom(HashMap::new());
    });
    assert!(font.has_custom_encoding());
}

#[test]
fn test_has_custom_encoding_false_standard() {
    let font = make_font(|_| {});
    assert!(!font.has_custom_encoding());
}

#[test]
fn test_has_custom_encoding_false_identity() {
    let font = make_font(|f| {
        f.encoding = Encoding::Identity;
    });
    assert!(!font.has_custom_encoding());
}

// =========================================================================
// char_to_unicode — Symbol font path
// =========================================================================

#[test]
fn test_char_to_unicode_symbol_font() {
    let font = make_font(|f| {
        f.base_font = "Symbol".to_string();
        f.flags = Some(0x04); // Symbolic
        f.encoding = Encoding::Standard("SymbolicBuiltIn".to_string());
    });
    // alpha
    assert_eq!(font.char_to_unicode(0x61), Some("α".to_string()));
    // Sigma
    assert_eq!(font.char_to_unicode(0x53), Some("Σ".to_string()));
    // integral
    assert_eq!(font.char_to_unicode(0xF2), Some("∫".to_string()));
}

#[test]
fn test_char_to_unicode_zapfdingbats_font() {
    let font = make_font(|f| {
        f.base_font = "ZapfDingbats".to_string();
        f.flags = Some(0x04); // Symbolic
        f.encoding = Encoding::Standard("SymbolicBuiltIn".to_string());
    });
    // checkmark
    assert_eq!(font.char_to_unicode(0x33), Some("✓".to_string()));
    // star
    assert_eq!(font.char_to_unicode(0x48), Some("★".to_string()));
}

// =========================================================================
// char_to_unicode — ligature expansion path
// =========================================================================

#[test]
fn test_char_to_unicode_ligature_fallback_expansion() {
    // When no encoding/ToUnicode mapping exists, Priority 6 falls back
    // to standard Unicode ligature decomposition (U+FB00–FB06 → components).
    let font = make_font(|f| {
        f.encoding = Encoding::Standard("WinAnsiEncoding".to_string());
    });
    assert_eq!(font.char_to_unicode(0xFB01), Some("fi".to_string()));
    assert_eq!(font.char_to_unicode(0xFB03), Some("ffi".to_string()));
}

// =========================================================================
// char_to_unicode — custom encoding with ligature
// =========================================================================

#[test]
fn test_char_to_unicode_custom_encoding_with_ligature() {
    let mut custom = HashMap::new();
    custom.insert(0x01, '\u{FB01}'); // fi ligature
    let font = make_font(|f| {
        f.encoding = Encoding::Custom(custom);
    });
    // Should expand ligature
    assert_eq!(font.char_to_unicode(0x01), Some("fi".to_string()));
}

#[test]
fn test_char_to_unicode_custom_encoding_multi_char_map() {
    let font = make_font(|f| {
        f.encoding = Encoding::Custom(HashMap::new());
        f.multi_char_map.insert(0x01, "ff".to_string());
    });
    assert_eq!(font.char_to_unicode(0x01), Some("ff".to_string()));
}

// =========================================================================
// char_to_unicode — ToUnicode with U+FFFD (replacement character skip)
// =========================================================================

#[test]
fn test_char_to_unicode_tounicode_fffd_fallback() {
    // A ToUnicode mapping to U+FFFD means the font author explicitly declared
    // "no Unicode equivalent" for this code. Per Fix B (§9.10.2) the function
    // must return U+FFFD and NOT fall through to the encoding-based path.
    let cmap_data = b"beginbfchar\n<0041> <FFFD>\nendbfchar";
    let font = make_font(|f| {
        f.to_unicode = Some(LazyCMap::new(cmap_data.to_vec()));
        f.encoding = Encoding::Standard("WinAnsiEncoding".to_string());
    });
    // ToUnicode says U+FFFD → return U+FFFD, do NOT fall through to WinAnsi 'A'
    assert_eq!(font.char_to_unicode(0x41), Some("\u{FFFD}".to_string()));
}

#[test]
fn test_char_to_unicode_tounicode_control_char_fallback() {
    // A ToUnicode mapping to a C0 control character is filtered by Fix B.
    // The function must return U+FFFD and NOT fall through to the encoding.
    let cmap_data = b"beginbfchar\n<0041> <0001>\nendbfchar";
    let font = make_font(|f| {
        f.to_unicode = Some(LazyCMap::new(cmap_data.to_vec()));
        f.encoding = Encoding::Standard("WinAnsiEncoding".to_string());
    });
    // C0 control char (U+0001) → U+FFFD, do NOT fall through to WinAnsi 'A'
    assert_eq!(font.char_to_unicode(0x41), Some("\u{FFFD}".to_string()));
}

// =========================================================================
// char_to_unicode — Type0 with Identity-H and CIDSystemInfo
// =========================================================================

#[test]
fn test_char_to_unicode_type0_identity_h_with_sysinfo() {
    let font = make_font(|f| {
        f.base_font = "CIDFont+F1".to_string();
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Standard("Identity-H".to_string());
        f.cid_system_info = Some(CIDSystemInfo {
            registry: "Adobe".to_string(),
            ordering: "Identity".to_string(),
            supplement: 0,
        });
        f.cid_to_gid_map = Some(CIDToGIDMap::Identity);
    });
    // Adobe-Identity with no TrueType cmap → CID-as-Unicode fallback
    // For non-control Unicode code points, char_code == Unicode
    assert_eq!(font.char_to_unicode(0x41), Some("A".to_string()));
    assert_eq!(font.char_to_unicode(0x4E2D), Some("\u{4E2D}".to_string())); // 中
}

#[test]
fn test_char_to_unicode_type0_identity_h_no_sysinfo() {
    let font = make_font(|f| {
        f.base_font = "CIDFont+F2".to_string();
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Standard("Identity-H".to_string());
    });
    // No CIDSystemInfo → CID-as-Unicode last resort
    assert_eq!(font.char_to_unicode(0x42), Some("B".to_string()));
}

// =========================================================================
// char_to_unicode — Type0 with Identity encoding (not Standard)
// =========================================================================

#[test]
fn test_char_to_unicode_type0_identity_encoding_cid_as_unicode() {
    let font = make_font(|f| {
        f.base_font = "MyCIDFont".to_string();
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Identity;
        // No TrueType cmap, no CIDToGIDMap → CID-as-Unicode fallback
    });
    assert_eq!(font.char_to_unicode(0x41), Some("A".to_string()));
}

#[test]
fn test_char_to_unicode_type0_identity_encoding_control_char() {
    let font = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Identity;
    });
    // Control char (0x01) should return FFFD because CID-as-Unicode skips controls
    // but the last resort returns FFFD
    let result = font.char_to_unicode(0x01);
    assert_eq!(result, Some("\u{FFFD}".to_string()));
}

// =========================================================================
// char_to_unicode — Identity encoding for simple (non-Type0) fonts
// =========================================================================

#[test]
fn test_char_to_unicode_simple_font_identity() {
    let font = make_font(|f| {
        f.subtype = "Type1".to_string();
        f.encoding = Encoding::Identity;
    });
    assert_eq!(font.char_to_unicode(0x41), Some("A".to_string()));
    assert_eq!(font.char_to_unicode(0x263A), Some("☺".to_string()));
}

// =========================================================================
// char_to_unicode — TrueType StandardEncoding fallback
// =========================================================================

#[test]
fn test_char_to_unicode_truetype_standard_encoding_ascii() {
    let font = make_font(|f| {
        f.subtype = "TrueType".to_string();
        f.encoding = Encoding::Standard("StandardEncoding".to_string());
    });
    // Should use standard encoding lookup for ASCII
    assert_eq!(font.char_to_unicode(0x41), Some("A".to_string()));
}

// =========================================================================
// char_to_unicode — MacRomanEncoding
// =========================================================================

#[test]
fn test_char_to_unicode_macroman_extended() {
    let font = make_font(|f| {
        f.encoding = Encoding::Standard("MacRomanEncoding".to_string());
    });
    assert_eq!(font.char_to_unicode(0x41), Some("A".to_string()));
    // 0x80 → Adieresis (Ä)
    assert_eq!(font.char_to_unicode(0x80), Some("\u{00C4}".to_string()));
}

// =========================================================================
// char_to_unicode — Type0 Identity encoding with CIDToGIDMap + AGL fallback
// =========================================================================

#[test]
fn test_char_to_unicode_type0_identity_agl_fallback() {
    let font = make_font(|f| {
        f.base_font = "SubsetFont+F3".to_string();
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Identity;
        f.cid_to_gid_map = Some(CIDToGIDMap::Identity);
        // No TrueType cmap → AGL fallback path
    });
    // CID 0x41 → GID 0x41 → glyph name "A" → AGL → 'A'
    assert_eq!(font.char_to_unicode(0x41), Some("A".to_string()));
}

// =========================================================================
// char_to_unicode — Type0 RKSJ (Shift-JIS) path
// =========================================================================

#[test]
fn test_char_to_unicode_type0_rksj() {
    let font = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Standard("90ms-RKSJ-H".to_string());
    });
    // ASCII char through Shift-JIS
    assert_eq!(font.char_to_unicode(0x41), Some("A".to_string()));
}

// =========================================================================
// char_to_unicode — Type0 Identity-H/V at Priority 3 fallback
// =========================================================================

#[test]
fn test_char_to_unicode_type0_identity_v() {
    let font = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Standard("Identity-V".to_string());
    });
    // No CIDSystemInfo → CID-as-Unicode last resort
    assert_eq!(font.char_to_unicode(0x42), Some("B".to_string()));
}

// =========================================================================
// char_to_unicode — unknown encoding for simple font
// =========================================================================

#[test]
fn test_char_to_unicode_unknown_standard_encoding() {
    let font = make_font(|f| {
        f.encoding = Encoding::Standard("SomeRandomEncoding".to_string());
    });
    // Unknown encoding falls back to ASCII passthrough for printable
    assert_eq!(font.char_to_unicode(0x41), Some("A".to_string()));
    // Non-ASCII will return None from standard_encoding_lookup
    assert_eq!(font.char_to_unicode(0x80), None);
}

// =========================================================================
// char_to_unicode — Type0 Identity encoding with large CID (> 0xFFFF)
// =========================================================================

#[test]
fn test_char_to_unicode_type0_identity_large_cid() {
    let font = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Identity;
        f.cid_to_gid_map = Some(CIDToGIDMap::Identity);
    });
    // CID > 0xFFFF: TrueType cmap lookup returns early with None,
    // AGL fallback also returns early for large CIDs,
    // then CID-as-Unicode fallback kicks in: 0x10000 is valid Unicode (Linear B Syllable B008 A)
    assert_eq!(font.char_to_unicode(0x10000), Some("\u{10000}".to_string()));
    // But a CID that maps to a control character should return FFFD
    assert_eq!(font.char_to_unicode(0x01), Some("\u{FFFD}".to_string()));
}

// =========================================================================
// char_to_unicode — Type0 predefined CMap fallback (Priority 2b)
// =========================================================================

#[test]
fn test_char_to_unicode_type0_predefined_cmap_japan1() {
    let font = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Identity; // Will be tried at priority 2b
        f.cid_system_info = Some(CIDSystemInfo {
            registry: "Adobe".to_string(),
            ordering: "Japan1".to_string(),
            supplement: 4,
        });
    });
    // CID 843 → Hiragana あ (U+3042) via predefined Japan1 CMap
    assert_eq!(font.char_to_unicode(843), Some("\u{3042}".to_string()));
}
