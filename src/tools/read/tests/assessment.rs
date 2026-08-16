use super::*;

/// One page of pure vector art, one truly blank page, and one text page.
fn pdf_vector_blank_and_text() -> Vec<u8> {
    let vector = b"0 0 1 rg\n40 40 120 120 re\nf\n0.8 0 0 RG\n4 w\n60 60 m 140 140 l\nS".to_vec();
    let blank = b"q Q".to_vec();
    let text = b"BT /F1 18 Tf 20 150 Td (Readable page) Tj ET".to_vec();
    let stream = |data: &[u8]| {
        let mut body = format!("<< /Length {} >>\nstream\n", data.len()).into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\nendstream");
        body
    };
    assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << >> \
          /Contents 6 0 R >>"
            .to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << >> \
          /Contents 7 0 R >>"
            .to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources \
          << /Font << /F1 9 0 R >> >> /Contents 8 0 R >>"
            .to_vec(),
        stream(&vector),
        stream(&blank),
        stream(&text),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    ])
}

fn page_states(text: &str) -> Vec<(usize, String)> {
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("Page states: "))
        .expect("page state line");
    line.split_whitespace()
        .filter_map(|entry| {
            let (page, state) = entry.split_once('=')?;
            Some((page.parse().ok()?, state.to_owned()))
        })
        .collect()
}

/// A vector-only page needs rendering; a blank page does not. Collapsing them into
/// one state tells the caller to render something that would come back empty, or
/// tells them a drawn page is empty.
#[test]
fn vector_and_blank_pages_get_different_states() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(
        fixture.path().join("mixed.pdf"),
        pdf_vector_blank_and_text(),
    )
    .expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    let mut probe = request("mixed.pdf");
    probe.pages = Some("1-3".to_owned());
    let text = execute(&access, &probe, &cancellation).expect("partial success");

    assert_eq!(
        page_states(&text),
        vec![(1, "image_required".to_owned()), (2, "blank".to_owned()),]
    );
    assert!(text.contains("(blank page)"));
    // Only the vector page is worth rendering, so only it gets a retry line.
    assert!(text.contains("Retry: pdf_mode=\"image\" pages=\"1\""));
    assert!(!text.contains("pages=\"2\""));
}

/// The assessment split exists so the cheap path stays cheap. Neither assessment may
/// decode a pixel or start a renderer.
#[test]
fn assessment_never_decodes_pixels_or_renders() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("mixed.pdf"), pdf_text_then_image()).expect("pdf");
    fs::write(
        fixture.path().join("shapes.pdf"),
        pdf_vector_blank_and_text(),
    )
    .expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    // Includes the all-image selection that ends in `pdf_image_required`: deciding
    // that a render is needed must itself cost no pixels.
    for (name, pages) in [
        ("mixed.pdf", "1-2"),
        ("shapes.pdf", "1-3"),
        ("shapes.pdf", "1-2"),
    ] {
        let mut probe = request(name);
        probe.pages = Some(pages.to_owned());
        let (result, metrics) =
            agentshim_pdf_read::measure(|| execute(&access, &probe, &cancellation));
        assert!(
            matches!(result, Ok(_) | Err(ReadError::PdfImageRequired { .. })),
            "{name} pages={pages} produced an unexpected outcome"
        );
        assert_eq!(
            metrics.render_pixels, 0,
            "{name} pages={pages} rasterised a page"
        );
        assert_eq!(
            metrics.png_bytes, 0,
            "{name} pages={pages} encoded an image"
        );
        assert_eq!(
            metrics.decoded_streams, 0,
            "{name} pages={pages} decoded a filtered stream"
        );
        assert_eq!(
            metrics.font_database_loads, 0,
            "{name} pages={pages} loaded the system font database"
        );
    }
}

/// A single page that cannot be processed becomes a placeholder; the readable pages
/// still come back. Failing the whole call would discard work the caller can use.
#[test]
fn one_unprocessable_page_does_not_fail_the_call() {
    let fixture = tempfile::tempdir().expect("fixture");
    let mut bytes = pdf_with_pages(3);
    // Corrupt page 2's content stream length so its extraction fails without
    // damaging the document structure the other pages need.
    let marker = b"(Page 2 body)";
    let position = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("page 2 content");
    bytes[position..position + marker.len()].copy_from_slice(b"(Page 2 body\xff");
    fs::write(fixture.path().join("damaged.pdf"), bytes).expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    let mut probe = request("damaged.pdf");
    probe.pages = Some("1-3".to_owned());
    let text = execute(&access, &probe, &cancellation).expect("partial success");

    assert!(text.contains("Page 1 body"));
    assert!(text.contains("Page 3 body"));
    assert_eq!(page_states(&text).len(), 1);
    assert_eq!(page_states(&text)[0].0, 2);
}

/// An all-image selection is the only case that becomes a typed error.
#[test]
fn only_a_wholly_unusable_selection_escalates_to_an_error() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("scan.pdf"), pdf_full_page_image()).expect("pdf");
    fs::write(
        fixture.path().join("shapes.pdf"),
        pdf_vector_blank_and_text(),
    )
    .expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    assert!(matches!(
        execute(&access, &request("scan.pdf"), &cancellation),
        Err(ReadError::PdfImageRequired { .. })
    ));

    // Vector page plus blank page: still nothing readable, still image_required.
    let mut vector_only = request("shapes.pdf");
    vector_only.pages = Some("1-2".to_owned());
    assert!(matches!(
        execute(&access, &vector_only, &cancellation),
        Err(ReadError::PdfImageRequired { .. })
    ));

    // Add the readable page and it becomes a partial success instead.
    let mut with_text = request("shapes.pdf");
    with_text.pages = Some("1-3".to_owned());
    assert!(execute(&access, &with_text, &cancellation).is_ok());
}

/// Image payloads are bounded independently of the text output budget, and the cap
/// is spent on whole pages: half an image is not a smaller image.
#[test]
fn image_payloads_are_bounded_and_only_whole_pages_are_delivered() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("long.pdf"), pdf_with_pages(6)).expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    let mut probe = request("long.pdf");
    probe.pdf_mode = Some(PdfMode::Image);
    probe.pages = Some("1-4".to_owned());
    let output = execute_output(&access, &probe, &cancellation).expect("rendered pages");

    assert!(!output.images.is_empty());
    assert!(output.images.len() <= 4, "the image page cap is 4");
    let payload: usize = output.images.iter().map(|image| image.data.len()).sum();
    assert!(
        payload <= MAX_IMAGE_BASE64_BYTES,
        "payload {payload} exceeds the base64 ceiling"
    );
    // Every delivered page is whole: each block decodes to a complete PNG.
    for image in &output.images {
        let png = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .expect("valid base64");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(png.ends_with(b"IEND\xaeB`\x82"), "truncated PNG delivered");
    }
}

/// Rendering must not change what the text path initialises or how long it takes.
#[test]
fn image_mode_does_not_change_text_mode_behaviour() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_with_text()).expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    let mut image = request("document.pdf");
    image.pdf_mode = Some(PdfMode::Image);
    execute(&access, &image, &cancellation).expect("render");

    // A text read after a render must still touch no renderer state.
    let (result, metrics) =
        agentshim_pdf_read::measure(|| execute(&access, &request("document.pdf"), &cancellation));
    result.expect("text read");
    assert_eq!(metrics.render_pixels, 0);
    assert_eq!(metrics.png_bytes, 0);
    // Scanning system fonts is the render path's most expensive setup. A text read
    // must not pay for it, before or after a render has happened.
    assert_eq!(metrics.font_database_loads, 0);
}
