use super::*;

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
    assert!(text.contains(&expected), "expected {expected} in {text}");
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
    assert!(text.contains("mode=auto"));

    let mut image = request("document.pdf");
    image.pdf_mode = Some(PdfMode::Image);
    let rendered = execute(&access, &image, &cancellation).expect("image");
    assert!(rendered.contains(&format!("source={source}")));
}

fn source_id_of(text: &str) -> String {
    text.lines()
        .find(|line| line.starts_with("PDF: "))
        .and_then(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("source="))
        })
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
    assert!(text.contains("PDF: pages=1-10/14"));
    assert!(text.contains("pages=\"11-14\""));

    let mut over_cap = request("long.pdf");
    over_cap.pages = Some("1-14".to_owned());
    execute(&access, &over_cap, &cancellation).expect("14 pages is inside the 20-page cap");

    // Wider than the document but not wider than the cap once clamped: the caller does
    // not know the page count yet, so this is how a whole document gets asked for.
    let mut past_the_end = request("long.pdf");
    past_the_end.pages = Some("1-21".to_owned());
    let text = execute(&access, &past_the_end, &cancellation).expect("clamped to the last page");
    assert!(text.contains("/14 mode=auto"));

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
        let expected_pages = if first == "3" {
            "3/3".to_owned()
        } else {
            format!("{first}-3/3")
        };
        assert!(
            text.contains(&format!("PDF: pages={expected_pages}")),
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
    assert!(output.text.contains("PDF: pages=2-3/3"));
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
    assert!(output.text.contains("PDF: pages=1/14"));
    assert!(output.text.contains("pages=\"2\""));

    let mut five = request("long.pdf");
    five.pdf_mode = Some(PdfMode::Image);
    five.pages = Some("1-5".to_owned());
    assert!(matches!(
        execute(&access, &five, &cancellation),
        Err(ReadError::Validation(_))
    ));
}

/// A page whose Markdown is large enough that only a few fit one response.
/// Pages whose text-run counts are given individually, so one page can be far denser
/// than its neighbours.
pub(super) fn pdf_with_page_densities(lines_per_page: &[usize]) -> Vec<u8> {
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

pub(super) fn delivered_pages(text: &str) -> Vec<usize> {
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
    let (result, metrics) = codexshim_pdf_read::measure(|| execute(&access, &probe, &cancellation));
    let text = result.expect("bulky read");

    let shown = delivered_pages(&text);
    assert!(
        !shown.is_empty() && shown.len() < 20,
        "the fixture must deliver some but not all pages, got {}",
        shown.len()
    );
    assert!(
        text.contains(&format!("pages=\"{}", shown.len() + 1)),
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
            let _ =
                document.page_to_markdown(page, &codexshim_pdf_read::MarkdownOptions::default());
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
    assert!(complete.contains("No remaining text at this offset."));
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
        let output = execute_output(&access, &probe, &cancellation).expect("bounded response");
        assert!(
            output.fits_content_budget(),
            "{name} produced an over-budget envelope of {} bytes",
            output.text.len()
        );
        assert!(
            output.fits_model_budget(&cancellation),
            "{name} produced an over-budget model payload"
        );
    }
}
