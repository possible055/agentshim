use super::*;

#[test]
fn test_operator_td() {
    let op = Operator::Td { tx: 10.0, ty: 20.0 };
    match op {
        Operator::Td { tx, ty } => {
            assert_eq!(tx, 10.0);
            assert_eq!(ty, 20.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_tm() {
    let op = Operator::Tm {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 100.0,
        f: 200.0,
    };
    match op {
        Operator::Tm { a, b, c, d, e, f } => {
            assert_eq!(a, 1.0);
            assert_eq!(b, 0.0);
            assert_eq!(c, 0.0);
            assert_eq!(d, 1.0);
            assert_eq!(e, 100.0);
            assert_eq!(f, 200.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_tj() {
    let op = Operator::Tj {
        text: b"Hello".to_vec(),
    };
    match op {
        Operator::Tj { text } => {
            assert_eq!(text, b"Hello");
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_tf() {
    let op = Operator::Tf {
        font: "F1".to_string(),
        size: 12.0,
    };
    match op {
        Operator::Tf { font, size } => {
            assert_eq!(font, "F1");
            assert_eq!(size, 12.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_rgb() {
    let op = Operator::SetFillRgb {
        r: 1.0,
        g: 0.0,
        b: 0.0,
    };
    match op {
        Operator::SetFillRgb { r, g, b } => {
            assert_eq!(r, 1.0);
            assert_eq!(g, 0.0);
            assert_eq!(b, 0.0);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_text_element_string() {
    let elem = TextElement::String(b"Text".to_vec());
    match elem {
        TextElement::String(s) => {
            assert_eq!(s, b"Text");
        }
        _ => panic!("Wrong element type"),
    }
}

#[test]
fn test_text_element_offset() {
    let elem = TextElement::Offset(-100.0);
    match elem {
        TextElement::Offset(offset) => {
            assert_eq!(offset, -100.0);
        }
        _ => panic!("Wrong element type"),
    }
}

#[test]
fn test_operator_clone() {
    let op1 = Operator::Tj {
        text: b"Test".to_vec(),
    };
    let op2 = op1.clone();
    assert_eq!(op1, op2);
}

#[test]
fn test_operator_save_restore() {
    let save = Operator::SaveState;
    let restore = Operator::RestoreState;
    assert!(matches!(save, Operator::SaveState));
    assert!(matches!(restore, Operator::RestoreState));
}

#[test]
fn test_operator_other() {
    let op = Operator::Other {
        name: "Do".to_string(),
        operands: Box::new(vec![Object::Name("Im1".to_string())]),
    };
    match op {
        Operator::Other { name, operands } => {
            assert_eq!(name, "Do");
            assert_eq!(operands.len(), 1);
        }
        _ => panic!("Wrong operator type"),
    }
}

#[test]
fn test_operator_enum_size() {
    let size = std::mem::size_of::<Operator>();
    eprintln!("Operator enum size: {} bytes", size);
    // After boxing BeginMarkedContentDict.properties, InlineImage.dict,
    // Other.operands, SetFillColorN/SetStrokeColorN.name:
    // largest variant is now SetFillColorN/SetStrokeColorN at Vec<f32>(24) + Option<Box<String>>(8) = 32 bytes
    // Enum: 32 (payload) + 8 (discriminant + alignment) = 40 bytes (was 112)
    assert!(
        size <= 40,
        "Operator enum too large: {} bytes (expected <= 40)",
        size
    );
}

#[test]
fn test_text_element_eq() {
    let elem1 = TextElement::String(b"Test".to_vec());
    let elem2 = TextElement::String(b"Test".to_vec());
    assert_eq!(elem1, elem2);

    let elem3 = TextElement::Offset(10.0);
    let elem4 = TextElement::Offset(10.0);
    assert_eq!(elem3, elem4);
}

// =========================================================================
// validate_operands_for_raw_operator tests
// =========================================================================

#[test]
fn test_validate_moveto_valid() {
    let operands = vec![Object::Integer(10), Object::Integer(20)];
    assert!(Operator::validate_operands_for_raw_operator("m", &operands).is_ok());
}

#[test]
fn test_validate_moveto_wrong_count() {
    let operands = vec![Object::Integer(10)];
    let err = Operator::validate_operands_for_raw_operator("m", &operands);
    assert!(err.is_err());
    let msg = format!("{}", err.unwrap_err());
    assert!(msg.contains("moveto"));
    assert!(msg.contains("2 operands"));
}

#[test]
fn test_validate_lineto_valid() {
    let operands = vec![Object::Real(1.5), Object::Real(2.5)];
    assert!(Operator::validate_operands_for_raw_operator("l", &operands).is_ok());
}

#[test]
fn test_validate_lineto_wrong_count() {
    let operands = vec![Object::Integer(1), Object::Integer(2), Object::Integer(3)];
    let err = Operator::validate_operands_for_raw_operator("l", &operands);
    assert!(err.is_err());
}

#[test]
fn test_validate_curveto_valid() {
    let operands = vec![
        Object::Integer(1),
        Object::Integer(2),
        Object::Integer(3),
        Object::Integer(4),
        Object::Integer(5),
        Object::Integer(6),
    ];
    assert!(Operator::validate_operands_for_raw_operator("c", &operands).is_ok());
}

#[test]
fn test_validate_curveto_wrong_count() {
    let operands = vec![Object::Integer(1), Object::Integer(2)];
    assert!(Operator::validate_operands_for_raw_operator("c", &operands).is_err());
}

#[test]
fn test_validate_curveto_v_valid() {
    let operands = vec![
        Object::Integer(1),
        Object::Integer(2),
        Object::Integer(3),
        Object::Integer(4),
    ];
    assert!(Operator::validate_operands_for_raw_operator("v", &operands).is_ok());
}

#[test]
fn test_validate_curveto_v_wrong_count() {
    let operands = vec![Object::Integer(1)];
    assert!(Operator::validate_operands_for_raw_operator("v", &operands).is_err());
}

#[test]
fn test_validate_curveto_y_valid() {
    let operands = vec![
        Object::Integer(1),
        Object::Integer(2),
        Object::Integer(3),
        Object::Integer(4),
    ];
    assert!(Operator::validate_operands_for_raw_operator("y", &operands).is_ok());
}

#[test]
fn test_validate_curveto_y_wrong_count() {
    let operands = vec![];
    assert!(Operator::validate_operands_for_raw_operator("y", &operands).is_err());
}

#[test]
fn test_validate_closepath_valid() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("h", &operands).is_ok());
}

#[test]
fn test_validate_closepath_wrong_count() {
    let operands = vec![Object::Integer(1)];
    assert!(Operator::validate_operands_for_raw_operator("h", &operands).is_err());
}

#[test]
fn test_validate_rectangle_valid() {
    let operands = vec![
        Object::Integer(0),
        Object::Integer(0),
        Object::Integer(100),
        Object::Integer(200),
    ];
    assert!(Operator::validate_operands_for_raw_operator("re", &operands).is_ok());
}

#[test]
fn test_validate_rectangle_wrong_count() {
    let operands = vec![Object::Integer(0), Object::Integer(0)];
    assert!(Operator::validate_operands_for_raw_operator("re", &operands).is_err());
}

#[test]
fn test_validate_td_valid() {
    let operands = vec![Object::Real(10.0), Object::Real(20.0)];
    assert!(Operator::validate_operands_for_raw_operator("Td", &operands).is_ok());
}

#[test]
fn test_validate_td_wrong_count() {
    let operands = vec![Object::Real(10.0)];
    assert!(Operator::validate_operands_for_raw_operator("Td", &operands).is_err());
}

#[test]
fn test_validate_td_uppercase_valid() {
    let operands = vec![Object::Integer(5), Object::Integer(10)];
    assert!(Operator::validate_operands_for_raw_operator("TD", &operands).is_ok());
}

#[test]
fn test_validate_td_uppercase_wrong_count() {
    let operands = vec![];
    assert!(Operator::validate_operands_for_raw_operator("TD", &operands).is_err());
}

#[test]
fn test_validate_tm_valid() {
    let operands = vec![
        Object::Real(1.0),
        Object::Real(0.0),
        Object::Real(0.0),
        Object::Real(1.0),
        Object::Real(72.0),
        Object::Real(720.0),
    ];
    assert!(Operator::validate_operands_for_raw_operator("Tm", &operands).is_ok());
}

#[test]
fn test_validate_tm_wrong_count() {
    let operands = vec![Object::Real(1.0)];
    assert!(Operator::validate_operands_for_raw_operator("Tm", &operands).is_err());
}

#[test]
fn test_validate_tstar_valid() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("T*", &operands).is_ok());
}

#[test]
fn test_validate_tstar_wrong_count() {
    let operands = vec![Object::Integer(1)];
    assert!(Operator::validate_operands_for_raw_operator("T*", &operands).is_err());
}

#[test]
fn test_validate_tj_valid() {
    let operands = vec![Object::String(b"Hello".to_vec())];
    assert!(Operator::validate_operands_for_raw_operator("Tj", &operands).is_ok());
}

#[test]
fn test_validate_tj_wrong_count() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("Tj", &operands).is_err());
}

#[test]
fn test_validate_tj_array_valid() {
    let operands = vec![Object::Array(vec![
        Object::String(b"He".to_vec()),
        Object::Integer(-120),
        Object::String(b"llo".to_vec()),
    ])];
    assert!(Operator::validate_operands_for_raw_operator("TJ", &operands).is_ok());
}

#[test]
fn test_validate_tj_array_wrong_count() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("TJ", &operands).is_err());
}

#[test]
fn test_validate_quote_valid() {
    let operands = vec![Object::String(b"text".to_vec())];
    assert!(Operator::validate_operands_for_raw_operator("'", &operands).is_ok());
}

#[test]
fn test_validate_quote_wrong_count() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("'", &operands).is_err());
}

#[test]
fn test_validate_double_quote_valid() {
    let operands = vec![
        Object::Real(1.0),
        Object::Real(2.0),
        Object::String(b"text".to_vec()),
    ];
    assert!(Operator::validate_operands_for_raw_operator("\"", &operands).is_ok());
}

#[test]
fn test_validate_double_quote_wrong_count() {
    let operands = vec![Object::Real(1.0)];
    assert!(Operator::validate_operands_for_raw_operator("\"", &operands).is_err());
}

#[test]
fn test_validate_tc_valid() {
    let operands = vec![Object::Real(0.5)];
    assert!(Operator::validate_operands_for_raw_operator("Tc", &operands).is_ok());
}

#[test]
fn test_validate_tc_wrong_count() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("Tc", &operands).is_err());
}

#[test]
fn test_validate_tw_valid() {
    let operands = vec![Object::Real(1.0)];
    assert!(Operator::validate_operands_for_raw_operator("Tw", &operands).is_ok());
}

#[test]
fn test_validate_tw_wrong_count() {
    let operands = vec![Object::Real(1.0), Object::Real(2.0)];
    assert!(Operator::validate_operands_for_raw_operator("Tw", &operands).is_err());
}

#[test]
fn test_validate_tz_valid() {
    let operands = vec![Object::Integer(150)];
    assert!(Operator::validate_operands_for_raw_operator("Tz", &operands).is_ok());
}

#[test]
fn test_validate_tz_wrong_count() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("Tz", &operands).is_err());
}

#[test]
fn test_validate_tl_valid() {
    let operands = vec![Object::Real(14.0)];
    assert!(Operator::validate_operands_for_raw_operator("TL", &operands).is_ok());
}

#[test]
fn test_validate_tl_wrong_count() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("TL", &operands).is_err());
}

#[test]
fn test_validate_tf_valid() {
    let operands = vec![Object::Name("F1".to_string()), Object::Real(12.0)];
    assert!(Operator::validate_operands_for_raw_operator("Tf", &operands).is_ok());
}

#[test]
fn test_validate_tf_wrong_count() {
    let operands = vec![Object::Name("F1".to_string())];
    assert!(Operator::validate_operands_for_raw_operator("Tf", &operands).is_err());
}

#[test]
fn test_validate_tr_valid() {
    let operands = vec![Object::Integer(0)];
    assert!(Operator::validate_operands_for_raw_operator("Tr", &operands).is_ok());
}

#[test]
fn test_validate_tr_wrong_count() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("Tr", &operands).is_err());
}

#[test]
fn test_validate_ts_valid() {
    let operands = vec![Object::Real(5.0)];
    assert!(Operator::validate_operands_for_raw_operator("Ts", &operands).is_ok());
}

#[test]
fn test_validate_ts_wrong_count() {
    let operands = vec![Object::Real(1.0), Object::Real(2.0)];
    assert!(Operator::validate_operands_for_raw_operator("Ts", &operands).is_err());
}

#[test]
fn test_validate_save_restore_state_valid() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("q", &operands).is_ok());
    assert!(Operator::validate_operands_for_raw_operator("Q", &operands).is_ok());
}

#[test]
fn test_validate_save_state_wrong_count() {
    let operands = vec![Object::Integer(1)];
    assert!(Operator::validate_operands_for_raw_operator("q", &operands).is_err());
}

#[test]
fn test_validate_restore_state_wrong_count() {
    let operands = vec![Object::Integer(1)];
    assert!(Operator::validate_operands_for_raw_operator("Q", &operands).is_err());
}

#[test]
fn test_validate_cm_valid() {
    let operands = vec![
        Object::Real(1.0),
        Object::Real(0.0),
        Object::Real(0.0),
        Object::Real(1.0),
        Object::Real(0.0),
        Object::Real(0.0),
    ];
    assert!(Operator::validate_operands_for_raw_operator("cm", &operands).is_ok());
}

#[test]
fn test_validate_cm_wrong_count() {
    let operands = vec![Object::Real(1.0)];
    assert!(Operator::validate_operands_for_raw_operator("cm", &operands).is_err());
}

#[test]
fn test_validate_rg_valid() {
    let operands = vec![Object::Real(1.0), Object::Real(0.0), Object::Real(0.0)];
    assert!(Operator::validate_operands_for_raw_operator("rg", &operands).is_ok());
}

#[test]
fn test_validate_rg_wrong_count() {
    let operands = vec![Object::Real(1.0)];
    assert!(Operator::validate_operands_for_raw_operator("rg", &operands).is_err());
}

#[test]
fn test_validate_rg_uppercase_valid() {
    let operands = vec![Object::Real(0.0), Object::Real(1.0), Object::Real(0.0)];
    assert!(Operator::validate_operands_for_raw_operator("RG", &operands).is_ok());
}

#[test]
fn test_validate_rg_uppercase_wrong_count() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("RG", &operands).is_err());
}

#[test]
fn test_validate_g_valid() {
    let operands = vec![Object::Real(0.5)];
    assert!(Operator::validate_operands_for_raw_operator("g", &operands).is_ok());
}

#[test]
fn test_validate_g_wrong_count() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("g", &operands).is_err());
}

#[test]
fn test_validate_g_uppercase_valid() {
    let operands = vec![Object::Real(0.5)];
    assert!(Operator::validate_operands_for_raw_operator("G", &operands).is_ok());
}

#[test]
fn test_validate_g_uppercase_wrong_count() {
    let operands = vec![Object::Real(0.5), Object::Real(0.5)];
    assert!(Operator::validate_operands_for_raw_operator("G", &operands).is_err());
}

#[test]
fn test_validate_k_valid() {
    let operands = vec![
        Object::Real(0.0),
        Object::Real(0.0),
        Object::Real(0.0),
        Object::Real(1.0),
    ];
    assert!(Operator::validate_operands_for_raw_operator("k", &operands).is_ok());
}

#[test]
fn test_validate_k_wrong_count() {
    let operands = vec![Object::Real(0.0)];
    assert!(Operator::validate_operands_for_raw_operator("k", &operands).is_err());
}

#[test]
fn test_validate_k_uppercase_valid() {
    let operands = vec![
        Object::Real(0.0),
        Object::Real(1.0),
        Object::Real(0.0),
        Object::Real(0.0),
    ];
    assert!(Operator::validate_operands_for_raw_operator("K", &operands).is_ok());
}

#[test]
fn test_validate_k_uppercase_wrong_count() {
    let operands = vec![Object::Real(0.0), Object::Real(1.0)];
    assert!(Operator::validate_operands_for_raw_operator("K", &operands).is_err());
}

#[test]
fn test_validate_bt_et_valid() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("BT", &operands).is_ok());
    assert!(Operator::validate_operands_for_raw_operator("ET", &operands).is_ok());
}

#[test]
fn test_validate_bt_wrong_count() {
    let operands = vec![Object::Integer(1)];
    assert!(Operator::validate_operands_for_raw_operator("BT", &operands).is_err());
}

#[test]
fn test_validate_et_wrong_count() {
    let operands = vec![Object::Integer(1)];
    assert!(Operator::validate_operands_for_raw_operator("ET", &operands).is_err());
}

#[test]
fn test_validate_do_valid() {
    let operands = vec![Object::Name("Im1".to_string())];
    assert!(Operator::validate_operands_for_raw_operator("Do", &operands).is_ok());
}

#[test]
fn test_validate_do_wrong_count() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("Do", &operands).is_err());
}

#[test]
fn test_validate_unknown_operator_passes() {
    // Unknown operators should not produce errors (lenient behavior)
    let operands = vec![Object::Integer(1), Object::Integer(2), Object::Integer(3)];
    assert!(Operator::validate_operands_for_raw_operator("xyz_unknown", &operands).is_ok());
}

#[test]
fn test_validate_unknown_operator_empty_operands() {
    let operands: Vec<Object> = vec![];
    assert!(Operator::validate_operands_for_raw_operator("BMC", &operands).is_ok());
}

// =========================================================================
// Additional Operator variant construction tests
// =========================================================================
