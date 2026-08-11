use super::*;

#[test]
fn test_file_attachment_icon_all_variants() {
    let icons = [
        (FileAttachmentIcon::GraphPushPin, "GraphPushPin"),
        (FileAttachmentIcon::PaperclipTag, "PaperclipTag"),
        (FileAttachmentIcon::PushPin, "PushPin"),
    ];
    for (icon, name) in &icons {
        assert_eq!(icon.pdf_name(), *name);
        assert_eq!(FileAttachmentIcon::from_pdf_name(name), *icon);
    }
}

#[test]
fn test_file_attachment_icon_unknown_defaults_to_paperclip() {
    assert_eq!(
        FileAttachmentIcon::from_pdf_name("X"),
        FileAttachmentIcon::PaperclipTag
    );
}

#[test]
fn test_file_attachment_icon_default() {
    let default: FileAttachmentIcon = Default::default();
    assert_eq!(default, FileAttachmentIcon::PaperclipTag);
}

// =========================================================================
// ReplyType tests
// =========================================================================

#[test]
fn test_reply_type_all_variants() {
    assert_eq!(ReplyType::Reply.pdf_name(), "R");
    assert_eq!(ReplyType::Group.pdf_name(), "Group");

    assert_eq!(ReplyType::from_pdf_name("R"), ReplyType::Reply);
    assert_eq!(ReplyType::from_pdf_name("Group"), ReplyType::Group);
}

#[test]
fn test_reply_type_unknown_defaults_to_reply() {
    assert_eq!(ReplyType::from_pdf_name("X"), ReplyType::Reply);
    assert_eq!(ReplyType::from_pdf_name(""), ReplyType::Reply);
}

#[test]
fn test_reply_type_default() {
    let default: ReplyType = Default::default();
    assert_eq!(default, ReplyType::Reply);
}

// =========================================================================
// HighlightMode tests
// =========================================================================

#[test]
fn test_highlight_mode_all_variants() {
    let modes = [
        (HighlightMode::None, "N"),
        (HighlightMode::Invert, "I"),
        (HighlightMode::Outline, "O"),
        (HighlightMode::Push, "P"),
    ];
    for (mode, name) in &modes {
        assert_eq!(mode.pdf_name(), *name);
        assert_eq!(HighlightMode::from_pdf_name(name), *mode);
    }
}

#[test]
fn test_highlight_mode_unknown_defaults_to_invert() {
    assert_eq!(HighlightMode::from_pdf_name("X"), HighlightMode::Invert);
    assert_eq!(HighlightMode::from_pdf_name("I"), HighlightMode::Invert);
}

#[test]
fn test_highlight_mode_default() {
    let default: HighlightMode = Default::default();
    assert_eq!(default, HighlightMode::Invert);
}

// =========================================================================
// WidgetFieldType tests
// =========================================================================

#[test]
fn test_widget_field_type_default() {
    let default: WidgetFieldType = Default::default();
    assert_eq!(default, WidgetFieldType::Text);
}

#[test]
fn test_widget_field_type_checkbox() {
    let checked = WidgetFieldType::Checkbox { checked: true };
    let unchecked = WidgetFieldType::Checkbox { checked: false };
    assert_ne!(checked, unchecked);
    match checked {
        WidgetFieldType::Checkbox { checked } => assert!(checked),
        _ => panic!("Expected Checkbox"),
    }
}

#[test]
fn test_widget_field_type_radio() {
    let radio = WidgetFieldType::Radio {
        selected: Some("Option1".to_string()),
    };
    match radio {
        WidgetFieldType::Radio { selected } => {
            assert_eq!(selected, Some("Option1".to_string()));
        }
        _ => panic!("Expected Radio"),
    }
}

#[test]
fn test_widget_field_type_choice() {
    let choice = WidgetFieldType::Choice {
        options: vec!["A".to_string(), "B".to_string(), "C".to_string()],
        selected: Some("B".to_string()),
    };
    match choice {
        WidgetFieldType::Choice { options, selected } => {
            assert_eq!(options.len(), 3);
            assert_eq!(selected, Some("B".to_string()));
        }
        _ => panic!("Expected Choice"),
    }
}

#[test]
fn test_widget_field_type_variants() {
    // Just verify construction works for all variants
    let _ = WidgetFieldType::Text;
    let _ = WidgetFieldType::Button;
    let _ = WidgetFieldType::Signature;
    let _ = WidgetFieldType::Unknown;
}

// =========================================================================
// quad_points extended tests
// =========================================================================

#[test]
fn test_quad_points_parse() {
    let flat: Vec<f64> = vec![
        0.0, 0.0, 100.0, 0.0, 100.0, 50.0, 0.0, 50.0, 200.0, 200.0, 300.0, 200.0, 300.0, 250.0,
        200.0, 250.0,
    ];
    let quads = quad_points::parse(&flat);
    assert_eq!(quads.len(), 2);
    assert_eq!(quads[0][0], 0.0);
    assert_eq!(quads[1][0], 200.0);
}

#[test]
fn test_quad_points_parse_partial() {
    // Less than 8 values should produce 0 quads (chunks_exact drops remainder)
    let flat: Vec<f64> = vec![1.0, 2.0, 3.0];
    let quads = quad_points::parse(&flat);
    assert!(quads.is_empty());
}

#[test]
fn test_quad_points_parse_empty() {
    let quads = quad_points::parse(&[]);
    assert!(quads.is_empty());
}

#[test]
fn test_quad_points_flatten() {
    let quads: Vec<[f64; 8]> = vec![
        [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0],
        [9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0],
    ];
    let flat = quad_points::flatten(&quads);
    assert_eq!(flat.len(), 16);
    assert_eq!(flat[0], 1.0);
    assert_eq!(flat[8], 9.0);
    assert_eq!(flat[15], 16.0);
}

#[test]
fn test_quad_points_flatten_empty() {
    let quads: Vec<[f64; 8]> = vec![];
    let flat = quad_points::flatten(&quads);
    assert!(flat.is_empty());
}

#[test]
fn test_quad_points_roundtrip() {
    let original: Vec<[f64; 8]> = vec![[10.0, 20.0, 110.0, 20.0, 110.0, 40.0, 10.0, 40.0]];
    let flat = quad_points::flatten(&original);
    let recovered = quad_points::parse(&flat);
    assert_eq!(recovered, original);
}

#[test]
fn test_quad_points_bounding_rect_rotated() {
    // A rotated quad where points are not axis-aligned
    let quad: [f64; 8] = [50.0, 0.0, 100.0, 50.0, 50.0, 100.0, 0.0, 50.0];
    let r = quad_points::bounding_rect(&quad);
    assert_eq!(r.x, 0.0);
    assert_eq!(r.y, 0.0);
    assert_eq!(r.width, 100.0);
    assert_eq!(r.height, 100.0);
}

// =========================================================================
// Clone, Copy, Debug trait verification
// =========================================================================

#[test]
fn test_annotation_subtype_clone_copy() {
    let subtype = AnnotationSubtype::Highlight;
    let cloned = subtype;
    assert_eq!(subtype, cloned); // Copy trait
}

#[test]
fn test_annotation_subtype_debug() {
    let debug = format!("{:?}", AnnotationSubtype::ThreeD);
    assert!(debug.contains("ThreeD"));
}

#[test]
fn test_annotation_subtype_hash() {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    set.insert(AnnotationSubtype::Text);
    set.insert(AnnotationSubtype::Link);
    set.insert(AnnotationSubtype::Text); // Duplicate
    assert_eq!(set.len(), 2);
}
