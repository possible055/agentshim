#[derive(Clone, Copy)]
pub enum GlobStage {
    #[cfg(feature = "bench-internals")]
    Total,
    Setup,
    TraversalWall,
    Render,
    MergeWaitWorker,
    MergeWorkWorker,
}

#[cfg(feature = "bench-internals")]
#[derive(Clone, Debug)]
pub struct GlobStageTimings {
    pub total_ns: u64,
    pub setup_ns: u64,
    pub traversal_wall_ns: u64,
    pub final_sort_ns: u64,
    pub render_ns: u64,
    pub merge_wait_worker_ns: u64,
    pub merge_work_worker_ns: u64,
    pub batches: usize,
    pub matched_entries: usize,
    pub retained_entries: usize,
    pub retained_memory_bytes: usize,
}

#[cfg(feature = "bench-internals")]
#[derive(Clone, Debug)]
pub struct ProfiledGlob {
    pub output: String,
    pub timings: GlobStageTimings,
}

#[cfg(feature = "bench-internals")]
#[derive(Default)]
pub struct GlobProfileCounters {
    total_ns: std::sync::atomic::AtomicU64,
    setup_ns: std::sync::atomic::AtomicU64,
    traversal_wall_ns: std::sync::atomic::AtomicU64,
    final_sort_ns: std::sync::atomic::AtomicU64,
    render_ns: std::sync::atomic::AtomicU64,
    merge_wait_worker_ns: std::sync::atomic::AtomicU64,
    merge_work_worker_ns: std::sync::atomic::AtomicU64,
    batches: std::sync::atomic::AtomicUsize,
    matched_entries: std::sync::atomic::AtomicUsize,
    retained_entries: std::sync::atomic::AtomicUsize,
    retained_memory_bytes: std::sync::atomic::AtomicUsize,
}

#[cfg(feature = "bench-internals")]
pub enum GlobProfiler {
    Disabled,
    Enabled(GlobProfileCounters),
}

#[cfg(feature = "bench-internals")]
impl GlobProfiler {
    pub fn disabled() -> Self {
        Self::Disabled
    }

    pub fn enabled() -> Self {
        Self::Enabled(GlobProfileCounters::default())
    }

    pub fn span(&self, stage: GlobStage) -> GlobSpan<'_> {
        let Self::Enabled(counters) = self else {
            return GlobSpan { active: None };
        };
        GlobSpan {
            active: Some((counters, stage, std::time::Instant::now())),
        }
    }

    pub fn record_batch(&self, matched_entries: usize) {
        let Self::Enabled(counters) = self else {
            return;
        };
        counters
            .batches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        counters
            .matched_entries
            .fetch_add(matched_entries, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_retained(&self, retained_entries: usize, retained_memory_bytes: usize) {
        let Self::Enabled(counters) = self else {
            return;
        };
        counters
            .retained_entries
            .store(retained_entries, std::sync::atomic::Ordering::Relaxed);
        counters
            .retained_memory_bytes
            .store(retained_memory_bytes, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> GlobStageTimings {
        let Self::Enabled(counters) = self else {
            unreachable!("disabled profiler has no snapshot");
        };
        let load_u64 =
            |value: &std::sync::atomic::AtomicU64| value.load(std::sync::atomic::Ordering::Relaxed);
        GlobStageTimings {
            total_ns: load_u64(&counters.total_ns),
            setup_ns: load_u64(&counters.setup_ns),
            traversal_wall_ns: load_u64(&counters.traversal_wall_ns),
            final_sort_ns: load_u64(&counters.final_sort_ns),
            render_ns: load_u64(&counters.render_ns),
            merge_wait_worker_ns: load_u64(&counters.merge_wait_worker_ns),
            merge_work_worker_ns: load_u64(&counters.merge_work_worker_ns),
            batches: counters.batches.load(std::sync::atomic::Ordering::Relaxed),
            matched_entries: counters
                .matched_entries
                .load(std::sync::atomic::Ordering::Relaxed),
            retained_entries: counters
                .retained_entries
                .load(std::sync::atomic::Ordering::Relaxed),
            retained_memory_bytes: counters
                .retained_memory_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

#[cfg(feature = "bench-internals")]
pub struct GlobSpan<'a> {
    active: Option<(&'a GlobProfileCounters, GlobStage, std::time::Instant)>,
}

#[cfg(feature = "bench-internals")]
impl Drop for GlobSpan<'_> {
    fn drop(&mut self) {
        let Some((counters, stage, started)) = self.active else {
            return;
        };
        let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let target = match stage {
            GlobStage::Total => &counters.total_ns,
            GlobStage::Setup => &counters.setup_ns,
            GlobStage::TraversalWall => &counters.traversal_wall_ns,
            GlobStage::Render => &counters.render_ns,
            GlobStage::MergeWaitWorker => &counters.merge_wait_worker_ns,
            GlobStage::MergeWorkWorker => &counters.merge_work_worker_ns,
        };
        let _ = target.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |current| Some(current.saturating_add(elapsed)),
        );
    }
}

#[cfg(not(feature = "bench-internals"))]
#[derive(Default)]
pub struct GlobProfiler;

#[cfg(not(feature = "bench-internals"))]
impl GlobProfiler {
    pub fn disabled() -> Self {
        Self
    }

    pub fn span(&self, _stage: GlobStage) -> GlobSpan {
        let _ = self;
        GlobSpan
    }

    pub fn record_batch(&self, _matched_entries: usize) {
        let _ = self;
    }

    pub fn record_retained(&self, _retained_entries: usize, _retained_memory_bytes: usize) {
        let _ = self;
    }
}

#[cfg(not(feature = "bench-internals"))]
pub struct GlobSpan;

#[cfg(not(feature = "bench-internals"))]
impl Drop for GlobSpan {
    fn drop(&mut self) {}
}
