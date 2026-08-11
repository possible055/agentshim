use super::*;

// ── Tests for prescan_text_regions (P1 memchr optimization) ──────

#[test]
fn test_prescan_single_bt_et() {
    let stream = b"BT /F1 12 Tf (Hello) Tj ET";
    let result = prescan_text_regions(stream);
    assert!(result.is_some(), "Should return Some for valid stream");
    let regions = result.unwrap().regions().to_vec();
    assert!(!regions.is_empty(), "Should find at least 1 region");
    let (start, end) = regions[0];
    assert_eq!(start, 0, "Region should start at BT");
    assert!(end >= 26, "Region should extend to or past ET");
}

#[test]
fn test_prescan_multiple_bt_et() {
    let stream = b"BT (A) Tj ET BT (B) Tj ET BT (C) Tj ET";
    let result = prescan_text_regions(stream);
    assert!(result.is_some());
    let regions = result.unwrap().regions().to_vec();
    assert!(
        !regions.is_empty(),
        "Should find regions for 3 BT/ET blocks"
    );
}

#[test]
fn test_prescan_do_operator() {
    let stream = b"/Im1 Do";
    let result = prescan_text_regions(stream);
    assert!(result.is_some());
    let regions = result.unwrap().regions().to_vec();
    assert!(!regions.is_empty(), "Should find region for Do operator");
}

#[test]
fn test_prescan_no_text_ops() {
    let stream = b"100 200 m 300 400 l S 0 0 100 100 re f";
    let result = prescan_text_regions(stream);
    assert!(result.is_some());
    let regions = result.unwrap().regions().to_vec();
    assert!(
        regions.is_empty(),
        "Pure graphics should return empty regions, got {:?}",
        regions
    );
}

#[test]
fn test_prescan_bt_in_string_literal() {
    let stream = b"(text BT here) Tj";
    let result = prescan_text_regions(stream);
    assert!(
        result.is_some(),
        "Should not return None for string containing BT"
    );
}

#[test]
fn test_prescan_merges_overlapping_regions() {
    let stream = b"q BT (A) Tj ET Q q BT (B) Tj ET Q";
    let result = prescan_text_regions(stream);
    assert!(result.is_some());
    let regions = result.unwrap().regions().to_vec();
    assert!(!regions.is_empty());
    for i in 1..regions.len() {
        assert!(
            regions[i].0 >= regions[i - 1].1,
            "Regions should not overlap after merge: {:?} and {:?}",
            regions[i - 1],
            regions[i]
        );
    }
}

// ══════════════════════════════════════════════════════════════════
// Additional coverage tests
// ══════════════════════════════════════════════════════════════════

// ── Inline image parsing ────────────────────────────────────────

#[test]
fn test_parse_inline_image_basic() {
    // BI /W 4 /H 4 /BPC 8 /CS /DeviceGray ID <4 bytes data> EI
    let stream = b"BI /W 4 /H 4 /BPC 8 /CS /DeviceGray ID ABCD EI";
    let ops = parse_content_stream(stream).unwrap();
    // The parser should produce at least one InlineImage
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::InlineImage { .. })));
    // Check that the inline image dict contains expected keys
    for op in &ops {
        if let Operator::InlineImage { dict, data } = op {
            assert_eq!(dict.get("W").and_then(|o| o.as_integer()), Some(4));
            assert_eq!(dict.get("H").and_then(|o| o.as_integer()), Some(4));
            assert_eq!(dict.get("BPC").and_then(|o| o.as_integer()), Some(8));
            assert!(!data.is_empty());
        }
    }
}

#[test]
fn test_parse_inline_image_empty_data() {
    // Inline image with minimal data
    let stream = b"BI /W 1 /H 1 ID X EI";
    let ops = parse_content_stream(stream).unwrap();
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::InlineImage { .. })));
}

#[test]
fn test_parse_inline_image_in_stream_context() {
    // Inline image surrounded by other operators
    let stream = b"q 1 0 0 1 0 0 cm BI /W 2 /H 2 ID AB EI Q";
    let ops = parse_content_stream(stream).unwrap();
    assert!(ops.len() >= 3);
    assert!(matches!(ops[0], Operator::SaveState));
    assert!(matches!(ops[1], Operator::Cm { .. }));
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::InlineImage { .. })));
    assert!(ops.iter().any(|op| matches!(op, Operator::RestoreState)));
}

#[test]
fn test_parse_inline_image_null_before_ei() {
    // PDF spec Table 1 lists NULL (0x00) as whitespace.
    let mut stream = b"q BI /W 2 /H 2 ID AB".to_vec();
    stream.extend_from_slice(b"\x00EI Q BT (Hi) Tj ET");
    let ops = parse_content_stream(&stream).unwrap();
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::InlineImage { .. })));
    assert!(ops.iter().any(|op| matches!(op, Operator::RestoreState)));
    // Text after the inline image must still be extracted
    assert!(ops.iter().any(|op| matches!(op, Operator::Tj { .. })));
}
