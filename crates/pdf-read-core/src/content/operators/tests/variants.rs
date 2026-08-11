use super::*;

#[test]
fn test_operator_td_uppercase() {
    let op = Operator::TD { tx: 0.0, ty: -14.0 };
    match op {
        Operator::TD { tx, ty } => {
            assert_eq!(tx, 0.0);
            assert_eq!(ty, -14.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_tstar() {
    let op = Operator::TStar;
    assert!(matches!(op, Operator::TStar));
}

#[test]
fn test_operator_tj_array() {
    let op = Operator::TJ {
        array: vec![
            TextElement::String(b"He".to_vec()),
            TextElement::Offset(-120.0),
            TextElement::String(b"llo".to_vec()),
        ],
    };
    match op {
        Operator::TJ { array } => {
            assert_eq!(array.len(), 3);
            assert!(matches!(&array[0], TextElement::String(s) if s == b"He"));
            assert!(matches!(&array[1], TextElement::Offset(o) if *o == -120.0));
            assert!(matches!(&array[2], TextElement::String(s) if s == b"llo"));
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_quote() {
    let op = Operator::Quote {
        text: b"next line".to_vec(),
    };
    match op {
        Operator::Quote { text } => assert_eq!(text, b"next line"),
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_double_quote() {
    let op = Operator::DoubleQuote {
        word_space: 1.0,
        char_space: 2.0,
        text: b"quoted".to_vec(),
    };
    match op {
        Operator::DoubleQuote {
            word_space,
            char_space,
            text,
        } => {
            assert_eq!(word_space, 1.0);
            assert_eq!(char_space, 2.0);
            assert_eq!(text, b"quoted");
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_tc() {
    let op = Operator::Tc { char_space: 0.5 };
    assert!(matches!(op, Operator::Tc { char_space } if char_space == 0.5));
}

#[test]
fn test_operator_tw() {
    let op = Operator::Tw { word_space: 1.5 };
    assert!(matches!(op, Operator::Tw { word_space } if word_space == 1.5));
}

#[test]
fn test_operator_tz() {
    let op = Operator::Tz { scale: 150.0 };
    assert!(matches!(op, Operator::Tz { scale } if scale == 150.0));
}

#[test]
fn test_operator_tl() {
    let op = Operator::TL { leading: 14.0 };
    assert!(matches!(op, Operator::TL { leading } if leading == 14.0));
}

#[test]
fn test_operator_tr() {
    let op = Operator::Tr { render: 2 };
    assert!(matches!(op, Operator::Tr { render } if render == 2));
}

#[test]
fn test_operator_ts() {
    let op = Operator::Ts { rise: 5.0 };
    assert!(matches!(op, Operator::Ts { rise } if rise == 5.0));
}

#[test]
fn test_operator_cm() {
    let op = Operator::Cm {
        a: 2.0,
        b: 0.0,
        c: 0.0,
        d: 2.0,
        e: 10.0,
        f: 20.0,
    };
    match op {
        Operator::Cm { a, b, c, d, e, f } => {
            assert_eq!(a, 2.0);
            assert_eq!(b, 0.0);
            assert_eq!(c, 0.0);
            assert_eq!(d, 2.0);
            assert_eq!(e, 10.0);
            assert_eq!(f, 20.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_stroke_rgb() {
    let op = Operator::SetStrokeRgb {
        r: 0.0,
        g: 0.5,
        b: 1.0,
    };
    match op {
        Operator::SetStrokeRgb { r, g, b } => {
            assert_eq!(r, 0.0);
            assert_eq!(g, 0.5);
            assert_eq!(b, 1.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_fill_gray() {
    let op = Operator::SetFillGray { gray: 0.5 };
    assert!(matches!(op, Operator::SetFillGray { gray } if gray == 0.5));
}

#[test]
fn test_operator_stroke_gray() {
    let op = Operator::SetStrokeGray { gray: 0.0 };
    assert!(matches!(op, Operator::SetStrokeGray { gray } if gray == 0.0));
}

#[test]
fn test_operator_fill_cmyk() {
    let op = Operator::SetFillCmyk {
        c: 1.0,
        m: 0.0,
        y: 0.0,
        k: 0.0,
    };
    match op {
        Operator::SetFillCmyk { c, m, y, k } => {
            assert_eq!(c, 1.0);
            assert_eq!(m, 0.0);
            assert_eq!(y, 0.0);
            assert_eq!(k, 0.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_stroke_cmyk() {
    let op = Operator::SetStrokeCmyk {
        c: 0.0,
        m: 1.0,
        y: 0.0,
        k: 0.0,
    };
    match op {
        Operator::SetStrokeCmyk { c, m, y, k } => {
            assert_eq!(c, 0.0);
            assert_eq!(m, 1.0);
            assert_eq!(y, 0.0);
            assert_eq!(k, 0.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_fill_color_space() {
    let op = Operator::SetFillColorSpace {
        name: "DeviceRGB".to_string(),
    };
    match op {
        Operator::SetFillColorSpace { name } => assert_eq!(name, "DeviceRGB"),
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_stroke_color_space() {
    let op = Operator::SetStrokeColorSpace {
        name: "DeviceCMYK".to_string(),
    };
    match op {
        Operator::SetStrokeColorSpace { name } => assert_eq!(name, "DeviceCMYK"),
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_fill_color() {
    let op = Operator::SetFillColor {
        components: vec![0.1, 0.2, 0.3],
    };
    match op {
        Operator::SetFillColor { components } => {
            assert_eq!(components, vec![0.1, 0.2, 0.3]);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_stroke_color() {
    let op = Operator::SetStrokeColor {
        components: vec![0.5],
    };
    match op {
        Operator::SetStrokeColor { components } => {
            assert_eq!(components, vec![0.5]);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_fill_color_n_with_name() {
    let op = Operator::SetFillColorN {
        components: vec![0.1, 0.2, 0.3],
        name: Some(Box::new("Pattern1".to_string())),
    };
    match op {
        Operator::SetFillColorN { components, name } => {
            assert_eq!(components, vec![0.1, 0.2, 0.3]);
            assert_eq!(name, Some(Box::new("Pattern1".to_string())));
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_fill_color_n_without_name() {
    let op = Operator::SetFillColorN {
        components: vec![0.5],
        name: None,
    };
    match op {
        Operator::SetFillColorN { components, name } => {
            assert_eq!(components, vec![0.5]);
            assert!(name.is_none());
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_stroke_color_n() {
    let op = Operator::SetStrokeColorN {
        components: vec![],
        name: Some(Box::new("P1".to_string())),
    };
    match op {
        Operator::SetStrokeColorN { components, name } => {
            assert!(components.is_empty());
            assert_eq!(name, Some(Box::new("P1".to_string())));
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_begin_end_text() {
    assert!(matches!(Operator::BeginText, Operator::BeginText));
    assert!(matches!(Operator::EndText, Operator::EndText));
}

#[test]
fn test_operator_do() {
    let op = Operator::Do {
        name: "Im1".to_string(),
    };
    match op {
        Operator::Do { name } => assert_eq!(name, "Im1"),
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_moveto() {
    let op = Operator::MoveTo { x: 100.0, y: 200.0 };
    match op {
        Operator::MoveTo { x, y } => {
            assert_eq!(x, 100.0);
            assert_eq!(y, 200.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_lineto() {
    let op = Operator::LineTo { x: 300.0, y: 400.0 };
    match op {
        Operator::LineTo { x, y } => {
            assert_eq!(x, 300.0);
            assert_eq!(y, 400.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_curveto() {
    let op = Operator::CurveTo {
        x1: 1.0,
        y1: 2.0,
        x2: 3.0,
        y2: 4.0,
        x3: 5.0,
        y3: 6.0,
    };
    match op {
        Operator::CurveTo {
            x1,
            y1,
            x2,
            y2,
            x3,
            y3,
        } => {
            assert_eq!(x1, 1.0);
            assert_eq!(y1, 2.0);
            assert_eq!(x2, 3.0);
            assert_eq!(y2, 4.0);
            assert_eq!(x3, 5.0);
            assert_eq!(y3, 6.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_curveto_v() {
    let op = Operator::CurveToV {
        x2: 10.0,
        y2: 20.0,
        x3: 30.0,
        y3: 40.0,
    };
    match op {
        Operator::CurveToV { x2, y2, x3, y3 } => {
            assert_eq!(x2, 10.0);
            assert_eq!(y2, 20.0);
            assert_eq!(x3, 30.0);
            assert_eq!(y3, 40.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_curveto_y() {
    let op = Operator::CurveToY {
        x1: 10.0,
        y1: 20.0,
        x3: 30.0,
        y3: 40.0,
    };
    match op {
        Operator::CurveToY { x1, y1, x3, y3 } => {
            assert_eq!(x1, 10.0);
            assert_eq!(y1, 20.0);
            assert_eq!(x3, 30.0);
            assert_eq!(y3, 40.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_closepath() {
    assert!(matches!(Operator::ClosePath, Operator::ClosePath));
}

#[test]
fn test_operator_rectangle() {
    let op = Operator::Rectangle {
        x: 10.0,
        y: 20.0,
        width: 100.0,
        height: 50.0,
    };
    match op {
        Operator::Rectangle {
            x,
            y,
            width,
            height,
        } => {
            assert_eq!(x, 10.0);
            assert_eq!(y, 20.0);
            assert_eq!(width, 100.0);
            assert_eq!(height, 50.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_path_painting() {
    assert!(matches!(Operator::Stroke, Operator::Stroke));
    assert!(matches!(Operator::Fill, Operator::Fill));
    assert!(matches!(Operator::FillEvenOdd, Operator::FillEvenOdd));
    assert!(matches!(
        Operator::CloseFillStroke,
        Operator::CloseFillStroke
    ));
    assert!(matches!(Operator::EndPath, Operator::EndPath));
}

#[test]
fn test_operator_clipping() {
    assert!(matches!(Operator::ClipNonZero, Operator::ClipNonZero));
    assert!(matches!(Operator::ClipEvenOdd, Operator::ClipEvenOdd));
}

#[test]
fn test_operator_set_line_width() {
    let op = Operator::SetLineWidth { width: 2.5 };
    assert!(matches!(op, Operator::SetLineWidth { width } if width == 2.5));
}

#[test]
fn test_operator_set_dash() {
    let op = Operator::SetDash {
        array: vec![3.0, 2.0],
        phase: 0.0,
    };
    match op {
        Operator::SetDash { array, phase } => {
            assert_eq!(array, vec![3.0, 2.0]);
            assert_eq!(phase, 0.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_set_line_cap() {
    let op = Operator::SetLineCap { cap_style: 1 };
    assert!(matches!(op, Operator::SetLineCap { cap_style } if cap_style == 1));
}

#[test]
fn test_operator_set_line_join() {
    let op = Operator::SetLineJoin { join_style: 2 };
    assert!(matches!(op, Operator::SetLineJoin { join_style } if join_style == 2));
}

#[test]
fn test_operator_set_miter_limit() {
    let op = Operator::SetMiterLimit { limit: 10.0 };
    assert!(matches!(op, Operator::SetMiterLimit { limit } if limit == 10.0));
}

#[test]
fn test_operator_set_rendering_intent() {
    let op = Operator::SetRenderingIntent {
        intent: "Perceptual".to_string(),
    };
    match op {
        Operator::SetRenderingIntent { intent } => assert_eq!(intent, "Perceptual"),
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_set_flatness() {
    let op = Operator::SetFlatness { tolerance: 50.0 };
    assert!(matches!(op, Operator::SetFlatness { tolerance } if tolerance == 50.0));
}

#[test]
fn test_operator_set_ext_gstate() {
    let op = Operator::SetExtGState {
        dict_name: "GS1".to_string(),
    };
    match op {
        Operator::SetExtGState { dict_name } => assert_eq!(dict_name, "GS1"),
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_paint_shading() {
    let op = Operator::PaintShading {
        name: "Sh1".to_string(),
    };
    match op {
        Operator::PaintShading { name } => assert_eq!(name, "Sh1"),
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_inline_image() {
    let mut dict = std::collections::HashMap::new();
    dict.insert("W".to_string(), Object::Integer(100));
    dict.insert("H".to_string(), Object::Integer(50));
    let op = Operator::InlineImage {
        dict: Box::new(dict),
        data: vec![0xFF; 10],
    };
    match op {
        Operator::InlineImage { dict, data } => {
            assert_eq!(dict.len(), 2);
            assert_eq!(data.len(), 10);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_begin_marked_content() {
    let op = Operator::BeginMarkedContent {
        tag: "Span".to_string(),
    };
    match op {
        Operator::BeginMarkedContent { tag } => assert_eq!(tag, "Span"),
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_begin_marked_content_dict() {
    let op = Operator::BeginMarkedContentDict {
        tag: "P".to_string(),
        properties: Box::new(Object::Name("MCID0".to_string())),
    };
    match op {
        Operator::BeginMarkedContentDict { tag, properties } => {
            assert_eq!(tag, "P");
            assert!(matches!(*properties, Object::Name(ref n) if n == "MCID0"));
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_end_marked_content() {
    assert!(matches!(
        Operator::EndMarkedContent,
        Operator::EndMarkedContent
    ));
}

#[test]
fn test_operator_debug_format() {
    let op = Operator::Td { tx: 1.0, ty: 2.0 };
    let debug = format!("{:?}", op);
    assert!(debug.contains("Td"));
    assert!(debug.contains("1.0"));
    assert!(debug.contains("2.0"));
}

#[test]
fn test_operator_equality() {
    let op1 = Operator::SetFillGray { gray: 0.5 };
    let op2 = Operator::SetFillGray { gray: 0.5 };
    let op3 = Operator::SetFillGray { gray: 0.6 };
    assert_eq!(op1, op2);
    assert_ne!(op1, op3);
}

#[test]
fn test_text_element_debug_format() {
    let elem = TextElement::Offset(-50.0);
    let debug = format!("{:?}", elem);
    assert!(debug.contains("Offset"));
    assert!(debug.contains("-50.0"));
}

#[test]
fn test_text_element_inequality() {
    let s1 = TextElement::String(b"abc".to_vec());
    let s2 = TextElement::String(b"def".to_vec());
    let o1 = TextElement::Offset(10.0);
    assert_ne!(s1, s2);
    assert_ne!(s1, o1);
}
