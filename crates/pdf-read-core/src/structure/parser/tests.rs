use super::*;

#[test]
fn parse_budget_unbounded_never_exceeds() {
    // The default (general-purpose) guard imposes no cap: a huge element
    // count must not trip it, so `parse_structure_tree` returns the whole tree.
    let guard = ParseBudget::unbounded();
    assert!(!guard.exceeded(0));
    assert!(!guard.exceeded(MAX_STRUCT_ELEMENTS + 1));
    assert!(!guard.exceeded(usize::MAX));
    // `None` budget is the unbounded guard.
    assert!(!ParseBudget::from_option(None).exceeded(usize::MAX));
}

#[test]
fn parse_budget_bounded_caps_element_count() {
    // A bounded guard (Some(duration)) still applies the companion element
    // cap, deterministically, independent of wall-clock timing.
    let guard = ParseBudget::from_option(Some(Duration::from_secs(3600)));
    assert!(!guard.exceeded(MAX_STRUCT_ELEMENTS));
    assert!(guard.exceeded(MAX_STRUCT_ELEMENTS + 1));
}

#[test]
fn test_struct_type_mapping() {
    let role_map = {
        let mut map = HashMap::new();
        map.insert("Heading1".to_string(), "H1".to_string());
        map
    };

    let mapped = role_map
        .get("Heading1")
        .map(|s| s.as_str())
        .unwrap_or("Heading1");
    assert_eq!(mapped, "H1");
}

#[test]
fn test_decode_pdf_text_string_utf8() {
    let text = b"Hello World";
    assert_eq!(decode_pdf_text_string(text), "Hello World");
}

#[test]
fn test_decode_pdf_text_string_utf16be() {
    // UTF-16BE BOM + "AB"
    let bytes = vec![0xFE, 0xFF, 0x00, 0x41, 0x00, 0x42];
    assert_eq!(decode_pdf_text_string(&bytes), "AB");
}

#[test]
fn test_decode_pdf_text_string_utf16le() {
    // UTF-16LE BOM + "AB"
    let bytes = vec![0xFF, 0xFE, 0x41, 0x00, 0x42, 0x00];
    assert_eq!(decode_pdf_text_string(&bytes), "AB");
}

#[test]
fn test_decode_pdf_text_string_pdfdoc_encoding() {
    // ASCII subset works as PDFDocEncoding
    let bytes = vec![0x48, 0x65, 0x6C, 0x6C, 0x6F]; // "Hello"
    let result = decode_pdf_text_string(&bytes);
    assert_eq!(result, "Hello");
}

#[test]
fn test_resolve_object_direct() {
    // Direct object should be returned as-is
    let obj = Object::Integer(42);
    let doc = {
        let pdf = build_test_pdf();
        PdfDocument::from_bytes(pdf).unwrap()
    };
    let result = resolve_object(&doc, &obj).unwrap();
    assert_eq!(result, Object::Integer(42));
}

fn minimal_test_doc() -> PdfDocument {
    let pdf = build_test_pdf();
    PdfDocument::from_bytes(pdf).unwrap()
}

#[test]
fn test_parse_marked_content_ref_not_dict() {
    let doc = minimal_test_doc();
    let obj = Object::Integer(5);
    let page_map = HashMap::new();
    let result = parse_marked_content_ref(&doc, &obj, &page_map).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_parse_marked_content_ref_wrong_type() {
    let doc = minimal_test_doc();
    let mut dict = HashMap::new();
    dict.insert("Type".to_string(), Object::Name("NotMCR".to_string()));
    dict.insert("MCID".to_string(), Object::Integer(5));
    let obj = Object::Dictionary(dict);
    let page_map = HashMap::new();
    let result = parse_marked_content_ref(&doc, &obj, &page_map).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_parse_marked_content_ref_missing_mcid() {
    let doc = minimal_test_doc();
    let mut dict = HashMap::new();
    dict.insert("Type".to_string(), Object::Name("MCR".to_string()));
    let obj = Object::Dictionary(dict);
    let page_map = HashMap::new();
    let result = parse_marked_content_ref(&doc, &obj, &page_map).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_parse_marked_content_ref_valid() {
    let doc = minimal_test_doc();
    let mut dict = HashMap::new();
    dict.insert("Type".to_string(), Object::Name("MCR".to_string()));
    dict.insert("MCID".to_string(), Object::Integer(7));
    let obj = Object::Dictionary(dict);
    let page_map = HashMap::new();
    let result = parse_marked_content_ref(&doc, &obj, &page_map).unwrap();
    assert!(result.is_some());
    if let Some(StructChild::MarkedContentRef { mcid, page, scope }) = result {
        assert_eq!(mcid, 7);
        assert_eq!(page, 0); // default
        assert_eq!(scope, crate::structure::McidScope::Page(0));
    }
}

#[test]
fn test_parse_marked_content_ref_with_page() {
    let doc = minimal_test_doc();
    let mut page_map = HashMap::new();
    page_map.insert(10, 2u32); // object 10 -> page 2

    let mut dict = HashMap::new();
    dict.insert("Type".to_string(), Object::Name("MCR".to_string()));
    dict.insert("MCID".to_string(), Object::Integer(3));
    dict.insert(
        "Pg".to_string(),
        Object::Reference(crate::object::ObjectRef { id: 10, gen: 0 }),
    );
    let obj = Object::Dictionary(dict);
    let result = parse_marked_content_ref(&doc, &obj, &page_map).unwrap();
    if let Some(StructChild::MarkedContentRef { mcid, page, scope }) = result {
        assert_eq!(mcid, 3);
        assert_eq!(page, 2);
        assert_eq!(scope, crate::structure::McidScope::Page(2));
    } else {
        panic!("Expected MarkedContentRef");
    }
}

#[test]
fn test_parse_structure_tree_untagged_pdf() {
    let pdf = build_test_pdf();
    let doc = PdfDocument::from_bytes(pdf).unwrap();
    let result = parse_structure_tree(&doc).unwrap();
    assert!(result.is_none()); // No StructTreeRoot in minimal PDF
}

/// Build a minimal PDF for testing
fn build_test_pdf() -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let off1 = pdf.len();
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let off2 = pdf.len();
    pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let off3 = pdf.len();
    pdf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\n",
    );

    let xref_offset = pdf.len();
    pdf.extend_from_slice(b"xref\n0 4\n");
    pdf.extend_from_slice(b"0000000000 65535 f \r\n");
    pdf.extend_from_slice(format!("{:010} 00000 n \r\n", off1).as_bytes());
    pdf.extend_from_slice(format!("{:010} 00000 n \r\n", off2).as_bytes());
    pdf.extend_from_slice(format!("{:010} 00000 n \r\n", off3).as_bytes());
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            xref_offset
        )
        .as_bytes(),
    );
    pdf
}
