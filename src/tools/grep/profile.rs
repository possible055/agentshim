#[cfg(feature = "bench-internals")]
use std::sync::Arc;

#[cfg(feature = "bench-internals")]
use super::candidates::Candidate;
use super::{candidates::CandidateMetrics, request::GrepSourcePolicy};

#[derive(Clone, Copy)]
pub(super) enum GrepStage {
    #[cfg(feature = "bench-internals")]
    Total,
    Setup,
    CandidateTraversal,
    CandidateSort,
    SearchWall,
    SearchOpenWorker,
    SearchOpenHandleWorker,
    SearchSymlinkMetadataWorker,
    SearchScanWorker,
    CaptureReadWorker,
    ClassificationWorker,
    SearchReaderWorker,
    SearchFileWorker,
    SearchSliceWorker,
    SearchBeforeFingerprintWorker,
    SearchAfterFingerprintWorker,
    #[cfg(any(test, feature = "bench-internals"))]
    SearchPathnameReopenWorker,
    #[cfg(any(test, feature = "bench-internals"))]
    SearchPathnameFingerprintWorker,
    SearchVerifyWorker,
    OrderedReduceWall,
    OrderedWaitWorker,
    Render,
}

#[cfg(feature = "bench-internals")]
#[derive(Clone, Debug)]
pub struct GrepStageTimings {
    pub total_ns: u64,
    pub setup_ns: u64,
    pub candidate_traversal_ns: u64,
    pub candidate_sort_ns: u64,
    pub search_wall_ns: u64,
    pub search_open_worker_ns: u64,
    pub search_open_handle_worker_ns: u64,
    pub search_symlink_metadata_worker_ns: u64,
    pub search_scan_worker_ns: u64,
    pub capture_read_worker_ns: u64,
    pub classification_worker_ns: u64,
    pub search_reader_worker_ns: u64,
    pub search_file_worker_ns: u64,
    pub search_slice_worker_ns: u64,
    pub search_before_fingerprint_worker_ns: u64,
    pub search_after_fingerprint_worker_ns: u64,
    pub search_pathname_reopen_worker_ns: u64,
    pub search_pathname_fingerprint_worker_ns: u64,
    pub search_verify_worker_ns: u64,
    pub ordered_reduce_wall_ns: u64,
    pub ordered_wait_worker_ns: u64,
    pub render_ns: u64,
    pub search_reader_files: usize,
    pub search_file_files: usize,
    pub search_slice_files: usize,
    pub mmap_requested_files: usize,
    pub pathname_reopens: usize,
    pub render_copy_bytes: usize,
    pub candidate_count: usize,
    pub searched_candidates: usize,
    pub matched_candidates: usize,
    pub reduced_candidates: usize,
    pub scan_complete: bool,
    pub candidate_estimated_retained_bytes: usize,
    pub candidate_retained_memory_bytes: usize,
    pub candidate_vec_capacity: usize,
    pub candidate_soft_target_crossings: usize,
    pub candidate_key_bytes: usize,
    pub candidate_key_capacity: usize,
    pub candidate_capability_key_bytes: usize,
    pub candidate_capability_key_capacity: usize,
    pub candidate_absolute_bytes: usize,
    pub candidate_absolute_capacity: usize,
    pub candidate_sort_key_bytes: usize,
    pub candidate_sort_key_capacity: usize,
    pub candidate_slash_path_bytes: usize,
    pub candidate_slash_path_capacity: usize,
    pub lanes: usize,
    pub speculative_lease_requested_bytes: usize,
    pub speculative_lease_granted_bytes: usize,
    pub capture_exact_retries: usize,
    pub heap_limit_retries: usize,
    pub retry_successes: usize,
    pub retry_ceiling_bytes: usize,
    pub legacy_stream_files: usize,
}

#[cfg(feature = "bench-internals")]
#[derive(Clone, Debug)]
pub struct ProfiledGrep {
    pub output: String,
    pub timings: GrepStageTimings,
}

#[cfg(feature = "bench-internals")]
#[derive(Clone, Copy, Debug)]
pub struct GrepWorkerMetrics {
    pub spawned: usize,
    pub peak_active: usize,
    pub active: usize,
}

#[cfg(feature = "bench-internals")]
pub(super) struct GrepProfileCounters {
    total_ns: std::sync::atomic::AtomicU64,
    setup_ns: std::sync::atomic::AtomicU64,
    candidate_traversal_ns: std::sync::atomic::AtomicU64,
    candidate_sort_ns: std::sync::atomic::AtomicU64,
    search_wall_ns: std::sync::atomic::AtomicU64,
    search_open_worker_ns: std::sync::atomic::AtomicU64,
    search_open_handle_worker_ns: std::sync::atomic::AtomicU64,
    search_symlink_metadata_worker_ns: std::sync::atomic::AtomicU64,
    search_scan_worker_ns: std::sync::atomic::AtomicU64,
    capture_read_worker_ns: std::sync::atomic::AtomicU64,
    classification_worker_ns: std::sync::atomic::AtomicU64,
    search_reader_worker_ns: std::sync::atomic::AtomicU64,
    search_file_worker_ns: std::sync::atomic::AtomicU64,
    search_slice_worker_ns: std::sync::atomic::AtomicU64,
    search_before_fingerprint_worker_ns: std::sync::atomic::AtomicU64,
    search_after_fingerprint_worker_ns: std::sync::atomic::AtomicU64,
    search_pathname_reopen_worker_ns: std::sync::atomic::AtomicU64,
    search_pathname_fingerprint_worker_ns: std::sync::atomic::AtomicU64,
    search_verify_worker_ns: std::sync::atomic::AtomicU64,
    ordered_reduce_wall_ns: std::sync::atomic::AtomicU64,
    ordered_wait_worker_ns: std::sync::atomic::AtomicU64,
    render_ns: std::sync::atomic::AtomicU64,
    search_reader_files: std::sync::atomic::AtomicUsize,
    search_file_files: std::sync::atomic::AtomicUsize,
    search_slice_files: std::sync::atomic::AtomicUsize,
    mmap_requested_files: std::sync::atomic::AtomicUsize,
    pathname_reopens: std::sync::atomic::AtomicUsize,
    render_copy_bytes: std::sync::atomic::AtomicUsize,
    candidate_count: std::sync::atomic::AtomicUsize,
    searched_candidates: std::sync::atomic::AtomicUsize,
    matched_candidates: std::sync::atomic::AtomicUsize,
    reduced_candidates: std::sync::atomic::AtomicUsize,
    scan_complete: std::sync::atomic::AtomicBool,
    candidate_estimated_retained_bytes: std::sync::atomic::AtomicUsize,
    candidate_vec_capacity: std::sync::atomic::AtomicUsize,
    candidate_soft_target_crossings: std::sync::atomic::AtomicUsize,
    candidate_key_bytes: std::sync::atomic::AtomicUsize,
    candidate_key_capacity: std::sync::atomic::AtomicUsize,
    candidate_capability_key_bytes: std::sync::atomic::AtomicUsize,
    candidate_capability_key_capacity: std::sync::atomic::AtomicUsize,
    candidate_absolute_bytes: std::sync::atomic::AtomicUsize,
    candidate_absolute_capacity: std::sync::atomic::AtomicUsize,
    candidate_sort_key_bytes: std::sync::atomic::AtomicUsize,
    candidate_sort_key_capacity: std::sync::atomic::AtomicUsize,
    candidate_slash_path_bytes: std::sync::atomic::AtomicUsize,
    candidate_slash_path_capacity: std::sync::atomic::AtomicUsize,
    lanes: std::sync::atomic::AtomicUsize,
    speculative_lease_requested_bytes: std::sync::atomic::AtomicUsize,
    speculative_lease_granted_bytes: std::sync::atomic::AtomicUsize,
    capture_exact_retries: std::sync::atomic::AtomicUsize,
    heap_limit_retries: std::sync::atomic::AtomicUsize,
    retry_successes: std::sync::atomic::AtomicUsize,
    retry_ceiling_bytes: std::sync::atomic::AtomicUsize,
    legacy_stream_files: std::sync::atomic::AtomicUsize,
}

#[cfg(feature = "bench-internals")]
impl Default for GrepProfileCounters {
    fn default() -> Self {
        Self {
            total_ns: std::sync::atomic::AtomicU64::new(0),
            setup_ns: std::sync::atomic::AtomicU64::new(0),
            candidate_traversal_ns: std::sync::atomic::AtomicU64::new(0),
            candidate_sort_ns: std::sync::atomic::AtomicU64::new(0),
            search_wall_ns: std::sync::atomic::AtomicU64::new(0),
            search_open_worker_ns: std::sync::atomic::AtomicU64::new(0),
            search_open_handle_worker_ns: std::sync::atomic::AtomicU64::new(0),
            search_symlink_metadata_worker_ns: std::sync::atomic::AtomicU64::new(0),
            search_scan_worker_ns: std::sync::atomic::AtomicU64::new(0),
            capture_read_worker_ns: std::sync::atomic::AtomicU64::new(0),
            classification_worker_ns: std::sync::atomic::AtomicU64::new(0),
            search_reader_worker_ns: std::sync::atomic::AtomicU64::new(0),
            search_file_worker_ns: std::sync::atomic::AtomicU64::new(0),
            search_slice_worker_ns: std::sync::atomic::AtomicU64::new(0),
            search_before_fingerprint_worker_ns: std::sync::atomic::AtomicU64::new(0),
            search_after_fingerprint_worker_ns: std::sync::atomic::AtomicU64::new(0),
            search_pathname_reopen_worker_ns: std::sync::atomic::AtomicU64::new(0),
            search_pathname_fingerprint_worker_ns: std::sync::atomic::AtomicU64::new(0),
            search_verify_worker_ns: std::sync::atomic::AtomicU64::new(0),
            ordered_reduce_wall_ns: std::sync::atomic::AtomicU64::new(0),
            ordered_wait_worker_ns: std::sync::atomic::AtomicU64::new(0),
            render_ns: std::sync::atomic::AtomicU64::new(0),
            search_reader_files: std::sync::atomic::AtomicUsize::new(0),
            search_file_files: std::sync::atomic::AtomicUsize::new(0),
            search_slice_files: std::sync::atomic::AtomicUsize::new(0),
            mmap_requested_files: std::sync::atomic::AtomicUsize::new(0),
            pathname_reopens: std::sync::atomic::AtomicUsize::new(0),
            render_copy_bytes: std::sync::atomic::AtomicUsize::new(0),
            candidate_count: std::sync::atomic::AtomicUsize::new(0),
            searched_candidates: std::sync::atomic::AtomicUsize::new(0),
            matched_candidates: std::sync::atomic::AtomicUsize::new(0),
            reduced_candidates: std::sync::atomic::AtomicUsize::new(0),
            scan_complete: std::sync::atomic::AtomicBool::new(false),
            candidate_estimated_retained_bytes: std::sync::atomic::AtomicUsize::new(0),
            candidate_vec_capacity: std::sync::atomic::AtomicUsize::new(0),
            candidate_soft_target_crossings: std::sync::atomic::AtomicUsize::new(0),
            candidate_key_bytes: std::sync::atomic::AtomicUsize::new(0),
            candidate_key_capacity: std::sync::atomic::AtomicUsize::new(0),
            candidate_capability_key_bytes: std::sync::atomic::AtomicUsize::new(0),
            candidate_capability_key_capacity: std::sync::atomic::AtomicUsize::new(0),
            candidate_absolute_bytes: std::sync::atomic::AtomicUsize::new(0),
            candidate_absolute_capacity: std::sync::atomic::AtomicUsize::new(0),
            candidate_sort_key_bytes: std::sync::atomic::AtomicUsize::new(0),
            candidate_sort_key_capacity: std::sync::atomic::AtomicUsize::new(0),
            candidate_slash_path_bytes: std::sync::atomic::AtomicUsize::new(0),
            candidate_slash_path_capacity: std::sync::atomic::AtomicUsize::new(0),
            lanes: std::sync::atomic::AtomicUsize::new(0),
            speculative_lease_requested_bytes: std::sync::atomic::AtomicUsize::new(0),
            speculative_lease_granted_bytes: std::sync::atomic::AtomicUsize::new(0),
            capture_exact_retries: std::sync::atomic::AtomicUsize::new(0),
            heap_limit_retries: std::sync::atomic::AtomicUsize::new(0),
            retry_successes: std::sync::atomic::AtomicUsize::new(0),
            retry_ceiling_bytes: std::sync::atomic::AtomicUsize::new(0),
            legacy_stream_files: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[cfg(feature = "bench-internals")]
#[derive(Clone)]
pub(super) enum GrepProfiler {
    Disabled,
    Enabled(Arc<GrepProfileCounters>),
}

#[cfg(feature = "bench-internals")]
impl GrepProfiler {
    pub(super) fn disabled() -> Self {
        Self::Disabled
    }

    pub(super) fn enabled() -> Self {
        Self::Enabled(Arc::default())
    }

    pub(super) fn span(&self, stage: GrepStage) -> GrepSpan<'_> {
        let Self::Enabled(counters) = self else {
            return GrepSpan { active: None };
        };
        GrepSpan {
            active: Some((counters, stage, std::time::Instant::now())),
        }
    }

    pub(super) fn set_workload(&self, candidate_count: usize, lanes: usize) {
        let Self::Enabled(counters) = self else {
            return;
        };
        counters
            .candidate_count
            .store(candidate_count, std::sync::atomic::Ordering::Relaxed);
        counters
            .lanes
            .store(lanes, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn record_candidate_metrics(&self, metrics: CandidateMetrics) {
        let Self::Enabled(counters) = self else {
            return;
        };
        counters
            .candidate_count
            .store(metrics.count, std::sync::atomic::Ordering::Relaxed);
        counters.candidate_estimated_retained_bytes.store(
            metrics.estimated_retained_bytes,
            std::sync::atomic::Ordering::Relaxed,
        );
        counters
            .candidate_vec_capacity
            .store(metrics.vec_capacity, std::sync::atomic::Ordering::Relaxed);
        counters.candidate_soft_target_crossings.store(
            metrics.soft_target_crossings,
            std::sync::atomic::Ordering::Relaxed,
        );
        counters
            .candidate_key_bytes
            .store(metrics.key_bytes, std::sync::atomic::Ordering::Relaxed);
        counters
            .candidate_key_capacity
            .store(metrics.key_capacity, std::sync::atomic::Ordering::Relaxed);
        counters.candidate_capability_key_bytes.store(
            metrics.capability_key_bytes,
            std::sync::atomic::Ordering::Relaxed,
        );
        counters.candidate_capability_key_capacity.store(
            metrics.capability_key_capacity,
            std::sync::atomic::Ordering::Relaxed,
        );
        counters
            .candidate_absolute_bytes
            .store(metrics.absolute_bytes, std::sync::atomic::Ordering::Relaxed);
        counters.candidate_absolute_capacity.store(
            metrics.absolute_capacity,
            std::sync::atomic::Ordering::Relaxed,
        );
        counters
            .candidate_sort_key_bytes
            .store(metrics.sort_key_bytes, std::sync::atomic::Ordering::Relaxed);
        counters.candidate_sort_key_capacity.store(
            metrics.sort_key_capacity,
            std::sync::atomic::Ordering::Relaxed,
        );
        counters.candidate_slash_path_bytes.store(
            metrics.slash_path_bytes,
            std::sync::atomic::Ordering::Relaxed,
        );
        counters.candidate_slash_path_capacity.store(
            metrics.slash_path_capacity,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub(super) fn record_search_reader(&self) {
        if let Self::Enabled(counters) = self {
            counters
                .search_reader_files
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn record_legacy_stream(&self) {
        if let Self::Enabled(counters) = self {
            counters
                .legacy_stream_files
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn record_speculative_lease(&self, requested: usize, granted: bool) {
        let Self::Enabled(counters) = self else {
            return;
        };
        let _ = counters.speculative_lease_requested_bytes.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |current| Some(current.saturating_add(requested)),
        );
        if granted {
            let _ = counters.speculative_lease_granted_bytes.fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |current| Some(current.saturating_add(requested)),
            );
        }
    }

    pub(super) fn record_retry(&self, capture: bool, ceiling: usize, success: bool) {
        let Self::Enabled(counters) = self else {
            return;
        };
        let retries = if capture {
            &counters.capture_exact_retries
        } else {
            &counters.heap_limit_retries
        };
        retries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        counters
            .retry_ceiling_bytes
            .fetch_max(ceiling, std::sync::atomic::Ordering::Relaxed);
        if success {
            counters
                .retry_successes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn record_search_slice(&self) {
        if let Self::Enabled(counters) = self {
            counters
                .search_slice_files
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn record_search_file(&self, source: GrepSourcePolicy) {
        if let Self::Enabled(counters) = self {
            counters
                .search_file_files
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if matches!(
                source,
                GrepSourcePolicy::MmapAlways | GrepSourcePolicy::MmapThreshold(_)
            ) {
                counters
                    .mmap_requested_files
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    pub(super) fn record_searched_candidate(&self) {
        if let Self::Enabled(counters) = self {
            counters
                .searched_candidates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn record_matched_candidate(&self) {
        if let Self::Enabled(counters) = self {
            counters
                .matched_candidates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn record_reduced_candidate(&self) {
        if let Self::Enabled(counters) = self {
            counters
                .reduced_candidates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn set_scan_complete(&self) {
        if let Self::Enabled(counters) = self {
            counters
                .scan_complete
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn record_pathname_reopen(&self) {
        if let Self::Enabled(counters) = self {
            counters
                .pathname_reopens
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn add_render_copy_bytes(&self, bytes: usize) {
        if let Self::Enabled(counters) = self {
            let _ = counters.render_copy_bytes.fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |current| Some(current.saturating_add(bytes)),
            );
        }
    }

    pub(super) fn snapshot(&self) -> GrepStageTimings {
        let Self::Enabled(counters) = self else {
            unreachable!("disabled profiler has no snapshot");
        };
        let candidate_count = load_usize(&counters.candidate_count);
        let candidate_vec_capacity = load_usize(&counters.candidate_vec_capacity);
        let candidate_estimated_retained_bytes =
            load_usize(&counters.candidate_estimated_retained_bytes);
        GrepStageTimings {
            total_ns: load_u64(&counters.total_ns),
            setup_ns: load_u64(&counters.setup_ns),
            candidate_traversal_ns: load_u64(&counters.candidate_traversal_ns),
            candidate_sort_ns: load_u64(&counters.candidate_sort_ns),
            search_wall_ns: load_u64(&counters.search_wall_ns),
            search_open_worker_ns: load_u64(&counters.search_open_worker_ns),
            search_open_handle_worker_ns: load_u64(&counters.search_open_handle_worker_ns),
            search_symlink_metadata_worker_ns: load_u64(
                &counters.search_symlink_metadata_worker_ns,
            ),
            search_scan_worker_ns: load_u64(&counters.search_scan_worker_ns),
            capture_read_worker_ns: load_u64(&counters.capture_read_worker_ns),
            classification_worker_ns: load_u64(&counters.classification_worker_ns),
            search_reader_worker_ns: load_u64(&counters.search_reader_worker_ns),
            search_file_worker_ns: load_u64(&counters.search_file_worker_ns),
            search_slice_worker_ns: load_u64(&counters.search_slice_worker_ns),
            search_before_fingerprint_worker_ns: load_u64(
                &counters.search_before_fingerprint_worker_ns,
            ),
            search_after_fingerprint_worker_ns: load_u64(
                &counters.search_after_fingerprint_worker_ns,
            ),
            search_pathname_reopen_worker_ns: load_u64(&counters.search_pathname_reopen_worker_ns),
            search_pathname_fingerprint_worker_ns: load_u64(
                &counters.search_pathname_fingerprint_worker_ns,
            ),
            search_verify_worker_ns: load_u64(&counters.search_verify_worker_ns),
            ordered_reduce_wall_ns: load_u64(&counters.ordered_reduce_wall_ns),
            ordered_wait_worker_ns: load_u64(&counters.ordered_wait_worker_ns),
            render_ns: load_u64(&counters.render_ns),
            search_reader_files: load_usize(&counters.search_reader_files),
            search_file_files: load_usize(&counters.search_file_files),
            search_slice_files: load_usize(&counters.search_slice_files),
            mmap_requested_files: load_usize(&counters.mmap_requested_files),
            pathname_reopens: load_usize(&counters.pathname_reopens),
            render_copy_bytes: load_usize(&counters.render_copy_bytes),
            candidate_count,
            searched_candidates: load_usize(&counters.searched_candidates),
            matched_candidates: load_usize(&counters.matched_candidates),
            reduced_candidates: load_usize(&counters.reduced_candidates),
            scan_complete: counters
                .scan_complete
                .load(std::sync::atomic::Ordering::Relaxed),
            candidate_estimated_retained_bytes,
            candidate_retained_memory_bytes: candidate_estimated_retained_bytes.saturating_add(
                candidate_vec_capacity
                    .saturating_sub(candidate_count)
                    .saturating_mul(std::mem::size_of::<Candidate>()),
            ),
            candidate_vec_capacity,
            candidate_soft_target_crossings: load_usize(&counters.candidate_soft_target_crossings),
            candidate_key_bytes: load_usize(&counters.candidate_key_bytes),
            candidate_key_capacity: load_usize(&counters.candidate_key_capacity),
            candidate_capability_key_bytes: load_usize(&counters.candidate_capability_key_bytes),
            candidate_capability_key_capacity: load_usize(
                &counters.candidate_capability_key_capacity,
            ),
            candidate_absolute_bytes: load_usize(&counters.candidate_absolute_bytes),
            candidate_absolute_capacity: load_usize(&counters.candidate_absolute_capacity),
            candidate_sort_key_bytes: load_usize(&counters.candidate_sort_key_bytes),
            candidate_sort_key_capacity: load_usize(&counters.candidate_sort_key_capacity),
            candidate_slash_path_bytes: load_usize(&counters.candidate_slash_path_bytes),
            candidate_slash_path_capacity: load_usize(&counters.candidate_slash_path_capacity),
            lanes: load_usize(&counters.lanes),
            speculative_lease_requested_bytes: load_usize(
                &counters.speculative_lease_requested_bytes,
            ),
            speculative_lease_granted_bytes: load_usize(&counters.speculative_lease_granted_bytes),
            capture_exact_retries: load_usize(&counters.capture_exact_retries),
            heap_limit_retries: load_usize(&counters.heap_limit_retries),
            retry_successes: load_usize(&counters.retry_successes),
            retry_ceiling_bytes: load_usize(&counters.retry_ceiling_bytes),
            legacy_stream_files: load_usize(&counters.legacy_stream_files),
        }
    }
}

#[cfg(feature = "bench-internals")]
fn load_u64(counter: &std::sync::atomic::AtomicU64) -> u64 {
    counter.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(feature = "bench-internals")]
fn load_usize(counter: &std::sync::atomic::AtomicUsize) -> usize {
    counter.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(feature = "bench-internals")]
pub(super) struct GrepSpan<'a> {
    active: Option<(&'a GrepProfileCounters, GrepStage, std::time::Instant)>,
}

#[cfg(feature = "bench-internals")]
impl Drop for GrepSpan<'_> {
    fn drop(&mut self) {
        let Some((counters, stage, started)) = self.active else {
            return;
        };
        let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let target = match stage {
            GrepStage::Total => &counters.total_ns,
            GrepStage::Setup => &counters.setup_ns,
            GrepStage::CandidateTraversal => &counters.candidate_traversal_ns,
            GrepStage::CandidateSort => &counters.candidate_sort_ns,
            GrepStage::SearchWall => &counters.search_wall_ns,
            GrepStage::SearchOpenWorker => &counters.search_open_worker_ns,
            GrepStage::SearchOpenHandleWorker => &counters.search_open_handle_worker_ns,
            GrepStage::SearchSymlinkMetadataWorker => &counters.search_symlink_metadata_worker_ns,
            GrepStage::SearchScanWorker => &counters.search_scan_worker_ns,
            GrepStage::CaptureReadWorker => &counters.capture_read_worker_ns,
            GrepStage::ClassificationWorker => &counters.classification_worker_ns,
            GrepStage::SearchReaderWorker => &counters.search_reader_worker_ns,
            GrepStage::SearchFileWorker => &counters.search_file_worker_ns,
            GrepStage::SearchSliceWorker => &counters.search_slice_worker_ns,
            GrepStage::SearchBeforeFingerprintWorker => {
                &counters.search_before_fingerprint_worker_ns
            }
            GrepStage::SearchAfterFingerprintWorker => &counters.search_after_fingerprint_worker_ns,
            GrepStage::SearchPathnameReopenWorker => &counters.search_pathname_reopen_worker_ns,
            GrepStage::SearchPathnameFingerprintWorker => {
                &counters.search_pathname_fingerprint_worker_ns
            }
            GrepStage::SearchVerifyWorker => &counters.search_verify_worker_ns,
            GrepStage::OrderedReduceWall => &counters.ordered_reduce_wall_ns,
            GrepStage::OrderedWaitWorker => &counters.ordered_wait_worker_ns,
            GrepStage::Render => &counters.render_ns,
        };
        let _ = target.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |current| Some(current.saturating_add(elapsed)),
        );
    }
}

#[cfg(feature = "bench-internals")]
static GREP_WORKERS_SPAWNED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(feature = "bench-internals")]
static ACTIVE_GREP_WORKERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(feature = "bench-internals")]
static PEAK_ACTIVE_GREP_WORKERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "bench-internals")]
pub fn reset_worker_metrics() {
    GREP_WORKERS_SPAWNED.store(0, std::sync::atomic::Ordering::Relaxed);
    ACTIVE_GREP_WORKERS.store(0, std::sync::atomic::Ordering::Relaxed);
    PEAK_ACTIVE_GREP_WORKERS.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(feature = "bench-internals")]
pub fn worker_metrics() -> GrepWorkerMetrics {
    GrepWorkerMetrics {
        spawned: GREP_WORKERS_SPAWNED.load(std::sync::atomic::Ordering::Relaxed),
        peak_active: PEAK_ACTIVE_GREP_WORKERS.load(std::sync::atomic::Ordering::Relaxed),
        active: ACTIVE_GREP_WORKERS.load(std::sync::atomic::Ordering::Relaxed),
    }
}

#[cfg(feature = "bench-internals")]
pub(super) struct GrepWorkerActivity;

#[cfg(feature = "bench-internals")]
impl GrepWorkerActivity {
    pub(super) fn enter() -> Self {
        GREP_WORKERS_SPAWNED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let active = ACTIVE_GREP_WORKERS
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            .saturating_add(1);
        PEAK_ACTIVE_GREP_WORKERS.fetch_max(active, std::sync::atomic::Ordering::Relaxed);
        Self
    }
}

#[cfg(feature = "bench-internals")]
impl Drop for GrepWorkerActivity {
    fn drop(&mut self) {
        ACTIVE_GREP_WORKERS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(not(feature = "bench-internals"))]
#[derive(Clone, Default)]
pub(super) struct GrepProfiler;

#[cfg(not(feature = "bench-internals"))]
impl GrepProfiler {
    pub(super) fn disabled() -> Self {
        Self
    }

    pub(super) fn span(&self, _stage: GrepStage) -> GrepSpan {
        let _ = self;
        GrepSpan
    }

    pub(super) fn set_workload(&self, _candidate_count: usize, _lanes: usize) {
        let _ = self;
    }

    pub(super) fn record_candidate_metrics(&self, _metrics: CandidateMetrics) {
        let _ = self;
    }

    pub(super) fn record_search_reader(&self) {
        let _ = self;
    }

    pub(super) fn record_legacy_stream(&self) {
        let _ = self;
    }

    pub(super) fn record_speculative_lease(&self, _requested: usize, _granted: bool) {
        let _ = self;
    }

    pub(super) fn record_retry(&self, _capture: bool, _ceiling: usize, _success: bool) {
        let _ = self;
    }

    pub(super) fn record_search_slice(&self) {
        let _ = self;
    }

    pub(super) fn record_search_file(&self, _source: GrepSourcePolicy) {
        let _ = self;
    }

    pub(super) fn record_searched_candidate(&self) {
        let _ = self;
    }

    pub(super) fn record_matched_candidate(&self) {
        let _ = self;
    }

    pub(super) fn record_reduced_candidate(&self) {
        let _ = self;
    }

    pub(super) fn set_scan_complete(&self) {
        let _ = self;
    }

    #[cfg(test)]
    pub(super) fn record_pathname_reopen(&self) {
        let _ = self;
    }

    pub(super) fn add_render_copy_bytes(&self, _bytes: usize) {
        let _ = self;
    }
}

#[cfg(not(feature = "bench-internals"))]
pub(super) struct GrepSpan;

#[cfg(not(feature = "bench-internals"))]
impl Drop for GrepSpan {
    fn drop(&mut self) {}
}

#[cfg(not(feature = "bench-internals"))]
pub(super) struct GrepWorkerActivity;

#[cfg(not(feature = "bench-internals"))]
impl GrepWorkerActivity {
    pub(super) fn enter() -> Self {
        Self
    }
}
