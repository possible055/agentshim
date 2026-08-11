use super::*;

// ==========================================
// sid_to_name tests
// ==========================================

#[test]
fn test_sid_to_name_notdef() {
    assert_eq!(sid_to_name(0), Some(".notdef"));
}

#[test]
fn test_sid_to_name_space() {
    assert_eq!(sid_to_name(1), Some("space"));
}

#[test]
fn test_sid_to_name_letters() {
    assert_eq!(sid_to_name(34), Some("A"));
    assert_eq!(sid_to_name(59), Some("Z"));
    assert_eq!(sid_to_name(66), Some("a"));
    assert_eq!(sid_to_name(91), Some("z"));
}

#[test]
fn test_sid_to_name_digits() {
    assert_eq!(sid_to_name(17), Some("zero"));
    assert_eq!(sid_to_name(26), Some("nine"));
}

#[test]
fn test_sid_to_name_punctuation() {
    assert_eq!(sid_to_name(2), Some("exclam"));
    assert_eq!(sid_to_name(15), Some("period"));
    assert_eq!(sid_to_name(13), Some("comma"));
}

#[test]
fn test_sid_to_name_ligatures() {
    assert_eq!(sid_to_name(109), Some("fi"));
    assert_eq!(sid_to_name(110), Some("fl"));
    assert_eq!(sid_to_name(266), Some("ff"));
    assert_eq!(sid_to_name(267), Some("ffi"));
    assert_eq!(sid_to_name(268), Some("ffl"));
}

#[test]
fn test_sid_to_name_accented() {
    assert_eq!(sid_to_name(171), Some("Aacute"));
    assert_eq!(sid_to_name(200), Some("aacute"));
    assert_eq!(sid_to_name(227), Some("ydieresis"));
}

#[test]
fn test_sid_to_name_last_entries() {
    assert_eq!(sid_to_name(388), Some("Regular"));
    assert_eq!(sid_to_name(389), Some("Roman"));
    assert_eq!(sid_to_name(390), Some("Semibold"));
}

#[test]
fn test_sid_to_name_out_of_range() {
    assert_eq!(sid_to_name(391), None);
    assert_eq!(sid_to_name(500), None);
    assert_eq!(sid_to_name(u16::MAX), None);
}

// ==========================================
// parse_index tests
// ==========================================

#[test]
fn test_parse_index_too_short() {
    assert_eq!(parse_index(&[0x00], 0), None);
}

#[test]
fn test_parse_index_empty() {
    // count = 0
    let data = [0x00, 0x00];
    let result = parse_index(&data, 0);
    assert!(result.is_some());
    let (entries, next) = result.unwrap();
    assert!(entries.is_empty());
    assert_eq!(next, 2);
}

#[test]
fn test_parse_index_single_entry() {
    // count=1, off_size=1, offsets=[1, 4] (entry is 3 bytes)
    let data = vec![
        0x00, 0x01, // count = 1
        0x01, // off_size = 1
        0x01, // offset[0] = 1
        0x04, // offset[1] = 4
        b'A', b'B', b'C', // data: "ABC"
    ];
    let result = parse_index(&data, 0);
    assert!(result.is_some());
    let (entries, _next) = result.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0], b"ABC");
}

#[test]
fn test_parse_index_multiple_entries() {
    // count=2, off_size=1, offsets=[1, 3, 5]
    let data = vec![
        0x00, 0x02, // count = 2
        0x01, // off_size = 1
        0x01, // offset[0] = 1
        0x03, // offset[1] = 3
        0x05, // offset[2] = 5
        b'H', b'i', // entry 0: "Hi"
        b'O', b'K', // entry 1: "OK"
    ];
    let result = parse_index(&data, 0);
    assert!(result.is_some());
    let (entries, _next) = result.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0], b"Hi");
    assert_eq!(entries[1], b"OK");
}

#[test]
fn test_parse_index_invalid_off_size_zero() {
    let data = vec![0x00, 0x01, 0x00]; // off_size = 0
    assert_eq!(parse_index(&data, 0), None);
}

#[test]
fn test_parse_index_invalid_off_size_too_large() {
    let data = vec![0x00, 0x01, 0x05]; // off_size = 5
    assert_eq!(parse_index(&data, 0), None);
}

#[test]
fn test_parse_index_truncated_offset_array() {
    // count=1, off_size=1, but not enough data for offsets
    let data = vec![0x00, 0x01, 0x01, 0x01]; // missing second offset
    assert_eq!(parse_index(&data, 0), None);
}

#[test]
fn test_parse_index_with_offset() {
    // Parse index starting at offset 3
    let data = vec![
        0xFF, 0xFF, 0xFF, // padding
        0x00, 0x01, // count = 1
        0x01, // off_size = 1
        0x01, // offset[0] = 1
        0x02, // offset[1] = 2
        b'X', // data: "X"
    ];
    let result = parse_index(&data, 3);
    assert!(result.is_some());
    let (entries, _) = result.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0], b"X");
}

#[test]
fn test_parse_index_off_size_2() {
    // count=1, off_size=2, offsets=[0x0001, 0x0003]
    let data = vec![
        0x00, 0x01, // count = 1
        0x02, // off_size = 2
        0x00, 0x01, // offset[0] = 1
        0x00, 0x03, // offset[1] = 3
        b'A', b'B', // data: "AB"
    ];
    let result = parse_index(&data, 0);
    assert!(result.is_some());
    let (entries, _) = result.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0], b"AB");
}

#[test]
fn test_parse_index_data_out_of_bounds() {
    // offsets reference data beyond the buffer
    let data = vec![
        0x00, 0x01, // count = 1
        0x01, // off_size = 1
        0x01, // offset[0] = 1
        0xFF, // offset[1] = 255 (way out of bounds)
    ];
    assert_eq!(parse_index(&data, 0), None);
}

// ==========================================
// parse_dict_operand tests
// ==========================================

#[test]
fn test_parse_dict_operand_empty() {
    assert_eq!(parse_dict_operand(&[], 0), None);
}

#[test]
fn test_parse_dict_operand_single_byte_zero() {
    // b0 = 139 => value = 139 - 139 = 0
    assert_eq!(parse_dict_operand(&[139], 0), Some((0, 1)));
}

#[test]
fn test_parse_dict_operand_single_byte_positive() {
    // b0 = 246 => value = 246 - 139 = 107
    assert_eq!(parse_dict_operand(&[246], 0), Some((107, 1)));
}

#[test]
fn test_parse_dict_operand_single_byte_negative() {
    // b0 = 32 => value = 32 - 139 = -107
    assert_eq!(parse_dict_operand(&[32], 0), Some((-107, 1)));
}

#[test]
fn test_parse_dict_operand_two_byte_positive() {
    // b0=247, b1=0 => (247-247)*256 + 0 + 108 = 108
    assert_eq!(parse_dict_operand(&[247, 0], 0), Some((108, 2)));
    // b0=250, b1=255 => (250-247)*256 + 255 + 108 = 1131
    assert_eq!(parse_dict_operand(&[250, 255], 0), Some((1131, 2)));
}

#[test]
fn test_parse_dict_operand_two_byte_negative() {
    // b0=251, b1=0 => -(251-251)*256 - 0 - 108 = -108
    assert_eq!(parse_dict_operand(&[251, 0], 0), Some((-108, 2)));
    // b0=254, b1=255 => -(254-251)*256 - 255 - 108 = -1131
    assert_eq!(parse_dict_operand(&[254, 255], 0), Some((-1131, 2)));
}

#[test]
fn test_parse_dict_operand_two_byte_truncated() {
    // b0=247 but no b1
    assert_eq!(parse_dict_operand(&[247], 0), None);
    assert_eq!(parse_dict_operand(&[251], 0), None);
}

#[test]
fn test_parse_dict_operand_three_byte_int16() {
    // b0=28, then 16-bit signed int
    // 0x00, 0x01 => 1
    assert_eq!(parse_dict_operand(&[28, 0x00, 0x01], 0), Some((1, 3)));
    // 0xFF, 0xFF => -1
    assert_eq!(parse_dict_operand(&[28, 0xFF, 0xFF], 0), Some((-1, 3)));
    // 0x7F, 0xFF => 32767
    assert_eq!(parse_dict_operand(&[28, 0x7F, 0xFF], 0), Some((32767, 3)));
}

#[test]
fn test_parse_dict_operand_three_byte_truncated() {
    assert_eq!(parse_dict_operand(&[28, 0x00], 0), None);
}

#[test]
fn test_parse_dict_operand_five_byte_int32() {
    // b0=29, then 32-bit signed int
    // 0x00, 0x00, 0x00, 0x01 => 1
    assert_eq!(
        parse_dict_operand(&[29, 0x00, 0x00, 0x00, 0x01], 0),
        Some((1, 5))
    );
    // 0xFF, 0xFF, 0xFF, 0xFF => -1
    assert_eq!(
        parse_dict_operand(&[29, 0xFF, 0xFF, 0xFF, 0xFF], 0),
        Some((-1, 5))
    );
}

#[test]
fn test_parse_dict_operand_five_byte_truncated() {
    assert_eq!(parse_dict_operand(&[29, 0x00, 0x00, 0x00], 0), None);
}

#[test]
fn test_parse_dict_operand_real_number() {
    // b0=30, then nibble pairs terminated by 0xF
    // 1.5 would be encoded as: 0x1A 0x5F (1, '.', 5, end)
    // But since we just skip real numbers and return 0...
    let data = [30, 0x1A, 0x5F];
    let result = parse_dict_operand(&data, 0);
    assert!(result.is_some());
    let (val, consumed) = result.unwrap();
    assert_eq!(val, 0); // Real numbers return 0
    assert_eq!(consumed, 3);
}

#[test]
fn test_parse_dict_operand_real_nibble1_end() {
    // nibble1 = 0xF => end marker in high nibble
    let data = [30, 0xF0];
    let result = parse_dict_operand(&data, 0);
    assert!(result.is_some());
    assert_eq!(result.unwrap(), (0, 2));
}

#[test]
fn test_parse_dict_operand_real_unterminated() {
    // Real number that runs off the end without terminator
    let data = [30, 0x12, 0x34];
    assert_eq!(parse_dict_operand(&data, 0), None);
}

#[test]
fn test_parse_dict_operand_unknown_byte() {
    // Byte values 0-21 are operators, not operands => None
    assert_eq!(parse_dict_operand(&[0], 0), None);
    assert_eq!(parse_dict_operand(&[21], 0), None);
    // Byte 255 should be handled by the 251..=254 range => this is 255
    assert_eq!(parse_dict_operand(&[255], 0), None);
}

#[test]
fn test_parse_dict_operand_with_offset() {
    // Parse from a non-zero position
    let data = [0x00, 0x00, 139]; // value 0 at position 2
    assert_eq!(parse_dict_operand(&data, 2), Some((0, 1)));
}

// ==========================================
// parse_top_dict tests
// ==========================================

#[test]
fn test_parse_top_dict_empty() {
    let (enc, charset) = parse_top_dict(&[]);
    assert_eq!(enc, 0);
    assert_eq!(charset, 0);
}

#[test]
fn test_parse_top_dict_encoding_offset() {
    // Push operand 42 (139+42=181), then operator 16 (encoding)
    let data = [181, 16];
    let (enc, charset) = parse_top_dict(&data);
    assert_eq!(enc, 42);
    assert_eq!(charset, 0); // default
}

#[test]
fn test_parse_top_dict_charset_offset() {
    // Push operand 99 (139+99=238), then operator 15 (charset)
    let data = [238, 15];
    let (enc, charset) = parse_top_dict(&data);
    assert_eq!(enc, 0); // default
    assert_eq!(charset, 99);
}

#[test]
fn test_parse_top_dict_both_offsets() {
    // encoding=50 (189), op 16, charset=100 (239), op 15
    let data = [189, 16, 239, 15];
    let (enc, charset) = parse_top_dict(&data);
    assert_eq!(enc, 50);
    assert_eq!(charset, 100);
}

#[test]
fn test_parse_top_dict_two_byte_operator() {
    // b0=12 signals two-byte operator; b0=12, b1=X => operator (12<<8)|X
    // e.g. operator 12, sub 0 => (3072) => not encoding/charset, should be ignored
    let data = [139, 12, 0]; // push 0, then op 12.0
    let (enc, charset) = parse_top_dict(&data);
    assert_eq!(enc, 0);
    assert_eq!(charset, 0);
}

#[test]
fn test_parse_top_dict_unknown_operator() {
    // Unknown operator 17 (not encoding=16 or charset=15)
    let data = [181, 17]; // push 42, operator 17
    let (enc, charset) = parse_top_dict(&data);
    assert_eq!(enc, 0); // not set
    assert_eq!(charset, 0); // not set
}

#[test]
fn test_parse_top_dict_skip_unparseable() {
    // If operand parse fails, we just skip the byte
    // Use byte 255 which is not a valid operand (nor operator)
    let data = [255, 181, 16]; // 255 skipped, then 42 + encoding op
    let (enc, charset) = parse_top_dict(&data);
    assert_eq!(enc, 42);
    assert_eq!(charset, 0);
}

// ==========================================
// parse_charset tests
// ==========================================

#[test]
fn test_parse_charset_out_of_bounds() {
    assert_eq!(parse_charset(&[0x00], 5, 10), None);
}

#[test]
fn test_parse_charset_format0() {
    // format=0, then SIDs for GID 1,2,3...
    let data = vec![
        0x00, // format 0
        0x00, 0x01, // SID 1 (space)
        0x00, 0x22, // SID 34 (A)
        0x00, 0x42, // SID 66 (a)
    ];
    let result = parse_charset(&data, 0, 4);
    assert!(result.is_some());
    let sids = result.unwrap();
    assert_eq!(sids.len(), 4);
    assert_eq!(sids[0], 0); // .notdef
    assert_eq!(sids[1], 1); // space
    assert_eq!(sids[2], 34); // A
    assert_eq!(sids[3], 66); // a
}

#[test]
fn test_parse_charset_format1() {
    // format=1, range: first_sid=34, n_left=2 => SIDs 34,35,36
    let data = vec![
        0x01, // format 1
        0x00, 0x22, // first_sid = 34
        0x02, // n_left = 2
    ];
    let result = parse_charset(&data, 0, 4);
    assert!(result.is_some());
    let sids = result.unwrap();
    assert_eq!(sids.len(), 4);
    assert_eq!(sids[0], 0); // .notdef
    assert_eq!(sids[1], 34); // A
    assert_eq!(sids[2], 35); // B
    assert_eq!(sids[3], 36); // C
}

#[test]
fn test_parse_charset_format2() {
    // format=2, range: first_sid=66, n_left=3 => SIDs 66,67,68,69
    let data = vec![
        0x02, // format 2
        0x00, 0x42, // first_sid = 66
        0x00, 0x03, // n_left = 3
    ];
    let result = parse_charset(&data, 0, 5);
    assert!(result.is_some());
    let sids = result.unwrap();
    assert_eq!(sids.len(), 5);
    assert_eq!(sids[0], 0);
    assert_eq!(sids[1], 66); // a
    assert_eq!(sids[2], 67); // b
    assert_eq!(sids[3], 68); // c
    assert_eq!(sids[4], 69); // d
}

#[test]
fn test_parse_charset_unknown_format() {
    let data = vec![0x03]; // format 3 doesn't exist
    assert_eq!(parse_charset(&data, 0, 2), None);
}

#[test]
fn test_parse_charset_format0_truncated() {
    // format=0 but not enough data for SIDs
    let data = vec![0x00, 0x00]; // only 1 byte of SID data (need 2)
    let result = parse_charset(&data, 0, 3);
    assert!(result.is_some());
    let sids = result.unwrap();
    // Should get .notdef + whatever it could parse
    assert_eq!(sids[0], 0);
}

#[test]
fn test_parse_charset_format1_limits_to_num_glyphs() {
    // Range would give more SIDs than needed
    let data = vec![
        0x01, // format 1
        0x00, 0x01, // first_sid = 1
        0xFF, // n_left = 255 (way more than we need)
    ];
    let result = parse_charset(&data, 0, 3);
    assert!(result.is_some());
    let sids = result.unwrap();
    assert_eq!(sids.len(), 3); // limited to num_glyphs
}

// ==========================================
// parse_encoding_table tests
// ==========================================

#[test]
fn test_parse_encoding_table_out_of_bounds() {
    assert_eq!(parse_encoding_table(&[0x00], 5), None);
}

#[test]
fn test_parse_encoding_table_format0() {
    let data = vec![
        0x00, // format 0 (no supplement)
        0x03, // n_codes = 3
        0x41, // code 0x41 => GID 1
        0x42, // code 0x42 => GID 2
        0x43, // code 0x43 => GID 3
    ];
    let result = parse_encoding_table(&data, 0);
    assert!(result.is_some());
    let map = result.unwrap();
    assert_eq!(map.get(&0x41), Some(&1));
    assert_eq!(map.get(&0x42), Some(&2));
    assert_eq!(map.get(&0x43), Some(&3));
}

#[test]
fn test_parse_encoding_table_format1() {
    let data = vec![
        0x01, // format 1
        0x01, // n_ranges = 1
        0x41, // first = 0x41
        0x02, // n_left = 2 (codes 0x41, 0x42, 0x43)
    ];
    let result = parse_encoding_table(&data, 0);
    assert!(result.is_some());
    let map = result.unwrap();
    assert_eq!(map.get(&0x41), Some(&1));
    assert_eq!(map.get(&0x42), Some(&2));
    assert_eq!(map.get(&0x43), Some(&3));
}

#[test]
fn test_parse_encoding_table_unknown_format() {
    let data = vec![0x02]; // format 2 doesn't exist for encoding
                           // 0x02 & 0x7F = 2
    assert_eq!(parse_encoding_table(&data, 0), None);
}

#[test]
fn test_parse_encoding_table_format0_truncated() {
    let data = vec![
        0x00, // format 0
        0x05, // n_codes = 5 but only 2 bytes of data follow
        0x41, 0x42,
    ];
    let result = parse_encoding_table(&data, 0);
    assert!(result.is_some());
    let map = result.unwrap();
    assert_eq!(map.len(), 2);
}

#[test]
fn test_parse_encoding_table_with_supplement() {
    let data = vec![
        0x80, // format 0 with supplement flag (bit 7 set)
        0x01, // n_codes = 1
        0x41, // code 0x41 => GID 1
        0x01, // n_sups = 1
        0x42, // supplement code = 0x42
        0x00, 0x22, // supplement SID = 34
    ];
    let result = parse_encoding_table(&data, 0);
    assert!(result.is_some());
    let map = result.unwrap();
    assert_eq!(map.get(&0x41), Some(&1));
    assert_eq!(map.get(&0x42), Some(&34)); // supplement
}

#[test]
fn test_parse_encoding_table_format1_truncated() {
    let data = vec![
        0x01, // format 1
        0x02, // n_ranges = 2, but only 1 range of data follows
        0x41, 0x01, // range 1: first=0x41, n_left=1
    ];
    let result = parse_encoding_table(&data, 0);
    assert!(result.is_some());
    let map = result.unwrap();
    assert_eq!(map.len(), 2); // Only first range parsed
}

#[test]
fn test_parse_encoding_table_format0_empty_pos() {
    // format 0, but pos after format byte is at end
    let data = vec![0x00];
    assert_eq!(parse_encoding_table(&data, 0), None);
}

#[test]
fn test_parse_encoding_table_format1_empty_pos() {
    let data = vec![0x01];
    assert_eq!(parse_encoding_table(&data, 0), None);
}
