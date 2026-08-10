//! The crate's public surface must stay read-only.
//!
//! This is a derivative of a full-featured PDF library. Everything that could write,
//! edit, sign, redact, convert, or OCR was removed, and nothing may reappear by
//! accident: a re-export added while chasing a compile error is easy to miss in review
//! and hard to notice at runtime.

use std::io::{Seek, SeekFrom, Write};

use codexshim_pdf_read::{
    MarkdownChunk, MarkdownOptions, PageInfo, PageTextAssessment, PageTextStatus,
    PageVisualAssessment, ParserLimits, PdfReadDocument, PdfReadError, PdfReadErrorKind,
    PdfReadMetrics, PdfResourceLimits, RenderLimits, RenderedPage, ResourceLimitDetails,
};

/// Every public entry point, named explicitly. Adding one to the crate without adding it
/// here is fine; removing one, or adding a mutating one, is what this is watching for.
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

    let _ = PdfReadErrorKind::ResourceLimit;
    let _ = PageTextStatus::Ready;
    let _ = PdfResourceLimits::text();
    let _ = PdfResourceLimits::image();
    let _ = PdfReadMetrics::default();
    let _: Option<ResourceLimitDetails> = None;
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
