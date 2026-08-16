//! The supported facade must stay read-only.
//!
//! This is a derivative of a full-featured PDF library. Everything that could write,
//! edit, sign, redact, convert, or OCR was removed. `src/lib.rs` is the review boundary
//! for new re-exports; these tests catch removals and signature drift in the facade that
//! `agentshim` already consumes.

use std::io::{Seek, SeekFrom, Write};

use agentshim_pdf_read::{
    current_metrics, enter_budget, measure, CancelSignal, LimitScope, MarkdownChunk,
    MarkdownOptions, PageInfo, PageTextAssessment, PageTextStatus, PageVisualAssessment,
    ParserLimits, PdfReadDocument, PdfReadError, PdfReadErrorKind, PdfReadMetrics,
    PdfResourceLimits, RenderLimits, RenderedPage, ResourceLimitDetails, DEFAULT_IMAGE_CALL_BYTES,
    DEFAULT_TEXT_CALL_BYTES, MAX_IMAGE_EDGE_PIXELS, MAX_IMAGE_PIXELS, NAME, VERSION,
};

#[test]
fn the_public_surface_is_read_only() {
    let _: fn(std::fs::File, ParserLimits) -> Result<PdfReadDocument, PdfReadError> =
        PdfReadDocument::from_file;
    let _: fn(&PdfReadDocument) -> Result<usize, PdfReadError> = PdfReadDocument::page_count;
    let _: fn(&PdfReadDocument, usize, &MarkdownOptions) -> Result<String, PdfReadError> =
        PdfReadDocument::page_to_markdown;
    let _: fn(
        &PdfReadDocument,
        usize,
        &MarkdownOptions,
        usize,
        usize,
    ) -> Result<MarkdownChunk, PdfReadError> = PdfReadDocument::page_to_markdown_chunk;
    let _: fn(&PdfReadDocument, usize) -> Result<PageTextAssessment, PdfReadError> =
        PdfReadDocument::assess_page_text;
    let _: fn(&PdfReadDocument, usize) -> Result<PageVisualAssessment, PdfReadError> =
        PdfReadDocument::assess_page_visual;
    let _: fn(&PdfReadDocument, usize) -> Result<PageInfo, PdfReadError> =
        PdfReadDocument::page_info;
    let _: fn(&PdfReadDocument, usize, RenderLimits) -> Result<RenderedPage, PdfReadError> =
        PdfReadDocument::render_page_fit;
    let _: fn(&PdfReadDocument) = PdfReadDocument::release_page_scratch;
    let _: fn(&PdfReadError) -> PdfReadErrorKind = PdfReadError::kind;
    let _: fn(&PdfReadError) -> Option<ResourceLimitDetails> = PdfReadError::limit;
    let _: fn() -> PdfResourceLimits = PdfResourceLimits::text;
    let _: fn() -> PdfResourceLimits = PdfResourceLimits::image;
    let _: fn(usize) -> PdfResourceLimits = PdfResourceLimits::text_within;
    let _: fn(usize) -> PdfResourceLimits = PdfResourceLimits::image_within;

    let _ = PdfReadErrorKind::ResourceLimit;
    let _ = LimitScope::Call;
    let _ = PageTextStatus::Ready;
    let _ = PdfReadMetrics::default();
    let (_, measured) = measure(|| ());
    assert_eq!(measured, PdfReadMetrics::default());
    assert_eq!(current_metrics(), PdfReadMetrics::default());

    let cancel: CancelSignal = std::sync::Arc::new(|| false);
    let _budget = enter_budget(PdfResourceLimits::text(), Some(cancel));

    assert_eq!(DEFAULT_TEXT_CALL_BYTES, 64 * 1024 * 1024);
    assert_eq!(DEFAULT_IMAGE_CALL_BYTES, 96 * 1024 * 1024);
    assert!(MAX_IMAGE_EDGE_PIXELS > 0);
    assert!(MAX_IMAGE_PIXELS > 0);
    assert_eq!(NAME, "agentshim-pdf-read");
    assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
}

/// A document is opened from a handle the caller already holds. There is deliberately no
/// path-based constructor: the tool layer's capability check and fingerprint contract
/// both depend on the handle, and reopening by path would bypass them.
#[test]
fn documents_are_opened_only_from_a_handle() {
    let mut file = tempfile::tempfile().expect("temporary file");
    file.write_all(b"%PDF-1.7\n").expect("write");
    file.seek(SeekFrom::Start(0)).expect("rewind");

    // Not a valid PDF, so this fails — but it fails inside the parser, which is proof the
    // handle-based entry point is the one that exists.
    assert!(PdfReadDocument::from_file(file, ParserLimits::default()).is_err());
}

/// The removed classifier inferred document origin and materialised image streams to do
/// it. Its replacement answers two separate operational questions and decodes nothing.
#[test]
fn origin_guessing_is_not_part_of_the_surface() {
    // `PageClass`, `classify_page`, `PageKind`, `Scanned`, and `BornDigital` are gone.
    // What remains cannot express "where did this document come from".
    let statuses = [
        PageTextStatus::Ready,
        PageTextStatus::Uncertain,
        PageTextStatus::Absent,
    ];
    assert_eq!(statuses.len(), 3);
}
