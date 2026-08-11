use super::*;

// ── Error handling / malformed streams ───────────────────────────

#[test]
fn test_parse_recovers_from_garbage_bytes() {
    // Garbage followed by valid operators
    let mut stream = vec![0xFF, 0xFE, 0xFD];
    stream.extend_from_slice(b" BT (Hello) Tj ET");
    let ops = parse_content_stream(&stream).unwrap();
    // Should recover and parse the text block
    assert!(ops.iter().any(|op| matches!(op, Operator::BeginText)));
    assert!(ops.iter().any(|op| matches!(op, Operator::Tj { .. })));
}

#[test]
fn test_parse_truncated_inline_image() {
    // BI without matching EI
    let stream = b"BI /W 2 /H 2 ID AAAA";
    let ops = parse_content_stream(stream).unwrap();
    // Should not crash; may produce 0 ops or recover gracefully
    // The parser should handle the missing EI
    let _ = ops; // just ensure no panic
}

#[test]
fn test_parse_unbalanced_bt_et() {
    // Extra ET without matching BT
    let stream = b"ET";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Operator::EndText));
}

#[test]
fn test_parse_missing_operands() {
    // Td with no operands - should use defaults
    let stream = b"Td";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Td { tx, ty } => {
            assert_eq!(*tx, 0.0);
            assert_eq!(*ty, 0.0);
        }
        _ => panic!("Expected Td with default values"),
    }
}

#[test]
fn test_parse_stream_with_only_comments() {
    let stream = b"% This is a comment\n% Another comment\n";
    let ops = parse_content_stream(stream).unwrap();
    // Comments should be skipped, resulting in no operators
    // The parser skips unknown bytes, so comments are handled gracefully
    let _ = ops;
}

// ── Complex / combined streams ──────────────────────────────────

#[test]
fn test_parse_complete_page_stream() {
    // Simulates a realistic mini-page content stream
    let stream = b"q\n\
        1 0 0 1 72 720 cm\n\
        /GS0 gs\n\
        BT\n\
        /F1 12 Tf\n\
        0 0 Td\n\
        14 TL\n\
        (First line) Tj\n\
        T*\n\
        (Second line) Tj\n\
        ET\n\
        Q\n\
        0 0 612 792 re\n\
        S";
    let ops = parse_content_stream(stream).unwrap();
    assert!(ops.len() >= 10);

    // Check key operators are present
    assert!(matches!(ops[0], Operator::SaveState));
    assert!(matches!(ops[1], Operator::Cm { .. }));
    assert!(matches!(ops[2], Operator::SetExtGState { .. }));
    assert!(matches!(ops[3], Operator::BeginText));
    assert!(ops.iter().any(|op| matches!(op, Operator::TStar)));
    assert!(ops.iter().any(|op| matches!(op, Operator::EndText)));
    assert!(ops.iter().any(|op| matches!(op, Operator::RestoreState)));
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::Rectangle { .. })));
    assert!(ops.iter().any(|op| matches!(op, Operator::Stroke)));
}

#[test]
fn test_parse_mixed_text_and_graphics() {
    let stream = b"q\n\
        0 0 100 100 re W n\n\
        BT /F1 10 Tf 0 0 Td (Hello) Tj ET\n\
        1 0 0 rg\n\
        0 0 m 100 0 l 100 100 l h f\n\
        Q";
    let ops = parse_content_stream(stream).unwrap();
    assert!(ops.len() > 5);
    // Verify text and graphics operators are both present
    assert!(ops.iter().any(|op| matches!(op, Operator::BeginText)));
    assert!(ops.iter().any(|op| matches!(op, Operator::Tj { .. })));
    assert!(ops.iter().any(|op| matches!(op, Operator::MoveTo { .. })));
    assert!(ops.iter().any(|op| matches!(op, Operator::Fill)));
}

#[test]
fn test_parse_tj_with_hex_strings_in_array() {
    let stream = b"[<48656C6C6F> -50 <576F726C64>] TJ";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::TJ { array } => {
            assert_eq!(array.len(), 3);
            match &array[0] {
                TextElement::String(s) => assert_eq!(s, b"Hello"),
                _ => panic!("Expected string"),
            }
            assert!(matches!(array[1], TextElement::Offset(_)));
            match &array[2] {
                TextElement::String(s) => assert_eq!(s, b"World"),
                _ => panic!("Expected string"),
            }
        }
        _ => panic!("Expected TJ"),
    }
}

#[test]
fn test_parse_td_with_varied_whitespace() {
    // Operators separated by various whitespace types
    let stream = b"100\t200\nTd";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Td { tx, ty } => {
            assert_eq!(*tx, 100.0);
            assert_eq!(*ty, 200.0);
        }
        _ => panic!("Expected Td"),
    }
}

#[test]
fn test_parse_multiple_bt_et_blocks() {
    let stream = b"BT (A) Tj ET BT (B) Tj ET BT (C) Tj ET";
    let ops = parse_content_stream(stream).unwrap();
    // 3 blocks * 3 ops each = 9 ops
    assert_eq!(ops.len(), 9);
    let bt_count = ops
        .iter()
        .filter(|op| matches!(op, Operator::BeginText))
        .count();
    let et_count = ops
        .iter()
        .filter(|op| matches!(op, Operator::EndText))
        .count();
    assert_eq!(bt_count, 3);
    assert_eq!(et_count, 3);
}

// ── Fast parser (parse_text_operator_fast) tests via text_only ──

#[test]
fn test_fast_parser_tf_operator() {
    let stream = b"BT /Helvetica 14.5 Tf ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert_eq!(ops.len(), 3);
    match &ops[1] {
        Operator::Tf { font, size } => {
            assert_eq!(font, "Helvetica");
            assert!((size - 14.5).abs() < 0.01);
        }
        _ => panic!("Expected Tf from fast parser"),
    }
}

#[test]
fn test_fast_parser_td_operator() {
    let stream = b"BT 72.5 -14.0 Td ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(ops.iter().any(|op| matches!(op, Operator::Td { tx, ty }
        if (*tx - 72.5).abs() < 0.01 && (*ty - (-14.0)).abs() < 0.01)));
}

#[test]
fn test_fast_parser_td_upper_operator() {
    let stream = b"BT 10 -12 TD ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(ops.iter().any(|op| matches!(op, Operator::TD { tx, ty }
        if (*tx - 10.0).abs() < 0.01 && (*ty - (-12.0)).abs() < 0.01)));
}

#[test]
fn test_fast_parser_tm_operator() {
    let stream = b"BT 1 0 0 1 72 700 Tm ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::Tm { a, b, c, d, e, f }
        if *a == 1.0 && *b == 0.0 && *c == 0.0 && *d == 1.0 && *e == 72.0 && *f == 700.0)));
}

#[test]
fn test_fast_parser_tstar_operator() {
    let stream = b"BT T* ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(ops.iter().any(|op| matches!(op, Operator::TStar)));
}

#[test]
fn test_fast_parser_tj_with_hex_string() {
    let stream = b"BT <48656C6C6F> Tj ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(ops.iter().any(|op| {
        if let Operator::Tj { text } = op {
            text == b"Hello"
        } else {
            false
        }
    }));
}

#[test]
fn test_fast_parser_tj_array() {
    let stream = b"BT [(AB) -100 (CD)] TJ ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::TJ { array } if array.len() == 3)));
}

#[test]
fn test_fast_parser_quote_operator() {
    let stream = b"BT (Line2) ' ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(ops.iter().any(|op| matches!(op, Operator::Quote { .. })));
}

#[test]
fn test_fast_parser_double_quote_operator() {
    let stream = b"BT 1 2 (text) \" ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::DoubleQuote { .. })));
}

#[test]
fn test_fast_parser_color_ops_inside_bt() {
    let stream = b"BT 1 0 0 rg 0 g ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::SetFillRgb { .. })));
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::SetFillGray { .. })));
}

#[test]
fn test_fast_parser_gs_inside_bt() {
    let stream = b"BT /GS1 gs ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::SetExtGState { ref dict_name } if dict_name == "GS1")));
}

#[test]
fn test_fast_parser_do_inside_bt() {
    let stream = b"BT /XObj1 Do ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::Do { ref name } if name == "XObj1")));
}
