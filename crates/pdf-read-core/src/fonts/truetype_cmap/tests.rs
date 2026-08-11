use super::*;
use byteorder::{BigEndian, WriteBytesExt};

/// Build a minimal TrueType font with a cmap format 4 table.
fn build_truetype_with_cmap_format4(mappings: &[(u16, u16)]) -> Vec<u8> {
    // We need: sfnt header + table directory (1 table: cmap) + cmap table
    let mut data = Vec::new();

    // ---- sfnt header ----
    data.write_u32::<BigEndian>(0x00010000).unwrap(); // TrueType version
    data.write_u16::<BigEndian>(1).unwrap(); // numTables = 1
    data.write_u16::<BigEndian>(16).unwrap(); // searchRange
    data.write_u16::<BigEndian>(0).unwrap(); // entrySelector
    data.write_u16::<BigEndian>(0).unwrap(); // rangeShift

    // ---- table directory (1 entry) ----
    let cmap_offset: u32 = 12 + 16; // sfnt header (12) + 1 table record (16)
    data.write_u32::<BigEndian>(0x636D6170).unwrap(); // 'cmap' tag
    data.write_u32::<BigEndian>(0).unwrap(); // checksum (unused)
    data.write_u32::<BigEndian>(cmap_offset).unwrap(); // offset
    data.write_u32::<BigEndian>(0).unwrap(); // length (unused)

    // ---- cmap table header ----
    data.write_u16::<BigEndian>(0).unwrap(); // version
    data.write_u16::<BigEndian>(1).unwrap(); // numSubtables = 1

    // subtable record: platform=3 (Windows), encoding=1 (Unicode BMP)
    let subtable_offset: u32 = 4 + 8; // cmap header (4) + 1 record (8)
    data.write_u16::<BigEndian>(3).unwrap(); // platformID
    data.write_u16::<BigEndian>(1).unwrap(); // encodingID
    data.write_u32::<BigEndian>(subtable_offset).unwrap();

    // ---- cmap format 4 subtable ----
    // Build segments from mappings. Each mapping is (charCode, gid).
    // For simplicity, create one segment per mapping + the sentinel 0xFFFF segment.
    let mut segments: Vec<(u16, u16, i16)> = Vec::new(); // (start, end, delta)
    for &(char_code, gid) in mappings {
        let delta = gid as i16 - char_code as i16;
        segments.push((char_code, char_code, delta));
    }
    segments.push((0xFFFF, 0xFFFF, 1)); // sentinel

    let seg_count = segments.len();
    let seg_count_x2 = (seg_count * 2) as u16;

    data.write_u16::<BigEndian>(4).unwrap(); // format
                                             // length placeholder (we'll fill in later)
    let length_pos = data.len();
    data.write_u16::<BigEndian>(0).unwrap(); // length
    data.write_u16::<BigEndian>(0).unwrap(); // language

    data.write_u16::<BigEndian>(seg_count_x2).unwrap();
    data.write_u16::<BigEndian>(0).unwrap(); // searchRange
    data.write_u16::<BigEndian>(0).unwrap(); // entrySelector
    data.write_u16::<BigEndian>(0).unwrap(); // rangeShift

    // endCode array
    for seg in &segments {
        data.write_u16::<BigEndian>(seg.1).unwrap();
    }
    // reserved pad
    data.write_u16::<BigEndian>(0).unwrap();
    // startCode array
    for seg in &segments {
        data.write_u16::<BigEndian>(seg.0).unwrap();
    }
    // idDelta array
    for seg in &segments {
        data.write_i16::<BigEndian>(seg.2).unwrap();
    }
    // idRangeOffset array (all zeros = use delta formula)
    for _ in &segments {
        data.write_u16::<BigEndian>(0).unwrap();
    }

    // Fill in format 4 length
    let fmt4_start = length_pos - 2; // format field
    let fmt4_len = data.len() - fmt4_start;
    let len_bytes = (fmt4_len as u16).to_be_bytes();
    data[length_pos] = len_bytes[0];
    data[length_pos + 1] = len_bytes[1];

    data
}

/// Build a minimal TrueType font with a cmap format 0 table.
/// `glyph_ids` is the 256-entry byte → glyph id array.
fn build_truetype_with_cmap_format0(glyph_ids: [u8; 256]) -> Vec<u8> {
    let mut data = Vec::new();

    // sfnt header
    data.write_u32::<BigEndian>(0x00010000).unwrap();
    data.write_u16::<BigEndian>(1).unwrap(); // numTables
    data.write_u16::<BigEndian>(16).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();

    // table directory
    let cmap_offset: u32 = 12 + 16;
    data.write_u32::<BigEndian>(0x636D6170).unwrap(); // 'cmap'
    data.write_u32::<BigEndian>(0).unwrap(); // checksum
    data.write_u32::<BigEndian>(cmap_offset).unwrap();
    data.write_u32::<BigEndian>(0).unwrap(); // length

    // cmap table header
    data.write_u16::<BigEndian>(0).unwrap(); // version
    data.write_u16::<BigEndian>(1).unwrap(); // numSubtables = 1

    // Use platform (1, 0) — Macintosh / Roman — so the parser routes
    // directly through the format-0 path.
    let subtable_offset: u32 = 4 + 8;
    data.write_u16::<BigEndian>(1).unwrap(); // platformID = Macintosh
    data.write_u16::<BigEndian>(0).unwrap(); // encodingID = Roman
    data.write_u32::<BigEndian>(subtable_offset).unwrap();

    // ---- cmap format 0 subtable ----
    data.write_u16::<BigEndian>(0).unwrap(); // format
    data.write_u16::<BigEndian>(262).unwrap(); // length (fixed for format 0)
    data.write_u16::<BigEndian>(0).unwrap(); // language
    data.extend_from_slice(&glyph_ids);

    data
}

/// Build a minimal TrueType font with a cmap format 6 table.
fn build_truetype_with_cmap_format6(first_code: u16, gids: &[u16]) -> Vec<u8> {
    let mut data = Vec::new();

    // sfnt header
    data.write_u32::<BigEndian>(0x00010000).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u16::<BigEndian>(16).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();

    // table directory
    let cmap_offset: u32 = 12 + 16;
    data.write_u32::<BigEndian>(0x636D6170).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();
    data.write_u32::<BigEndian>(cmap_offset).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();

    // cmap header
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u16::<BigEndian>(3).unwrap(); // platform 3
    data.write_u16::<BigEndian>(1).unwrap(); // encoding 1
    data.write_u32::<BigEndian>(4 + 8).unwrap();

    // format 6
    data.write_u16::<BigEndian>(6).unwrap(); // format
    data.write_u16::<BigEndian>((10 + gids.len() * 2) as u16)
        .unwrap(); // length
    data.write_u16::<BigEndian>(0).unwrap(); // language
    data.write_u16::<BigEndian>(first_code).unwrap();
    data.write_u16::<BigEndian>(gids.len() as u16).unwrap();
    for &gid in gids {
        data.write_u16::<BigEndian>(gid).unwrap();
    }

    data
}

/// Build a minimal TrueType font with a cmap format 12 table.
fn build_truetype_with_cmap_format12(groups: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut data = Vec::new();

    // sfnt header
    data.write_u32::<BigEndian>(0x00010000).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u16::<BigEndian>(16).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();

    // table directory
    let cmap_offset: u32 = 12 + 16;
    data.write_u32::<BigEndian>(0x636D6170).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();
    data.write_u32::<BigEndian>(cmap_offset).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();

    // cmap header
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u16::<BigEndian>(3).unwrap(); // platform 3
    data.write_u16::<BigEndian>(10).unwrap(); // encoding 10 (full repertoire)
    data.write_u32::<BigEndian>(4 + 8).unwrap();

    // format 12
    data.write_u16::<BigEndian>(12).unwrap(); // format
    data.write_u16::<BigEndian>(0).unwrap(); // reserved
    data.write_u32::<BigEndian>((16 + groups.len() * 12) as u32)
        .unwrap(); // length
    data.write_u32::<BigEndian>(0).unwrap(); // language
    data.write_u32::<BigEndian>(groups.len() as u32).unwrap();
    for &(start, end, start_gid) in groups {
        data.write_u32::<BigEndian>(start).unwrap();
        data.write_u32::<BigEndian>(end).unwrap();
        data.write_u32::<BigEndian>(start_gid).unwrap();
    }

    data
}

/// A standalone cmap format-4 subtable body (format … idRangeOffset), one
/// segment per `(code, gid)` mapping plus the `0xFFFF` sentinel.
fn fmt4_subtable(mappings: &[(u16, u16)]) -> Vec<u8> {
    let mut segs: Vec<(u16, u16, i16)> = mappings
        .iter()
        .map(|&(c, g)| (c, c, (g as i32 - c as i32) as i16))
        .collect();
    segs.push((0xFFFF, 0xFFFF, 1));
    let mut s = Vec::new();
    s.write_u16::<BigEndian>(4).unwrap(); // format
    let len_pos = s.len();
    s.write_u16::<BigEndian>(0).unwrap(); // length placeholder
    s.write_u16::<BigEndian>(0).unwrap(); // language
    s.write_u16::<BigEndian>((segs.len() * 2) as u16).unwrap(); // segCountX2
    for _ in 0..3 {
        s.write_u16::<BigEndian>(0).unwrap(); // searchRange/entrySelector/rangeShift
    }
    for seg in &segs {
        s.write_u16::<BigEndian>(seg.1).unwrap(); // endCode
    }
    s.write_u16::<BigEndian>(0).unwrap(); // reserved pad
    for seg in &segs {
        s.write_u16::<BigEndian>(seg.0).unwrap(); // startCode
    }
    for seg in &segs {
        s.write_i16::<BigEndian>(seg.2).unwrap(); // idDelta
    }
    for _ in &segs {
        s.write_u16::<BigEndian>(0).unwrap(); // idRangeOffset
    }
    let len = (s.len() as u16).to_be_bytes();
    s[len_pos] = len[0];
    s[len_pos + 1] = len[1];
    s
}

/// Two-subtable cmap: a `(3,1)` Unicode subtable and a `(3,0)` symbol one.
fn cmap_two_subtables(unicode: &[(u16, u16)], symbol: &[(u16, u16)]) -> Vec<u8> {
    let body_uni = fmt4_subtable(unicode);
    let body_sym = fmt4_subtable(symbol);
    let off_uni = 4 + 2 * 8; // version+numTables + 2 encoding records
    let off_sym = off_uni + body_uni.len();
    let mut t = Vec::new();
    t.write_u16::<BigEndian>(0).unwrap(); // version
    t.write_u16::<BigEndian>(2).unwrap(); // numTables
    for (plat, enc, off) in [(3u16, 1u16, off_uni), (3, 0, off_sym)] {
        t.write_u16::<BigEndian>(plat).unwrap();
        t.write_u16::<BigEndian>(enc).unwrap();
        t.write_u32::<BigEndian>(off as u32).unwrap();
    }
    t.extend(body_uni);
    t.extend(body_sym);
    t
}

/// Assemble an sfnt from `(tag, data)` tables — sorted by tag (the directory
/// is binary-searched), 4-byte aligned. Just enough for `ttf-parser`.
fn assemble(mut tables: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    tables.sort_by_key(|(tag, _)| *tag);
    let mut out = Vec::new();
    out.write_u32::<BigEndian>(0x0001_0000).unwrap(); // sfnt version
    out.write_u16::<BigEndian>(tables.len() as u16).unwrap();
    for _ in 0..3 {
        out.write_u16::<BigEndian>(0).unwrap(); // searchRange/entrySelector/rangeShift
    }
    let mut offset = 12 + tables.len() * 16;
    for (tag, data) in &tables {
        out.write_u32::<BigEndian>(*tag).unwrap();
        out.write_u32::<BigEndian>(0).unwrap(); // checksum (unchecked)
        out.write_u32::<BigEndian>(offset as u32).unwrap();
        out.write_u32::<BigEndian>(data.len() as u32).unwrap();
        offset += (data.len() + 3) & !3;
    }
    for (_, data) in &tables {
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    out
}

/// Regression for the symbolic-font Ç→Ê bug, exercising the real
/// `from_font_data` → `ttf-parser` extraction path. The content byte is NOT
/// the GID: the `(3,0)` subtable maps byte 3 → GID 2 (the `Ç` glyph), while
/// the byte used directly as a GID hits GID 3 → `Ê` (the old behaviour).
#[test]
fn symbolic_byte_resolves_via_symbol_cmap() {
    // head/hhea/maxp are the minimum ttf-parser's Face::parse requires.
    let mut head = Vec::new();
    head.write_u16::<BigEndian>(1).unwrap(); // majorVersion
    head.write_u16::<BigEndian>(0).unwrap(); // minorVersion
    head.write_u32::<BigEndian>(0).unwrap(); // fontRevision
    head.write_u32::<BigEndian>(0).unwrap(); // checkSumAdjustment
    head.write_u32::<BigEndian>(0x5F0F_3CF5).unwrap(); // magicNumber
    head.write_u16::<BigEndian>(0).unwrap(); // flags
    head.write_u16::<BigEndian>(1000).unwrap(); // unitsPerEm (offset 18)
    head.write_u64::<BigEndian>(0).unwrap(); // created
    head.write_u64::<BigEndian>(0).unwrap(); // modified
    for v in [0i16, 0, 1000, 1000, 0, 0, 0, 0, 0] {
        head.write_i16::<BigEndian>(v).unwrap(); // xMin..glyphDataFormat (incl. indexToLocFormat=0)
    }
    assert_eq!(head.len(), 54);

    let mut hhea = Vec::new();
    hhea.write_u16::<BigEndian>(1).unwrap(); // majorVersion
    hhea.write_u16::<BigEndian>(0).unwrap(); // minorVersion
    for v in [800i16, -200, 0, 1000, 0, 0, 1000, 1, 0, 0, 0, 0, 0, 0, 0] {
        hhea.write_i16::<BigEndian>(v).unwrap();
    }
    hhea.write_u16::<BigEndian>(1).unwrap(); // numberOfHMetrics (offset 34)
    assert_eq!(hhea.len(), 36);

    let mut maxp = Vec::new();
    maxp.write_u32::<BigEndian>(0x0000_5000).unwrap(); // version 0.5
    maxp.write_u16::<BigEndian>(4).unwrap(); // numGlyphs

    let cmap = cmap_two_subtables(&[(0x00C7, 2), (0x00CA, 3)], &[(3, 2)]);
    let font = assemble(vec![
        (0x6865_6164, head), // 'head'
        (0x6868_6561, hhea), // 'hhea'
        (0x6D61_7870, maxp), // 'maxp'
        (0x636D_6170, cmap), // 'cmap'
    ]);

    let parsed = TrueTypeCMap::from_font_data(&font).expect("parse synthetic font");
    assert_eq!(parsed.code_to_gid(3), Some(2)); // byte → GID via (3,0)
    assert_eq!(parsed.get_unicode(2), Some('Ç')); // fixed: byte → GID → Unicode
    assert_eq!(parsed.get_unicode(3), Some('Ê')); // old byte-as-GID path was wrong
    assert_eq!(parsed.code_to_gid(99), None); // no symbol entry → fallback to byte-as-GID
}

#[test]
fn test_sfnt_header_parsing() {
    // Valid TrueType with empty cmap format 4
    let data = build_truetype_with_cmap_format4(&[]);
    let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
    assert!(cmap.is_empty());
}

#[test]
fn test_invalid_sfnt_version() {
    let mut data = vec![0u8; 100];
    // Invalid version bytes
    data[0] = 0xFF;
    data[1] = 0xFF;
    data[2] = 0xFF;
    data[3] = 0xFF;
    let result = TrueTypeCMap::from_font_data(&data);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Invalid sfnt version"));
}

#[test]
fn test_opentype_version_accepted() {
    // Build data with OTTO version
    let mut data = build_truetype_with_cmap_format4(&[(65, 1)]); // 'A' -> gid 1
                                                                 // Replace version with OTTO (0x4F54544F)
    data[0] = 0x4F;
    data[1] = 0x54;
    data[2] = 0x54;
    data[3] = 0x4F;
    let result = TrueTypeCMap::from_font_data(&data);
    assert!(result.is_ok());
}

#[test]
fn test_apple_truetype_version_accepted() {
    let mut data = build_truetype_with_cmap_format4(&[(65, 1)]);
    // Replace version with "true" (0x74727565)
    data[0] = 0x74;
    data[1] = 0x72;
    data[2] = 0x75;
    data[3] = 0x65;
    let result = TrueTypeCMap::from_font_data(&data);
    assert!(result.is_ok());
}

#[test]
fn test_no_cmap_table() {
    let mut data = Vec::new();
    // sfnt header with 1 table but NOT cmap
    data.write_u32::<BigEndian>(0x00010000).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u16::<BigEndian>(16).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    // table record for 'head' (not 'cmap')
    data.write_u32::<BigEndian>(0x68656164).unwrap(); // 'head'
    data.write_u32::<BigEndian>(0).unwrap();
    data.write_u32::<BigEndian>(28).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();

    let result = TrueTypeCMap::from_font_data(&data);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("cmap table not found"));
}

#[test]
fn test_format4_basic_ascii() {
    // Map A(65)->1, B(66)->2, C(67)->3
    let data = build_truetype_with_cmap_format4(&[(65, 1), (66, 2), (67, 3)]);
    let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
    assert_eq!(cmap.len(), 3);
    assert_eq!(cmap.get_unicode(1), Some('A'));
    assert_eq!(cmap.get_unicode(2), Some('B'));
    assert_eq!(cmap.get_unicode(3), Some('C'));
    assert_eq!(cmap.get_unicode(4), None);
}

#[test]
fn test_format4_extended_unicode() {
    // Map some non-ASCII: é(233)->10, ñ(241)->11
    let data = build_truetype_with_cmap_format4(&[(233, 10), (241, 11)]);
    let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
    assert_eq!(cmap.get_unicode(10), Some('é'));
    assert_eq!(cmap.get_unicode(11), Some('ñ'));
}

#[test]
fn test_format6_basic() {
    // Format 6: first_code=65, gids=[1, 2, 3] -> maps A->gid1, B->gid2, C->gid3
    let data = build_truetype_with_cmap_format6(65, &[1, 2, 3]);
    let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
    assert_eq!(cmap.len(), 3);
    assert_eq!(cmap.get_unicode(1), Some('A'));
    assert_eq!(cmap.get_unicode(2), Some('B'));
    assert_eq!(cmap.get_unicode(3), Some('C'));
}

#[test]
fn test_format6_non_zero_first_code() {
    // Start at code 48 ('0') for digits
    let data = build_truetype_with_cmap_format6(48, &[10, 11, 12]);
    let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
    assert_eq!(cmap.get_unicode(10), Some('0'));
    assert_eq!(cmap.get_unicode(11), Some('1'));
    assert_eq!(cmap.get_unicode(12), Some('2'));
}

#[test]
fn test_format12_basic() {
    // One group: chars 65-67 -> gids 1-3
    let data = build_truetype_with_cmap_format12(&[(65, 67, 1)]);
    let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
    assert_eq!(cmap.len(), 3);
    assert_eq!(cmap.get_unicode(1), Some('A'));
    assert_eq!(cmap.get_unicode(2), Some('B'));
    assert_eq!(cmap.get_unicode(3), Some('C'));
}

#[test]
fn test_format12_multiple_groups() {
    let data = build_truetype_with_cmap_format12(&[
        (65, 67, 1),  // A-C -> gids 1-3
        (48, 50, 10), // 0-2 -> gids 10-12
    ]);
    let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
    assert_eq!(cmap.len(), 6);
    assert_eq!(cmap.get_unicode(1), Some('A'));
    assert_eq!(cmap.get_unicode(10), Some('0'));
    assert_eq!(cmap.get_unicode(12), Some('2'));
}

#[test]
fn test_get_unicode_missing() {
    let data = build_truetype_with_cmap_format4(&[(65, 1)]);
    let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
    assert_eq!(cmap.get_unicode(999), None);
}

#[test]
fn test_len_and_is_empty() {
    let data_empty = build_truetype_with_cmap_format4(&[]);
    let cmap_empty = TrueTypeCMap::from_font_data(&data_empty).unwrap();
    assert_eq!(cmap_empty.len(), 0);
    assert!(cmap_empty.is_empty());

    let data_one = build_truetype_with_cmap_format4(&[(65, 1)]);
    let cmap_one = TrueTypeCMap::from_font_data(&data_one).unwrap();
    assert_eq!(cmap_one.len(), 1);
    assert!(!cmap_one.is_empty());
}

#[test]
fn test_cmap_format0_byte_indexed() {
    // Build a format-0 cmap where byte code 0x41 ('A') maps to gid 10,
    // 0x42 ('B') to gid 11, and everything else is 0 (.notdef).
    let mut gids = [0u8; 256];
    gids[0x41] = 10;
    gids[0x42] = 11;
    gids[0x7A] = 50;
    let data = build_truetype_with_cmap_format0(gids);
    let cmap = TrueTypeCMap::from_font_data(&data).expect("format 0 parse");
    assert_eq!(cmap.get_unicode(10), Some('A'));
    assert_eq!(cmap.get_unicode(11), Some('B'));
    assert_eq!(cmap.get_unicode(50), Some('z'));
    // Absent glyphs map to nothing.
    assert_eq!(cmap.get_unicode(99), None);
}

#[test]
fn test_cmap_format0_mac_roman_high_half() {
    // Byte 0x8A in Mac Roman is 'ä' (U+00E4), not raw 0x8A.
    let mut gids = [0u8; 256];
    gids[0x41] = 10; // 'A' — ASCII pass-through
    gids[0x8A] = 20; // 'ä' via Mac Roman table
    gids[0xA5] = 30; // '•' bullet via Mac Roman
    let data = build_truetype_with_cmap_format0(gids);
    let cmap = TrueTypeCMap::from_font_data(&data).expect("format 0 parse");
    assert_eq!(cmap.get_unicode(10), Some('A'));
    assert_eq!(
        cmap.get_unicode(20),
        Some('ä'),
        "Mac Roman 0x8A must map to U+00E4 (a-umlaut), not raw 0x8A"
    );
    assert_eq!(cmap.get_unicode(30), Some('•'));
}

#[test]
fn test_cmap_format0_rejects_truncated() {
    // 256-byte array but declared length wrong → truncated.
    let mut data = Vec::new();
    data.write_u32::<BigEndian>(0x00010000).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u16::<BigEndian>(16).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    let cmap_offset: u32 = 12 + 16;
    data.write_u32::<BigEndian>(0x636D6170).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();
    data.write_u32::<BigEndian>(cmap_offset).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u32::<BigEndian>(4 + 8).unwrap();
    data.write_u16::<BigEndian>(0).unwrap(); // format
                                             // Declare the correct length (262) but only append 8 bytes of
                                             // glyphIdArray instead of 256 — parser should detect the
                                             // truncation via read_exact.
    data.write_u16::<BigEndian>(262).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    data.extend_from_slice(&[0u8; 8]);
    let result = TrueTypeCMap::from_font_data(&data);
    assert!(result.is_err());
}

#[test]
fn test_unsupported_cmap_format() {
    let mut data = Vec::new();
    // sfnt header
    data.write_u32::<BigEndian>(0x00010000).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u16::<BigEndian>(16).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    // cmap table directory entry
    let cmap_offset: u32 = 12 + 16;
    data.write_u32::<BigEndian>(0x636D6170).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();
    data.write_u32::<BigEndian>(cmap_offset).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();
    // cmap header
    data.write_u16::<BigEndian>(0).unwrap(); // version
    data.write_u16::<BigEndian>(1).unwrap(); // 1 subtable
    data.write_u16::<BigEndian>(3).unwrap(); // platform 3
    data.write_u16::<BigEndian>(1).unwrap(); // encoding 1
    data.write_u32::<BigEndian>(4 + 8).unwrap(); // subtable offset
                                                 // format 2 (unsupported)
    data.write_u16::<BigEndian>(2).unwrap();

    let result = TrueTypeCMap::from_font_data(&data);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Unsupported cmap format"));
}

#[test]
fn test_unsupported_cmap_version() {
    let mut data = Vec::new();
    // sfnt header
    data.write_u32::<BigEndian>(0x00010000).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u16::<BigEndian>(16).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    // cmap table directory entry
    let cmap_offset: u32 = 12 + 16;
    data.write_u32::<BigEndian>(0x636D6170).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();
    data.write_u32::<BigEndian>(cmap_offset).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();
    // cmap header with invalid version
    data.write_u16::<BigEndian>(99).unwrap(); // version != 0

    let result = TrueTypeCMap::from_font_data(&data);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .contains("Unsupported cmap table version"));
}

#[test]
fn test_no_suitable_subtable() {
    let mut data = Vec::new();
    // sfnt header
    data.write_u32::<BigEndian>(0x00010000).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u16::<BigEndian>(16).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    // cmap table directory entry
    let cmap_offset: u32 = 12 + 16;
    data.write_u32::<BigEndian>(0x636D6170).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();
    data.write_u32::<BigEndian>(cmap_offset).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();
    // cmap header with 0 subtables
    data.write_u16::<BigEndian>(0).unwrap(); // version
    data.write_u16::<BigEndian>(0).unwrap(); // 0 subtables

    let result = TrueTypeCMap::from_font_data(&data);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("No suitable cmap subtable"));
}

#[test]
fn test_truncated_data() {
    // Just a few bytes - not even a valid header
    let data = vec![0u8; 4];
    let result = TrueTypeCMap::from_font_data(&data);
    assert!(result.is_err());
}

#[test]
fn test_clone_and_debug() {
    let data = build_truetype_with_cmap_format4(&[(65, 1)]);
    let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
    let cloned = cmap.clone();
    assert_eq!(cloned.get_unicode(1), Some('A'));
    let debug = format!("{:?}", cmap);
    assert!(debug.contains("TrueTypeCMap"));
}

#[test]
fn test_platform_priority_windows_full_over_bmp() {
    // Build a font with 2 subtables: platform 3/encoding 1 and 3/10
    // The 3/10 (full) should be preferred
    let mut data = Vec::new();

    // sfnt header
    data.write_u32::<BigEndian>(0x00010000).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u16::<BigEndian>(16).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();
    data.write_u16::<BigEndian>(0).unwrap();

    let cmap_offset: u32 = 12 + 16;
    data.write_u32::<BigEndian>(0x636D6170).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();
    data.write_u32::<BigEndian>(cmap_offset).unwrap();
    data.write_u32::<BigEndian>(0).unwrap();

    // cmap header with 2 subtables
    data.write_u16::<BigEndian>(0).unwrap(); // version
    data.write_u16::<BigEndian>(2).unwrap(); // 2 subtables

    // Both point to same subtable (format 12) for simplicity
    let subtable_off: u32 = 4 + 8 * 2; // cmap header + 2 records
                                       // Record 1: platform 3, encoding 1
    data.write_u16::<BigEndian>(3).unwrap();
    data.write_u16::<BigEndian>(1).unwrap();
    data.write_u32::<BigEndian>(subtable_off).unwrap();
    // Record 2: platform 3, encoding 10 (higher priority)
    data.write_u16::<BigEndian>(3).unwrap();
    data.write_u16::<BigEndian>(10).unwrap();
    data.write_u32::<BigEndian>(subtable_off).unwrap();

    // format 12 subtable: one group: A(65)->gid1
    data.write_u16::<BigEndian>(12).unwrap();
    data.write_u16::<BigEndian>(0).unwrap(); // reserved
    data.write_u32::<BigEndian>(28).unwrap(); // length
    data.write_u32::<BigEndian>(0).unwrap(); // language
    data.write_u32::<BigEndian>(1).unwrap(); // 1 group
    data.write_u32::<BigEndian>(65).unwrap();
    data.write_u32::<BigEndian>(65).unwrap();
    data.write_u32::<BigEndian>(1).unwrap();

    let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
    assert_eq!(cmap.get_unicode(1), Some('A'));
}
