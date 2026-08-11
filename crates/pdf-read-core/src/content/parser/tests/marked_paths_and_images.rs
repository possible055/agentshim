use super::*;

// ── Marked content operators ────────────────────────────────────

#[test]
fn test_parse_bmc() {
    let stream = b"/Span BMC";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::BeginMarkedContent { tag } => assert_eq!(tag, "Span"),
        _ => panic!("Expected BeginMarkedContent"),
    }
}

#[test]
fn test_parse_bdc_with_dict() {
    let stream = b"/Span << /MCID 0 >> BDC";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::BeginMarkedContentDict { tag, properties } => {
            assert_eq!(tag, "Span");
            assert!(!matches!(**properties, Object::Null));
        }
        _ => panic!("Expected BeginMarkedContentDict"),
    }
}

#[test]
fn test_parse_bdc_with_name_ref() {
    let stream = b"/Span /MC0 BDC";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::BeginMarkedContentDict { tag, properties } => {
            assert_eq!(tag, "Span");
            assert_eq!(properties.as_name(), Some("MC0"));
        }
        _ => panic!("Expected BeginMarkedContentDict"),
    }
}

#[test]
fn test_parse_emc() {
    let stream = b"EMC";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Operator::EndMarkedContent));
}

#[test]
fn test_parse_marked_content_nesting() {
    let stream = b"/Article BMC /P << /MCID 1 >> BDC BT (text) Tj ET EMC EMC";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 7);
    assert!(matches!(ops[0], Operator::BeginMarkedContent { .. }));
    assert!(matches!(ops[1], Operator::BeginMarkedContentDict { .. }));
    assert!(matches!(ops[2], Operator::BeginText));
    assert!(matches!(ops[3], Operator::Tj { .. }));
    assert!(matches!(ops[4], Operator::EndText));
    assert!(matches!(ops[5], Operator::EndMarkedContent));
    assert!(matches!(ops[6], Operator::EndMarkedContent));
}

// ── Path operators ──────────────────────────────────────────────

#[test]
fn test_parse_bezier_curves() {
    let stream = b"10 20 30 40 50 60 c";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::CurveTo {
            x1,
            y1,
            x2,
            y2,
            x3,
            y3,
        } => {
            assert_eq!(*x1, 10.0);
            assert_eq!(*y1, 20.0);
            assert_eq!(*x2, 30.0);
            assert_eq!(*y2, 40.0);
            assert_eq!(*x3, 50.0);
            assert_eq!(*y3, 60.0);
        }
        _ => panic!("Expected CurveTo"),
    }
}

#[test]
fn test_parse_curve_to_v() {
    let stream = b"10 20 30 40 v";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::CurveToV { x2, y2, x3, y3 } => {
            assert_eq!(*x2, 10.0);
            assert_eq!(*y2, 20.0);
            assert_eq!(*x3, 30.0);
            assert_eq!(*y3, 40.0);
        }
        _ => panic!("Expected CurveToV"),
    }
}

#[test]
fn test_parse_curve_to_y() {
    let stream = b"10 20 30 40 y";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::CurveToY { x1, y1, x3, y3 } => {
            assert_eq!(*x1, 10.0);
            assert_eq!(*y1, 20.0);
            assert_eq!(*x3, 30.0);
            assert_eq!(*y3, 40.0);
        }
        _ => panic!("Expected CurveToY"),
    }
}

#[test]
fn test_parse_close_path() {
    let stream = b"h";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Operator::ClosePath));
}

#[test]
fn test_parse_fill_variants() {
    let stream = b"f\nf*\nn";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 3);
    assert!(matches!(ops[0], Operator::Fill));
    assert!(matches!(ops[1], Operator::FillEvenOdd));
    assert!(matches!(ops[2], Operator::EndPath));
}

#[test]
fn test_parse_close_fill_stroke() {
    let stream = b"b";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Operator::CloseFillStroke));
}

#[test]
fn test_parse_clipping() {
    let stream = b"W\nW*";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], Operator::ClipNonZero));
    assert!(matches!(ops[1], Operator::ClipEvenOdd));
}

#[test]
fn test_parse_rectangle() {
    let stream = b"10 20 100 50 re";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Rectangle {
            x,
            y,
            width,
            height,
        } => {
            assert_eq!(*x, 10.0);
            assert_eq!(*y, 20.0);
            assert_eq!(*width, 100.0);
            assert_eq!(*height, 50.0);
        }
        _ => panic!("Expected Rectangle"),
    }
}

// ── Text state operators ────────────────────────────────────────

#[test]
fn test_parse_tr_render_mode() {
    let stream = b"1 Tr";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Operator::Tr { render: 1 }));
}

#[test]
fn test_parse_ts_text_rise() {
    let stream = b"5 Ts";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Operator::Ts { rise } if rise == 5.0));
}

#[test]
fn test_parse_double_quote_operator() {
    let stream = b"1.5 0.5 (Hello) \"";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::DoubleQuote {
            word_space,
            char_space,
            text,
        } => {
            assert!((word_space - 1.5).abs() < 0.001);
            assert!((char_space - 0.5).abs() < 0.001);
            assert_eq!(text, b"Hello");
        }
        _ => panic!("Expected DoubleQuote"),
    }
}

#[test]
fn test_parse_single_quote_operator() {
    let stream = b"(NextLine) '";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Quote { text } => {
            assert_eq!(text, b"NextLine");
        }
        _ => panic!("Expected Quote"),
    }
}

// ── Unknown / Other operators ───────────────────────────────────

#[test]
fn test_parse_unknown_operator() {
    let stream = b"42 XYZ";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Other { name, operands } => {
            assert_eq!(name, "XYZ");
            assert_eq!(operands.len(), 1);
        }
        _ => panic!("Expected Other operator"),
    }
}

#[test]
fn test_parse_unknown_operator_no_operands() {
    let stream = b"BX";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Other { name, operands } => {
            assert_eq!(name, "BX");
            assert_eq!(operands.len(), 0);
        }
        _ => panic!("Expected Other for BX"),
    }
}

#[test]
fn test_parse_compatibility_operators() {
    let stream = b"BX EX";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 2);
    assert!(matches!(&ops[0], Operator::Other { name, .. } if name == "BX"));
    assert!(matches!(&ops[1], Operator::Other { name, .. } if name == "EX"));
}

#[test]
fn test_parse_mp_dp_marked_point() {
    let stream = b"/Tag MP /Tag2 << /K 1 >> DP";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 2);
    assert!(matches!(&ops[0], Operator::Other { name, .. } if name == "MP"));
    assert!(matches!(&ops[1], Operator::Other { name, .. } if name == "DP"));
}

// ── parse_content_stream_images_only ────────────────────────────

#[test]
fn test_images_only_captures_do() {
    let stream = b"q 1 0 0 1 0 0 cm /Im1 Do Q";
    let ops = parse_content_stream_images_only(stream).unwrap();
    // Should capture q, cm, Do, Q
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::Do { ref name } if name == "Im1")));
    assert!(ops.iter().any(|op| matches!(op, Operator::SaveState)));
    assert!(ops.iter().any(|op| matches!(op, Operator::RestoreState)));
    assert!(ops.iter().any(|op| matches!(op, Operator::Cm { .. })));
}

#[test]
fn test_images_only_skips_text_blocks() {
    let stream = b"BT /F1 12 Tf (Hello) Tj ET /Im1 Do";
    let ops = parse_content_stream_images_only(stream).unwrap();
    // Should NOT contain any text operators (BT, Tf, Tj, ET)
    for op in &ops {
        assert!(!matches!(op, Operator::BeginText));
        assert!(!matches!(op, Operator::EndText));
        assert!(!matches!(op, Operator::Tf { .. }));
        assert!(!matches!(op, Operator::Tj { .. }));
    }
    // Should contain Do
    assert!(ops.iter().any(|op| matches!(op, Operator::Do { .. })));
}

#[test]
fn test_images_only_empty_stream() {
    let ops = parse_content_stream_images_only(b"").unwrap();
    assert!(ops.is_empty());
}

#[test]
fn test_images_only_pure_text_stream() {
    let stream = b"BT /F1 12 Tf 100 700 Td (Hello) Tj ET";
    let ops = parse_content_stream_images_only(stream).unwrap();
    // No image operators expected
    assert!(ops.is_empty() || !ops.iter().any(|op| matches!(op, Operator::Tj { .. })));
}

#[test]
fn test_images_only_inline_image() {
    let stream = b"q 1 0 0 1 0 0 cm BI /W 2 /H 2 ID AB EI Q";
    let ops = parse_content_stream_images_only(stream).unwrap();
    // Should capture the inline image
    assert!(ops
        .iter()
        .any(|op| matches!(op, Operator::InlineImage { .. })));
}

#[test]
fn test_images_only_multiple_text_blocks() {
    let stream = b"BT (A) Tj ET BT (B) Tj ET q /Im1 Do Q BT (C) Tj ET";
    let ops = parse_content_stream_images_only(stream).unwrap();
    // Should skip all text blocks, capture Do
    assert!(ops.iter().any(|op| matches!(op, Operator::Do { .. })));
    assert!(!ops.iter().any(|op| matches!(op, Operator::Tj { .. })));
}

// ── parse_content_stream_text_only edge cases ───────────────────

#[test]
fn test_text_only_preserves_text_state_ops_inside_bt() {
    let stream = b"BT 2 Tc 3 Tw 50 Tz 14 TL 1 Tr 5 Ts ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert_eq!(ops.len(), 8); // BT + 6 state ops + ET
    assert!(matches!(ops[0], Operator::BeginText));
    assert!(matches!(ops[1], Operator::Tc { .. }));
    assert!(matches!(ops[2], Operator::Tw { .. }));
    assert!(matches!(ops[3], Operator::Tz { .. }));
    assert!(matches!(ops[4], Operator::TL { .. }));
    assert!(matches!(ops[5], Operator::Tr { .. }));
    assert!(matches!(ops[6], Operator::Ts { .. }));
    assert!(matches!(ops[7], Operator::EndText));
}

#[test]
fn test_text_only_inline_image_outside_bt_skipped() {
    let stream = b"BI /W 2 /H 2 ID XY EI BT (Hi) Tj ET";
    let ops = parse_content_stream_text_only(stream).unwrap();
    // Inline image outside text block should be skipped in text-only mode
    assert!(!ops
        .iter()
        .any(|op| matches!(op, Operator::InlineImage { .. })));
    assert!(ops.iter().any(|op| matches!(op, Operator::Tj { .. })));
}

#[test]
fn test_text_only_cm_before_bt_preserved() {
    // cm before BT should be preserved (needed for CTM calculations)
    let stream = b"q 1 0 0 1 72 700 cm BT (Hello) Tj ET Q";
    let ops = parse_content_stream_text_only(stream).unwrap();
    assert!(ops.iter().any(|op| matches!(op, Operator::Cm { .. })));
    assert!(ops.iter().any(|op| matches!(op, Operator::Tj { .. })));
}
