use super::*;

#[test]
fn cipher_discriminator_flags_disagreeing_builtin_encoding() {
    // A subset cipher: ASCII-letter codes resolve to unrelated glyphs, so
    // they disagree with WinAnsi on every overlapping code (0/N agree).
    let cipher: HashMap<u8, char> = [(b'A', 'ñ'), (b'B', 'k'), (b'C', 'º'), (b'D', 'p')]
        .into_iter()
        .collect();
    assert!(builtin_encoding_looks_like_cipher(
        &cipher,
        "WinAnsiEncoding"
    ));
}

#[test]
fn cipher_discriminator_keeps_mostly_agreeing_builtin_encoding() {
    // A real text encoding: agrees with the named base on most codes (a
    // single non-standard slot is not enough to look like a cipher).
    let real: HashMap<u8, char> = [(b'A', 'A'), (b'B', 'B'), (b'C', 'C'), (0xCA, ' ')]
        .into_iter()
        .collect();
    assert!(!builtin_encoding_looks_like_cipher(
        &real,
        "WinAnsiEncoding"
    ));
}

#[test]
fn cipher_discriminator_no_overlap_is_not_a_cipher() {
    // No codes overlap the named base's mapped range → no evidence → not a
    // cipher (preserve the prior overlay behaviour).
    let empty: HashMap<u8, char> = HashMap::new();
    assert!(!builtin_encoding_looks_like_cipher(
        &empty,
        "WinAnsiEncoding"
    ));
}

#[test]
fn test_standard_encoding_ascii() {
    assert_eq!(
        standard_encoding_lookup("WinAnsiEncoding", b'A'),
        Some("A".to_string())
    );
    assert_eq!(
        standard_encoding_lookup("WinAnsiEncoding", b'Z'),
        Some("Z".to_string())
    );
    assert_eq!(
        standard_encoding_lookup("WinAnsiEncoding", b'0'),
        Some("0".to_string())
    );
}

#[test]
fn test_standard_encoding_space() {
    assert_eq!(
        standard_encoding_lookup("WinAnsiEncoding", b' '),
        Some(" ".to_string())
    );
}

#[test]
fn test_font_info_is_bold() {
    let font = FontInfo {
        base_font: "Times-Bold".to_string(),
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: None,
        font_weight: Some(700),
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
    assert!(font.is_bold());

    let font2 = FontInfo {
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
    assert!(!font2.is_bold());
}

#[test]
fn test_font_info_is_italic() {
    let font = FontInfo {
        base_font: "Times-Italic".to_string(),
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
    assert!(font.is_italic());

    let font2 = FontInfo {
        base_font: "Courier-Oblique".to_string(),
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
    assert!(font2.is_italic());
}

#[test]
fn test_char_to_unicode_with_tounicode() {
    // Create a simple CMap with one custom mapping
    let cmap_data = b"beginbfchar\n<0041> <0058>\nendbfchar"; // Map 0x41 to 'X'

    let font = FontInfo {
        base_font: "CustomFont".to_string(),
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: Some(LazyCMap::new(cmap_data.to_vec())),
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

    // Should use ToUnicode mapping (priority)
    assert_eq!(font.char_to_unicode(0x41), Some("X".to_string()));
    // Should fall back to standard encoding
    assert_eq!(font.char_to_unicode(0x42), Some("B".to_string()));
}

#[test]
fn test_char_to_unicode_standard_encoding() {
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

    assert_eq!(font.char_to_unicode(0x41), Some("A".to_string()));
    assert_eq!(font.char_to_unicode(0x20), Some(" ".to_string()));
}

#[test]
fn test_char_to_unicode_identity() {
    // Test Type0 font WITHOUT ToUnicode - should return U+FFFD per PDF Spec 9.10.2
    let font_type0 = FontInfo {
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

    // Type0 without ToUnicode should use CID-as-Unicode fallback
    assert_eq!(font_type0.char_to_unicode(0x41), Some("A".to_string()));
    assert_eq!(
        font_type0.char_to_unicode(0x263A),
        Some("\u{263A}".to_string())
    );

    // Test Type1 font WITH Identity encoding - should work correctly
    let font_type1 = FontInfo {
        base_font: "TimesRoman".to_string(),
        subtype: "Type1".to_string(),
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

    // Simple fonts (Type1) CAN use Identity encoding for valid Unicode codes
    assert_eq!(font_type1.char_to_unicode(0x41), Some("A".to_string()));
    assert_eq!(font_type1.char_to_unicode(0x263A), Some("☺".to_string()));
}

#[test]
fn test_lookup_predefined_cmap_adobe_gb1() {
    // Test Adobe-GB1 (Simplified Chinese) CMap lookup
    let cid_system_info = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "GB1".to_string(),
        supplement: 2,
    });

    // Test ASCII from CID (CID 34 -> 'A')
    assert_eq!(
        lookup_predefined_cmap("UniGB-UCS2-H", &cid_system_info, 34),
        Some(0x41)
    );

    // Test known CJK mapping (CID 4559 -> U+4E2D "中")
    assert_eq!(
        lookup_predefined_cmap("UniGB-UCS2-H", &cid_system_info, 4559),
        Some(0x4E2D)
    );

    // Test unknown CID
    assert_eq!(
        lookup_predefined_cmap("UniGB-UCS2-H", &cid_system_info, 50000),
        None
    );

    // Test without CIDSystemInfo (should return None)
    assert_eq!(lookup_predefined_cmap("UniGB-UCS2-H", &None, 34), None);
}

#[test]
fn test_lookup_predefined_cmap_adobe_japan1() {
    // Test Adobe-Japan1 (Japanese) CMap lookup
    let cid_system_info = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "Japan1".to_string(),
        supplement: 4,
    });

    // Test ASCII from CID (CID 34 -> 'A')
    assert_eq!(
        lookup_predefined_cmap("UniJIS-UCS2-H", &cid_system_info, 34),
        Some(0x41)
    );

    // Test Hiragana from CID (CID 843 -> あ U+3042)
    assert_eq!(
        lookup_predefined_cmap("UniJIS-UCS2-H", &cid_system_info, 843),
        Some(0x3042)
    );

    // Test unknown CID
    assert_eq!(
        lookup_predefined_cmap("UniJIS-UCS2-H", &cid_system_info, 50000),
        None
    );
}

#[test]
fn test_lookup_predefined_cmap_adobe_cns1() {
    // Test Adobe-CNS1 (Traditional Chinese) CMap lookup
    let cid_system_info = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "CNS1".to_string(),
        supplement: 3,
    });

    // Test ASCII from CID (CID 34 -> 'A')
    assert_eq!(
        lookup_predefined_cmap("UniCNS-UCS2-H", &cid_system_info, 34),
        Some(0x41)
    );

    // Test CJK from CID (CID 595 -> 一 U+4E00)
    assert_eq!(
        lookup_predefined_cmap("UniCNS-UCS2-H", &cid_system_info, 595),
        Some(0x4E00)
    );
}

#[test]
fn test_lookup_predefined_cmap_adobe_korea1() {
    // Test Adobe-Korea1 (Korean) CMap lookup
    let cid_system_info = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "Korea1".to_string(),
        supplement: 1,
    });

    // Test ASCII from CID (CID 34 -> 'A')
    assert_eq!(
        lookup_predefined_cmap("UniKS-UCS2-H", &cid_system_info, 34),
        Some(0x41)
    );

    // Test Hangul from CID (CID 1086 -> 가 U+AC00)
    assert_eq!(
        lookup_predefined_cmap("UniKS-UCS2-H", &cid_system_info, 1086),
        Some(0xAC00)
    );
}

#[test]
fn test_lookup_predefined_cmap_adobe_arabic_persian() {
    // Adobe-Arabic-1 / Adobe-Persian-1 CIDFonts without /ToUnicode (Nazanin,
    // Yagut, Mitra, Lotus): §9.10.3 step-3 identity fallback over the Arabic
    // block; without it these decode as Latin-Extended-B garbage.
    let arabic = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "Arabic".to_string(),
        supplement: 0,
    });
    let persian = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "Persian".to_string(),
        supplement: 0,
    });

    // CID 0x0627 = ا (ARABIC LETTER ALEF), 0x0641 = ف (ARABIC LETTER FEH).
    assert_eq!(
        lookup_predefined_cmap("Identity-H", &arabic, 0x0627),
        Some(0x0627)
    );
    assert_eq!(
        lookup_predefined_cmap("Identity-H", &persian, 0x0641),
        Some(0x0641)
    );
}

#[test]
fn test_lookup_predefined_cmap_wrong_ordering() {
    // Test that lookup fails if CIDSystemInfo ordering doesn't match
    let cid_system_info_wrong = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "WrongOrdering".to_string(),
        supplement: 1,
    });

    // Should return None because ordering doesn't match
    assert_eq!(
        lookup_predefined_cmap("UniGB-UCS2-H", &cid_system_info_wrong, 34),
        None
    );
}

#[test]
fn test_encoding_clone() {
    let enc = Encoding::Standard("WinAnsiEncoding".to_string());
    let enc2 = enc.clone();
    match enc2 {
        Encoding::Standard(name) => assert_eq!(name, "WinAnsiEncoding"),
        _ => panic!("Wrong encoding type"),
    }
}

#[test]
fn test_font_info_clone() {
    let font = FontInfo {
        base_font: "Test".to_string(),
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

    let font2 = font.clone();
    assert_eq!(font2.base_font, "Test");
}

#[test]
fn test_glyph_name_to_unicode_basic() {
    assert_eq!(glyph_name_to_unicode("A"), Some('A'));
    assert_eq!(glyph_name_to_unicode("a"), Some('a'));
    assert_eq!(glyph_name_to_unicode("zero"), Some('0'));
    assert_eq!(glyph_name_to_unicode("nine"), Some('9'));
}

#[test]
fn test_glyph_name_to_unicode_punctuation() {
    assert_eq!(glyph_name_to_unicode("space"), Some(' '));
    assert_eq!(glyph_name_to_unicode("quotesingle"), Some('\''));
    assert_eq!(glyph_name_to_unicode("grave"), Some('`'));
    assert_eq!(glyph_name_to_unicode("hyphen"), Some('-'));
    // Official AGL: "minus" maps to U+2212 (MINUS SIGN), not U+002D (HYPHEN-MINUS)
    assert_eq!(glyph_name_to_unicode("minus"), Some('−'));
}

#[test]
fn test_glyph_name_to_unicode_special() {
    assert_eq!(glyph_name_to_unicode("bullet"), Some('•'));
    assert_eq!(glyph_name_to_unicode("dagger"), Some('†'));
    assert_eq!(glyph_name_to_unicode("daggerdbl"), Some('‡'));
    assert_eq!(glyph_name_to_unicode("ellipsis"), Some('…'));
    assert_eq!(glyph_name_to_unicode("emdash"), Some('—'));
    assert_eq!(glyph_name_to_unicode("endash"), Some('–'));
}

#[test]
fn test_glyph_name_to_unicode_quotes() {
    assert_eq!(glyph_name_to_unicode("quotesinglbase"), Some('‚'));
    assert_eq!(glyph_name_to_unicode("quotedblbase"), Some('„'));
    // Official AGL uses proper curly quotes, not straight quotes
    assert_eq!(glyph_name_to_unicode("quotedblleft"), Some('\u{201C}')); // LEFT DOUBLE QUOTATION MARK
    assert_eq!(glyph_name_to_unicode("quotedblright"), Some('\u{201D}')); // RIGHT DOUBLE QUOTATION MARK
    assert_eq!(glyph_name_to_unicode("quoteleft"), Some('\u{2018}'));
    assert_eq!(glyph_name_to_unicode("quoteright"), Some('\u{2019}'));
}

#[test]
fn test_glyph_name_to_unicode_accented() {
    assert_eq!(glyph_name_to_unicode("Aacute"), Some('Á'));
    assert_eq!(glyph_name_to_unicode("aacute"), Some('á'));
    assert_eq!(glyph_name_to_unicode("Ntilde"), Some('Ñ'));
    assert_eq!(glyph_name_to_unicode("ntilde"), Some('ñ'));
}

#[test]
fn test_glyph_name_to_unicode_currency() {
    assert_eq!(glyph_name_to_unicode("Euro"), Some('€'));
    assert_eq!(glyph_name_to_unicode("sterling"), Some('£'));
    assert_eq!(glyph_name_to_unicode("yen"), Some('¥'));
    assert_eq!(glyph_name_to_unicode("cent"), Some('¢'));
}

#[test]
fn test_glyph_name_to_unicode_ligatures() {
    assert_eq!(glyph_name_to_unicode("fi"), Some('ﬁ'));
    assert_eq!(glyph_name_to_unicode("fl"), Some('ﬂ'));
    assert_eq!(glyph_name_to_unicode("ffi"), Some('ﬃ'));
}

#[test]
fn test_glyph_name_to_unicode_uni_xxxx() {
    // Test uni format (4 hex digits)
    assert_eq!(glyph_name_to_unicode("uni0041"), Some('A'));
    assert_eq!(glyph_name_to_unicode("uni2022"), Some('•'));
}

#[test]
fn test_glyph_name_to_unicode_u_xxxx() {
    // Test u format (variable hex digits)
    assert_eq!(glyph_name_to_unicode("u0041"), Some('A'));
    assert_eq!(glyph_name_to_unicode("u2022"), Some('•'));
}

#[test]
fn test_glyph_name_to_unicode_unknown() {
    assert_eq!(glyph_name_to_unicode("unknownglyph"), None);
    assert_eq!(glyph_name_to_unicode(""), None);
}
