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

#[derive(Clone, Debug)]
pub struct RuntimeResources {
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
    shutdown: CancellationToken,
}

pub struct MemoryReservation {
    resources: RuntimeResources,
    permits: Vec<OwnedSemaphorePermit>,
    reserved_bytes: usize,
}

impl MemoryReservation {
    pub fn from_initial(
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

    pub fn try_grow_to(&mut self, bytes: usize) -> bool {
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
            #[cfg(any(test, feature = "test-hooks"))]
            pdf_gate_acquisitions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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

    /// Cancel the global shutdown token: the single irreversible
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
        self.read_only_calls.available_permits() < MAX_READ_ONLY_CALLS
            || self.process_calls.available_permits() < self.config.process_calls
    }

    /// Blocking quiescence barrier for foreground process owners: returns once every
    /// configured permit is free, or `false` at `deadline`. The wait deliberately ignores
    /// the shutdown token — cancelling it is what started the shutdown — and acquires
    /// rather than closes the semaphore, so admission keeps working while it drains.
    pub fn wait_for_process_quiescence(&self, deadline: std::time::Instant) -> bool {
        let permits =
            u32::try_from(self.config.process_calls).expect("process capacity fits u32 permits");
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let acquisition = async {
                    self.process_calls
                        .clone()
                        .acquire_many_owned(permits)
                        .await
                        .is_ok()
                };
                let bounded =
                    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), acquisition);
                handle.block_on(bounded).unwrap_or(false)
            }
            Err(_) => loop {
                if self
                    .process_calls
                    .clone()
                    .try_acquire_many_owned(permits)
                    .is_ok()
                {
                    return true;
                }
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            },
        }
    }

    #[must_use]
    pub fn file_work_pool(&self) -> Arc<FileWorkPool> {
        Arc::clone(&self.file_work)
    }

    pub fn try_admit_read_only(&self) -> Option<OwnedSemaphorePermit> {
        self.read_only_calls.clone().try_acquire_owned().ok()
    }

    /// Admission is fail-fast and double-checked around the permit: after the global
    /// shutdown token fires, no caller may hold — or newly acquire — a foreground slot.
    pub fn try_admit_process(&self) -> Option<OwnedSemaphorePermit> {
        if self.shutdown.is_cancelled() {
            return None;
        }
        let permit = self.process_calls.clone().try_acquire_owned().ok()?;
        if self.shutdown.is_cancelled() {
            return None;
        }
        Some(permit)
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
        #[cfg(any(test, feature = "test-hooks"))]
        self.pdf_gate_acquisitions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::time::timeout(
            PDF_GATE_WAIT,
            acquire(&self.pdf_calls, request, &self.shutdown, 1),
        )
        .await
        .ok()?
        .ok()
    }

    #[must_use]
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn try_acquire_pdf_gate(&self) -> Option<OwnedSemaphorePermit> {
        self.pdf_calls.clone().try_acquire_owned().ok()
    }

    /// Free slots on the PDF gate. Used to assert that no path leaks the permit.
    #[must_use]
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn available_pdf_slots(&self) -> usize {
        self.pdf_calls.available_permits()
    }

    #[must_use]
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn pdf_gate_acquisitions(&self) -> usize {
        self.pdf_gate_acquisitions
            .load(std::sync::atomic::Ordering::Relaxed)
    }
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

    #[must_use]
    #[cfg(test)]
    pub fn available_memory_bytes(&self) -> usize {
        self.memory
            .available_permits()
            .saturating_mul(MEMORY_PERMIT_BYTES)
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
