use std::io::{Seek, SeekFrom, Write};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agentshim_pdf_read::{
    enter_budget, MarkdownOptions, ParserLimits, PdfReadDocument, PdfReadError, PdfReadErrorKind,
    PdfResourceLimits, RenderLimits,
};

use super::corpus;

fn try_open(bytes: &[u8]) -> Result<PdfReadDocument, PdfReadError> {
    let mut file = tempfile::tempfile().expect("temporary corpus file");
    file.write_all(bytes).expect("write corpus fixture");
    file.seek(SeekFrom::Start(0))
        .expect("rewind corpus fixture");
    PdfReadDocument::from_file(file, ParserLimits::default())
}

fn open(bytes: &[u8]) -> PdfReadDocument {
    try_open(bytes).expect("open corpus fixture")
}

/// Opening parses the cross-reference table, so the budget has to be installed around
/// `from_file` as well — reconstruction happens there, not on the first page read.
fn read_under(limits: PdfResourceLimits, bytes: &[u8]) -> Result<(), PdfReadError> {
    let _scope = enter_budget(limits, None);
    let document = try_open(bytes)?;
    for page in 0..document.page_count()? {
        document.page_to_markdown(page, &MarkdownOptions::default())?;
    }
    Ok(())
}

fn read_all_pages(document: &PdfReadDocument) -> Result<(), PdfReadError> {
    for page in 0..document.page_count()? {
        document.page_to_markdown(page, &MarkdownOptions::default())?;
    }
    Ok(())
}

/// The former 64 MiB object cache default was the entire text-mode reservation, so the
/// cache alone could consume the budget the whole call has to fit inside.
#[test]
fn the_object_cache_default_fits_inside_the_call_budget() {
    let limits = ParserLimits::default();
    let text = PdfResourceLimits::text();

    assert_eq!(limits.object_cache_bytes, text.object_cache_bytes);
    assert!(limits.object_cache_bytes < text.call_total_bytes);
    assert_eq!(limits.object_cache_bytes, 16 * 1024 * 1024);
}

/// Both Flate negatives inflate about a thousandfold. Under a call budget they must be
/// refused before the allocation, naming the budget that refused them.
#[test]
fn flate_bombs_are_refused_by_the_per_stream_ceiling() {
    let error = read_under(
        PdfResourceLimits::text(),
        &corpus::flate_bomb_content_stream(),
    )
    .expect_err("a content-stream bomb must be refused on the text path");
    assert_eq!(error.kind(), PdfReadErrorKind::ResourceLimit);
    let limit = error.limit().expect("structured limit details");
    assert_eq!(limit.resource, "pdf_single_stream");
    assert_eq!(
        limit.limit_bytes,
        PdfResourceLimits::text().single_stream_bytes as u64
    );
    assert_eq!(limit.observed_bytes, limit.limit_bytes);

    let (_, metrics) = agentshim_pdf_read::measure(|| {
        let _scope = enter_budget(PdfResourceLimits::image(), None);
        let document = open(&corpus::flate_bomb_image());
        let _ = document.render_page_fit(0, RenderLimits::default());
    });
    assert_eq!(
        metrics.peak_decoded_stream_bytes, 0,
        "the image bomb must never reach an allocation"
    );
}

/// Image mode allows a larger single stream than text mode; the same fixture must be
/// judged against whichever budget is installed.
#[test]
fn the_installed_mode_decides_the_stream_ceiling() {
    let bytes = corpus::flate_bomb_content_stream();

    let text_limit = read_under(PdfResourceLimits::text(), &bytes)
        .expect_err("text mode refuses")
        .limit()
        .expect("details");
    let image_limit = read_under(PdfResourceLimits::image(), &bytes)
        .expect_err("image mode refuses")
        .limit()
        .expect("details");

    assert_eq!(
        text_limit.limit_bytes,
        PdfResourceLimits::text().single_stream_bytes as u64
    );
    assert_eq!(
        image_limit.limit_bytes,
        PdfResourceLimits::image().single_stream_bytes as u64
    );
    assert!(image_limit.limit_bytes > text_limit.limit_bytes);
}

/// The environment override was a switch for turning the resource contract off from
/// outside the process. Setting it must change nothing.
#[test]
fn the_decompression_environment_override_has_no_effect() {
    let executable = std::env::current_exe().expect("resource limit test executable");
    for value in ["4096", "1", "not_a_number"] {
        let output = Command::new(&executable)
            .args([
                "--exact",
                "resource_limits::the_decompression_environment_override_child_fixture",
            ])
            .env("PDF_OXIDE_MAX_DECOMPRESS_MB", value)
            .output()
            .expect("run environment override child");
        assert!(
            output.status.success(),
            "PDF_OXIDE_MAX_DECOMPRESS_MB={value} changed the effective ceiling:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn the_decompression_environment_override_child_fixture() {
    if std::env::var_os("PDF_OXIDE_MAX_DECOMPRESS_MB").is_none() {
        return;
    }
    let bytes = corpus::flate_bomb_content_stream();
    let observed = read_under(PdfResourceLimits::text(), &bytes)
        .expect_err("bomb refused")
        .limit()
        .expect("details");
    assert_eq!(
        observed.limit_bytes,
        PdfResourceLimits::text().single_stream_bytes as u64
    );
}

/// Both xref damage triggers reach reconstruction, which used to read the whole file
/// into a second buffer. They must still recover with only a windowed scan available.
#[test]
fn xref_reconstruction_recovers_within_a_bounded_buffer() {
    for (name, bytes) in [
        ("unparsable startxref", corpus::broken_xref_unparsable()),
        ("empty table", corpus::broken_xref_empty_table()),
    ] {
        let _scope = enter_budget(PdfResourceLimits::text(), None);
        let document =
            try_open(&bytes).unwrap_or_else(|error| panic!("{name} must still open: {error}"));
        let markdown = document
            .page_to_markdown(0, &MarkdownOptions::default())
            .unwrap_or_else(|error| panic!("{name} must still recover: {error}"));
        assert!(!markdown.trim().is_empty(), "{name}");
    }
}

/// A budget whose rebuild buffer cannot hold one scan window refuses reconstruction
/// instead of silently falling back to reading the file whole.
#[test]
fn a_rebuild_buffer_below_one_window_refuses_reconstruction() {
    let cramped = PdfResourceLimits {
        xref_rebuild_bytes: 1024,
        ..PdfResourceLimits::text()
    };
    let error = read_under(cramped, &corpus::broken_xref_unparsable())
        .expect_err("a cramped rebuild buffer must be reported, not worked around");
    assert_eq!(error.kind(), PdfReadErrorKind::ResourceLimit);
    assert_eq!(error.limit().expect("details").resource, "pdf_xref_rebuild");
}

/// Ordinary corpus documents must stay well inside the reservation, or the limit would
/// be rejecting valid input rather than abuse.
#[test]
fn representative_corpus_stays_inside_the_text_reservation() {
    for (name, bytes) in [
        ("born_digital_text", corpus::born_digital_text()),
        ("flate_compressed_text", corpus::flate_compressed_text()),
        ("table_document", corpus::table_document()),
        ("garbled_text_layer", corpus::garbled_text_layer()),
        ("hidden_text_layer", corpus::hidden_text_layer()),
        ("mixed_document", corpus::mixed_document()),
        ("full_page_image", corpus::full_page_image()),
        ("vector_graphics", corpus::vector_graphics()),
        ("blank_page", corpus::blank_page()),
        ("oversized_media_box", corpus::oversized_media_box()),
    ] {
        read_under(PdfResourceLimits::text(), &bytes).unwrap_or_else(|error| {
            panic!("{name} must fit the text reservation: {error}");
        });
    }
}

#[test]
fn rendering_stays_inside_the_image_reservation() {
    for (name, bytes) in [
        ("full_page_image", corpus::full_page_image()),
        ("vector_graphics", corpus::vector_graphics()),
        ("oversized_media_box", corpus::oversized_media_box()),
        (
            "oversized_image_dimensions",
            corpus::oversized_image_dimensions(),
        ),
    ] {
        let _scope = enter_budget(PdfResourceLimits::image(), None);
        let document = open(&bytes);
        document
            .render_page_fit(0, RenderLimits::default())
            .unwrap_or_else(|error| panic!("{name} must fit the image reservation: {error}"));
    }
}

/// A surface budget below what the page needs must refuse before the pixels are
/// allocated, not after.
#[test]
fn an_oversized_surface_is_refused_before_allocation() {
    let cramped = PdfResourceLimits {
        render_surface_bytes: 64 * 1024,
        ..PdfResourceLimits::image()
    };
    let _scope = enter_budget(cramped, None);
    let document = open(&corpus::oversized_media_box());

    let error = document
        .render_page_fit(0, RenderLimits::default())
        .expect_err("the surface budget must refuse this page");
    assert_eq!(error.kind(), PdfReadErrorKind::ResourceLimit);
    assert_eq!(
        error.limit().expect("details").resource,
        "pdf_render_surface"
    );
}

/// Every path that scans the file for object headers is reachable from ordinary damaged
/// input, and each one used to buffer the whole document. A rebuild budget smaller than
/// the file proves they now read in windows: if any of them still buffered whole, the
/// budget check at window setup could not have let it through.
#[test]
fn recovery_scans_never_buffer_the_whole_source() {
    let bulky = {
        let mut builder = corpus::PdfBuilder::new();
        let catalog = builder.reserve();
        let page_tree = builder.reserve();
        let page = builder.reserve();
        let font = builder.add(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
        );
        builder.add_stream("", &vec![b'%'; 3 * 1024 * 1024]);
        let content = builder.add_stream(
            "",
            b"BT\n/F1 18 Tf\n1 0 0 1 20 250 Tm\n(Recovered from a large file) Tj\nET",
        );
        builder.define(
            catalog,
            format!("<< /Type /Catalog /Pages {page_tree} 0 R >>"),
        );
        builder.define(
            page_tree,
            format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"),
        );
        builder.define(
            page,
            format!(
                "<< /Type /Page /Parent {page_tree} 0 R /MediaBox [0 0 300 300] /Resources \
                 << /Font << /F1 {font} 0 R >> >> /Contents {content} 0 R >>"
            ),
        );
        builder.build_with(catalog, corpus::XrefStyle::UnparsableStartxref)
    };
    assert!(bulky.len() > 3 * 1024 * 1024);

    let windowed = PdfResourceLimits {
        xref_rebuild_bytes: 1024 * 1024,
        ..PdfResourceLimits::text()
    };
    let _scope = enter_budget(windowed, None);
    let document = try_open(&bulky).expect("a damaged large file still opens");
    let markdown = document
        .page_to_markdown(0, &MarkdownOptions::default())
        .expect("and still reads");
    assert!(markdown.contains("Recovered from a large file"));
}

/// Declared geometry is attacker-controlled and sizes buffers, so it must be refused
/// before anything is allocated from it — not after a 30 GB request fails.
#[test]
fn malicious_image_dimensions_never_reach_an_allocation() {
    let ((), metrics) = agentshim_pdf_read::measure(|| {
        let _scope = enter_budget(PdfResourceLimits::image(), None);
        let document = open(&corpus::oversized_image_dimensions());
        let rendered = document
            .render_page_fit(0, RenderLimits::default())
            .expect("the page renders without the impossible image");
        assert_eq!((rendered.width_pixels, rendered.height_pixels), (625, 625));
    });

    assert_eq!(
        metrics.peak_decoded_stream_bytes, 0,
        "an impossible image must not be decoded"
    );
    assert!(metrics.render_pixels <= 625 * 625);
}

/// The pixel and edge ceilings are separate: an extreme aspect ratio can stay under the
/// pixel cap while still overflowing a row-stride computation.
#[test]
fn image_pixel_and_edge_ceilings_are_both_enforced() {
    use agentshim_pdf_read::{MAX_IMAGE_EDGE_PIXELS, MAX_IMAGE_PIXELS};

    assert!(u64::from(MAX_IMAGE_EDGE_PIXELS) * u64::from(MAX_IMAGE_EDGE_PIXELS) > MAX_IMAGE_PIXELS);
    assert!(u64::from(MAX_IMAGE_EDGE_PIXELS + 1) < MAX_IMAGE_PIXELS);
}

/// Cancellation must stop work at a checkpoint rather than run to completion.
#[test]
fn cancellation_stops_work_at_a_checkpoint() {
    let document = open(&corpus::born_digital_text());
    let cancelled = Arc::new(AtomicBool::new(true));
    let signal = {
        let cancelled = Arc::clone(&cancelled);
        Arc::new(move || cancelled.load(Ordering::SeqCst)) as agentshim_pdf_read::CancelSignal
    };
    let _scope = enter_budget(PdfResourceLimits::image(), Some(signal));

    let error = document
        .render_page_fit(0, RenderLimits::default())
        .expect_err("a cancelled call must stop");
    assert_eq!(error.kind(), PdfReadErrorKind::Cancelled);
}

/// Leaving a scope must restore the previous one, or one call's ceiling would leak into
/// whatever runs next on the same thread.
#[test]
fn budget_scopes_do_not_leak_between_calls() {
    let bytes = corpus::flate_bomb_content_stream();
    assert!(read_under(PdfResourceLimits::text(), &bytes).is_err());
    let document = open(&bytes);
    read_all_pages(&document).expect("no budget installed means no call ceiling");
}

/// The input class the byte ceilings do not describe.
#[test]
fn a_dense_page_is_refused_by_the_page_budget_not_by_a_byte_ceiling() {
    let limits = PdfResourceLimits::text();
    let bytes = corpus::dense_text_page(corpus::DENSE_PAGE_REFUSED_OPERATIONS);

    let error = read_under(limits, &bytes).expect_err("a page past the span ceiling is refused");
    assert_eq!(error.kind(), PdfReadErrorKind::ResourceLimit);
    let limit = error.limit().expect("structured limit details");
    assert!(
        matches!(limit.resource, "pdf_page_spans" | "pdf_stream_operators"),
        "expected a page-shaped budget to refuse it, got {}",
        limit.resource
    );

    assert_eq!(limit.scope, agentshim_pdf_read::LimitScope::Page);

    let (_, metrics) = agentshim_pdf_read::measure(|| {
        let _ = read_under(limits, &bytes);
    });
    let decoded = metrics.decoded_stream_bytes;
    assert!(
        decoded < limits.single_stream_bytes as u64,
        "the fixture must stay under the per-stream ceiling to be the case this test is for, decoded {decoded}"
    );
}

/// A page just inside the ceiling still reads, so the bound is a ceiling rather than a
/// ban on dense pages.
#[test]
fn a_page_inside_the_span_ceiling_still_reads() {
    let limits = PdfResourceLimits::text();
    assert!(corpus::DENSE_PAGE_FITTING_OPERATIONS < limits.page_spans);
    read_under(
        limits,
        &corpus::dense_text_page(corpus::DENSE_PAGE_FITTING_OPERATIONS),
    )
    .expect("a page inside the ceiling must still be delivered");
}

/// The sub-limits deliberately over-subscribe the call, so the total has to be checked
/// rather than inferred from them.
#[test]
fn the_call_total_is_enforced_and_not_merely_declared() {
    let text = PdfResourceLimits::text();
    let sum = text.object_cache_bytes
        + text.stream_cache_bytes
        + text.page_markdown_bytes
        + text.xref_rebuild_bytes
        + text.single_stream_bytes;
    assert!(
        sum > text.call_total_bytes,
        "if the sub-limits fit inside the total, this test proves nothing"
    );

    let over_subscribed = PdfResourceLimits {
        single_stream_bytes: 64 * 1024 * 1024,
        call_total_bytes: 4096,
        ..text
    };
    let error = read_under(over_subscribed, &corpus::flate_bomb_content_stream())
        .expect_err("an allocation past the call total must be refused");
    assert_eq!(error.kind(), PdfReadErrorKind::ResourceLimit);
    let limit = error.limit().expect("structured limit details");
    assert_eq!(
        limit.limit_bytes, 4096,
        "the ceiling must come from the call total, not from the per-stream sub-limit"
    );
}

/// Every ceiling moves with the reservation, so a configured budget configures the
/// enforcement rather than only the bookkeeping.
#[test]
fn the_ceilings_are_derived_from_the_reservation() {
    let small = PdfResourceLimits::text_within(32 * 1024 * 1024);
    let large = PdfResourceLimits::text_within(128 * 1024 * 1024);

    assert!(small.object_cache_bytes < large.object_cache_bytes);
    assert!(small.single_stream_bytes < large.single_stream_bytes);
    assert!(small.stream_cache_bytes < large.stream_cache_bytes);
    assert!(small.page_markdown_bytes < large.page_markdown_bytes);
    assert!(small.xref_rebuild_bytes < large.xref_rebuild_bytes);
    assert!(small.page_spans < large.page_spans);
    assert!(small.stream_operators < large.stream_operators);
    assert_eq!(small.call_total_bytes, 32 * 1024 * 1024);
    assert_eq!(large.call_total_bytes, 128 * 1024 * 1024);
}

/// Text mode must not be able to allocate a render surface at all, and image mode must
/// not be able to hold page Markdown: a zero here means "this mode produces none", and
/// treating it as "unbounded" is how a budget silently stops applying.
#[test]
fn a_zero_sub_limit_refuses_rather_than_permits() {
    let text = PdfResourceLimits::text();
    assert_eq!(text.render_surface_bytes, 0);
    let _scope = enter_budget(text, None);
    let document = open(&corpus::oversized_media_box());
    let error = document
        .render_page_fit(0, RenderLimits::default())
        .expect_err("text mode must refuse a render surface outright");
    assert_eq!(error.kind(), PdfReadErrorKind::ResourceLimit);
}

/// Cancellation has to be noticed inside the content-stream loop, not only between
/// pages: a single dense page was previously uninterruptible for its whole duration.
#[test]
fn cancellation_stops_inside_a_single_page() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    let signal: agentshim_pdf_read::CancelSignal = Arc::new(move || {
        flag.store(true, Ordering::SeqCst);
        true
    });

    let bytes = corpus::dense_text_page(corpus::DENSE_PAGE_REFUSED_OPERATIONS);
    let _scope = enter_budget(PdfResourceLimits::text(), Some(signal));
    let document = try_open(&bytes).expect("open dense fixture");
    let error = document
        .page_to_markdown(0, &MarkdownOptions::default())
        .expect_err("a cancelled page must stop rather than finish");
    assert_eq!(error.kind(), PdfReadErrorKind::Cancelled);
    assert!(cancelled.load(Ordering::SeqCst));
}
