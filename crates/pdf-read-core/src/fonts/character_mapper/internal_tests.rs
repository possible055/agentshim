use super::*;

#[test]
fn test_ascii_glyph_names() {
    let mapper = CharacterMapper::new();

    // Test ASCII character to glyph name conversion
    assert_eq!(mapper.code_to_glyph_name(0x20), Some("space".to_string()));
    assert_eq!(mapper.code_to_glyph_name(0x41), Some("A".to_string()));
    assert_eq!(mapper.code_to_glyph_name(0x61), Some("a".to_string()));
}

// ==========================================================================
// Adobe-Arabic-1 / Adobe-Persian-1 /Ordering routing
// ==========================================================================

#[test]
fn test_arabic_persian_ordering_maps_to_arabic_block() {
    let mapper = CharacterMapper::new();
    let arabic = PredefinedCMapConfig {
        ordering: "Arabic".to_string(),
    };
    let persian = PredefinedCMapConfig {
        ordering: "Persian".to_string(),
    };

    // CID 0x0627 = ا (ARABIC LETTER ALEF), 0x0641 = ف (ARABIC LETTER FEH).
    assert_eq!(
        mapper.lookup_predefined_cmap(&arabic, 0x0627),
        Some("\u{0627}".to_string()),
        "Arabic ordering must map CID 0x0627 → U+0627"
    );
    assert_eq!(
        mapper.lookup_predefined_cmap(&persian, 0x0641),
        Some("\u{0641}".to_string()),
        "Persian ordering must map CID 0x0641 → U+0641"
    );
}

#[test]
fn test_cjk_orderings_unaffected_by_arabic_arm() {
    // Regression: the new arm must not perturb the Identity path.
    let mapper = CharacterMapper::new();
    let identity = PredefinedCMapConfig {
        ordering: "Identity".to_string(),
    };
    assert_eq!(
        mapper.lookup_predefined_cmap(&identity, 0x0041),
        Some("A".to_string())
    );
}

// ==========================================================================
// Adobe Glyph List §6 + synthetic-name patterns (v0.3.54 #535)
// ==========================================================================
// Covers Priority 3c of the §9.10.2 fallback chain: PostScript glyph names
// coming from the embedded font program's `post` (TrueType) or `charset`
// (CFF) table get mapped to Unicode via AGL exact match, then `uniXXXX`,
// then `uXXXXX`. Used by the new `embedded_glyph_name` lookup in
// `font_dict.rs` to recover bullets and `fi`/`fl` ligatures on Identity-H
// subset fonts without a `CIDToGIDMap`.

#[test]
fn glyph_name_to_unicode_agl_exact_match() {
    // Canonical AGL entries — the bullet/ligature cases that motivate #535.
    assert_eq!(
        glyph_name_to_unicode("bullet"),
        Some("\u{2022}".to_string())
    );
    assert_eq!(glyph_name_to_unicode("fi"), Some("\u{FB01}".to_string()));
    assert_eq!(glyph_name_to_unicode("fl"), Some("\u{FB02}".to_string()));
    // Sanity: ASCII names.
    assert_eq!(glyph_name_to_unicode("A"), Some("A".to_string()));
    assert_eq!(glyph_name_to_unicode("space"), Some(" ".to_string()));
}

#[test]
#[allow(non_snake_case)]
fn glyph_name_to_unicode_uniXXXX_synth() {
    // Per AGL §6, `uni` + 4 hex digits = U+XXXX.
    assert_eq!(
        glyph_name_to_unicode("uni2022"),
        Some("\u{2022}".to_string())
    );
    assert_eq!(
        glyph_name_to_unicode("uniFB01"),
        Some("\u{FB01}".to_string())
    );
    assert_eq!(glyph_name_to_unicode("uni00E9"), Some("é".to_string()));
    // 3 or 5 hex digits → not the uniXXXX pattern, fall through.
    assert_eq!(glyph_name_to_unicode("uni202"), None);
    assert_eq!(glyph_name_to_unicode("uni20221"), None);
    // Non-hex chars → reject.
    assert_eq!(glyph_name_to_unicode("uniGGGG"), None);
}

#[test]
#[allow(non_snake_case)]
fn glyph_name_to_unicode_uXXXXX_synth() {
    // `u` + 4..6 hex digits = U+XXXXX (BMP + SMP + SIP, no surrogates).
    assert_eq!(glyph_name_to_unicode("u2022"), Some("\u{2022}".to_string()));
    assert_eq!(
        glyph_name_to_unicode("u1F600"),
        Some("\u{1F600}".to_string())
    ); // 😀 (SMP)
       // Surrogate codepoint → reject.
    assert_eq!(glyph_name_to_unicode("uD800"), None);
    // Beyond 0x10FFFF → reject (7 hex digits also out of allowed length).
    assert_eq!(glyph_name_to_unicode("u110000"), None);
    // 3 hex digits or 7 hex digits → fall through.
    assert_eq!(glyph_name_to_unicode("u20"), None);
    assert_eq!(glyph_name_to_unicode("u1F60000"), None);
}

#[test]
fn glyph_name_to_unicode_variant_suffix_stripped() {
    // Subset fonts often use `.alt`, `.sc`, `.001`, etc. as stylistic variant
    // markers. The base name before the dot is the canonical glyph; the
    // codepoint is what we want for extraction.
    assert_eq!(glyph_name_to_unicode("A.sc"), Some("A".to_string()));
    assert_eq!(
        glyph_name_to_unicode("bullet.alt"),
        Some("\u{2022}".to_string())
    );
    assert_eq!(
        glyph_name_to_unicode("fi.001"),
        Some("\u{FB01}".to_string())
    );
    // Unknown base name → still None.
    assert_eq!(glyph_name_to_unicode("xyzzy.sc"), None);
}

#[test]
fn glyph_name_to_unicode_unknown_or_control() {
    // Not in AGL, not a synth pattern.
    assert_eq!(glyph_name_to_unicode(""), None);
    assert_eq!(glyph_name_to_unicode(".notdef"), None);
    assert_eq!(glyph_name_to_unicode("xyzzy"), None);
    // uniXXXX for a control char → reject (not useful as text).
    assert_eq!(glyph_name_to_unicode("uni0007"), None); // BEL
}

#[test]
fn test_glyph_name_lookup() {
    let mapper = CharacterMapper::new();

    // Test that Adobe Glyph List lookups work
    assert!(mapper.map_glyph_name("A").is_some());
    assert!(mapper.map_glyph_name("space").is_some());
}

// ===== Tests for Predefined CMap Support (Issue #104 Sub-category 3) =====

#[test]
fn test_predefined_cmap_japan1_ascii() {
    let mut mapper = CharacterMapper::new();
    mapper.set_predefined_cmap(Some(PredefinedCMapConfig {
        ordering: "Japan1".to_string(),
    }));
    // CID 34 -> 'A' (U+0041) in Adobe-Japan1
    // Note: This goes through Priority 2 (glyph list) first for ASCII range.
    // For CIDs outside ASCII range, it falls through to Priority 3.
    let result = mapper.map_character(34);
    assert!(result.is_some());
}

#[test]
fn test_predefined_cmap_japan1_hiragana() {
    let mut mapper = CharacterMapper::new();
    // Clear tounicode_cmap so Priority 3 is reached
    mapper.set_tounicode_cmap(None);
    mapper.set_predefined_cmap(Some(PredefinedCMapConfig {
        ordering: "Japan1".to_string(),
    }));
    // CID 843 -> U+3042 (hiragana 'a') in Adobe-Japan1
    let result = mapper.map_character(843);
    assert_eq!(result, Some("\u{3042}".to_string())); // あ
}

#[test]
fn test_predefined_cmap_gb1_chinese() {
    let mut mapper = CharacterMapper::new();
    mapper.set_predefined_cmap(Some(PredefinedCMapConfig {
        ordering: "GB1".to_string(),
    }));
    // CID 4559 -> U+4E2D (中) in Adobe-GB1
    let result = mapper.map_character(4559);
    assert_eq!(result, Some("\u{4E2D}".to_string())); // 中
}

#[test]
fn test_predefined_cmap_korea1_hangul() {
    let mut mapper = CharacterMapper::new();
    mapper.set_predefined_cmap(Some(PredefinedCMapConfig {
        ordering: "Korea1".to_string(),
    }));
    // CID 1086 -> U+AC00 (가) in Adobe-Korea1
    let result = mapper.map_character(1086);
    assert_eq!(result, Some("\u{AC00}".to_string())); // 가
}

#[test]
fn test_predefined_cmap_cns1() {
    let mut mapper = CharacterMapper::new();
    mapper.set_predefined_cmap(Some(PredefinedCMapConfig {
        ordering: "CNS1".to_string(),
    }));
    // CID 34 -> 'A' (U+0041) in Adobe-CNS1
    let result = mapper.map_character(34);
    assert!(result.is_some());
}

#[test]
fn test_predefined_cmap_identity() {
    let mut mapper = CharacterMapper::new();
    mapper.set_predefined_cmap(Some(PredefinedCMapConfig {
        ordering: "Identity".to_string(),
    }));
    // Identity: CID 0x4E2D == U+4E2D directly
    let result = mapper.map_character(0x4E2D);
    assert_eq!(result, Some("\u{4E2D}".to_string())); // 中
}

#[test]
fn test_predefined_cmap_unknown_ordering() {
    let mut mapper = CharacterMapper::new();
    mapper.set_predefined_cmap(Some(PredefinedCMapConfig {
        ordering: "UnknownCollection".to_string(),
    }));
    // Unknown ordering should fall through to next priority
    // Code 0x4E2D is outside ASCII/WinAnsi range, so no glyph name match
    // With unknown ordering, predefined CMap returns None
    // Falls through to U+FFFD
    let result = mapper.map_character(0x4E2D);
    assert_eq!(result, Some("\u{FFFD}".to_string()));
}

#[test]
fn test_predefined_cmap_not_set() {
    let mapper = CharacterMapper::new();
    // Without predefined CMap set, mapper should still work for ASCII
    assert_eq!(mapper.map_character(0x41), Some("A".to_string()));
}

#[test]
fn test_tounicode_overrides_predefined_cmap() {
    use super::super::cmap::parse_tounicode_cmap;

    let mut mapper = CharacterMapper::new();
    mapper.set_predefined_cmap(Some(PredefinedCMapConfig {
        ordering: "Japan1".to_string(),
    }));

    // Create a simple ToUnicode CMap that maps CID 843 to 'X'
    let cmap_data = b"/CIDInit /ProcSet findresource begin\n\
            12 dict begin\n\
            begincmap\n\
            /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
            /CMapName /Adobe-Identity-UCS def\n\
            1 beginbfchar\n\
            <034B> <0058>\n\
            endbfchar\n\
            endcmap\n\
            CMapName currentdict /CMap defineresource pop\n\
            end\n\
            end";

    if let Ok(cmap) = parse_tounicode_cmap(cmap_data) {
        mapper.set_tounicode_cmap(Some(cmap));
    }

    // ToUnicode (Priority 1) should override predefined CMap (Priority 3)
    let result = mapper.map_character(843); // 0x034B
    assert_eq!(result, Some("X".to_string()));
}

#[test]
fn test_predefined_cmap_config_clone() {
    let config = PredefinedCMapConfig {
        ordering: "Japan1".to_string(),
    };
    let cloned = config.clone();
    assert_eq!(cloned.ordering, "Japan1");
}

// Regression test for #363: subset Type0 font with Identity-H and an
// incomplete ToUnicode CMap. When the CMap is present but misses a CID,
// the old fallback chain would either:
//   - Priority 2 map the CID through the ASCII glyph list (treating cid
//     0x25 as '%' even though the font's CID 0x25 is some other letter)
//   - Priority 3 with Identity ordering return `cid as u32` directly
//
// Both produce ASCII-shifted "ciphertext" like `%B+$%8A//$2*%01*1%6APP` in
// real PDFs (nougat_035.pdf page 13 was the original symptom).
//
// Per ISO 32000-1 §9.10.2: when a ToUnicode CMap is attached to a font,
// it is authoritative. A miss must produce U+FFFD, not be papered over
// by a code-as-ASCII or CID-as-Unicode heuristic.
#[test]
fn tounicode_miss_on_identity_h_font_returns_replacement() {
    use super::super::cmap::parse_tounicode_cmap;

    let mut mapper = CharacterMapper::new();
    mapper.set_predefined_cmap(Some(PredefinedCMapConfig {
        ordering: "Identity".to_string(),
    }));

    // Minimal ToUnicode: only CID 0x0001 → 'T'. 0x0025 is absent.
    let cmap_data = b"/CIDInit /ProcSet findresource begin\n\
            12 dict begin\n\
            begincmap\n\
            /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
            /CMapName /Adobe-Identity-UCS def\n\
            1 beginbfchar\n\
            <0001> <0054>\n\
            endbfchar\n\
            endcmap\n\
            CMapName currentdict /CMap defineresource pop\n\
            end\n\
            end";
    mapper.set_tounicode_cmap(Some(parse_tounicode_cmap(cmap_data).unwrap()));

    // ToUnicode hit: baseline sanity.
    assert_eq!(mapper.map_character(0x0001), Some("T".to_string()));

    // ToUnicode miss: must return U+FFFD, not '%' (AGL) and not U+0025 (Identity).
    let result = mapper.map_character(0x0025);
    assert_eq!(
        result,
        Some("\u{FFFD}".to_string()),
        "ToUnicode-present-but-missed must produce U+FFFD, got {result:?}"
    );
}

// Guard: simple fonts (Type1 / TrueType with WinAnsiEncoding) that have no
// ToUnicode CMap must still resolve codes via the Adobe Glyph List. The
// #363 fix only activates when a ToUnicode CMap is attached.
#[test]
fn no_tounicode_still_uses_adobe_glyph_list() {
    let mapper = CharacterMapper::new();
    assert_eq!(mapper.map_character(0x41), Some("A".to_string()));
    assert_eq!(mapper.map_character(0x25), Some("%".to_string()));
}
