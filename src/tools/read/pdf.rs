use base64::Engine as _;
use codexshim_pdf_read::{
    MarkdownOptions, PageClass, ParserLimits, PdfReadDocument, RenderLimits,
};
use std::fmt::Write as _;

use crate::tools::ToolImage;

const MAX_TEXT_PAGES: usize = 20;
const DEFAULT_TEXT_PAGES: usize = 10;
const MAX_IMAGE_PAGES: usize = 4;
const MAX_IMAGE_BASE64_BYTES: usize = 7 * 1024 * 1024;

fn has_pdf_header(prefix: &[u8]) -> bool {
    prefix.get(..8).is_some_and(|header| {
        header.starts_with(b"%PDF-")
            && header[5].is_ascii_digit()
            && header[6] == b'.'
            && header[7].is_ascii_digit()
    })
}

fn has_pdf_parameters(request: &ReadRequest) -> bool {
    request.pdf_mode.is_some() || request.pages.is_some()
}

fn read_pdf(
    file: &File,
    absolute: &str,
    request: &ReadRequest,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ReadError> {
    if request.start_line.is_some() || request.line_count.is_some() || request.encoding.is_some() {
        return Err(ReadError::Validation(
            "encoding, start_line, and line_count do not apply to PDF input".to_owned(),
        ));
    }
    check_pdf_cancellation(cancellation)?;
    let parser_file = file.try_clone()?.into_std();
    let document = PdfReadDocument::from_file(parser_file, ParserLimits::default())?;
    let page_count = document.page_count()?;
    let mode = request.pdf_mode.unwrap_or(PdfMode::Text);
    let pages = select_pages(request.pages.as_deref(), mode, page_count)?;
    match mode {
        PdfMode::Text => read_pdf_text(&document, absolute, page_count, pages, cancellation),
        PdfMode::Image => read_pdf_images(&document, absolute, page_count, pages, cancellation),
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
    let maximum = match mode {
        PdfMode::Text => MAX_TEXT_PAGES,
        PdfMode::Image => MAX_IMAGE_PAGES,
    };
    let (start, end) = match requested {
        Some(selector) => parse_page_selector(selector)?,
        None => match mode {
            PdfMode::Text if page_count > DEFAULT_TEXT_PAGES => {
                return Err(ReadError::Validation(format!(
                    "PDF has {page_count} pages; specify pages for documents longer than \
                     {DEFAULT_TEXT_PAGES} pages"
                )));
            }
            PdfMode::Text => (1, page_count),
            PdfMode::Image => (1, page_count.min(MAX_IMAGE_PAGES)),
        },
    };
    if start > page_count || end > page_count {
        return Err(ReadError::Validation(format!(
            "pages {start}-{end} exceed PDF page count {page_count}"
        )));
    }
    let count = end - start + 1;
    if count > maximum {
        return Err(ReadError::Validation(format!(
            "{mode:?} PDF mode accepts at most {maximum} pages per call"
        )));
    }
    Ok((start - 1..end).collect())
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

fn read_pdf_text(
    document: &PdfReadDocument,
    absolute: &str,
    page_count: usize,
    pages: Vec<usize>,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ReadError> {
    let mut extracted = Vec::with_capacity(pages.len());
    for page in pages {
        check_pdf_cancellation(cancellation)?;
        let markdown = document.page_to_markdown(page, &MarkdownOptions::default())?;
        let body = if markdown.trim().is_empty() {
            match document.classify_page(page) {
                Ok(PageClass::Scanned | PageClass::ImageText) => format!(
                    "(no text layer; retry with pdf_mode=\"image\" and pages=\"{}\")",
                    page + 1
                ),
                Ok(PageClass::Empty) => "(blank page)".to_owned(),
                _ => "(no extractable text)".to_owned(),
            }
        } else {
            markdown
        };
        extracted.push((page, body.trim().to_owned()));
        check_pdf_cancellation(cancellation)?;
    }
    fit_pdf_text_output(absolute, page_count, &extracted)
}

fn fit_pdf_text_output(
    absolute: &str,
    page_count: usize,
    pages: &[(usize, String)],
) -> Result<ToolOutput, ReadError> {
    for shown in (1..=pages.len()).rev() {
        let output = format_pdf_text_output(absolute, page_count, &pages[..shown], None);
        if output.fits_content_budget() {
            return Ok(output);
        }
    }

    let body = &pages[0].1;
    let boundaries = body
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(body.len()))
        .collect::<Vec<_>>();
    let mut low = 0_usize;
    let mut high = boundaries.len();
    let mut best = None;
    while low < high {
        let midpoint = low + (high - low) / 2;
        let end = boundaries[midpoint];
        let truncated = truncated_page_body(&body[..end]);
        let output =
            format_pdf_text_output(absolute, page_count, &pages[..1], Some(&truncated));
        if output.fits_content_budget() {
            best = Some(end);
            low = midpoint + 1;
        } else {
            high = midpoint;
        }
    }
    let best = best.ok_or(crate::output::OutputError::RequiredContentTooLarge)?;
    let newline_end = body[..best].rfind('\n').filter(|end| *end > 0);
    let end = newline_end.unwrap_or(best);
    let truncated = truncated_page_body(&body[..end]);
    let output = format_pdf_text_output(absolute, page_count, &pages[..1], Some(&truncated));
    if output.fits_content_budget() {
        Ok(output)
    } else {
        Err(crate::output::OutputError::RequiredContentTooLarge.into())
    }
}

fn truncated_page_body(prefix: &str) -> String {
    format!("{}\n… [page truncated]", prefix.trim_end())
}

fn format_pdf_text_output(
    absolute: &str,
    page_count: usize,
    pages: &[(usize, String)],
    first_body: Option<&str>,
) -> ToolOutput {
    let first = pages[0].0 + 1;
    let last = pages[pages.len() - 1].0 + 1;
    let mut text = format!(
        "Path: {absolute}\nPDF: pages {first}-{last} of {page_count} as Markdown"
    );
    for (index, (page, body)) in pages.iter().enumerate() {
        let body = if index == 0 {
            first_body.unwrap_or(body)
        } else {
            body
        };
        write!(text, "\n\n## Page {}\n{body}", page + 1).expect("writing to String cannot fail");
    }
    if last < page_count {
        let next_end = (last + MAX_TEXT_PAGES).min(page_count);
        write!(
            text,
            "\n\nPartial: pages {first}-{last} of {page_count} shown. Continue with pages=\"{}-{next_end}\".",
            last + 1
        )
        .expect("writing to String cannot fail");
    } else {
        text.push_str("\n\nComplete.");
    }
    ToolOutput::new(text)
}

fn read_pdf_images(
    document: &PdfReadDocument,
    absolute: &str,
    page_count: usize,
    pages: Vec<usize>,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ReadError> {
    let first = pages[0] + 1;
    let requested_last = pages[pages.len() - 1] + 1;
    let mut images = Vec::with_capacity(pages.len());
    let mut payload_bytes = 0_usize;
    let mut last = first - 1;
    for page in pages {
        check_pdf_cancellation(cancellation)?;
        let rendered = document.render_page_fit(page, RenderLimits::default())?;
        let data = base64::engine::general_purpose::STANDARD.encode(rendered.png);
        if payload_bytes.saturating_add(data.len()) > MAX_IMAGE_BASE64_BYTES {
            if images.is_empty() {
                return Err(ReadError::ResourceLimit(
                    "first rendered PDF page exceeds the image payload limit".to_owned(),
                ));
            }
            break;
        }
        payload_bytes += data.len();
        images.push(ToolImage {
            data,
            mime_type: "image/png",
        });
        last = page + 1;
        check_pdf_cancellation(cancellation)?;
    }
    let mut text =
        format!("Path: {absolute}\nPDF: pages {first}-{last} of {page_count} as PNG images");
    if last < requested_last || last < page_count {
        let next_end = (last + MAX_IMAGE_PAGES).min(page_count);
        write!(
            text,
            "\nPartial: continue with pages=\"{}-{next_end}\".",
            last + 1
        )
        .expect("writing to String cannot fail");
    } else {
        text.push_str("\nComplete.");
    }
    Ok(ToolOutput::with_images(text, images))
}

fn check_pdf_cancellation(cancellation: &CancellationToken) -> Result<(), ReadError> {
    if cancellation.is_cancelled() {
        Err(ReadError::Cancelled)
    } else {
        Ok(())
    }
}
