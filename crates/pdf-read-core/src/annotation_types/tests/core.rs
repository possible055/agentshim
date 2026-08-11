use super::*;

#[test]
fn test_annotation_subtype_roundtrip() {
    let subtypes = [
        AnnotationSubtype::Text,
        AnnotationSubtype::Link,
        AnnotationSubtype::Highlight,
        AnnotationSubtype::StrikeOut,
        AnnotationSubtype::Stamp,
        AnnotationSubtype::Ink,
        AnnotationSubtype::Widget,
        AnnotationSubtype::Redact,
    ];

    for subtype in subtypes {
        let name = subtype.pdf_name();
        let parsed = AnnotationSubtype::from_pdf_name(name);
        assert_eq!(subtype, parsed);
    }
}

#[test]
fn test_annotation_flags() {
    let mut flags = AnnotationFlags::empty();
    assert!(!flags.is_printable());

    flags.set(AnnotationFlags::PRINT);
    assert!(flags.is_printable());

    flags.set(AnnotationFlags::READ_ONLY);
    assert!(flags.is_read_only());

    flags.clear(AnnotationFlags::PRINT);
    assert!(!flags.is_printable());
    assert!(flags.is_read_only());
}

#[test]
fn test_annotation_color() {
    let yellow = AnnotationColor::yellow();
    let arr = yellow.to_array().unwrap();
    assert_eq!(arr, vec![1.0, 1.0, 0.0]);

    let parsed = AnnotationColor::from_array(&arr);
    assert_eq!(parsed, yellow);
}

#[test]
fn test_quad_points() {
    let rect = Rect::new(100.0, 200.0, 50.0, 20.0);
    let quad = quad_points::from_rect(&rect);

    assert_eq!(quad[0], 100.0); // x1
    assert_eq!(quad[1], 200.0); // y1
    assert_eq!(quad[2], 150.0); // x2
    assert_eq!(quad[3], 200.0); // y2

    let bounding = quad_points::bounding_rect(&quad);
    assert_eq!(bounding.x, rect.x);
    assert_eq!(bounding.y, rect.y);
    assert_eq!(bounding.width, rect.width);
    assert_eq!(bounding.height, rect.height);
}

#[test]
fn test_stamp_types() {
    assert_eq!(StampType::Approved.pdf_name(), "Approved");
    assert_eq!(StampType::Draft.pdf_name(), "Draft");
    assert_eq!(
        StampType::Custom("MyStamp".to_string()).pdf_name(),
        "MyStamp"
    );

    assert_eq!(StampType::from_pdf_name("Approved"), StampType::Approved);
    assert_eq!(
        StampType::from_pdf_name("CustomName"),
        StampType::Custom("CustomName".to_string())
    );
}

#[test]
fn test_line_ending_styles() {
    let styles = [
        LineEndingStyle::None,
        LineEndingStyle::OpenArrow,
        LineEndingStyle::ClosedArrow,
        LineEndingStyle::Circle,
        LineEndingStyle::Square,
    ];

    for style in styles {
        let name = style.pdf_name();
        let parsed = LineEndingStyle::from_pdf_name(name);
        assert_eq!(style, parsed);
    }
}

#[test]
fn test_is_markup() {
    assert!(AnnotationSubtype::Text.is_markup());
    assert!(AnnotationSubtype::Highlight.is_markup());
    assert!(AnnotationSubtype::Ink.is_markup());
    assert!(!AnnotationSubtype::Link.is_markup());
    assert!(!AnnotationSubtype::Widget.is_markup());
    assert!(!AnnotationSubtype::Popup.is_markup());
}

#[test]
fn test_is_text_markup() {
    assert!(AnnotationSubtype::Highlight.is_text_markup());
    assert!(AnnotationSubtype::Underline.is_text_markup());
    assert!(AnnotationSubtype::Squiggly.is_text_markup());
    assert!(AnnotationSubtype::StrikeOut.is_text_markup());
    assert!(!AnnotationSubtype::Text.is_text_markup());
    assert!(!AnnotationSubtype::Ink.is_text_markup());
}

// =========================================================================
// Comprehensive AnnotationSubtype roundtrip tests for all variants
// =========================================================================

#[test]
fn test_annotation_subtype_all_variants_roundtrip() {
    let all_subtypes = [
        (AnnotationSubtype::Text, "Text"),
        (AnnotationSubtype::Link, "Link"),
        (AnnotationSubtype::FreeText, "FreeText"),
        (AnnotationSubtype::Line, "Line"),
        (AnnotationSubtype::Square, "Square"),
        (AnnotationSubtype::Circle, "Circle"),
        (AnnotationSubtype::Polygon, "Polygon"),
        (AnnotationSubtype::PolyLine, "PolyLine"),
        (AnnotationSubtype::Highlight, "Highlight"),
        (AnnotationSubtype::Underline, "Underline"),
        (AnnotationSubtype::Squiggly, "Squiggly"),
        (AnnotationSubtype::StrikeOut, "StrikeOut"),
        (AnnotationSubtype::Stamp, "Stamp"),
        (AnnotationSubtype::Caret, "Caret"),
        (AnnotationSubtype::Ink, "Ink"),
        (AnnotationSubtype::Popup, "Popup"),
        (AnnotationSubtype::FileAttachment, "FileAttachment"),
        (AnnotationSubtype::Sound, "Sound"),
        (AnnotationSubtype::Movie, "Movie"),
        (AnnotationSubtype::Widget, "Widget"),
        (AnnotationSubtype::Screen, "Screen"),
        (AnnotationSubtype::PrinterMark, "PrinterMark"),
        (AnnotationSubtype::TrapNet, "TrapNet"),
        (AnnotationSubtype::Watermark, "Watermark"),
        (AnnotationSubtype::ThreeD, "3D"),
        (AnnotationSubtype::Redact, "Redact"),
        (AnnotationSubtype::RichMedia, "RichMedia"),
        (AnnotationSubtype::Unknown, "Unknown"),
    ];

    for (subtype, expected_name) in &all_subtypes {
        assert_eq!(
            subtype.pdf_name(),
            *expected_name,
            "pdf_name mismatch for {:?}",
            subtype
        );
        let parsed = AnnotationSubtype::from_pdf_name(expected_name);
        assert_eq!(*subtype, parsed, "roundtrip mismatch for {}", expected_name);
    }
}

#[test]
fn test_annotation_subtype_unknown_name() {
    let parsed = AnnotationSubtype::from_pdf_name("NonExistentType");
    assert_eq!(parsed, AnnotationSubtype::Unknown);
}

#[test]
fn test_annotation_subtype_is_markup_complete() {
    // All markup types
    let markup = [
        AnnotationSubtype::Text,
        AnnotationSubtype::FreeText,
        AnnotationSubtype::Line,
        AnnotationSubtype::Square,
        AnnotationSubtype::Circle,
        AnnotationSubtype::Polygon,
        AnnotationSubtype::PolyLine,
        AnnotationSubtype::Highlight,
        AnnotationSubtype::Underline,
        AnnotationSubtype::Squiggly,
        AnnotationSubtype::StrikeOut,
        AnnotationSubtype::Stamp,
        AnnotationSubtype::Caret,
        AnnotationSubtype::Ink,
        AnnotationSubtype::FileAttachment,
        AnnotationSubtype::Sound,
        AnnotationSubtype::Redact,
    ];
    for subtype in &markup {
        assert!(subtype.is_markup(), "{:?} should be markup", subtype);
    }

    // All non-markup types
    let non_markup = [
        AnnotationSubtype::Link,
        AnnotationSubtype::Popup,
        AnnotationSubtype::Movie,
        AnnotationSubtype::Widget,
        AnnotationSubtype::Screen,
        AnnotationSubtype::PrinterMark,
        AnnotationSubtype::TrapNet,
        AnnotationSubtype::Watermark,
        AnnotationSubtype::ThreeD,
        AnnotationSubtype::RichMedia,
        AnnotationSubtype::Unknown,
    ];
    for subtype in &non_markup {
        assert!(!subtype.is_markup(), "{:?} should NOT be markup", subtype);
    }
}

// =========================================================================
// AnnotationFlags extended tests
// =========================================================================

#[test]
fn test_annotation_flags_new() {
    let flags = AnnotationFlags::new(0b101); // INVISIBLE | PRINT
    assert!(flags.is_invisible());
    assert!(!flags.is_hidden());
    assert!(flags.is_printable());
}

#[test]
fn test_annotation_flags_printable_constructor() {
    let flags = AnnotationFlags::printable();
    assert!(flags.is_printable());
    assert!(!flags.is_invisible());
    assert!(!flags.is_hidden());
    assert_eq!(flags.bits(), AnnotationFlags::PRINT);
}

#[test]
fn test_annotation_flags_all_named_flags() {
    let mut flags = AnnotationFlags::empty();

    flags.set(AnnotationFlags::INVISIBLE);
    assert!(flags.is_invisible());

    flags.set(AnnotationFlags::HIDDEN);
    assert!(flags.is_hidden());

    flags.set(AnnotationFlags::PRINT);
    assert!(flags.is_printable());

    flags.set(AnnotationFlags::NO_ZOOM);
    assert!(flags.is_no_zoom());

    flags.set(AnnotationFlags::NO_ROTATE);
    assert!(flags.is_no_rotate());

    flags.set(AnnotationFlags::READ_ONLY);
    assert!(flags.is_read_only());

    flags.set(AnnotationFlags::LOCKED);
    assert!(flags.is_locked());
}

#[test]
fn test_annotation_flags_no_view_and_toggle() {
    let mut flags = AnnotationFlags::empty();
    flags.set(AnnotationFlags::NO_VIEW);
    assert!(flags.contains(AnnotationFlags::NO_VIEW));

    flags.set(AnnotationFlags::TOGGLE_NO_VIEW);
    assert!(flags.contains(AnnotationFlags::TOGGLE_NO_VIEW));

    flags.set(AnnotationFlags::LOCKED_CONTENTS);
    assert!(flags.contains(AnnotationFlags::LOCKED_CONTENTS));
}

#[test]
fn test_annotation_flags_clear_all() {
    let mut flags = AnnotationFlags::new(0xFFFF);
    flags.clear(AnnotationFlags::PRINT);
    assert!(!flags.is_printable());
    assert!(flags.is_invisible()); // Other flags still set
}

#[test]
fn test_annotation_flags_default() {
    let flags = AnnotationFlags::default();
    assert_eq!(flags.bits(), 0);
    assert!(!flags.is_printable());
    assert!(!flags.is_invisible());
}

#[test]
fn test_annotation_flags_bits_roundtrip() {
    let flags = AnnotationFlags::new(0b1010_0101);
    assert_eq!(flags.bits(), 0b1010_0101);
}

// =========================================================================
// BorderStyleType tests
// =========================================================================

#[test]
fn test_border_style_type_all_variants() {
    let styles = [
        (BorderStyleType::Solid, "S"),
        (BorderStyleType::Dashed, "D"),
        (BorderStyleType::Beveled, "B"),
        (BorderStyleType::Inset, "I"),
        (BorderStyleType::Underline, "U"),
    ];
    for (style, name) in &styles {
        assert_eq!(style.pdf_name(), *name);
        assert_eq!(BorderStyleType::from_pdf_name(name), *style);
    }
}

#[test]
fn test_border_style_type_unknown_defaults_to_solid() {
    assert_eq!(BorderStyleType::from_pdf_name("X"), BorderStyleType::Solid);
    assert_eq!(BorderStyleType::from_pdf_name(""), BorderStyleType::Solid);
}

#[test]
fn test_border_style_type_default() {
    let default: BorderStyleType = Default::default();
    assert_eq!(default, BorderStyleType::Solid);
}

// =========================================================================
// AnnotationBorderStyle tests
// =========================================================================

#[test]
fn test_annotation_border_style_solid() {
    let bs = AnnotationBorderStyle::solid(2.0);
    assert_eq!(bs.width, 2.0);
    assert_eq!(bs.style, BorderStyleType::Solid);
    assert!(bs.dash_pattern.is_none());
}

#[test]
fn test_annotation_border_style_dashed() {
    let bs = AnnotationBorderStyle::dashed(1.5, 3.0, 2.0);
    assert_eq!(bs.width, 1.5);
    assert_eq!(bs.style, BorderStyleType::Dashed);
    assert_eq!(bs.dash_pattern, Some(vec![3.0, 2.0]));
}

#[test]
fn test_annotation_border_style_none() {
    let bs = AnnotationBorderStyle::none();
    assert_eq!(bs.width, 0.0);
    assert_eq!(bs.style, BorderStyleType::Solid);
    assert!(bs.dash_pattern.is_none());
}

#[test]
fn test_annotation_border_style_default() {
    let bs: AnnotationBorderStyle = Default::default();
    assert_eq!(bs.width, 0.0);
    assert_eq!(bs.style, BorderStyleType::Solid);
    assert!(bs.dash_pattern.is_none());
}

// =========================================================================
// BorderEffectStyle tests
// =========================================================================

#[test]
fn test_border_effect_style_roundtrip() {
    assert_eq!(BorderEffectStyle::None.pdf_name(), "S");
    assert_eq!(BorderEffectStyle::Cloudy.pdf_name(), "C");

    assert_eq!(
        BorderEffectStyle::from_pdf_name("S"),
        BorderEffectStyle::None
    );
    assert_eq!(
        BorderEffectStyle::from_pdf_name("C"),
        BorderEffectStyle::Cloudy
    );
    assert_eq!(
        BorderEffectStyle::from_pdf_name("X"),
        BorderEffectStyle::None
    );
    // default
}

#[test]
fn test_border_effect_style_default() {
    let default: BorderEffectStyle = Default::default();
    assert_eq!(default, BorderEffectStyle::None);
}

#[test]
fn test_border_effect_default() {
    let effect: BorderEffect = Default::default();
    assert_eq!(effect.style, BorderEffectStyle::None);
    assert_eq!(effect.intensity, 0.0);
}

// =========================================================================
// LineEndingStyle complete tests
// =========================================================================

#[test]
fn test_line_ending_style_all_variants() {
    let styles = [
        (LineEndingStyle::None, "None"),
        (LineEndingStyle::Square, "Square"),
        (LineEndingStyle::Circle, "Circle"),
        (LineEndingStyle::Diamond, "Diamond"),
        (LineEndingStyle::OpenArrow, "OpenArrow"),
        (LineEndingStyle::ClosedArrow, "ClosedArrow"),
        (LineEndingStyle::Butt, "Butt"),
        (LineEndingStyle::ROpenArrow, "ROpenArrow"),
        (LineEndingStyle::RClosedArrow, "RClosedArrow"),
        (LineEndingStyle::Slash, "Slash"),
    ];
    for (style, name) in &styles {
        assert_eq!(style.pdf_name(), *name, "pdf_name mismatch for {:?}", style);
        assert_eq!(
            LineEndingStyle::from_pdf_name(name),
            *style,
            "from_pdf_name mismatch for {}",
            name
        );
    }
}

#[test]
fn test_line_ending_style_unknown_defaults_to_none() {
    assert_eq!(
        LineEndingStyle::from_pdf_name("Unknown"),
        LineEndingStyle::None
    );
    assert_eq!(LineEndingStyle::from_pdf_name(""), LineEndingStyle::None);
}

#[test]
fn test_line_ending_style_default() {
    let default: LineEndingStyle = Default::default();
    assert_eq!(default, LineEndingStyle::None);
}

// =========================================================================
// AnnotationColor extended tests
// =========================================================================

#[test]
fn test_annotation_color_factory_methods() {
    let red = AnnotationColor::red();
    assert_eq!(red, AnnotationColor::Rgb(1.0, 0.0, 0.0));

    let green = AnnotationColor::green();
    assert_eq!(green, AnnotationColor::Rgb(0.0, 1.0, 0.0));

    let blue = AnnotationColor::blue();
    assert_eq!(blue, AnnotationColor::Rgb(0.0, 0.0, 1.0));

    let black = AnnotationColor::black();
    assert_eq!(black, AnnotationColor::Gray(0.0));

    let white = AnnotationColor::white();
    assert_eq!(white, AnnotationColor::Gray(1.0));
}

#[test]
fn test_annotation_color_to_array_none() {
    let none = AnnotationColor::None;
    assert!(none.to_array().is_none());
}

#[test]
fn test_annotation_color_to_array_gray() {
    let gray = AnnotationColor::Gray(0.5);
    assert_eq!(gray.to_array(), Some(vec![0.5]));
}

#[test]
fn test_annotation_color_to_array_cmyk() {
    let cmyk = AnnotationColor::Cmyk(0.1, 0.2, 0.3, 0.4);
    assert_eq!(cmyk.to_array(), Some(vec![0.1, 0.2, 0.3, 0.4]));
}

#[test]
fn test_annotation_color_from_array_all_sizes() {
    assert_eq!(AnnotationColor::from_array(&[]), AnnotationColor::None);
    assert_eq!(
        AnnotationColor::from_array(&[0.5]),
        AnnotationColor::Gray(0.5)
    );
    assert_eq!(
        AnnotationColor::from_array(&[1.0, 0.0, 0.0]),
        AnnotationColor::Rgb(1.0, 0.0, 0.0)
    );
    assert_eq!(
        AnnotationColor::from_array(&[0.1, 0.2, 0.3, 0.4]),
        AnnotationColor::Cmyk(0.1, 0.2, 0.3, 0.4)
    );
    // Invalid sizes default to None
    assert_eq!(
        AnnotationColor::from_array(&[1.0, 2.0]),
        AnnotationColor::None
    );
    assert_eq!(
        AnnotationColor::from_array(&[1.0, 2.0, 3.0, 4.0, 5.0]),
        AnnotationColor::None
    );
}

#[test]
fn test_annotation_color_default() {
    let default: AnnotationColor = Default::default();
    assert_eq!(default, AnnotationColor::None);
}

// =========================================================================
// TextAnnotationIcon tests
// =========================================================================

#[test]
fn test_text_annotation_icon_all_variants() {
    let icons = [
        (TextAnnotationIcon::Comment, "Comment"),
        (TextAnnotationIcon::Key, "Key"),
        (TextAnnotationIcon::Note, "Note"),
        (TextAnnotationIcon::Help, "Help"),
        (TextAnnotationIcon::NewParagraph, "NewParagraph"),
        (TextAnnotationIcon::Paragraph, "Paragraph"),
        (TextAnnotationIcon::Insert, "Insert"),
    ];
    for (icon, name) in &icons {
        assert_eq!(icon.pdf_name(), *name);
        assert_eq!(TextAnnotationIcon::from_pdf_name(name), *icon);
    }
}

#[test]
fn test_text_annotation_icon_unknown_defaults_to_note() {
    assert_eq!(
        TextAnnotationIcon::from_pdf_name("Unknown"),
        TextAnnotationIcon::Note
    );
}

#[test]
fn test_text_annotation_icon_default() {
    let default: TextAnnotationIcon = Default::default();
    assert_eq!(default, TextAnnotationIcon::Note);
}

// =========================================================================
// TextMarkupType tests
// =========================================================================

#[test]
fn test_text_markup_type_subtype() {
    assert_eq!(
        TextMarkupType::Highlight.subtype(),
        AnnotationSubtype::Highlight
    );
    assert_eq!(
        TextMarkupType::Underline.subtype(),
        AnnotationSubtype::Underline
    );
    assert_eq!(
        TextMarkupType::Squiggly.subtype(),
        AnnotationSubtype::Squiggly
    );
    assert_eq!(
        TextMarkupType::StrikeOut.subtype(),
        AnnotationSubtype::StrikeOut
    );
}

#[test]
fn test_text_markup_type_default_color() {
    let h = TextMarkupType::Highlight.default_color();
    assert_eq!(h, AnnotationColor::yellow());

    let u = TextMarkupType::Underline.default_color();
    assert_eq!(u, AnnotationColor::green());

    let sq = TextMarkupType::Squiggly.default_color();
    assert_eq!(sq, AnnotationColor::Rgb(1.0, 0.5, 0.0));

    let so = TextMarkupType::StrikeOut.default_color();
    assert_eq!(so, AnnotationColor::red());
}

// =========================================================================
// StampType extended tests
// =========================================================================

#[test]
fn test_stamp_type_all_standard_variants() {
    let stamps = [
        (StampType::Approved, "Approved"),
        (StampType::Experimental, "Experimental"),
        (StampType::NotApproved, "NotApproved"),
        (StampType::AsIs, "AsIs"),
        (StampType::Expired, "Expired"),
        (StampType::NotForPublicRelease, "NotForPublicRelease"),
        (StampType::Confidential, "Confidential"),
        (StampType::Final, "Final"),
        (StampType::Sold, "Sold"),
        (StampType::Departmental, "Departmental"),
        (StampType::ForComment, "ForComment"),
        (StampType::TopSecret, "TopSecret"),
        (StampType::Draft, "Draft"),
        (StampType::ForPublicRelease, "ForPublicRelease"),
    ];
    for (stamp, name) in &stamps {
        assert_eq!(stamp.pdf_name(), *name, "pdf_name mismatch for {:?}", stamp);
        assert_eq!(
            StampType::from_pdf_name(name),
            *stamp,
            "from_pdf_name mismatch for {}",
            name
        );
    }
}

#[test]
fn test_stamp_type_custom_roundtrip() {
    let custom = StampType::Custom("CompanyLogo".to_string());
    assert_eq!(custom.pdf_name(), "CompanyLogo");
    assert_eq!(
        StampType::from_pdf_name("CompanyLogo"),
        StampType::Custom("CompanyLogo".to_string())
    );
}

#[test]
fn test_stamp_type_default() {
    let default: StampType = Default::default();
    assert_eq!(default, StampType::Draft);
}

// =========================================================================
// FreeTextIntent tests
// =========================================================================

#[test]
fn test_free_text_intent_all_variants() {
    let intents = [
        (FreeTextIntent::FreeText, "FreeText"),
        (FreeTextIntent::FreeTextCallout, "FreeTextCallout"),
        (FreeTextIntent::FreeTextTypeWriter, "FreeTextTypeWriter"),
    ];
    for (intent, name) in &intents {
        assert_eq!(intent.pdf_name(), *name);
        assert_eq!(FreeTextIntent::from_pdf_name(name), *intent);
    }
}

#[test]
fn test_free_text_intent_unknown_defaults_to_freetext() {
    assert_eq!(
        FreeTextIntent::from_pdf_name("Something"),
        FreeTextIntent::FreeText
    );
}

#[test]
fn test_free_text_intent_default() {
    let default: FreeTextIntent = Default::default();
    assert_eq!(default, FreeTextIntent::FreeText);
}

// =========================================================================
// TextAlignment tests
// =========================================================================

#[test]
fn test_text_alignment_all_variants() {
    assert_eq!(TextAlignment::Left.to_pdf_int(), 0);
    assert_eq!(TextAlignment::Center.to_pdf_int(), 1);
    assert_eq!(TextAlignment::Right.to_pdf_int(), 2);

    assert_eq!(TextAlignment::from_pdf_int(0), TextAlignment::Left);
    assert_eq!(TextAlignment::from_pdf_int(1), TextAlignment::Center);
    assert_eq!(TextAlignment::from_pdf_int(2), TextAlignment::Right);
}

#[test]
fn test_text_alignment_unknown_defaults_to_left() {
    assert_eq!(TextAlignment::from_pdf_int(-1), TextAlignment::Left);
    assert_eq!(TextAlignment::from_pdf_int(3), TextAlignment::Left);
    assert_eq!(TextAlignment::from_pdf_int(999), TextAlignment::Left);
}

#[test]
fn test_text_alignment_default() {
    let default: TextAlignment = Default::default();
    assert_eq!(default, TextAlignment::Left);
}

// =========================================================================
// CaretSymbol tests
// =========================================================================

#[test]
fn test_caret_symbol_all_variants() {
    assert_eq!(CaretSymbol::None.pdf_name(), "None");
    assert_eq!(CaretSymbol::Paragraph.pdf_name(), "P");

    assert_eq!(CaretSymbol::from_pdf_name("None"), CaretSymbol::None);
    assert_eq!(CaretSymbol::from_pdf_name("P"), CaretSymbol::Paragraph);
}

#[test]
fn test_caret_symbol_unknown_defaults_to_none() {
    assert_eq!(CaretSymbol::from_pdf_name("Q"), CaretSymbol::None);
    assert_eq!(CaretSymbol::from_pdf_name(""), CaretSymbol::None);
}

#[test]
fn test_caret_symbol_default() {
    let default: CaretSymbol = Default::default();
    assert_eq!(default, CaretSymbol::None);
}

// =========================================================================
// FileAttachmentIcon tests
// =========================================================================
