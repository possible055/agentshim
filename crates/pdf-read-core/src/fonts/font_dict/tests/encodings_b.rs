use super::*;

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
// get_font_weight — DemiBold name heuristic
// =========================================================================

#[test]
fn test_get_font_weight_demibold() {
    let font = make_font(|f| {
        f.base_font = "MyFont-DemiBold".to_string();
    });
    assert_eq!(font.get_font_weight(), FontWeight::SemiBold);
}

#[test]
fn test_get_font_weight_heavy() {
    let font = make_font(|f| {
        f.base_font = "MyFont-Heavy".to_string();
    });
    assert_eq!(font.get_font_weight(), FontWeight::Black);
}

#[test]
fn test_get_font_weight_ultrabold() {
    let font = make_font(|f| {
        f.base_font = "MyFont-UltraBold".to_string();
    });
    assert_eq!(font.get_font_weight(), FontWeight::ExtraBold);
}

#[test]
fn test_get_font_weight_ultralight() {
    let font = make_font(|f| {
        f.base_font = "MyFont-UltraLight".to_string();
    });
    assert_eq!(font.get_font_weight(), FontWeight::ExtraLight);
}

// =========================================================================
// get_byte_to_char_table
// =========================================================================

#[test]
fn test_get_byte_to_char_table_basic() {
    let font = make_font(|f| {
        f.encoding = Encoding::Standard("WinAnsiEncoding".to_string());
    });
    let table = font.get_byte_to_char_table();
    // ASCII 'A' (0x41 = 65) should be 'A'
    assert_eq!(table[0x41], 'A');
    // space (0x20 = 32)
    assert_eq!(table[0x20], ' ');
    // Control chars (except tab/newline/cr) should be '\0'
    assert_eq!(table[0x01], '\0');
}

#[test]
fn test_get_byte_to_char_table_tab_newline_passthrough() {
    let font = make_font(|f| {
        let mut custom = HashMap::new();
        custom.insert(0x09u8, '\t');
        custom.insert(0x0Au8, '\n');
        custom.insert(0x0Du8, '\r');
        f.encoding = Encoding::Custom(custom);
    });
    let table = font.get_byte_to_char_table();
    assert_eq!(table[0x09], '\t');
    assert_eq!(table[0x0A], '\n');
    assert_eq!(table[0x0D], '\r');
}

// =========================================================================
// get_byte_to_width_table
// =========================================================================

#[test]
fn test_get_byte_to_width_table_basic() {
    let font = make_font(|f| {
        f.widths = Some(vec![200.0, 300.0, 400.0]);
        f.first_char = Some(65); // 'A'
        f.default_width = 500.0;
    });
    let table = font.get_byte_to_width_table();
    assert_eq!(table[65], 200.0);
    assert_eq!(table[66], 300.0);
    assert_eq!(table[67], 400.0);
    // Unmapped code uses default
    assert_eq!(table[0], 500.0);
    assert_eq!(table[100], 500.0);
}

#[test]
fn test_get_byte_to_width_table_no_widths() {
    let font = make_font(|f| {
        f.default_width = 600.0;
    });
    let table = font.get_byte_to_width_table();
    // All entries should be default_width
    for &w in table.iter() {
        assert_eq!(w, 600.0);
    }
}

// =========================================================================
// lookup_predefined_cmap — fallback by ordering alone
// =========================================================================

#[test]
fn test_lookup_predefined_cmap_ordering_fallback_gb1() {
    // Even with non-standard CMap name, ordering "GB1" should work
    let sysinfo = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "GB1".to_string(),
        supplement: 2,
    });
    assert_eq!(
        lookup_predefined_cmap("SomeCustomCMap", &sysinfo, 34),
        Some(0x41)
    );
}

#[test]
fn test_lookup_predefined_cmap_ordering_fallback_japan1() {
    let sysinfo = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "Japan1".to_string(),
        supplement: 4,
    });
    assert_eq!(
        lookup_predefined_cmap("CustomJapanCMap", &sysinfo, 34),
        Some(0x41)
    );
}

#[test]
fn test_lookup_predefined_cmap_ordering_fallback_cns1() {
    let sysinfo = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "CNS1".to_string(),
        supplement: 3,
    });
    assert_eq!(
        lookup_predefined_cmap("CustomCNSCMap", &sysinfo, 34),
        Some(0x41)
    );
}

#[test]
fn test_lookup_predefined_cmap_ordering_fallback_korea1() {
    let sysinfo = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "Korea1".to_string(),
        supplement: 1,
    });
    assert_eq!(
        lookup_predefined_cmap("CustomKoreaCMap", &sysinfo, 34),
        Some(0x41)
    );
}

#[test]
fn test_lookup_predefined_cmap_unknown_ordering() {
    let sysinfo = Some(CIDSystemInfo {
        registry: "Custom".to_string(),
        ordering: "Unknown".to_string(),
        supplement: 0,
    });
    assert_eq!(lookup_predefined_cmap("AnyCMap", &sysinfo, 34), None);
}

// =========================================================================
// truetype_cmap() accessor — non-TrueType font
// =========================================================================

#[test]
fn test_truetype_cmap_not_truetype() {
    let font = make_font(|f| {
        f.is_truetype_font = false;
        f.embedded_font_data = None;
    });
    assert!(font.truetype_cmap().is_none());
}

#[test]
fn test_truetype_cmap_truetype_no_data() {
    let font = make_font(|f| {
        f.is_truetype_font = true;
        f.embedded_font_data = None;
    });
    assert!(font.truetype_cmap().is_none());
}

#[test]
fn test_truetype_cmap_truetype_empty_data() {
    let font = make_font(|f| {
        f.is_truetype_font = true;
        f.embedded_font_data = Some(Arc::new(vec![]));
    });
    assert!(font.truetype_cmap().is_none());
}

#[test]
fn test_truetype_cmap_truetype_invalid_data() {
    let font = make_font(|f| {
        f.is_truetype_font = true;
        f.embedded_font_data = Some(Arc::new(vec![0xFF, 0xFF, 0xFF, 0xFF]));
    });
    // Invalid font data → extraction fails → None
    assert!(font.truetype_cmap().is_none());
}

#[test]
fn test_has_truetype_cmap_no_data() {
    let font = make_font(|f| {
        f.is_truetype_font = false;
    });
    assert!(!font.has_truetype_cmap());
}

// =========================================================================
// set_truetype_cmap
// =========================================================================

#[test]
fn test_set_truetype_cmap_to_none() {
    let mut font = make_font(|_| {});
    font.set_truetype_cmap(None);
    assert!(font.truetype_cmap().is_none());
}

// =========================================================================
// CIDToGIDMap edge cases
// =========================================================================

#[test]
fn test_cid_to_gid_explicit_empty() {
    let map = CIDToGIDMap::Explicit(vec![]);
    // Empty array → all fall back to identity
    assert_eq!(map.get_gid(0), 0);
    assert_eq!(map.get_gid(100), 100);
}

#[test]
fn test_cid_to_gid_explicit_boundary() {
    let map = CIDToGIDMap::Explicit(vec![99, 88]);
    assert_eq!(map.get_gid(0), 99);
    assert_eq!(map.get_gid(1), 88);
    // index 2 is out of bounds → identity
    assert_eq!(map.get_gid(2), 2);
}

#[test]
fn test_cid_to_gid_identity_max() {
    let map = CIDToGIDMap::Identity;
    assert_eq!(map.get_gid(u16::MAX), u16::MAX);
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
// Encoding enum Debug/Clone
// =========================================================================

#[test]
fn test_encoding_identity_clone() {
    let enc = Encoding::Identity;
    let enc2 = enc.clone();
    assert!(matches!(enc2, Encoding::Identity));
}

#[test]
fn test_encoding_custom_clone() {
    let mut map = HashMap::new();
    map.insert(1u8, 'X');
    let enc = Encoding::Custom(map);
    let enc2 = enc.clone();
    match enc2 {
        Encoding::Custom(m) => assert_eq!(m.get(&1), Some(&'X')),
        _ => panic!("Wrong encoding type"),
    }
}

#[test]
fn test_encoding_debug() {
    let enc = Encoding::Standard("WinAnsiEncoding".to_string());
    let debug = format!("{:?}", enc);
    assert!(debug.contains("WinAnsiEncoding"));
}

// =========================================================================
// CIDSystemInfo clone/debug
// =========================================================================

#[test]
fn test_cidsysteminfo_clone() {
    let info = CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "Japan1".to_string(),
        supplement: 6,
    };
    let info2 = info.clone();
    assert_eq!(info2.registry, "Adobe");
    assert_eq!(info2.ordering, "Japan1");
    assert_eq!(info2.supplement, 6);
}

#[test]
fn test_cidsysteminfo_debug() {
    let info = CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "GB1".to_string(),
        supplement: 2,
    };
    let debug = format!("{:?}", info);
    assert!(debug.contains("Adobe"));
    assert!(debug.contains("GB1"));
}

// =========================================================================
// CIDToGIDMap clone/debug
// =========================================================================

#[test]
fn test_cidtogidmap_clone() {
    let map = CIDToGIDMap::Explicit(vec![1, 2, 3]);
    let map2 = map.clone();
    assert_eq!(map2.get_gid(0), 1);
}

#[test]
fn test_cidtogidmap_debug() {
    let map = CIDToGIDMap::Identity;
    let debug = format!("{:?}", map);
    assert!(debug.contains("Identity"));
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

// =========================================================================
// gid_to_standard_glyph_name — boundary checks
// =========================================================================

#[test]
fn test_gid_to_standard_glyph_name_boundary_values() {
    // First valid entry
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x20), Some("space"));
    // Last valid in basic ASCII
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0x7E),
        Some("asciitilde")
    );
    // Just before first valid
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x1F), None);
    // 0x7F (DEL) is not mapped
    assert_eq!(FontInfo::gid_to_standard_glyph_name(0x7F), None);
    // Last valid entry
    assert_eq!(
        FontInfo::gid_to_standard_glyph_name(0xFF),
        Some("ydieresis")
    );
}

// =========================================================================
// glyph_name_to_unicode — AGL completeness spot checks
// =========================================================================
