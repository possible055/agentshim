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
