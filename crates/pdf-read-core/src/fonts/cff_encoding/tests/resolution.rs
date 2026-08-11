use super::*;

// ==========================================
// resolve_glyph_name tests
// ==========================================

#[test]
fn test_resolve_glyph_name_predefined() {
    let string_index: Vec<&[u8]> = vec![];
    assert_eq!(
        resolve_glyph_name(0, &string_index),
        Some(".notdef".to_string())
    );
    assert_eq!(
        resolve_glyph_name(1, &string_index),
        Some("space".to_string())
    );
    assert_eq!(resolve_glyph_name(34, &string_index), Some("A".to_string()));
    assert_eq!(
        resolve_glyph_name(390, &string_index),
        Some("Semibold".to_string())
    );
}

#[test]
fn test_resolve_glyph_name_custom_string() {
    let custom: Vec<&[u8]> = vec![b"MyGlyph", b"AnotherGlyph"];
    // SID 391 => index 0 in string_index
    assert_eq!(
        resolve_glyph_name(391, &custom),
        Some("MyGlyph".to_string())
    );
    // SID 392 => index 1
    assert_eq!(
        resolve_glyph_name(392, &custom),
        Some("AnotherGlyph".to_string())
    );
}

#[test]
fn test_resolve_glyph_name_custom_out_of_range() {
    let custom: Vec<&[u8]> = vec![b"OnlyOne"];
    assert_eq!(resolve_glyph_name(393, &custom), None); // index 2, but only 1 entry
}

#[test]
fn test_resolve_glyph_name_custom_invalid_utf8() {
    let invalid_utf8: Vec<&[u8]> = vec![&[0xFF, 0xFE]];
    assert_eq!(resolve_glyph_name(391, &invalid_utf8), None);
}

// ==========================================
// extract_cff_from_opentype tests
// ==========================================

#[test]
fn test_extract_cff_from_opentype_too_short() {
    assert_eq!(extract_cff_from_opentype(&[0; 8]), None);
}

#[test]
fn test_extract_cff_from_opentype_not_opentype() {
    let data = vec![0x00; 16];
    assert_eq!(extract_cff_from_opentype(&data), None);
}

#[test]
fn test_extract_cff_from_opentype_otto_no_cff_table() {
    // "OTTO" magic, 0 tables
    let data = vec![
        0x4F, 0x54, 0x54, 0x4F, // "OTTO"
        0x00, 0x00, // num_tables = 0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // searchRange, entrySelector, rangeShift
    ];
    assert_eq!(extract_cff_from_opentype(&data), None);
}

#[test]
fn test_extract_cff_from_opentype_with_cff_table() {
    let cff_data = b"\x01\x00\x04\x01"; // Minimal CFF header
    let cff_offset: u32 = 28; // 12 (header) + 16 (one table record)
    let cff_length: u32 = cff_data.len() as u32;

    let mut data = vec![
        0x4F, 0x54, 0x54, 0x4F, // "OTTO"
        0x00, 0x01, // num_tables = 1
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // searchRange etc
    ];
    // Table record: tag "CFF ", checksum, offset, length
    data.extend_from_slice(b"CFF "); // tag
    data.extend_from_slice(&[0, 0, 0, 0]); // checksum
    data.extend_from_slice(&cff_offset.to_be_bytes()); // offset
    data.extend_from_slice(&cff_length.to_be_bytes()); // length
    data.extend_from_slice(cff_data);

    let result = extract_cff_from_opentype(&data);
    assert!(result.is_some());
    assert_eq!(result.unwrap(), cff_data);
}

#[test]
fn test_extract_cff_from_opentype_truncated_table_dir() {
    // OTTO with 1 table but data too short
    let data = vec![
        0x4F, 0x54, 0x54, 0x4F, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // table record starts but is truncated
        b'C', b'F',
    ];
    assert_eq!(extract_cff_from_opentype(&data), None);
}

// ==========================================
// parse_cff_encoding (integration) tests
// ==========================================

#[test]
fn test_parse_cff_encoding_too_short() {
    assert_eq!(parse_cff_encoding(&[0, 1, 2]), None);
}

#[test]
fn test_parse_cff_encoding_wrong_version() {
    // Not version 1, and not an OpenType wrapper
    let data = vec![0x02, 0x00, 0x04, 0x01, 0x00];
    assert_eq!(parse_cff_encoding(&data), None);
}

#[test]
fn test_parse_cff_encoding_version1_too_short_after_check() {
    // Version byte is 1 but too small overall
    let data = vec![0x01, 0x00, 0x04];
    assert_eq!(parse_cff_encoding(&data), None);
}

#[test]
fn test_parse_cff_encoding_expert_encoding() {
    // Build a minimal valid CFF with encoding_offset=1 (ExpertEncoding)
    // This requires: header, name INDEX, top dict INDEX (with encoding=1), string INDEX
    let data = build_minimal_cff(1, 0);
    let result = parse_cff_encoding(&data);
    assert_eq!(result, None); // ExpertEncoding => None
}

#[test]
fn test_parse_cff_encoding_standard_encoding_default_charset() {
    // encoding_offset=0, charset_offset=0 (both defaults)
    let data = build_minimal_cff(0, 0);
    let result = parse_cff_encoding(&data);
    assert_eq!(result, None); // StandardEncoding with default charset => None
}

/// Helper: builds a minimal CFF font binary with specified encoding and charset offsets.
fn build_minimal_cff(encoding_offset: i32, charset_offset: i32) -> Vec<u8> {
    // CFF Header: major=1, minor=0, hdrSize=4, offSize=1
    let mut data = vec![1, 0, 4, 1];

    // Name INDEX: 1 entry "Test"
    append_index(&mut data, &[b"Test"]);

    // Top DICT INDEX: encode encoding_offset and charset_offset
    let top_dict = build_top_dict(encoding_offset, charset_offset);
    append_index(&mut data, &[&top_dict]);

    // String INDEX: empty
    append_index(&mut data, &[]);

    // Global Subr INDEX: empty
    append_index(&mut data, &[]);

    data
}

/// Encode a CFF DICT with encoding (op 16) and charset (op 15) operands.
fn build_top_dict(encoding_offset: i32, charset_offset: i32) -> Vec<u8> {
    let mut dict = Vec::new();
    // Encode encoding_offset as operand, then op 16
    encode_dict_int(&mut dict, encoding_offset);
    dict.push(16); // encoding operator
                   // Encode charset_offset as operand, then op 15
    encode_dict_int(&mut dict, charset_offset);
    dict.push(15); // charset operator
    dict
}

/// Encode a CFF integer operand into DICT format.
fn encode_dict_int(out: &mut Vec<u8>, val: i32) {
    if (-107..=107).contains(&val) {
        out.push((val + 139) as u8);
    } else if (108..=1131).contains(&val) {
        let v = val - 108;
        out.push((v / 256 + 247) as u8);
        out.push((v % 256) as u8);
    } else if (-1131..=-108).contains(&val) {
        let v = -val - 108;
        out.push((v / 256 + 251) as u8);
        out.push((v % 256) as u8);
    } else if (-32768..=32767).contains(&val) {
        out.push(28);
        let bytes = (val as i16).to_be_bytes();
        out.push(bytes[0]);
        out.push(bytes[1]);
    } else {
        out.push(29);
        let bytes = val.to_be_bytes();
        out.extend_from_slice(&bytes);
    }
}

/// Append a CFF INDEX to a data vector.
fn append_index(data: &mut Vec<u8>, entries: &[&[u8]]) {
    let count = entries.len() as u16;
    data.extend_from_slice(&count.to_be_bytes());
    if count == 0 {
        return;
    }
    data.push(1); // off_size = 1

    // Offsets (1-based)
    let mut offset: u8 = 1;
    data.push(offset);
    for entry in entries {
        offset += entry.len() as u8;
        data.push(offset);
    }
    // Data
    for entry in entries {
        data.extend_from_slice(entry);
    }
}

// ==========================================
// glyph_name_to_sid tests
// ==========================================

#[test]
fn test_glyph_name_to_sid_known_names() {
    assert_eq!(glyph_name_to_sid(".notdef"), Some(0));
    assert_eq!(glyph_name_to_sid("space"), Some(1));
    assert_eq!(glyph_name_to_sid("A"), Some(34));
    assert_eq!(glyph_name_to_sid("B"), Some(35));
    assert_eq!(glyph_name_to_sid("Z"), Some(59));
    assert_eq!(glyph_name_to_sid("a"), Some(66));
    assert_eq!(glyph_name_to_sid("z"), Some(91));
    assert_eq!(glyph_name_to_sid("zero"), Some(17));
    assert_eq!(glyph_name_to_sid("nine"), Some(26));
}

#[test]
fn test_glyph_name_to_sid_unknown() {
    assert_eq!(glyph_name_to_sid("nonexistent_glyph_xyz"), None);
    assert_eq!(glyph_name_to_sid(""), None);
}

#[test]
fn test_glyph_name_to_sid_roundtrip() {
    // Verify sid_to_name and glyph_name_to_sid are consistent
    for sid in 0u16..391 {
        if let Some(name) = sid_to_name(sid) {
            assert_eq!(
                glyph_name_to_sid(name),
                Some(sid),
                "Roundtrip failed for SID {} (name '{}')",
                sid,
                name
            );
        }
    }
}

// ==========================================
// parse_cff_gid_mapping tests
// ==========================================

#[test]
fn test_parse_cff_gid_mapping_invalid_data() {
    assert!(parse_cff_gid_mapping(&[]).is_none());
    assert!(parse_cff_gid_mapping(&[0, 1, 2]).is_none());
    assert!(parse_cff_gid_mapping(&[2, 0, 4, 2]).is_none()); // wrong version
}

// ==========================================
// resolve_bytes_via_pdf_encoding tests
//
// These exercise the name-resolution layer in isolation — the layer
// that was missing in `parse_cff_gid_mapping` and is the substantive
// fix here. The CFF binary parser is reused unchanged, so its tests
// remain authoritative for the parsing path.
// ==========================================

use crate::fonts::font_dict::Encoding;

/// Pin a real-world sparse-CFF subset pattern: charset enumerates
/// space, A, B, C, O, V, N (SIDs 1, 34, 35, 36, 48, 55, 47) on GIDs
/// 1..=7. PDF /Encoding is WinAnsiEncoding. The resolver must produce
/// GIDs for every charset entry — not just for byte 0x41 ("A") as the
/// sparse CFF Encoding table would have implied.
#[test]
fn resolve_via_pdf_encoding_recovers_all_charset_glyphs() {
    // GID order: 0 = .notdef (implicit), 1 = space, 2 = A, 3 = B,
    // 4 = C, 5 = O, 6 = V, 7 = N.
    let charset = [0u16, 1, 34, 35, 36, 48, 55, 47];
    let string_index: Vec<&[u8]> = Vec::new();
    let pdf_enc = Encoding::Standard("WinAnsiEncoding".to_string());
    let differences: HashMap<u8, String> = HashMap::new();

    let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);

    assert_eq!(map.get(&0x20), Some(&1), "0x20 (space) → GID 1");
    assert_eq!(map.get(&0x41), Some(&2), "0x41 (A) → GID 2");
    assert_eq!(map.get(&0x42), Some(&3), "0x42 (B) → GID 3");
    assert_eq!(map.get(&0x43), Some(&4), "0x43 (C) → GID 4");
    assert_eq!(map.get(&0x4f), Some(&5), "0x4f (O) → GID 5");
    assert_eq!(map.get(&0x56), Some(&6), "0x56 (V) → GID 6");
    assert_eq!(map.get(&0x4e), Some(&7), "0x4e (N) → GID 7");

    // Bytes whose glyph name is not in the Charset stay out.
    assert!(!map.contains_key(&0x7e), "0x7e (asciitilde) not in charset");
}

/// /Differences entries override the base predefined encoding.
#[test]
fn resolve_via_pdf_encoding_honors_differences_array() {
    // Charset includes "bullet" (SID 116) at GID 1.
    let charset = [0u16, 116];
    let string_index: Vec<&[u8]> = Vec::new();
    let pdf_enc = Encoding::Standard("WinAnsiEncoding".to_string());
    let mut differences = HashMap::new();
    // Override byte 0x95 to glyph name "bullet" — WinAnsi's native
    // 0x95 is also "bullet", but we want to pin that the /Differences
    // path is exercised (and would beat a divergent base encoding).
    differences.insert(0x95u8, "bullet".to_string());

    let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);
    assert_eq!(map.get(&0x95), Some(&1));
}

/// Identity encoding short-circuits via the outer function; the
/// helper itself is a no-op for Identity.
#[test]
fn resolve_via_pdf_encoding_skips_identity() {
    let charset = [0u16, 34];
    let string_index: Vec<&[u8]> = Vec::new();
    let pdf_enc = Encoding::Identity;
    let differences: HashMap<u8, String> = HashMap::new();
    let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);
    assert!(map.is_empty(), "Identity → no base byte→name resolution");
}

/// Custom-string SIDs (>=391) resolved through the String INDEX
/// land in the name→GID map.
#[test]
fn resolve_via_pdf_encoding_resolves_custom_string_sids() {
    // GID 1 is a glyph named "customGlyph" via custom SID 391.
    let charset = [0u16, 391];
    let custom: &[u8] = b"customGlyph";
    let string_index: Vec<&[u8]> = vec![custom];
    let pdf_enc = Encoding::Standard("WinAnsiEncoding".to_string());
    let mut differences = HashMap::new();
    differences.insert(0x21u8, "customGlyph".to_string());

    let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);
    assert_eq!(map.get(&0x21), Some(&1));
}

// ==========================================
// Base-encoding selection (§9.6.6.1 / Annex D)
//
// /BaseEncoding picks which byte→glyph-name table resolves high bytes
// 0x80-0xFF. The three predefined bases diverge:
//   - WinAnsi 0x80   = "euro"      (Windows-1252)
//   - MacRoman 0x80  = "Adieresis" (Annex D Table D.2)
//   - Standard 0xA4  = "fraction"  (Annex D Table D.1; WinAnsi = "currency")
// Resolving the wrong table for a non-WinAnsi base would mis-route bytes.
// ==========================================

/// MacRomanEncoding: byte 0x80 must resolve via "Adieresis", not "euro".
#[test]
fn resolve_via_pdf_encoding_uses_mac_roman_table_for_mac_base() {
    // GID 1 = Adieresis (SID 173, predefined)
    // GID 2 = euro (custom SID 391 → string_index[0])
    let charset: Vec<u16> = vec![0, 173, 391];
    let string_index: Vec<&[u8]> = vec![b"euro"];
    let differences: HashMap<u8, String> = HashMap::new();
    let pdf_enc = Encoding::Standard("MacRomanEncoding".to_string());

    let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);
    assert_eq!(
        map.get(&0x80),
        Some(&1),
        "MacRoman 0x80 → Adieresis (GID 1)"
    );
}

/// StandardEncoding: byte 0xA4 must resolve via "fraction", not "currency".
#[test]
fn resolve_via_pdf_encoding_uses_standard_encoding_table_for_standard_base() {
    // GID 1 = fraction (SID 99), GID 2 = currency (SID 103).
    let charset: Vec<u16> = vec![0, 99, 103];
    let string_index: Vec<&[u8]> = vec![];
    let differences: HashMap<u8, String> = HashMap::new();
    let pdf_enc = Encoding::Standard("StandardEncoding".to_string());

    let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);
    assert_eq!(
        map.get(&0xA4),
        Some(&1),
        "StandardEncoding 0xA4 → fraction (GID 1)"
    );
}

/// WinAnsiEncoding regression guard: byte 0x80 still resolves to "euro".
#[test]
fn resolve_via_pdf_encoding_uses_winansi_table_for_winansi_base() {
    // Same fixture as the MacRoman test — only the declared base differs.
    let charset: Vec<u16> = vec![0, 173, 391];
    let string_index: Vec<&[u8]> = vec![b"euro"];
    let differences: HashMap<u8, String> = HashMap::new();
    let pdf_enc = Encoding::Standard("WinAnsiEncoding".to_string());

    let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);
    assert_eq!(map.get(&0x80), Some(&2), "WinAnsi 0x80 → euro (GID 2)");
}

// ==========================================
// Charset capacity: full nGlyphs, not capped at 256
// ==========================================

/// `parse_charset` already handles arbitrary nGlyphs; the cap lives in the
/// caller (`parse_cff_gid_mapping_with_pdf_encoding`). This pins the
/// parser's tolerance for >256 entries so a regression there would
/// surface independently of the caller refactor.
#[test]
fn parse_charset_format0_handles_more_than_256_entries() {
    // Format 0: 1-byte format + 2-byte SID per non-.notdef GID.
    let mut data = vec![0x00u8];
    for gid in 1u16..=299u16 {
        data.extend_from_slice(&gid.to_be_bytes());
    }
    let sids = parse_charset(&data, 0, 300).expect("parse_charset returned None");
    assert_eq!(sids.len(), 300, "300 entries (GID 0 + 299 enumerated)");
    assert_eq!(sids[0], 0, "GID 0 is .notdef (SID 0)");
    assert_eq!(sids[1], 1, "GID 1 → SID 1");
    assert_eq!(sids[256], 256, "GID 256 → SID 256 (past the old 256 cap)");
    assert_eq!(sids[299], 299, "GID 299 → SID 299 (last entry)");
}

/// CFF Top DICT must surface the CharStrings INDEX offset (op 17) so
/// callers can read the real nGlyphs from the INDEX header instead of
/// guessing 256.
#[test]
fn parse_top_dict_surfaces_charstrings_offset() {
    // Minimal top DICT: one operand + op 17 (CharStrings).
    // Operand 1234 encoded as a 3-byte int per CFF DICT encoding:
    //   b0 = 29 (4-byte int marker is 29? Actually 28 = 2-byte, 29 = 4-byte)
    // Use the 2-byte form: b0=28, then 2 BE bytes.
    let mut dict = vec![28u8];
    dict.extend_from_slice(&1234i16.to_be_bytes());
    dict.push(17u8); // op 17 = CharStrings

    let (charstrings_offset, _enc, _charset) = parse_top_dict_with_charstrings(&dict);
    assert_eq!(
        charstrings_offset, 1234,
        "Top DICT op 17 → CharStrings offset"
    );
}

/// Read the count field of a CFF INDEX header. nGlyphs is the count of
/// the CharStrings INDEX.
#[test]
fn read_index_count_returns_header_count() {
    // INDEX header: 2-byte count, 1-byte offSize, then offsets+data.
    // For this helper we only need the first 2 bytes.
    let data = [0x01u8, 0x2C]; // count = 300 (0x012C)
    assert_eq!(read_index_count(&data, 0), Some(300));
}
