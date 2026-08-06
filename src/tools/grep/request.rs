use std::{
    collections::BTreeMap,
    io::{self},
    path::Path,
    sync::{Arc, Condvar, Mutex},
};

use cap_std::fs::File;
use globset::GlobBuilder;
use grep_matcher::{LineTerminator, Matcher};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, MmapChoice, Searcher, SearcherBuilder, Sink, SinkContext, SinkError, SinkMatch,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    output::{MODEL_BYTE_LIMIT, OutputFormatter, OutputLimits},
    path::{FileAccess, PathError, ResolvedPath},
    sorting,
    tools::ToolOutput,
    tools::read::FileFingerprint,
    traversal::{TraversalControl, TraversalError, TraversalSummary, walk},
};

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1_000;
const MAX_CONTEXT: usize = 20;
const CANDIDATE_MEMORY_BYTES: usize = 8 * 1024 * 1024;
const SEARCH_HEAP_BYTES: usize = 1024 * 1024;
const CAPTURE_MEMORY_BYTES: usize = 1024 * 1024;
const PAGE_MEMORY_BYTES: usize = MODEL_BYTE_LIMIT;
const MEMORY_SAFETY_BYTES: usize = 8 * 1024 * 1024;
const ORDERED_WINDOW_FACTOR: usize = 1;
const GENERIC_OMISSION: &str = "[grep result omitted: exceeds output budget]";
const CONTENT_OMISSION: &str = "[line text omitted: exceeds output budget]";

#[must_use]
pub(crate) fn memory_charge(lanes: usize) -> usize {
    CANDIDATE_MEMORY_BYTES
        .saturating_add(
            lanes.saturating_mul(SEARCH_HEAP_BYTES.saturating_add(CAPTURE_MEMORY_BYTES)),
        )
        .saturating_add(PAGE_MEMORY_BYTES)
        .saturating_add(MEMORY_SAFETY_BYTES)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum GrepMode {
    #[default]
    Content,
    Files,
    Count,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CaseMode {
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrepRequest {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub mode: Option<GrepMode>,
    pub fixed_strings: Option<bool>,
    pub case: Option<CaseMode>,
    pub context_lines: Option<usize>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GrepItem {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GrepResult {
    pub mode: GrepMode,
    pub items: Vec<GrepItem>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub next_offset: Option<usize>,
    pub skipped: usize,
    pub traversal: TraversalSummary,
}

impl GrepRequest {
    /// Validate scalar constraints before regex compilation or filesystem I/O.
    ///
    /// # Errors
    ///
    /// Returns a validation error for NUL, context above 20, or limit outside 1..=1,000.
    pub fn validate(&self) -> Result<(), GrepError> {
        if self.pattern.contains('\0')
            || self.path.as_deref().is_some_and(|path| path.contains('\0'))
            || self.glob.as_deref().is_some_and(|glob| glob.contains('\0'))
        {
            return Err(GrepError::Validation(
                "pattern, path, and glob must not contain NUL".to_owned(),
            ));
        }
        if self.context_lines.unwrap_or(0) > MAX_CONTEXT {
            return Err(GrepError::Validation(
                "context_lines must be from 0 to 20".to_owned(),
            ));
        }
        if !(1..=MAX_LIMIT).contains(&self.limit.unwrap_or(DEFAULT_LIMIT)) {
            return Err(GrepError::Validation(
                "limit must be from 1 to 1000".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GrepError {
    #[error("invalid grep request: {0}")]
    Validation(String),
    #[error("Rust regex compile error: {0}; lookaround and backreferences are not supported")]
    Regex(String),
    #[error("invalid grep glob: {0}")]
    Glob(String),
    #[error("grep candidates exceed the bounded memory budget; narrow path or glob")]
    CandidateMemory,
    #[error("grep matching content exceeds the bounded capture budget; narrow the query")]
    CaptureMemory,
    #[error("grep cancelled")]
    Cancelled,
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Traversal(#[from] TraversalError),
    #[error(transparent)]
    Output(#[from] crate::output::OutputError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Search policy-admitted files with an ordered bounded worker window.
///
/// # Errors
///
/// Returns validation, regex, traversal, resource, cancellation, I/O, or output errors.
#[cfg(any(test, feature = "bench-internals"))]
pub fn execute(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    lanes: usize,
    cancellation: &CancellationToken,
) -> Result<String, GrepError> {
    execute_output(access, request, lanes, cancellation).map(|result| result.text)
}

pub(crate) fn execute_output(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    lanes: usize,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, GrepError> {
    execute_inner(access, request, lanes, cancellation)
}

fn execute_inner(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    lanes: usize,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, GrepError> {
    request.validate()?;
    let matcher = Arc::new(build_matcher(request)?);
    let glob = request
        .glob
        .as_deref()
        .map(|pattern| {
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .map(|glob| glob.compile_matcher())
                .map_err(|error| GrepError::Glob(error.to_string()))
        })
        .transpose()?;
    let (candidates, traversal_summary, single_file) = collect_candidates(
        access,
        request.path.as_deref().unwrap_or("."),
        glob.as_ref(),
        cancellation,
    )
    .map_err(normalize_cancellation)?;
    let skipped = traversal_summary.io_errors
        + traversal_summary.escaped_entries
        + traversal_summary.non_unicode_entries;
    if skipped > 0 {
        tracing::warn!(target: "codexshim", event = "grep_skipped", phase = "execution", outcome = "degraded_success", counters = %format!("io_errors={},escaped_entries={},non_unicode_entries={}", traversal_summary.io_errors, traversal_summary.escaped_entries, traversal_summary.non_unicode_entries));
    }
    let needed = request
        .offset
        .unwrap_or(0)
        .saturating_add(request.limit.unwrap_or(DEFAULT_LIMIT))
        .saturating_add(1);
    let plan = SearchPlan {
        mode: request.mode.unwrap_or_default(),
        context: request.context_lines.unwrap_or(0),
        capture_records: needed,
    };
    let lanes = lanes.clamp(1, candidates.len().max(1));
    let access = Arc::clone(access);
    let context = OrderedSearchContext {
        cancellation,
        access: &access,
        matcher: &matcher,
        plan,
        single_file,
    };
    let page = ordered_search(&candidates, lanes, request, traversal_summary, &context)?;
    render(request, &page, cancellation)
}

fn normalize_cancellation(error: GrepError) -> GrepError {
    if matches!(error, GrepError::Traversal(TraversalError::Cancelled)) {
        GrepError::Cancelled
    } else {
        error
    }
}

fn build_matcher(request: &GrepRequest) -> Result<RegexMatcher, GrepError> {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .fixed_strings(request.fixed_strings.unwrap_or(false))
        .crlf(true)
        .ban_byte(Some(0));
    match request.case.unwrap_or_default() {
        CaseMode::Smart => {
            builder.case_smart(true);
        }
        CaseMode::Sensitive => {
            builder.case_insensitive(false);
        }
        CaseMode::Insensitive => {
            builder.case_insensitive(true);
        }
    }
    builder
        .build(&request.pattern)
        .map_err(|error| GrepError::Regex(error.to_string()))
}
