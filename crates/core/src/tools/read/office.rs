use cap_std::fs::File;
use std::fmt::Write as _;
use tokio_util::sync::CancellationToken;

use super::{
    cursor,
    request::{ReadError, ReadRequest},
};
use crate::{
    output::{CallBudget, OutputLimits, PARTIAL_MARKER},
    tools::ToolOutput,
};

#[allow(
    clippy::too_many_arguments,
    reason = "mirrors read_pdf's call shape plus the hint and reservation resolved once by prepare"
)]
pub fn read_office(
    file: &File,
    absolute: &str,
    request: &ReadRequest,
    source_id: &str,
    cancellation: &CancellationToken,
    hint: agentshim_office_read::OfficeFormat,
    call_bytes: usize,
    output_budget: &dyn CallBudget,
) -> Result<ToolOutput, ReadError> {
    validate_parameters(request)?;
    let requested = request.decoded_office_cursor()?;
    if requested.is_some_and(|value| value.source_id != source_id) {
        return Err(ReadError::Changed);
    }
    let cancel = cancellation.clone();
    let parser_file = file.try_clone()?.into_std();
    let mut document = agentshim_office_read::OfficeReadDocument::from_file(
        parser_file,
        hint,
        agentshim_office_read::OfficeReadLimits::within(call_bytes),
        agentshim_office_read::CancelSignal::new(move || cancel.is_cancelled()),
    )?;
    let logical = requested.map(|value| {
        agentshim_office_read::OfficeLogicalCursor::new(
            value.format,
            value.unit_index,
            value.offset,
        )
    });
    let header = format!(
        "Office: {} as Markdown\nSource: {absolute}\n\n",
        document.format().label()
    );
    let reserve = 256_usize;
    let maximum = output_budget
        .page_bytes()
        .saturating_sub(header.len())
        .saturating_sub(reserve)
        .max(1);
    let mut chunk = document.markdown_chunk(logical.as_ref(), maximum)?;
    let aware = OutputLimits::for_content_within(&chunk.markdown, output_budget.page_bytes()).bytes;
    let aware_maximum = aware
        .saturating_sub(header.len())
        .saturating_sub(reserve)
        .max(1);
    if aware_maximum < maximum {
        chunk = document.markdown_chunk(logical.as_ref(), aware_maximum)?;
    }
    let mut text = header;
    text.push_str(&chunk.markdown);
    if let Some(next) = chunk.next {
        let token =
            cursor::encode_office(source_id, chunk.format, next.unit_index(), next.offset());
        let _ = write!(
            text,
            "\n\n{PARTIAL_MARKER} Office content continues. Continue with office_cursor=\"{token}\"."
        );
    }
    Ok(ToolOutput::new(text))
}

fn validate_parameters(request: &ReadRequest) -> Result<(), ReadError> {
    if request.start_line.is_some()
        || request.line_count.is_some()
        || request.encoding.is_some()
        || request.pdf_mode.is_some()
        || request.pages.is_some()
        || request.pdf_cursor.is_some()
    {
        return Err(ReadError::Validation(
            "text and PDF parameters do not apply to Office input".to_owned(),
        ));
    }
    Ok(())
}

pub fn format_hint(path: &str) -> Option<agentshim_office_read::OfficeFormat> {
    let extension = std::path::Path::new(path).extension()?.to_str()?;
    match extension.to_ascii_lowercase().as_str() {
        "docx" => Some(agentshim_office_read::OfficeFormat::Docx),
        "xlsx" => Some(agentshim_office_read::OfficeFormat::Xlsx),
        "pptx" => Some(agentshim_office_read::OfficeFormat::Pptx),
        "doc" => Some(agentshim_office_read::OfficeFormat::Doc),
        "xls" => Some(agentshim_office_read::OfficeFormat::Xls),
        "ppt" => Some(agentshim_office_read::OfficeFormat::Ppt),
        _ => None,
    }
}
