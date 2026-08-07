use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    fs,
    io,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
    },
};

use globset::GlobBuilder;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    output::{OutputFormatter, OutputLimits},
    path::{FileAccess, PathError, PathSortKey, ResolvedPath},
    sorting,
    tools::ToolOutput,
    traversal::{
        TraversalControl, TraversalError, TraversalSummary, walk, walk_parallel_batched,
    },
};

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1_000;
const MAX_MATCHES: usize = 100_000;
const RETAINED_MEMORY_BYTES: usize = 32 * 1024 * 1024;
const MEMORY_SAFETY_BYTES: usize = 8 * 1024 * 1024;
const PATH_OMISSION: &str = "[glob path omitted: exceeds output budget]";
const PARALLEL_BATCH_SIZE: usize = 256;
const PARALLEL_ROOT_ENTRY_THRESHOLD: usize = 8;
static ACTIVE_ADAPTIVE_GLOBS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
pub enum GlobTraversal {
    Adaptive,
    Serial,
    ParallelBatched,
}

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
    execute_inner_with_traversal(
        access,
        request,
        cancellation,
        GlobTraversal::Adaptive,
    )
}

#[cfg(any(test, feature = "bench-internals"))]
pub fn execute_with_traversal(
    access: &Arc<FileAccess>,
    request: &GlobRequest,
    cancellation: &CancellationToken,
    traversal: GlobTraversal,
) -> Result<String, GlobError> {
    execute_inner_with_traversal(access, request, cancellation, traversal)
        .map(|output| output.text)
}

fn execute_inner_with_traversal(
    access: &Arc<FileAccess>,
    request: &GlobRequest,
    cancellation: &CancellationToken,
    traversal: GlobTraversal,
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
    let mut activity = None;
    let traversal = match traversal {
        GlobTraversal::Adaptive => {
            let guard = ActiveAdaptiveGlob::enter();
            let selected = if guard.was_idle && prefer_parallel(access, &base) {
                GlobTraversal::ParallelBatched
            } else {
                GlobTraversal::Serial
            };
            activity = Some(guard);
            selected
        }
        selected => selected,
    };
    let offset = request.offset.unwrap_or(0);
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let retain = offset.saturating_add(limit).min(MAX_MATCHES);
    let collection = GlobCollection {
        store: TopK::new(retain),
        total: 0,
        terminal_error: None,
    };
    let (collection, summary) = match traversal {
        GlobTraversal::Adaptive => unreachable!("adaptive traversal was resolved"),
        GlobTraversal::Serial => collect_serial(
            access,
            &base,
            request.include_ignored.unwrap_or(false),
            cancellation,
            &matcher,
            collection,
        )?,
        GlobTraversal::ParallelBatched => collect_parallel(
            access,
            &base,
            request.include_ignored.unwrap_or(false),
            cancellation,
            &matcher,
            collection,
        )?,
    };
    if let Some(error) = collection.terminal_error {
        return Err(error);
    }
    let skipped = summary.io_errors + summary.escaped_entries + summary.non_unicode_entries;
    if skipped > 0 {
        tracing::warn!(target: "codexshim", event = "traversal_skipped", phase = "execution", outcome = "degraded_success", counters = %format!("io_errors={},escaped_entries={},non_unicode_entries={}", summary.io_errors, summary.escaped_entries, summary.non_unicode_entries));
    }
    let retained = collection.store.into_sorted(cancellation)?;
    let result = render(request, &retained, collection.total, summary, cancellation);
    drop(activity);
    result
}

fn prefer_parallel(access: &FileAccess, base: &ResolvedPath) -> bool {
    if access.root().verify().is_err()
        || (base.is_ambient()
            && access
                .symlink_metadata_kind(base)
                .is_ok_and(|kind| kind.is_symlink))
        || !access.metadata_kind(base).is_ok_and(|kind| kind.is_dir)
    {
        return false;
    }
    fs::read_dir(base.absolute()).is_ok_and(|entries| {
        entries.take(PARALLEL_ROOT_ENTRY_THRESHOLD).count()
            >= PARALLEL_ROOT_ENTRY_THRESHOLD
    })
}

struct ActiveAdaptiveGlob {
    was_idle: bool,
}

impl ActiveAdaptiveGlob {
    fn enter() -> Self {
        Self {
            was_idle: ACTIVE_ADAPTIVE_GLOBS.fetch_add(1, AtomicOrdering::AcqRel) == 0,
        }
    }
}

impl Drop for ActiveAdaptiveGlob {
    fn drop(&mut self) {
        ACTIVE_ADAPTIVE_GLOBS.fetch_sub(1, AtomicOrdering::AcqRel);
    }
}

struct GlobCollection {
    store: TopK,
    total: usize,
    terminal_error: Option<GlobError>,
}

fn collect_serial(
    access: &FileAccess,
    base: &ResolvedPath,
    include_ignored: bool,
    cancellation: &CancellationToken,
    matcher: &globset::GlobMatcher,
    mut collection: GlobCollection,
) -> Result<(GlobCollection, TraversalSummary), GlobError> {
    let summary = walk(
        access,
        base,
        include_ignored,
        cancellation,
        |entry| {
            if !matcher.is_match(entry.key) {
                return TraversalControl::Continue;
            }
            if let Err(error) = record_match(&mut collection.total) {
                collection.terminal_error = Some(error);
                return TraversalControl::Stop;
            }
            let path = match access.resolve_traversal_entry(base, entry.absolute) {
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
        },
    )?;
    Ok((collection, summary))
}

fn collect_parallel(
    access: &FileAccess,
    base: &ResolvedPath,
    include_ignored: bool,
    cancellation: &CancellationToken,
    matcher: &globset::GlobMatcher,
    collection: GlobCollection,
) -> Result<(GlobCollection, TraversalSummary), GlobError> {
    let collection = Mutex::new(collection);
    let summary = walk_parallel_batched(
        access,
        base,
        include_ignored,
        cancellation,
        PARALLEL_BATCH_SIZE,
        |batch| {
            let mut found = Vec::new();
            for entry in batch {
                if !matcher.is_match(&entry.key) {
                    continue;
                }
                match access.resolve_traversal_entry(base, &entry.absolute) {
                    Ok(path) => found.push(path),
                    Err(error) => {
                        collection
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .terminal_error = Some(error.into());
                        return TraversalControl::Stop;
                    }
                }
            }
            let mut collection = collection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if collection.terminal_error.is_some() {
                return TraversalControl::Stop;
            }
            for path in found {
                if let Err(error) = record_match(&mut collection.total) {
                    collection.terminal_error = Some(error);
                    return TraversalControl::Stop;
                }
                if let Err(error) = collection.store.admit(&path) {
                    collection.terminal_error = Some(error);
                    return TraversalControl::Stop;
                }
            }
            TraversalControl::Continue
        },
    )?;
    let collection = collection
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok((collection, summary))
}

fn record_match(total: &mut usize) -> Result<(), GlobError> {
    if *total >= MAX_MATCHES {
        return Err(GlobError::TooManyMatches);
    }
    *total += 1;
    Ok(())
}
