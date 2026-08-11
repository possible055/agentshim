use super::*;
use std::io::Cursor;

#[test]
fn test_reconstruct_simple_pdf() {
    let pdf_data = b"%PDF-1.4\n\
            1 0 obj\n\
            << /Type /Catalog /Pages 2 0 R >>\n\
            endobj\n\
            2 0 obj\n\
            << /Type /Pages /Count 0 /Kids [] >>\n\
            endobj\n\
            trailer\n\
            << /Root 1 0 R /Size 3 >>\n\
            startxref\n\
            0\n\
            %%EOF";

    let mut cursor = Cursor::new(pdf_data);
    let result = reconstruct_xref(&mut cursor);

    assert!(result.is_ok());
    let (xref, trailer, _synthetic) = result.unwrap();

    // Should find objects 1 and 2
    assert!(xref.contains(1));
    assert!(xref.contains(2));

    // Trailer should have Root entry
    if let Some(dict) = trailer.as_dict() {
        assert!(dict.contains_key("Root"));
    } else {
        panic!("Trailer is not a dictionary");
    }
}

#[test]
fn test_is_catalog() {
    let mut dict = HashMap::new();
    dict.insert("Type".to_string(), Object::Name("Catalog".to_string()));
    let catalog = Object::Dictionary(dict);

    assert!(is_catalog(&catalog));

    let not_catalog = Object::Integer(42);
    assert!(!is_catalog(&not_catalog));
}

#[test]
fn test_reconstruct_no_objects() {
    let pdf_data = b"%PDF-1.4\n\
            This is not a valid PDF with objects\n\
            %%EOF";

    let mut cursor = Cursor::new(pdf_data);
    let result = reconstruct_xref(&mut cursor);

    assert!(result.is_err());
}
