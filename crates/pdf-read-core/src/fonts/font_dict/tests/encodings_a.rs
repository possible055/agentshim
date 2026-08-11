use super::*;

// =========================================================================
// shift_jis_to_unicode
// =========================================================================

#[test]
fn test_shift_jis_single_byte_ascii() {
    // Single-byte ASCII should decode normally
    assert_eq!(shift_jis_to_unicode(0x41), Some('A'));
    assert_eq!(shift_jis_to_unicode(0x20), Some(' '));
}

#[test]
fn test_shift_jis_two_byte_katakana() {
    // 0x8341 is Shift-JIS for katakana "ア" (U+30A2)
    assert_eq!(shift_jis_to_unicode(0x8341), Some('ア'));
}

#[test]
fn test_shift_jis_invalid() {
    // 0xFFFF is not a valid Shift-JIS sequence
    assert_eq!(shift_jis_to_unicode(0xFFFF), None);
}

// =========================================================================
// standard_encoding_lookup — extended coverage
// =========================================================================

#[test]
fn test_standard_encoding_lookup_standard_encoding_ascii() {
    assert_eq!(
        standard_encoding_lookup("StandardEncoding", b'A'),
        Some("A".to_string())
    );
    assert_eq!(
        standard_encoding_lookup("StandardEncoding", b' '),
        Some(" ".to_string())
    );
}

#[test]
fn test_standard_encoding_lookup_standard_encoding_extended() {
    // StandardEncoding 0xAE → fi ligature (U+FB01)
    assert_eq!(
        standard_encoding_lookup("StandardEncoding", 0xAE),
        Some("\u{FB01}".to_string())
    );
    // 0xD0 → emdash (U+2014)
    assert_eq!(
        standard_encoding_lookup("StandardEncoding", 0xD0),
        Some("\u{2014}".to_string())
    );
    // 0xA1 → exclamdown
    assert_eq!(
        standard_encoding_lookup("StandardEncoding", 0xA1),
        Some("\u{00A1}".to_string())
    );
}

#[test]
fn test_standard_encoding_lookup_standard_encoding_unmapped() {
    // 0x00 is in the control range, outside 32..=126
    assert_eq!(standard_encoding_lookup("StandardEncoding", 0x00), None);
    // 0xB0 is not mapped in StandardEncoding
    assert_eq!(standard_encoding_lookup("StandardEncoding", 0xB0), None);
}

#[test]
fn test_standard_encoding_lookup_macroman_ascii() {
    assert_eq!(
        standard_encoding_lookup("MacRomanEncoding", b'Z'),
        Some("Z".to_string())
    );
}

#[test]
fn test_standard_encoding_lookup_macroman_extended() {
    // 0x80 → Adieresis (U+00C4)
    assert_eq!(
        standard_encoding_lookup("MacRomanEncoding", 0x80),
        Some("\u{00C4}".to_string())
    );
    // 0xD0 → endash (U+2013)
    assert_eq!(
        standard_encoding_lookup("MacRomanEncoding", 0xD0),
        Some("\u{2013}".to_string())
    );
    // 0xCA → NBSP (U+00A0)
    assert_eq!(
        standard_encoding_lookup("MacRomanEncoding", 0xCA),
        Some("\u{00A0}".to_string())
    );
    // 0xF0 → Apple logo (private use U+F8FF)
    assert_eq!(
        standard_encoding_lookup("MacRomanEncoding", 0xF0),
        Some("\u{F8FF}".to_string())
    );
}

#[test]
fn test_standard_encoding_lookup_macroman_unmapped() {
    // 0x00 is control range
    assert_eq!(standard_encoding_lookup("MacRomanEncoding", 0x00), None);
}

#[test]
fn test_standard_encoding_lookup_winansi_extended() {
    // 0x80 → Euro sign (U+20AC)
    assert_eq!(
        standard_encoding_lookup("WinAnsiEncoding", 0x80),
        Some("\u{20AC}".to_string())
    );
    // 0x96 → En dash (U+2013)
    assert_eq!(
        standard_encoding_lookup("WinAnsiEncoding", 0x96),
        Some("\u{2013}".to_string())
    );
    // 0xA0 → NBSP direct ISO-8859-1 mapping
    assert_eq!(
        standard_encoding_lookup("WinAnsiEncoding", 0xA0),
        Some("\u{00A0}".to_string())
    );
}

#[test]
fn test_standard_encoding_lookup_winansi_undefined_holes() {
    // 0x81 is undefined in WinAnsi/Windows-1252
    assert_eq!(standard_encoding_lookup("WinAnsiEncoding", 0x81), None);
    // 0x8D is undefined
    assert_eq!(standard_encoding_lookup("WinAnsiEncoding", 0x8D), None);
}

#[test]
fn test_standard_encoding_lookup_pdfdoc() {
    // 0x80 → bullet (U+2022)
    assert_eq!(
        standard_encoding_lookup("PDFDocEncoding", 0x80),
        Some("\u{2022}".to_string())
    );
    // 0x84 → emdash (U+2014)
    assert_eq!(
        standard_encoding_lookup("PDFDocEncoding", 0x84),
        Some("\u{2014}".to_string())
    );
    // ASCII range
    assert_eq!(
        standard_encoding_lookup("PDFDocEncoding", b'B'),
        Some("B".to_string())
    );
}

#[test]
fn test_standard_encoding_lookup_unknown_encoding() {
    // Unknown encoding: ASCII passthrough for printable chars
    assert_eq!(
        standard_encoding_lookup("SomeWeirdEncoding", b'X'),
        Some("X".to_string())
    );
    // Non-printable or < 32 → None
    assert_eq!(standard_encoding_lookup("SomeWeirdEncoding", 0x01), None);
    // High byte → None (not ASCII)
    assert_eq!(standard_encoding_lookup("SomeWeirdEncoding", 0x80), None);
}

// =========================================================================
// pdfdoc_encoding_lookup
// =========================================================================

#[test]
fn test_pdfdoc_encoding_ascii_range() {
    assert_eq!(pdfdoc_encoding_lookup(0x00), Some('\0'));
    assert_eq!(pdfdoc_encoding_lookup(0x41), Some('A'));
    assert_eq!(pdfdoc_encoding_lookup(0x7F), Some('\x7F'));
}

#[test]
fn test_pdfdoc_encoding_special_range() {
    assert_eq!(pdfdoc_encoding_lookup(0x80), Some('\u{2022}')); // bullet
    assert_eq!(pdfdoc_encoding_lookup(0x85), Some('\u{2013}')); // endash
    assert_eq!(pdfdoc_encoding_lookup(0x93), Some('\u{FB01}')); // fi ligature
    assert_eq!(pdfdoc_encoding_lookup(0x94), Some('\u{FB02}')); // fl ligature
    assert_eq!(pdfdoc_encoding_lookup(0x92), Some('\u{2122}')); // trademark
}

#[test]
fn test_pdfdoc_encoding_undefined() {
    assert_eq!(pdfdoc_encoding_lookup(0x9F), None);
}

#[test]
fn test_pdfdoc_encoding_latin1_range() {
    assert_eq!(pdfdoc_encoding_lookup(0xA0), Some('\u{00A0}')); // NBSP
    assert_eq!(pdfdoc_encoding_lookup(0xFF), Some('\u{00FF}')); // ydieresis
}

// =========================================================================
// symbol_encoding_lookup — extended coverage
// =========================================================================

#[test]
fn test_symbol_encoding_greek_lowercase() {
    assert_eq!(symbol_encoding_lookup(0x61), Some('α'));
    assert_eq!(symbol_encoding_lookup(0x62), Some('β'));
    assert_eq!(symbol_encoding_lookup(0x67), Some('γ'));
    assert_eq!(symbol_encoding_lookup(0x72), Some('ρ'));
    assert_eq!(symbol_encoding_lookup(0x77), Some('ω'));
}

#[test]
fn test_symbol_encoding_greek_uppercase() {
    assert_eq!(symbol_encoding_lookup(0x44), Some('Δ'));
    assert_eq!(symbol_encoding_lookup(0x53), Some('Σ'));
    assert_eq!(symbol_encoding_lookup(0x57), Some('Ω'));
}

#[test]
fn test_symbol_encoding_math_operators() {
    assert_eq!(symbol_encoding_lookup(0xE1), Some('∑')); // summation
    assert_eq!(symbol_encoding_lookup(0xF2), Some('∫')); // integral
    assert_eq!(symbol_encoding_lookup(0xD6), Some('√')); // radical
    assert_eq!(symbol_encoding_lookup(0xB1), Some('±')); // plusminus
    assert_eq!(symbol_encoding_lookup(0xB9), Some('≠')); // notequal
}

#[test]
fn test_symbol_encoding_digits() {
    // Digits 0x30-0x39 map to themselves
    assert_eq!(symbol_encoding_lookup(0x30), Some('0'));
    assert_eq!(symbol_encoding_lookup(0x39), Some('9'));
}

#[test]
fn test_symbol_encoding_punctuation() {
    assert_eq!(symbol_encoding_lookup(0x20), Some(' '));
    assert_eq!(symbol_encoding_lookup(0x2B), Some('+'));
    assert_eq!(symbol_encoding_lookup(0x2D), Some('−')); // minus (not hyphen)
}

#[test]
fn test_symbol_encoding_unmapped() {
    assert_eq!(symbol_encoding_lookup(0x00), None);
    assert_eq!(symbol_encoding_lookup(0x01), None);
}

// =========================================================================
// zapf_dingbats_encoding_lookup — extended coverage
// =========================================================================

#[test]
fn test_zapf_dingbats_common() {
    assert_eq!(zapf_dingbats_encoding_lookup(0x20), Some(' '));
    assert_eq!(zapf_dingbats_encoding_lookup(0x21), Some('✁'));
    assert_eq!(zapf_dingbats_encoding_lookup(0x33), Some('✓')); // checkmark
    assert_eq!(zapf_dingbats_encoding_lookup(0x34), Some('✔')); // bold checkmark
    assert_eq!(zapf_dingbats_encoding_lookup(0x48), Some('★')); // black star
}

#[test]
fn test_zapf_dingbats_geometric() {
    assert_eq!(zapf_dingbats_encoding_lookup(0x6C), Some('●')); // black circle
    assert_eq!(zapf_dingbats_encoding_lookup(0x6F), Some('■')); // black square
}

/// ZapfDingbats circled-digit ranges (Annex D.6); codes in hex of the
/// spec's octal CODE column.
#[test]
fn test_zapf_dingbats_circled_digits() {
    // ① ⑩  (a120–a129, octal 254–265) → U+2460–U+2469
    assert_eq!(zapf_dingbats_encoding_lookup(0xAC), Some('\u{2460}')); // ①
    assert_eq!(zapf_dingbats_encoding_lookup(0xB5), Some('\u{2469}')); // ⑩
                                                                       // ❶ ❿  (a130–a139, octal 266–277) → U+2776–U+277F
    assert_eq!(zapf_dingbats_encoding_lookup(0xB6), Some('\u{2776}')); // ❶
    assert_eq!(zapf_dingbats_encoding_lookup(0xBF), Some('\u{277F}')); // ❿
                                                                       // ➀ ➉  (a140–a149, octal 300–311) → U+2780–U+2789
    assert_eq!(zapf_dingbats_encoding_lookup(0xC0), Some('\u{2780}')); // ➀
    assert_eq!(zapf_dingbats_encoding_lookup(0xC9), Some('\u{2789}')); // ➉
                                                                       // ➊ ➓  (a150–a159, octal 312–323) → U+278A–U+2793
    assert_eq!(zapf_dingbats_encoding_lookup(0xCA), Some('\u{278A}')); // ➊
    assert_eq!(zapf_dingbats_encoding_lookup(0xD3), Some('\u{2793}')); // ➓
}

/// ZapfDingbats arrow ranges (Annex D.6, octal 324–376).
#[test]
fn test_zapf_dingbats_arrows() {
    assert_eq!(zapf_dingbats_encoding_lookup(0xD4), Some('\u{2794}')); // ➔
    assert_eq!(zapf_dingbats_encoding_lookup(0xD5), Some('\u{2192}')); // →
    assert_eq!(zapf_dingbats_encoding_lookup(0xD8), Some('\u{2798}')); // ➘
    assert_eq!(zapf_dingbats_encoding_lookup(0xEF), Some('\u{27AF}')); // ➯
    assert_eq!(zapf_dingbats_encoding_lookup(0xF0), None); // undefined in D.6
    assert_eq!(zapf_dingbats_encoding_lookup(0xF1), Some('\u{27B1}')); // ➱
    assert_eq!(zapf_dingbats_encoding_lookup(0xFE), Some('\u{27BE}')); // ➾
}

#[test]
fn test_zapf_dingbats_unmapped() {
    assert_eq!(zapf_dingbats_encoding_lookup(0x00), None);
    assert_eq!(zapf_dingbats_encoding_lookup(0xFF), None);
}

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

// =========================================================================
// get_glyph_width — simple font widths array
// =========================================================================

#[test]
fn test_get_glyph_width_simple_font_widths_array() {
    let font = make_font(|f| {
        f.widths = Some(vec![200.0, 300.0, 400.0, 500.0]);
        f.first_char = Some(65); // 'A'
        f.last_char = Some(68); // 'D'
        f.default_width = 600.0;
    });
    assert_eq!(font.get_glyph_width(65), 200.0); // 'A'
    assert_eq!(font.get_glyph_width(66), 300.0); // 'B'
    assert_eq!(font.get_glyph_width(67), 400.0); // 'C'
    assert_eq!(font.get_glyph_width(68), 500.0); // 'D'
                                                 // Out of range → default_width
    assert_eq!(font.get_glyph_width(64), 600.0);
    assert_eq!(font.get_glyph_width(69), 600.0);
}

#[test]
fn test_get_glyph_width_below_first_char() {
    let font = make_font(|f| {
        f.widths = Some(vec![250.0]);
        f.first_char = Some(100);
        f.last_char = Some(100);
        f.default_width = 777.0;
    });
    // char_code < first_char → negative index → default
    assert_eq!(font.get_glyph_width(50), 777.0);
}

#[test]
fn test_get_glyph_width_no_widths_no_cid() {
    let font = make_font(|f| {
        f.default_width = 550.0;
    });
    assert_eq!(font.get_glyph_width(65), 550.0);
}

// =========================================================================
// get_space_glyph_width
// =========================================================================

#[test]
fn test_get_space_glyph_width_from_array() {
    let font = make_font(|f| {
        f.widths = Some(vec![250.0]); // only one entry
        f.first_char = Some(32); // space = 0x20 = 32
        f.last_char = Some(32);
    });
    assert_eq!(font.get_space_glyph_width(), 250.0);
}

#[test]
fn test_get_space_glyph_width_default() {
    let font = make_font(|f| {
        f.default_width = 333.0;
    });
    assert_eq!(font.get_space_glyph_width(), 333.0);
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
// is_symbolic — flags and name-based detection
// =========================================================================

#[test]
fn test_is_symbolic_flag_set() {
    let font = make_font(|f| {
        f.flags = Some(0x04); // bit 3 set
    });
    assert!(font.is_symbolic());
}

#[test]
fn test_is_symbolic_flag_not_set() {
    let font = make_font(|f| {
        f.flags = Some(0x20); // nonsymbolic bit only
    });
    assert!(!font.is_symbolic());
}

#[test]
fn test_is_symbolic_no_flags_symbol_name() {
    let font = make_font(|f| {
        f.base_font = "Symbol".to_string();
    });
    assert!(font.is_symbolic());
}

#[test]
fn test_is_symbolic_no_flags_zapf_name() {
    let font = make_font(|f| {
        f.base_font = "ZapfDingbats".to_string();
    });
    assert!(font.is_symbolic());
}

#[test]
fn test_is_symbolic_no_flags_normal_name() {
    let font = make_font(|f| {
        f.base_font = "Helvetica".to_string();
    });
    assert!(!font.is_symbolic());
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
