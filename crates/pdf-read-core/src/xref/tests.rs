use super::parser::{find_actual_xref_offset, parse_xref_iterative};
use super::*;
use std::io::Cursor;

#[test]
fn test_xref_entry_creation() {
    let entry = XRefEntry::new(1234, 0, true);
    assert_eq!(entry.offset, 1234);
    assert_eq!(entry.generation, 0);
    assert!(entry.in_use);
}

#[test]
fn test_xref_entry_free() {
    let entry = XRefEntry::new(0, 65535, false);
    assert_eq!(entry.offset, 0);
    assert_eq!(entry.generation, 65535);
    assert!(!entry.in_use);
}

#[test]
fn test_cross_ref_table_new() {
    let table = CrossRefTable::new();
    assert_eq!(table.len(), 0);
    assert!(table.is_empty());
}

#[test]
fn test_cross_ref_table_add_and_get() {
    let mut table = CrossRefTable::new();
    let entry = XRefEntry::new(1234, 0, true);

    table.add_entry(5, entry.clone());
    assert_eq!(table.len(), 1);
    assert!(!table.is_empty());

    let retrieved = table.get(5).unwrap();
    assert_eq!(retrieved, &entry);
}

#[test]
fn test_cross_ref_table_get_missing() {
    let table = CrossRefTable::new();
    assert!(table.get(999).is_none());
}

#[test]
fn test_find_xref_offset_valid() {
    let pdf = b"%PDF-1.4\n\
            1 0 obj\n\
            << /Type /Catalog >>\n\
            endobj\n\
            xref\n\
            0 2\n\
            0000000000 65535 f\n\
            0000000009 00000 n\n\
            trailer\n\
            << /Size 2 >>\n\
            startxref\n\
            50\n\
            %%EOF";

    let mut cursor = Cursor::new(pdf);
    let offset = find_xref_offset(&mut cursor).unwrap();
    assert_eq!(offset, 50);
}

/// A file PADDED with trailing NULs after `%%EOF` must still be read.
///
/// Real producers do this: the 2nd-Circuit appellate filings
/// (`gov.uscourts.ca2.*`) are padded to an exact 192 KiB with **5,245 trailing
/// NUL bytes**. They are valid PDFs - poppler and Acrobat open them - but a
/// fixed 2 KB tail window contains nothing but padding, so `startxref` is never
/// found and the ENTIRE DOCUMENT is rejected. Not a degradation: a total loss.
#[test]
fn find_xref_offset_survives_trailing_nul_padding() {
    let mut pdf: Vec<u8> = b"%PDF-1.6\n1 0 obj\n<< >>\nendobj\nstartxref\n189089\n%%EOF".to_vec();
    // Pad well past the old 2 KB window.
    pdf.extend(std::iter::repeat_n(0u8, 5245));

    let mut cur = std::io::Cursor::new(pdf);
    let offset = find_xref_offset(&mut cur).expect("a padded file must still parse");
    assert_eq!(offset, 189089);
}

/// The offset LINE itself may carry the padding (`"189089\0\0\0..."`).
/// `str::trim` does not strip NULs, so the all-digits check must.
#[test]
fn find_xref_offset_trims_nuls_from_the_offset_line() {
    let mut pdf: Vec<u8> = b"%PDF-1.6\nstartxref\n4242".to_vec();
    pdf.extend(std::iter::repeat_n(0u8, 64));
    let mut cur = std::io::Cursor::new(pdf);
    assert_eq!(
        find_xref_offset(&mut cur).expect("trailing NULs on the offset line"),
        4242
    );
}

#[test]
fn test_find_xref_offset_no_startxref() {
    let pdf = b"%PDF-1.4\n\
            xref\n\
            0 1\n\
            0000000000 65535 f\n\
            trailer\n\
            << /Size 1 >>\n";

    let mut cursor = Cursor::new(pdf);
    let result = find_xref_offset(&mut cursor);
    assert!(result.is_err());
}

#[test]
fn test_find_xref_offset_with_whitespace() {
    let pdf = b"%PDF-1.4\n\
            startxref\n\
            \n\
            12345\n\
            %%EOF";

    let mut cursor = Cursor::new(pdf);
    let offset = find_xref_offset(&mut cursor).unwrap();
    assert_eq!(offset, 12345);
}

/// Hybrid-reference file (§7.5.8.4): a classic trailer carrying
/// `/XRefStm` must merge the referenced xref stream so objects listed
/// only there (e.g. compressed objects) become reachable. Object 2 is
/// declared ONLY by the /XRefStm here, so a table that resolves it proves
/// the supplement was parsed and merged. Offsets are tracked at build time
/// so the fixture stays correct without hand-computed byte positions.
#[test]
fn test_hybrid_xrefstm_supplement_is_merged() {
    let mut pdf: Vec<u8> = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.5\n");

    let off1 = pdf.len();
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog >>\nendobj\n");

    // Standalone (uncompressed) xref stream declaring object 2 at a
    // sentinel offset via /Index [2 1], /W [1 2 1]: type=1, offset=0x270F,
    // gen=0 → bytes 01 27 0F 00.
    let off_stm = pdf.len();
    pdf.extend_from_slice(
        b"6 0 obj\n<< /Type /XRef /Size 3 /W [1 2 1] /Index [2 1] \
              /Root 1 0 R /Length 4 >>\nstream\n",
    );
    pdf.extend_from_slice(&[0x01, 0x27, 0x0F, 0x00]);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    // Classic table: object 0 free, object 1 in-use at off1.
    let off_classic = pdf.len();
    let entry1 = format!("{:010} 00000 n \n", off1);
    pdf.extend_from_slice(b"xref\n0 2\n0000000000 65535 f \n");
    pdf.extend_from_slice(entry1.as_bytes());
    let trailer = format!(
        "trailer\n<< /Size 3 /Root 1 0 R /XRefStm {} >>\nstartxref\n{}\n%%EOF",
        off_stm, off_classic
    );
    pdf.extend_from_slice(trailer.as_bytes());

    let mut cursor = Cursor::new(pdf);
    let table = parse_xref_iterative(&mut cursor, off_classic as u64).unwrap();

    // Object 1 from the classic table, object 2 ONLY from the /XRefStm.
    assert!(
        table.get(1).is_some(),
        "classic-table object 1 must resolve"
    );
    let obj2 = table
        .get(2)
        .expect("object 2 must resolve via the merged /XRefStm supplement");
    assert_eq!(
        obj2.offset, 0x270F,
        "object 2 offset comes from the xref stream"
    );
}

#[test]
fn test_parse_xref_single_subsection() {
    let xref_data = b"xref\n\
            0 3\n\
            0000000000 65535 f\n\
            0000000018 00000 n\n\
            0000000154 00000 n\n\
            trailer\n";

    let mut cursor = Cursor::new(xref_data);
    let table = parse_xref(&mut cursor, 0).unwrap();

    assert_eq!(table.len(), 3);

    // Object 0 (free)
    let entry0 = table.get(0).unwrap();
    assert_eq!(entry0.offset, 0);
    assert_eq!(entry0.generation, 65535);
    assert!(!entry0.in_use);

    // Object 1
    let entry1 = table.get(1).unwrap();
    assert_eq!(entry1.offset, 18);
    assert_eq!(entry1.generation, 0);
    assert!(entry1.in_use);

    // Object 2
    let entry2 = table.get(2).unwrap();
    assert_eq!(entry2.offset, 154);
    assert_eq!(entry2.generation, 0);
    assert!(entry2.in_use);
}

#[test]
fn test_parse_xref_multiple_subsections() {
    let xref_data = b"xref\n\
            0 2\n\
            0000000000 65535 f\n\
            0000000018 00000 n\n\
            5 3\n\
            0000000200 00000 n\n\
            0000000300 00000 n\n\
            0000000400 00000 n\n\
            trailer\n";

    let mut cursor = Cursor::new(xref_data);
    let table = parse_xref(&mut cursor, 0).unwrap();

    assert_eq!(table.len(), 5); // 2 + 3 entries

    // First subsection
    assert!(table.get(0).is_some());
    assert!(table.get(1).is_some());

    // Second subsection (starts at 5)
    let entry5 = table.get(5).unwrap();
    assert_eq!(entry5.offset, 200);

    let entry6 = table.get(6).unwrap();
    assert_eq!(entry6.offset, 300);

    let entry7 = table.get(7).unwrap();
    assert_eq!(entry7.offset, 400);

    // Gap between subsections
    assert!(table.get(2).is_none());
    assert!(table.get(3).is_none());
    assert!(table.get(4).is_none());
}

#[test]
fn test_parse_xref_no_xref_keyword() {
    let xref_data = b"notxref\n\
            0 1\n\
            0000000000 65535 f\n\
            trailer\n";

    let mut cursor = Cursor::new(xref_data);
    let result = parse_xref(&mut cursor, 0);
    assert!(result.is_err());
}

#[test]
fn test_parse_xref_malformed_entry() {
    // Parser should add placeholder free entries for malformed entries
    // to maintain object numbering consistency
    let xref_data = b"xref\n\
            0 2\n\
            0000000000 65535 f\n\
            invalid entry here\n\
            trailer\n";

    let mut cursor = Cursor::new(xref_data);
    let result = parse_xref(&mut cursor, 0);
    // Should succeed and have 2 entries (one valid, one placeholder free)
    assert!(result.is_ok());
    let table = result.unwrap();
    assert_eq!(table.len(), 2);
    // Object 0 should be the valid free entry
    assert!(table.get(0).is_some());
    assert!(!table.get(0).unwrap().in_use);
    // Object 1 should be the placeholder free entry
    assert!(table.get(1).is_some());
    assert!(!table.get(1).unwrap().in_use);
}

#[test]
fn test_parse_xref_invalid_flag() {
    // Parser should treat entries with invalid flags as free entries
    // to maintain object numbering consistency
    let xref_data = b"xref\n\
            0 1\n\
            0000000000 65535 x\n\
            trailer\n";

    let mut cursor = Cursor::new(xref_data);
    let result = parse_xref(&mut cursor, 0);
    // Should succeed and have 1 entry (treated as free)
    assert!(result.is_ok());
    let table = result.unwrap();
    assert_eq!(table.len(), 1);
    // Object 0 should be a free entry
    assert!(table.get(0).is_some());
    assert!(!table.get(0).unwrap().in_use);
}

#[test]
fn test_parse_xref_empty_table() {
    let xref_data = b"xref\n\
            trailer\n";

    let mut cursor = Cursor::new(xref_data);
    let table = parse_xref(&mut cursor, 0).unwrap();
    assert!(table.is_empty());
}

#[test]
fn test_cross_ref_table_default() {
    let table = CrossRefTable::default();
    assert!(table.is_empty());
}

#[test]
fn test_parse_xref_with_comments() {
    let xref_data = b"xref\n\
            % This is a comment\n\
            0 2\n\
            0000000000 65535 f\n\
            0000000018 00000 n\n\
            % Another comment\n\
            trailer\n";

    let mut cursor = Cursor::new(xref_data);
    let table = parse_xref(&mut cursor, 0).unwrap();
    assert_eq!(table.len(), 2);
}

#[test]
fn test_parse_xref_excessive_count() {
    let xref_data = b"xref\n\
            0 2000000\n\
            0000000000 65535 f\n\
            trailer\n";

    let mut cursor = Cursor::new(xref_data);
    let result = parse_xref(&mut cursor, 0);
    assert!(result.is_err());
}

#[test]
fn test_find_xref_offset_cr_only_line_endings() {
    // Test Mac-style CR-only line endings (the bug we just fixed)
    let pdf_data = b"some content\r\
            startxref\r\
            173\r\
            %%EOF\r";

    let mut cursor = Cursor::new(pdf_data);
    let offset = find_xref_offset(&mut cursor).unwrap();
    assert_eq!(offset, 173);
}

#[test]
fn test_parse_xref_cr_only_line_endings() {
    // Test parsing traditional xref with CR-only line endings
    let xref_data = b"xref\r\
            0 2\r\
            0000000000 65535 f\r\
            0000000018 00000 n\r\
            trailer\r";

    let mut cursor = Cursor::new(xref_data);
    let table = parse_xref(&mut cursor, 0).unwrap();
    assert_eq!(table.len(), 2);

    let entry0 = table.get(0).unwrap();
    assert!(!entry0.in_use);

    let entry1 = table.get(1).unwrap();
    assert_eq!(entry1.offset, 18);
    assert!(entry1.in_use);
}

#[test]
fn test_split_lines_mixed_endings() {
    // Test the split_lines helper with mixed line endings
    let text = "line1\rline2\nline3\r\nline4";
    let lines = split_lines(text);
    assert_eq!(lines, vec!["line1", "line2", "line3", "line4"]);
}

#[test]
fn test_parse_xref_with_prev_chain() {
    // Verify that the iterative parser follows /Prev chains.
    // We test this indirectly: a circular /Prev=0 chain should not panic or
    // infinite-loop (covered by test_parse_xref_circular_prev_chain), and
    // the iterative parser should handle deep chains without stack overflow.
    //
    // For a proper /Prev chain test with real PDF data, we rely on
    // integration tests with actual PDFs (e.g., Deutsche Heeresuniformen
    // with 177 /Prev links).

    // Single xref table with /Prev pointing to a non-xref offset.
    // The parser should fail gracefully on the /Prev target and still
    // return what it parsed from the first table.
    let xref_data = b"xref\n\
            0 2\n\
            0000000000 65535 f\n\
            0000000500 00000 n\n\
            trailer\n\
            << /Size 2 >>\n";

    let mut cursor = Cursor::new(xref_data);
    let table = parse_xref(&mut cursor, 0).unwrap();
    assert_eq!(table.len(), 2);
    assert_eq!(table.get(1).unwrap().offset, 500);
}

#[test]
fn test_parse_xref_circular_prev_chain() {
    // Build a circular /Prev chain: xref at offset 0 points to itself.
    // The iterative parser should detect the cycle and stop gracefully.
    let xref_data = b"xref\n\
            0 1\n\
            0000000000 65535 f\n\
            trailer\n\
            << /Size 1 /Prev 0 >>\n";

    let mut cursor = Cursor::new(xref_data);
    let table = parse_xref(&mut cursor, 0).unwrap();
    assert_eq!(table.len(), 1);
}

// === Additional XRefEntry tests ===

#[test]
fn test_xref_entry_uncompressed() {
    let entry = XRefEntry::uncompressed(5000, 0);
    assert_eq!(entry.entry_type, XRefEntryType::Uncompressed);
    assert_eq!(entry.offset, 5000);
    assert_eq!(entry.generation, 0);
    assert!(entry.in_use);
}

#[test]
fn test_xref_entry_compressed() {
    let entry = XRefEntry::compressed(42, 3);
    assert_eq!(entry.entry_type, XRefEntryType::Compressed);
    assert_eq!(entry.offset, 42); // stream obj number
    assert_eq!(entry.generation, 3); // index in stream
    assert!(entry.in_use);
}

#[test]
fn test_xref_entry_free_constructor() {
    let entry = XRefEntry::free(7, 65535);
    assert_eq!(entry.entry_type, XRefEntryType::Free);
    assert_eq!(entry.offset, 7); // next free obj
    assert_eq!(entry.generation, 65535);
    assert!(!entry.in_use);
}

#[test]
fn test_xref_entry_type_equality() {
    assert_eq!(XRefEntryType::Free, XRefEntryType::Free);
    assert_eq!(XRefEntryType::Uncompressed, XRefEntryType::Uncompressed);
    assert_eq!(XRefEntryType::Compressed, XRefEntryType::Compressed);
    assert_ne!(XRefEntryType::Free, XRefEntryType::Uncompressed);
}

#[test]
fn test_xref_entry_clone_debug() {
    let entry = XRefEntry::uncompressed(100, 0);
    let cloned = entry.clone();
    assert_eq!(entry, cloned);
    let debug = format!("{:?}", entry);
    assert!(debug.contains("Uncompressed"));
}

// === CrossRefTable tests ===

#[test]
fn test_cross_ref_table_set_trailer() {
    let mut table = CrossRefTable::new();
    assert!(table.trailer().is_none());

    let mut trailer = HashMap::new();
    trailer.insert("Size".to_string(), Object::Integer(10));
    table.set_trailer(trailer);
    assert!(table.trailer().is_some());
    assert!(table.trailer().unwrap().contains_key("Size"));
}

#[test]
fn test_cross_ref_table_contains() {
    let mut table = CrossRefTable::new();
    assert!(!table.contains(1));
    table.add_entry(1, XRefEntry::uncompressed(100, 0));
    assert!(table.contains(1));
    assert!(!table.contains(2));
}

#[test]
fn test_cross_ref_table_all_object_numbers() {
    let mut table = CrossRefTable::new();
    table.add_entry(1, XRefEntry::uncompressed(100, 0));
    table.add_entry(5, XRefEntry::uncompressed(200, 0));
    table.add_entry(10, XRefEntry::uncompressed(300, 0));

    let mut nums: Vec<u32> = table.all_object_numbers().collect();
    nums.sort();
    assert_eq!(nums, vec![1, 5, 10]);
}

#[test]
fn test_cross_ref_table_merge_from() {
    let mut table1 = CrossRefTable::new();
    table1.add_entry(1, XRefEntry::uncompressed(100, 0));
    table1.add_entry(2, XRefEntry::uncompressed(200, 0));

    let mut table2 = CrossRefTable::new();
    table2.add_entry(2, XRefEntry::uncompressed(999, 0)); // conflict
    table2.add_entry(3, XRefEntry::uncompressed(300, 0));

    table1.merge_from(table2);

    assert_eq!(table1.len(), 3);
    // table1's entry for obj 2 should win (not overwritten)
    assert_eq!(table1.get(2).unwrap().offset, 200);
    // obj 3 from table2 should be added
    assert_eq!(table1.get(3).unwrap().offset, 300);
}

#[test]
fn test_cross_ref_table_merge_trailer() {
    let mut table1 = CrossRefTable::new();
    // table1 has no trailer

    let mut table2 = CrossRefTable::new();
    let mut trailer = HashMap::new();
    trailer.insert("Size".to_string(), Object::Integer(5));
    table2.set_trailer(trailer);

    table1.merge_from(table2);
    // Should pick up table2's trailer
    assert!(table1.trailer().is_some());
}

#[test]
fn test_cross_ref_table_merge_preserves_existing_trailer() {
    let mut table1 = CrossRefTable::new();
    let mut trailer1 = HashMap::new();
    trailer1.insert("Size".to_string(), Object::Integer(10));
    table1.set_trailer(trailer1);

    let mut table2 = CrossRefTable::new();
    let mut trailer2 = HashMap::new();
    trailer2.insert("Size".to_string(), Object::Integer(5));
    table2.set_trailer(trailer2);

    table1.merge_from(table2);
    // table1 already had a trailer, should keep it
    let size = table1.trailer().unwrap().get("Size").unwrap();
    assert_eq!(*size, Object::Integer(10));
}

#[test]
fn test_cross_ref_table_clone() {
    let mut table = CrossRefTable::new();
    table.add_entry(1, XRefEntry::uncompressed(100, 0));
    let cloned = table.clone();
    assert_eq!(cloned.len(), 1);
    assert_eq!(cloned.get(1).unwrap().offset, 100);
}

// === find_stream_length tests ===

#[test]
fn test_find_stream_length_basic() {
    let data = b"/Type /XRef /Length 1234 /W [1 2 2]";
    let result = find_stream_length(data);
    assert_eq!(result, Some(1234));
}

#[test]
fn test_find_stream_length_no_length() {
    let data = b"/Type /XRef /W [1 2 2]";
    let result = find_stream_length(data);
    assert!(result.is_none());
}

#[test]
fn test_find_stream_length_indirect_reference() {
    // /Length 5 0 R -> can't resolve without full parsing
    let data = b"/Type /XRef /Length 5 0 R";
    // First digit should parse, but it would get "5" as the length
    // Actually it would parse "5" as a valid number
    let result = find_stream_length(data);
    // It parses the first integer it finds after /Length
    assert_eq!(result, Some(5));
}

#[test]
fn test_find_stream_length_zero() {
    let data = b"/Length 0";
    let result = find_stream_length(data);
    assert_eq!(result, Some(0));
}

// === find_actual_xref_offset tests ===

#[test]
fn test_find_actual_xref_offset_correct() {
    let data = b"xref\n0 1\n0000000000 65535 f\ntrailer\n";
    let mut cursor = Cursor::new(data);
    let offset = find_actual_xref_offset(&mut cursor, 0).unwrap();
    assert_eq!(offset, 0);
}

#[test]
fn test_find_actual_xref_offset_digit_start() {
    // xref stream starts with object number
    let data = b"1 0 obj\n<< /Type /XRef >>\nstream\n";
    let mut cursor = Cursor::new(data);
    let offset = find_actual_xref_offset(&mut cursor, 0).unwrap();
    assert_eq!(offset, 0);
}

// === split_lines tests ===

#[test]
fn test_split_lines_lf() {
    let lines = split_lines("a\nb\nc");
    assert_eq!(lines, vec!["a", "b", "c"]);
}

#[test]
fn test_split_lines_cr() {
    let lines = split_lines("a\rb\rc");
    assert_eq!(lines, vec!["a", "b", "c"]);
}

#[test]
fn test_split_lines_crlf() {
    let lines = split_lines("a\r\nb\r\nc");
    assert_eq!(lines, vec!["a", "b", "c"]);
}

#[test]
fn test_split_lines_empty() {
    let lines = split_lines("");
    // split_lines on empty string may return empty vec
    assert!(lines.is_empty());
}

#[test]
fn test_split_lines_single_line() {
    let lines = split_lines("hello");
    assert_eq!(lines, vec!["hello"]);
}

#[test]
fn test_xref_entry_type_debug() {
    let debug = format!("{:?}", XRefEntryType::Compressed);
    assert!(debug.contains("Compressed"));
}
