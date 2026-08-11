use std::{
    cmp::Ordering,
    path::Path,
    sync::{Arc, Mutex},
};

use tokio_util::sync::CancellationToken;

use crate::{
    path::{FileAccess, ResolvedPath},
    runtime::{MemoryReservation, RuntimeResources},
    sorting,
    traversal::{
        OwnedTraversalEntry, ParallelTraversal, TraversalControl, TraversalSummary,
        prefer_parallel_root, walk, walk_parallel_batched,
        walk_parallel_batched_with_literal_prefix, walk_with_literal_prefix,
    },
};

#[cfg(any(test, feature = "bench-internals"))]
use super::request::CANDIDATE_SOFT_TARGET_BYTES;
use super::{
    profile::{GrepProfiler, GrepStage},
    request::{
        CandidatePolicy, GrepError, GrepTraversal, PARALLEL_BATCH_SIZE,
        UNSTABLE_SORT_MIN_CANDIDATES,
    },
};

#[derive(Clone, Debug)]
pub(super) struct Candidate {
    pub(super) path: Arc<ResolvedPath>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_candidates(
    access: &FileAccess,
    input: &str,
    glob: Option<&globset::GlobMatcher>,
    cancellation: &CancellationToken,
    traversal: GrepTraversal,
    resources: &RuntimeResources,
    #[cfg(any(test, feature = "bench-internals"))] literal_prefix: Option<&Path>,
    policy: CandidatePolicy,
    profiler: &GrepProfiler,
    memory: Option<MemoryReservation>,
) -> Result<(Vec<Candidate>, TraversalSummary, bool), GrepError> {
    let traversal_span = profiler.span(GrepStage::CandidateTraversal);
    let base = access.resolve(Path::new(input))?;
    if access.metadata_kind(&base)?.is_file {
        let candidate = candidate(base)?;
        let matches = glob.is_none_or(|glob| {
            candidate
                .path
                .slash_path()
                .is_some_and(|path| glob.is_match(path))
        });
        let mut collection =
            CandidateCollection::new(policy, resources.config().grep_memory_bytes, memory);
        if matches {
            collection.admit(candidate)?;
        }
        profiler.record_candidate_metrics(collection.metrics());
        return Ok((collection.candidates, TraversalSummary::default(), true));
    }
    let wants_parallel = match traversal {
        GrepTraversal::Adaptive => prefer_parallel_root(access, &base),
        GrepTraversal::ParallelBatched => true,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepTraversal::ParallelBatchedLiteralPrefix => true,
        GrepTraversal::Serial => false,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepTraversal::SerialLiteralPrefix => false,
    };
    let pool = resources.file_work_pool();
    let traversal_credits = if wants_parallel {
        pool.try_credits(pool.extra_capacity())
    } else {
        Vec::new()
    };
    let traversal_threads = traversal_credits.len().saturating_add(1);
    let traversal = match traversal {
        GrepTraversal::Adaptive if traversal_credits.is_empty() => GrepTraversal::Serial,
        GrepTraversal::Adaptive => GrepTraversal::ParallelBatched,
        GrepTraversal::ParallelBatched if traversal_credits.is_empty() => GrepTraversal::Serial,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepTraversal::ParallelBatchedLiteralPrefix if traversal_credits.is_empty() => {
            GrepTraversal::SerialLiteralPrefix
        }
        selected => selected,
    };
    let collection = CandidateCollection::new(policy, resources.config().grep_memory_bytes, memory);
    let (mut candidates, summary, metrics) = match traversal {
        GrepTraversal::Adaptive => unreachable!("adaptive traversal was resolved"),
        GrepTraversal::Serial => {
            collect_candidates_serial(access, &base, glob, cancellation, None, collection)?
        }
        GrepTraversal::ParallelBatched => collect_candidates_parallel(
            access,
            &base,
            glob,
            cancellation,
            None,
            traversal_threads,
            collection,
        )?,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepTraversal::SerialLiteralPrefix => collect_candidates_serial(
            access,
            &base,
            glob,
            cancellation,
            literal_prefix,
            collection,
        )?,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepTraversal::ParallelBatchedLiteralPrefix => collect_candidates_parallel(
            access,
            &base,
            glob,
            cancellation,
            literal_prefix,
            traversal_threads,
            collection,
        )?,
    };
    drop(traversal_credits);
    drop(traversal_span);
    let sort_span = profiler.span(GrepStage::CandidateSort);
    sort_candidates(&mut candidates, cancellation)?;
    drop(sort_span);
    profiler.record_candidate_metrics(metrics);
    Ok((candidates, summary, false))
}

fn sort_candidates(
    candidates: &mut [Candidate],
    cancellation: &CancellationToken,
) -> Result<(), GrepError> {
    let compare =
        |left: &Candidate, right: &Candidate| left.path.sort_key().cmp(right.path.sort_key());
    #[cfg(feature = "bench-internals")]
    {
        match std::env::var(BENCH_SORT_ENV).as_deref() {
            Ok("heapsort") => sorting::sort_by(candidates, cancellation, compare),
            Ok("unstable") => sorting::sort_unstable_by(candidates, cancellation, compare),
            Err(std::env::VarError::NotPresent) => {
                production_sort_candidates(candidates, cancellation, compare)
            }
            Ok(value) => {
                return Err(GrepError::Validation(format!(
                    "{BENCH_SORT_ENV} must be heapsort or unstable; got {value}"
                )));
            }
            Err(error) => {
                return Err(GrepError::Validation(format!(
                    "{BENCH_SORT_ENV} is not valid Unicode: {error}"
                )));
            }
        }
        .map_err(|_| GrepError::Cancelled)
    }
    #[cfg(not(feature = "bench-internals"))]
    {
        production_sort_candidates(candidates, cancellation, compare)
            .map_err(|_| GrepError::Cancelled)
    }
}

fn production_sort_candidates(
    candidates: &mut [Candidate],
    cancellation: &CancellationToken,
    compare: impl FnMut(&Candidate, &Candidate) -> Ordering,
) -> Result<(), sorting::SortCancelled> {
    if candidates.len() >= UNSTABLE_SORT_MIN_CANDIDATES {
        sorting::sort_unstable_by(candidates, cancellation, compare)
    } else {
        sorting::sort_by(candidates, cancellation, compare)
    }
}

fn collect_candidates_serial(
    access: &FileAccess,
    base: &ResolvedPath,
    glob: Option<&globset::GlobMatcher>,
    cancellation: &CancellationToken,
    literal_prefix: Option<&Path>,
    mut collection: CandidateCollection,
) -> Result<(Vec<Candidate>, TraversalSummary, CandidateMetrics), GrepError> {
    let mut terminal_error = None;
    let mut visit = |entry: crate::traversal::TraversalEntry<'_>| {
        let candidate = match candidate_from_entry(
            access,
            base,
            glob,
            entry.key,
            entry.absolute,
            entry.file_type,
        ) {
            Ok(candidate) => candidate,
            Err(error) => {
                terminal_error = Some(error);
                return TraversalControl::Stop;
            }
        };
        if let Some(candidate) = candidate
            && let Err(error) = collection.admit(candidate)
        {
            terminal_error = Some(error);
            return TraversalControl::Stop;
        }
        TraversalControl::Continue
    };
    let summary = if let Some(literal_prefix) = literal_prefix {
        walk_with_literal_prefix(
            access,
            base,
            false,
            cancellation,
            literal_prefix,
            &mut visit,
        )?
    } else {
        walk(access, base, false, cancellation, &mut visit)?
    };
    if let Some(error) = terminal_error {
        return Err(error);
    }
    let metrics = collection.metrics();
    Ok((collection.candidates, summary, metrics))
}

fn collect_candidates_parallel(
    access: &FileAccess,
    base: &ResolvedPath,
    glob: Option<&globset::GlobMatcher>,
    cancellation: &CancellationToken,
    literal_prefix: Option<&Path>,
    traversal_threads: usize,
    collection: CandidateCollection,
) -> Result<(Vec<Candidate>, TraversalSummary, CandidateMetrics), GrepError> {
    let collection = Mutex::new(collection);
    let visit = |batch: &[OwnedTraversalEntry]| {
        collect_candidate_batch(access, base, glob, batch, &collection)
    };
    let summary = if let Some(literal_prefix) = literal_prefix {
        walk_parallel_batched_with_literal_prefix(
            access,
            base,
            false,
            cancellation,
            ParallelTraversal {
                batch_size: PARALLEL_BATCH_SIZE,
                threads: traversal_threads,
            },
            literal_prefix,
            visit,
        )?
    } else {
        walk_parallel_batched(
            access,
            base,
            false,
            cancellation,
            ParallelTraversal {
                batch_size: PARALLEL_BATCH_SIZE,
                threads: traversal_threads,
            },
            visit,
        )?
    };
    let collection = collection
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(error) = collection.terminal_error {
        return Err(error);
    }
    let metrics = collection.metrics();
    Ok((collection.candidates, summary, metrics))
}

fn collect_candidate_batch(
    access: &FileAccess,
    base: &ResolvedPath,
    glob: Option<&globset::GlobMatcher>,
    batch: &[OwnedTraversalEntry],
    collection: &Mutex<CandidateCollection>,
) -> TraversalControl {
    let mut found = Vec::with_capacity(batch.len());
    for entry in batch {
        match candidate_from_entry(
            access,
            base,
            glob,
            &entry.key,
            &entry.absolute,
            entry.file_type,
        ) {
            Ok(Some(candidate)) => found.push(candidate),
            Ok(None) => {}
            Err(error) => {
                let mut collection = collection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                collection.fail(error);
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
    for candidate in found {
        if let Err(error) = collection.admit(candidate) {
            collection.fail(error);
            return TraversalControl::Stop;
        }
    }
    TraversalControl::Continue
}

fn candidate_from_entry(
    access: &FileAccess,
    base: &ResolvedPath,
    glob: Option<&globset::GlobMatcher>,
    key: &Path,
    absolute: &Path,
    file_type: Option<std::fs::FileType>,
) -> Result<Option<Candidate>, GrepError> {
    if !file_type.is_some_and(|file_type| file_type.is_file() || file_type.is_symlink())
        || glob.is_some_and(|glob| !glob.is_match(key))
    {
        return Ok(None);
    }
    let path = access.resolve_walked_entry(base, key, absolute)?;
    candidate(path).map(Some)
}

pub(super) struct CandidateCollection {
    pub(super) candidates: Vec<Candidate>,
    policy: CandidatePolicy,
    memory_limit: usize,
    memory: Option<MemoryReservation>,
    pub(super) path_retained_bytes: usize,
    pub(super) estimated_retained_bytes: usize,
    pub(super) soft_target_crossings: usize,
    key_bytes: usize,
    key_capacity: usize,
    capability_key_bytes: usize,
    capability_key_capacity: usize,
    absolute_bytes: usize,
    absolute_capacity: usize,
    sort_key_bytes: usize,
    sort_key_capacity: usize,
    slash_path_bytes: usize,
    slash_path_capacity: usize,
    terminal_error: Option<GrepError>,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CandidateMetrics {
    pub(super) count: usize,
    pub(super) estimated_retained_bytes: usize,
    pub(super) vec_capacity: usize,
    pub(super) soft_target_crossings: usize,
    pub(super) key_bytes: usize,
    pub(super) key_capacity: usize,
    pub(super) capability_key_bytes: usize,
    pub(super) capability_key_capacity: usize,
    pub(super) absolute_bytes: usize,
    pub(super) absolute_capacity: usize,
    pub(super) sort_key_bytes: usize,
    pub(super) sort_key_capacity: usize,
    pub(super) slash_path_bytes: usize,
    pub(super) slash_path_capacity: usize,
}

impl CandidateCollection {
    pub(super) fn new(
        policy: CandidatePolicy,
        memory_limit: usize,
        memory: Option<MemoryReservation>,
    ) -> Self {
        let candidates = Vec::with_capacity(1_024);
        let estimated_retained_bytes = candidates
            .capacity()
            .saturating_mul(std::mem::size_of::<Candidate>());
        Self {
            candidates,
            policy,
            memory_limit,
            memory,
            path_retained_bytes: 0,
            estimated_retained_bytes,
            soft_target_crossings: 0,
            key_bytes: 0,
            key_capacity: 0,
            capability_key_bytes: 0,
            capability_key_capacity: 0,
            absolute_bytes: 0,
            absolute_capacity: 0,
            sort_key_bytes: 0,
            sort_key_capacity: 0,
            slash_path_bytes: 0,
            slash_path_capacity: 0,
            terminal_error: None,
        }
    }

    pub(super) fn admit(&mut self, candidate: Candidate) -> Result<(), GrepError> {
        let components = candidate.path.memory_components();
        self.candidates
            .try_reserve(1)
            .map_err(|_| GrepError::CandidateMemory)?;
        let path_retained = self
            .path_retained_bytes
            .saturating_add(components.key_capacity)
            .saturating_add(components.capability_key_capacity)
            .saturating_add(components.absolute_capacity)
            .saturating_add(components.sort_key_capacity)
            .saturating_add(components.slash_path_capacity)
            .saturating_add(std::mem::size_of::<ResolvedPath>());
        let retained = path_retained.saturating_add(
            self.candidates
                .capacity()
                .saturating_mul(std::mem::size_of::<Candidate>()),
        );
        let hard_limit = self.memory_limit.min(match self.policy {
            #[cfg(any(test, feature = "bench-internals"))]
            CandidatePolicy::FatalCeiling => CANDIDATE_SOFT_TARGET_BYTES,
            CandidatePolicy::SoftTarget => self.memory_limit,
        });
        if retained > hard_limit {
            return Err(GrepError::CandidateMemory);
        }
        if self
            .memory
            .as_mut()
            .is_some_and(|memory| !memory.try_grow_to(retained))
        {
            return Err(GrepError::MemoryBusy);
        }
        self.path_retained_bytes = path_retained;
        self.estimated_retained_bytes = retained;
        self.key_bytes = self.key_bytes.saturating_add(components.key_bytes);
        self.key_capacity = self.key_capacity.saturating_add(components.key_capacity);
        self.capability_key_bytes = self
            .capability_key_bytes
            .saturating_add(components.capability_key_bytes);
        self.capability_key_capacity = self
            .capability_key_capacity
            .saturating_add(components.capability_key_capacity);
        self.absolute_bytes = self
            .absolute_bytes
            .saturating_add(components.absolute_bytes);
        self.absolute_capacity = self
            .absolute_capacity
            .saturating_add(components.absolute_capacity);
        self.sort_key_bytes = self
            .sort_key_bytes
            .saturating_add(components.sort_key_bytes);
        self.sort_key_capacity = self
            .sort_key_capacity
            .saturating_add(components.sort_key_capacity);
        self.slash_path_bytes = self
            .slash_path_bytes
            .saturating_add(components.slash_path_bytes);
        self.slash_path_capacity = self
            .slash_path_capacity
            .saturating_add(components.slash_path_capacity);
        self.candidates.push(candidate);
        #[cfg(any(test, feature = "bench-internals"))]
        if retained > CANDIDATE_SOFT_TARGET_BYTES {
            self.soft_target_crossings = self.soft_target_crossings.saturating_add(1);
        }
        Ok(())
    }

    pub(super) fn metrics(&self) -> CandidateMetrics {
        CandidateMetrics {
            count: self.candidates.len(),
            estimated_retained_bytes: self.estimated_retained_bytes,
            vec_capacity: self.candidates.capacity(),
            soft_target_crossings: self.soft_target_crossings,
            key_bytes: self.key_bytes,
            key_capacity: self.key_capacity,
            capability_key_bytes: self.capability_key_bytes,
            capability_key_capacity: self.capability_key_capacity,
            absolute_bytes: self.absolute_bytes,
            absolute_capacity: self.absolute_capacity,
            sort_key_bytes: self.sort_key_bytes,
            sort_key_capacity: self.sort_key_capacity,
            slash_path_bytes: self.slash_path_bytes,
            slash_path_capacity: self.slash_path_capacity,
        }
    }

    fn fail(&mut self, error: GrepError) {
        if self.terminal_error.is_none() {
            self.terminal_error = Some(error);
        }
    }
}

pub(super) fn candidate(path: ResolvedPath) -> Result<Candidate, GrepError> {
    path.absolute()
        .to_str()
        .ok_or_else(|| GrepError::Validation("candidate path is not valid Unicode".to_owned()))?;
    Ok(Candidate {
        path: Arc::new(path),
    })
}
