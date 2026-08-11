use super::*;

// ── scan_graphics_region tests ──────────────────────────────────

#[test]
fn test_scan_graphics_region_finds_bt() {
    let data = b"100 200 m 300 400 l BT";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    assert!(matches!(result, ScanResult::FoundBT { .. }));
}

#[test]
fn test_scan_graphics_region_finds_bi() {
    let data = b"100 200 m BI";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    assert!(matches!(result, ScanResult::InlineImage { .. }));
}

#[test]
fn test_scan_graphics_region_end_of_data() {
    let data = b"100 200 300 ";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    assert!(matches!(result, ScanResult::EndOfData));
}

#[test]
fn test_scan_graphics_region_skips_path_ops() {
    // A stream with only skippable path operators followed by BT
    let data = b"100 200 m 300 400 l h f BT";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    assert!(matches!(result, ScanResult::FoundBT { .. }));
}

#[test]
fn test_scan_graphics_region_unmatched_q() {
    // Q without matching q should yield SimpleOp
    let data = b"Q";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    assert!(matches!(
        result,
        ScanResult::SimpleOp {
            op: Operator::RestoreState,
            ..
        }
    ));
}

#[test]
fn test_scan_graphics_region_deferred_q_with_trigger() {
    // q followed by Do should yield DeferredThenText
    let data = b"q 1 0 0 1 0 0 cm /Im1 Do";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    assert!(matches!(result, ScanResult::DeferredThenText { .. }));
}

#[test]
fn test_scan_graphics_region_cm_with_inline_floats() {
    // cm outside q context should try inline parse
    let data = b"1 0 0 1 72 700 cm";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    match result {
        ScanResult::SimpleOp {
            op: Operator::Cm { a, b, c, d, e, f },
            ..
        } => {
            assert_eq!(a, 1.0);
            assert_eq!(b, 0.0);
            assert_eq!(c, 0.0);
            assert_eq!(d, 1.0);
            assert_eq!(e, 72.0);
            assert_eq!(f, 700.0);
        }
        _ => panic!(
            "Expected SimpleOp with Cm, got {:?}",
            match result {
                ScanResult::EndOfData => "EndOfData",
                ScanResult::FoundBT { .. } => "FoundBT",
                ScanResult::InlineImage { .. } => "InlineImage",
                ScanResult::NeedFullParse { .. } => "NeedFullParse",
                ScanResult::DeferredThenText { .. } => "DeferredThenText",
                ScanResult::SimpleOp { .. } => "SimpleOp (wrong variant)",
                ScanResult::TooManyErrors { .. } => "TooManyErrors",
            }
        ),
    }
}

#[test]
fn test_scan_graphics_region_skips_comments() {
    let data = b"% this is a comment\nBT";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    assert!(matches!(result, ScanResult::FoundBT { .. }));
}

#[test]
fn test_scan_graphics_region_skips_strings() {
    let data = b"(some string) BT";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    // After skipping the string, should eventually find BT
    // (or NeedFullParse since string is an operand to an unknown op)
    assert!(!matches!(result, ScanResult::TooManyErrors { .. }));
}

#[test]
fn test_scan_graphics_region_skips_hex_strings() {
    let data = b"<4142> BT";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    assert!(!matches!(result, ScanResult::TooManyErrors { .. }));
}

#[test]
fn test_scan_graphics_region_skips_arrays() {
    let data = b"[1 2 3] BT";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    assert!(!matches!(result, ScanResult::TooManyErrors { .. }));
}

#[test]
fn test_scan_graphics_region_skips_dicts() {
    let data = b"<< /K 1 >> BT";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    assert!(!matches!(result, ScanResult::TooManyErrors { .. }));
}

#[test]
fn test_scan_graphics_region_skips_names() {
    let data = b"/Name BT";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    assert!(!matches!(result, ScanResult::TooManyErrors { .. }));
}

#[test]
fn test_scan_graphics_region_keyword_operands() {
    // "true", "false", "null" should be treated as operands, not operators
    let data = b"true false null BT";
    let mut errors = 0usize;
    let result = scan_graphics_region(data, &mut errors);
    assert!(matches!(result, ScanResult::FoundBT { .. }));
}

// ── Raw skip functions tests ────────────────────────────────────

#[test]
fn test_skip_literal_string_raw_basic() {
    let data = b"(Hello) rest";
    let result = skip_literal_string_raw(data, 0);
    assert_eq!(result, Some(7));
}

#[test]
fn test_skip_literal_string_raw_nested() {
    let data = b"(Hello (World)) rest";
    let result = skip_literal_string_raw(data, 0);
    assert_eq!(result, Some(15));
}

#[test]
fn test_skip_literal_string_raw_escaped() {
    let data = b"(Hello\\)World) rest";
    let result = skip_literal_string_raw(data, 0);
    assert_eq!(result, Some(14));
}

#[test]
fn test_skip_literal_string_raw_unterminated() {
    let data = b"(Hello";
    let result = skip_literal_string_raw(data, 0);
    assert!(result.is_none());
}

#[test]
fn test_skip_hex_string_raw_basic() {
    let data = b"<4142> rest";
    let result = skip_hex_string_raw(data, 0);
    assert_eq!(result, Some(6));
}

#[test]
fn test_skip_hex_string_raw_unterminated() {
    let data = b"<4142";
    let result = skip_hex_string_raw(data, 0);
    assert!(result.is_none());
}

#[test]
fn test_skip_name_raw_basic() {
    let data = b"/FontName 12";
    let result = skip_name_raw(data, 0);
    assert_eq!(result, 9);
}

#[test]
fn test_skip_array_raw_basic() {
    let data = b"[1 2 3] rest";
    let result = skip_array_raw(data, 0);
    assert_eq!(result, Some(7));
}

#[test]
fn test_skip_array_raw_nested() {
    let data = b"[1 [2 3] 4]";
    let result = skip_array_raw(data, 0);
    assert_eq!(result, Some(data.len()));
}

#[test]
fn test_skip_array_raw_with_string() {
    let data = b"[(Hello) 1]";
    let result = skip_array_raw(data, 0);
    assert_eq!(result, Some(data.len()));
}

#[test]
fn test_skip_array_raw_with_dict() {
    let data = b"[<< /K 1 >>]";
    let result = skip_array_raw(data, 0);
    assert_eq!(result, Some(data.len()));
}

#[test]
fn test_skip_array_raw_with_hex_string() {
    let data = b"[<4142>] rest";
    let result = skip_array_raw(data, 0);
    assert_eq!(result, Some(8));
}

#[test]
fn test_skip_array_raw_unterminated() {
    let data = b"[1 2 3";
    let result = skip_array_raw(data, 0);
    assert!(result.is_none());
}

#[test]
fn test_skip_dict_raw_basic() {
    let data = b"<< /K 1 >>";
    let result = skip_dict_raw(data, 0);
    assert_eq!(result, Some(data.len()));
}

#[test]
fn test_skip_dict_raw_nested() {
    let data = b"<< /A << /B 1 >> >>";
    let result = skip_dict_raw(data, 0);
    assert_eq!(result, Some(data.len()));
}

#[test]
fn test_skip_dict_raw_with_string() {
    let data = b"<< /K (Hello) >>";
    let result = skip_dict_raw(data, 0);
    assert_eq!(result, Some(data.len()));
}

#[test]
fn test_skip_dict_raw_with_hex_string() {
    let data = b"<< /K <4142> >>";
    let result = skip_dict_raw(data, 0);
    assert_eq!(result, Some(data.len()));
}

#[test]
fn test_skip_dict_raw_unterminated() {
    let data = b"<< /K 1";
    let result = skip_dict_raw(data, 0);
    assert!(result.is_none());
}

// ── is_skippable_graphics_op_bytes tests ────────────────────────

#[test]
fn test_is_skippable_path_construction() {
    assert!(is_skippable_graphics_op_bytes(b"m"));
    assert!(is_skippable_graphics_op_bytes(b"l"));
    assert!(is_skippable_graphics_op_bytes(b"c"));
    assert!(is_skippable_graphics_op_bytes(b"v"));
    assert!(is_skippable_graphics_op_bytes(b"y"));
    assert!(is_skippable_graphics_op_bytes(b"h"));
    assert!(is_skippable_graphics_op_bytes(b"re"));
}

#[test]
fn test_is_skippable_path_painting() {
    assert!(is_skippable_graphics_op_bytes(b"S"));
    assert!(is_skippable_graphics_op_bytes(b"s"));
    assert!(is_skippable_graphics_op_bytes(b"f"));
    assert!(is_skippable_graphics_op_bytes(b"F"));
    assert!(is_skippable_graphics_op_bytes(b"f*"));
    assert!(is_skippable_graphics_op_bytes(b"B"));
    assert!(is_skippable_graphics_op_bytes(b"B*"));
    assert!(is_skippable_graphics_op_bytes(b"b"));
    assert!(is_skippable_graphics_op_bytes(b"b*"));
    assert!(is_skippable_graphics_op_bytes(b"n"));
}

#[test]
fn test_is_skippable_clipping() {
    assert!(is_skippable_graphics_op_bytes(b"W"));
    assert!(is_skippable_graphics_op_bytes(b"W*"));
}

#[test]
fn test_is_skippable_graphics_state() {
    assert!(is_skippable_graphics_op_bytes(b"w"));
    assert!(is_skippable_graphics_op_bytes(b"J"));
    assert!(is_skippable_graphics_op_bytes(b"j"));
    assert!(is_skippable_graphics_op_bytes(b"M"));
    assert!(is_skippable_graphics_op_bytes(b"d"));
    assert!(is_skippable_graphics_op_bytes(b"i"));
    assert!(is_skippable_graphics_op_bytes(b"ri"));
    assert!(is_skippable_graphics_op_bytes(b"sh"));
}

#[test]
fn test_is_skippable_color() {
    assert!(is_skippable_graphics_op_bytes(b"rg"));
    assert!(is_skippable_graphics_op_bytes(b"RG"));
    assert!(is_skippable_graphics_op_bytes(b"g"));
    assert!(is_skippable_graphics_op_bytes(b"G"));
    assert!(is_skippable_graphics_op_bytes(b"k"));
    assert!(is_skippable_graphics_op_bytes(b"K"));
    assert!(is_skippable_graphics_op_bytes(b"cs"));
    assert!(is_skippable_graphics_op_bytes(b"CS"));
    assert!(is_skippable_graphics_op_bytes(b"sc"));
    assert!(is_skippable_graphics_op_bytes(b"SC"));
    assert!(is_skippable_graphics_op_bytes(b"scn"));
    assert!(is_skippable_graphics_op_bytes(b"SCN"));
}
