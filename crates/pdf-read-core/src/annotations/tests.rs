use super::*;

#[test]
fn test_annotation_creation() {
    let annot = Annotation {
        annotation_type: "Annot".to_string(),
        subtype: Some("Text".to_string()),
        subtype_enum: AnnotationSubtype::Text,
        contents: Some("This is a comment".to_string()),
        rect: Some([100.0, 200.0, 150.0, 250.0]),
        author: Some("John Doe".to_string()),
        creation_date: Some("D:20231030120000".to_string()),
        modification_date: None,
        subject: Some("Review".to_string()),
        destination: None,
        action: None,
        quad_points: None,
        color: None,
        opacity: None,
        flags: AnnotationFlags::empty(),
        border: None,
        interior_color: None,
        field_type: None,
        field_name: None,
        field_value: None,
        default_value: None,
        field_flags: None,
        options: None,
        appearance_state: None,
        raw_dict: None,
    };

    assert_eq!(annot.annotation_type, "Annot");
    assert_eq!(annot.subtype, Some("Text".to_string()));
    assert_eq!(annot.subtype_enum, AnnotationSubtype::Text);
    assert_eq!(annot.contents, Some("This is a comment".to_string()));
    assert!(annot.rect.is_some());
}

#[test]
fn test_highlight_annotation() {
    let annot = Annotation {
        annotation_type: "Annot".to_string(),
        subtype: Some("Highlight".to_string()),
        subtype_enum: AnnotationSubtype::Highlight,
        contents: Some("Highlighted text".to_string()),
        rect: Some([100.0, 700.0, 200.0, 720.0]),
        author: Some("Reviewer".to_string()),
        creation_date: None,
        modification_date: None,
        subject: None,
        destination: None,
        action: None,
        quad_points: Some(vec![[
            100.0, 700.0, 200.0, 700.0, 200.0, 720.0, 100.0, 720.0,
        ]]),
        color: Some(vec![1.0, 1.0, 0.0]), // Yellow
        opacity: Some(0.5),
        flags: AnnotationFlags::printable(),
        border: None,
        interior_color: None,
        field_type: None,
        field_name: None,
        field_value: None,
        default_value: None,
        field_flags: None,
        options: None,
        appearance_state: None,
        raw_dict: None,
    };

    assert!(annot.subtype_enum.is_text_markup());
    assert!(annot.quad_points.is_some());
    assert_eq!(annot.quad_points.as_ref().unwrap().len(), 1);
    assert_eq!(annot.color, Some(vec![1.0, 1.0, 0.0]));
    assert_eq!(annot.opacity, Some(0.5));
    assert!(annot.flags.is_printable());
}

#[test]
fn test_parse_number_array() {
    use crate::object::Object;

    // RGB color
    let arr = vec![Object::Real(1.0), Object::Real(0.5), Object::Real(0.0)];
    let result = PdfDocument::parse_number_array(Some(&Object::Array(arr)));
    assert_eq!(result, Some(vec![1.0, 0.5, 0.0]));

    // Mixed integers and reals
    let arr2 = vec![Object::Integer(1), Object::Real(0.5)];
    let result2 = PdfDocument::parse_number_array(Some(&Object::Array(arr2)));
    assert_eq!(result2, Some(vec![1.0, 0.5]));

    // None
    let result3 = PdfDocument::parse_number_array(None);
    assert!(result3.is_none());
}

#[test]
fn test_parse_quad_points() {
    use crate::object::Object;

    // Single quad (8 values)
    let arr: Vec<Object> = vec![
        Object::Real(100.0),
        Object::Real(700.0),
        Object::Real(200.0),
        Object::Real(700.0),
        Object::Real(200.0),
        Object::Real(720.0),
        Object::Real(100.0),
        Object::Real(720.0),
    ];
    let result = PdfDocument::parse_quad_points(Some(&Object::Array(arr)));
    assert!(result.is_some());
    let quads = result.unwrap();
    assert_eq!(quads.len(), 1);
    assert_eq!(quads[0][0], 100.0);
    assert_eq!(quads[0][6], 100.0);
}

#[test]
fn test_widget_text_field_annotation() {
    let annot = Annotation {
        annotation_type: "Annot".to_string(),
        subtype: Some("Widget".to_string()),
        subtype_enum: AnnotationSubtype::Widget,
        contents: None,
        rect: Some([100.0, 700.0, 300.0, 720.0]),
        author: None,
        creation_date: None,
        modification_date: None,
        subject: None,
        destination: None,
        action: None,
        quad_points: None,
        color: None,
        opacity: None,
        flags: AnnotationFlags::empty(),
        border: None,
        interior_color: None,
        field_type: Some(WidgetFieldType::Text),
        field_name: Some("FirstName".to_string()),
        field_value: Some("John".to_string()),
        default_value: None,
        field_flags: None,
        options: None,
        appearance_state: None,
        raw_dict: None,
    };

    assert_eq!(annot.subtype_enum, AnnotationSubtype::Widget);
    assert_eq!(annot.field_type, Some(WidgetFieldType::Text));
    assert_eq!(annot.field_name, Some("FirstName".to_string()));
    assert_eq!(annot.field_value, Some("John".to_string()));
}

#[test]
fn test_widget_checkbox_annotation() {
    let annot = Annotation {
        annotation_type: "Annot".to_string(),
        subtype: Some("Widget".to_string()),
        subtype_enum: AnnotationSubtype::Widget,
        contents: None,
        rect: Some([100.0, 600.0, 120.0, 620.0]),
        author: None,
        creation_date: None,
        modification_date: None,
        subject: None,
        destination: None,
        action: None,
        quad_points: None,
        color: None,
        opacity: None,
        flags: AnnotationFlags::empty(),
        border: None,
        interior_color: None,
        field_type: Some(WidgetFieldType::Checkbox { checked: true }),
        field_name: Some("AcceptTerms".to_string()),
        field_value: Some("Yes".to_string()),
        default_value: None,
        field_flags: None,
        options: None,
        appearance_state: Some("Yes".to_string()),
        raw_dict: None,
    };

    assert_eq!(annot.subtype_enum, AnnotationSubtype::Widget);
    match &annot.field_type {
        Some(WidgetFieldType::Checkbox { checked }) => assert!(*checked),
        _ => panic!("Expected Checkbox field type"),
    }
    assert_eq!(annot.appearance_state, Some("Yes".to_string()));
}

#[test]
fn test_widget_choice_annotation() {
    let annot = Annotation {
        annotation_type: "Annot".to_string(),
        subtype: Some("Widget".to_string()),
        subtype_enum: AnnotationSubtype::Widget,
        contents: None,
        rect: Some([100.0, 500.0, 250.0, 520.0]),
        author: None,
        creation_date: None,
        modification_date: None,
        subject: None,
        destination: None,
        action: None,
        quad_points: None,
        color: None,
        opacity: None,
        flags: AnnotationFlags::empty(),
        border: None,
        interior_color: None,
        field_type: Some(WidgetFieldType::Choice {
            options: vec![
                "Option A".to_string(),
                "Option B".to_string(),
                "Option C".to_string(),
            ],
            selected: Some("Option B".to_string()),
        }),
        field_name: Some("Selection".to_string()),
        field_value: Some("Option B".to_string()),
        default_value: Some("Option A".to_string()),
        field_flags: None,
        options: Some(vec![
            "Option A".to_string(),
            "Option B".to_string(),
            "Option C".to_string(),
        ]),
        appearance_state: None,
        raw_dict: None,
    };

    assert_eq!(annot.subtype_enum, AnnotationSubtype::Widget);
    match &annot.field_type {
        Some(WidgetFieldType::Choice { options, selected }) => {
            assert_eq!(options.len(), 3);
            assert_eq!(selected, &Some("Option B".to_string()));
        }
        _ => panic!("Expected Choice field type"),
    }
    assert_eq!(annot.options.as_ref().unwrap().len(), 3);
}

#[test]
fn test_widget_field_type_default() {
    assert_eq!(WidgetFieldType::default(), WidgetFieldType::Text);
}

#[test]
fn test_parse_string_value() {
    assert_eq!(
        PdfDocument::parse_string_value(Some(&Object::String(b"Hello".to_vec()))),
        Some("Hello".to_string())
    );
    assert_eq!(
        PdfDocument::parse_string_value(Some(&Object::Name("MyName".to_string()))),
        Some("MyName".to_string())
    );
    assert_eq!(
        PdfDocument::parse_string_value(Some(&Object::Integer(42))),
        Some("42".to_string())
    );
    assert_eq!(PdfDocument::parse_string_value(None), None);
}

#[test]
fn test_parse_options_array() {
    let arr = vec![
        Object::String(b"Option 1".to_vec()),
        Object::String(b"Option 2".to_vec()),
    ];
    let result = PdfDocument::parse_options_array(Some(&Object::Array(arr)));
    assert!(result.is_some());
    let opts = result.unwrap();
    assert_eq!(opts.len(), 2);
    assert_eq!(opts[0], "Option 1");
    assert_eq!(opts[1], "Option 2");

    // Test empty array
    let empty: Vec<Object> = vec![];
    assert!(PdfDocument::parse_options_array(Some(&Object::Array(empty))).is_none());

    // Test None
    assert!(PdfDocument::parse_options_array(None).is_none());
}
