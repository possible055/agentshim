use super::continuation::{delivered_pages, pdf_with_page_densities};
use super::*;

#[test]
fn reads_pdf_as_markdown_or_png_without_extension_routing() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.bin"), pdf_with_text()).expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    let text = execute(&access, &request("document.bin"), &cancellation).expect("markdown");
    assert!(text.contains("PDF: pages=1/1 mode=auto source="));
    assert!(text.contains("PDF read heading"));

    let mut image_request = request("document.bin");
    image_request.pdf_mode = Some(PdfMode::Image);
    image_request.pages = Some("1".to_owned());
    let output = execute_output(&access, &image_request, &cancellation).expect("rendered image");
    assert_eq!(output.images.len(), 1);
    let png = base64::engine::general_purpose::STANDARD
        .decode(&output.images[0].data)
        .expect("base64");
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
}

#[test]
fn pdf_auto_detection_requires_the_header_at_byte_zero() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(
        fixture.path().join("ordinary.txt"),
        "ordinary text\n%PDF-1.7 is only a quoted marker\n",
    )
    .expect("text fixture");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    let text = execute(&access, &request("ordinary.txt"), &cancellation).expect("ordinary text");
    assert!(text.contains("2\t%PDF-1.7 is only a quoted marker"));

    let mut forced = request("ordinary.txt");
    forced.pdf_mode = Some(PdfMode::Text);
    assert!(matches!(
        execute(&access, &forced, &cancellation),
        Err(ReadError::Pdf(_))
    ));
}

/// `pdf_mode` must stay `Option` with no serde default. A defaulted `auto` would make
/// `has_pdf_parameters()` true for every request, routing plain text through the PDF
/// reader and charging it PDF memory instead of the 256 KiB text budget.
#[test]
fn an_absent_pdf_mode_keeps_text_reads_on_the_text_budget() {
    let parsed: ReadRequest =
        serde_json::from_str(r#"{"path":"source.txt"}"#).expect("minimal request");
    assert!(parsed.pdf_mode.is_none());
    assert!(parsed.pages.is_none());
    assert!(parsed.pdf_cursor.is_none());

    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("source.txt"), "text\n").expect("text");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    let prepared = prepare(&access, &parsed, &cancellation, budgets()).expect("prepare text");
    assert_eq!(prepared.memory_charge(), TEXT_READ_MEMORY_BYTES);
    assert_ne!(TEXT_READ_MEMORY_BYTES, DEFAULT_PDF_TEXT_MEMORY_BYTES);

    let text = execute(&access, &parsed, &cancellation).expect("plain text read");
    assert!(text.contains("1\ttext"));
    assert!(!text.contains("PDF:"));
}

#[test]
fn page_selectors_reject_zero_reversed_and_malformed_ranges() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("long.pdf"), pdf_with_pages(3)).expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    for selector in ["0", "0-2", "3-1", "1,2", "1-2-3", "", "x", "1-"] {
        let mut probe = request("long.pdf");
        probe.pages = Some(selector.to_owned());
        assert!(
            matches!(
                execute(&access, &probe, &cancellation),
                Err(ReadError::Validation(_))
            ),
            "pages={selector:?} must be rejected"
        );
    }

    let mut beyond = request("long.pdf");
    beyond.pages = Some("4".to_owned());
    assert!(matches!(
        execute(&access, &beyond, &cancellation),
        Err(ReadError::Validation(_))
    ));

    let mut single = request("long.pdf");
    single.pages = Some("2".to_owned());
    let text = execute(&access, &single, &cancellation).expect("single page");
    assert!(text.contains("PDF: pages=2/3"));
}

/// Image and text modes take different charges at prepare time, before PDF work starts.
#[test]
fn prepare_charges_image_and_text_modes_differently() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_with_text()).expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    let text = prepare(&access, &request("document.pdf"), &cancellation, budgets())
        .expect("prepare text PDF");
    let mut image = request("document.pdf");
    image.pdf_mode = Some(PdfMode::Image);
    let image = prepare(&access, &image, &cancellation, budgets()).expect("prepare image PDF");

    assert_eq!(text.pdf_mode(), Some(PdfMode::Auto));
    assert_eq!(image.pdf_mode(), Some(PdfMode::Image));
    assert_ne!(text.memory_charge(), image.memory_charge());
    assert_ne!(text.runtime_limit(), image.runtime_limit());
}

/// One page too dense to deliver must cost the caller that page and nothing else.
///
/// This is why the page ceiling is a page-scoped refusal rather than a call-scoped
/// one. Treating it as fatal — which is right for a call budget that is genuinely
/// spent — would throw away every page the caller could still read.
#[test]
fn a_page_past_the_span_ceiling_does_not_cost_the_pages_around_it() {
    let ceiling = codexshim_pdf_read::PdfResourceLimits::text().page_spans;
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(
        fixture.path().join("mixed.pdf"),
        pdf_with_page_densities(&[2, ceiling * 4, 2]),
    )
    .expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    let mut probe = request("mixed.pdf");
    probe.pages = Some("1-3".to_owned());
    let text = execute(&access, &probe, &cancellation).expect("partial success");

    assert_eq!(delivered_pages(&text), vec![1, 2, 3]);
    assert!(text.contains("page 1 cell 0"), "got {text}");
    assert!(text.contains("page 3 cell 0"), "got {text}");
    assert!(
        text.contains("unavailable"),
        "the dense page must be reported as unavailable rather than blank, got {text}"
    );
}

#[test]
fn a_selection_of_only_dense_pages_fails() {
    let ceiling = codexshim_pdf_read::PdfResourceLimits::text().page_spans;
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(
        fixture.path().join("dense.pdf"),
        pdf_with_page_densities(&[ceiling * 4]),
    )
    .expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    assert!(matches!(
        execute(&access, &request("dense.pdf"), &cancellation),
        Err(ReadError::PdfProcessing(_))
    ));
}
