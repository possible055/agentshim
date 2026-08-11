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

/// Integration Test 1: ForceBold flag detection (PDF Spec Table 123, bit 19)
#[test]
fn test_get_font_weight_force_bold_flag() {
    // Test ForceBold flag set (bit 19 = 0x80000 = 524288)
    let font_with_force_bold = FontInfo {
        base_font: "Helvetica".to_string(),
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: None,
        font_weight: None,    // No explicit weight
        flags: Some(0x80000), // ForceBold flag set
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

    assert_eq!(font_with_force_bold.get_font_weight(), FontWeight::Bold);
    assert!(font_with_force_bold.is_bold());

    // Test without ForceBold flag
    let font_without_force_bold = FontInfo {
        base_font: "Helvetica".to_string(),
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: None,
        font_weight: None,
        flags: Some(0x40000), // Different flag, NOT ForceBold
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

    assert_eq!(
        font_without_force_bold.get_font_weight(),
        FontWeight::Normal
    );
    assert!(!font_without_force_bold.is_bold());
}

/// Integration Test 2: StemV analysis for weight inference
#[test]
fn test_get_font_weight_stem_v_analysis() {
    // Test StemV > 110 → Bold
    let font_heavy_stem = FontInfo {
        base_font: "UnknownFont".to_string(),
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: None,
        font_weight: None,
        flags: None,
        stem_v: Some(120.0), // Heavy stem
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

    assert_eq!(font_heavy_stem.get_font_weight(), FontWeight::Bold);
    assert!(font_heavy_stem.is_bold());

    // Test StemV 80-110 → Medium
    let font_medium_stem = FontInfo {
        base_font: "UnknownFont".to_string(),
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: None,
        font_weight: None,
        flags: None,
        stem_v: Some(95.0), // Medium stem
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

    assert_eq!(font_medium_stem.get_font_weight(), FontWeight::Medium);
    assert!(!font_medium_stem.is_bold());

    // Test StemV < 80 → Normal
    let font_light_stem = FontInfo {
        base_font: "UnknownFont".to_string(),
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: None,
        font_weight: None,
        flags: None,
        stem_v: Some(70.0), // Light stem
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

    assert_eq!(font_light_stem.get_font_weight(), FontWeight::Normal);
    assert!(!font_light_stem.is_bold());
}

/// Integration Test 3: Priority cascade (FontWeight > ForceBold > Name > StemV)
#[test]
fn test_get_font_weight_priority_cascade() {
    // Priority 1: Explicit FontWeight field overrides everything
    let font_explicit = FontInfo {
        base_font: "Helvetica-Bold".to_string(), // Name says Bold
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: None,
        font_weight: Some(300), // But explicit weight is Light
        flags: Some(0x80000),   // ForceBold flag set
        stem_v: Some(120.0),    // Heavy stem
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

    assert_eq!(font_explicit.get_font_weight(), FontWeight::Light);
    assert!(!font_explicit.is_bold());

    // Priority 2: ForceBold overrides name and StemV
    let font_force_bold = FontInfo {
        base_font: "Helvetica".to_string(), // Name says Normal
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: None,
        font_weight: None,    // No explicit weight
        flags: Some(0x80000), // ForceBold flag set
        stem_v: Some(70.0),   // Light stem
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

    assert_eq!(font_force_bold.get_font_weight(), FontWeight::Bold);
    assert!(font_force_bold.is_bold());

    // Priority 3: Name heuristics override StemV
    let font_name = FontInfo {
        base_font: "Helvetica-Bold".to_string(),
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: None,
        font_weight: None,
        flags: None,
        stem_v: Some(70.0), // Light stem, but name says Bold
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

    assert_eq!(font_name.get_font_weight(), FontWeight::Bold);
    assert!(font_name.is_bold());
}
