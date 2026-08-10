#[cfg(test)]
mod tests {
    use std::{fmt::Write as _, fs, sync::Arc};

    use encoding_rs::{BIG5, GB18030, GBK};
    use base64::Engine as _;
    use tokio_util::sync::CancellationToken;

    use super::{
        AFTER_READ_HOOK, BEFORE_READ_HOOK, DecodeError, MAX_LINE_COUNT, MAX_IMAGE_BASE64_BYTES,
        MAX_IMAGE_PAGES, PdfMemoryBudgets, PdfMode, ReadError, ReadRequest,
        TEXT_READ_MEMORY_BYTES, execute, execute_output, prepare,
    };
    use crate::path::{FileAccess, ReadScope, RepositoryRoot};
    use crate::runtime::{DEFAULT_PDF_IMAGE_MEMORY_BYTES, DEFAULT_PDF_TEXT_MEMORY_BYTES};

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
        fs::write(fixture.path().join("latin.txt"), [0x63, 0x61, 0x66, 0xE9])
            .expect("windows-1252");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();

        let utf8 = execute(&root, &request("utf8.txt"), &cancellation).expect("read utf8");
        assert!(utf8.contains("1\talpha\n2\tbeta\nComplete."));
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
        object(
            2,
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        );
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
            let mut stream =
                format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
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
        let mut content =
            format!("<< /Length {} >>\nstream\n", operations.len()).into_bytes();
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
        let output =
            execute_output(&access, &image_request, &cancellation).expect("rendered image");
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

        let text =
            execute(&access, &request("ordinary.txt"), &cancellation).expect("ordinary text");
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
            let prepared =
                prepare(&access, &probe, &cancellation, configured).expect("prepare PDF");
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

        let prepared = prepare(&access, &request("source.txt"), &cancellation, budgets()).expect("prepare");
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

        let text = prepare(&access, &request("source.txt"), &cancellation, budgets()).expect("prepare text");
        assert_eq!(text.memory_charge(), TEXT_READ_MEMORY_BYTES);
        let pdf = prepare(&access, &request("document.bin"), &cancellation, budgets()).expect("prepare PDF");
        assert_eq!(pdf.memory_charge(), DEFAULT_PDF_TEXT_MEMORY_BYTES);
    }

    /// A page that will not fit whole is resumed by byte offset rather than truncated
    /// into something the caller cannot continue from.
    #[test]
    fn an_oversized_first_page_becomes_an_offset_continuation() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(
            fixture.path().join("bulky.pdf"),
            pdf_with_bulky_pages(2, 900),
        )
        .expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        let text = execute(&access, &request("bulky.pdf"), &cancellation).expect("bounded page");

        assert_eq!(delivered_pages(&text), vec![1]);
        assert!(
            text.contains("pdf_text_offset="),
            "an unfinished page must offer a resume offset, got {text}"
        );
        // The old behaviour ended the body with an unrecoverable marker.
        assert!(!text.contains("[page truncated]"));
        assert!(text.is_char_boundary(text.len()));
    }

    /// The next range is as wide as what was actually delivered, not as wide as what was
    /// asked for: after six of twenty pages the caller must be sent to 7-12, not 7-26.
    #[test]
    fn continuation_width_follows_delivered_pages() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(
            fixture.path().join("bulky.pdf"),
            pdf_with_bulky_pages(20, 120),
        )
        .expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        let mut probe = request("bulky.pdf");
        probe.pages = Some("1-20".to_owned());
        let text = execute(&access, &probe, &cancellation).expect("bounded pages");

        let shown = delivered_pages(&text).len();
        assert!(
            (2..20).contains(&shown),
            "the fixture must deliver some but not all pages, got {shown}"
        );
        let expected = format!("pages=\"{}-{}\"", shown + 1, shown * 2);
        assert!(
            text.contains(&expected),
            "expected {expected} in {text}"
        );
    }

    #[test]
    fn every_successful_pdf_response_carries_the_source_id() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("document.pdf"), pdf_with_text()).expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        let text = execute(&access, &request("document.pdf"), &cancellation).expect("markdown");
        let source = source_id_of(&text);
        assert_eq!(source.len(), 16);
        assert!(text.contains("Mode: auto"));

        let mut image = request("document.pdf");
        image.pdf_mode = Some(PdfMode::Image);
        let rendered = execute(&access, &image, &cancellation).expect("image");
        assert!(rendered.contains(&format!("Source: {source}")));
    }

    fn source_id_of(text: &str) -> String {
        text.lines()
            .find_map(|line| line.strip_prefix("Source: "))
            .expect("every successful PDF response reports its source id")
            .to_owned()
    }

    /// A continuation naming a different source version must fail rather than stitch two
    /// documents together.
    #[test]
    fn continuation_rejects_a_source_id_from_another_version() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("document.pdf"), pdf_with_text()).expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        let first = execute(&access, &request("document.pdf"), &cancellation).expect("markdown");
        let source = source_id_of(&first);

        let mut matching = request("document.pdf");
        matching.pages = Some("1".to_owned());
        matching.pdf_source_id = Some(source);
        execute(&access, &matching, &cancellation).expect("matching source id is accepted");

        let mut stale = request("document.pdf");
        stale.pages = Some("1".to_owned());
        stale.pdf_source_id = Some("0000000000000000".to_owned());
        assert!(matches!(
            execute(&access, &stale, &cancellation),
            Err(ReadError::Changed)
        ));
    }

    #[test]
    fn text_modes_deliver_a_first_batch_instead_of_refusing_long_documents() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("long.pdf"), pdf_with_pages(14)).expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        let text = execute(&access, &request("long.pdf"), &cancellation).expect("first batch");
        assert!(text.contains("PDF: pages 1-10 of 14"));
        assert!(text.contains("Continue with pages=\"11-14\""));

        let mut over_cap = request("long.pdf");
        over_cap.pages = Some("1-14".to_owned());
        execute(&access, &over_cap, &cancellation).expect("14 pages is inside the 20-page cap");

        // Wider than the document but not wider than the cap once clamped: the caller does
        // not know the page count yet, so this is how a whole document gets asked for.
        let mut past_the_end = request("long.pdf");
        past_the_end.pages = Some("1-21".to_owned());
        let text = execute(&access, &past_the_end, &cancellation).expect("clamped to the last page");
        assert!(text.contains(" of 14 as Markdown"));

        // The cap still bounds a selection the document can actually satisfy.
        fs::write(fixture.path().join("longer.pdf"), pdf_with_pages(25)).expect("pdf");
        let mut too_many = request("longer.pdf");
        too_many.pages = Some("1-21".to_owned());
        assert!(matches!(
            execute(&access, &too_many, &cancellation),
            Err(ReadError::Validation(_))
        ));
    }

    /// A range running past the last page is clamped; one starting past it is not, because
    /// it selects nothing at all.
    #[test]
    fn page_ranges_are_clamped_to_the_document_but_must_start_inside_it() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("short.pdf"), pdf_with_pages(3)).expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        for selector in ["1-3", "1-9", "2-9", "3-3"] {
            let text = execute(
                &access,
                &{
                    let mut probe = request("short.pdf");
                    probe.pages = Some(selector.to_owned());
                    probe
                },
                &cancellation,
            )
            .unwrap_or_else(|error| panic!("pages={selector:?} must be clamped, got {error}"));
            let first = selector.split('-').next().expect("start");
            assert!(
                text.contains(&format!("PDF: pages {first}-3 of 3")),
                "pages={selector:?} should end at the last page, got {text}"
            );
        }

        for selector in ["4", "4-9", "9-9"] {
            let mut probe = request("short.pdf");
            probe.pages = Some(selector.to_owned());
            assert!(
                matches!(
                    execute(&access, &probe, &cancellation),
                    Err(ReadError::Validation(_))
                ),
                "pages={selector:?} starts past the end and must be rejected"
            );
        }

        // Image mode clamps on the same rule, then applies its own smaller ceiling.
        let mut image = request("short.pdf");
        image.pdf_mode = Some(PdfMode::Image);
        image.pages = Some("2-9".to_owned());
        let output = execute_output(&access, &image, &cancellation).expect("clamped image range");
        assert_eq!(output.images.len(), 2);
        assert!(output.text.contains("PDF: pages 2-3 of 3"));
    }

    #[test]
    fn image_mode_without_pages_renders_only_the_first_page() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("long.pdf"), pdf_with_pages(14)).expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        let mut image = request("long.pdf");
        image.pdf_mode = Some(PdfMode::Image);
        let output = execute_output(&access, &image, &cancellation).expect("first page only");
        assert_eq!(output.images.len(), 1);
        assert!(output.text.contains("PDF: pages 1-1 of 14"));
        assert!(output.text.contains("Continue with pages=\"2\""));

        let mut five = request("long.pdf");
        five.pdf_mode = Some(PdfMode::Image);
        five.pages = Some("1-5".to_owned());
        assert!(matches!(
            execute(&access, &five, &cancellation),
            Err(ReadError::Validation(_))
        ));
    }

    #[test]
    fn a_selection_without_any_text_is_a_typed_image_required_error() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("image.pdf"), pdf_full_page_image()).expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        let error = execute(&access, &request("image.pdf"), &cancellation)
            .expect_err("an image-only page has no text");
        let ReadError::PdfImageRequired { pages, source_id } = error else {
            panic!("expected pdf_image_required");
        };
        assert_eq!(pages, "1");
        assert_eq!(source_id.len(), 16);
    }

    /// Mixed documents succeed: the readable pages are returned and the image-only page
    /// becomes a placeholder with the exact retry parameters.
    #[test]
    fn a_mixed_selection_succeeds_with_a_placeholder_and_retry_request() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("mixed.pdf"), pdf_text_then_image()).expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        let text = execute(&access, &request("mixed.pdf"), &cancellation).expect("partial success");
        assert!(text.contains("PDF read heading"));
        assert!(text.contains("Pages: 1=text_ready 2=image_required"));
        assert!(text.contains("(no extractable text; retry with pdf_mode=\"image\" and pages=\"2\")"));
        assert!(text.contains("Retry: pdf_mode=\"image\" pages=\"2\""));
    }

    /// `classify_page()` materialises image streams. Until the assessment split removes
    /// that, no text-mode read may reach it, or the path this plan exists to make cheap
    /// would decode pixels on every default call.
    #[test]
    fn text_modes_never_reach_the_image_materialising_classifier() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("mixed.pdf"), pdf_text_then_image()).expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        for mode in [None, Some(PdfMode::Auto), Some(PdfMode::Text)] {
            let mut probe = request("mixed.pdf");
            probe.pdf_mode = mode;
            let (result, metrics) =
                codexshim_pdf_read::measure(|| execute(&access, &probe, &cancellation));
            result.expect("mixed document succeeds");
            assert_eq!(metrics.render_pixels, 0, "{mode:?} rasterised a page");
            assert_eq!(
                metrics.font_database_loads, 0,
                "{mode:?} loaded the system font database"
            );
        }
    }

    /// A page whose Markdown is large enough that only a few fit one response.
    /// Pages whose text-run counts are given individually, so one page can be far denser
    /// than its neighbours.
    fn pdf_with_page_densities(lines_per_page: &[usize]) -> Vec<u8> {
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

    fn pdf_with_bulky_pages(count: usize, lines_per_page: usize) -> Vec<u8> {
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

    fn delivered_pages(text: &str) -> Vec<usize> {
        text.lines()
            .filter_map(|line| line.strip_prefix("## Page "))
            .filter_map(|number| number.parse().ok())
            .collect()
    }

    /// The plan's rule is "stop parsing when it no longer fits", not "extract everything
    /// and drop the tail". Pages past the cut must never be touched.
    #[test]
    fn pages_past_the_output_budget_are_never_extracted() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(
            fixture.path().join("bulky.pdf"),
            pdf_with_bulky_pages(20, 200),
        )
        .expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        let mut probe = request("bulky.pdf");
        probe.pages = Some("1-20".to_owned());
        let (result, metrics) =
            codexshim_pdf_read::measure(|| execute(&access, &probe, &cancellation));
        let text = result.expect("bulky read");

        let shown = delivered_pages(&text);
        assert!(
            !shown.is_empty() && shown.len() < 20,
            "the fixture must deliver some but not all pages, got {}",
            shown.len()
        );
        assert!(
            text.contains(&format!("Continue with pages=\"{}", shown.len() + 1)),
            "missing continuation in {text}"
        );

        // Compare against the cost of extracting the whole selection. One page beyond
        // the last delivered one is expected — that is the page whose arrival proved the
        // budget was full — so the ceiling is (delivered + 1) pages' worth. Anything at
        // or near the all-20 figure means the tail was parsed and then discarded.
        let path = fixture.path().join("bulky.pdf");
        let ((), whole) = codexshim_pdf_read::measure(|| {
            let document = codexshim_pdf_read::PdfReadDocument::from_file(
                std::fs::File::open(&path).expect("open fixture"),
                codexshim_pdf_read::ParserLimits::default(),
            )
            .expect("open document");
            for page in 0..20 {
                let _ = document
                    .page_to_markdown(page, &codexshim_pdf_read::MarkdownOptions::default());
            }
        });
        let per_page = whole.content_operators / 20;
        let ceiling = per_page * (shown.len() as u64 + 1);
        assert!(
            metrics.content_operators <= ceiling,
            "parsed {} operators for {} delivered pages; a stop-early read may cost at \
             most {ceiling} and extracting all 20 costs {}",
            metrics.content_operators,
            shown.len(),
            whole.content_operators
        );
        assert!(
            metrics.content_operators < whole.content_operators,
            "a stop-early read must cost strictly less than extracting every page"
        );
    }

    /// Every round must be replayable and the rounds must reassemble the page exactly.
    #[test]
    fn a_single_page_reassembles_losslessly_across_rounds() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(
            fixture.path().join("bulky.pdf"),
            pdf_with_bulky_pages(1, 900),
        )
        .expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        let mut first = request("bulky.pdf");
        first.pages = Some("1".to_owned());
        let opening = execute(&access, &first, &cancellation).expect("first round");
        let source = source_id_of(&opening);

        let mut assembled = String::new();
        let mut response = opening;
        let mut rounds = 0;
        loop {
            rounds += 1;
            assert!(rounds < 64, "continuation did not terminate");
            assembled.push_str(page_body_of(&response));
            let Some(offset) = next_offset_of(&response) else {
                break;
            };
            let mut resume = request("bulky.pdf");
            resume.pages = Some("1".to_owned());
            resume.pdf_text_offset = Some(offset);
            resume.pdf_source_id = Some(source.clone());
            response = execute(&access, &resume, &cancellation).expect("resume round");
        }

        assert!(rounds > 1, "the fixture must need more than one round");
        let whole = whole_page_markdown(fixture.path().join("bulky.pdf").as_path());
        assert_eq!(
            assembled, whole,
            "concatenated rounds must equal the page exactly"
        );
    }

    fn whole_page_markdown(path: &std::path::Path) -> String {
        let file = std::fs::File::open(path).expect("open fixture");
        let document = codexshim_pdf_read::PdfReadDocument::from_file(
            file,
            codexshim_pdf_read::ParserLimits::default(),
        )
        .expect("open document");
        document
            .page_to_markdown(0, &codexshim_pdf_read::MarkdownOptions::default())
            .expect("page markdown")
    }

    /// The body between the page heading and the trailing metadata.
    fn page_body_of(text: &str) -> &str {
        let start = text.find("## Page ").expect("page heading");
        let body_start = start + text[start..].find('\n').expect("heading newline") + 1;
        let tail = text[body_start..]
            .find("\n\nPartial:")
            .or_else(|| text[body_start..].find("\n\nComplete."))
            .unwrap_or(text.len() - body_start);
        &text[body_start..body_start + tail]
    }

    fn next_offset_of(text: &str) -> Option<usize> {
        let marker = "pdf_text_offset=";
        let start = text.find(marker)? + marker.len();
        let end = text[start..]
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len() - start);
        text[start..start + end].parse().ok()
    }

    #[test]
    fn offset_continuation_rejects_out_of_range_and_reports_completion() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("document.pdf"), pdf_with_text()).expect("pdf");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        let opening = execute(&access, &request("document.pdf"), &cancellation).expect("read");
        let source = source_id_of(&opening);
        let length = whole_page_markdown(fixture.path().join("document.pdf").as_path()).len();

        let mut beyond = request("document.pdf");
        beyond.pages = Some("1".to_owned());
        beyond.pdf_text_offset = Some(length + 1);
        beyond.pdf_source_id = Some(source.clone());
        assert!(
            execute(&access, &beyond, &cancellation).is_err(),
            "an offset past the page must be rejected"
        );

        let mut at_end = request("document.pdf");
        at_end.pages = Some("1".to_owned());
        at_end.pdf_text_offset = Some(length);
        at_end.pdf_source_id = Some(source);
        let complete = execute(&access, &at_end, &cancellation).expect("offset at end");
        assert!(complete.contains("Complete."));
    }

    /// Whatever the budget does to the response, the envelope itself must never exceed
    /// it — including the continuation metadata appended after the content.
    #[test]
    fn no_delivered_envelope_exceeds_the_output_budget() {
        let fixture = tempfile::tempdir().expect("fixture");
        let access = access(fixture.path());
        let cancellation = CancellationToken::new();

        for (pages, lines) in [(1_usize, 1_usize), (1, 900), (3, 400), (20, 200), (7, 60)] {
            let name = format!("case-{pages}-{lines}.pdf");
            fs::write(
                fixture.path().join(&name),
                pdf_with_bulky_pages(pages, lines),
            )
            .expect("pdf");
            let mut probe = request(&name);
            probe.pages = Some(format!("1-{pages}"));
            let output =
                execute_output(&access, &probe, &cancellation).expect("bounded response");
            assert!(
                output.fits_content_budget(),
                "{name} produced an over-budget envelope of {} bytes",
                output.text.len()
            );
        }
    }

    /// One page of pure vector art, one truly blank page, and one text page.
    fn pdf_vector_blank_and_text() -> Vec<u8> {
        let vector =
            b"0 0 1 rg\n40 40 120 120 re\nf\n0.8 0 0 RG\n4 w\n60 60 m 140 140 l\nS".to_vec();
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
            .find_map(|line| line.strip_prefix("Pages: "))
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
            vec![
                (1, "image_required".to_owned()),
                (2, "blank".to_owned()),
                (3, "text_ready".to_owned()),
            ]
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
                codexshim_pdf_read::measure(|| execute(&access, &probe, &cancellation));
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
        assert_eq!(page_states(&text).len(), 3);
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

    /// The contract moved from 7 MiB to 8 MiB. Pinned so the constant, the README, and
    /// the four-page division stay in step.
    #[test]
    fn the_image_payload_ceiling_is_eight_mebibytes() {
        assert_eq!(MAX_IMAGE_BASE64_BYTES, 8 * 1024 * 1024);
        assert_eq!(
            MAX_IMAGE_BASE64_BYTES / MAX_IMAGE_PAGES,
            2 * 1024 * 1024
        );
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
        let (result, metrics) = codexshim_pdf_read::measure(|| {
            execute(&access, &request("document.pdf"), &cancellation)
        });
        result.expect("text read");
        assert_eq!(metrics.render_pixels, 0);
        assert_eq!(metrics.png_bytes, 0);
        // Scanning system fonts is the render path's most expensive setup. A text read
        // must not pay for it, before or after a render has happened.
        assert_eq!(metrics.font_database_loads, 0);
    }

    #[test]
    fn continuation_parameter_combinations_are_rejected_before_io() {
        let mut offset_without_pages = request("document.pdf");
        offset_without_pages.pdf_text_offset = Some(0);
        assert!(matches!(
            offset_without_pages.validate(),
            Err(ReadError::Validation(_))
        ));

        let mut offset_with_range = request("document.pdf");
        offset_with_range.pdf_text_offset = Some(0);
        offset_with_range.pages = Some("1-3".to_owned());
        assert!(matches!(
            offset_with_range.validate(),
            Err(ReadError::Validation(_))
        ));

        let mut offset_with_image = request("document.pdf");
        offset_with_image.pdf_text_offset = Some(0);
        offset_with_image.pages = Some("2".to_owned());
        offset_with_image.pdf_mode = Some(PdfMode::Image);
        assert!(matches!(
            offset_with_image.validate(),
            Err(ReadError::Validation(_))
        ));

        let mut resume_without_source = request("document.pdf");
        resume_without_source.pdf_text_offset = Some(512);
        resume_without_source.pages = Some("2".to_owned());
        assert!(matches!(
            resume_without_source.validate(),
            Err(ReadError::Validation(_))
        ));

        let mut empty_source = request("document.pdf");
        empty_source.pdf_source_id = Some(String::new());
        assert!(matches!(
            empty_source.validate(),
            Err(ReadError::Validation(_))
        ));

        let mut valid = request("document.pdf");
        valid.pdf_text_offset = Some(512);
        valid.pages = Some("2".to_owned());
        valid.pdf_source_id = Some("abcdef0123456789".to_owned());
        valid.validate().expect("a complete resume request is valid");

        let mut zero_offset = request("document.pdf");
        zero_offset.pdf_text_offset = Some(0);
        zero_offset.pages = Some("2".to_owned());
        zero_offset
            .validate()
            .expect("a zero offset needs no source id");
    }

    #[test]
    fn auto_detects_common_chinese_encodings_conservatively() {
        let fixture = tempfile::tempdir().expect("fixture");
        let traditional_text = "繁體中文測試資料\n第二行內容足夠辨識\n";
        let simplified_text = "简体中文测试数据\n第二行内容足够识别\n";
        let gb18030_text = "简体中文扩展字符𠀀测试\n第二行内容足够识别\n";

        let (traditional, _, traditional_errors) = BIG5.encode(traditional_text);
        assert!(!traditional_errors);
        fs::write(fixture.path().join("traditional.txt"), traditional.as_ref())
            .expect("Big5 fixture");

        let (simplified, _, simplified_errors) = GBK.encode(simplified_text);
        assert!(!simplified_errors);
        fs::write(fixture.path().join("simplified.txt"), simplified.as_ref())
            .expect("GBK fixture");

        let (gb18030, _, gb18030_errors) = GB18030.encode(gb18030_text);
        assert!(!gb18030_errors);
        fs::write(fixture.path().join("gb18030.txt"), gb18030.as_ref())
            .expect("GB18030 fixture");

        let (ambiguous, _, ambiguous_errors) = BIG5.encode("中文\n");
        assert!(!ambiguous_errors);
        fs::write(fixture.path().join("ambiguous.txt"), ambiguous.as_ref())
            .expect("short Big5 fixture");

        let root = access(fixture.path());
        let cancellation = CancellationToken::new();
        let traditional = execute(&root, &request("traditional.txt"), &cancellation)
            .expect("auto-detect Big5");
        assert!(traditional.contains("Encoding: Big5\n1\t繁體中文測試資料"));

        let simplified = execute(&root, &request("simplified.txt"), &cancellation)
            .expect("auto-detect GBK");
        assert!(simplified.contains("Encoding: GBK\n1\t简体中文测试数据"));

        let gb18030 =
            execute(&root, &request("gb18030.txt"), &cancellation).expect("auto-detect GB18030");
        assert!(gb18030.contains("Encoding: GBK\n1\t简体中文扩展字符𠀀测试"));

        assert!(matches!(
            execute(&root, &request("ambiguous.txt"), &cancellation),
            Err(ReadError::Decode(DecodeError::UndetectedEncoding))
        ));
        let mut explicit = request("ambiguous.txt");
        explicit.encoding = Some("big5".to_owned());
        let explicit =
            execute(&root, &explicit, &cancellation).expect("explicit short Big5 fallback");
        assert!(explicit.contains("Encoding: Big5\n1\t中文"));
    }

    #[test]
    fn empty_long_binary_invalid_and_out_of_range_are_bounded() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("empty.txt"), "").expect("empty");
        fs::write(fixture.path().join("long.txt"), "x".repeat(100_000)).expect("long");
        fs::write(fixture.path().join("binary.bin"), b"\x89PNG\r\n\x1A\nrest").expect("binary");
        fs::write(fixture.path().join("invalid.txt"), [0xFF]).expect("invalid");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();

        let empty = execute(&root, &request("empty.txt"), &cancellation).expect("empty read");
        assert!(empty.ends_with("\nComplete."));
        let long = execute(&root, &request("long.txt"), &cancellation).expect("long read");
        assert!(long.contains("[line truncated]"));
        assert!(long.len() <= crate::output::MODEL_BYTE_LIMIT);
        assert!(matches!(
            execute(&root, &request("binary.bin"), &cancellation),
            Err(ReadError::Binary)
        ));
        assert!(matches!(
            execute(&root, &request("invalid.txt"), &cancellation),
            Err(ReadError::Decode(_))
        ));

        let mut beyond = request("empty.txt");
        beyond.start_line = Some(100);
        let output = execute(&root, &beyond, &cancellation).expect("past eof");
        assert!(output.ends_with("\nComplete."));
    }

    #[test]
    fn deep_page_skips_unretained_line_prefixes_without_changing_output() {
        let fixture = tempfile::tempdir().expect("fixture");
        let mut text = String::new();
        for line in 1..=20_000 {
            use std::fmt::Write as _;
            writeln!(text, "line-{line:05}-{}", "x".repeat(64)).expect("fixture line");
        }
        fs::write(fixture.path().join("deep.txt"), text).expect("deep fixture");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();
        let mut page = request("deep.txt");
        page.start_line = Some(19_991);
        page.line_count = Some(5);

        let output = execute(&root, &page, &cancellation).expect("deep page");
        assert!(output.contains("19991\tline-19991-"));
        assert!(output.contains("19995\tline-19995-"));
        assert!(!output.contains("19990\t"));
        assert!(output.ends_with("Partial: next_start_line=19996."));
    }

    #[test]
    fn retries_one_change_then_succeeds() {
        let fixture = tempfile::tempdir().expect("fixture");
        let path = fixture.path().join("race.txt");
        fs::write(&path, "old\n").expect("old");
        let changed = path.clone();
        BEFORE_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(&changed, "new\n").expect("change file");
            }));
        });
        let root = access(fixture.path());
        let output = execute(&root, &request("race.txt"), &CancellationToken::new())
            .expect("retry succeeds");
        assert!(output.contains("1\tnew"));
    }

    #[test]
    fn unrestricted_external_read_preserves_change_detection() {
        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        let path = outside.path().join("race.txt");
        fs::write(&path, "old\n").expect("old");
        let changed = path.clone();
        BEFORE_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(&changed, "new\n").expect("change file");
            }));
        });
        let root = access_with_scope(fixture.path(), ReadScope::Unrestricted);
        let output = execute(
            &root,
            &request(&path.to_string_lossy()),
            &CancellationToken::new(),
        )
        .expect("ambient retry succeeds");
        assert!(output.contains("1\tnew"));
    }

    #[test]
    fn second_change_fails_explicitly() {
        let fixture = tempfile::tempdir().expect("fixture");
        let path = fixture.path().join("race.txt");
        fs::write(&path, "old\n").expect("old");
        let changed = path.clone();
        AFTER_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(&changed, "first\n").expect("first change");
                let changed_again = changed.clone();
                BEFORE_READ_HOOK.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        fs::write(&changed_again, "second\n").expect("second change");
                    }));
                });
            }));
        });
        let root = access(fixture.path());
        assert!(matches!(
            execute(&root, &request("race.txt"), &CancellationToken::new()),
            Err(ReadError::Changed)
        ));
    }

    #[test]
    fn webp_magic_requires_riff_container() {
        assert!(!super::has_binary_magic(b"abcdefghWEBP source text"));
        assert!(super::has_binary_magic(b"RIFF1234WEBP"));
    }

    #[test]
    fn validation_and_directory_fail_before_content_read() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir(fixture.path().join("directory")).expect("directory");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();

        let mut invalid = request("directory");
        invalid.line_count = Some(MAX_LINE_COUNT + 1);
        assert!(matches!(
            execute(&root, &invalid, &cancellation),
            Err(ReadError::Validation(_))
        ));
        assert!(matches!(
            execute(&root, &request("directory"), &cancellation),
            Err(ReadError::Directory)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn capability_allows_internal_symlink_and_blocks_escape() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::write(fixture.path().join("target.txt"), "inside\n").expect("target");
        fs::write(outside.path().join("secret.txt"), "outside\n").expect("secret");
        symlink("target.txt", fixture.path().join("inside-link")).expect("inside link");
        symlink(
            outside.path().join("secret.txt"),
            fixture.path().join("escape-link"),
        )
        .expect("escape link");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();

        let inside =
            execute(&root, &request("inside-link"), &cancellation).expect("internal symlink read");
        assert!(inside.contains("1\tinside"));
        assert!(execute(&root, &request("escape-link"), &cancellation).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unrestricted_external_read_rejects_explicit_symlinks() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        let target = outside.path().join("target.txt");
        let link = outside.path().join("link.txt");
        fs::write(&target, "outside\n").expect("target");
        symlink(&target, &link).expect("link");
        let root = access_with_scope(fixture.path(), ReadScope::Unrestricted);
        assert!(matches!(
            execute(
                &root,
                &request(&link.to_string_lossy()),
                &CancellationToken::new()
            ),
            Err(ReadError::NotRegular)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn named_pipe_and_device_paths_are_rejected_without_blocking() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        let fixture = tempfile::tempdir().expect("fixture");
        let fifo = fixture.path().join("source.fifo");
        let fifo_bytes = CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path");
        assert_eq!(unsafe { libc::mkfifo(fifo_bytes.as_ptr(), 0o600) }, 0);
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();
        assert!(execute(&root, &request("source.fifo"), &cancellation).is_err());
        assert!(execute(&root, &request("/dev/null"), &cancellation).is_err());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an elevated Windows process to create symbolic-link fixtures"]
    fn windows_symlink_capability_allows_internal_link_and_blocks_reparse_escape() {
        use std::os::windows::fs::symlink_file;

        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::write(fixture.path().join("target.txt"), "inside\n").expect("target");
        fs::write(outside.path().join("secret.txt"), "outside\n").expect("secret");
        symlink_file("target.txt", fixture.path().join("inside-link")).expect("inside link");
        symlink_file(
            outside.path().join("secret.txt"),
            fixture.path().join("escape-link"),
        )
        .expect("escape reparse link");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();

        let inside =
            execute(&root, &request("inside-link"), &cancellation).expect("internal link read");
        assert!(inside.contains("1\tinside"));
        assert!(execute(&root, &request("escape-link"), &cancellation).is_err());

        let ambient_link = outside.path().join("ambient-link");
        symlink_file(outside.path().join("secret.txt"), &ambient_link).expect("ambient link");
        let unrestricted = access_with_scope(fixture.path(), ReadScope::Unrestricted);
        assert!(matches!(
            execute(
                &unrestricted,
                &request(&ambient_link.to_string_lossy()),
                &cancellation
            ),
            Err(ReadError::NotRegular)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_unicode_space_and_long_path_read() {
        let fixture = tempfile::tempdir().expect("fixture");
        let mut relative = std::path::PathBuf::from("Unicode space 界");
        for index in 0..12 {
            relative.push(format!("long-segment-{index:02}-xxxxxxxx"));
        }
        fs::create_dir_all(fixture.path().join(&relative)).expect("long path directories");
        relative.push("source file.rs");
        fs::write(fixture.path().join(&relative), "long path content\n").expect("long path file");
        assert!(fixture.path().join(&relative).as_os_str().len() > 260);
        let root = access(fixture.path());
        let output = execute(
            &root,
            &request(&relative.to_string_lossy()),
            &CancellationToken::new(),
        )
        .expect("long path read");
        assert!(output.contains("1\tlong path content"));
    }
}
