use std::{
    io,
    path::Path,
    sync::{Arc, Mutex},
};

#[cfg(feature = "bench-internals")]
use super::profile::ProfiledGlob;
use super::{
    profile::{GlobProfiler, GlobStage},
    result::{GlobMatch, TopK, render},
};

use globset::GlobBuilder;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    path::{FileAccess, PathError, PathSortKey, ResolvedPath},
    runtime::{FileWorkCredit, FileWorkPool, RuntimeResources},
    tools::ToolOutput,
    traversal::{
        ParallelTraversal, TraversalControl, TraversalError, TraversalSummary,
        prefer_parallel_root, walk, walk_parallel_batched,
        walk_parallel_batched_with_literal_prefix, walk_with_literal_prefix,
    },
};

pub(super) const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1_000;
pub(super) const MAX_MATCHES: usize = 100_000;
const MEMORY_SAFETY_BYTES: usize = 8 * 1024 * 1024;
pub(super) const PATH_OMISSION: &str = "[glob path omitted: exceeds output budget]";
const PARALLEL_BATCH_SIZE: usize = 256;

#[derive(Clone, Copy)]
pub enum GlobTraversal {
    Adaptive,
    Serial,
    ParallelBatched,
    #[cfg(any(test, feature = "bench-internals"))]
    SerialLiteralPrefix,
    #[cfg(any(test, feature = "bench-internals"))]
    ParallelBatchedLiteralPrefix,
}

/// Filesystem entry kind returned by glob.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GlobEntryType {
    #[default]
    File,
    Directory,
    Any,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GlobRequest {
    pub pattern: String,
    pub path: Option<String>,
    pub include_ignored: Option<bool>,
    #[serde(rename = "type")]
    pub entry_type: Option<GlobEntryType>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
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

#[must_use]
pub(crate) fn memory_charge(request: &GlobRequest) -> usize {
    let offset = request.offset.unwrap_or(0);
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let retain = offset.saturating_add(limit).min(MAX_MATCHES);
    MEMORY_SAFETY_BYTES.saturating_add(retain.saturating_mul(std::mem::size_of::<GlobMatch>()))
}

#[derive(Debug, thiserror::Error)]
pub enum GlobError {
    #[error("invalid glob request: {0}")]
    Validation(String),
    #[error("invalid glob pattern: {0}")]
    Pattern(String),
    #[error("more than 100000 paths matched; narrow pattern or path")]
    TooManyMatches,
    #[error(
        "retained glob paths exceed the configured memory limit; narrow pattern or offset, or \
         raise CODEXSHIM_GLOB_MEMORY_BYTES"
    )]
    Memory,
    #[error("glob could not grow within the shared memory capacity; retry later")]
    MemoryBusy,
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
    execute_inner(
        access,
        request,
        benchmark_resources(),
        cancellation,
        GlobTraversal::Adaptive,
        &GlobProfiler::disabled(),
        None,
    )
    .map(|result| result.text)
}

pub(crate) fn execute_output(
    access: &Arc<FileAccess>,
    request: &GlobRequest,
    resources: &RuntimeResources,
    cancellation: &CancellationToken,
    memory: crate::runtime::MemoryReservation,
) -> Result<ToolOutput, GlobError> {
    execute_inner(
        access,
        request,
        resources,
        cancellation,
        GlobTraversal::Adaptive,
        &GlobProfiler::disabled(),
        Some(memory),
    )
}

fn execute_inner(
    access: &Arc<FileAccess>,
    request: &GlobRequest,
    resources: &RuntimeResources,
    cancellation: &CancellationToken,
    traversal: GlobTraversal,
    profiler: &GlobProfiler,
    memory: Option<crate::runtime::MemoryReservation>,
) -> Result<ToolOutput, GlobError> {
    execute_inner_with_traversal(
        access,
        request,
        resources,
        cancellation,
        traversal,
        profiler,
        memory,
    )
}

#[cfg(any(test, feature = "bench-internals"))]
/// Execute glob with an explicit traversal strategy for tests and benchmarks.
///
/// # Errors
///
/// Returns the same validation, path, traversal, cancellation, and formatting
/// errors as the production glob path.
pub fn execute_with_traversal(
    access: &Arc<FileAccess>,
    request: &GlobRequest,
    cancellation: &CancellationToken,
    traversal: GlobTraversal,
) -> Result<String, GlobError> {
    execute_inner_with_traversal(
        access,
        request,
        benchmark_resources(),
        cancellation,
        traversal,
        &GlobProfiler::disabled(),
        None,
    )
    .map(|output| output.text)
}

#[cfg(feature = "bench-internals")]
/// Execute one explicitly selected glob traversal and return stage timings.
///
/// # Errors
///
/// Returns the same validation, traversal, cancellation, and output errors as
/// [`execute_with_traversal`].
pub fn execute_profiled_with_traversal(
    access: &Arc<FileAccess>,
    request: &GlobRequest,
    cancellation: &CancellationToken,
    traversal: GlobTraversal,
) -> Result<ProfiledGlob, GlobError> {
    let profiler = GlobProfiler::enabled();
    let total_span = profiler.span(GlobStage::Total);
    let result = execute_inner_with_traversal(
        access,
        request,
        benchmark_resources(),
        cancellation,
        traversal,
        &profiler,
        None,
    );
    drop(total_span);
    let output = result?;
    Ok(ProfiledGlob {
        output: output.text,
        timings: profiler.snapshot(),
    })
}

fn execute_inner_with_traversal(
    access: &Arc<FileAccess>,
    request: &GlobRequest,
    resources: &RuntimeResources,
    cancellation: &CancellationToken,
    traversal: GlobTraversal,
    profiler: &GlobProfiler,
    memory: Option<crate::runtime::MemoryReservation>,
) -> Result<ToolOutput, GlobError> {
    let file_work_pool = resources.file_work_pool();
    let _file_work_request = file_work_pool.begin_request();
    let setup_span = profiler.span(GlobStage::Setup);
    request.validate()?;
    let matcher = GlobBuilder::new(&request.pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map_err(|error| GlobError::Pattern(error.to_string()))?
        .compile_matcher();
    #[cfg(any(test, feature = "bench-internals"))]
    let literal_prefix = crate::traversal::literal_path_prefix(&request.pattern);
    let base_input = request.path.as_deref().unwrap_or(".");
    let base = access.resolve(Path::new(base_input))?;
    let selection = select_glob_traversal(access, &base, traversal, &file_work_pool);
    let traversal = selection.traversal;
    let offset = request.offset.unwrap_or(0);
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let retain = offset.saturating_add(limit).min(MAX_MATCHES);
    let collection = GlobCollection {
        store: TopK::new(retain, resources.config().glob_memory_bytes, memory)?,
        total: 0,
        terminal_error: None,
    };
    let regular_plan = regular_collect_plan(request, selection.threads);
    #[cfg(any(test, feature = "bench-internals"))]
    let prefix_plan = GlobCollectPlan {
        include_ignored: regular_plan.include_ignored,
        entry_type: regular_plan.entry_type,
        literal_prefix: literal_prefix.as_deref(),
        traversal_threads: selection.threads,
    };
    drop(setup_span);
    let traversal_span = profiler.span(GlobStage::TraversalWall);
    let (collection, summary) = match traversal {
        GlobTraversal::Adaptive => unreachable!("adaptive traversal was resolved"),
        GlobTraversal::Serial => collect_serial(
            access,
            &base,
            cancellation,
            &matcher,
            collection,
            regular_plan,
        )?,
        GlobTraversal::ParallelBatched => collect_parallel(
            access,
            &base,
            cancellation,
            &matcher,
            collection,
            profiler,
            regular_plan,
        )?,
        #[cfg(any(test, feature = "bench-internals"))]
        GlobTraversal::SerialLiteralPrefix => collect_serial(
            access,
            &base,
            cancellation,
            &matcher,
            collection,
            prefix_plan,
        )?,
        #[cfg(any(test, feature = "bench-internals"))]
        GlobTraversal::ParallelBatchedLiteralPrefix => collect_parallel(
            access,
            &base,
            cancellation,
            &matcher,
            collection,
            profiler,
            prefix_plan,
        )?,
    };
    drop(selection.credits);
    drop(traversal_span);
    if let Some(error) = collection.terminal_error {
        return Err(error);
    }
    profiler.record_retained(
        collection.store.len(),
        collection.store.retained_memory_bytes(),
    );
    let skipped = summary.io_errors + summary.escaped_entries + summary.non_unicode_entries;
    if skipped > 0 {
        tracing::warn!(target: "codexshim", event = "traversal_skipped", phase = "execution", outcome = "degraded_success", counters = %format!("io_errors={},escaped_entries={},non_unicode_entries={}", summary.io_errors, summary.escaped_entries, summary.non_unicode_entries));
    }
    let sort_span = profiler.span(GlobStage::FinalSort);
    let retained = collection.store.into_sorted(cancellation)?;
    drop(sort_span);
    let render_span = profiler.span(GlobStage::Render);
    let result = render(request, &retained, collection.total, summary, cancellation);
    drop(render_span);
    result
}

struct GlobTraversalSelection {
    traversal: GlobTraversal,
    credits: Vec<FileWorkCredit>,
    threads: usize,
}

fn select_glob_traversal(
    access: &FileAccess,
    base: &ResolvedPath,
    requested: GlobTraversal,
    pool: &FileWorkPool,
) -> GlobTraversalSelection {
    let wants_parallel = match requested {
        GlobTraversal::Adaptive => prefer_parallel_root(access, base),
        GlobTraversal::ParallelBatched => true,
        #[cfg(any(test, feature = "bench-internals"))]
        GlobTraversal::ParallelBatchedLiteralPrefix => true,
        GlobTraversal::Serial => false,
        #[cfg(any(test, feature = "bench-internals"))]
        GlobTraversal::SerialLiteralPrefix => false,
    };
    let credits = if wants_parallel {
        pool.try_credits(pool.extra_capacity())
    } else {
        Vec::new()
    };
    let traversal = match requested {
        GlobTraversal::Adaptive if credits.is_empty() => GlobTraversal::Serial,
        GlobTraversal::Adaptive => GlobTraversal::ParallelBatched,
        GlobTraversal::ParallelBatched if credits.is_empty() => GlobTraversal::Serial,
        #[cfg(any(test, feature = "bench-internals"))]
        GlobTraversal::ParallelBatchedLiteralPrefix if credits.is_empty() => {
            GlobTraversal::SerialLiteralPrefix
        }
        selected => selected,
    };
    GlobTraversalSelection {
        traversal,
        threads: credits.len().saturating_add(1),
        credits,
    }
}

struct GlobCollection {
    store: TopK,
    total: usize,
    terminal_error: Option<GlobError>,
}

#[derive(Clone, Copy)]
struct GlobCollectPlan<'a> {
    include_ignored: bool,
    entry_type: GlobEntryType,
    literal_prefix: Option<&'a Path>,
    traversal_threads: usize,
}

fn regular_collect_plan(
    request: &GlobRequest,
    traversal_threads: usize,
) -> GlobCollectPlan<'static> {
    GlobCollectPlan {
        include_ignored: request.include_ignored.unwrap_or(false),
        entry_type: request.entry_type.unwrap_or_default(),
        literal_prefix: None,
        traversal_threads,
    }
}

fn matches_entry_type(entry_type: GlobEntryType, file_type: Option<std::fs::FileType>) -> bool {
    match entry_type {
        GlobEntryType::File => {
            file_type.is_some_and(|file_type| file_type.is_file() || file_type.is_symlink())
        }
        GlobEntryType::Directory => file_type.is_some_and(|file_type| file_type.is_dir()),
        GlobEntryType::Any => true,
    }
}

fn collect_serial(
    access: &FileAccess,
    base: &ResolvedPath,
    cancellation: &CancellationToken,
    matcher: &globset::GlobMatcher,
    mut collection: GlobCollection,
    plan: GlobCollectPlan<'_>,
) -> Result<(GlobCollection, TraversalSummary), GlobError> {
    let mut visit = |entry: crate::traversal::TraversalEntry<'_>| {
        if !matches_entry_type(plan.entry_type, entry.file_type) || !matcher.is_match(entry.key) {
            return TraversalControl::Continue;
        }
        if let Err(error) = record_match(&mut collection.total) {
            collection.terminal_error = Some(error);
            return TraversalControl::Stop;
        }
        let sort_key = PathSortKey::new(entry.key);
        if !collection.store.might_admit(&sort_key) {
            return TraversalControl::Continue;
        }
        let path = match access.resolve_walked_entry(base, entry.key, entry.absolute) {
            Ok(path) => path,
            Err(error) => {
                collection.terminal_error = Some(error.into());
                return TraversalControl::Stop;
            }
        };
        if let Err(error) = collection.store.admit(&path) {
            collection.terminal_error = Some(error);
            return TraversalControl::Stop;
        }
        TraversalControl::Continue
    };
    let summary = if let Some(literal_prefix) = plan.literal_prefix {
        walk_with_literal_prefix(
            access,
            base,
            plan.include_ignored,
            cancellation,
            literal_prefix,
            &mut visit,
        )?
    } else {
        walk(access, base, plan.include_ignored, cancellation, &mut visit)?
    };
    Ok((collection, summary))
}

fn collect_parallel(
    access: &FileAccess,
    base: &ResolvedPath,
    cancellation: &CancellationToken,
    matcher: &globset::GlobMatcher,
    collection: GlobCollection,
    profiler: &GlobProfiler,
    plan: GlobCollectPlan<'_>,
) -> Result<(GlobCollection, TraversalSummary), GlobError> {
    let collection = Mutex::new(collection);
    let visit = |batch: &[crate::traversal::OwnedTraversalEntry]| {
        let threshold = {
            let wait_span = profiler.span(GlobStage::MergeWaitWorker);
            let collection = collection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(wait_span);
            collection.store.threshold()
        };
        let mut found = Vec::new();
        let mut matched_entries = 0_usize;
        for entry in batch {
            if !matches_entry_type(plan.entry_type, entry.file_type)
                || !matcher.is_match(&entry.key)
            {
                continue;
            }
            matched_entries = matched_entries.saturating_add(1);
            let sort_key = PathSortKey::new(&entry.key);
            if !threshold.might_admit(&sort_key) {
                continue;
            }
            match access.resolve_walked_entry(base, &entry.key, &entry.absolute) {
                Ok(path) => found.push(path),
                Err(error) => {
                    let wait_span = profiler.span(GlobStage::MergeWaitWorker);
                    let mut collection = collection
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    drop(wait_span);
                    let hold_span = profiler.span(GlobStage::MergeWorkWorker);
                    collection.terminal_error = Some(error.into());
                    drop(hold_span);
                    return TraversalControl::Stop;
                }
            }
        }
        profiler.record_batch(matched_entries);
        let wait_span = profiler.span(GlobStage::MergeWaitWorker);
        let mut collection = collection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(wait_span);
        let hold_span = profiler.span(GlobStage::MergeWorkWorker);
        if collection.terminal_error.is_some() {
            return TraversalControl::Stop;
        }
        for _ in 0..matched_entries {
            if let Err(error) = record_match(&mut collection.total) {
                collection.terminal_error = Some(error);
                return TraversalControl::Stop;
            }
        }
        for path in found {
            if let Err(error) = collection.store.admit(&path) {
                collection.terminal_error = Some(error);
                return TraversalControl::Stop;
            }
        }
        drop(hold_span);
        TraversalControl::Continue
    };
    let summary = if let Some(literal_prefix) = plan.literal_prefix {
        walk_parallel_batched_with_literal_prefix(
            access,
            base,
            plan.include_ignored,
            cancellation,
            ParallelTraversal {
                batch_size: PARALLEL_BATCH_SIZE,
                threads: plan.traversal_threads,
            },
            literal_prefix,
            visit,
        )?
    } else {
        walk_parallel_batched(
            access,
            base,
            plan.include_ignored,
            cancellation,
            ParallelTraversal {
                batch_size: PARALLEL_BATCH_SIZE,
                threads: plan.traversal_threads,
            },
            visit,
        )?
    };
    let collection = collection
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok((collection, summary))
}

#[cfg(any(test, feature = "bench-internals"))]
fn benchmark_resources() -> &'static RuntimeResources {
    static RESOURCES: std::sync::OnceLock<RuntimeResources> = std::sync::OnceLock::new();
    RESOURCES.get_or_init(|| {
        RuntimeResources::new(crate::runtime::RuntimeConfig::for_tests(
            std::thread::available_parallelism().map_or(1, usize::from),
        ))
    })
}

pub(super) fn record_match(total: &mut usize) -> Result<(), GlobError> {
    if *total >= MAX_MATCHES {
        return Err(GlobError::TooManyMatches);
    }
    *total += 1;
    Ok(())
}
