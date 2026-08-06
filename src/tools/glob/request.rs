use std::{cmp::Ordering, collections::BinaryHeap, io, path::Path, sync::Arc};

use globset::GlobBuilder;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    output::{OutputFormatter, OutputLimits},
    path::{FileAccess, PathError, PathSortKey, ResolvedPath},
    sorting,
    tools::ToolOutput,
    traversal::{TraversalControl, TraversalError, TraversalSummary, walk},
};

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1_000;
const MAX_MATCHES: usize = 100_000;
const RETAINED_MEMORY_BYTES: usize = 32 * 1024 * 1024;
const MEMORY_SAFETY_BYTES: usize = 8 * 1024 * 1024;
const PATH_OMISSION: &str = "[glob path omitted: exceeds output budget]";

#[must_use]
pub(crate) fn memory_charge() -> usize {
    RETAINED_MEMORY_BYTES.saturating_add(MEMORY_SAFETY_BYTES)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GlobRequest {
    pub pattern: String,
    pub path: Option<String>,
    pub include_ignored: Option<bool>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GlobItem {
    pub path: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GlobResult {
    pub items: Vec<GlobItem>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub next_offset: Option<usize>,
    pub skipped: TraversalSummary,
}

impl GlobRequest {
    /// Validate scalar request constraints before traversal.
    ///
    /// # Errors
    ///
    /// Returns a validation error for empty/NUL values or a limit outside 1..=1,000.
    pub fn validate(&self) -> Result<(), GlobError> {
        if self.pattern.is_empty() {
            return Err(GlobError::Validation(
                "pattern must not be empty".to_owned(),
            ));
        }
        if self.pattern.contains('\0')
            || self.path.as_deref().is_some_and(|path| path.contains('\0'))
        {
            return Err(GlobError::Validation(
                "pattern and path must not contain NUL".to_owned(),
            ));
        }
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(GlobError::Validation(
                "limit must be from 1 to 1000".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GlobError {
    #[error("invalid glob request: {0}")]
    Validation(String),
    #[error("invalid glob pattern: {0}")]
    Pattern(String),
    #[error("more than 100000 paths matched; narrow pattern or path")]
    TooManyMatches,
    #[error("retained glob paths exceed the bounded memory budget; narrow pattern or offset")]
    Memory,
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Traversal(#[from] TraversalError),
    #[error(transparent)]
    Output(#[from] crate::output::OutputError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Find logical-root-relative pattern matches with deterministic Top-K pagination.
///
/// # Errors
///
/// Returns validation, traversal, match-limit, memory, cancellation, or output errors.
#[cfg(any(test, feature = "bench-internals"))]
pub fn execute(
    access: &Arc<FileAccess>,
    request: &GlobRequest,
    cancellation: &CancellationToken,
) -> Result<String, GlobError> {
    execute_output(access, request, cancellation).map(|result| result.text)
}

pub(crate) fn execute_output(
    access: &Arc<FileAccess>,
    request: &GlobRequest,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, GlobError> {
    execute_inner(access, request, cancellation)
}

fn execute_inner(
    access: &Arc<FileAccess>,
    request: &GlobRequest,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, GlobError> {
    request.validate()?;
    let matcher = GlobBuilder::new(&request.pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map_err(|error| GlobError::Pattern(error.to_string()))?
        .compile_matcher();
    let base_input = request.path.as_deref().unwrap_or(".");
    let base = access.resolve(Path::new(base_input))?;
    let offset = request.offset.unwrap_or(0);
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let retain = offset.saturating_add(limit).min(MAX_MATCHES);
    let mut store = TopK::new(retain);
    let mut total = 0_usize;
    let mut terminal_error = None;
    let summary = walk(
        access,
        &base,
        request.include_ignored.unwrap_or(false),
        cancellation,
        |entry| {
            if !matcher.is_match(entry.key) {
                return TraversalControl::Continue;
            }
            if let Err(error) = record_match(&mut total) {
                terminal_error = Some(error);
                return TraversalControl::Stop;
            }
            let path = match access.resolve_traversal_entry(&base, entry.absolute) {
                Ok(path) => path,
                Err(error) => {
                    terminal_error = Some(error.into());
                    return TraversalControl::Stop;
                }
            };
            if let Err(error) = store.admit(&path) {
                terminal_error = Some(error);
                return TraversalControl::Stop;
            }
            TraversalControl::Continue
        },
    )?;
    if let Some(error) = terminal_error {
        return Err(error);
    }
    let skipped = summary.io_errors + summary.escaped_entries + summary.non_unicode_entries;
    if skipped > 0 {
        tracing::warn!(target: "codexshim", event = "traversal_skipped", phase = "execution", outcome = "degraded_success", counters = %format!("io_errors={},escaped_entries={},non_unicode_entries={}", summary.io_errors, summary.escaped_entries, summary.non_unicode_entries));
    }
    let retained = store.into_sorted(cancellation)?;
    render(request, &retained, total, summary, cancellation)
}

fn record_match(total: &mut usize) -> Result<(), GlobError> {
    if *total >= MAX_MATCHES {
        return Err(GlobError::TooManyMatches);
    }
    *total += 1;
    Ok(())
}
