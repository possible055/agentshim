use std::fmt::Write as _;

use super::{PdfMode, PdfReadOutcome, mode_label};
use crate::tools::read::cursor;

pub(super) fn format_pdf_outcome(absolute: &str, outcome: &PdfReadOutcome) -> String {
    let first = outcome.pages[0].index + 1;
    let last = outcome.pages[outcome.pages.len() - 1].index + 1;
    let pages = if first == last {
        first.to_string()
    } else {
        format!("{first}-{last}")
    };
    let mut text = format!(
        "PDF: pages={pages}/{} mode={} source={}",
        outcome.page_count,
        mode_label(outcome.mode),
        outcome.source_id
    );

    let states = outcome
        .pages
        .iter()
        .filter(|page| page.state != super::PdfPageState::TextReady)
        .map(|page| format!("{}={}", page.index + 1, page.state.label()))
        .collect::<Vec<_>>()
        .join(" ");
    if !states.is_empty() {
        write!(text, "\nPage states: {states}").expect("writing to String cannot fail");
    }

    for page in &outcome.pages {
        if outcome.mode == PdfMode::Image {
            continue;
        }
        write!(text, "\n\n## Page {}\n{}", page.index + 1, page.body)
            .expect("writing to String cannot fail");
    }

    if let Some(continuation) = &outcome.continuation {
        write!(
            text,
            "\n\n{} pdf_mode=\"{}\" pages=\"{}\" {}=\"{}\".",
            crate::output::PARTIAL_MARKER,
            mode_label(outcome.mode),
            continuation.pages,
            crate::output::PDF_CURSOR_FIELD,
            cursor::encode(&outcome.source_id, continuation.text_offset)
        )
        .expect("writing to String cannot fail");
    }

    for retry in &outcome.retry_with {
        write!(
            text,
            "\nRetry: pdf_mode=\"{}\" pages=\"{}\" {}=\"{}\".",
            mode_label(retry.mode),
            retry.pages,
            crate::output::PDF_CURSOR_FIELD,
            cursor::encode(&outcome.source_id, None)
        )
        .expect("writing to String cannot fail");
    }
    let _ = absolute;
    text
}
