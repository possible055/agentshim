use base64::Engine as _;
use codexshim_pdf_read::{PdfReadDocument, RenderLimits};
use tokio_util::sync::CancellationToken;

use crate::tools::{ToolImage, ToolOutput};

use super::{
    MAX_IMAGE_BASE64_BYTES, PdfContinuation, PdfMode, PdfPageOutcome, PdfPageState, PdfReadOutcome,
    ReadError, check_pdf_cancellation, format::format_pdf_outcome, text::page_range_label,
};

pub(super) fn read_pdf_images(
    document: &PdfReadDocument,
    absolute: &str,
    page_count: usize,
    pages: Vec<usize>,
    source_id: &str,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ReadError> {
    let requested_last = pages[pages.len() - 1];
    let mut delivered = Vec::with_capacity(pages.len());
    let mut images = Vec::with_capacity(pages.len());
    let mut payload_bytes = 0_usize;
    for page in pages {
        check_pdf_cancellation(cancellation)?;
        let rendered = document.render_page_fit(page, RenderLimits::default())?;

        // Base64 is 4 bytes per 3, so the encoded size is known from the PNG length
        // before the encoding runs. Checking first means an oversized page never has two
        // copies of itself alive at once.
        let encoded_len = rendered.png.len().div_ceil(3).saturating_mul(4);
        if payload_bytes.saturating_add(encoded_len) > MAX_IMAGE_BASE64_BYTES {
            if images.is_empty() {
                return Err(ReadError::ResourceLimit {
                    message: "first rendered PDF page exceeds the image payload limit".to_owned(),
                    resource: "pdf_image_base64",
                    limit_bytes: Some(MAX_IMAGE_BASE64_BYTES as u64),
                    observed_bytes: Some(encoded_len as u64),
                });
            }
            // Whole pages only: half an image is not a smaller image, so the budget is
            // spent on complete pages and the rest becomes a continuation.
            break;
        }

        // Consume the PNG into the encoder so the raw bytes are freed as the encoded
        // string is built, rather than both being held.
        let data = base64::engine::general_purpose::STANDARD.encode(rendered.png);
        payload_bytes += data.len();
        images.push(ToolImage {
            data,
            mime_type: "image/png",
        });
        delivered.push(PdfPageOutcome {
            index: page,
            state: PdfPageState::TextReady,
            body: String::new(),
            fatal: None,
        });
        check_pdf_cancellation(cancellation)?;
    }

    let last = delivered[delivered.len() - 1].index;
    let continuation = if last < requested_last || last + 1 < page_count {
        let next_first = last + 1;
        let next_last = (next_first + delivered.len() - 1).min(page_count - 1);
        Some(PdfContinuation {
            pages: page_range_label(next_first, next_last),
            text_offset: None,
        })
    } else {
        None
    };

    let outcome = PdfReadOutcome {
        mode: PdfMode::Image,
        page_count,
        source_id: source_id.to_owned(),
        pages: delivered,
        continuation,
        retry_with: Vec::new(),
    };
    Ok(ToolOutput::with_images(
        format_pdf_outcome(absolute, &outcome),
        images,
    ))
}
