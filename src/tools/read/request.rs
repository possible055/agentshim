use std::{
    io::{self, Read},
    path::Path,
    sync::Arc,
};

use cap_std::fs::File;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::{
    encoding::{DecodeControl, DecodeError, SourceEncoding, decode_stream},
    output::{OutputFormatter, OutputLimits},
    path::{FileAccess, PathError, ResolvedPath},
    tools::ToolOutput,
};

const PREFIX_BYTES: usize = 8 * 1024;
const CANDIDATE_BYTES: usize = 64 * 1024;
const LINE_PREFIX_BYTES: usize = 8 * 1024;
const MAX_LINE_COUNT: usize = 2_000;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    pub path: String,
    pub start_line: Option<usize>,
    pub line_count: Option<usize>,
    pub encoding: Option<String>,
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
        Ok(())
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
    #[error("file changed while it was being read")]
    Changed,
    #[error("read cancelled")]
    Cancelled,
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error(transparent)]
    Output(#[from] crate::output::OutputError),
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
    access: &Arc<FileAccess>,
    request: &ReadRequest,
    cancellation: &CancellationToken,
) -> Result<String, ReadError> {
    execute_output(access, request, cancellation).map(|result| result.text)
}

pub(crate) fn execute_output(
    access: &Arc<FileAccess>,
    request: &ReadRequest,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ReadError> {
    execute_inner(access, request, cancellation)
}

fn execute_inner(
    access: &Arc<FileAccess>,
    request: &ReadRequest,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ReadError> {
    request.validate()?;
    let resolved = access.resolve(Path::new(&request.path))?;
    let absolute = crate::path::display_path(resolved.absolute());
    match read_once(access, &resolved, &absolute, request, cancellation)? {
        Attempt::Stable(output) => return Ok(output),
        Attempt::Changed => {
            tracing::warn!(target: "codexshim", event = "read_retry", phase = "execution", outcome = "degraded_success", reason = "file_changed");
        }
    }
    match read_once(access, &resolved, &absolute, request, cancellation) {
        Ok(Attempt::Stable(output)) => Ok(output),
        Ok(Attempt::Changed) => Err(ReadError::Changed),
        Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            Err(ReadError::Changed)
        }
        Err(error) => Err(error),
    }
}
