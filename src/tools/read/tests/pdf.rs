use super::continuation::{delivered_pages, pdf_with_page_densities};
use super::*;

#[test]
fn reads_pdf_as_markdown_or_png_without_extension_routing() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.bin"), pdf_with_text()).expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    let text = execute(&access, &request("document.bin"), &cancellation).expect("markdown");
    assert!(text.contains("PDF: pages 1-1 of 1 as Markdown"));
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
    assert!(parsed.pdf_text_offset.is_none());
    assert!(parsed.pdf_source_id.is_none());

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
    assert!(text.contains("PDF: pages 2-2 of 3"));
}

/// The mode reservation and runtime ceiling must both be known from `prepare`, before
/// any PDF work starts, because that is when admission happens.
#[test]
fn prepare_resolves_the_pdf_mode_charge_and_runtime_ceiling() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_with_text()).expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    for (mode, expected_bytes, expected_limit) in [
        (
            None,
            DEFAULT_PDF_TEXT_MEMORY_BYTES,
            crate::runtime::PDF_TEXT_RUNTIME_LIMIT,
        ),
        (
            Some(PdfMode::Auto),
            DEFAULT_PDF_TEXT_MEMORY_BYTES,
            crate::runtime::PDF_TEXT_RUNTIME_LIMIT,
        ),
        (
            Some(PdfMode::Text),
            DEFAULT_PDF_TEXT_MEMORY_BYTES,
            crate::runtime::PDF_TEXT_RUNTIME_LIMIT,
        ),
        (
            Some(PdfMode::Image),
            DEFAULT_PDF_IMAGE_MEMORY_BYTES,
            crate::runtime::PDF_IMAGE_RUNTIME_LIMIT,
        ),
    ] {
        let mut probe = request("document.pdf");
        probe.pdf_mode = mode;
        let prepared = prepare(&access, &probe, &cancellation, budgets()).expect("prepare PDF");

        assert_eq!(prepared.pdf_mode(), Some(mode.unwrap_or(PdfMode::Auto)));
        assert_eq!(prepared.runtime_limit(), Some(expected_limit));
        assert_eq!(
            prepared.memory_charge(),
            expected_bytes,
            "{mode:?} mode reservation"
        );
    }
}

/// What the scheduler charges and what the parser is held to must be one number.
///
/// Two constants that merely happen to be equal drift the moment one becomes
/// configurable — which is what a fixed core ceiling beside a configurable
/// reservation would be.
#[test]
fn the_charge_and_the_enforced_ceiling_are_the_same_number() {
    assert_eq!(
        DEFAULT_PDF_TEXT_MEMORY_BYTES,
        codexshim_pdf_read::PdfResourceLimits::text().call_total_bytes
    );
    assert_eq!(
        DEFAULT_PDF_IMAGE_MEMORY_BYTES,
        codexshim_pdf_read::PdfResourceLimits::image().call_total_bytes
    );

    // A reservation away from the default has to move the ceiling with it.
    let configured = PdfMemoryBudgets {
        text_bytes: 40 * 1024 * 1024,
        image_bytes: 72 * 1024 * 1024,
    };
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_with_text()).expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    for (mode, expected) in [
        (PdfMode::Text, configured.text_bytes),
        (PdfMode::Image, configured.image_bytes),
    ] {
        let mut probe = request("document.pdf");
        probe.pdf_mode = Some(mode);
        let prepared = prepare(&access, &probe, &cancellation, configured).expect("prepare PDF");
        assert_eq!(prepared.memory_charge(), expected, "{mode:?} charge");
        let limits = match mode {
            PdfMode::Image => codexshim_pdf_read::PdfResourceLimits::image_within(expected),
            PdfMode::Auto | PdfMode::Text => {
                codexshim_pdf_read::PdfResourceLimits::text_within(expected)
            }
        };
        assert_eq!(limits.call_total_bytes, prepared.memory_charge());
    }
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

/// A selection that is nothing but dense pages has nothing to deliver, so it fails —
/// the partial-success rule is about keeping usable pages, not about never failing.
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

/// The page ceiling is derived from the reservation, so a smaller reservation is a
/// smaller page allowance rather than the same allowance with a smaller label.
#[test]
fn the_page_span_ceiling_moves_with_the_reservation() {
    let small = codexshim_pdf_read::PdfResourceLimits::text_within(32 * 1024 * 1024);
    let default = codexshim_pdf_read::PdfResourceLimits::text();
    let large = codexshim_pdf_read::PdfResourceLimits::text_within(128 * 1024 * 1024);

    assert!(small.page_spans < default.page_spans);
    assert!(default.page_spans < large.page_spans);
    // However small the reservation, a page is always allowed some text.
    assert!(codexshim_pdf_read::PdfResourceLimits::text_within(0).page_spans > 0);
}

#[test]
fn a_text_read_never_takes_a_pdf_charge_or_runtime_ceiling() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("source.txt"), "text\n").expect("text");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    let prepared =
        prepare(&access, &request("source.txt"), &cancellation, budgets()).expect("prepare");
    assert_eq!(prepared.pdf_mode(), None);
    assert_eq!(prepared.runtime_limit(), None);
    assert_eq!(prepared.memory_charge(), TEXT_READ_MEMORY_BYTES);
}

#[test]
fn prepares_memory_charge_from_content_before_execution() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("source.txt"), "text\n").expect("text");
    fs::write(fixture.path().join("document.bin"), pdf_with_text()).expect("pdf");
    let access = access(fixture.path());
    let cancellation = CancellationToken::new();

    let text =
        prepare(&access, &request("source.txt"), &cancellation, budgets()).expect("prepare text");
    assert_eq!(text.memory_charge(), TEXT_READ_MEMORY_BYTES);
    let pdf =
        prepare(&access, &request("document.bin"), &cancellation, budgets()).expect("prepare PDF");
    assert_eq!(pdf.memory_charge(), DEFAULT_PDF_TEXT_MEMORY_BYTES);
}
