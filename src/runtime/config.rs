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
    pub scheduler_threads: usize,
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
            scheduler_threads: default_scheduler_threads(available),
            blocking_threads: MAX_SEARCH_LANES,
        })
    }

    #[must_use]
    pub fn for_tests(worker_lanes: usize) -> Self {
        Self {
            worker_lanes: worker_lanes.clamp(1, MAX_SEARCH_LANES),
            scheduler_threads: 1,
            blocking_threads: MAX_SEARCH_LANES,
        }
    }
}

fn default_worker_lanes(available: usize) -> usize {
    available.saturating_mul(2).clamp(1, 8)
}

fn default_scheduler_threads(available: usize) -> usize {
    available.clamp(1, 2)
}

pub struct SearchLanes {
    permits: Vec<OwnedSemaphorePermit>,
}

impl SearchLanes {
    #[must_use]
    pub fn len(&self) -> usize {
        self.permits.len()
    }
}
