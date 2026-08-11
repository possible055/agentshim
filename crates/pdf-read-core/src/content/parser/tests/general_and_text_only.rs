use super::*;

#[test]
fn test_parse_simple_text() {
    let stream = b"BT /F1 12 Tf 100 700 Td (Hello) Tj ET";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 5);

    assert!(matches!(ops[0], Operator::BeginText));
    assert!(matches!(ops[1], Operator::Tf { ref font, size } if font == "F1" && size == 12.0));
    assert!(matches!(ops[2], Operator::Td { tx, ty } if tx == 100.0 && ty == 700.0));
    assert!(matches!(ops[3], Operator::Tj { .. }));
    assert!(matches!(ops[4], Operator::EndText));
}

#[test]
fn test_parse_text_matrix() {
    let stream = b"1 0 0 1 100 200 Tm";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);

    match &ops[0] {
        Operator::Tm { a, b, c, d, e, f } => {
            assert_eq!(*a, 1.0);
            assert_eq!(*b, 0.0);
            assert_eq!(*c, 0.0);
            assert_eq!(*d, 1.0);
            assert_eq!(*e, 100.0);
            assert_eq!(*f, 200.0);
        }
        _ => panic!("Expected Tm operator"),
    }
}

#[test]
fn test_parse_tj_array() {
    let stream = b"[(Hello) -100 (World)] TJ";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);

    match &ops[0] {
        Operator::TJ { array } => {
            assert_eq!(array.len(), 3);
            assert!(matches!(array[0], TextElement::String(_)));
            assert!(matches!(array[1], TextElement::Offset(-100.0)));
            assert!(matches!(array[2], TextElement::String(_)));
        }
        _ => panic!("Expected TJ operator"),
    }
}

#[test]
fn test_parse_color_operators() {
    // Add proper spacing between operators
    let stream = b"1 0 0 rg\n0 1 0 RG";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 2);

    match &ops[0] {
        Operator::SetFillRgb { r, g, b } => {
            assert_eq!(*r, 1.0);
            assert_eq!(*g, 0.0);
            assert_eq!(*b, 0.0);
        }
        _ => panic!("Expected rg operator"),
    }

    match &ops[1] {
        Operator::SetStrokeRgb { r, g, b } => {
            assert_eq!(*r, 0.0);
            assert_eq!(*g, 1.0);
            assert_eq!(*b, 0.0);
        }
        _ => panic!("Expected RG operator"),
    }
}

#[test]
fn test_parse_graphics_state() {
    let stream = b"q 1 0 0 1 50 50 cm Q";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 3);

    assert!(matches!(ops[0], Operator::SaveState));
    assert!(matches!(ops[1], Operator::Cm { .. }));
    assert!(matches!(ops[2], Operator::RestoreState));
}

#[test]
fn test_parse_t_star() {
    let stream = b"T*";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Operator::TStar));
}

#[test]
fn test_parse_text_state() {
    let stream = b"2 Tc 3 Tw 50 Tz 14 TL";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 4);

    assert!(matches!(ops[0], Operator::Tc { char_space } if char_space == 2.0));
    assert!(matches!(ops[1], Operator::Tw { word_space } if word_space == 3.0));
    assert!(matches!(ops[2], Operator::Tz { scale } if scale == 50.0));
    assert!(matches!(ops[3], Operator::TL { leading } if leading == 14.0));
}

#[test]
fn test_parse_quote_operators() {
    let stream = b"(Text1) ' 1 0.5 (Text2) \"";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 2);

    assert!(matches!(ops[0], Operator::Quote { .. }));
    assert!(matches!(ops[1], Operator::DoubleQuote { .. }));
}

#[test]
fn test_parse_path_operators() {
    let stream = b"100 200 m 150 250 l 10 10 50 50 re S";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 4);

    assert!(matches!(ops[0], Operator::MoveTo { x, y } if x == 100.0 && y == 200.0));
    assert!(matches!(ops[1], Operator::LineTo { x, y } if x == 150.0 && y == 250.0));
    assert!(matches!(ops[2], Operator::Rectangle { .. }));
    assert!(matches!(ops[3], Operator::Stroke));
}

#[test]
fn test_parse_do_operator() {
    let stream = b"/Im1 Do";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);

    match &ops[0] {
        Operator::Do { name } => {
            assert_eq!(name, "Im1");
        }
        _ => panic!("Expected Do operator"),
    }
}

#[test]
fn test_parse_empty_stream() {
    let stream = b"";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 0);
}

#[test]
fn test_parse_whitespace_only() {
    let stream = b"   \n  \t  ";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 0);
}

#[test]
fn test_parse_real_numbers() {
    let stream = b"1.5 2.7 Td";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);

    match &ops[0] {
        Operator::Td { tx, ty } => {
            assert_eq!(*tx, 1.5);
            assert_eq!(*ty, 2.7);
        }
        _ => panic!("Expected Td operator"),
    }
}

#[test]
fn test_content_stream_operator_limit() {
    // Build a stream with more than MAX_OPERATORS simple operators.
    // Each "q\n" is a SaveState operator (1 byte + newline).
    let count = super::MAX_OPERATORS + 1000;
    let stream: Vec<u8> = "q\n".repeat(count).into_bytes();
    let ops = parse_content_stream(&stream).unwrap();
    assert_eq!(ops.len(), super::MAX_OPERATORS);
}

#[test]
fn test_content_stream_consecutive_error_bailout() {
    // A stream of junk bytes that can't be parsed as any operator.
    // The parser should bail out after MAX_CONSECUTIVE_ERRORS skips.
    let junk = vec![0xFFu8; super::MAX_CONSECUTIVE_ERRORS + 500];
    let ops = parse_content_stream(&junk).unwrap();
    assert!(ops.is_empty());
}

// ── Tests for text-only parser ─────────────────────────────────────

#[test]
fn test_text_only_skips_graphics() {
    let stream = b"100 200 m 300 400 l S BT /F1 12 Tf (Hello) Tj ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert_eq!(ops.len(), 4);
    assert!(matches!(ops[0], Operator::BeginText));
    assert!(matches!(ops[1], Operator::Tf { ref font, size } if font == "F1" && size == 12.0));
    assert!(matches!(ops[2], Operator::Tj { .. }));
    assert!(matches!(ops[3], Operator::EndText));
}

#[test]
fn test_text_only_preserves_state_ops() {
    let stream = b"q 1 0 0 1 50 50 cm /Im1 Do Q";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert_eq!(ops.len(), 4);
    assert!(matches!(ops[0], Operator::SaveState));
    assert!(matches!(ops[1], Operator::Cm { .. }));
    assert!(matches!(ops[2], Operator::Do { ref name } if name == "Im1"));
    assert!(matches!(ops[3], Operator::RestoreState));
}

#[test]
fn test_do_resolves_name_when_stray_operands_precede_it() {
    // A `q ... /Name Do Q` sequence whose leading numeric operands were
    // never consumed by a `cm` (missing/dropped token, or any other
    // non-conformant producer quirk). Per ISO 32000-1:2008 §7.8.2 an
    // operator's operand is whatever immediately precedes it — here
    // that's the Name, not the stray numbers ahead of it — so Do must
    // still resolve "Overlay", not silently produce "".
    let stream = b"q 1 0 0 1 20 150 /Overlay Do Q";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(
        ops.iter()
            .any(|op| matches!(op, Operator::Do { ref name } if name == "Overlay")),
        "expected a Do operator naming \"Overlay\", got: {:?}",
        ops
    );

    // Same stream through the full parser (used when ink exclusion is active).
    let ops_full = parse_content_stream(stream).unwrap();
    assert!(ops_full
        .iter()
        .any(|op| matches!(op, Operator::Do { ref name } if name == "Overlay")));
}

#[test]
fn test_text_only_preserves_color_ops_outside_bt() {
    // Color operators outside BT/ET are NOT skipped: nothing reverts
    // them before a later BT, so a colour set here must still reach
    // GraphicsState for whatever text object comes next (regression
    // for the "colour set before BT extracted as black" bug fixed by
    // is_color_op_bytes in scan_graphics_region).
    let stream = b"1 0 0 rg 0.5 g /CS1 cs";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert_eq!(ops.len(), 3);
    assert!(matches!(ops[0], Operator::SetFillRgb { r, g, b } if r == 1.0 && g == 0.0 && b == 0.0));
    assert!(matches!(ops[1], Operator::SetFillGray { gray } if gray == 0.5));
    assert!(matches!(ops[2], Operator::SetFillColorSpace { ref name } if name == "CS1"));
}

#[test]
fn test_text_only_skips_complex_paths() {
    let stream = b"0 0 m 100 0 l 100 100 l 0 100 l h f 50 50 m 60 50 70 60 80 70 c S \
          BT /F1 10 Tf 72 700 Td (Text after paths) Tj ET \
          200 200 m 300 300 l S";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert_eq!(ops.len(), 5);
    assert!(matches!(ops[0], Operator::BeginText));
    assert!(matches!(ops[1], Operator::Tf { .. }));
    assert!(matches!(ops[2], Operator::Td { .. }));
    assert!(matches!(ops[3], Operator::Tj { .. }));
    assert!(matches!(ops[4], Operator::EndText));
}

#[test]
fn test_text_only_handles_marked_content() {
    let stream = b"/Span BMC BT (Hello) Tj ET EMC";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert_eq!(ops.len(), 5);
    assert!(matches!(ops[0], Operator::BeginMarkedContent { ref tag } if tag == "Span"));
    assert!(matches!(ops[1], Operator::BeginText));
    assert!(matches!(ops[2], Operator::Tj { .. }));
    assert!(matches!(ops[3], Operator::EndText));
    assert!(matches!(ops[4], Operator::EndMarkedContent));
}

#[test]
fn test_text_only_empty_and_whitespace() {
    assert_eq!(parse_content_stream_text_only(b"").unwrap().len(), 0);
    assert_eq!(
        parse_content_stream_text_only(b"   \n\t  ").unwrap().len(),
        0
    );
}

#[test]
fn test_text_only_graphics_only_stream() {
    let stream = b"0 0 m 100 0 l 100 100 l 0 100 l h f";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert_eq!(ops.len(), 0);
}

#[test]
fn test_text_only_dash_pattern_skipped() {
    let stream = b"[3 2] 0 d BT (Hi) Tj ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert_eq!(ops.len(), 3);
    assert!(matches!(ops[0], Operator::BeginText));
    assert!(matches!(ops[1], Operator::Tj { .. }));
    assert!(matches!(ops[2], Operator::EndText));
}

#[test]
fn test_text_only_gs_operator_preserved() {
    let stream = b"/GS0 gs BT (text) Tj ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert_eq!(ops.len(), 4);
    assert!(matches!(ops[0], Operator::SetExtGState { ref dict_name } if dict_name == "GS0"));
    assert!(matches!(ops[1], Operator::BeginText));
}

#[test]
fn test_text_only_matches_full_parse_for_text() {
    let stream = b"q 1 0 0 1 72 700 cm BT /F1 12 Tf 0 0 Td (Hello World) Tj ET Q";
    let full = parse_content_stream(stream).unwrap();
    let text_only = parse_content_stream_text_only(stream).unwrap();

    // text_only should have the same operators minus the path/clipping ones
    // In this case there are no graphics-only ops, so they should match
    assert_eq!(full.len(), text_only.len());
}

#[test]
fn test_skip_operand_token_numbers() {
    assert_eq!(skip_operand_token(b"123 ").unwrap().0, b" ");
    assert_eq!(skip_operand_token(b"-45.6 ").unwrap().0, b" ");
    assert_eq!(skip_operand_token(b"+0.5 ").unwrap().0, b" ");
    assert_eq!(skip_operand_token(b".002 ").unwrap().0, b" ");
}

#[test]
fn test_skip_operand_token_strings() {
    assert_eq!(skip_operand_token(b"(hello) ").unwrap().0, b" ");
    assert_eq!(skip_operand_token(b"(nested (parens)) ").unwrap().0, b" ");
    assert_eq!(skip_operand_token(b"(escaped \\) paren) ").unwrap().0, b" ");
    assert_eq!(skip_operand_token(b"<48656C6C6F> ").unwrap().0, b" ");
}

#[test]
fn test_skip_operand_token_names_arrays_dicts() {
    assert_eq!(skip_operand_token(b"/Name ").unwrap().0, b" ");
    assert_eq!(skip_operand_token(b"[1 2 3] ").unwrap().0, b" ");
    assert_eq!(skip_operand_token(b"[(text) -100] ").unwrap().0, b" ");
    assert_eq!(skip_operand_token(b"<< /K 1 >> ").unwrap().0, b" ");
}

#[test]
fn test_text_only_consecutive_error_bailout() {
    let junk = vec![0xFFu8; super::MAX_CONSECUTIVE_ERRORS + 500];
    let ops = parse_content_stream_text_only(&junk).unwrap();
    assert!(ops.is_empty());
}
