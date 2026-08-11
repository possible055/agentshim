mod format;
mod image;
mod text;

use cap_std::fs::File;
use codexshim_pdf_read::{CancelSignal, ParserLimits, PdfReadDocument, PdfResourceLimits};
use tokio_util::sync::CancellationToken;

use crate::tools::ToolOutput;

use super::request::{PdfMode, ReadError, ReadRequest};

use self::{
    image::read_pdf_images,
    text::{TextRead, read_pdf_text},
};

const MAX_TEXT_PAGES: usize = 20;
const DEFAULT_TEXT_PAGES: usize = 10;
pub(super) const MAX_IMAGE_PAGES: usize = 4;
const DEFAULT_IMAGE_PAGES: usize = 1;
/// Total base64 across one image response.
///
/// 8 MiB rather than the previous 7: four pages divide into it evenly at 2 MiB each,
/// which is easier to reason about, and image payloads do not draw on the text output
/// budget. This is a deliberate contract change — the constant, the README, and the
/// boundary tests move together.
pub(super) const MAX_IMAGE_BASE64_BYTES: usize = 8 * 1024 * 1024;

/// Operational next-step state for one selected page.
///
/// These describe what a caller can do about the page, not what the document is. There
/// is deliberately no `scanned` flag: the format carries no reliable origin signal, and
/// one document routinely mixes extractable text, bare rasters, vector art, blank
/// pages, and partial text layers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PdfPageState {
    /// Usable text of acceptable quality.
    TextReady,
    /// Text is present but its character quality or coverage is doubtful. Returned with
    /// the text, never instead of it.
    TextUncertain,
    /// No usable text, but something is drawn — including pages that are pure vector art
    /// with no image `XObject` at all.
    ImageRequired,
    /// Nothing drawn and no text.
    Blank,
    /// This page could not be processed. Distinct from `ImageRequired`, which means the
    /// page is fine but needs a different mode: reporting a failure as a blank page
    /// would tell the caller they had finished reading when they had not.
    Unavailable,
}

impl PdfPageState {
    fn label(self) -> &'static str {
        match self {
            Self::TextReady => "text_ready",
            Self::TextUncertain => "text_uncertain",
            Self::ImageRequired => "image_required",
            Self::Blank => "blank",
            Self::Unavailable => "unavailable",
        }
    }
}

pub(crate) struct PdfPageOutcome {
    /// Zero-based.
    index: usize,
    state: PdfPageState,
    body: String,
    /// Set when this page's failure must end the call instead of becoming a placeholder.
    fatal: Option<ReadError>,
}

impl Clone for PdfPageOutcome {
    /// `ReadError` is not `Clone`, and a fatal page never reaches the speculative
    /// candidate list, so the clone used by the page walk carries `None`.
    fn clone(&self) -> Self {
        Self {
            index: self.index,
            state: self.state,
            body: self.body.clone(),
            fatal: None,
        }
    }
}

/// Where the caller should resume, expressed so it can be replayed verbatim.
pub(crate) struct PdfContinuation {
    pages: String,
    text_offset: Option<usize>,
}

pub(crate) struct PdfRetryRequest {
    mode: PdfMode,
    pages: String,
}

/// The internal shape of a successful PDF read.
///
/// The plan calls for structured `details` on success, but the MCP success envelope has
/// no extension point yet, so this is rendered through the existing text formatter
/// rather than introducing a parallel response type for PDFs alone.
pub(crate) struct PdfReadOutcome {
    mode: PdfMode,
    page_count: usize,
    source_id: String,
    pages: Vec<PdfPageOutcome>,
    continuation: Option<PdfContinuation>,
    retry_with: Vec<PdfRetryRequest>,
}

pub(super) fn has_pdf_header(prefix: &[u8]) -> bool {
    prefix.get(..8).is_some_and(|header| {
        header.starts_with(b"%PDF-")
            && header[5].is_ascii_digit()
            && header[6] == b'.'
            && header[7].is_ascii_digit()
    })
}

pub(super) fn has_pdf_parameters(request: &ReadRequest) -> bool {
    request.pdf_mode.is_some()
        || request.pages.is_some()
        || request.pdf_text_offset.is_some()
        || request.pdf_source_id.is_some()
}

pub(super) fn read_pdf(
    file: &File,
    absolute: &str,
    request: &ReadRequest,
    source_id: &str,
    cancellation: &CancellationToken,
    call_bytes: usize,
) -> Result<ToolOutput, ReadError> {
    if request.start_line.is_some() || request.line_count.is_some() || request.encoding.is_some() {
        return Err(ReadError::Validation(
            "encoding, start_line, and line_count do not apply to PDF input".to_owned(),
        ));
    }
    verify_source_id(request, source_id)?;
    check_pdf_cancellation(cancellation)?;
    let mode = request.pdf_mode.unwrap_or(PdfMode::Auto);
    // Installed before the document is opened, because cross-reference parsing and
    // reconstruction both happen there — a budget entered afterwards would leave the one
    // path that can allocate a second copy of the source unbounded.
    let _budget = codexshim_pdf_read::enter_budget(
        match mode {
            PdfMode::Image => PdfResourceLimits::image_within(call_bytes),
            PdfMode::Auto | PdfMode::Text => PdfResourceLimits::text_within(call_bytes),
        },
        Some(cancellation_signal(cancellation)),
    );
    let parser_file = file.try_clone()?.into_std();
    let document = PdfReadDocument::from_file(parser_file, ParserLimits::default())?;
    let page_count = document.page_count()?;
    let pages = select_pages(request.pages.as_deref(), mode, page_count)?;
    match mode {
        PdfMode::Auto | PdfMode::Text => read_pdf_text(
            &document,
            &TextRead {
                absolute,
                mode,
                page_count,
                source_id,
            },
            pages,
            request,
            cancellation,
        ),
        PdfMode::Image => read_pdf_images(
            &document,
            absolute,
            page_count,
            pages,
            source_id,
            cancellation,
        ),
    }
}

/// A continuation that names a different source version must fail rather than stitch
/// two documents together. A request without an id keeps the pre-existing fingerprint
/// behaviour and is reported as unverified.
fn verify_source_id(request: &ReadRequest, source_id: &str) -> Result<(), ReadError> {
    match request.pdf_source_id.as_deref() {
        Some(supplied) if supplied != source_id => Err(ReadError::Changed),
        _ => Ok(()),
    }
}

fn select_pages(
    requested: Option<&str>,
    mode: PdfMode,
    page_count: usize,
) -> Result<Vec<usize>, ReadError> {
    if page_count == 0 {
        return Err(ReadError::Validation("PDF has no pages".to_owned()));
    }
    let (maximum, default_pages) = match mode {
        PdfMode::Auto | PdfMode::Text => (MAX_TEXT_PAGES, DEFAULT_TEXT_PAGES),
        PdfMode::Image => (MAX_IMAGE_PAGES, DEFAULT_IMAGE_PAGES),
    };
    let (start, requested_end) = match requested {
        Some(selector) => parse_page_selector(selector)?,
        // An unspecified range delivers a first batch with a continuation. Requiring an
        // explicit range would force a page-count round trip before the most common
        // request — "show me the beginning" — could be answered at all.
        None => (1, page_count.min(default_pages)),
    };
    if start > page_count {
        return Err(ReadError::Validation(format!(
            "pages {start}-{requested_end} exceed PDF page count {page_count}"
        )));
    }
    // A range that overshoots the end is clamped rather than refused. The caller does not
    // know the page count before the first call, so "1-20" is how a whole paper gets asked
    // for; failing it would cost a round trip to learn a number the response already
    // carries. A range starting past the end is still an error, because it selects nothing.
    let end = requested_end.min(page_count);
    // Counted after clamping, so the ceiling bounds the pages actually walked rather than
    // the width of the selector.
    let count = end - start + 1;
    if count > maximum {
        return Err(ReadError::Validation(format!(
            "{} PDF mode accepts at most {maximum} pages per call",
            mode_label(mode)
        )));
    }
    Ok((start - 1..end).collect())
}

fn mode_label(mode: PdfMode) -> &'static str {
    match mode {
        PdfMode::Auto => "auto",
        PdfMode::Text => "text",
        PdfMode::Image => "image",
    }
}

pub(super) fn parse_page_selector(selector: &str) -> Result<(usize, usize), ReadError> {
    if selector.is_empty() || selector.contains(',') {
        return Err(ReadError::Validation(
            "pages must be one page or one continuous range such as \"1-5\"".to_owned(),
        ));
    }
    let mut parts = selector.split('-');
    let start = parts
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| ReadError::Validation("pages must use one-based numbers".to_owned()))?;
    let end = match parts.next() {
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| *value >= start)
            .ok_or_else(|| {
                ReadError::Validation("pages range end must not precede its start".to_owned())
            })?,
        None => start,
    };
    if parts.next().is_some() {
        return Err(ReadError::Validation(
            "pages must contain at most one range separator".to_owned(),
        ));
    }
    Ok((start, end))
}

/// Lets the core stop inside long loops instead of only at page boundaries.
///
/// The mode runtime ceiling cancels this same token, so checkpoint density is what
/// decides how quickly a timeout actually stops work.
fn cancellation_signal(cancellation: &CancellationToken) -> CancelSignal {
    let token = cancellation.clone();
    std::sync::Arc::new(move || token.is_cancelled())
}

fn check_pdf_cancellation(cancellation: &CancellationToken) -> Result<(), ReadError> {
    if cancellation.is_cancelled() {
        Err(ReadError::Cancelled)
    } else {
        Ok(())
    }
}
