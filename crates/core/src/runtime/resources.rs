use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use super::{
    config::{
        MAX_OPEN_FILES, MAX_PDF_CALLS, MAX_READ_ONLY_CALLS, MEMORY_GROWTH_BYTES,
        MEMORY_PERMIT_BYTES, PDF_GATE_WAIT, RuntimeConfig,
    },
    file_work::FileWorkPool,
};

#[derive(Debug)]
pub struct RuntimeCapacity {
    config: RuntimeConfig,
    read_only_calls: Arc<Semaphore>,
    worker_lanes: Arc<Semaphore>,
    open_files: Arc<Semaphore>,
    process_calls: Arc<Semaphore>,
    memory: Arc<Semaphore>,
    pdf_calls: Arc<Semaphore>,
    #[cfg(any(test, feature = "test-hooks"))]
    pdf_gate_acquisitions: Arc<std::sync::atomic::AtomicUsize>,
    file_work: Arc<FileWorkPool>,
}

#[derive(Clone, Debug)]
pub struct RuntimeResources {
    capacity: Arc<RuntimeCapacity>,
    shutdown: CancellationToken,
}

pub struct MemoryReservation {
    capacity: Arc<RuntimeCapacity>,
    permits: Vec<OwnedSemaphorePermit>,
    reserved_bytes: usize,
}

#[derive(Debug)]
pub(crate) struct OutputLease {
    _permits: Vec<OwnedSemaphorePermit>,
}

impl OutputLease {
    pub(crate) fn new(permits: Vec<OwnedSemaphorePermit>) -> Self {
        Self { _permits: permits }
    }
}

impl MemoryReservation {
    pub(crate) fn from_initial(
        resources: &RuntimeResources,
        permit: OwnedSemaphorePermit,
        reserved_bytes: usize,
    ) -> Self {
        Self {
            capacity: Arc::clone(&resources.capacity),
            permits: vec![permit],
            reserved_bytes: rounded_memory_bytes(reserved_bytes),
        }
    }

    pub fn try_grow_to(&mut self, bytes: usize) -> bool {
        let target = bytes.div_ceil(MEMORY_GROWTH_BYTES) * MEMORY_GROWTH_BYTES;
        if target <= self.reserved_bytes {
            return true;
        }
        let additional = target - self.reserved_bytes;
        let Some(permit) = self.capacity.try_reserve_memory(additional) else {
            return false;
        };
        self.permits.push(permit);
        self.reserved_bytes = target;
        true
    }
}

impl RuntimeCapacity {
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
            #[cfg(any(test, feature = "test-hooks"))]
            pdf_gate_acquisitions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            file_work: Arc::new(FileWorkPool::new(config.worker_lanes)),
        }
    }

    #[must_use]
    pub fn config(&self) -> RuntimeConfig {
        self.config
    }

    fn try_reserve_memory(&self, bytes: usize) -> Option<OwnedSemaphorePermit> {
        let permits = bytes
            .div_ceil(MEMORY_PERMIT_BYTES)
            .clamp(1, self.config.memory_bytes / MEMORY_PERMIT_BYTES);
        let permits = u32::try_from(permits).ok()?;
        self.memory.clone().try_acquire_many_owned(permits).ok()
    }
}

impl RuntimeResources {
    #[must_use]
    pub fn new(config: RuntimeConfig) -> Self {
        Self::with_shutdown(config, CancellationToken::new())
    }

    #[must_use]
    pub fn with_shutdown(config: RuntimeConfig, shutdown: CancellationToken) -> Self {
        Self::from_capacity(Arc::new(RuntimeCapacity::new(config)), shutdown)
    }

    #[must_use]
    pub fn from_capacity(capacity: Arc<RuntimeCapacity>, shutdown: CancellationToken) -> Self {
        Self { capacity, shutdown }
    }

    #[must_use]
    pub fn capacity(&self) -> Arc<RuntimeCapacity> {
        Arc::clone(&self.capacity)
    }

    #[must_use]
    pub fn config(&self) -> RuntimeConfig {
        self.capacity.config()
    }

    #[must_use]
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Cancel this engine's shutdown token: the single irreversible
    /// `Accepting -> Stopping` linearization point for process ownership.
    pub fn cancel_shutdown(&self) {
        self.shutdown.cancel();
    }

    /// Non-blocking probe for the idle watchdog: true while any foreground read-only or
    /// process permit is held. Work queued but not yet admitted is invisible by design —
    /// admission re-checks the shutdown token, so such callers fail fast instead of being
    /// killed mid-call.
    #[must_use]
    pub fn has_in_flight_calls(&self) -> bool {
        self.capacity.read_only_calls.available_permits() < MAX_READ_ONLY_CALLS
            || self.capacity.process_calls.available_permits() < self.config().process_calls
    }

    /// Blocking quiescence barrier for foreground process owners: returns once every
    /// configured permit is free, or `false` at `deadline`. The wait deliberately ignores
    /// the shutdown token — cancelling it is what started the shutdown — and acquires
    /// rather than closes the semaphore, so admission keeps working while it drains.
    pub fn wait_for_process_quiescence(&self, deadline: std::time::Instant) -> bool {
        wait_for_permits(
            &self.capacity.process_calls,
            self.config().process_calls,
            deadline,
        )
    }

    pub fn wait_for_quiescence(&self, deadline: std::time::Instant) -> bool {
        self.wait_for_process_quiescence(deadline)
            && wait_for_permits(
                &self.capacity.read_only_calls,
                MAX_READ_ONLY_CALLS,
                deadline,
            )
    }

    #[must_use]
    pub(crate) fn file_work_pool(&self) -> Arc<FileWorkPool> {
        Arc::clone(&self.capacity.file_work)
    }

    pub(crate) fn try_admit_read_only(&self) -> Option<OwnedSemaphorePermit> {
        if self.shutdown.is_cancelled() {
            return None;
        }
        let permit = self
            .capacity
            .read_only_calls
            .clone()
            .try_acquire_owned()
            .ok()?;
        if self.shutdown.is_cancelled() {
            return None;
        }
        Some(permit)
    }

    /// Admission is fail-fast and double-checked around the permit: after the engine
    /// shutdown token fires, no caller may hold — or newly acquire — a foreground slot.
    pub(crate) fn try_admit_process(&self) -> Option<OwnedSemaphorePermit> {
        if self.shutdown.is_cancelled() {
            return None;
        }
        let permit = self
            .capacity
            .process_calls
            .clone()
            .try_acquire_owned()
            .ok()?;
        if self.shutdown.is_cancelled() {
            return None;
        }
        Some(permit)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn try_admit_process_for_test(&self) -> Option<OwnedSemaphorePermit> {
        self.try_admit_process()
    }

    /// Acquire one shared blocking/search lane.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Cancelled`] when either cancellation token fires.
    pub(crate) async fn acquire_worker(
        &self,
        request: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        acquire(&self.capacity.worker_lanes, request, &self.shutdown, 1).await
    }

    #[must_use]
    #[cfg(any(test, feature = "bench-internals"))]
    pub fn try_acquire_worker(&self) -> Option<OwnedSemaphorePermit> {
        self.capacity.worker_lanes.clone().try_acquire_owned().ok()
    }

    /// Acquire one open-file slot.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Cancelled`] when either cancellation token fires.
    pub(crate) async fn acquire_open_file(
        &self,
        request: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        acquire(&self.capacity.open_files, request, &self.shutdown, 1).await
    }

    #[must_use]
    pub(crate) fn try_acquire_open_file(&self) -> Option<OwnedSemaphorePermit> {
        self.capacity.open_files.clone().try_acquire_owned().ok()
    }

    /// Reserve best-effort in-memory working space, rounded up to KiB permits.
    ///
    /// # Errors
    ///
    /// Requests above the configured target reserve at most the target and continue; callers
    /// choose an equivalent fallback when a try-only reservation is unavailable.
    pub(crate) async fn reserve_memory(
        &self,
        bytes: usize,
        request: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        let permits = bytes
            .div_ceil(MEMORY_PERMIT_BYTES)
            .clamp(1, self.config().memory_bytes / MEMORY_PERMIT_BYTES);
        let permits = u32::try_from(permits).expect("soft memory target fits u32 permits");
        acquire(&self.capacity.memory, request, &self.shutdown, permits).await
    }

    /// Acquire the single PDF work slot, waiting at most [`PDF_GATE_WAIT`].
    ///
    /// A bounded wait rather than an immediate failure: plain text reads never touch this
    /// gate, so a PDF pausing here cannot delay them, while two ordinary back-to-back PDF
    /// reads would otherwise turn into an error the caller is expected to retry
    /// immediately — which is just a spin.
    ///
    /// Returns `None` on timeout, cancellation, or shutdown.
    pub(crate) async fn acquire_pdf_gate(
        &self,
        request: &CancellationToken,
    ) -> Option<OwnedSemaphorePermit> {
        #[cfg(any(test, feature = "test-hooks"))]
        self.capacity
            .pdf_gate_acquisitions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::time::timeout(
            PDF_GATE_WAIT,
            acquire(&self.capacity.pdf_calls, request, &self.shutdown, 1),
        )
        .await
        .ok()?
        .ok()
    }

    #[must_use]
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn try_acquire_pdf_gate(&self) -> Option<OwnedSemaphorePermit> {
        self.capacity.pdf_calls.clone().try_acquire_owned().ok()
    }

    /// Free slots on the PDF gate. Used to assert that no path leaks the permit.
    #[must_use]
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn available_pdf_slots(&self) -> usize {
        self.capacity.pdf_calls.available_permits()
    }

    #[must_use]
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn pdf_gate_acquisitions(&self) -> usize {
        self.capacity
            .pdf_gate_acquisitions
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl RuntimeResources {
    #[must_use]
    pub(crate) fn try_reserve_memory(&self, bytes: usize) -> Option<OwnedSemaphorePermit> {
        self.capacity.try_reserve_memory(bytes)
    }

    #[must_use]
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn available_memory_bytes(&self) -> usize {
        self.capacity
            .memory
            .available_permits()
            .saturating_mul(MEMORY_PERMIT_BYTES)
    }
}

fn rounded_memory_bytes(bytes: usize) -> usize {
    bytes.div_ceil(MEMORY_PERMIT_BYTES) * MEMORY_PERMIT_BYTES
}

fn wait_for_permits(
    semaphore: &Arc<Semaphore>,
    permits: usize,
    deadline: std::time::Instant,
) -> bool {
    let permits = u32::try_from(permits).expect("runtime capacity fits u32 permits");
    loop {
        if semaphore.clone().try_acquire_many_owned(permits).is_ok() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
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
