use std::fmt::Write as _;

use super::{PdfMode, PdfReadOutcome, mode_label};

pub(super) fn format_pdf_outcome(absolute: &str, outcome: &PdfReadOutcome) -> String {
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
