use std::{env, ffi::OsStr, io, sync::Arc};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

pub const MAX_READ_ONLY_CALLS: usize = 16;
pub const MAX_SEARCH_LANES: usize = 16;
pub const MAX_OPEN_FILES: usize = 64;
pub const DEFAULT_PROCESS_CALLS: usize = 16;
pub const MAX_CONFIGURED_PROCESS_CALLS: usize = 32;
pub const MEMORY_SOFT_TARGET_BYTES: usize = 128 * 1024 * 1024;
const MEMORY_PERMIT_BYTES: usize = 1024;
const TRANSPORT_BLOCKING_THREADS: usize = 2;
const WORKER_ENV: &str = "CODEXSHIM_IO_WORKERS";
const PROCESS_CALLS_ENV: &str = "CODEXSHIM_PROCESS_CALLS";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    pub worker_lanes: usize,
    pub scheduler_threads: usize,
    pub blocking_threads: usize,
    pub process_calls: usize,
}

impl RuntimeConfig {
    /// Resolve bounded runtime parallelism, including the optional worker override.
    ///
    /// # Errors
    ///
    /// Returns invalid input when either runtime environment override is outside its
    /// documented integer range.
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
        let process_calls = parse_process_calls(env::var_os(PROCESS_CALLS_ENV).as_deref())?;
        Ok(Self {
            worker_lanes,
            scheduler_threads: default_scheduler_threads(available),
            blocking_threads: blocking_threads(process_calls),
            process_calls,
        })
    }

    #[must_use]
    pub fn for_tests(worker_lanes: usize) -> Self {
        Self {
            worker_lanes: worker_lanes.clamp(1, MAX_SEARCH_LANES),
            scheduler_threads: 1,
            blocking_threads: blocking_threads(DEFAULT_PROCESS_CALLS),
            process_calls: DEFAULT_PROCESS_CALLS,
        }
    }
}

fn parse_process_calls(value: Option<&OsStr>) -> io::Result<usize> {
    match value {
        None => Ok(DEFAULT_PROCESS_CALLS),
        Some(value) => value
            .to_str()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| (1..=MAX_CONFIGURED_PROCESS_CALLS).contains(value))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{PROCESS_CALLS_ENV} must be an integer from 1 to \
                         {MAX_CONFIGURED_PROCESS_CALLS}"
                    ),
                )
            }),
    }
}

fn blocking_threads(process_calls: usize) -> usize {
    process_calls + MAX_READ_ONLY_CALLS + TRANSPORT_BLOCKING_THREADS
}

#[cfg(windows)]
fn default_worker_lanes(available: usize) -> usize {
    available.saturating_mul(4).clamp(1, MAX_SEARCH_LANES)
}

#[cfg(not(windows))]
fn default_worker_lanes(available: usize) -> usize {
    available.saturating_mul(2).clamp(1, 8)
}

fn default_scheduler_threads(available: usize) -> usize {
    available.clamp(1, 2)
}
