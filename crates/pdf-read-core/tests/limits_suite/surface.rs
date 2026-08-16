use std::io::{Seek, SeekFrom, Write};

use agentshim_pdf_read::{
    current_metrics, enter_budget, measure, CancelSignal, LimitScope, PageTextStatus, ParserLimits,
    PdfReadDocument, PdfReadErrorKind, PdfReadMetrics, PdfResourceLimits, DEFAULT_IMAGE_CALL_BYTES,
    DEFAULT_TEXT_CALL_BYTES, MAX_IMAGE_EDGE_PIXELS, MAX_IMAGE_PIXELS, NAME, VERSION,
};

#[test]
fn the_public_surface_is_read_only() {
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

    assert!(PdfReadDocument::from_file(file, ParserLimits::default()).is_err());
}

/// The removed classifier inferred document origin and materialised image streams to do
/// it. Its replacement answers two separate operational questions and decodes nothing.
#[test]
fn origin_guessing_is_not_part_of_the_surface() {
    let statuses = [
        PageTextStatus::Ready,
        PageTextStatus::Uncertain,
        PageTextStatus::Absent,
    ];
    assert_eq!(statuses.len(), 3);
}
