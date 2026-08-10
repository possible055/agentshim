#[derive(Clone, Debug)]
pub struct RuntimeResources {
    config: RuntimeConfig,
    read_only_calls: Arc<Semaphore>,
    worker_lanes: Arc<Semaphore>,
    open_files: Arc<Semaphore>,
    process_calls: Arc<Semaphore>,
    memory: Arc<Semaphore>,
    pdf_calls: Arc<Semaphore>,
    file_work: Arc<FileWorkPool>,
    shutdown: CancellationToken,
}

pub(crate) struct MemoryReservation {
    resources: RuntimeResources,
    permits: Vec<OwnedSemaphorePermit>,
    reserved_bytes: usize,
}

impl MemoryReservation {
    pub(crate) fn from_initial(
        resources: RuntimeResources,
        permit: OwnedSemaphorePermit,
        reserved_bytes: usize,
    ) -> Self {
        Self {
            resources,
            permits: vec![permit],
            reserved_bytes: rounded_memory_bytes(reserved_bytes),
        }
    }

    pub(crate) fn try_grow_to(&mut self, bytes: usize) -> bool {
        let target = bytes.div_ceil(MEMORY_GROWTH_BYTES) * MEMORY_GROWTH_BYTES;
        if target <= self.reserved_bytes {
            return true;
        }
        let additional = target - self.reserved_bytes;
        let Some(permit) = self.resources.try_reserve_memory(additional) else {
            return false;
        };
        self.permits.push(permit);
        self.reserved_bytes = target;
        true
    }
}

impl RuntimeResources {
    #[must_use]
    pub fn new(config: RuntimeConfig) -> Self {
        Self {
            config,
            read_only_calls: Arc::new(Semaphore::new(MAX_READ_ONLY_CALLS)),
            worker_lanes: Arc::new(Semaphore::new(config.worker_lanes)),
            open_files: Arc::new(Semaphore::new(MAX_OPEN_FILES)),
            process_calls: Arc::new(Semaphore::new(config.process_calls)),
            memory: Arc::new(Semaphore::new(config.memory_bytes / MEMORY_PERMIT_BYTES)),
            pdf_calls: Arc::new(Semaphore::new(MAX_PDF_CALLS)),
            file_work: Arc::new(FileWorkPool::new(config.worker_lanes)),
            shutdown: CancellationToken::new(),
        }
    }

    #[must_use]
    pub fn config(&self) -> RuntimeConfig {
        self.config
    }

    #[must_use]
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    #[must_use]
    pub fn file_work_pool(&self) -> Arc<FileWorkPool> {
        Arc::clone(&self.file_work)
    }

    pub fn try_admit_read_only(&self) -> Option<OwnedSemaphorePermit> {
        self.read_only_calls.clone().try_acquire_owned().ok()
    }

    pub fn try_admit_process(&self) -> Option<OwnedSemaphorePermit> {
        self.process_calls.clone().try_acquire_owned().ok()
    }

    /// Acquire one shared blocking/search lane.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Cancelled`] when either cancellation token fires.
    pub async fn acquire_worker(
        &self,
        request: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        acquire(&self.worker_lanes, request, &self.shutdown, 1).await
    }

    #[must_use]
    #[cfg(any(test, feature = "bench-internals"))]
    pub fn try_acquire_worker(&self) -> Option<OwnedSemaphorePermit> {
        self.worker_lanes.clone().try_acquire_owned().ok()
    }

    /// Acquire one open-file slot.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Cancelled`] when either cancellation token fires.
    pub async fn acquire_open_file(
        &self,
        request: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        acquire(&self.open_files, request, &self.shutdown, 1).await
    }

    #[must_use]
    pub fn try_acquire_open_file(&self) -> Option<OwnedSemaphorePermit> {
        self.open_files.clone().try_acquire_owned().ok()
    }

    /// Reserve best-effort in-memory working space, rounded up to KiB permits.
    ///
    /// # Errors
    ///
    /// Requests above the configured target reserve at most the target and continue; callers
    /// choose an equivalent fallback when a try-only reservation is unavailable.
    pub async fn reserve_memory(
        &self,
        bytes: usize,
        request: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        let permits = bytes
            .div_ceil(MEMORY_PERMIT_BYTES)
            .clamp(1, self.config.memory_bytes / MEMORY_PERMIT_BYTES);
        let permits = u32::try_from(permits).expect("soft memory target fits u32 permits");
        acquire(&self.memory, request, &self.shutdown, permits).await
    }

    /// Acquire the single PDF work slot, waiting at most [`PDF_GATE_WAIT`].
    ///
    /// A bounded wait rather than an immediate failure: plain text reads never touch this
    /// gate, so a PDF pausing here cannot delay them, while two ordinary back-to-back PDF
    /// reads would otherwise turn into an error the caller is expected to retry
    /// immediately — which is just a spin.
    ///
    /// Returns `None` on timeout, cancellation, or shutdown.
    pub async fn acquire_pdf_gate(
        &self,
        request: &CancellationToken,
    ) -> Option<OwnedSemaphorePermit> {
        PDF_GATE_ACQUISITIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::time::timeout(
            PDF_GATE_WAIT,
            acquire(&self.pdf_calls, request, &self.shutdown, 1),
        )
        .await
        .ok()?
        .ok()
    }

    #[must_use]
    #[cfg(test)]
    pub fn try_acquire_pdf_gate(&self) -> Option<OwnedSemaphorePermit> {
        self.pdf_calls.clone().try_acquire_owned().ok()
    }

    /// Free slots on the PDF gate. Used to assert that no path leaks the permit.
    #[must_use]
    #[cfg(test)]
    pub fn available_pdf_slots(&self) -> usize {
        self.pdf_calls.available_permits()
    }
}

/// Counts attempts to take the gate, not successes.
///
/// The retry loop must take the gate at most once per call: releasing between attempts
/// would let another call in, so a caller who hit `file_changed` would come back with
/// `resource_busy` instead. A counter is the only way to tell "held across both attempts"
/// apart from "released and immediately re-taken", which look identical from outside.
static PDF_GATE_ACQUISITIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub fn pdf_gate_acquisitions() -> usize {
    PDF_GATE_ACQUISITIONS.load(std::sync::atomic::Ordering::Relaxed)
}

impl RuntimeResources {

    #[must_use]
    pub fn try_reserve_memory(&self, bytes: usize) -> Option<OwnedSemaphorePermit> {
        let permits = bytes
            .div_ceil(MEMORY_PERMIT_BYTES)
            .clamp(1, self.config.memory_bytes / MEMORY_PERMIT_BYTES);
        let permits = u32::try_from(permits).ok()?;
        self.memory.clone().try_acquire_many_owned(permits).ok()
    }
}

fn rounded_memory_bytes(bytes: usize) -> usize {
    bytes.div_ceil(MEMORY_PERMIT_BYTES) * MEMORY_PERMIT_BYTES
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AcquireError {
    #[error("request cancelled")]
    Cancelled,
}

async fn acquire(
    semaphore: &Arc<Semaphore>,
    request: &CancellationToken,
    shutdown: &CancellationToken,
    permits: u32,
) -> Result<OwnedSemaphorePermit, AcquireError> {
    tokio::select! {
        biased;
        () = request.cancelled() => Err(AcquireError::Cancelled),
        () = shutdown.cancelled() => Err(AcquireError::Cancelled),
        permit = semaphore.clone().acquire_many_owned(permits) => {
            permit.map_err(|_| AcquireError::Cancelled)
        }
    }
}

include!("tests.rs");
