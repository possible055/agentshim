use super::*;
use crate::object::Object;

fn pdf_doc_with_extra_object(extra: Option<&[u8]>) -> PdfDocument {
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    let mut append = |number: usize, body: &[u8]| {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    };
    append(1, b"<< /Type /Catalog /Pages 2 0 R >>");
    append(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    append(
        3,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
    );
    if let Some(body) = extra {
        append(4, body);
    }
    let xref = pdf.len();
    let object_count = offsets.len() + 1;
    pdf.extend_from_slice(format!("xref\n0 {object_count}\n0000000000 65535 f \n").as_bytes());
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {object_count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
            .as_bytes(),
    );
    PdfDocument::from_bytes(pdf).expect("open minimal PDF")
}

fn minimal_pdf_doc() -> PdfDocument {
    pdf_doc_with_extra_object(None)
}

fn image_mask_dict(width: i64, height: i64) -> HashMap<String, Object> {
    HashMap::from([
        ("Width".to_string(), Object::Integer(width)),
        ("Height".to_string(), Object::Integer(height)),
    ])
}

fn ccitt_mask_dict(width: i64, height: i64, columns: i64, rows: i64) -> HashMap<String, Object> {
    let mut dict = image_mask_dict(width, height);
    dict.insert(
        "Filter".to_string(),
        Object::Name("CCITTFaxDecode".to_string()),
    );
    dict.insert(
        "DecodeParms".to_string(),
        Object::Dictionary(HashMap::from([
            ("K".to_string(), Object::Integer(-1)),
            ("Columns".to_string(), Object::Integer(columns)),
            ("Rows".to_string(), Object::Integer(rows)),
        ])),
    );
    dict
}

mod basic;
mod pipeline;
