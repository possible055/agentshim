use base64::Engine as _;
use codexshim_pdf_read::{
    CancelSignal, MarkdownOptions, PageTextStatus, ParserLimits, PdfReadDocument,
    PdfResourceLimits, RenderLimits,
};
use std::fmt::Write as _;

use crate::tools::ToolImage;

const MAX_TEXT_PAGES: usize = 20;
const DEFAULT_TEXT_PAGES: usize = 10;
const MAX_IMAGE_PAGES: usize = 4;
const DEFAULT_IMAGE_PAGES: usize = 1;
/// Total base64 across one image response.
///
/// 8 MiB rather than the previous 7: four pages divide into it evenly at 2 MiB each,
/// which is easier to reason about, and image payloads do not draw on the text output
/// budget. This is a deliberate contract change — the constant, the README, and the
/// boundary tests move together.
const MAX_IMAGE_BASE64_BYTES: usize = 8 * 1024 * 1024;

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

fn has_pdf_header(prefix: &[u8]) -> bool {
    prefix.get(..8).is_some_and(|header| {
        header.starts_with(b"%PDF-")
            && header[5].is_ascii_digit()
            && header[6] == b'.'
            && header[7].is_ascii_digit()
    })
}

fn has_pdf_parameters(request: &ReadRequest) -> bool {
    request.pdf_mode.is_some()
        || request.pages.is_some()
        || request.pdf_text_offset.is_some()
        || request.pdf_source_id.is_some()
}

fn read_pdf(
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

fn parse_page_selector(selector: &str) -> Result<(usize, usize), ReadError> {
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

/// Until the assessment split lands, `auto` and `text` share one rule: non-empty text is
/// returned, and only a selection that is empty throughout becomes `pdf_image_required`.
///
/// Neither mode calls `classify_page()`. That classifier materialises image streams, so
/// using it here would decode pixels on the path this plan exists to make cheap.
/// Everything about a text read that stays constant while the page walk advances.
struct TextRead<'a> {
    absolute: &'a str,
    mode: PdfMode,
    page_count: usize,
    source_id: &'a str,
}

fn read_pdf_text(
    document: &PdfReadDocument,
    read: &TextRead<'_>,
    pages: Vec<usize>,
    request: &ReadRequest,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ReadError> {
    let TextRead {
        absolute,
        mode,
        page_count,
        source_id,
    } = *read;
    if let Some(offset) = request.pdf_text_offset {
        return resume_page(document, read, pages[0], offset, cancellation);
    }

    let selected_first = pages[0];
    let selected_last = pages[pages.len() - 1];
    let mut committed: Vec<PdfPageOutcome> = Vec::new();
    let mut stopped_before = None;

    // One page at a time, committed only once the whole envelope still fits. Extracting
    // the full selection first and dropping the tail afterwards would pay for parsing,
    // layout, and Markdown on pages that were never going to be returned — on a 20-page
    // request that is most of the work.
    let mut has_text = false;
    for page in pages {
        check_pdf_cancellation(cancellation)?;
        let mut outcome = extract_page(document, page);
        if let Some(error) = outcome.fatal.take() {
            return Err(error);
        }
        has_text |= matches!(
            outcome.state,
            PdfPageState::TextReady | PdfPageState::TextUncertain
        );
        let mut candidate = committed.clone();
        candidate.push(outcome);
        if fits(
            absolute,
            mode,
            page_count,
            source_id,
            &candidate,
            None,
            cancellation,
        ) {
            committed = candidate;
            // The walk is forward-only, so this page's spans and content will never be
            // read again; keeping them would hold call budget for no benefit.
            document.release_page_scratch();
            check_pdf_cancellation(cancellation)?;
            continue;
        }
        stopped_before = Some(page);
        break;
    }

    if committed.is_empty() {
        // Not even one page fits whole, so the caller resumes inside it by byte offset
        // rather than receiving an unrecoverable truncation.
        let page = stopped_before.unwrap_or(selected_first);
        return resume_page(document, read, page, 0, cancellation);
    }

    // A typed error is only right when nothing in the whole selection was usable.
    // Escalating on a partial selection would discard pages the caller can already read.
    if !has_text && stopped_before.is_none() {
        let states: Vec<PdfPageState> = committed.iter().map(|page| page.state).collect();
        if states.iter().all(|state| *state == PdfPageState::Unavailable) {
            return Err(ReadError::PdfProcessing(format!(
                "no page in {} could be processed",
                page_range_label(selected_first, selected_last)
            )));
        }
        if states.contains(&PdfPageState::ImageRequired)
            && states
                .iter()
                .all(|state| matches!(state, PdfPageState::ImageRequired | PdfPageState::Blank))
        {
            return Err(ReadError::PdfImageRequired {
                pages: page_range_label(selected_first, selected_last),
                source_id: source_id.to_owned(),
            });
        }
    }

    let outcome = build_text_outcome(mode, page_count, source_id, &committed, None);
    Ok(ToolOutput::new(format_pdf_outcome(absolute, &outcome)))
}

/// Route one page to its operational state.
///
/// Text usability and visible content are two independent questions, and only their
/// combination decides the next step. A page with no usable text is `ImageRequired` when
/// something is drawn on it — including a purely vector page with no image `XObject` — and
/// `Blank` only when nothing is drawn at all.
///
/// A page whose processing fails is `Unavailable`, never `Blank`. Reporting a failure as
/// an empty page tells the caller they have finished reading when they have not, which
/// is worse than telling them it failed.
fn extract_page(document: &PdfReadDocument, page: usize) -> PdfPageOutcome {
    let markdown = match document
        .page_to_markdown(page, &MarkdownOptions::default())
        .map_err(ReadError::from)
    {
        Ok(markdown) => markdown,
        Err(error) if is_fatal(&error) => {
            return PdfPageOutcome {
                index: page,
                state: PdfPageState::Unavailable,
                body: String::new(),
                fatal: Some(error),
            };
        }
        Err(error) => return unavailable_page(page, &error),
    };

    let trimmed = markdown.trim();
    if !trimmed.is_empty() {
        // `text` never discards non-empty text; a doubtful assessment only labels it.
        let uncertain = document
            .assess_page_text(page)
            .is_ok_and(|text| text.status == PageTextStatus::Uncertain);
        let state = if uncertain {
            PdfPageState::TextUncertain
        } else {
            PdfPageState::TextReady
        };
        return PdfPageOutcome {
            index: page,
            state,
            body: trimmed.to_owned(),
            fatal: None,
        };
    }

    match document.assess_page_visual(page).map_err(ReadError::from) {
        Ok(visual) if visual.has_visible_content() => PdfPageOutcome {
            index: page,
            state: PdfPageState::ImageRequired,
            body: format!(
                "(no extractable text; retry with pdf_mode=\"image\" and pages=\"{}\")",
                page + 1
            ),
            fatal: None,
        },
        Ok(_) => PdfPageOutcome {
            index: page,
            state: PdfPageState::Blank,
            body: "(blank page)".to_owned(),
            fatal: None,
        },
        Err(error) if is_fatal(&error) => PdfPageOutcome {
            index: page,
            state: PdfPageState::Unavailable,
            body: String::new(),
            fatal: Some(error),
        },
        Err(error) => unavailable_page(page, &error),
    }
}

fn unavailable_page(page: usize, error: &ReadError) -> PdfPageOutcome {
    PdfPageOutcome {
        index: page,
        state: PdfPageState::Unavailable,
        body: format!("(page could not be processed: {error})"),
        fatal: None,
    }
}

/// Whether a per-page failure must end the whole call rather than become a placeholder.
///
/// A resource ceiling or a cancellation is not a property of the page: continuing would
/// keep spending against a budget that already refused, and the remaining pages would
/// fail the same way.
/// Whether this failure ends the whole call rather than just this page.
///
/// A page-scoped limit is the one refusal that is not fatal: a single unusually dense
/// page among twenty is a reason to report that page as unavailable, not to discard the
/// nineteen the caller can still read. Every other limit means the call's own budget is
/// spent, and continuing would keep spending against a ceiling that already said no.
fn is_fatal(error: &ReadError) -> bool {
    match error {
        ReadError::Cancelled | ReadError::ResourceLimit { .. } => true,
        ReadError::Pdf(inner) => match inner.kind() {
            codexshim_pdf_read::PdfReadErrorKind::ResourceLimit => !matches!(
                inner.limit().map(|limit| limit.scope),
                Some(codexshim_pdf_read::LimitScope::Page)
            ),
            codexshim_pdf_read::PdfReadErrorKind::Cancelled
            | codexshim_pdf_read::PdfReadErrorKind::Encrypted => true,
            _ => false,
        },
        _ => false,
    }
}

fn fits(
    absolute: &str,
    mode: PdfMode,
    page_count: usize,
    source_id: &str,
    pages: &[PdfPageOutcome],
    first_body: Option<&str>,
    cancellation: &CancellationToken,
) -> bool {
    let outcome = build_text_outcome(mode, page_count, source_id, pages, first_body);
    let output = ToolOutput::new(format_pdf_outcome(absolute, &outcome));
    output.fits_content_and_model(cancellation)
}

/// Deliver as much of one page as fits, starting at `offset`.
///
/// The returned `next_pdf_text_offset` is a UTF-8 boundary into this page's own Markdown,
/// so concatenating the rounds reproduces the page exactly. Nothing here is discarded and
/// re-extracted: the chunk is cut to the largest prefix the envelope can hold.
fn resume_page(
    document: &PdfReadDocument,
    read: &TextRead<'_>,
    page: usize,
    offset: usize,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ReadError> {
    let TextRead {
        absolute,
        mode,
        page_count,
        source_id,
    } = *read;
    let budget = crate::output::effective_byte_limit();
    let chunk = document.page_to_markdown_chunk(page, &MarkdownOptions::default(), offset, budget)?;
    if chunk.text.is_empty() && offset >= chunk.page_bytes && chunk.page_bytes > 0 {
        // Resuming exactly at the end is a completion, not an error.
        let outcome = completed_page_outcome(mode, page_count, source_id, page);
        return Ok(ToolOutput::new(format_pdf_outcome(absolute, &outcome)));
    }

    let mut low = 0_usize;
    let mut high = chunk.text.len();
    let mut best = 0_usize;
    while low <= high {
        let midpoint = low + (high - low) / 2;
        let mut end = midpoint.min(chunk.text.len());
        while end > 0 && !chunk.text.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = page_chunk_outcome(
            mode,
            page_count,
            source_id,
            page,
            &chunk.text[..end],
            (offset + end < chunk.page_bytes).then_some(offset + end),
        );
        let output = ToolOutput::new(format_pdf_outcome(absolute, &candidate));
        if output.fits_content_and_model(cancellation) {
            best = end;
            if midpoint == high {
                break;
            }
            low = midpoint + 1;
        } else {
            if midpoint == 0 {
                break;
            }
            high = midpoint - 1;
        }
    }
    if best == 0 {
        return Err(crate::output::OutputError::RequiredContentTooLarge.into());
    }

    let outcome = page_chunk_outcome(
        mode,
        page_count,
        source_id,
        page,
        &chunk.text[..best],
        (offset + best < chunk.page_bytes).then_some(offset + best),
    );
    Ok(ToolOutput::new(format_pdf_outcome(absolute, &outcome)))
}

fn page_chunk_outcome(
    mode: PdfMode,
    page_count: usize,
    source_id: &str,
    page: usize,
    body: &str,
    next_offset: Option<usize>,
) -> PdfReadOutcome {
    PdfReadOutcome {
        mode,
        page_count,
        source_id: source_id.to_owned(),
        pages: vec![PdfPageOutcome {
            index: page,
            state: PdfPageState::TextReady,
            body: body.to_owned(),
            fatal: None,
        }],
        continuation: next_offset.map(|offset| PdfContinuation {
            pages: page_range_label(page, page),
            text_offset: Some(offset),
        }),
        retry_with: Vec::new(),
    }
}

fn completed_page_outcome(
    mode: PdfMode,
    page_count: usize,
    source_id: &str,
    page: usize,
) -> PdfReadOutcome {
    PdfReadOutcome {
        mode,
        page_count,
        source_id: source_id.to_owned(),
        pages: vec![PdfPageOutcome {
            index: page,
            state: PdfPageState::TextReady,
            body: "(page complete at this offset)".to_owned(),
            fatal: None,
        }],
        continuation: None,
        retry_with: Vec::new(),
    }
}

fn page_range_label(first: usize, last: usize) -> String {
    if first == last {
        format!("{}", first + 1)
    } else {
        format!("{}-{}", first + 1, last + 1)
    }
}

/// Build the outcome for the pages that actually fit.
///
/// The continuation width is derived from what was delivered, not from what was
/// requested: after delivering pages 1-6 of a 20-page request the next call must be
/// 7-12, never 7-26.
fn build_text_outcome(
    mode: PdfMode,
    page_count: usize,
    source_id: &str,
    pages: &[PdfPageOutcome],
    first_body: Option<&str>,
) -> PdfReadOutcome {
    let last = pages[pages.len() - 1].index;
    let delivered = pages
        .iter()
        .enumerate()
        .map(|(position, page)| PdfPageOutcome {
            index: page.index,
            state: page.state,
            body: match (position, first_body) {
                (0, Some(body)) => body.to_owned(),
                _ => page.body.clone(),
            },
            fatal: None,
        })
        .collect::<Vec<_>>();

    let truncated_first_page = first_body.is_some();
    let continuation = if truncated_first_page {
        // The page itself did not fit, so the caller resumes inside it rather than
        // moving on. The byte offset arrives with the bounded sink in a later phase.
        Some(PdfContinuation {
            pages: page_range_label(last, last),
            text_offset: None,
        })
    } else if last + 1 < page_count {
        let next_first = last + 1;
        let next_last = (next_first + delivered.len() - 1).min(page_count - 1);
        Some(PdfContinuation {
            pages: page_range_label(next_first, next_last),
            text_offset: None,
        })
    } else {
        None
    };

    let retry_with = delivered
        .iter()
        .filter(|page| page.state == PdfPageState::ImageRequired)
        .map(|page| PdfRetryRequest {
            mode: PdfMode::Image,
            pages: page_range_label(page.index, page.index),
        })
        .collect();

    PdfReadOutcome {
        mode,
        page_count,
        source_id: source_id.to_owned(),
        pages: delivered,
        continuation,
        retry_with,
    }
}

fn format_pdf_outcome(absolute: &str, outcome: &PdfReadOutcome) -> String {
    let first = outcome.pages[0].index + 1;
    let last = outcome.pages[outcome.pages.len() - 1].index + 1;
    let rendering = match outcome.mode {
        PdfMode::Image => "PNG images",
        PdfMode::Auto | PdfMode::Text => "Markdown",
    };
    let mut text = format!(
        "Path: {absolute}\nPDF: pages {first}-{last} of {} as {rendering}\nMode: {}\n\
         Source: {}",
        outcome.page_count,
        mode_label(outcome.mode),
        outcome.source_id
    );

    let states = outcome
        .pages
        .iter()
        .map(|page| format!("{}={}", page.index + 1, page.state.label()))
        .collect::<Vec<_>>()
        .join(" ");
    if !states.is_empty() {
        write!(text, "\nPages: {states}").expect("writing to String cannot fail");
    }

    for page in &outcome.pages {
        if outcome.mode == PdfMode::Image {
            continue;
        }
        write!(text, "\n\n## Page {}\n{}", page.index + 1, page.body)
            .expect("writing to String cannot fail");
    }

    match &outcome.continuation {
        Some(continuation) => {
            write!(
                text,
                "\n\nPartial: pages {first}-{last} of {} shown. Continue with pages=\"{}\"",
                outcome.page_count, continuation.pages
            )
            .expect("writing to String cannot fail");
            if let Some(offset) = continuation.text_offset {
                write!(text, " and pdf_text_offset={offset}")
                    .expect("writing to String cannot fail");
            }
            write!(text, " and pdf_source_id=\"{}\".", outcome.source_id)
                .expect("writing to String cannot fail");
        }
        None => text.push_str("\n\nComplete."),
    }

    for retry in &outcome.retry_with {
        write!(
            text,
            "\nRetry: pdf_mode=\"{}\" pages=\"{}\" pdf_source_id=\"{}\".",
            mode_label(retry.mode),
            retry.pages,
            outcome.source_id
        )
        .expect("writing to String cannot fail");
    }
    text
}

fn read_pdf_images(
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
