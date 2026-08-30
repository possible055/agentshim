use std::{io, sync::Arc};

use globset::GlobBuilder;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "bench-internals")]
use super::profile::ProfiledGrep;
use super::{
    file_search::SearchPlan,
    pipeline::pipelined_search,
    profile::{GrepProfiler, GrepStage},
    result::render_with_budget,
};
use crate::{
    path::{FileAccess, PathError},
    runtime::{MemoryReservation, RuntimeResources},
    tools::ToolOutput,
    traversal::TraversalError,
};

pub const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1_000;
const MAX_CONTEXT: usize = 20;
pub const MEMORY_SOURCE_BYTES: usize = 256 * 1024;
pub const SEARCH_HEAP_BYTES: usize = 8 * 1024 * 1024;
pub const CAPTURE_MEMORY_BYTES: usize = 8 * 1024 * 1024;
pub const PAGE_MEMORY_BYTES: usize = 512 * 1024;
const CANDIDATE_HEADROOM_BYTES: usize = 1024 * 1024;
pub const PARALLEL_BATCH_SIZE: usize = 256;
pub const GENERIC_OMISSION: &str = "[grep result omitted: exceeds output budget]";
pub const CONTENT_OMISSION: &str = "[line text omitted: exceeds output budget]";

#[derive(Clone, Copy, Debug)]
#[allow(
    clippy::struct_field_names,
    reason = "unit suffixes prevent memory-budget fields from being combined as unitless values"
)]
pub struct GrepMemoryPolicy {
    pub request_bytes: usize,
    pub base_search_heap_bytes: usize,
    pub capture_bytes: usize,
    pub page_bytes: usize,
    pub memory_source_bytes: usize,
    pub decode_input_bytes: usize,
    pub decode_output_bytes: usize,
}

impl GrepMemoryPolicy {
    pub fn new(request_bytes: usize) -> Self {
        let page_bytes = PAGE_MEMORY_BYTES.min(request_bytes);
        let lane_budget = request_bytes
            .saturating_sub(page_bytes)
            .saturating_sub(CANDIDATE_HEADROOM_BYTES.min(request_bytes / 8));
        let memory_source_bytes = MEMORY_SOURCE_BYTES.min(lane_budget / 8);
        let decode_input_bytes = (64 * 1024).min(lane_budget / 16);
        let decode_output_bytes = (64 * 1024).min(lane_budget / 16);
        let search_and_capture = lane_budget
            .saturating_sub(memory_source_bytes)
            .saturating_sub(decode_input_bytes)
            .saturating_sub(decode_output_bytes);
        let base_search_heap_bytes = SEARCH_HEAP_BYTES.min(search_and_capture / 2);
        let capture_bytes =
            CAPTURE_MEMORY_BYTES.min(search_and_capture.saturating_sub(base_search_heap_bytes));
        Self {
            request_bytes,
            base_search_heap_bytes,
            capture_bytes,
            page_bytes,
            memory_source_bytes,
            decode_input_bytes,
            decode_output_bytes,
        }
    }

    #[cfg(test)]
    pub const fn candidate_only(request_bytes: usize) -> Self {
        Self {
            request_bytes,
            base_search_heap_bytes: 0,
            capture_bytes: 0,
            page_bytes: 0,
            memory_source_bytes: 0,
            decode_input_bytes: 0,
            decode_output_bytes: 0,
        }
    }

    pub const fn base_lane_bytes(self) -> usize {
        self.base_search_heap_bytes
            .saturating_add(self.capture_bytes)
            .saturating_add(self.memory_source_bytes)
            .saturating_add(self.decode_input_bytes)
            .saturating_add(self.decode_output_bytes)
    }

    pub const fn base_reservation_bytes(self) -> usize {
        self.base_lane_bytes().saturating_add(self.page_bytes)
    }

    #[cfg(test)]
    pub const fn candidate_limit_bytes(self) -> usize {
        self.request_bytes
            .saturating_sub(self.base_reservation_bytes())
    }

    pub const fn speculative_batch_bytes(self, mode: GrepMode, batch_len: usize) -> usize {
        match mode {
            GrepMode::Content => self
                .base_search_heap_bytes
                .saturating_add(self.memory_source_bytes)
                .saturating_add(self.decode_input_bytes)
                .saturating_add(self.decode_output_bytes)
                .saturating_add(self.capture_bytes.saturating_mul(batch_len)),
            GrepMode::Files | GrepMode::Count => self
                .base_search_heap_bytes
                .saturating_add(self.memory_source_bytes)
                .saturating_add(self.decode_input_bytes)
                .saturating_add(self.decode_output_bytes),
        }
    }

    pub fn large_search_heap_ceiling(self) -> usize {
        const LARGE_SEARCH_CAP_BYTES: usize = 256 * 1024 * 1024;
        self.request_bytes
            .saturating_sub(self.capture_bytes)
            .saturating_sub(self.page_bytes)
            .saturating_sub(self.memory_source_bytes)
            .saturating_sub(self.decode_input_bytes)
            .saturating_sub(self.decode_output_bytes)
            .min(LARGE_SEARCH_CAP_BYTES)
            .max(self.base_search_heap_bytes)
    }
}
#[cfg(feature = "bench-internals")]
const BENCH_SOURCE_ENV: &str = "AGENTSHIM_BENCH_GREP_SOURCE";
#[cfg(feature = "bench-internals")]
const BENCH_PATHNAME_REOPEN_ENV: &str = "AGENTSHIM_BENCH_GREP_PATHNAME_REOPEN";
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrepTraversal {
    Adaptive,
    #[cfg(any(test, feature = "bench-internals"))]
    Serial,
    #[cfg(any(test, feature = "bench-internals"))]
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
    pub include_ignored: Option<bool>,
    pub encoding: Option<String>,
    pub fallback_encoding: Option<String>,
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
        if self.encoding.is_some() && self.fallback_encoding.is_some() {
            return Err(GrepError::Validation(
                "encoding and fallback_encoding are mutually exclusive".to_owned(),
            ));
        }
        self.encoding_labels()?;
        Ok(())
    }

    /// Resolve both encoding arguments against the target kind that was just determined.
    ///
    /// Each argument is rejected for the wrong kind rather than ignored: an argument that
    /// silently does nothing is exactly the failure this contract exists to prevent.
    ///
    /// # Errors
    ///
    /// Returns a validation error for an unknown label or a target-kind mismatch.
    fn resolved_encodings_inner(
        &self,
        single_file: bool,
    ) -> Result<(Option<&'static str>, Option<&'static str>), GrepError> {
        let (encoding, fallback_encoding) = self.encoding_labels()?;
        if encoding.is_some() && !single_file {
            return Err(GrepError::Validation(
                "encoding applies to a single-file path; use fallback_encoding for a directory"
                    .to_owned(),
            ));
        }
        if fallback_encoding.is_some() && single_file {
            return Err(GrepError::Validation(
                "fallback_encoding applies to a directory path; use encoding for a single file"
                    .to_owned(),
            ));
        }
        Ok((encoding, fallback_encoding))
    }

    /// Resolve both encoding arguments to canonical labels.
    ///
    /// Done once per request so a rejected label fails before any filesystem work, and so
    /// the per-candidate path never has to resolve a label again.
    ///
    /// # Errors
    ///
    /// Returns a validation error for a label `encoding_rs` does not recognise.
    pub fn encoding_labels(
        &self,
    ) -> Result<(Option<&'static str>, Option<&'static str>), GrepError> {
        let resolve = |label: &Option<String>| {
            label
                .as_deref()
                .map(crate::encoding::normalize_label)
                .transpose()
                .map_err(|error| GrepError::Validation(error.to_string()))
        };
        Ok((resolve(&self.encoding)?, resolve(&self.fallback_encoding)?))
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
         AGENTSHIM_GREP_MEMORY_BYTES"
    )]
    CandidateMemory,
    #[error("grep memory capacity is busy; retry the request later")]
    MemoryBusy,
    #[error("grep worker pool state was poisoned")]
    PoolPoison,
    #[error("{}", .0.single_file_message())]
    Unsearchable(crate::output::SkipReason),
    #[error("grep cancelled")]
    Cancelled,
    #[error("grep resource {0} is busy")]
    ResourceBusy(&'static str),
    #[error("grep worker failed: {0}")]
    Worker(String),
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
    execute_with_variant(
        access,
        request,
        lanes,
        cancellation,
        GrepTraversal::Adaptive,
        GrepBenchmarkVariant::default(),
    )
}

#[cfg(test)]
pub fn execute_with_memory_budget(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    lanes: usize,
    memory_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<String, GrepError> {
    let mut config = crate::runtime::RuntimeConfig::for_tests(lanes);
    config.grep_memory_bytes = memory_bytes;
    config.memory_bytes = memory_bytes;
    let resources = RuntimeResources::new(config);
    let _worker = resources
        .try_acquire_worker()
        .ok_or_else(|| io::Error::other("test base worker permit unavailable"))?;
    let _open_file = resources
        .try_acquire_open_file()
        .ok_or_else(|| io::Error::other("test base open-file permit unavailable"))?;
    let initial_bytes = base_memory_charge(memory_bytes);
    let initial = resources
        .try_reserve_memory(initial_bytes)
        .ok_or_else(|| io::Error::other("test base memory permit unavailable"))?;
    let memory = MemoryReservation::from_initial(&resources, initial, initial_bytes);
    execute_inner(
        access,
        request,
        &resources,
        cancellation,
        GrepExecution {
            traversal: GrepTraversal::Adaptive,
            variant: GrepBenchmarkVariant::default(),
            profiler: &GrepProfiler::disabled(),
            output_budget: &crate::output::TestCallBudget::default(),
            memory: Some(memory),
        },
    )
    .map(|output| output.text)
}

#[must_use]
pub fn base_memory_charge(request_bytes: usize) -> usize {
    GrepMemoryPolicy::new(request_bytes).base_reservation_bytes()
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
    execute_with_variant(
        access,
        request,
        lanes,
        cancellation,
        traversal,
        GrepBenchmarkVariant::default(),
    )
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
                output_budget: &crate::output::TestCallBudget::default(),
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
                output_budget: &crate::output::TestCallBudget::default(),
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
        .try_reserve_memory(base_memory_charge(resources.config().grep_memory_bytes))
        .ok_or_else(|| io::Error::other("benchmark base memory permit unavailable"))?;
    execute(&resources)
}

pub fn execute_output_with_budget(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    resources: &RuntimeResources,
    cancellation: &CancellationToken,
    memory: MemoryReservation,
    output_budget: &dyn crate::output::CallBudget,
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
            output_budget,
        },
    )
}

struct GrepExecution<'a> {
    traversal: GrepTraversal,
    variant: GrepBenchmarkVariant,
    profiler: &'a GrepProfiler,
    memory: Option<MemoryReservation>,
    output_budget: &'a dyn crate::output::CallBudget,
}

#[allow(
    clippy::too_many_lines,
    reason = "the reservation must span candidate collection, search, and rendering"
)]
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
        output_budget,
    } = execution;
    let memory_policy = GrepMemoryPolicy::new(resources.config().grep_memory_bytes);
    let file_work_pool = resources.file_work_pool();
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
    let base = access.resolve(std::path::Path::new(request.path.as_deref().unwrap_or(".")))?;
    let single_file = access.metadata_kind(&base)?.is_file;
    let (encoding, fallback_encoding) = request.resolved_encodings_inner(single_file)?;
    drop(setup_span);
    let mode = request.mode.unwrap_or_default();
    let probe = request
        .offset
        .unwrap_or(0)
        .saturating_add(request.limit.unwrap_or(DEFAULT_LIMIT))
        .saturating_add(1);
    let allow_early_stop = !single_file
        && request.offset.unwrap_or(0) == 0
        && matches!(mode, GrepMode::Content | GrepMode::Files)
        && file_work_pool.extra_capacity() == 0;
    let plan = SearchPlan {
        mode,
        context: request.context_lines.unwrap_or(0),
        probe,
        skip: 0,
        allow_early_stop,
        encoding,
        fallback_encoding,
        memory: memory_policy,
    };
    let include_ignored = resources.config().include_ignored(request.include_ignored);
    let search_span = profiler.span(GrepStage::SearchWall);
    let page = pipelined_search(
        access,
        &base,
        matcher,
        glob.as_ref(),
        include_ignored,
        cancellation,
        traversal,
        #[cfg(any(test, feature = "bench-internals"))]
        literal_prefix.as_deref(),
        #[cfg(not(any(test, feature = "bench-internals")))]
        None,
        request,
        plan,
        variant,
        profiler,
        resources,
        memory,
    )
    .map_err(normalize_cancellation)?;
    drop(search_span);
    let render_span = profiler.span(GrepStage::Render);
    let output = render_with_budget(request, &page, cancellation, output_budget);
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

pub fn build_matcher(request: &GrepRequest) -> Result<RegexMatcher, GrepError> {
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
