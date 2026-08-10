//! Text usability and visible content are separate questions with separate answers.
//!
//! The old single `PageClass` gave a vector page, a raster scan, and a garbled text page
//! the same value, which cannot route a caller correctly. These pin the split.

mod corpus;

use std::io::{Seek, SeekFrom, Write};

use codexshim_pdf_read::{
    enter_budget, measure, PageTextStatus, ParserLimits, PdfReadDocument, PdfResourceLimits,
};

fn open(bytes: &[u8]) -> PdfReadDocument {
    let mut file = tempfile::tempfile().expect("temporary corpus file");
    file.write_all(bytes).expect("write corpus fixture");
    file.seek(SeekFrom::Start(0))
        .expect("rewind corpus fixture");
    PdfReadDocument::from_file(file, ParserLimits::default()).expect("open corpus fixture")
}

#[test]
fn born_digital_text_is_ready_and_draws_nothing_else() {
    let document = open(&corpus::born_digital_text());
    let text = document.assess_page_text(0).expect("text assessment");
    let visual = document.assess_page_visual(0).expect("visual assessment");

    assert_eq!(text.status, PageTextStatus::Ready);
    assert!(text.extracted_characters > 0);
    assert_eq!(text.invalid_character_ratio, 0.0);
    assert!(!visual.has_image_xobjects);
}

/// Text quality is doubtful, but the text is still there. `text` mode must be able to
/// return it rather than being told the page has nothing.
#[test]
fn a_garbled_layer_is_uncertain_rather_than_absent() {
    let document = open(&corpus::garbled_text_layer());
    let text = document.assess_page_text(0).expect("text assessment");

    assert_eq!(text.status, PageTextStatus::Uncertain);
    assert!(text.extracted_characters > 0);
}

/// The routing distinction the old single class could not express.
#[test]
fn vector_pages_and_blank_pages_differ_in_the_visual_assessment() {
    let vector = open(&corpus::vector_graphics());
    let blank = open(&corpus::blank_page());

    let vector_text = vector.assess_page_text(0).expect("text");
    let vector_visual = vector.assess_page_visual(0).expect("visual");
    let blank_text = blank.assess_page_text(0).expect("text");
    let blank_visual = blank.assess_page_visual(0).expect("visual");

    assert_eq!(vector_text.status, PageTextStatus::Absent);
    assert_eq!(blank_text.status, PageTextStatus::Absent);

    // No text either way, so only the visual answer separates them.
    assert!(vector_visual.has_visible_content());
    assert!(!vector_visual.has_image_xobjects);
    assert!(!blank_visual.has_visible_content());
}

#[test]
fn a_full_page_raster_has_no_text_and_an_image_xobject() {
    let document = open(&corpus::full_page_image());
    let text = document.assess_page_text(0).expect("text");
    let visual = document.assess_page_visual(0).expect("visual");

    assert_eq!(text.status, PageTextStatus::Absent);
    assert!(visual.has_image_xobjects);
    assert!(visual.has_visible_content());
}

/// Invisible `Tr 3` text over a raster: text is extractable, and the page also draws.
/// Both facts are true at once, which is exactly why they are separate assessments.
#[test]
fn a_hidden_text_layer_reports_text_and_an_image() {
    let document = open(&corpus::hidden_text_layer());
    let text = document.assess_page_text(0).expect("text");
    let visual = document.assess_page_visual(0).expect("visual");

    assert!(text.extracted_characters > 0);
    assert!(visual.has_image_xobjects);
}

#[test]
fn the_mixed_document_reports_a_distinct_shape_per_page() {
    let document = open(&corpus::mixed_document());
    let shapes: Vec<(PageTextStatus, bool, bool)> = (0..corpus::MIXED_PAGE_COUNT)
        .map(|page| {
            let text = document.assess_page_text(page).expect("text");
            let visual = document.assess_page_visual(page).expect("visual");
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

/// The whole point of the split: deciding a page needs rendering must not itself decode
/// any pixels. `classify_page` called `extract_images()`, which did.
#[test]
fn assessment_decodes_no_pixels_and_starts_no_renderer() {
    for (name, bytes) in [
        ("full_page_image", corpus::full_page_image()),
        ("mixed_document", corpus::mixed_document()),
        ("vector_graphics", corpus::vector_graphics()),
        ("hidden_text_layer", corpus::hidden_text_layer()),
        ("flate_compressed_text", corpus::flate_compressed_text()),
    ] {
        let ((), metrics) = measure(|| {
            let _scope = enter_budget(PdfResourceLimits::text(), None);
            let document = open(&bytes);
            for page in 0..document.page_count().expect("page count") {
                let _ = document.assess_page_text(page);
                let _ = document.assess_page_visual(page);
            }
        });
        assert_eq!(metrics.render_pixels, 0, "{name} rasterised");
        assert_eq!(metrics.png_bytes, 0, "{name} encoded an image");
    }
}

/// `full_page_image` stores its raster unfiltered, so any decoder invocation at all
/// during assessment would mean the pixel data was read.
#[test]
fn assessing_a_raster_page_never_touches_its_stream() {
    let bytes = corpus::flate_bomb_image();
    let ((), metrics) = measure(|| {
        let _scope = enter_budget(PdfResourceLimits::text(), None);
        let document = open(&bytes);
        let visual = document.assess_page_visual(0).expect("visual");
        assert!(visual.has_image_xobjects);
    });

    assert_eq!(
        metrics.peak_decoded_stream_bytes, 0,
        "assessment inflated the image stream"
    );
}
