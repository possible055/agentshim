use std::io::{Seek, SeekFrom, Write};

use codexshim_pdf_read::{MarkdownOptions, PageClass, ParserLimits, PdfReadDocument, RenderLimits};

fn push_object(pdf: &mut Vec<u8>, offsets: &mut [usize], id: usize, body: &[u8]) {
    offsets[id] = pdf.len();
    pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
    pdf.extend_from_slice(body);
    pdf.extend_from_slice(b"\nendobj\n");
}

fn characterization_pdf() -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = vec![0; 9];

    push_object(
        &mut pdf,
        &mut offsets,
        1,
        b"<< /Type /Catalog /Pages 2 0 R >>",
    );
    push_object(
        &mut pdf,
        &mut offsets,
        2,
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
    );
    push_object(
        &mut pdf,
        &mut offsets,
        3,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 7 0 R >> >> /Contents 5 0 R >>",
    );
    push_object(
        &mut pdf,
        &mut offsets,
        4,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /XObject << /Im0 8 0 R >> >> /Contents 6 0 R >>",
    );

    let text = b"BT /F1 18 Tf 20 150 Td (Characterization heading) Tj 0 -30 Td /F1 11 Tf (Readable body text.) Tj ET";
    push_object(
        &mut pdf,
        &mut offsets,
        5,
        format!("<< /Length {} >>\nstream\n", text.len())
            .as_bytes()
            .iter()
            .copied()
            .chain(text.iter().copied())
            .chain(b"\nendstream".iter().copied())
            .collect::<Vec<_>>()
            .as_slice(),
    );

    let image_ops = b"q 200 0 0 200 0 0 cm /Im0 Do Q";
    push_object(
        &mut pdf,
        &mut offsets,
        6,
        format!("<< /Length {} >>\nstream\n", image_ops.len())
            .as_bytes()
            .iter()
            .copied()
            .chain(image_ops.iter().copied())
            .chain(b"\nendstream".iter().copied())
            .collect::<Vec<_>>()
            .as_slice(),
    );
    push_object(
        &mut pdf,
        &mut offsets,
        7,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    );
    push_object(
        &mut pdf,
        &mut offsets,
        8,
        b"<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /Length 1 >>\nstream\n\x80\nendstream",
    );

    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 9\n0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(b"trailer\n<< /Size 9 /Root 1 0 R >>\nstartxref\n");
    pdf.extend_from_slice(format!("{xref}\n%%EOF\n").as_bytes());
    pdf
}

fn scanned_pdf() -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = vec![0; 6];
    let width = 400;
    let height = 200;

    push_object(
        &mut pdf,
        &mut offsets,
        1,
        b"<< /Type /Catalog /Pages 2 0 R >>",
    );
    push_object(
        &mut pdf,
        &mut offsets,
        2,
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    );
    push_object(
        &mut pdf,
        &mut offsets,
        3,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 200] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>",
    );

    let pixels = vec![0x80; width * height * 3];
    let mut image = format!(
        "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length {} >>\nstream\n",
        pixels.len()
    )
    .into_bytes();
    image.extend_from_slice(&pixels);
    image.extend_from_slice(b"\nendstream");
    push_object(&mut pdf, &mut offsets, 4, &image);

    let image_ops = b"q 400 0 0 200 0 0 cm /Im0 Do Q";
    let mut content = format!("<< /Length {} >>\nstream\n", image_ops.len()).into_bytes();
    content.extend_from_slice(image_ops);
    content.extend_from_slice(b"\nendstream");
    push_object(&mut pdf, &mut offsets, 5, &content);

    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(b"trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n");
    pdf.extend_from_slice(format!("{xref}\n%%EOF\n").as_bytes());
    pdf
}

fn open_pdf(bytes: &[u8]) -> PdfReadDocument {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(bytes).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    PdfReadDocument::from_file(file, ParserLimits::default()).unwrap()
}

#[test]
fn characterizes_page_count() {
    let document = open_pdf(&characterization_pdf());

    assert_eq!(document.page_count().unwrap(), 2);
}

#[test]
fn characterizes_markdown() {
    let document = open_pdf(&characterization_pdf());
    let markdown = document
        .page_to_markdown(0, &MarkdownOptions::default())
        .unwrap();

    assert!(markdown.contains("Characterization heading"));
    assert!(markdown.contains("Readable body text."));
}

#[test]
fn characterizes_page_classification() {
    let document = open_pdf(&characterization_pdf());
    let scanned_document = open_pdf(&scanned_pdf());
    let scanned = scanned_document.classify_page(0).unwrap();

    assert_eq!(document.classify_page(0).unwrap(), PageClass::TextLayer);
    assert_eq!(scanned, PageClass::Scanned);
}

#[test]
fn characterizes_rendering_and_page_info() {
    let document = open_pdf(&characterization_pdf());
    let page_info = document.page_info(0).unwrap();
    let rendered = document
        .render_page_fit(0, RenderLimits::default())
        .unwrap();

    assert_eq!(page_info.width_points, 200.0);
    assert_eq!(page_info.height_points, 200.0);
    assert_eq!((rendered.width_pixels, rendered.height_pixels), (416, 416));
    assert!(rendered.png.starts_with(b"\x89PNG\r\n\x1a\n"));
}
