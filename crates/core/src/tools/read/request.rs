use std::io;

use serde::Deserialize;
#[cfg(any(test, feature = "bench-internals"))]
use tokio_util::sync::CancellationToken;

use super::cursor::{self, PdfCursor};
use super::pdf::parse_page_selector;
#[cfg(any(test, feature = "bench-internals"))]
use super::prepared::{Attempt, PdfMemoryBudgets, execute_prepared, prepare};
use crate::{encoding::DecodeError, path::PathError};
#[cfg(any(test, feature = "bench-internals"))]
use crate::{path::FileAccess, tools::ToolOutput};

pub const PREFIX_BYTES: usize = 8 * 1024;
pub const CANDIDATE_BYTES: usize = 256 * 1024;
pub const LINE_PREFIX_BYTES: usize = 8 * 1024;
pub const MAX_LINE_COUNT: usize = 2_000;
pub const TEXT_READ_MEMORY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PdfMode {
    Auto,
    Text,
    Image,
}

/// `Auto` is the documented default, but it must never become a `serde` default.
/// `has_pdf_parameters()` treats a present `pdf_mode` as "route this to the PDF
/// reader", so a defaulted field would send every plain-text read down the PDF path and
/// charge it PDF memory instead of the 256 KiB text budget. The default is applied with
/// `unwrap_or` inside the PDF branch only.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    pub path: String,
    pub start_line: Option<usize>,
    pub line_count: Option<usize>,
    pub encoding: Option<String>,
    pub pdf_mode: Option<PdfMode>,
    pub pages: Option<String>,
    pub pdf_cursor: Option<String>,
}

impl ReadRequest {
    /// Validate the request ranges and string contract before filesystem I/O.
    ///
    /// # Errors
    ///
    /// Returns a validation error for empty/NUL path, zero line values, or a
    /// `line_count` above 2,000.
    pub fn validate(&self) -> Result<(), ReadError> {
        if self.path.is_empty() {
            return Err(ReadError::Validation("path must not be empty".to_owned()));
        }
        if self.path.contains('\0') {
            return Err(ReadError::Validation(
                "path must not contain NUL".to_owned(),
            ));
        }
        if self.start_line == Some(0) {
            return Err(ReadError::Validation(
                "start_line must be at least 1".to_owned(),
            ));
        }
        if let Some(line_count) = self.line_count
            && !(1..=MAX_LINE_COUNT).contains(&line_count)
        {
            return Err(ReadError::Validation(
                "line_count must be from 1 to 2000".to_owned(),
            ));
        }
        self.validate_continuation()?;
        Ok(())
    }

    /// Continuation parameters constrain each other regardless of the file's type, so
    /// they are checked before any filesystem work.
    fn validate_continuation(&self) -> Result<(), ReadError> {
        let Some(cursor) = self.decoded_pdf_cursor()? else {
            return Ok(());
        };
        if cursor.text_offset.is_none() {
            return Ok(());
        }
        if self.pdf_mode == Some(PdfMode::Image) {
            return Err(ReadError::Validation(
                "a pdf_cursor that resumes inside a page does not apply to pdf_mode=\"image\""
                    .to_owned(),
            ));
        }
        let single_page = match self.pages.as_deref() {
            Some(selector) => {
                let (start, end) = parse_page_selector(selector)?;
                start == end
            }
            None => false,
        };
        if single_page {
            Ok(())
        } else {
            Err(ReadError::Validation(
                "a pdf_cursor that resumes inside a page requires pages to name exactly one page"
                    .to_owned(),
            ))
        }
    }

    /// # Errors
    ///
    /// Returns a validation error when the token was not produced by this server.
    pub fn decoded_pdf_cursor(&self) -> Result<Option<PdfCursor<'_>>, ReadError> {
        self.pdf_cursor.as_deref().map(cursor::decode).transpose()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("invalid read request: {0}")]
    Validation(String),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("path cannot be represented losslessly in model-visible JSON")]
    #[allow(dead_code)]
    NonUnicodePath,
    #[error("target is a directory; use glob to list its contents")]
    Directory,
    #[error("target is not a regular file")]
    NotRegular,
    #[error("cannot read binary, image, PDF, or executable content as source text")]
    Binary,
    #[error("read resource limit exceeded: {message}")]
    ResourceLimit {
        message: String,
        /// Names the budget that was hit so callers can tell a payload cap apart from a
        /// stream cap without parsing the message.
        resource: &'static str,
        limit_bytes: Option<u64>,
        observed_bytes: Option<u64>,
    },
    #[error(
        "no selected PDF page has extractable text; retry pages {pages} with pdf_mode=\"image\""
    )]
    PdfImageRequired { pages: String, cursor: String },
    /// Every selected page failed to process. A single failed page is a placeholder;
    /// only a selection with nothing usable in it becomes an error.
    #[error("PDF processing failed: {0}")]
    PdfProcessing(String),
    #[error("file changed while it was being read")]
    Changed,
    #[error("read cancelled")]
    Cancelled,
    #[error("read resource {resource} is busy")]
    ResourceBusy {
        resource: &'static str,
        retry_after: Option<std::time::Duration>,
    },
    #[error("PDF read exceeded its mode runtime limit")]
    ResourceTimeout {
        limit: std::time::Duration,
        elapsed: std::time::Duration,
    },
    #[error("read worker failed: {0}")]
    Worker(String),
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error(transparent)]
    Output(#[from] crate::output::OutputError),
    #[error(transparent)]
    Pdf(#[from] agentshim_pdf_read::PdfReadError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Read one regular source file through the configured filesystem access policy.
///
/// # Errors
///
/// Returns a validation, path, I/O, encoding, consistency, cancellation, or
/// output-budget error without returning a mixed file version.
#[cfg(any(test, feature = "bench-internals"))]
pub fn execute(
    access: &std::sync::Arc<FileAccess>,
    request: &ReadRequest,
    cancellation: &CancellationToken,
) -> Result<String, ReadError> {
    execute_output(access, request, cancellation).map(|result| result.text)
}

#[cfg(any(test, feature = "bench-internals"))]
pub fn execute_output(
    access: &std::sync::Arc<FileAccess>,
    request: &ReadRequest,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ReadError> {
    execute_inner(access, request, cancellation)
}

#[cfg(any(test, feature = "bench-internals"))]
fn execute_inner(
    access: &std::sync::Arc<FileAccess>,
    request: &ReadRequest,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ReadError> {
    let prepared = prepare(access, request, cancellation, PdfMemoryBudgets::defaults())?;
    match execute_prepared(access, request, prepared, cancellation)? {
        Attempt::Stable(output) => return Ok(output),
        Attempt::Changed => {
            tracing::warn!(target: "agentshim", event = "read_retry", phase = "execution", outcome = "degraded_success", reason = "file_changed");
        }
    }
    let prepared = match prepare(access, request, cancellation, PdfMemoryBudgets::defaults()) {
        Ok(prepared) => prepared,
        Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ReadError::Changed);
        }
        Err(error) => return Err(error),
    };
    match execute_prepared(access, request, prepared, cancellation) {
        Ok(Attempt::Stable(output)) => Ok(output),
        Ok(Attempt::Changed) => Err(ReadError::Changed),
        Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            Err(ReadError::Changed)
        }
        Err(error) => Err(error),
    }
}
