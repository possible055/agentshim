mod corpus;

use std::io::{Seek, SeekFrom, Write};

use agentshim_pdf_read::{
    MarkdownOptions, PageTextStatus, ParserLimits, PdfReadDocument, RenderLimits,
};

fn open(bytes: &[u8]) -> PdfReadDocument {
    let mut file = tempfile::tempfile().expect("temporary corpus file");
    file.write_all(bytes).expect("write corpus fixture");
    file.seek(SeekFrom::Start(0))
        .expect("rewind corpus fixture");
    PdfReadDocument::from_file(file, ParserLimits::default()).expect("open corpus fixture")
}

fn markdown(document: &PdfReadDocument, page: usize) -> String {
    document
        .page_to_markdown(page, &MarkdownOptions::default())
        .expect("extract corpus Markdown")
}

#[test]
fn born_digital_text_extracts_every_page() {
    let document = open(&corpus::born_digital_text());

    assert_eq!(
        document.page_count().unwrap(),
        corpus::BORN_DIGITAL_PAGE_COUNT
    );
    assert_eq!(
        markdown(&document, 0),
        "## Section 1\n\nBody paragraph for section 1.\n"
    );
    assert_eq!(
        markdown(&document, 4),
        "## Section 5\n\nBody paragraph for section 5.\n"
    );
    assert_eq!(
        markdown(&document, 11),
        "## Section 12\n\nBody paragraph for section 12.\n"
    );
    assert_eq!(
        document.assess_page_text(0).unwrap().status,
        PageTextStatus::Ready
    );
}

#[test]
fn flate_compressed_text_extracts_like_its_raw_equivalent() {
    let document = open(&corpus::flate_compressed_text());

    assert_eq!(
        markdown(&document, 0),
        "## Compressed heading\n\nCompressed body text.\n"
    );
    assert_eq!(
        document.assess_page_text(0).unwrap().status,
        PageTextStatus::Ready
    );
}

/// Column cells are emitted as separate show operators, and the current extractor runs
/// them together rather than recovering a Markdown table. Pinned as-is so Phase 4's
/// bounded sink cannot change layout output without the diff showing it.
#[test]
fn table_document_extraction_is_pinned() {
    let document = open(&corpus::table_document());

    assert_eq!(
        markdown(&document, 0),
        "## Synthetic fiscal summary\n\n\
         YearRevenueCostNet Income 2021365,817212,98194,680\n\n\
         2022394,328223,54699,803\n\n\
         2023383,285214,13796,995\n\n\
         2024391,035210,35293,736\n"
    );
    assert_eq!(
        document.assess_page_text(0).unwrap().status,
        PageTextStatus::Ready
    );
}

/// The quality gate flags this page even though extraction returns usable characters.
/// `text` mode must keep returning the text and only label it.
#[test]
fn garbled_text_layer_keeps_text_but_fails_the_quality_gate() {
    let document = open(&corpus::garbled_text_layer());
    let extracted = markdown(&document, 0);

    assert_eq!(
        extracted,
        "Loremipsumdolor\n\nsitametconsecte\n\nturadipiscingel\n\nitseddoeiusmod\n"
    );
    assert!(!extracted.trim().is_empty());
    // The quality gate flags it, but the text is still returned rather than discarded.
    assert_eq!(
        document.assess_page_text(0).unwrap().status,
        PageTextStatus::Uncertain
    );
}

/// Invisible `Tr 3` text over a full-page raster: usable text and a drawn image are both
/// true at once, which one combined class could never express.
#[test]
fn hidden_text_layer_reports_both_text_and_an_image() {
    let document = open(&corpus::hidden_text_layer());

    assert_eq!(markdown(&document, 0), "Recognised text behind the scan\n");
    assert_eq!(
        document.assess_page_text(0).unwrap().status,
        PageTextStatus::Ready
    );
}

#[test]
fn full_page_image_has_no_text() {
    let document = open(&corpus::full_page_image());
    let rendered = document
        .render_page_fit(0, RenderLimits::default())
        .expect("render full-page image");

    assert_eq!(markdown(&document, 0), "");
    assert_eq!(
        document.assess_page_text(0).unwrap().status,
        PageTextStatus::Absent
    );
    assert!(document.assess_page_visual(0).unwrap().has_image_xobjects);
    assert_eq!((rendered.width_pixels, rendered.height_pixels), (832, 416));
    assert!(rendered.png.starts_with(b"\x89PNG\r\n\x1a\n"));
}

#[test]
fn mixed_document_interleaves_every_page_shape() {
    let document = open(&corpus::mixed_document());

    assert_eq!(document.page_count().unwrap(), corpus::MIXED_PAGE_COUNT);
    assert_eq!(
        markdown(&document, corpus::MIXED_TEXT_PAGE),
        "## Readable heading\n\nReadable body text.\n"
    );
    assert_eq!(markdown(&document, corpus::MIXED_IMAGE_PAGE), "");
    assert_eq!(markdown(&document, corpus::MIXED_VECTOR_PAGE), "");
    assert_eq!(markdown(&document, corpus::MIXED_BLANK_PAGE), "");
    assert_eq!(
        markdown(&document, corpus::MIXED_TEXT_OVER_IMAGE_PAGE),
        "Caption above the figure\n"
    );

    // The vector page and the blank page are no longer the same value, and neither is
    // conflated with the raster page.
    let shapes: Vec<(PageTextStatus, bool, bool)> = (0..corpus::MIXED_PAGE_COUNT)
        .map(|page| {
            let text = document.assess_page_text(page).unwrap();
            let visual = document.assess_page_visual(page).unwrap();
            (
                text.status,
                visual.has_visible_content(),
                visual.has_image_xobjects,
            )
        })
        .collect();
    assert_eq!(
        shapes,
        vec![
            (PageTextStatus::Ready, false, false),
            (PageTextStatus::Absent, true, true),
            (PageTextStatus::Absent, true, false),
            (PageTextStatus::Absent, false, false),
            (PageTextStatus::Ready, true, true),
        ]
    );
}

/// A vector-only page draws something and a blank page does not. The visual assessment
/// is what separates them; the text assessment says "absent" for both.
#[test]
fn vector_pages_are_visibly_distinct_from_blank_pages() {
    let vector = open(&corpus::vector_graphics());
    let blank = open(&corpus::blank_page());

    assert_eq!(markdown(&vector, 0), "");
    assert_eq!(markdown(&blank, 0), "");
    assert!(vector.assess_page_visual(0).unwrap().has_visible_content());
    assert!(!blank.assess_page_visual(0).unwrap().has_visible_content());

    let rendered = vector
        .render_page_fit(0, RenderLimits::default())
        .expect("render vector page");
    assert_eq!((rendered.width_pixels, rendered.height_pixels), (625, 625));
}

/// Both xref damage triggers reach `reconstruct_xref()`, which now scans in bounded
/// windows instead of buffering the whole file. Recovery must survive that change.
#[test]
fn both_broken_xref_triggers_recover() {
    let unparsable = open(&corpus::broken_xref_unparsable());
    let empty_table = open(&corpus::broken_xref_empty_table());

    assert_eq!(unparsable.page_count().unwrap(), 1);
    assert_eq!(markdown(&unparsable, 0), "Recovered after xref damage\n");
    assert_eq!(empty_table.page_count().unwrap(), 1);
    assert_eq!(markdown(&empty_table, 0), "Recovered from an empty table\n");
}

/// Declared dimensions of 100000x100000 are refused by the pre-allocation size check,
/// and the rest of the page still renders.
#[test]
fn oversized_image_dimensions_do_not_allocate() {
    let document = open(&corpus::oversized_image_dimensions());
    let rendered = document
        .render_page_fit(0, RenderLimits::default())
        .expect("render page with an oversized image");

    assert_eq!(markdown(&document, 0), "");
    assert_eq!((rendered.width_pixels, rendered.height_pixels), (625, 625));
}

/// The render pixel budget, not the DPI scale, decides the surface for a 200-inch page.
#[test]
fn oversized_media_box_is_capped_by_the_pixel_budget() {
    let document = open(&corpus::oversized_media_box());
    let info = document.page_info(0).unwrap();
    let limits = RenderLimits::default();
    let rendered = document
        .render_page_fit(0, limits)
        .expect("render oversized media box");

    assert_eq!(info.width_points, 14400.0);
    assert_eq!(markdown(&document, 0), "Wide canvas\n");
    assert_eq!(
        (rendered.width_pixels, rendered.height_pixels),
        (2000, 2000)
    );
    assert!(
        u64::from(rendered.width_pixels) * u64::from(rendered.height_pixels) <= limits.max_pixels
    );
}

/// Both Flate negatives inflate roughly a thousandfold. Without a call budget installed
/// the library stays permissive and only the 256 MiB backstop applies, which is what
/// this pins. `resource_limits.rs` covers the refusal that a budget produces.
#[test]
#[ignore = "inflates two 48 MiB Flate streams; run with --ignored"]
fn flate_bombs_pass_the_backstop_when_no_call_budget_is_installed() {
    let content_bomb = corpus::flate_bomb_content_stream();
    let image_bomb = corpus::flate_bomb_image();

    assert!(
        content_bomb.len() < 128 * 1024,
        "the compressed fixture must stay tiny to be a real bomb"
    );
    assert!(image_bomb.len() < 128 * 1024);

    // Both inflate without error. The content bomb is 48 MiB of spaces, so it parses to
    // no text at all: the point is that the expansion was accepted, not what it yielded.
    let content = open(&content_bomb);
    assert_eq!(markdown(&content, 0), "");

    let image = open(&image_bomb);
    assert_eq!(markdown(&image, 0), "");
    assert!(image.assess_page_visual(0).unwrap().has_image_xobjects);
}
