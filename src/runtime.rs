use std::{env, io, sync::Arc};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

pub const MAX_READ_ONLY_CALLS: usize = 8;
pub const MAX_SEARCH_LANES: usize = 16;
pub const MAX_OPEN_FILES: usize = 64;
pub const MAX_PROCESS_CALLS: usize = 2;
pub const MEMORY_BUDGET_BYTES: usize = 64 * 1024 * 1024;
const MEMORY_PERMIT_BYTES: usize = 1024;
const WORKER_ENV: &str = "CODEXSHIM_IO_WORKERS";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    pub worker_lanes: usize,
    pub blocking_threads: usize,
}

impl RuntimeConfig {
    /// Resolve bounded runtime parallelism, including the optional worker override.
    ///
    /// # Errors
    ///
    /// Returns invalid input when `CODEXSHIM_IO_WORKERS` is not an integer from 1 to 16.
    pub fn from_env() -> io::Result<Self> {
        let available = std::thread::available_parallelism().map_or(1, usize::from);
        let default_workers = default_worker_lanes(available);
        let worker_lanes = match env::var_os(WORKER_ENV) {
            None => default_workers,
            Some(value) => value
                .to_str()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| (1..=MAX_SEARCH_LANES).contains(value))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{WORKER_ENV} must be an integer from 1 to {MAX_SEARCH_LANES}"),
                    )
                })?,
        };
        Ok(Self {
            worker_lanes,
            blocking_threads: MAX_SEARCH_LANES,
        })
    }

    #[must_use]
    pub fn for_tests(worker_lanes: usize) -> Self {
        Self {
            worker_lanes: worker_lanes.clamp(1, MAX_SEARCH_LANES),
            blocking_threads: MAX_SEARCH_LANES,
        }
    }
}

fn default_worker_lanes(available: usize) -> usize {
    available.saturating_mul(2).clamp(1, 8)
}

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

    /// Acquire one outer read-only call slot until request or server cancellation.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Cancelled`] when either cancellation token fires.
    pub async fn acquire_read_only(
        &self,
        request: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        acquire(&self.read_only_calls, request, &self.shutdown, 1).await
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

    /// Acquire several shared blocking/search lanes as one global lease.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Cancelled`] when either cancellation token fires.
    pub async fn acquire_workers(
        &self,
        count: usize,
        request: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        let count = count.clamp(1, self.config.worker_lanes);
        let count = u32::try_from(count).map_err(|_| AcquireError::TooLarge)?;
        acquire(&self.worker_lanes, request, &self.shutdown, count).await
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

    /// Acquire one process slot independently from read-only worker admission.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Cancelled`] when either cancellation token fires.
    pub async fn acquire_process(
        &self,
        request: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        acquire(&self.process_calls, request, &self.shutdown, 1).await
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

#[cfg(test)]
mod tests {
    use super::{
        AcquireError, MAX_PROCESS_CALLS, MEMORY_BUDGET_BYTES, RuntimeConfig, RuntimeResources,
        default_worker_lanes,
    };
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn read_only_admission_is_bounded_and_cancellable() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(1));
        let request = CancellationToken::new();
        let mut permits = Vec::new();
        for _ in 0..super::MAX_READ_ONLY_CALLS {
            permits.push(
                resources
                    .acquire_read_only(&request)
                    .await
                    .expect("acquire slot"),
            );
        }

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            resources.acquire_read_only(&cancelled).await.unwrap_err(),
            AcquireError::Cancelled
        );
        drop(permits);
    }

    #[test]
    fn default_workers_allow_bounded_io_overlap() {
        assert_eq!(default_worker_lanes(1), 2);
        assert_eq!(default_worker_lanes(2), 4);
        assert_eq!(default_worker_lanes(4), 8);
        assert_eq!(default_worker_lanes(64), 8);
    }

    #[tokio::test]
    async fn memory_reservations_are_hard_bounded() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(1));
        let request = CancellationToken::new();
        assert_eq!(
            resources
                .reserve_memory(MEMORY_BUDGET_BYTES + 1, &request)
                .await
                .unwrap_err(),
            AcquireError::TooLarge
        );
    }

    #[tokio::test]
    async fn process_admission_is_independent_bounded_and_cancellable() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(1));
        let request = CancellationToken::new();
        let mut permits = Vec::new();
        for _ in 0..MAX_PROCESS_CALLS {
            permits.push(
                resources
                    .acquire_process(&request)
                    .await
                    .expect("process slot"),
            );
        }
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            resources.acquire_process(&cancelled).await.unwrap_err(),
            AcquireError::Cancelled
        );
        drop(permits);
        let _read_only = resources
            .acquire_read_only(&request)
            .await
            .expect("read-only admission remains independent");
    }
}
