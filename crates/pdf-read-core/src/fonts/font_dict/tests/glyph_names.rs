use super::*;

// =========================================================================
// glyph_name_to_unicode — extended edge cases
// =========================================================================

#[test]
fn test_glyph_name_to_unicode_tex_math() {
    assert_eq!(glyph_name_to_unicode("square"), Some('\u{25A1}'));
    assert_eq!(glyph_name_to_unicode("emptyset"), Some('\u{2205}'));
    assert_eq!(glyph_name_to_unicode("infty"), Some('\u{221E}'));
    assert_eq!(glyph_name_to_unicode("nabla"), Some('\u{2207}'));
    assert_eq!(glyph_name_to_unicode("forall"), Some('\u{2200}'));
    assert_eq!(glyph_name_to_unicode("checkmark"), Some('\u{2713}'));
}

#[test]
fn test_glyph_name_to_unicode_underscore_compound() {
    // "f_f" should return first component 'f' via AGL
    assert_eq!(glyph_name_to_unicode("f_f"), Some('f'));
    // "T_h" should return first component 'T' via AGL
    assert_eq!(glyph_name_to_unicode("T_h"), Some('T'));
}

#[test]
fn test_glyph_name_to_unicode_uni_format_edge_cases() {
    // Too short (not 7 chars total)
    assert_eq!(glyph_name_to_unicode("uni004"), None);
    // Invalid hex
    assert_eq!(glyph_name_to_unicode("uniZZZZ"), None);
}

#[test]
fn test_glyph_name_to_unicode_u_format_long() {
    // u1F600 = grinning face emoji
    assert_eq!(glyph_name_to_unicode("u1F600"), Some('\u{1F600}'));
}

// =========================================================================
// glyph_name_to_unicode_string — compound names
// =========================================================================

#[test]
fn test_glyph_name_to_unicode_string_simple() {
    // Single char should just return it as string
    assert_eq!(glyph_name_to_unicode_string("A"), Some("A".to_string()));
}

#[test]
fn test_glyph_name_to_unicode_string_compound_ff() {
    // glyph_name_to_unicode("f_f") returns Some('f') — first component via AGL
    // So glyph_name_to_unicode_string wraps it as "f" (single-char short-circuit)
    assert_eq!(glyph_name_to_unicode_string("f_f"), Some("f".to_string()));
}

#[test]
fn test_glyph_name_to_unicode_string_compound_all_known() {
    // Use a compound name where each component is known individually.
    // "T_h" → glyph_name_to_unicode finds 'T' (first component) → returns "T"
    assert_eq!(glyph_name_to_unicode_string("T_h"), Some("T".to_string()));
}

#[test]
fn test_glyph_name_to_unicode_string_compound_unknown_part() {
    // "f_unknownglyph" — glyph_name_to_unicode finds 'f' (first component via underscore rule)
    // So it returns Some("f") not None
    assert_eq!(
        glyph_name_to_unicode_string("f_unknownglyph"),
        Some("f".to_string())
    );
}

#[test]
fn test_glyph_name_to_unicode_string_totally_unknown_compound() {
    // Both parts unknown — should return None
    assert_eq!(glyph_name_to_unicode_string("xyzzy_plugh"), None);
}

#[test]
fn test_glyph_name_to_unicode_string_unknown() {
    assert_eq!(glyph_name_to_unicode_string("totallyunknown"), None);
}

// =========================================================================
// #535 follow-up — unified AGL fallback chain (v0.3.55)
//
// The #535 fix added a robust ToUnicode + embedded-cmap + AGL
// fallback chain in `src/fonts/character_mapper.rs::glyph_name_to_unicode`,
// but the original full-document Type0 / Identity-H call site at
// `font_dict.rs::Font::char_code_to_unicode` was the only consumer. Simple
// fonts, Type1 / CFF embedded encodings, and `/Differences` arrays still
// routed through this `font_dict::glyph_name_to_unicode` entry, which
// lacked the newer chain's variant-suffix stripping (`.alt`, `.sc`,
// `.001`). delegates to the unified chain as a final fallback so
// all callers — including any future inline-image font-resolution path
// (PDF spec §8.9.7) — share the same behaviour.
//
// Refs #535.
// =========================================================================

#[test]
fn glyph_name_with_variant_suffix_resolves_via_unified_chain() {
    // Subset fonts append stylistic-variant tags (`.sc`, `.alt`, `.001`)
    // to the canonical glyph name. The chain strips the suffix
    // returns the base codepoint; this entry now picks that up too.
    assert_eq!(glyph_name_to_unicode("A.sc"), Some('A'));
    assert_eq!(glyph_name_to_unicode("bullet.alt"), Some('\u{2022}'));
    assert_eq!(glyph_name_to_unicode("fi.001"), Some('\u{FB01}'));
    // Unknown base + suffix → still unknown.
    assert_eq!(glyph_name_to_unicode("xyzzy.sc"), None);
}

#[test]
fn glyph_name_string_with_variant_suffix_resolves_via_unified_chain() {
    // Same as above through the multi-codepoint return surface used by
    // /Differences-array parsing.
    assert_eq!(glyph_name_to_unicode_string("A.sc"), Some("A".to_string()));
    assert_eq!(
        glyph_name_to_unicode_string("bullet.alt"),
        Some("\u{2022}".to_string())
    );
    assert_eq!(
        glyph_name_to_unicode_string("fi.001"),
        Some("\u{FB01}".to_string())
    );
}

#[test]
fn unified_chain_does_not_regress_existing_lookups() {
    // Belt-and-suspenders: the canonical AGL names and uniXXXX / uXXXXX
    // synth patterns the old chain handled stay byte-identical.
    assert_eq!(glyph_name_to_unicode("A"), Some('A'));
    assert_eq!(glyph_name_to_unicode("space"), Some(' '));
    assert_eq!(glyph_name_to_unicode("bullet"), Some('\u{2022}'));
    assert_eq!(glyph_name_to_unicode("fi"), Some('\u{FB01}'));
    assert_eq!(glyph_name_to_unicode("uni2022"), Some('\u{2022}'));
    assert_eq!(glyph_name_to_unicode("u1F600"), Some('\u{1F600}'));
    // Unknown stays unknown.
    assert_eq!(glyph_name_to_unicode("totallyunknown"), None);
}

// =========================================================================
// is_ligature_char and expand_ligature_char
// =========================================================================

#[test]
fn test_is_ligature_char_all_variants() {
    assert!(is_ligature_char('\u{FB00}')); // ff
    assert!(is_ligature_char('\u{FB01}')); // fi
    assert!(is_ligature_char('\u{FB02}')); // fl
    assert!(is_ligature_char('\u{FB03}')); // ffi
    assert!(is_ligature_char('\u{FB04}')); // ffl
    assert!(is_ligature_char('\u{FB05}')); // long s + t
    assert!(is_ligature_char('\u{FB06}')); // st
    assert!(!is_ligature_char('A'));
    assert!(!is_ligature_char(' '));
}

#[test]
fn test_expand_ligature_char_all_variants() {
    assert_eq!(expand_ligature_char('\u{FB00}'), Some("ff"));
    assert_eq!(expand_ligature_char('\u{FB01}'), Some("fi"));
    assert_eq!(expand_ligature_char('\u{FB02}'), Some("fl"));
    assert_eq!(expand_ligature_char('\u{FB03}'), Some("ffi"));
    assert_eq!(expand_ligature_char('\u{FB04}'), Some("ffl"));
    assert_eq!(expand_ligature_char('\u{FB05}'), Some("st"));
    assert_eq!(expand_ligature_char('\u{FB06}'), Some("st"));
    assert_eq!(expand_ligature_char('x'), None);
}

#[test]
fn test_normalize_cjk_radical_forms() {
    // Kangxi Radicals (U+2F00–2FDF) → unified ideograph.
    assert_eq!(normalize_cjk_radical_forms("⽋点"), "欠点");
    assert_eq!(normalize_cjk_radical_forms("⽴⾮⾔⾦"), "立非言金");
    // Mixed radical + normal text: only the radical is rewritten.
    assert_eq!(normalize_cjk_radical_forms("実⽴確率"), "実立確率");
    // Fast path: no radical-block char → returned unchanged (incl. fullwidth).
    assert_eq!(normalize_cjk_radical_forms("欠点０１２"), "欠点０１２");
    assert_eq!(normalize_cjk_radical_forms("hello"), "hello");
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
