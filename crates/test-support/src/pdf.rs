use std::fmt::Write as _;

pub fn minimal_pdf(content: &[u8]) -> Vec<u8> {
    let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    stream.extend_from_slice(content);
    stream.extend_from_slice(b"\nendstream");

    assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
    ])
}

pub fn pdf_with_text() -> Vec<u8> {
    minimal_pdf(b"BT /F1 18 Tf 20 150 Td (PDF read heading) Tj ET")
}

pub fn pdf_with_pages(count: usize) -> Vec<u8> {
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

pub fn pdf_full_page_image() -> Vec<u8> {
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

pub fn pdf_text_then_image() -> Vec<u8> {
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

pub fn pdf_with_page_densities(lines_per_page: &[usize]) -> Vec<u8> {
    let count = lines_per_page.len();
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
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources \
                 << /Font << /F1 3 0 R >> >> /Contents {content_id} 0 R >>"
            )
            .into_bytes(),
        );
        let mut content = String::new();
        for line in 0..lines_per_page[index] {
            let x = 20 + (line % 10) * 58;
            let y = 780 - (line / 10) % 130 * 6;
            let _ = write!(
                content,
                "BT\n/F1 5 Tf\n1 0 0 1 {x} {y} Tm\n(page {} cell {line}) Tj\nET\n",
                index + 1
            );
        }
        let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        stream.extend_from_slice(content.as_bytes());
        stream.extend_from_slice(b"\nendstream");
        bodies.push(stream);
    }
    assemble_pdf(&bodies)
}

pub fn pdf_with_bulky_pages(count: usize, lines_per_page: usize) -> Vec<u8> {
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
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 40000] /Resources \
                 << /Font << /F1 3 0 R >> >> /Contents {content_id} 0 R >>"
            )
            .into_bytes(),
        );
        let mut content = String::from("BT\n/F1 10 Tf\n");
        for line in 0..lines_per_page {
            let y = 39_000 - line * 14;
            let _ = write!(
                content,
                "1 0 0 1 20 {y} Tm\n(Page {} line {line} with enough text to matter) Tj\n",
                index + 1
            );
        }
        content.push_str("ET");
        let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        stream.extend_from_slice(content.as_bytes());
        stream.extend_from_slice(b"\nendstream");
        bodies.push(stream);
    }
    assemble_pdf(&bodies)
}

pub fn assemble_pdf(bodies: &[Vec<u8>]) -> Vec<u8> {
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
