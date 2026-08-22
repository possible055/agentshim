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

// Regression test for #363: subset Type0 font with Identity-H and an
// incomplete ToUnicode CMap. When the CMap is present but misses a CID,
// the old fallback chain would either:
//   - Priority 2 map the CID through the ASCII glyph list (treating cid
//     0x25 as '%' even though the font's CID 0x25 is some other letter)
//
// That produces ASCII-shifted "ciphertext" like `%B+$%8A//$2*%01*1%6APP` in
// real PDFs (nougat_035.pdf page 13 was the original symptom).
//
// Per ISO 32000-1 §9.10.2: when a ToUnicode CMap is attached to a font,
// it is authoritative. A miss must produce U+FFFD, not be papered over
// by a code-as-ASCII heuristic.
#[test]
fn tounicode_miss_on_identity_h_font_returns_replacement() {
    use super::super::cmap::parse_tounicode_cmap;

    let mut mapper = CharacterMapper::new();

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

    // ToUnicode miss: must return U+FFFD, not '%' (AGL).
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
