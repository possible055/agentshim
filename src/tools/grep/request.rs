use std::{io, sync::Arc};

use globset::GlobBuilder;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "bench-internals")]
use super::profile::ProfiledGrep;
use super::{
    candidates::{Candidate, collect_candidates},
    file_search::SearchPlan,
    ordered::{OrderedSearchContext, ordered_search},
    profile::{GrepProfiler, GrepStage},
    result::render,
};
use crate::{
    path::{FileAccess, PathError},
    runtime::{MemoryReservation, RuntimeResources},
    tools::ToolOutput,
    traversal::TraversalError,
};

pub(super) const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1_000;
const MAX_CONTEXT: usize = 20;
#[cfg(any(test, feature = "bench-internals"))]
pub(super) const CANDIDATE_SOFT_TARGET_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MEMORY_SOURCE_BYTES: usize = 16 * 1024;
pub(super) const SEARCH_HEAP_BYTES: usize = 1024 * 1024;
pub(super) const CAPTURE_MEMORY_BYTES: usize = 1024 * 1024;
pub(super) const PAGE_MEMORY_BYTES: usize = 48 * 1024;
pub(super) const PARALLEL_BATCH_SIZE: usize = 256;
pub(super) const CONTENT_SEARCH_BATCH_SIZE: usize = 8;
pub(super) const STREAM_SEARCH_BATCH_SIZE: usize = 16;
pub(super) const UNSTABLE_SORT_MIN_CANDIDATES: usize = 10_000;
pub(super) const GENERIC_OMISSION: &str = "[grep result omitted: exceeds output budget]";
pub(super) const CONTENT_OMISSION: &str = "[line text omitted: exceeds output budget]";
#[cfg(feature = "bench-internals")]
const BENCH_SOURCE_ENV: &str = "CODEXSHIM_BENCH_GREP_SOURCE";
#[cfg(feature = "bench-internals")]
const BENCH_PATHNAME_REOPEN_ENV: &str = "CODEXSHIM_BENCH_GREP_PATHNAME_REOPEN";
#[cfg(feature = "bench-internals")]
const BENCH_CANDIDATE_POLICY_ENV: &str = "CODEXSHIM_BENCH_GREP_CANDIDATE_POLICY";
#[cfg(feature = "bench-internals")]
pub(super) const BENCH_SORT_ENV: &str = "CODEXSHIM_BENCH_GREP_SORT";
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrepTraversal {
    Adaptive,
    Serial,
    ParallelBatched,
    #[cfg(any(test, feature = "bench-internals"))]
    SerialLiteralPrefix,
    #[cfg(any(test, feature = "bench-internals"))]
    ParallelBatchedLiteralPrefix,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrepSourcePolicy {
    Hybrid,
    #[cfg(any(test, feature = "bench-internals"))]
    CaptureLimit(u64),
    #[cfg(any(test, feature = "bench-internals"))]
    Reader,
    #[cfg(any(test, feature = "bench-internals"))]
    FileNever,
    #[cfg(any(test, feature = "bench-internals"))]
    MmapAlways,
    #[cfg(any(test, feature = "bench-internals"))]
    MmapThreshold(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathnameReopenPolicy {
    Off,
    #[cfg(any(test, feature = "bench-internals"))]
    On,
    #[cfg(any(test, feature = "bench-internals"))]
    ParentBatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GrepBenchmarkVariant {
    pub source: GrepSourcePolicy,
    pub pathname_reopen: PathnameReopenPolicy,
}

impl Default for GrepBenchmarkVariant {
    fn default() -> Self {
        Self {
            source: GrepSourcePolicy::Hybrid,
            pathname_reopen: PathnameReopenPolicy::Off,
        }
    }
}

impl GrepBenchmarkVariant {
    #[cfg(feature = "bench-internals")]
    fn from_env() -> Result<Self, GrepError> {
        let source = match std::env::var(BENCH_SOURCE_ENV).as_deref() {
            Ok("hybrid") | Err(std::env::VarError::NotPresent) => GrepSourcePolicy::Hybrid,
            Ok("reader") => GrepSourcePolicy::Reader,
            Ok("file-never") => GrepSourcePolicy::FileNever,
            Ok("mmap-always") => GrepSourcePolicy::MmapAlways,
            Ok(value) if value.starts_with("capture-limit:") => {
                let bytes = value["capture-limit:".len()..]
                    .parse::<u64>()
                    .ok()
                    .filter(|bytes| *bytes > 0)
                    .ok_or_else(|| {
                        GrepError::Validation(format!(
                            "{BENCH_SOURCE_ENV} capture limit must be a positive byte count"
                        ))
                    })?;
                GrepSourcePolicy::CaptureLimit(bytes)
            }
            Ok(value) if value.starts_with("mmap-threshold:") => {
                let bytes = value["mmap-threshold:".len()..]
                    .parse::<u64>()
                    .ok()
                    .filter(|bytes| *bytes > 0)
                    .ok_or_else(|| {
                        GrepError::Validation(format!(
                            "{BENCH_SOURCE_ENV} mmap threshold must be a positive byte count"
                        ))
                    })?;
                GrepSourcePolicy::MmapThreshold(bytes)
            }
            Ok(value) => {
                return Err(GrepError::Validation(format!(
                    "{BENCH_SOURCE_ENV} must be hybrid, capture-limit:<bytes>, reader, \
                     file-never, mmap-always, or mmap-threshold:<bytes>; got {value}"
                )));
            }
            Err(error) => {
                return Err(GrepError::Validation(format!(
                    "{BENCH_SOURCE_ENV} is not valid Unicode: {error}"
                )));
            }
        };
        let pathname_reopen = match std::env::var(BENCH_PATHNAME_REOPEN_ENV).as_deref() {
            Ok("off") | Err(std::env::VarError::NotPresent) => PathnameReopenPolicy::Off,
            Ok("on") => PathnameReopenPolicy::On,
            Ok("parent-batch") => PathnameReopenPolicy::ParentBatch,
            Ok(value) => {
                return Err(GrepError::Validation(format!(
                    "{BENCH_PATHNAME_REOPEN_ENV} must be on, off, or parent-batch; got {value}"
                )));
            }
            Err(error) => {
                return Err(GrepError::Validation(format!(
                    "{BENCH_PATHNAME_REOPEN_ENV} is not valid Unicode: {error}"
                )));
            }
        };
        Ok(Self {
            source,
            pathname_reopen,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CandidatePolicy {
    SoftTarget,
    #[cfg(any(test, feature = "bench-internals"))]
    FatalCeiling,
}

impl CandidatePolicy {
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn from_environment() -> Result<Self, GrepError> {
        #[cfg(feature = "bench-internals")]
        {
            match std::env::var(BENCH_CANDIDATE_POLICY_ENV).as_deref() {
                Ok("soft" | "unlimited") | Err(std::env::VarError::NotPresent) => {
                    Ok(Self::SoftTarget)
                }
                Ok("fatal-64m") => Ok(Self::FatalCeiling),
                Ok(value) => Err(GrepError::Validation(format!(
                    "{BENCH_CANDIDATE_POLICY_ENV} must be soft or fatal-64m; got {value}"
                ))),
                Err(error) => Err(GrepError::Validation(format!(
                    "{BENCH_CANDIDATE_POLICY_ENV} is not valid Unicode: {error}"
                ))),
            }
        }
        #[cfg(not(feature = "bench-internals"))]
        {
            Ok(Self::SoftTarget)
        }
    }
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
    #[error(
        "grep candidates exceed the configured memory limit; narrow path/glob or adjust \
         CODEXSHIM_GREP_MEMORY_BYTES"
    )]
    CandidateMemory,
    #[error("grep memory capacity is busy; retry the request later")]
    MemoryBusy,
    #[error("grep worker pool state was poisoned")]
    PoolPoison,
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
    with_benchmark_resources(lanes, |resources| {
        execute_inner(
            access,
            request,
            resources,
            cancellation,
            GrepExecution {
                traversal: GrepTraversal::Adaptive,
                variant: GrepBenchmarkVariant::default(),
                profiler: &GrepProfiler::disabled(),
                memory: None,
            },
        )
        .map(|result| result.text)
    })
}

#[must_use]
pub(crate) const fn base_memory_charge() -> usize {
    SEARCH_HEAP_BYTES
}

#[cfg(any(test, feature = "bench-internals"))]
/// Execute grep with an explicit candidate traversal strategy.
///
/// # Errors
///
/// Returns the same validation, traversal, search, cancellation, and output errors as grep.
pub fn execute_with_traversal(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    lanes: usize,
    cancellation: &CancellationToken,
    traversal: GrepTraversal,
) -> Result<String, GrepError> {
    with_benchmark_resources(lanes, |resources| {
        execute_inner(
            access,
            request,
            resources,
            cancellation,
            GrepExecution {
                traversal,
                variant: GrepBenchmarkVariant::default(),
                profiler: &GrepProfiler::disabled(),
                memory: None,
            },
        )
        .map(|result| result.text)
    })
}

#[cfg(any(test, feature = "bench-internals"))]
/// Execute grep with explicit traversal and benchmark source-validation policies.
///
/// # Errors
///
/// Returns the same validation, traversal, search, cancellation, and output errors as grep.
pub fn execute_with_variant(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    lanes: usize,
    cancellation: &CancellationToken,
    traversal: GrepTraversal,
    variant: GrepBenchmarkVariant,
) -> Result<String, GrepError> {
    with_benchmark_resources(lanes, |resources| {
        execute_inner(
            access,
            request,
            resources,
            cancellation,
            GrepExecution {
                traversal,
                variant,
                profiler: &GrepProfiler::disabled(),
                memory: None,
            },
        )
        .map(|result| result.text)
    })
}

#[cfg(feature = "bench-internals")]
/// Execute grep and return benchmark-only stage timings.
///
/// # Errors
///
/// Returns the same validation, traversal, search, cancellation, and output errors as grep.
pub fn execute_profiled(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    lanes: usize,
    cancellation: &CancellationToken,
) -> Result<ProfiledGrep, GrepError> {
    execute_profiled_with_traversal(
        access,
        request,
        lanes,
        cancellation,
        GrepTraversal::Adaptive,
    )
}

#[cfg(feature = "bench-internals")]
/// Execute profiled grep with an explicit candidate traversal strategy.
///
/// # Errors
///
/// Returns the same validation, traversal, search, cancellation, and output errors as grep.
pub fn execute_profiled_with_traversal(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    lanes: usize,
    cancellation: &CancellationToken,
    traversal: GrepTraversal,
) -> Result<ProfiledGrep, GrepError> {
    execute_profiled_with_variant(
        access,
        request,
        lanes,
        cancellation,
        traversal,
        GrepBenchmarkVariant::default(),
    )
}

#[cfg(feature = "bench-internals")]
/// Execute profiled grep with explicit traversal and source-validation policies.
///
/// # Errors
///
/// Returns the same validation, traversal, search, cancellation, and output errors as grep.
pub fn execute_profiled_with_variant(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    lanes: usize,
    cancellation: &CancellationToken,
    traversal: GrepTraversal,
    variant: GrepBenchmarkVariant,
) -> Result<ProfiledGrep, GrepError> {
    with_benchmark_resources(lanes, |resources| {
        let profiler = GrepProfiler::enabled();
        let total_span = profiler.span(GrepStage::Total);
        let output = execute_inner(
            access,
            request,
            resources,
            cancellation,
            GrepExecution {
                traversal,
                variant,
                profiler: &profiler,
                memory: None,
            },
        )?
        .text;
        drop(total_span);
        Ok(ProfiledGrep {
            output,
            timings: profiler.snapshot(),
        })
    })
}

#[cfg(any(test, feature = "bench-internals"))]
fn with_benchmark_resources<T>(
    lanes: usize,
    execute: impl FnOnce(&RuntimeResources) -> Result<T, GrepError>,
) -> Result<T, GrepError> {
    let resources = RuntimeResources::new(crate::runtime::RuntimeConfig::for_tests(lanes));
    let _worker = resources
        .try_acquire_worker()
        .ok_or_else(|| io::Error::other("benchmark base worker permit unavailable"))?;
    let _open_file = resources
        .try_acquire_open_file()
        .ok_or_else(|| io::Error::other("benchmark base open-file permit unavailable"))?;
    let _memory = resources
        .try_reserve_memory(base_memory_charge())
        .ok_or_else(|| io::Error::other("benchmark base memory permit unavailable"))?;
    execute(&resources)
}

pub(crate) fn execute_output(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    resources: &RuntimeResources,
    cancellation: &CancellationToken,
    memory: MemoryReservation,
) -> Result<ToolOutput, GrepError> {
    #[cfg(feature = "bench-internals")]
    let variant = GrepBenchmarkVariant::from_env()?;
    #[cfg(not(feature = "bench-internals"))]
    let variant = GrepBenchmarkVariant::default();
    execute_inner(
        access,
        request,
        resources,
        cancellation,
        GrepExecution {
            traversal: GrepTraversal::Adaptive,
            variant,
            profiler: &GrepProfiler::disabled(),
            memory: Some(memory),
        },
    )
}

struct GrepExecution<'a> {
    traversal: GrepTraversal,
    variant: GrepBenchmarkVariant,
    profiler: &'a GrepProfiler,
    memory: Option<MemoryReservation>,
}

fn execute_inner(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    resources: &RuntimeResources,
    cancellation: &CancellationToken,
    execution: GrepExecution<'_>,
) -> Result<ToolOutput, GrepError> {
    let GrepExecution {
        traversal,
        variant,
        profiler,
        memory,
    } = execution;
    let candidate_policy = CandidatePolicy::from_environment()?;
    let file_work_pool = resources.file_work_pool();
    let _file_work_request = file_work_pool.begin_request();
    let setup_span = profiler.span(GrepStage::Setup);
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
    #[cfg(any(test, feature = "bench-internals"))]
    let literal_prefix = request
        .glob
        .as_deref()
        .and_then(crate::traversal::literal_path_prefix);
    drop(setup_span);
    let (candidates, traversal_summary, single_file) = collect_candidates(
        access,
        request.path.as_deref().unwrap_or("."),
        glob.as_ref(),
        cancellation,
        traversal,
        resources,
        #[cfg(any(test, feature = "bench-internals"))]
        literal_prefix.as_deref(),
        candidate_policy,
        profiler,
        memory,
    )
    .map_err(normalize_cancellation)?;
    let skipped = traversal_summary.io_errors
        + traversal_summary.escaped_entries
        + traversal_summary.non_unicode_entries;
    if skipped > 0 {
        tracing::warn!(target: "codexshim", event = "grep_skipped", phase = "execution", outcome = "degraded_success", counters = %format!("io_errors={},escaped_entries={},non_unicode_entries={}", traversal_summary.io_errors, traversal_summary.escaped_entries, traversal_summary.non_unicode_entries));
    }
    let probe = request
        .offset
        .unwrap_or(0)
        .saturating_add(request.limit.unwrap_or(DEFAULT_LIMIT))
        .saturating_add(1);
    let mode = request.mode.unwrap_or_default();
    let plan = SearchPlan {
        mode,
        context: request.context_lines.unwrap_or(0),
        probe,
        allow_early_stop: !single_file && matches!(mode, GrepMode::Content | GrepMode::Files),
    };
    let lanes = resources
        .file_work_pool()
        .extra_capacity()
        .saturating_add(1)
        .clamp(1, candidates.len().max(1));
    profiler.set_workload(candidates.len(), lanes);
    let candidates: Arc<[Candidate]> = candidates.into();
    let access = Arc::clone(access);
    let context = OrderedSearchContext {
        cancellation,
        access: &access,
        matcher: &matcher,
        plan,
        single_file,
        variant,
        profiler,
        resources,
    };
    let search_span = profiler.span(GrepStage::SearchWall);
    let page = ordered_search(&candidates, lanes, request, traversal_summary, &context)?;
    drop(search_span);
    let render_span = profiler.span(GrepStage::Render);
    let output = render(request, &page, cancellation);
    drop(render_span);
    if let Ok(output) = &output {
        profiler.add_render_copy_bytes(output.text.len());
    }
    output
}

fn normalize_cancellation(error: GrepError) -> GrepError {
    if matches!(error, GrepError::Traversal(TraversalError::Cancelled)) {
        GrepError::Cancelled
    } else {
        error
    }
}

pub(super) fn build_matcher(request: &GrepRequest) -> Result<RegexMatcher, GrepError> {
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
