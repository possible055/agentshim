use super::*;

#[test]
fn test_set_stroke_color_separation() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "Separation".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![0.6],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    // gray = 1.0 - 0.6 = 0.4
    assert!((state.stroke_color_rgb.0 - 0.4).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_devicen_cmyk() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "DeviceN".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![0.1, 0.2, 0.3, 0.4],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!(state.stroke_color_cmyk.is_some());
}

#[test]
fn test_set_stroke_color_devicen_single() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "DeviceN".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![0.5],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.5).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_cal_rgb() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "CalRGB".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![0.5, 0.6, 0.7],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.5).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_cal_gray() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "CalGray".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![0.9],
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.9).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_unknown() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "UnknownCS".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColor {
            components: vec![0.5],
        })
        .unwrap();
    // Should not panic
}

// ========================================================================
// COVERAGE TESTS: SetFillColorN / SetStrokeColorN
// ========================================================================

#[test]
fn test_set_fill_color_n_with_pattern() {
    let mut extractor = TextExtractor::new();
    // Pattern color space with name
    extractor
        .execute_operator_public(Operator::SetFillColorN {
            components: vec![],
            name: Some(Box::new("P1".to_string())),
        })
        .unwrap();
    // Should not panic (pattern ignored)
}

#[test]
fn test_set_fill_color_n_without_pattern_gray() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "DeviceGray".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColorN {
            components: vec![0.3],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.3).abs() < 0.01);
}

#[test]
fn test_set_fill_color_n_without_pattern_rgb() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "DeviceRGB".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColorN {
            components: vec![0.1, 0.2, 0.3],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.1).abs() < 0.01);
}

#[test]
fn test_set_fill_color_n_lab() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "Lab".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColorN {
            components: vec![80.0, 0.0, 0.0],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.8).abs() < 0.01);
}

#[test]
fn test_set_fill_color_n_cmyk() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "DeviceCMYK".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColorN {
            components: vec![0.0, 0.0, 0.0, 0.0],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    // White (no ink)
    assert!((state.fill_color_rgb.0 - 1.0).abs() < 0.01);
}

#[test]
fn test_set_fill_color_n_iccbased() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "ICCBased".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColorN {
            components: vec![0.5, 0.6, 0.7],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.5).abs() < 0.01);
}

#[test]
fn test_set_fill_color_n_iccbased_gray() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "ICCBased".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColorN {
            components: vec![0.9],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.9).abs() < 0.01);
}

#[test]
fn test_set_fill_color_n_iccbased_cmyk() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "ICCBased".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColorN {
            components: vec![0.1, 0.2, 0.3, 0.4],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!(state.fill_color_cmyk.is_some());
}

#[test]
fn test_set_fill_color_n_separation() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "Separation".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColorN {
            components: vec![0.4],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.6).abs() < 0.01);
}

#[test]
fn test_set_fill_color_n_devicen() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "DeviceN".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColorN {
            components: vec![0.1, 0.2, 0.3, 0.4],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!(state.fill_color_cmyk.is_some());
}

#[test]
fn test_set_fill_color_n_devicen_single() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetFillColorSpace {
            name: "DeviceN".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetFillColorN {
            components: vec![0.2],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.8).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_n_with_pattern() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorN {
            components: vec![],
            name: Some(Box::new("P2".to_string())),
        })
        .unwrap();
    // Should not panic
}

#[test]
fn test_set_stroke_color_n_gray() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "DeviceGray".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColorN {
            components: vec![0.6],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.6).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_n_rgb() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "DeviceRGB".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColorN {
            components: vec![0.8, 0.7, 0.6],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.8).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_n_lab() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "Lab".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColorN {
            components: vec![60.0, 0.0, 0.0],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.6).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_n_cmyk() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "DeviceCMYK".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColorN {
            components: vec![0.0, 0.0, 1.0, 0.0], // yellow
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!(state.stroke_color_cmyk.is_some());
}

#[test]
fn test_set_stroke_color_n_iccbased_rgb() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "ICCBased".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColorN {
            components: vec![0.2, 0.3, 0.4],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.2).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_n_iccbased_gray() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "ICCBased".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColorN {
            components: vec![0.5],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.5).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_n_iccbased_cmyk() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "ICCBased".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColorN {
            components: vec![0.1, 0.2, 0.3, 0.4],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!(state.stroke_color_cmyk.is_some());
}

#[test]
fn test_set_stroke_color_n_separation() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "Separation".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColorN {
            components: vec![1.0],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.0).abs() < 0.01);
}

#[test]
fn test_set_stroke_color_n_devicen_cmyk() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "DeviceN".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColorN {
            components: vec![0.5, 0.5, 0.5, 0.5],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!(state.stroke_color_cmyk.is_some());
}

#[test]
fn test_set_stroke_color_n_devicen_single() {
    let mut extractor = TextExtractor::new();
    extractor
        .execute_operator_public(Operator::SetStrokeColorSpace {
            name: "DeviceN".to_string(),
        })
        .unwrap();
    extractor
        .execute_operator_public(Operator::SetStrokeColorN {
            components: vec![0.1],
            name: None,
        })
        .unwrap();

    let state = extractor.state_stack.current();
    assert!((state.stroke_color_rgb.0 - 0.9).abs() < 0.01);
}

// ========================================================================
// REGRESSION: named / unknown color space references
// ========================================================================

/// Named color space reference like "Cs1" should fall back by component
/// count rather than emitting a warn! (regression: warn spam on PDFs
/// with ICCBased color spaces registered under user-defined names).
#[test]
fn test_named_fill_color_space_fallback_gray() {
    let mut e = TextExtractor::new();
    e.execute_operator_public(Operator::SetFillColorSpace {
        name: "Cs1".to_string(),
    })
    .unwrap();
    e.execute_operator_public(Operator::SetFillColor {
        components: vec![0.4],
    })
    .unwrap();
    let state = e.state_stack.current();
    let (r, g, b) = state.fill_color_rgb;
    assert!((r - 0.4).abs() < 0.01 && (g - 0.4).abs() < 0.01 && (b - 0.4).abs() < 0.01);
}

#[test]
fn test_named_fill_color_space_fallback_rgb() {
    let mut e = TextExtractor::new();
    e.execute_operator_public(Operator::SetFillColorSpace {
        name: "Cs2".to_string(),
    })
    .unwrap();
    e.execute_operator_public(Operator::SetFillColor {
        components: vec![0.1, 0.2, 0.3],
    })
    .unwrap();
    let state = e.state_stack.current();
    assert!((state.fill_color_rgb.0 - 0.1).abs() < 0.01);
    assert!((state.fill_color_rgb.1 - 0.2).abs() < 0.01);
    assert!((state.fill_color_rgb.2 - 0.3).abs() < 0.01);
}

#[test]
fn test_advance_position_with_font() {
    let mut extractor = TextExtractor::new();
    let font = create_test_font();
    extractor.add_font("F1".to_string(), font);
    extractor.cached_current_font = extractor.fonts.get("F1").cloned();
    extractor.state_stack.current_mut().font_size = 12.0;
    extractor.state_stack.current_mut().font_name = Some("F1".to_string());
    extractor.state_stack.current_mut().horizontal_scaling = 100.0;

    let width = extractor.advance_position_for_string(b"Hi").unwrap();
    assert!(width > 0.0, "Width should be positive with font");
}

#[test]
fn test_advance_position_with_word_space() {
    let mut extractor = TextExtractor::new();
    extractor.state_stack.current_mut().font_size = 12.0;
    extractor.state_stack.current_mut().horizontal_scaling = 100.0;
    extractor.state_stack.current_mut().word_space = 5.0;

    let width = extractor.advance_position_for_string(b"A B").unwrap();
    assert!(width > 0.0);
}
