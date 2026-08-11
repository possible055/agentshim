use super::*;

/// Regression test for issue-336-example: adjacent MCID spans (gap ≤ 0) must NOT
/// have a space inserted between them.  The PDF stores e.g. "Q" (MCID 1) and "（"
/// (MCID 2) as separate marked-content runs that abut each other on the same line.
/// Before the fix, extract_cell always inserted a space between any two MCID blocks,
/// producing "Q （peu/d）" instead of the correct "Q（peu/d）".
#[test]
fn test_extract_cell_adjacent_mcid_spans_no_space() {
    use crate::layout::text_block::{Color, FontWeight};

    // Build TD > [MCID 1, MCID 2, MCID 3]  (three adjacent spans on the same line)
    let mut td = StructElem::new(StructType::TD);
    td.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    td.add_child(StructChild::MarkedContentRef {
        mcid: 2,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    td.add_child(StructChild::MarkedContentRef {
        mcid: 3,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut tr = StructElem::new(StructType::TR);
    tr.add_child(StructChild::StructElem(Box::new(td)));
    let mut table_elem = StructElem::new(StructType::Table);
    table_elem.add_child(StructChild::StructElem(Box::new(tr)));

    // Exact coordinates from issue-336 page 0 (Q（peu/d） column header):
    //   "Q"     x=345.79 w=8.22  end=354.01
    //   "（"    x=353.83 w=10.56 end=364.39   gap=-0.18 (overlap → no space)
    //   "peu/d" x=364.39 w=25.24             gap=0.00  (touching → no space)
    let base = crate::layout::TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: String::new(),
        bbox: Rect::new(0.0, 678.0, 0.0, 10.56),
        font_name: "Test".to_string(),
        font_size: 10.56,
        font_weight: FontWeight::Normal,
        is_italic: false,
        is_monospace: false,
        color: Color::black(),
        mcid: None,
        mcid_scope: None,
        sequence: 0,
        split_boundary_before: false,
        offset_semantic: false,
        char_spacing: 0.0,
        word_spacing: 0.0,
        horizontal_scaling: 1.0,
        primary_detected: false,
        char_widths: vec![],
        char_x_offsets: Vec::new(),
        heading_level: None,
        rotation_degrees: 0.0,
        wmode: 0,
        rtl_draw_logical: false,
    };
    let spans = vec![
        crate::layout::TextSpan {
            text: "Q".into(),
            bbox: Rect::new(345.79, 678.0, 8.22, 10.56),
            mcid: Some(1),
            mcid_scope: None,
            ..base.clone()
        },
        crate::layout::TextSpan {
            text: "（".into(),
            bbox: Rect::new(353.83, 678.0, 10.56, 10.56),
            mcid: Some(2),
            mcid_scope: None,
            ..base.clone()
        },
        crate::layout::TextSpan {
            text: "peu/d".into(),
            bbox: Rect::new(364.39, 678.0, 25.24, 10.56),
            mcid: Some(3),
            mcid_scope: None,
            ..base.clone()
        },
    ];

    let result = extract_table_from_spans(&table_elem, &spans).unwrap();
    assert_eq!(
        result.rows[0].cells[0].text, "Q（peu/d",
        "adjacent MCID spans must not get a space inserted between them"
    );
}

/// Companion test: MCID spans on different lines (multi-line cell) DO get a space.
#[test]
fn test_extract_cell_multiline_mcid_spans_have_space() {
    let mut td = StructElem::new(StructType::TD);
    td.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    td.add_child(StructChild::MarkedContentRef {
        mcid: 2,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut tr = StructElem::new(StructType::TR);
    tr.add_child(StructChild::StructElem(Box::new(td)));
    let mut table_elem = StructElem::new(StructType::Table);
    table_elem.add_child(StructChild::StructElem(Box::new(tr)));

    let base = crate::layout::TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: String::new(),
        bbox: Rect::new(0.0, 0.0, 0.0, 12.0),
        font_name: "Test".to_string(),
        font_size: 12.0,
        font_weight: crate::layout::text_block::FontWeight::Normal,
        is_italic: false,
        is_monospace: false,
        color: crate::layout::text_block::Color::black(),
        mcid: None,
        mcid_scope: None,
        sequence: 0,
        split_boundary_before: false,
        offset_semantic: false,
        char_spacing: 0.0,
        word_spacing: 0.0,
        horizontal_scaling: 1.0,
        primary_detected: false,
        char_widths: vec![],
        char_x_offsets: Vec::new(),
        heading_level: None,
        rotation_degrees: 0.0,
        wmode: 0,
        rtl_draw_logical: false,
    };
    // Line 1: "Hello" ends at x=100, y=200.  Line 2: "World" starts at x=10, y=188.
    // y_diff = 12 > line_h * 0.5 = 6 → different lines → space inserted.
    let spans = vec![
        crate::layout::TextSpan {
            text: "Hello".into(),
            bbox: Rect::new(10.0, 200.0, 90.0, 12.0),
            mcid: Some(1),
            mcid_scope: None,
            ..base.clone()
        },
        crate::layout::TextSpan {
            text: "World".into(),
            bbox: Rect::new(10.0, 188.0, 90.0, 12.0),
            mcid: Some(2),
            mcid_scope: None,
            ..base.clone()
        },
    ];

    let result = extract_table_from_spans(&table_elem, &spans).unwrap();
    assert_eq!(result.rows[0].cells[0].text, "Hello World");
}

/// The synthesized `cell.spans` on the tagged-PDF (MCID→TextBlock) path must
/// carry per-block `font_weight`/`is_italic`, otherwise the markdown/HTML
/// table renderers can't emit bold/italic markers and silently fall back to
/// plain text. Also asserts the inter-line space is carried into the span
/// text so renderers reconstructing from spans don't glue tokens across a
/// wrapped line.
#[test]
fn test_extract_cell_spans_carry_bold_italic_and_spacing() {
    use crate::layout::text_block::{Color, FontWeight};

    let mut td = StructElem::new(StructType::TD);
    td.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    td.add_child(StructChild::MarkedContentRef {
        mcid: 2,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut tr = StructElem::new(StructType::TR);
    tr.add_child(StructChild::StructElem(Box::new(td)));
    let mut table_elem = StructElem::new(StructType::Table);
    table_elem.add_child(StructChild::StructElem(Box::new(tr)));

    let base = crate::layout::TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: String::new(),
        bbox: Rect::new(0.0, 0.0, 0.0, 12.0),
        font_name: "Test".to_string(),
        font_size: 12.0,
        font_weight: FontWeight::Normal,
        is_italic: false,
        is_monospace: false,
        color: Color::black(),
        mcid: None,
        mcid_scope: None,
        sequence: 0,
        split_boundary_before: false,
        offset_semantic: false,
        char_spacing: 0.0,
        word_spacing: 0.0,
        horizontal_scaling: 1.0,
        primary_detected: false,
        char_widths: vec![],
        char_x_offsets: Vec::new(),
        heading_level: None,
        rotation_degrees: 0.0,
        wmode: 0,
        rtl_draw_logical: false,
    };
    // Line 1: bold "Bold" (y=200).  Line 2 (wrapped): italic "Italic" (y=188).
    let spans = vec![
        crate::layout::TextSpan {
            text: "Bold".into(),
            bbox: Rect::new(10.0, 200.0, 40.0, 12.0),
            font_weight: FontWeight::Bold,
            mcid: Some(1),
            mcid_scope: None,
            ..base.clone()
        },
        crate::layout::TextSpan {
            text: "Italic".into(),
            bbox: Rect::new(10.0, 188.0, 40.0, 12.0),
            is_italic: true,
            mcid: Some(2),
            mcid_scope: None,
            ..base.clone()
        },
    ];

    let result = extract_table_from_spans(&table_elem, &spans).unwrap();
    let cell = &result.rows[0].cells[0];
    assert_eq!(cell.spans.len(), 2, "both MCID blocks must yield a span");
    assert_eq!(cell.spans[0].text, "Bold");
    assert!(
        matches!(cell.spans[0].font_weight, FontWeight::Bold),
        "bold block must propagate FontWeight::Bold into the synthesized span"
    );
    assert!(
        !cell.spans[0].is_italic,
        "non-italic block must not be italic"
    );
    assert!(
        matches!(cell.spans[1].font_weight, FontWeight::Normal),
        "non-bold block must stay FontWeight::Normal"
    );
    assert!(
        cell.spans[1].is_italic,
        "italic block must propagate is_italic into the synthesized span"
    );
    assert_eq!(
        cell.spans[1].text, " Italic",
        "wrapped-line span must carry the leading inter-block space (review #533)"
    );
}

/// CJK + fullwidth operator with a gap that *exceeds* the 0.15em threshold must
/// still suppress space insertion — this exercises the new CJK-suppression branch
/// added in fix #485 (the `test_extract_cell_adjacent_mcid_spans_no_space` test
/// above only covers the gap ≤ threshold path, which never reaches this branch).
#[test]
fn test_extract_cell_cjk_fullwidth_gap_suppresses_space() {
    use crate::layout::text_block::{Color, FontWeight};

    // Build: TD with three MCIDs: "数" (CJK), "≤" (math op), "量" (CJK)
    // Place them with a gap of 3.0 pt (> font_size * 0.15 = 1.5 for 10 pt font)
    // so the gap branch fires, then the CJK suppression should prevent a space.
    let mut td = StructElem::new(StructType::TD);
    td.add_child(StructChild::MarkedContentRef {
        mcid: 10,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    td.add_child(StructChild::MarkedContentRef {
        mcid: 11,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    td.add_child(StructChild::MarkedContentRef {
        mcid: 12,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut tr = StructElem::new(StructType::TR);
    tr.add_child(StructChild::StructElem(Box::new(td)));
    let mut table_elem = StructElem::new(StructType::Table);
    table_elem.add_child(StructChild::StructElem(Box::new(tr)));

    let base = crate::layout::TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: String::new(),
        bbox: Rect::new(0.0, 100.0, 0.0, 10.0),
        font_name: "Test".to_string(),
        font_size: 10.0,
        font_weight: FontWeight::Normal,
        is_italic: false,
        is_monospace: false,
        color: Color::black(),
        mcid: None,
        mcid_scope: None,
        sequence: 0,
        split_boundary_before: false,
        offset_semantic: false,
        char_spacing: 0.0,
        word_spacing: 0.0,
        horizontal_scaling: 1.0,
        primary_detected: false,
        char_widths: vec![],
        char_x_offsets: Vec::new(),
        heading_level: None,
        rotation_degrees: 0.0,
        wmode: 0,
        rtl_draw_logical: false,
    };
    // "数" ends at x=10+10=20; "≤" starts at x=23 → gap=3.0 > 1.5 → gap branch fires
    // CJK("数")→math_op("≤") with at least one CJK side → suppress space
    // "≤" ends at x=23+10=33; "量" starts at x=36 → gap=3.0 → suppress space
    let spans = vec![
        crate::layout::TextSpan {
            text: "数".into(),
            bbox: Rect::new(10.0, 100.0, 10.0, 10.0),
            mcid: Some(10),
            mcid_scope: None,
            ..base.clone()
        },
        crate::layout::TextSpan {
            text: "≤".into(),
            bbox: Rect::new(23.0, 100.0, 10.0, 10.0),
            mcid: Some(11),
            mcid_scope: None,
            ..base.clone()
        },
        crate::layout::TextSpan {
            text: "量".into(),
            bbox: Rect::new(36.0, 100.0, 10.0, 10.0),
            mcid: Some(12),
            mcid_scope: None,
            ..base.clone()
        },
    ];

    let result = extract_table_from_spans(&table_elem, &spans).unwrap();
    assert_eq!(
        result.rows[0].cells[0].text, "数≤量",
        "CJK + math-op + CJK with gap > 0.15em should not have spaces inserted: \
             got '{}'",
        result.rows[0].cells[0].text
    );
}

/// Counterpart: Latin + Latin with a gap exceeding the threshold MUST insert a space.
/// This guards that the CJK-suppression branch does not affect non-CJK pairs.
#[test]
fn test_extract_cell_latin_gap_inserts_space() {
    use crate::layout::text_block::{Color, FontWeight};

    let mut td = StructElem::new(StructType::TD);
    td.add_child(StructChild::MarkedContentRef {
        mcid: 20,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    td.add_child(StructChild::MarkedContentRef {
        mcid: 21,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut tr = StructElem::new(StructType::TR);
    tr.add_child(StructChild::StructElem(Box::new(td)));
    let mut table_elem = StructElem::new(StructType::Table);
    table_elem.add_child(StructChild::StructElem(Box::new(tr)));

    let base = crate::layout::TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: String::new(),
        bbox: Rect::new(0.0, 100.0, 0.0, 10.0),
        font_name: "Test".to_string(),
        font_size: 10.0,
        font_weight: FontWeight::Normal,
        is_italic: false,
        is_monospace: false,
        color: Color::black(),
        mcid: None,
        mcid_scope: None,
        sequence: 0,
        split_boundary_before: false,
        offset_semantic: false,
        char_spacing: 0.0,
        word_spacing: 0.0,
        horizontal_scaling: 1.0,
        primary_detected: false,
        char_widths: vec![],
        char_x_offsets: Vec::new(),
        heading_level: None,
        rotation_degrees: 0.0,
        wmode: 0,
        rtl_draw_logical: false,
    };
    // "Hello" ends at 50; "world" starts at 53 → gap=3.0 > 1.5 → space inserted
    // Neither side is CJK, so the CJK suppression must NOT fire.
    let spans = vec![
        crate::layout::TextSpan {
            text: "Hello".into(),
            bbox: Rect::new(0.0, 100.0, 50.0, 10.0),
            mcid: Some(20),
            mcid_scope: None,
            ..base.clone()
        },
        crate::layout::TextSpan {
            text: "world".into(),
            bbox: Rect::new(53.0, 100.0, 30.0, 10.0),
            mcid: Some(21),
            mcid_scope: None,
            ..base.clone()
        },
    ];

    let result = extract_table_from_spans(&table_elem, &spans).unwrap();
    assert_eq!(
        result.rows[0].cells[0].text, "Hello world",
        "Latin→Latin with gap > 0.15em should insert a space: got '{}'",
        result.rows[0].cells[0].text
    );
}
