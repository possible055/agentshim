use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Condvar, Mutex},
};

use grep_regex::RegexMatcher;
use grep_searcher::Searcher;
use tokio_util::sync::CancellationToken;

use crate::{
    output::SkipReason, path::FileAccess, runtime::RuntimeResources, tools::read::FileFingerprint,
    traversal::TraversalSummary,
};

#[cfg(any(test, feature = "bench-internals"))]
use super::request::PathnameReopenPolicy;
use super::{
    profile::{GrepProfiler, GrepStage, GrepWorkerActivity},
    request::{
        CONTENT_SEARCH_BATCH_SIZE, GrepBenchmarkVariant, GrepError, GrepMode, GrepRequest,
        SEARCH_HEAP_BYTES, STREAM_SEARCH_BATCH_SIZE,
    },
    result::{Page, ReduceControl},
};

use super::{
    candidates::Candidate,
    file_search::{
        FileOutcome, FileSearchContext, OpenedCandidate, SearchPlan, build_searcher,
        fingerprint_opened_candidate, requires_path_identity, search_file_with_searcher,
        search_opened_candidate_with_searcher,
    },
};
type SearchResult = Result<FileOutcome, GrepError>;

struct ReadySearchBatch {
    start: usize,
    outcomes: VecDeque<SearchResult>,
    _credit: crate::runtime::FileWorkCredit,
    _memory: tokio::sync::OwnedSemaphorePermit,
}

enum PoolSlot {
    Empty,
    Running { start: usize, end: usize },
    Ready(ReadySearchBatch),
}

struct PoolWindow {
    slots: Box<[PoolSlot]>,
    retired: bool,
    retirement: CancellationToken,
}

type SharedSearchWindow = Arc<(Mutex<PoolWindow>, Condvar)>;

struct WindowRetirementGuard {
    shared: SharedSearchWindow,
    armed: bool,
}

impl WindowRetirementGuard {
    fn new(shared: SharedSearchWindow) -> Self {
        Self {
            shared,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WindowRetirementGuard {
    fn drop(&mut self) {
        if self.armed {
            retire_window(&self.shared);
        }
    }
}

struct SlotRetirementGuard {
    shared: SharedSearchWindow,
    slot_index: usize,
    start: usize,
    armed: bool,
}

impl SlotRetirementGuard {
    fn new(shared: SharedSearchWindow, slot_index: usize, start: usize) -> Self {
        Self {
            shared,
            slot_index,
            start,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SlotRetirementGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (lock, changed) = &*self.shared;
        let (mut state, poisoned) = match lock.lock() {
            Ok(state) => (state, false),
            Err(poisoned) => (poisoned.into_inner(), true),
        };
        if poisoned {
            state.retired = true;
            state.retirement.cancel();
            for slot in &mut state.slots {
                if matches!(slot, PoolSlot::Running { .. } | PoolSlot::Ready(_)) {
                    *slot = PoolSlot::Empty;
                }
            }
        } else if !state.retired
            && matches!(
                state.slots.get(self.slot_index),
                Some(PoolSlot::Running { start, .. }) if *start == self.start
            )
        {
            state.slots[self.slot_index] = PoolSlot::Empty;
        } else if state.retired {
            for slot in &mut state.slots {
                if matches!(slot, PoolSlot::Running { .. }) {
                    *slot = PoolSlot::Empty;
                }
            }
        }
        changed.notify_all();
    }
}

pub(super) struct OrderedSearchContext<'a> {
    pub(super) cancellation: &'a CancellationToken,
    pub(super) access: &'a Arc<FileAccess>,
    pub(super) matcher: &'a Arc<RegexMatcher>,
    pub(super) plan: SearchPlan,
    pub(super) single_file: bool,
    pub(super) variant: GrepBenchmarkVariant,
    pub(super) profiler: &'a GrepProfiler,
    pub(super) resources: &'a RuntimeResources,
}

#[derive(Clone)]
struct OwnedSearchContext {
    cancellation: CancellationToken,
    retirement: CancellationToken,
    access: Arc<FileAccess>,
    matcher: Arc<RegexMatcher>,
    plan: SearchPlan,
    variant: GrepBenchmarkVariant,
    profiler: GrepProfiler,
    resources: RuntimeResources,
}

pub(super) fn ordered_search(
    candidates: &Arc<[Candidate]>,
    _lanes: usize,
    request: &GrepRequest,
    traversal: TraversalSummary,
    context: &OrderedSearchContext<'_>,
) -> Result<Page, GrepError> {
    let pool = context.resources.file_work_pool();
    let shared: SharedSearchWindow = Arc::new((
        Mutex::new(PoolWindow {
            slots: (0..pool.extra_capacity())
                .map(|_| PoolSlot::Empty)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            retired: false,
            retirement: CancellationToken::new(),
        }),
        Condvar::new(),
    ));
    let mut retirement = WindowRetirementGuard::new(Arc::clone(&shared));
    let retirement_token = {
        let (lock, _) = &*shared;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retirement
            .clone()
    };
    let mut page = Page::new(request, traversal, context.plan.allow_early_stop);
    let owned = OwnedSearchContext {
        cancellation: context.cancellation.clone(),
        retirement: retirement_token,
        access: Arc::clone(context.access),
        matcher: Arc::clone(context.matcher),
        plan: context.plan,
        variant: context.variant,
        profiler: context.profiler.clone(),
        resources: context.resources.clone(),
    };
    let reduce_span = context.profiler.span(GrepStage::OrderedReduceWall);
    let mut next_dispatch = 0_usize;
    let mut index = 0_usize;
    while index < candidates.len() {
        if context.cancellation.is_cancelled() {
            return Err(GrepError::Cancelled);
        }
        let current_len = search_batch_len(&candidates[index..], context.plan.mode);
        let current_end = index + current_len;
        if next_dispatch <= index {
            next_dispatch = current_end;
        }
        while next_dispatch < candidates.len() {
            let dispatched = try_dispatch_batch(next_dispatch, candidates, &owned, &pool, &shared);
            if dispatched == 0 {
                break;
            }
            next_dispatch += dispatched;
        }
        let outcomes = match take_ready_batch(index, context, &shared)? {
            Some(outcomes) => outcomes,
            None => run_inline_batch(&candidates[index..current_end], &owned),
        };
        for outcome in outcomes {
            let outcome = outcome?;
            if outcome.retired {
                continue;
            }
            context.profiler.record_reduced_candidate();
            if page.reduce(
                outcome,
                request.mode.unwrap_or_default(),
                context.single_file,
            )? == ReduceControl::PageFull
            {
                drop(reduce_span);
                retire_window(&shared);
                retirement.disarm();
                return Ok(page);
            }
        }
        index = current_end;
    }
    drop(reduce_span);
    page.mark_complete();
    context.profiler.set_scan_complete();
    retire_window(&shared);
    retirement.disarm();
    Ok(page)
}

fn run_candidate_batch(
    candidates: &[Candidate],
    context: &OwnedSearchContext,
) -> VecDeque<SearchResult> {
    let mut outcomes = VecDeque::with_capacity(candidates.len());
    let mut searcher = build_searcher(context.plan, context.variant.source);
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
    if context.cancellation.is_cancelled() {
        outcomes.push_back(Err(GrepError::Cancelled));
        return outcomes;
    }
    if context.retirement.is_cancelled() {
        return outcomes;
    }
    let Some(first) = candidates.first() else {
        return outcomes;
    };
    let parent_span = context.profiler.span(GrepStage::SearchOpenHandleWorker);
    let reader = context.access.open_same_parent_reader(&first.path);
    drop(parent_span);
    let Ok(reader) = reader else {
        outcomes.extend(candidates.iter().map(|candidate| {
            Ok(FileOutcome::skipped(
                Some(Arc::clone(&candidate.path)),
                SkipReason::Io,
            ))
        }));
        return outcomes;
    };
    #[cfg(any(test, feature = "bench-internals"))]
    let parent_batch = context.variant.pathname_reopen == PathnameReopenPolicy::ParentBatch;
    #[cfg(not(any(test, feature = "bench-internals")))]
    let parent_batch = false;
    let Ok(parent_before) = batch_parent_fingerprint(&reader, parent_batch) else {
        outcomes.extend(candidates.iter().map(|candidate| {
            Ok(FileOutcome::skipped(
                Some(Arc::clone(&candidate.path)),
                SkipReason::Io,
            ))
        }));
        return outcomes;
    };
    for candidate in candidates {
        if context.cancellation.is_cancelled() {
            outcomes.push_back(Err(GrepError::Cancelled));
            break;
        }
        if context.retirement.is_cancelled() {
            break;
        }
        let outcome = run_batch_candidate(
            &reader,
            candidate,
            &file_context,
            &mut searcher,
            parent_batch,
        );
        let retired = outcome.as_ref().is_ok_and(|outcome| outcome.retired);
        outcomes.push_back(outcome);
        if retired {
            break;
        }
    }
    if let Some(parent_before) = parent_before {
        validate_batch_parent(&reader, &parent_before, &mut outcomes);
    }
    outcomes
}

#[allow(clippy::unnecessary_wraps)]
fn batch_parent_fingerprint(
    reader: &crate::path::SameParentReader<'_>,
    enabled: bool,
) -> io::Result<Option<FileFingerprint>> {
    #[cfg(not(any(test, feature = "bench-internals")))]
    {
        let _ = (reader, enabled);
        Ok(None)
    }
    #[cfg(any(test, feature = "bench-internals"))]
    {
        if !enabled {
            return Ok(None);
        }
        reader
            .directory()
            .map(FileFingerprint::from_dir)
            .transpose()
    }
}

fn run_batch_candidate(
    reader: &crate::path::SameParentReader<'_>,
    candidate: &Candidate,
    context: &FileSearchContext<'_>,
    searcher: &mut Searcher,
    parent_batch: bool,
) -> SearchResult {
    #[cfg(not(any(test, feature = "bench-internals")))]
    let _ = parent_batch;
    let open_span = context.profiler.span(GrepStage::SearchOpenWorker);
    let handle_span = context.profiler.span(GrepStage::SearchOpenHandleWorker);
    let file = reader.open(&candidate.path);
    drop(handle_span);
    let opened = file.and_then(|file| {
        fingerprint_opened_candidate(
            file,
            context.profiler,
            GrepStage::SearchBeforeFingerprintWorker,
            requires_path_identity(context.variant.pathname_reopen),
        )
    });
    drop(open_span);
    let Ok(OpenedCandidate { file, fingerprint }) = opened else {
        return Ok(FileOutcome::skipped(
            Some(Arc::clone(&candidate.path)),
            SkipReason::Io,
        ));
    };
    #[cfg(any(test, feature = "bench-internals"))]
    let mut outcome = search_opened_candidate_with_searcher(
        candidate,
        context,
        searcher,
        || {},
        file,
        &fingerprint,
    );
    #[cfg(not(any(test, feature = "bench-internals")))]
    let outcome = search_opened_candidate_with_searcher(
        candidate,
        context,
        searcher,
        || {},
        file,
        &fingerprint,
    );
    #[cfg(any(test, feature = "bench-internals"))]
    if parent_batch && outcome.is_ok() {
        let identity = reader
            .open_identity(&candidate.path)
            .and_then(|file| FileFingerprint::from_file(&file));
        if !identity.is_ok_and(|identity| identity == fingerprint) {
            outcome = Ok(FileOutcome::skipped(
                Some(Arc::clone(&candidate.path)),
                SkipReason::ChangedWhileSearched,
            ));
        }
    }
    outcome
}

fn validate_batch_parent(
    reader: &crate::path::SameParentReader<'_>,
    before: &FileFingerprint,
    outcomes: &mut VecDeque<SearchResult>,
) {
    #[cfg(not(any(test, feature = "bench-internals")))]
    {
        let _ = (reader, before, outcomes);
    }
    #[cfg(any(test, feature = "bench-internals"))]
    {
        let after = reader.reopen_parent().and_then(|directory| {
            directory
                .as_ref()
                .map(FileFingerprint::from_dir)
                .transpose()
        });
        if after
            .ok()
            .flatten()
            .is_some_and(|after| before.same_file(&after))
        {
            return;
        }
        for outcome in outcomes {
            if let Ok(current) = outcome {
                let path = current.path.clone();
                *outcome = Ok(FileOutcome::skipped(path, SkipReason::ChangedWhileSearched));
            }
        }
    }
}

fn run_inline_batch(
    candidates: &[Candidate],
    context: &OwnedSearchContext,
) -> VecDeque<SearchResult> {
    let needs_parent_permit = candidates
        .first()
        .is_some_and(|candidate| !candidate.path.is_ambient());
    let parent_open = needs_parent_permit
        .then(|| context.resources.try_acquire_open_file())
        .flatten();
    if !needs_parent_permit || parent_open.is_some() {
        let outcomes = run_candidate_batch(candidates, context);
        drop(parent_open);
        return outcomes;
    }

    let mut searcher = build_searcher(context.plan, context.variant.source);
    #[cfg(any(test, feature = "bench-internals"))]
    let fallback = {
        let mut fallback = context.clone();
        if fallback.variant.pathname_reopen == PathnameReopenPolicy::ParentBatch {
            fallback.variant.pathname_reopen = PathnameReopenPolicy::On;
        }
        fallback
    };
    #[cfg(not(any(test, feature = "bench-internals")))]
    let fallback = context.clone();
    let mut outcomes = VecDeque::with_capacity(candidates.len());
    for candidate in candidates {
        if fallback.cancellation.is_cancelled() {
            outcomes.push_back(Err(GrepError::Cancelled));
            break;
        }
        if fallback.retirement.is_cancelled() {
            break;
        }
        let outcome = run_candidate_with_searcher(candidate, &fallback, &mut searcher);
        let retired = outcome.as_ref().is_ok_and(|outcome| outcome.retired);
        outcomes.push_back(outcome);
        if retired {
            break;
        }
    }
    outcomes
}

fn run_candidate_with_searcher(
    candidate: &Candidate,
    context: &OwnedSearchContext,
    searcher: &mut Searcher,
) -> SearchResult {
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
    search_file_with_searcher(candidate, &file_context, searcher, || {})
}

fn publish_ready_batch(
    shared: &SharedSearchWindow,
    slot_index: usize,
    index: usize,
    ready: ReadySearchBatch,
    retirement: &mut SlotRetirementGuard,
) {
    let (lock, changed) = &**shared;
    match lock.lock() {
        Ok(mut state) => {
            if !state.retired
                && matches!(
                    state.slots[slot_index],
                    PoolSlot::Running { start, .. } if start == index
                )
            {
                state.slots[slot_index] = PoolSlot::Ready(ready);
                retirement.disarm();
            }
            changed.notify_all();
        }
        Err(poisoned) => {
            let mut state = poisoned.into_inner();
            state.retired = true;
            state.retirement.cancel();
            for slot in &mut state.slots {
                if matches!(slot, PoolSlot::Running { .. } | PoolSlot::Ready(_)) {
                    *slot = PoolSlot::Empty;
                }
            }
            drop(ready);
            changed.notify_all();
        }
    }
}

fn try_dispatch_batch(
    index: usize,
    candidates: &Arc<[Candidate]>,
    context: &OwnedSearchContext,
    pool: &Arc<crate::runtime::FileWorkPool>,
    shared: &SharedSearchWindow,
) -> usize {
    let batch_len = search_batch_len(&candidates[index..], context.plan.mode);
    let Some(credit) = pool.try_credit() else {
        return 0;
    };
    let Some(open_file) = context.resources.try_acquire_open_file() else {
        return 0;
    };
    let parent_open = if candidates[index].path.is_ambient() {
        None
    } else {
        let Some(parent_open) = context.resources.try_acquire_open_file() else {
            return 0;
        };
        Some(parent_open)
    };
    let memory_charge = if context.plan.mode == GrepMode::Content {
        SEARCH_HEAP_BYTES.saturating_mul(batch_len)
    } else {
        SEARCH_HEAP_BYTES
    };
    let Some(memory) = context.resources.try_reserve_memory(memory_charge) else {
        return 0;
    };
    let slot_index = {
        let (lock, _) = &**shared;
        let Ok(mut state) = lock.lock() else {
            retire_window(shared);
            return 0;
        };
        if state.retired {
            return 0;
        }
        let Some(slot_index) = state
            .slots
            .iter()
            .position(|slot| matches!(slot, PoolSlot::Empty))
        else {
            return 0;
        };
        state.slots[slot_index] = PoolSlot::Running {
            start: index,
            end: index + batch_len,
        };
        slot_index
    };
    let batch = Arc::clone(candidates);
    let context = context.clone();
    let job_shared = Arc::clone(shared);
    let job = move |credit| {
        let mut retirement = SlotRetirementGuard::new(Arc::clone(&job_shared), slot_index, index);
        let _worker_activity = GrepWorkerActivity::enter();
        let outcomes = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_candidate_batch(&batch[index..index + batch_len], &context)
        }))
        .unwrap_or_else(|_| {
            (0..batch_len)
                .map(|_| Err(GrepError::Io(io::Error::other("grep worker panicked"))))
                .collect()
        });
        drop(open_file);
        drop(parent_open);
        let ready = ReadySearchBatch {
            start: index,
            outcomes,
            _credit: credit,
            _memory: memory,
        };
        publish_ready_batch(&job_shared, slot_index, index, ready, &mut retirement);
    };
    if pool.spawn(credit, job).is_err() {
        let (lock, changed) = &**shared;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.slots[slot_index] = PoolSlot::Empty;
        changed.notify_all();
        return 0;
    }
    batch_len
}

fn search_batch_len(candidates: &[Candidate], mode: GrepMode) -> usize {
    let Some(first) = candidates.first() else {
        return 0;
    };
    let batch_size = match mode {
        GrepMode::Content => CONTENT_SEARCH_BATCH_SIZE,
        GrepMode::Files | GrepMode::Count => STREAM_SEARCH_BATCH_SIZE,
    };
    candidates
        .iter()
        .take(batch_size)
        .take_while(|candidate| first.path.has_same_parent(&candidate.path))
        .count()
}

fn take_ready_batch(
    index: usize,
    context: &OrderedSearchContext<'_>,
    shared: &SharedSearchWindow,
) -> Result<Option<VecDeque<SearchResult>>, GrepError> {
    let (lock, changed) = &**shared;
    let Ok(mut state) = lock.lock() else {
        retire_window(shared);
        return Err(GrepError::PoolPoison);
    };
    loop {
        if context.cancellation.is_cancelled() {
            return Err(GrepError::Cancelled);
        }
        if state.retired {
            return Err(GrepError::PoolPoison);
        }
        if let Some(slot_index) = state
            .slots
            .iter()
            .position(|slot| matches!(slot, PoolSlot::Ready(ready) if ready.start == index))
        {
            let PoolSlot::Ready(ready) =
                std::mem::replace(&mut state.slots[slot_index], PoolSlot::Empty)
            else {
                unreachable!("checked ready slot remains ready");
            };
            changed.notify_all();
            return Ok(Some(ready.outcomes));
        }
        if state.slots.iter().any(|slot| {
            matches!(
                slot,
                PoolSlot::Running { start, end } if *start <= index && index < *end
            )
        }) {
            let wait_span = context.profiler.span(GrepStage::OrderedWaitWorker);
            let waited = changed
                .wait_timeout(state, std::time::Duration::from_millis(10))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = waited.0;
            drop(wait_span);
            continue;
        }
        return Ok(None);
    }
}

fn retire_window(shared: &SharedSearchWindow) {
    let (lock, changed) = &**shared;
    let mut state = match lock.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };
    state.retired = true;
    state.retirement.cancel();
    for slot in &mut state.slots {
        if matches!(slot, PoolSlot::Running { .. } | PoolSlot::Ready(_)) {
            *slot = PoolSlot::Empty;
        }
    }
    changed.notify_all();
}
