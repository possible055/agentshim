use super::*;

#[test]
fn test_table_new() {
    let table = Table::new();
    assert!(table.is_empty());
    assert_eq!(table.col_count, 0);
    assert!(!table.has_header);
    assert!(table.bbox.is_none());
}

#[test]
fn test_table_bbox() {
    let mut table = Table::new();
    assert!(table.bbox.is_none());

    table.bbox = Some(Rect::new(10.0, 20.0, 100.0, 50.0));
    assert!(table.bbox.is_some());
    let bbox = table.bbox.unwrap();
    assert_eq!(bbox.x, 10.0);
    assert_eq!(bbox.y, 20.0);
    assert_eq!(bbox.width, 100.0);
    assert_eq!(bbox.height, 50.0);
}

#[test]
fn test_table_row_new() {
    let header_row = TableRow::new(true);
    assert!(header_row.is_header);
    assert!(header_row.cells.is_empty());

    let body_row = TableRow::new(false);
    assert!(!body_row.is_header);
}

#[test]
fn test_table_cell_new() {
    let cell = TableCell::new("Hello".to_string(), false);
    assert_eq!(cell.text, "Hello");
    assert!(!cell.is_header);
    assert_eq!(cell.colspan, 1);
    assert_eq!(cell.rowspan, 1);
    assert!(cell.mcids.is_empty());
}

#[test]
fn test_table_cell_with_spans() {
    let cell = TableCell::new("Data".to_string(), false)
        .with_colspan(2)
        .with_rowspan(3);

    assert_eq!(cell.colspan, 2);
    assert_eq!(cell.rowspan, 3);
}

#[test]
fn test_table_cell_header() {
    let cell = TableCell::new("Header".to_string(), true);
    assert!(cell.is_header);
}

#[test]
fn test_table_row_add_cells() {
    let mut row = TableRow::new(false);
    row.add_cell(TableCell::new("Cell1".to_string(), false));
    row.add_cell(TableCell::new("Cell2".to_string(), false));

    assert_eq!(row.cells.len(), 2);
    assert_eq!(row.cells[0].text, "Cell1");
    assert_eq!(row.cells[1].text, "Cell2");
}

#[test]
fn test_table_add_rows() {
    let mut table = Table::new();
    let mut row1 = TableRow::new(false);
    row1.add_cell(TableCell::new("A".to_string(), false));
    row1.add_cell(TableCell::new("B".to_string(), false));

    table.add_row(row1);
    assert_eq!(table.col_count, 2);
    assert_eq!(table.rows.len(), 1);
}

#[test]
fn test_table_has_header() {
    let mut table = Table::new();
    assert!(!table.has_header);

    table.has_header = true;
    assert!(table.has_header);
}

// ============================================================================
// find_table_elements() tests
// ============================================================================

/// Helper: create a minimal Table StructElem with MarkedContentRefs on a given page
fn make_table_elem(page: u32, mcids: &[u32]) -> StructElem {
    let mut table = StructElem::new(StructType::Table);
    let mut tr = StructElem::new(StructType::TR);
    for &mcid in mcids {
        let mut td = StructElem::new(StructType::TD);
        td.add_child(StructChild::MarkedContentRef {
            mcid,
            page,
            scope: crate::structure::McidScope::Page(page),
        });
        tr.add_child(StructChild::StructElem(Box::new(td)));
    }
    table.add_child(StructChild::StructElem(Box::new(tr)));
    table
}

#[test]
fn test_find_table_elements_finds_table_on_matching_page() {
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(make_table_elem(0, &[1, 2]));

    let tables = find_table_elements(&tree, 0);
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].struct_type, StructType::Table);
}

#[test]
fn test_find_table_elements_skips_table_on_different_page() {
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(make_table_elem(1, &[1, 2]));

    let tables = find_table_elements(&tree, 0);
    assert!(tables.is_empty());
}

#[test]
fn test_find_table_elements_empty_tree() {
    let tree = StructTreeRoot::new();
    let tables = find_table_elements(&tree, 0);
    assert!(tables.is_empty());
}

#[test]
fn test_find_table_elements_multiple_tables() {
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(make_table_elem(0, &[1, 2]));
    tree.add_root_element(make_table_elem(0, &[3, 4]));

    let tables = find_table_elements(&tree, 0);
    assert_eq!(tables.len(), 2);
}

#[test]
fn test_find_table_elements_nested_in_section() {
    let mut tree = StructTreeRoot::new();
    let mut sect = StructElem::new(StructType::Sect);
    sect.add_child(StructChild::StructElem(Box::new(make_table_elem(0, &[1]))));
    tree.add_root_element(sect);

    let tables = find_table_elements(&tree, 0);
    assert_eq!(tables.len(), 1);
}

#[test]
fn test_find_table_elements_table_with_page_attribute() {
    let mut tree = StructTreeRoot::new();
    let mut table = StructElem::new(StructType::Table);
    table.page = Some(2);
    // No MarkedContentRef children, but page attribute matches
    tree.add_root_element(table);

    let tables = find_table_elements(&tree, 2);
    assert_eq!(tables.len(), 1);
}

#[test]
fn test_find_table_elements_mixed_pages() {
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(make_table_elem(0, &[1]));
    tree.add_root_element(make_table_elem(1, &[2]));
    tree.add_root_element(make_table_elem(0, &[3]));

    let page0_tables = find_table_elements(&tree, 0);
    assert_eq!(page0_tables.len(), 2);

    let page1_tables = find_table_elements(&tree, 1);
    assert_eq!(page1_tables.len(), 1);
}

// ============================================================================
// find_table_elements_all_pages() — equivalence with the per-page walk
// ============================================================================

/// The single-walk all-pages bucketing must match, per page, the per-page
/// `find_table_elements` walk it replaces.
#[test]
fn test_find_table_elements_all_pages_matches_per_page() {
    let mut tree = StructTreeRoot::new();
    // page 0: two tables (one nested in a section), page 1: one table,
    // plus a page-attribute-only table on page 2.
    tree.add_root_element(make_table_elem(0, &[1, 2]));
    let mut sect = StructElem::new(StructType::Sect);
    sect.add_child(StructChild::StructElem(Box::new(make_table_elem(0, &[3]))));
    tree.add_root_element(sect);
    tree.add_root_element(make_table_elem(1, &[4]));
    let mut page_attr_table = StructElem::new(StructType::Table);
    page_attr_table.page = Some(2);
    tree.add_root_element(page_attr_table);

    let all = find_table_elements_all_pages(&tree);
    for page in 0..4u32 {
        let per_page = find_table_elements(&tree, page);
        let bucket = all.get(&page).cloned().unwrap_or_default();
        assert_eq!(
            bucket.len(),
            per_page.len(),
            "page {page}: bucket count must match per-page walk"
        );
        for (b, p) in bucket.iter().zip(per_page.iter()) {
            // same DFS pre-order ⇒ structurally identical elements
            assert_eq!(b.struct_type, p.struct_type);
            assert_eq!(b.page, p.page);
            assert_eq!(b.children.len(), p.children.len());
        }
    }
}

#[test]
fn test_find_table_elements_all_pages_empty_tree() {
    let tree = StructTreeRoot::new();
    assert!(find_table_elements_all_pages(&tree).is_empty());
}

// ============================================================================
// element_has_page_content() tests
// ============================================================================

#[test]
fn test_element_has_page_content_via_mcid() {
    let mut elem = StructElem::new(StructType::P);
    elem.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 3,
        scope: crate::structure::McidScope::Page(3),
    });

    assert!(element_has_page_content(&elem, 3));
    assert!(!element_has_page_content(&elem, 0));
}

#[test]
fn test_element_has_page_content_via_page_attribute() {
    let mut elem = StructElem::new(StructType::P);
    elem.page = Some(5);

    assert!(element_has_page_content(&elem, 5));
    assert!(!element_has_page_content(&elem, 0));
}

#[test]
fn test_element_has_page_content_recursive() {
    let mut parent = StructElem::new(StructType::Sect);
    let mut child = StructElem::new(StructType::P);
    child.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 2,
        scope: crate::structure::McidScope::Page(2),
    });
    parent.add_child(StructChild::StructElem(Box::new(child)));

    assert!(element_has_page_content(&parent, 2));
    assert!(!element_has_page_content(&parent, 0));
}

#[test]
fn test_element_has_page_content_empty() {
    let elem = StructElem::new(StructType::P);
    assert!(!element_has_page_content(&elem, 0));
}

#[test]
fn test_element_has_page_content_object_ref_ignored() {
    let mut elem = StructElem::new(StructType::P);
    elem.add_child(StructChild::ObjectRef(1, 0));
    assert!(!element_has_page_content(&elem, 0));
}

// ============================================================================
// extract_table_from_spans() tests
// ============================================================================

fn make_text_span(text: &str, mcid: Option<u32>) -> crate::layout::TextSpan {
    use crate::layout::text_block::{Color, FontWeight};

    crate::layout::TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: text.to_string(),
        bbox: Rect::new(0.0, 0.0, 50.0, 12.0),
        font_name: "Test".to_string(),
        font_size: 12.0,
        font_weight: FontWeight::Normal,
        is_italic: false,
        is_monospace: false,
        color: Color::black(),
        mcid,
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
    }
}

#[test]
fn test_extract_table_from_spans_basic() {
    // Build a simple Table > TR > [TD, TD] structure
    let mut table_elem = StructElem::new(StructType::Table);
    let mut tr = StructElem::new(StructType::TR);
    let mut td1 = StructElem::new(StructType::TD);
    td1.add_child(StructChild::MarkedContentRef {
        mcid: 10,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut td2 = StructElem::new(StructType::TD);
    td2.add_child(StructChild::MarkedContentRef {
        mcid: 11,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    tr.add_child(StructChild::StructElem(Box::new(td1)));
    tr.add_child(StructChild::StructElem(Box::new(td2)));
    table_elem.add_child(StructChild::StructElem(Box::new(tr)));

    let spans = vec![
        make_text_span("Hello", Some(10)),
        make_text_span("World", Some(11)),
        make_text_span("Unrelated", Some(99)),
    ];

    let result = extract_table_from_spans(&table_elem, &spans).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells.len(), 2);
    assert_eq!(result.rows[0].cells[0].text, "Hello");
    assert_eq!(result.rows[0].cells[1].text, "World");
}

#[test]
fn test_extract_table_from_spans_no_matching_mcids() {
    let mut table_elem = StructElem::new(StructType::Table);
    let mut tr = StructElem::new(StructType::TR);
    let mut td = StructElem::new(StructType::TD);
    td.add_child(StructChild::MarkedContentRef {
        mcid: 10,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    tr.add_child(StructChild::StructElem(Box::new(td)));
    table_elem.add_child(StructChild::StructElem(Box::new(tr)));

    // Spans have different MCIDs
    let spans = vec![make_text_span("Other", Some(99))];

    let result = extract_table_from_spans(&table_elem, &spans).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0].text, ""); // No matching content
}

#[test]
fn test_extract_table_from_spans_filters_no_mcid_spans() {
    let mut table_elem = StructElem::new(StructType::Table);
    let mut tr = StructElem::new(StructType::TR);
    let mut td = StructElem::new(StructType::TD);
    td.add_child(StructChild::MarkedContentRef {
        mcid: 5,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    tr.add_child(StructChild::StructElem(Box::new(td)));
    table_elem.add_child(StructChild::StructElem(Box::new(tr)));

    // Mix of spans with and without MCIDs
    let spans = vec![
        make_text_span("No MCID", None),
        make_text_span("Has MCID", Some(5)),
    ];

    let result = extract_table_from_spans(&table_elem, &spans).unwrap();
    assert_eq!(result.rows[0].cells[0].text, "Has MCID");
}

#[test]
fn test_extract_table_from_spans_with_thead() {
    let mut table_elem = StructElem::new(StructType::Table);

    // THead > TR > TH
    let mut thead = StructElem::new(StructType::THead);
    let mut hdr_tr = StructElem::new(StructType::TR);
    let mut th = StructElem::new(StructType::TH);
    th.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    hdr_tr.add_child(StructChild::StructElem(Box::new(th)));
    thead.add_child(StructChild::StructElem(Box::new(hdr_tr)));
    table_elem.add_child(StructChild::StructElem(Box::new(thead)));

    // TBody > TR > TD
    let mut tbody = StructElem::new(StructType::TBody);
    let mut body_tr = StructElem::new(StructType::TR);
    let mut td = StructElem::new(StructType::TD);
    td.add_child(StructChild::MarkedContentRef {
        mcid: 2,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    body_tr.add_child(StructChild::StructElem(Box::new(td)));
    tbody.add_child(StructChild::StructElem(Box::new(body_tr)));
    table_elem.add_child(StructChild::StructElem(Box::new(tbody)));

    let spans = vec![
        make_text_span("Header", Some(1)),
        make_text_span("Data", Some(2)),
    ];

    let result = extract_table_from_spans(&table_elem, &spans).unwrap();
    assert!(result.has_header);
    assert_eq!(result.rows.len(), 2);
    assert!(result.rows[0].is_header);
    assert!(!result.rows[1].is_header);
    assert_eq!(result.rows[0].cells[0].text, "Header");
    assert_eq!(result.rows[1].cells[0].text, "Data");
}

#[test]
fn test_extract_table_from_spans_empty_table() {
    let table_elem = StructElem::new(StructType::Table);
    let spans: Vec<crate::layout::TextSpan> = vec![];

    let result = extract_table_from_spans(&table_elem, &spans).unwrap();
    assert!(result.is_empty());
}
