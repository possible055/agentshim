use super::*;

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
