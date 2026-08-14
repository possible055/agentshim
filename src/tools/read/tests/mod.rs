use std::{fmt::Write as _, fs, sync::Arc};

use base64::Engine as _;
use encoding_rs::{BIG5, GB18030, GBK};
use tokio_util::sync::CancellationToken;

use crate::path::{FileAccess, ReadScope, RepositoryRoot};
use crate::runtime::DEFAULT_PDF_TEXT_MEMORY_BYTES;
use crate::tools::read::{
    AFTER_READ_HOOK, BEFORE_READ_HOOK, DecodeError, MAX_IMAGE_BASE64_BYTES, MAX_LINE_COUNT,
    PdfMemoryBudgets, PdfMode, ReadError, ReadRequest, TEXT_READ_MEMORY_BYTES, execute,
    execute_output, prepare,
};

fn budgets() -> PdfMemoryBudgets {
    PdfMemoryBudgets::defaults()
}

fn access(path: &std::path::Path) -> Arc<FileAccess> {
    access_with_scope(path, ReadScope::Normal)
}

fn access_with_scope(path: &std::path::Path, scope: ReadScope) -> Arc<FileAccess> {
    Arc::new(FileAccess::new(
        Arc::new(RepositoryRoot::open(path).expect("root")),
        scope,
    ))
}

fn request(path: &str) -> ReadRequest {
    ReadRequest {
        path: path.to_owned(),
        start_line: None,
        line_count: None,
        encoding: None,
        pdf_mode: None,
        pages: None,
        pdf_text_offset: None,
        pdf_source_id: None,
    }
}

#[test]
fn reads_numbered_utf8_crlf_and_utf16_pages() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("utf8.txt"), "alpha\r\nbeta\n").expect("utf8");
    fs::write(fixture.path().join("utf8-bom.txt"), b"\xEF\xBB\xBFbom\n").expect("utf8 bom");
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in "one\ntwo\nthree".encode_utf16() {
        utf16.extend(unit.to_le_bytes());
    }
    fs::write(fixture.path().join("utf16.txt"), utf16).expect("utf16");
    let mut utf16be = vec![0xFE, 0xFF];
    for unit in "big\nend".encode_utf16() {
        utf16be.extend(unit.to_be_bytes());
    }
    fs::write(fixture.path().join("utf16be.txt"), utf16be).expect("utf16be");
    fs::write(fixture.path().join("latin.txt"), [0x63, 0x61, 0x66, 0xE9]).expect("windows-1252");
    let root = access(fixture.path());
    let cancellation = CancellationToken::new();

    let utf8 = execute(&root, &request("utf8.txt"), &cancellation).expect("read utf8");
    assert_eq!(utf8, "1\talpha\n2\tbeta");
    assert!(!utf8.contains("Path:"));
    let bom = execute(&root, &request("utf8-bom.txt"), &cancellation).expect("utf8 bom");
    assert!(bom.contains("1\tbom"));

    let mut page = request("utf16.txt");
    page.start_line = Some(2);
    page.line_count = Some(1);
    let utf16 = execute(&root, &page, &cancellation).expect("read utf16");
    assert!(utf16.contains("Encoding: UTF-16LE\n2\ttwo"));
    assert!(utf16.ends_with("Partial: next_start_line=3."));
    let be = execute(&root, &request("utf16be.txt"), &cancellation).expect("utf16be");
    assert!(be.contains("Encoding: UTF-16BE\n1\tbig"));
    let mut latin = request("latin.txt");
    latin.encoding = Some("windows-1252".to_owned());
    let latin = execute(&root, &latin, &cancellation).expect("explicit encoding");
    assert!(latin.contains("Encoding: windows-1252\n1\tcafé"));
}

fn pdf_with_text() -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = [0_usize; 6];
    let mut object = |id: usize, body: &[u8]| {
        offsets[id] = pdf.len();
        pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    };
    object(1, b"<< /Type /Catalog /Pages 2 0 R >>");
    object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    object(
        3,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
    );
    let content = b"BT /F1 18 Tf 20 150 Td (PDF read heading) Tj ET";
    let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    stream.extend_from_slice(content);
    stream.extend_from_slice(b"\nendstream");
    object(4, &stream);
    object(
        5,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    );
    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(b"trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n");
    pdf.extend_from_slice(format!("{xref}\n%%EOF\n").as_bytes());
    pdf
}

/// Minimal multi-page document. Object ids are assigned as
/// catalog, page tree, font, then one page and one content stream per page.
fn pdf_with_pages(count: usize) -> Vec<u8> {
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    let page_ids: Vec<usize> = (0..count).map(|index| 4 + index * 2).collect();
    let kids = page_ids
        .iter()
        .map(|id| format!("{id} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    bodies.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    bodies.push(format!("<< /Type /Pages /Kids [{kids}] /Count {count} >>").into_bytes());
    bodies.push(
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    );
    for (index, page_id) in page_ids.iter().enumerate() {
        let content_id = page_id + 1;
        bodies.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources \
                 << /Font << /F1 3 0 R >> >> /Contents {content_id} 0 R >>"
            )
            .into_bytes(),
        );
        let content = format!("BT /F1 18 Tf 20 150 Td (Page {} body) Tj ET", index + 1);
        let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        stream.extend_from_slice(content.as_bytes());
        stream.extend_from_slice(b"\nendstream");
        bodies.push(stream);
    }
    assemble_pdf(&bodies)
}

fn pdf_full_page_image() -> Vec<u8> {
    let pixels = vec![0x80_u8; 80 * 80 * 3];
    let mut image = format!(
        "<< /Type /XObject /Subtype /Image /Width 80 /Height 80 /ColorSpace /DeviceRGB \
         /BitsPerComponent 8 /Length {} >>\nstream\n",
        pixels.len()
    )
    .into_bytes();
    image.extend_from_slice(&pixels);
    image.extend_from_slice(b"\nendstream");
    let operations = b"q 200 0 0 200 0 0 cm /Im0 Do Q";
    let mut content = format!("<< /Length {} >>\nstream\n", operations.len()).into_bytes();
    content.extend_from_slice(operations);
    content.extend_from_slice(b"\nendstream");

    assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources \
          << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"
            .to_vec(),
        image,
        content,
    ])
}

fn pdf_text_then_image() -> Vec<u8> {
    let pixels = vec![0x80_u8; 80 * 80 * 3];
    let mut image = format!(
        "<< /Type /XObject /Subtype /Image /Width 80 /Height 80 /ColorSpace /DeviceRGB \
         /BitsPerComponent 8 /Length {} >>\nstream\n",
        pixels.len()
    )
    .into_bytes();
    image.extend_from_slice(&pixels);
    image.extend_from_slice(b"\nendstream");

    let text_operations = b"BT /F1 18 Tf 20 150 Td (PDF read heading) Tj ET";
    let mut text_content =
        format!("<< /Length {} >>\nstream\n", text_operations.len()).into_bytes();
    text_content.extend_from_slice(text_operations);
    text_content.extend_from_slice(b"\nendstream");

    let image_operations = b"q 200 0 0 200 0 0 cm /Im0 Do Q";
    let mut image_content =
        format!("<< /Length {} >>\nstream\n", image_operations.len()).into_bytes();
    image_content.extend_from_slice(image_operations);
    image_content.extend_from_slice(b"\nendstream");

    assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources \
          << /Font << /F1 7 0 R >> >> /Contents 5 0 R >>"
            .to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources \
          << /XObject << /Im0 8 0 R >> >> /Contents 6 0 R >>"
            .to_vec(),
        text_content,
        image_content,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
        image,
    ])
}

fn assemble_pdf(bodies: &[Vec<u8>]) -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = vec![0_usize; bodies.len() + 1];
    for (index, body) in bodies.iter().enumerate() {
        let id = index + 1;
        offsets[id] = pdf.len();
        pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let size = bodies.len() + 1;
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n").as_bytes(),
    );
    pdf.extend_from_slice(format!("{xref}\n%%EOF\n").as_bytes());
    pdf
}

mod assessment;
mod continuation;
mod pdf;
mod text;
