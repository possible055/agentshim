use super::*;

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
