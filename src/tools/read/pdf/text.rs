use codexshim_pdf_read::{MarkdownOptions, PageTextStatus, PdfReadDocument};
use tokio_util::sync::CancellationToken;

use crate::tools::ToolOutput;

use super::{
    PdfContinuation, PdfMode, PdfPageOutcome, PdfPageState, PdfReadOutcome, PdfRetryRequest,
    ReadError, ReadRequest, check_pdf_cancellation, format::format_pdf_outcome,
};

/// Until the assessment split lands, `auto` and `text` share one rule: non-empty text is
/// returned, and only a selection that is empty throughout becomes `pdf_image_required`.
///
/// Neither mode calls `classify_page()`. That classifier materialises image streams, so
/// using it here would decode pixels on the path this plan exists to make cheap.
/// Everything about a text read that stays constant while the page walk advances.
pub(super) struct TextRead<'a> {
    pub(super) absolute: &'a str,
    pub(super) mode: PdfMode,
    pub(super) page_count: usize,
    pub(super) source_id: &'a str,
}

pub(super) fn read_pdf_text(
    document: &PdfReadDocument,
    read: &TextRead<'_>,
    pages: Vec<usize>,
    request: &ReadRequest,
    cancellation: &CancellationToken,
    output_budget: &crate::output::CallOutputBudget,
) -> Result<ToolOutput, ReadError> {
    let TextRead {
        absolute,
        mode,
        page_count,
        source_id,
    } = *read;
    if let Some(offset) = request.pdf_text_offset {
        return resume_page(
            document,
            read,
            pages[0],
            offset,
            cancellation,
            output_budget,
        );
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
            output_budget,
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
        return resume_page(document, read, page, 0, cancellation, output_budget);
    }

    // A typed error is only right when nothing in the whole selection was usable.
    // Escalating on a partial selection would discard pages the caller can already read.
    if !has_text && stopped_before.is_none() {
        let states: Vec<PdfPageState> = committed.iter().map(|page| page.state).collect();
        if states
            .iter()
            .all(|state| *state == PdfPageState::Unavailable)
        {
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

#[allow(
    clippy::too_many_arguments,
    reason = "the candidate envelope mirrors every continuation field that affects its cost"
)]
fn fits(
    absolute: &str,
    mode: PdfMode,
    page_count: usize,
    source_id: &str,
    pages: &[PdfPageOutcome],
    first_body: Option<&str>,
    cancellation: &CancellationToken,
    output_budget: &crate::output::CallOutputBudget,
) -> bool {
    let outcome = build_text_outcome(mode, page_count, source_id, pages, first_body);
    let output = ToolOutput::new(format_pdf_outcome(absolute, &outcome));
    output.fits_content_and_call(output_budget, cancellation)
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
    output_budget: &crate::output::CallOutputBudget,
) -> Result<ToolOutput, ReadError> {
    let TextRead {
        absolute,
        mode,
        page_count,
        source_id,
    } = *read;
    let budget = crate::output::effective_byte_limit();
    let chunk =
        document.page_to_markdown_chunk(page, &MarkdownOptions::default(), offset, budget)?;
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
        if output.fits_content_and_call(output_budget, cancellation) {
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
        return Err(crate::output::OutputError::BurstLimit.into());
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
            body: "No remaining text at this offset.".to_owned(),
            fatal: None,
        }],
        continuation: None,
        retry_with: Vec::new(),
    }
}

pub(super) fn page_range_label(first: usize, last: usize) -> String {
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
