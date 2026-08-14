use super::support::*;
use super::*;

#[test]
fn pdf_image_read_returns_an_image_content_block_over_real_stdio() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_with_text()).expect("PDF fixture");
    let mut session = Session::start_at(fixture.path());
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let response = call_tool(
        &mut session,
        2,
        "read",
        json!({
            "path": "document.pdf",
            "pdf_mode": "image",
            "pages": "1"
        }),
    );

    assert_eq!(response["result"]["isError"], false);
    let content = response["result"]["content"]
        .as_array()
        .expect("content blocks");
    assert_eq!(content.len(), 2);
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["mimeType"], "image/png");
    let encoded = content[1]["data"].as_str().expect("base64 image data");
    let png = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("valid base64");
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    session.close();
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

    let bodies: [Vec<u8>; 5] = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources \
          << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"
            .to_vec(),
        image,
        content,
    ];
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

/// `pdf_image_required` is not retryable with the same parameters, so the parameters that
/// would work have to travel with the error rather than only in its message.
#[test]
fn pdf_image_required_carries_structured_retry_parameters_over_real_stdio() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("scan.pdf"), pdf_full_page_image()).expect("PDF fixture");
    let mut session = Session::start_at(fixture.path());
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let response = call_tool(&mut session, 2, "read", json!({ "path": "scan.pdf" }));

    assert_eq!(response["result"]["isError"], true);
    let error = &response["result"]["structuredContent"]["error"];
    assert_eq!(error["code"], "pdf_image_required");
    assert_eq!(error["retryable"], false);
    let retry = &error["details"]["retry_with"][0];
    assert_eq!(retry["pdf_mode"], "image");
    assert_eq!(retry["pages"], "1");
    assert!(
        retry["pdf_cursor"]
            .as_str()
            .is_some_and(|cursor| cursor.len() == 16)
    );
    session.close();
}

/// Every successful PDF response reports the token a continuation must replay, not only
/// the offset-based single-page resume.
#[test]
fn pdf_text_read_reports_its_source_id_over_real_stdio() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_with_text()).expect("PDF fixture");
    let mut session = Session::start_at(fixture.path());
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let response = call_tool(&mut session, 2, "read", json!({ "path": "document.pdf" }));
    assert_eq!(response["result"]["isError"], false);
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text block");
    let source = text
        .lines()
        .find(|line| line.starts_with("PDF: "))
        .and_then(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("source="))
        })
        .expect("source id line");
    assert!(text.contains("mode=auto"));

    let stale = call_tool(
        &mut session,
        3,
        "read",
        json!({
            "path": "document.pdf",
            "pages": "1",
            "pdf_cursor": "0000000000000000"
        }),
    );
    assert_eq!(stale["result"]["isError"], true);

    let matching = call_tool(
        &mut session,
        4,
        "read",
        json!({
            "path": "document.pdf",
            "pages": "1",
            "pdf_cursor": source
        }),
    );
    assert_eq!(matching["result"]["isError"], false);
    session.close();
}
