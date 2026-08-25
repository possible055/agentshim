use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender};
use grep_regex::RegexMatcher;
use grep_searcher::Searcher;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

use crate::{
    output::SkipReason,
    path::{FileAccess, ResolvedPath},
    runtime::{FileWorkCredit, FileWorkPool, MemoryReservation, RuntimeResources},
    traversal::{
        OwnedTraversalEntry, ParallelTraversal, ParallelTraversalCallbacks, TraversalControl,
        TraversalEntry, TraversalSummary, prefer_parallel_root, walk, walk_parallel_batched,
        walk_parallel_batched_with_literal_prefix, walk_with_literal_prefix,
    },
};

use super::{
    candidates::{Candidate, candidate},
    file_search::{
        FileOutcome, FileSearchContext, RetryReason, SearchPlan, build_searcher,
        search_file_with_searcher,
    },
    profile::{GrepProfiler, GrepStage, GrepWorkerActivity},
    request::{
        GrepBenchmarkVariant, GrepError, GrepMemoryPolicy, GrepMode, GrepRequest, GrepTraversal,
        PARALLEL_BATCH_SIZE,
    },
    result::{Page, ReduceControl},
};

type SearchResult = Result<FileOutcome, GrepError>;

struct QueuedOutcome {
    result: Option<SearchResult>,
    _memory: OwnedSemaphorePermit,
    _credit: FileWorkCredit,
    profiler: GrepProfiler,
    bytes: usize,
}

impl QueuedOutcome {
    fn new(
        result: SearchResult,
        memory: OwnedSemaphorePermit,
        credit: FileWorkCredit,
        profiler: GrepProfiler,
        bytes: usize,
    ) -> Self {
        profiler.record_outcome_queued(bytes);
        Self {
            result: Some(result),
            _memory: memory,
            _credit: credit,
            profiler,
            bytes,
        }
    }

    fn take_result(&mut self) -> SearchResult {
        self.result.take().expect("queued outcome consumed once")
    }
}

impl Drop for QueuedOutcome {
    fn drop(&mut self) {
        self.profiler.record_outcome_released(self.bytes);
    }
}

type SharedPipelineState = Arc<Mutex<PipelineState>>;

struct PipelineState {
    page: Page,
    pending: usize,
    candidate_count: usize,
    stopped: bool,
    terminal_error: Option<GrepError>,
}

#[derive(Clone)]
struct PipelineContext {
    cancellation: CancellationToken,
    retirement: CancellationToken,
    access: Arc<FileAccess>,
    matcher: Arc<RegexMatcher>,
    plan: SearchPlan,
    variant: GrepBenchmarkVariant,
    profiler: GrepProfiler,
    resources: RuntimeResources,
    memory: GrepMemoryPolicy,
}

/// Search candidates as they are discovered and stop the traversal when the page is full.
///
/// # Errors
///
/// Returns validation, traversal, resource, cancellation, I/O, or output errors.
#[allow(
    clippy::too_many_arguments,
    reason = "the pipeline keeps traversal, search, and resource controls explicit"
)]
pub fn pipelined_search(
    access: &Arc<FileAccess>,
    base: &ResolvedPath,
    matcher: Arc<RegexMatcher>,
    glob: Option<&globset::GlobMatcher>,
    include_ignored: bool,
    cancellation: &CancellationToken,
    traversal: GrepTraversal,
    literal_prefix: Option<&Path>,
    request: &GrepRequest,
    plan: SearchPlan,
    variant: GrepBenchmarkVariant,
    profiler: &GrepProfiler,
    resources: &RuntimeResources,
    request_memory: Option<MemoryReservation>,
) -> Result<Page, GrepError> {
    let single_file = access.metadata_kind(base)?.is_file;
    let pool = resources.file_work_pool();
    let lanes = pool.extra_capacity().saturating_add(1);
    let context = PipelineContext {
        cancellation: cancellation.clone(),
        retirement: CancellationToken::new(),
        access: Arc::clone(access),
        matcher,
        plan,
        variant,
        profiler: profiler.clone(),
        resources: resources.clone(),
        memory: plan.memory,
    };
    let state = Arc::new(Mutex::new(PipelineState {
        page: Page::new(
            request,
            TraversalSummary::default(),
            plan.allow_early_stop,
            !single_file && pool.extra_capacity() > 0,
        ),
        pending: 0,
        candidate_count: 0,
        stopped: false,
        terminal_error: None,
    }));
    profiler.set_workload(0, lanes);

    let outcome_bytes = plan.memory.speculative_batch_bytes(plan.mode, 1).max(1);
    let queue_capacity = outcome_queue_capacity(plan.memory.request_bytes, outcome_bytes, &pool);
    let (sender, receiver) = crossbeam_channel::bounded(queue_capacity);
    if single_file {
        search_single_file(base, glob, request, &context, &state);
    } else {
        let traversal_span = profiler.span(GrepStage::Pipeline);
        let summary = traverse(
            access,
            base,
            glob,
            include_ignored,
            cancellation,
            traversal,
            literal_prefix,
            request,
            &context,
            &state,
            &sender,
            &receiver,
            &pool,
        )?;
        drop(traversal_span);
        let mut state_guard = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state_guard.page.set_traversal_summary(summary);
    }

    drain_pending(&state, &receiver, &context, request, single_file)?;
    let mut state = Arc::try_unwrap(state)
        .map_err(|_| GrepError::PoolPoison)?
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(error) = state.terminal_error.take() {
        return Err(error);
    }
    if state.stopped {
        context.retirement.cancel();
    } else {
        state.page.mark_complete();
        profiler.set_scan_complete();
    }
    let reported_lanes = lanes.clamp(1, state.candidate_count.max(1));
    profiler.set_workload(state.candidate_count, reported_lanes);
    drop(request_memory);
    Ok(state.page)
}

fn search_single_file(
    base: &ResolvedPath,
    glob: Option<&globset::GlobMatcher>,
    request: &GrepRequest,
    context: &PipelineContext,
    state: &SharedPipelineState,
) {
    let matches = glob.is_none_or(|glob| base.slash_path().is_some_and(|path| glob.is_match(path)));
    if !matches {
        return;
    }
    let candidate = match candidate(base.clone()) {
        Ok(candidate) => candidate,
        Err(error) => {
            let mut state_guard = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            record_error(&mut state_guard, context, error);
            return;
        }
    };
    let mut state_guard = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state_guard.candidate_count = 1;
    let outcome = run_candidate(&candidate, context);
    reduce_candidate(
        &mut state_guard,
        outcome,
        Some(&candidate),
        request,
        true,
        context,
    );
}

#[allow(clippy::too_many_arguments)]
fn traverse(
    access: &Arc<FileAccess>,
    base: &ResolvedPath,
    glob: Option<&globset::GlobMatcher>,
    include_ignored: bool,
    cancellation: &CancellationToken,
    requested: GrepTraversal,
    literal_prefix: Option<&Path>,
    request: &GrepRequest,
    context: &PipelineContext,
    state: &SharedPipelineState,
    sender: &Sender<QueuedOutcome>,
    receiver: &Receiver<QueuedOutcome>,
    pool: &Arc<FileWorkPool>,
) -> Result<TraversalSummary, GrepError> {
    let selected = resolve_traversal(access, base, requested, pool);
    let prefilter = |entry: TraversalEntry<'_>| matches_candidate_entry(glob, entry);
    let serial_visit = |entry: TraversalEntry<'_>| {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        visit_entry(
            access, base, glob, entry, request, context, &mut state, sender, receiver, pool,
        )
    };
    let parallel_visit = |batch: &[OwnedTraversalEntry]| {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for entry in batch {
            if state.stopped {
                return TraversalControl::Stop;
            }
            let candidate = match candidate_from_owned_entry(access, base, entry) {
                Ok(candidate) => candidate,
                Err(error) => {
                    record_error(&mut state, context, error);
                    return TraversalControl::Stop;
                }
            };
            state.candidate_count = state.candidate_count.saturating_add(1);
            if !schedule_candidate(
                &mut state, &candidate, request, context, sender, receiver, pool,
            ) {
                return TraversalControl::Stop;
            }
        }
        if state.stopped {
            TraversalControl::Stop
        } else {
            TraversalControl::Continue
        }
    };

    match selected {
        TraversalSelection::Serial => {
            if let Some(literal_prefix) = literal_prefix {
                walk_with_literal_prefix(
                    access,
                    base,
                    include_ignored,
                    cancellation,
                    literal_prefix,
                    serial_visit,
                )
                .map_err(GrepError::Traversal)
            } else {
                walk(access, base, include_ignored, cancellation, serial_visit)
                    .map_err(GrepError::Traversal)
            }
        }
        TraversalSelection::Parallel { threads } => {
            if let Some(literal_prefix) = literal_prefix {
                walk_parallel_batched_with_literal_prefix(
                    access,
                    base,
                    include_ignored,
                    cancellation,
                    ParallelTraversal {
                        batch_size: PARALLEL_BATCH_SIZE,
                        threads,
                    },
                    literal_prefix,
                    &ParallelTraversalCallbacks {
                        prefilter,
                        visitor: parallel_visit,
                    },
                )
                .map_err(GrepError::Traversal)
            } else {
                walk_parallel_batched(
                    access,
                    base,
                    include_ignored,
                    cancellation,
                    ParallelTraversal {
                        batch_size: PARALLEL_BATCH_SIZE,
                        threads,
                    },
                    &ParallelTraversalCallbacks {
                        prefilter,
                        visitor: parallel_visit,
                    },
                )
                .map_err(GrepError::Traversal)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum TraversalSelection {
    Serial,
    Parallel { threads: usize },
}

fn resolve_traversal(
    access: &FileAccess,
    base: &ResolvedPath,
    requested: GrepTraversal,
    pool: &FileWorkPool,
) -> TraversalSelection {
    let wants_parallel = match requested {
        GrepTraversal::Adaptive => prefer_parallel_root(access, base),
        #[cfg(any(test, feature = "bench-internals"))]
        GrepTraversal::ParallelBatched => true,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepTraversal::ParallelBatchedLiteralPrefix => true,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepTraversal::Serial => false,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepTraversal::SerialLiteralPrefix => false,
    };
    let threads = pool.extra_capacity().saturating_add(1).max(1);
    if wants_parallel && threads > 1 {
        TraversalSelection::Parallel { threads }
    } else {
        TraversalSelection::Serial
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "entry visitation needs the shared pipeline state and dispatch handles"
)]
fn visit_entry(
    access: &FileAccess,
    base: &ResolvedPath,
    glob: Option<&globset::GlobMatcher>,
    entry: TraversalEntry<'_>,
    request: &GrepRequest,
    context: &PipelineContext,
    state: &mut PipelineState,
    sender: &Sender<QueuedOutcome>,
    receiver: &Receiver<QueuedOutcome>,
    pool: &FileWorkPool,
) -> TraversalControl {
    if state.stopped {
        return TraversalControl::Stop;
    }
    if !matches_candidate_entry(glob, entry) {
        return TraversalControl::Continue;
    }
    let path = match access.resolve_walked_entry(base, entry.key, entry.absolute) {
        Ok(path) => path,
        Err(error) => {
            record_error(state, context, error.into());
            return TraversalControl::Stop;
        }
    };
    let candidate = match candidate(path) {
        Ok(candidate) => candidate,
        Err(error) => {
            record_error(state, context, error);
            return TraversalControl::Stop;
        }
    };
    state.candidate_count = state.candidate_count.saturating_add(1);
    if schedule_candidate(state, &candidate, request, context, sender, receiver, pool) {
        TraversalControl::Continue
    } else {
        TraversalControl::Stop
    }
}

fn candidate_from_owned_entry(
    access: &FileAccess,
    base: &ResolvedPath,
    entry: &OwnedTraversalEntry,
) -> Result<Candidate, GrepError> {
    candidate(access.resolve_walked_entry(base, &entry.key, &entry.absolute)?)
}

fn matches_candidate_entry(glob: Option<&globset::GlobMatcher>, entry: TraversalEntry<'_>) -> bool {
    entry
        .file_type
        .is_some_and(|file_type| file_type.is_file() || file_type.is_symlink())
        && glob.is_none_or(|glob| glob.is_match(entry.key))
}

fn schedule_candidate(
    state: &mut PipelineState,
    candidate: &Candidate,
    request: &GrepRequest,
    context: &PipelineContext,
    sender: &Sender<QueuedOutcome>,
    receiver: &Receiver<QueuedOutcome>,
    pool: &FileWorkPool,
) -> bool {
    loop {
        if state.stopped {
            return false;
        }
        if try_dispatch(candidate, context, sender, pool) {
            state.pending = state.pending.saturating_add(1);
            return true;
        }
        if state.pending == 0 {
            let outcome = run_candidate(candidate, context);
            reduce_candidate(state, outcome, Some(candidate), request, false, context);
            return !state.stopped;
        }
        if let Err(error) = receive_one(state, receiver, request, context) {
            record_error(state, context, error);
            return false;
        }
    }
}

fn try_dispatch(
    candidate: &Candidate,
    context: &PipelineContext,
    sender: &Sender<QueuedOutcome>,
    pool: &FileWorkPool,
) -> bool {
    let Some(credit) = pool.try_credit() else {
        return false;
    };
    let Some(open_file) = context.resources.try_acquire_open_file() else {
        drop(credit);
        return false;
    };
    let charge = context.memory.speculative_batch_bytes(context.plan.mode, 1);
    let Some(memory) = context.resources.try_reserve_memory(charge) else {
        drop((credit, open_file));
        return false;
    };
    context.profiler.record_speculative_lease(charge, true);
    let candidate = candidate.clone();
    let context = context.clone();
    let sender = sender.clone();
    let job = move |credit| {
        let _worker_activity = GrepWorkerActivity::enter();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_candidate(&candidate, &context)
        }))
        .unwrap_or_else(|_| Err(GrepError::Worker("grep worker panicked".to_owned())));
        drop(open_file);
        let mut outcome =
            QueuedOutcome::new(result, memory, credit, context.profiler.clone(), charge);
        loop {
            match sender.send_timeout(outcome, Duration::from_millis(10)) {
                Ok(()) | Err(SendTimeoutError::Disconnected(_)) => break,
                Err(SendTimeoutError::Timeout(returned)) => {
                    outcome = returned;
                    if context.cancellation.is_cancelled() {
                        break;
                    }
                }
            }
        }
    };
    if pool.spawn(credit, job).is_err() {
        return false;
    }
    true
}

fn receive_one(
    state: &mut PipelineState,
    receiver: &Receiver<QueuedOutcome>,
    request: &GrepRequest,
    context: &PipelineContext,
) -> Result<(), GrepError> {
    loop {
        match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(mut outcome) => {
                state.pending = state.pending.saturating_sub(1);
                if !state.stopped {
                    let result = outcome.take_result();
                    drop(outcome);
                    reduce_candidate(state, result, None, request, false, context);
                }
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {
                if context.cancellation.is_cancelled() {
                    return Err(GrepError::Cancelled);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Err(GrepError::PoolPoison),
        }
    }
}

fn drain_pending(
    state: &SharedPipelineState,
    receiver: &Receiver<QueuedOutcome>,
    context: &PipelineContext,
    request: &GrepRequest,
    single_file: bool,
) -> Result<(), GrepError> {
    loop {
        let pending = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending;
        if pending == 0 {
            return Ok(());
        }
        let mut outcome = loop {
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(outcome) => break outcome,
                Err(RecvTimeoutError::Timeout) if context.cancellation.is_cancelled() => {
                    return Err(GrepError::Cancelled);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(GrepError::PoolPoison),
            }
        };
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = state.pending.saturating_sub(1);
        if !state.stopped {
            let result = outcome.take_result();
            drop(outcome);
            reduce_candidate(&mut state, result, None, request, single_file, context);
        }
    }
}

fn reduce_candidate(
    state: &mut PipelineState,
    outcome: SearchResult,
    candidate: Option<&Candidate>,
    request: &GrepRequest,
    single_file: bool,
    context: &PipelineContext,
) {
    let mut outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            record_error(state, context, error);
            return;
        }
    };
    if outcome.retired {
        return;
    }
    if let Some(reason) = outcome.retry {
        let Some(candidate) = candidate.cloned().or_else(|| {
            outcome.path.as_ref().map(|path| Candidate {
                path: Arc::clone(path),
            })
        }) else {
            record_error(
                state,
                context,
                GrepError::Worker("retry outcome has no candidate path".to_owned()),
            );
            return;
        };
        match run_isolated_retry(&candidate, context, &state.page, reason) {
            Ok(retried) => outcome = retried,
            Err(error) => {
                record_error(state, context, error);
                return;
            }
        }
    }
    context.profiler.record_reduced_candidate();
    match state
        .page
        .reduce(outcome, request.mode.unwrap_or_default(), single_file)
    {
        Ok(ReduceControl::Continue) => {}
        Ok(ReduceControl::PageFull) => {
            state.stopped = true;
            context.retirement.cancel();
        }
        Err(error) => record_error(state, context, error),
    }
}

fn run_candidate(candidate: &Candidate, context: &PipelineContext) -> SearchResult {
    if context.cancellation.is_cancelled() {
        return Err(GrepError::Cancelled);
    }
    let mut searcher: Searcher = build_searcher(context.plan, context.variant.source);
    let file_context = FileSearchContext {
        access: &context.access,
        matcher: &context.matcher,
        plan: context.plan,
        cancellation: &context.cancellation,
        retirement: &context.retirement,
        variant: context.variant,
        profiler: &context.profiler,
        resources: Some(&context.resources),
    };
    search_file_with_searcher(candidate, &file_context, &mut searcher, || {})
}

fn run_isolated_retry(
    candidate: &Candidate,
    context: &PipelineContext,
    page: &Page,
    reason: RetryReason,
) -> SearchResult {
    let mut retry_context = context.clone();
    retry_context.retirement = CancellationToken::new();
    if retry_context.plan.mode == GrepMode::Content {
        let (skip, probe) = page.exact_search_window();
        retry_context.plan.skip = skip;
        retry_context.plan.probe = probe;
    }
    let large_heap = retry_context.memory.large_search_heap_ceiling();
    let extra_memory = if reason == RetryReason::HeapLimit {
        let additional = large_heap.saturating_sub(retry_context.memory.base_search_heap_bytes);
        if additional == 0 {
            retry_context
                .profiler
                .record_retry(false, large_heap, false);
            return Ok(FileOutcome::skipped(
                Some(Arc::clone(&candidate.path)),
                SkipReason::LineExceedsSearchHeap,
            ));
        }
        let Some(memory) = retry_context.resources.try_reserve_memory(additional) else {
            retry_context
                .profiler
                .record_retry(false, large_heap, false);
            return Ok(FileOutcome::skipped(
                Some(Arc::clone(&candidate.path)),
                SkipReason::LineExceedsSearchHeap,
            ));
        };
        retry_context.plan.memory.base_search_heap_bytes = large_heap;
        Some(memory)
    } else {
        None
    };
    let outcome = run_candidate(candidate, &retry_context)?;
    drop(extra_memory);
    let success = outcome.retry.is_none() && outcome.skip.is_none();
    retry_context.profiler.record_retry(
        reason == RetryReason::Capture,
        if reason == RetryReason::Capture {
            retry_context.memory.capture_bytes
        } else {
            large_heap
        },
        success,
    );
    Ok(match outcome.retry {
        Some(RetryReason::Capture) => {
            FileOutcome::skipped(Some(Arc::clone(&candidate.path)), SkipReason::CaptureBudget)
        }
        Some(RetryReason::HeapLimit) => FileOutcome::skipped(
            Some(Arc::clone(&candidate.path)),
            SkipReason::LineExceedsSearchHeap,
        ),
        None => outcome,
    })
}

fn record_error(state: &mut PipelineState, context: &PipelineContext, error: GrepError) {
    if state.terminal_error.is_none() {
        state.terminal_error = Some(error);
    }
    state.stopped = true;
    context.retirement.cancel();
}

fn outcome_queue_capacity(
    request_bytes: usize,
    outcome_bytes: usize,
    pool: &FileWorkPool,
) -> usize {
    let byte_capacity = request_bytes.div_ceil(outcome_bytes.max(1)).max(1);
    pool.extra_capacity().max(1).min(byte_capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_queue_is_bounded_by_workers_and_bytes() {
        let pool = FileWorkPool::new(16);
        assert_eq!(outcome_queue_capacity(8 * 1024, 4 * 1024, &pool), 2);
        assert_eq!(outcome_queue_capacity(8 * 1024, 16 * 1024, &pool), 1);
        assert_eq!(outcome_queue_capacity(usize::MAX, 1, &pool), 15);
    }
}
