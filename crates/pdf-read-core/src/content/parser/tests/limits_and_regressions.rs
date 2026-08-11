use super::*;

#[test]
fn test_is_not_skippable() {
    assert!(!is_skippable_graphics_op_bytes(b"BT"));
    assert!(!is_skippable_graphics_op_bytes(b"ET"));
    assert!(!is_skippable_graphics_op_bytes(b"Tj"));
    assert!(!is_skippable_graphics_op_bytes(b"TJ"));
    assert!(!is_skippable_graphics_op_bytes(b"Td"));
    assert!(!is_skippable_graphics_op_bytes(b"Tm"));
    assert!(!is_skippable_graphics_op_bytes(b"Tf"));
    assert!(!is_skippable_graphics_op_bytes(b"Do"));
    assert!(!is_skippable_graphics_op_bytes(b"cm"));
    assert!(!is_skippable_graphics_op_bytes(b"q"));
    assert!(!is_skippable_graphics_op_bytes(b"Q"));
    assert!(!is_skippable_graphics_op_bytes(b"gs"));
    assert!(!is_skippable_graphics_op_bytes(b"BI"));
}

// ── BYTE_CLASS table tests ──────────────────────────────────────

#[test]
fn test_byte_class_whitespace_and_digits() {
    assert_eq!(BYTE_CLASS[b' ' as usize], SCAN_SKIP);
    assert_eq!(BYTE_CLASS[b'\t' as usize], SCAN_SKIP);
    assert_eq!(BYTE_CLASS[b'\n' as usize], SCAN_SKIP);
    assert_eq!(BYTE_CLASS[b'\r' as usize], SCAN_SKIP);
    assert_eq!(BYTE_CLASS[0x0C], SCAN_SKIP);
    assert_eq!(BYTE_CLASS[0x00], SCAN_SKIP);
    for d in b'0'..=b'9' {
        assert_eq!(
            BYTE_CLASS[d as usize], SCAN_SKIP,
            "digit {} should be SKIP",
            d as char
        );
    }
    assert_eq!(BYTE_CLASS[b'.' as usize], SCAN_SKIP);
    assert_eq!(BYTE_CLASS[b'+' as usize], SCAN_SKIP);
    assert_eq!(BYTE_CLASS[b'-' as usize], SCAN_SKIP);
}

#[test]
fn test_byte_class_alpha() {
    for c in b'A'..=b'Z' {
        assert_eq!(
            BYTE_CLASS[c as usize], SCAN_ALPHA,
            "uppercase {} should be ALPHA",
            c as char
        );
    }
    for c in b'a'..=b'z' {
        assert_eq!(
            BYTE_CLASS[c as usize], SCAN_ALPHA,
            "lowercase {} should be ALPHA",
            c as char
        );
    }
    assert_eq!(BYTE_CLASS[b'\'' as usize], SCAN_ALPHA);
    assert_eq!(BYTE_CLASS[b'"' as usize], SCAN_ALPHA);
    assert_eq!(BYTE_CLASS[b'*' as usize], SCAN_ALPHA);
}

#[test]
fn test_byte_class_delimiters() {
    assert_eq!(BYTE_CLASS[b'(' as usize], SCAN_PAREN);
    assert_eq!(BYTE_CLASS[b'<' as usize], SCAN_ANGLE);
    assert_eq!(BYTE_CLASS[b'[' as usize], SCAN_BRACKET);
    assert_eq!(BYTE_CLASS[b'/' as usize], SCAN_SLASH);
    assert_eq!(BYTE_CLASS[b'%' as usize], SCAN_PERCENT);
}

// ── find_ei_operator tests ──────────────────────────────────────

#[test]
fn test_find_ei_operator_basic() {
    let data = b"binary data \nEI ";
    let result = find_ei_operator(data);
    assert!(result.is_ok());
}

#[test]
fn test_find_ei_operator_at_end() {
    let data = b"data \nEI";
    let result = find_ei_operator(data);
    assert!(result.is_ok());
}

#[test]
fn test_find_ei_operator_not_found() {
    let data = b"binary data without end marker";
    let result = find_ei_operator(data);
    assert!(result.is_err());
}

#[test]
fn test_find_ei_operator_ei_without_whitespace_prefix() {
    // EI without preceding whitespace should not match
    let data = b"dataEI ";
    let result = find_ei_operator(data);
    assert!(result.is_err());
}

// ── Operator limit enforcement ──────────────────────────────────

#[test]
fn test_text_only_operator_limit() {
    let count = super::MAX_OPERATORS + 500;
    let mut stream: Vec<u8> = Vec::new();
    stream.extend_from_slice(b"BT\n");
    for _ in 0..count {
        stream.extend_from_slice(b"T*\n");
    }
    stream.extend_from_slice(b"ET\n");
    let ops = parse_content_stream_text_only(&stream).unwrap();
    assert!(ops.len() <= super::MAX_OPERATORS + 1); // +1 for possible BT
}

#[test]
fn test_images_only_operator_limit() {
    let count = super::MAX_OPERATORS + 500;
    let stream: Vec<u8> = "q\n".repeat(count).into_bytes();
    let ops = parse_content_stream_images_only(&stream).unwrap();
    assert!(ops.len() <= super::MAX_OPERATORS);
}

// ── Consistency tests ───────────────────────────────────────────

#[test]
fn test_full_and_text_only_agree_on_text_operators() {
    let stream = b"BT /F1 12 Tf 72 700 Td (Test) Tj T* (Line2) Tj ET";
    let full = parse_content_stream(stream).unwrap();
    let text_only = parse_content_stream_text_only(stream).unwrap();
    // For a pure-text stream, full and text_only should be identical
    assert_eq!(full.len(), text_only.len());
    for (f, t) in full.iter().zip(text_only.iter()) {
        assert_eq!(f, t);
    }
}

#[test]
fn test_full_parse_and_execute_agree() {
    let stream = b"BT /F1 12 Tf (Hello) Tj ET";
    let full = parse_content_stream(stream).unwrap();
    let mut exec_ops = Vec::new();
    parse_and_execute_text_only(stream, |op| {
        exec_ops.push(op);
        Ok(())
    })
    .unwrap();
    // Both should produce the same text operators
    assert_eq!(full.len(), exec_ops.len());
}

// ── Edge cases for operand parsing via nom ──────────────────────

#[test]
fn test_skip_operand_token_dict_with_strings() {
    assert_eq!(skip_operand_token(b"<< /Key (value) >> ").unwrap().0, b" ");
}

#[test]
fn test_skip_operand_token_nested_array() {
    assert_eq!(skip_operand_token(b"[1 [2 3] 4] ").unwrap().0, b" ");
}

#[test]
fn test_skip_operand_token_array_with_hex_string() {
    assert_eq!(skip_operand_token(b"[<4142> 1] ").unwrap().0, b" ");
}

#[test]
fn test_skip_operand_token_array_with_dict() {
    assert_eq!(skip_operand_token(b"[<< /K 1 >>] ").unwrap().0, b" ");
}

#[test]
fn test_skip_operand_token_dict_with_nested_dict() {
    assert_eq!(skip_operand_token(b"<< /A << /B 1 >> >> ").unwrap().0, b" ");
}

#[test]
fn test_skip_operand_token_dict_with_hex_string() {
    assert_eq!(skip_operand_token(b"<< /K <4142> >> ").unwrap().0, b" ");
}

#[test]
fn test_skip_operand_token_errors() {
    // Characters that don't start a valid operand
    assert!(skip_operand_token(b"").is_err());
    assert!(skip_operand_token(b"@").is_err());
}

#[test]
fn test_prescan_forward_ctm_preserves_marked_content() {
    // Regression test: when prescan forward CTM builds regions starting at BT,
    // preceding BDC/BMC operators must be preserved so tagged PDF structure
    // (BDC ... BT ... ET ... EMC) is not broken.
    let mut cs = Vec::new();

    // Outer graphics state with a CTM transform
    cs.extend_from_slice(b"q\n");
    cs.extend_from_slice(b"1.5 0 0 1.5 50 50 cm\n");

    // Filler to push beyond 256KB threshold
    for i in 0..13000u32 {
        let line = format!(
            "{}.0 {}.0 m {}.0 {}.0 l n\n",
            i % 500,
            (i * 7) % 500,
            (i * 3) % 500,
            (i * 11) % 500
        );
        cs.extend_from_slice(line.as_bytes());
    }
    assert!(cs.len() > 256 * 1024);

    // Tagged structure: BDC wrapping BT/ET, then EMC
    cs.extend_from_slice(b"/Span << /MCID 0 >> BDC\n");
    cs.extend_from_slice(b"BT\n");
    cs.extend_from_slice(b"/F1 12 Tf\n");
    cs.extend_from_slice(b"(Hello tagged) Tj\n");
    cs.extend_from_slice(b"ET\n");
    cs.extend_from_slice(b"EMC\n");

    cs.extend_from_slice(b"Q\n");

    let mut ops = Vec::new();
    parse_and_execute_text_only(&cs, |op| {
        ops.push(op);
        Ok(())
    })
    .unwrap();

    // Verify BeginMarkedContentDict is present in the output
    let has_bdc = ops
        .iter()
        .any(|op| matches!(op, Operator::BeginMarkedContentDict { tag, .. } if tag == "Span"));
    assert!(
        has_bdc,
        "BeginMarkedContentDict(/Span) must be preserved; got ops: {:?}",
        ops.iter()
            .map(|op| format!("{:?}", std::mem::discriminant(op)))
            .collect::<Vec<_>>()
    );

    // Verify EndMarkedContent (EMC) is also present
    let has_emc = ops
        .iter()
        .any(|op| matches!(op, Operator::EndMarkedContent));
    assert!(
        has_emc,
        "EndMarkedContent (EMC) must be preserved in tagged PDF output"
    );
}
