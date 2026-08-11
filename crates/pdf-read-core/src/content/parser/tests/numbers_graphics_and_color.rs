use super::*;

// ── Number parsing edge cases ───────────────────────────────────

#[test]
fn test_parse_negative_numbers() {
    let stream = b"-100 -200 Td";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Td { tx, ty } => {
            assert_eq!(*tx, -100.0);
            assert_eq!(*ty, -200.0);
        }
        _ => panic!("Expected Td"),
    }
}

#[test]
fn test_parse_decimal_numbers() {
    let stream = b"0.001 99.999 Td";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Td { tx, ty } => {
            assert!((tx - 0.001).abs() < 0.0001);
            assert!((ty - 99.999).abs() < 0.01);
        }
        _ => panic!("Expected Td"),
    }
}

#[test]
fn test_parse_leading_dot_number() {
    let stream = b".5 .25 Td";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Td { tx, ty } => {
            assert!((tx - 0.5).abs() < 0.001);
            assert!((ty - 0.25).abs() < 0.001);
        }
        _ => panic!("Expected Td"),
    }
}

#[test]
fn test_parse_large_numbers() {
    let stream = b"99999 88888 Td";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Td { tx, ty } => {
            assert_eq!(*tx, 99999.0);
            assert_eq!(*ty, 88888.0);
        }
        _ => panic!("Expected Td"),
    }
}

#[test]
fn test_parse_zero() {
    let stream = b"0 0 Td";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Td { tx, ty } => {
            assert_eq!(*tx, 0.0);
            assert_eq!(*ty, 0.0);
        }
        _ => panic!("Expected Td"),
    }
}

// ── String parsing edge cases ───────────────────────────────────

#[test]
fn test_parse_string_with_nested_parens() {
    let stream = b"(Hello (World)) Tj";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Tj { text } => {
            assert_eq!(text, b"Hello (World)");
        }
        _ => panic!("Expected Tj"),
    }
}

#[test]
fn test_parse_string_with_escape_sequences() {
    let stream = b"(Line1\\nLine2\\r\\t) Tj";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Tj { text } => {
            // The PDF parser should handle escape sequences
            assert!(!text.is_empty());
        }
        _ => panic!("Expected Tj"),
    }
}

#[test]
fn test_parse_empty_string() {
    let stream = b"() Tj";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Tj { text } => {
            assert!(text.is_empty());
        }
        _ => panic!("Expected Tj"),
    }
}

#[test]
fn test_parse_hex_string() {
    let stream = b"<48656C6C6F> Tj";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Tj { text } => {
            assert_eq!(text, b"Hello");
        }
        _ => panic!("Expected Tj"),
    }
}

#[test]
fn test_parse_hex_string_odd_digits() {
    // Odd number of hex digits: trailing nibble should be padded with 0
    let stream = b"<ABC> Tj";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Tj { text } => {
            assert_eq!(text.len(), 2);
            assert_eq!(text[0], 0xAB);
            assert_eq!(text[1], 0xC0);
        }
        _ => panic!("Expected Tj"),
    }
}

#[test]
fn test_parse_empty_hex_string() {
    let stream = b"<> Tj";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::Tj { text } => {
            assert!(text.is_empty());
        }
        _ => panic!("Expected Tj"),
    }
}

// ── Graphics state operators ────────────────────────────────────

#[test]
fn test_parse_line_width() {
    let stream = b"2.5 w";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Operator::SetLineWidth { width } if (width - 2.5).abs() < 0.001));
}

#[test]
fn test_parse_line_cap() {
    let stream = b"1 J";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Operator::SetLineCap { cap_style: 1 }));
}

#[test]
fn test_parse_line_join() {
    let stream = b"2 j";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Operator::SetLineJoin { join_style: 2 }));
}

#[test]
fn test_parse_miter_limit() {
    let stream = b"10 M";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Operator::SetMiterLimit { limit } if limit == 10.0));
}

#[test]
fn test_parse_dash_pattern() {
    let stream = b"[3 2] 0 d";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::SetDash { array, phase } => {
            assert_eq!(array, &[3.0, 2.0]);
            assert_eq!(*phase, 0.0);
        }
        _ => panic!("Expected SetDash"),
    }
}

#[test]
fn test_parse_rendering_intent() {
    let stream = b"/AbsoluteColorimetric ri";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::SetRenderingIntent { intent } => {
            assert_eq!(intent, "AbsoluteColorimetric");
        }
        _ => panic!("Expected SetRenderingIntent"),
    }
}

#[test]
fn test_parse_flatness() {
    let stream = b"50 i";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Operator::SetFlatness { tolerance } if tolerance == 50.0));
}

#[test]
fn test_parse_ext_gstate() {
    let stream = b"/GS0 gs";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::SetExtGState { dict_name } => {
            assert_eq!(dict_name, "GS0");
        }
        _ => panic!("Expected SetExtGState"),
    }
}

#[test]
fn test_parse_paint_shading() {
    let stream = b"/Sh0 sh";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::PaintShading { name } => {
            assert_eq!(name, "Sh0");
        }
        _ => panic!("Expected PaintShading"),
    }
}

// ── Color operators ─────────────────────────────────────────────

#[test]
fn test_parse_gray_color() {
    let stream = b"0.5 g 0.8 G";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], Operator::SetFillGray { gray } if (gray - 0.5).abs() < 0.001));
    assert!(matches!(ops[1], Operator::SetStrokeGray { gray } if (gray - 0.8).abs() < 0.001));
}

#[test]
fn test_parse_cmyk_color() {
    let stream = b"0.1 0.2 0.3 0.4 k\n0.5 0.6 0.7 0.8 K";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 2);
    match &ops[0] {
        Operator::SetFillCmyk { c, m, y, k } => {
            assert!((c - 0.1).abs() < 0.01);
            assert!((m - 0.2).abs() < 0.01);
            assert!((y - 0.3).abs() < 0.01);
            assert!((k - 0.4).abs() < 0.01);
        }
        _ => panic!("Expected SetFillCmyk"),
    }
    match &ops[1] {
        Operator::SetStrokeCmyk { c, m, y, k } => {
            assert!((c - 0.5).abs() < 0.01);
            assert!((m - 0.6).abs() < 0.01);
            assert!((y - 0.7).abs() < 0.01);
            assert!((k - 0.8).abs() < 0.01);
        }
        _ => panic!("Expected SetStrokeCmyk"),
    }
}

#[test]
fn test_parse_color_space_operators() {
    let stream = b"/DeviceRGB cs /DeviceCMYK CS";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 2);
    match &ops[0] {
        Operator::SetFillColorSpace { name } => assert_eq!(name, "DeviceRGB"),
        _ => panic!("Expected SetFillColorSpace"),
    }
    match &ops[1] {
        Operator::SetStrokeColorSpace { name } => assert_eq!(name, "DeviceCMYK"),
        _ => panic!("Expected SetStrokeColorSpace"),
    }
}

#[test]
fn test_parse_sc_color_components() {
    let stream = b"0.1 0.2 0.3 sc";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::SetFillColor { components } => {
            assert_eq!(components.len(), 3);
            assert!((components[0] - 0.1).abs() < 0.01);
            assert!((components[1] - 0.2).abs() < 0.01);
            assert!((components[2] - 0.3).abs() < 0.01);
        }
        _ => panic!("Expected SetFillColor"),
    }
}

#[test]
fn test_parse_sc_stroke_color_components() {
    let stream = b"0.5 0.6 SC";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::SetStrokeColor { components } => {
            assert_eq!(components.len(), 2);
        }
        _ => panic!("Expected SetStrokeColor"),
    }
}

#[test]
fn test_parse_scn_with_pattern_name() {
    let stream = b"0.5 /Pattern1 scn";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::SetFillColorN { components, name } => {
            assert_eq!(components.len(), 1);
            assert!(name.is_some());
            assert_eq!(**name.as_ref().unwrap(), "Pattern1");
        }
        _ => panic!("Expected SetFillColorN"),
    }
}

#[test]
fn test_parse_scn_without_pattern_name() {
    let stream = b"0.1 0.2 0.3 scn";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::SetFillColorN { components, name } => {
            assert_eq!(components.len(), 3);
            assert!(name.is_none());
        }
        _ => panic!("Expected SetFillColorN"),
    }
}

#[test]
fn test_parse_scn_stroke_with_pattern() {
    let stream = b"0.5 /P1 SCN";
    let ops = parse_content_stream(stream).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        Operator::SetStrokeColorN { components, name } => {
            assert_eq!(components.len(), 1);
            assert!(name.is_some());
        }
        _ => panic!("Expected SetStrokeColorN"),
    }
}
