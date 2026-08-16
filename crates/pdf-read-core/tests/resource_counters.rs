//! Every hard limit planned for the call-level budget needs an observable counter
//! before a threshold can be justified. These tests keep the counters wired; the
//! measured values themselves are reported by `measure_corpus_counters`.

mod corpus;

use std::io::{Seek, SeekFrom, Write};

use agentshim_pdf_read::{
    measure, MarkdownOptions, ParserLimits, PdfReadDocument, PdfReadMetrics, RenderLimits,
};

fn open(bytes: &[u8]) -> PdfReadDocument {
    let mut file = tempfile::tempfile().expect("temporary corpus file");
    file.write_all(bytes).expect("write corpus fixture");
    file.seek(SeekFrom::Start(0))
        .expect("rewind corpus fixture");
    PdfReadDocument::from_file(file, ParserLimits::default()).expect("open corpus fixture")
}

fn text_pass(bytes: &[u8]) -> PdfReadMetrics {
    let bytes = bytes.to_vec();
    measure(|| {
        let document = open(&bytes);
        let pages = document.page_count().expect("page count");
        for page in 0..pages {
            let _ = document.page_to_markdown(page, &MarkdownOptions::default());
        }
    })
    .1
}

fn render_pass(bytes: &[u8]) -> PdfReadMetrics {
    let bytes = bytes.to_vec();
    measure(|| {
        let document = open(&bytes);
        let _ = document.render_page_fit(0, RenderLimits::default());
    })
    .1
}

#[test]
fn text_extraction_reports_decode_cache_and_operator_counters() {
    let metrics = text_pass(&corpus::born_digital_text());

    assert!(
        metrics.cached_objects > 0,
        "object cache counter is unwired"
    );
    assert!(
        metrics.peak_object_cache_bytes > 0,
        "object cache byte counter is unwired"
    );
    assert!(
        metrics.content_operators > 0,
        "content operator counter is unwired"
    );
}

#[test]
fn rendering_reports_pixel_and_png_counters() {
    let metrics = render_pass(&corpus::full_page_image());

    assert_eq!(metrics.render_pixels, 832 * 416);
    assert!(metrics.png_bytes > 0, "PNG byte counter is unwired");
}

#[test]
fn filtered_streams_report_decoder_counters() {
    let metrics = text_pass(&corpus::flate_compressed_text());

    assert!(metrics.decoded_streams > 0, "decoder counter is unwired");
    assert!(
        metrics.decoded_stream_bytes >= metrics.peak_decoded_stream_bytes,
        "total decoded bytes must cover the peak single stream"
    );
    assert!(metrics.peak_decoded_stream_bytes > 0);
}

/// The counter that Phase 3's per-stream cap will be enforced on must actually see the
/// inflated size, not the compressed one.
#[test]
#[ignore = "inflates a 48 MiB Flate stream; run with --ignored"]
fn decoder_counter_observes_the_inflated_size() {
    let metrics = text_pass(&corpus::flate_bomb_content_stream());

    assert!(
        metrics.peak_decoded_stream_bytes >= corpus::FLATE_BOMB_DECODED_BYTES as u64,
        "peak decoded stream was {} bytes, expected at least {}",
        metrics.peak_decoded_stream_bytes,
        corpus::FLATE_BOMB_DECODED_BYTES
    );
}

#[test]
fn measure_scopes_do_not_leak_between_calls() {
    let first = text_pass(&corpus::born_digital_text());
    let second = measure(|| {}).1;

    assert!(first.content_operators > 0);
    assert_eq!(second, PdfReadMetrics::default());
}

/// Prints the per-fixture counters used to derive Phase 3 thresholds. Reported rather
/// than asserted: the numbers are inputs to a threshold decision, not a contract.
#[test]
#[ignore = "measurement report; run with --ignored --nocapture"]
fn measure_corpus_counters() {
    let cases: Vec<(&str, Vec<u8>, bool)> = vec![
        ("born_digital_text", corpus::born_digital_text(), false),
        (
            "flate_compressed_text",
            corpus::flate_compressed_text(),
            false,
        ),
        ("table_document", corpus::table_document(), false),
        ("garbled_text_layer", corpus::garbled_text_layer(), false),
        ("hidden_text_layer", corpus::hidden_text_layer(), false),
        ("full_page_image", corpus::full_page_image(), true),
        ("mixed_document", corpus::mixed_document(), true),
        ("vector_graphics", corpus::vector_graphics(), true),
        ("blank_page", corpus::blank_page(), true),
        (
            "broken_xref_unparsable",
            corpus::broken_xref_unparsable(),
            false,
        ),
        (
            "broken_xref_empty_table",
            corpus::broken_xref_empty_table(),
            false,
        ),
        (
            "oversized_image_dimensions",
            corpus::oversized_image_dimensions(),
            true,
        ),
        ("oversized_media_box", corpus::oversized_media_box(), true),
    ];

    println!(
        "{:<28} {:>10} {:>12} {:>12} {:>10} {:>9} {:>12} {:>10}",
        "fixture", "streams", "decoded", "peak_stream", "cache", "objects", "operators", "pixels"
    );
    for (name, bytes, render) in cases {
        let mut metrics = text_pass(&bytes);
        if render {
            let rendered = render_pass(&bytes);
            metrics.render_pixels = rendered.render_pixels;
            metrics.png_bytes = rendered.png_bytes;
        }
        println!(
            "{:<28} {:>10} {:>12} {:>12} {:>10} {:>9} {:>12} {:>10}",
            name,
            metrics.decoded_streams,
            metrics.decoded_stream_bytes,
            metrics.peak_decoded_stream_bytes,
            metrics.peak_object_cache_bytes,
            metrics.cached_objects,
            metrics.content_operators,
            metrics.render_pixels
        );
    }
}
