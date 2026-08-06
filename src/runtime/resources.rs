#[derive(Clone, Debug)]
pub struct RuntimeResources {
    config: RuntimeConfig,
    read_only_calls: Arc<Semaphore>,
    worker_lanes: Arc<Semaphore>,
    open_files: Arc<Semaphore>,
    process_calls: Arc<Semaphore>,
    memory: Arc<Semaphore>,
    shutdown: CancellationToken,
}

impl RuntimeResources {
    #[must_use]
    pub fn new(config: RuntimeConfig) -> Self {
        Self {
            config,
            read_only_calls: Arc::new(Semaphore::new(MAX_READ_ONLY_CALLS)),
            worker_lanes: Arc::new(Semaphore::new(config.worker_lanes)),
            open_files: Arc::new(Semaphore::new(MAX_OPEN_FILES)),
            process_calls: Arc::new(Semaphore::new(MAX_PROCESS_CALLS)),
            memory: Arc::new(Semaphore::new(MEMORY_BUDGET_BYTES / MEMORY_PERMIT_BYTES)),
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

    /// Acquire one required search lane and any immediately available fair-share lanes.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Cancelled`] while waiting for the required lane.
    pub async fn acquire_search_lanes(
        &self,
        count: usize,
        request: &CancellationToken,
    ) -> Result<SearchLanes, AcquireError> {
        let target = count.clamp(1, self.config.worker_lanes);
        let required = acquire(&self.worker_lanes, request, &self.shutdown, 1).await?;
        let mut permits = Vec::with_capacity(target);
        permits.push(required);
        while permits.len() < target {
            if self.config.worker_lanes > 1 && self.worker_lanes.available_permits() <= 1 {
                break;
            }
            let Ok(permit) = self.worker_lanes.clone().try_acquire_owned() else {
                break;
            };
            permits.push(permit);
        }
        Ok(SearchLanes { permits })
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

    /// Acquire several open-file slots as one global lease.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Cancelled`] when either cancellation token fires.
    pub async fn acquire_open_files(
        &self,
        count: usize,
        request: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        let count = count.clamp(1, MAX_OPEN_FILES);
        let count = u32::try_from(count).map_err(|_| AcquireError::TooLarge)?;
        acquire(&self.open_files, request, &self.shutdown, count).await
    }

    /// Reserve bounded in-memory working space, rounded up to KiB permits.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::TooLarge`] above the global budget, or
    /// [`AcquireError::Cancelled`] when either cancellation token fires.
    pub async fn reserve_memory(
        &self,
        bytes: usize,
        request: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        let permits = bytes.div_ceil(MEMORY_PERMIT_BYTES).max(1);
        let permits = u32::try_from(permits).map_err(|_| AcquireError::TooLarge)?;
        if permits as usize > MEMORY_BUDGET_BYTES / MEMORY_PERMIT_BYTES {
            return Err(AcquireError::TooLarge);
        }
        acquire(&self.memory, request, &self.shutdown, permits).await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AcquireError {
    #[error("request cancelled")]
    Cancelled,
    #[error("requested memory exceeds the global budget")]
    TooLarge,
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
