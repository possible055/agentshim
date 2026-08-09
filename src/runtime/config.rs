use std::{env, ffi::OsStr, io, sync::Arc};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

pub const MAX_READ_ONLY_CALLS: usize = 16;
pub const MAX_SEARCH_LANES: usize = 16;
pub const MAX_OPEN_FILES: usize = 64;
pub const DEFAULT_PROCESS_CALLS: usize = 16;
pub const MAX_CONFIGURED_PROCESS_CALLS: usize = 32;
pub const DEFAULT_MEMORY_BYTES: usize = 256 * 1024 * 1024;
pub const DEFAULT_GREP_MEMORY_BYTES: usize = 256 * 1024 * 1024;
pub const DEFAULT_GLOB_MEMORY_BYTES: usize = 32 * 1024 * 1024;
pub const MIN_TOOL_MEMORY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TOOL_MEMORY_BYTES: usize = 1024 * 1024 * 1024;
const MEMORY_PERMIT_BYTES: usize = 1024;
const MEMORY_GROWTH_BYTES: usize = 1024 * 1024;
const TRANSPORT_BLOCKING_THREADS: usize = 2;
const WORKER_ENV: &str = "CODEXSHIM_IO_WORKERS";
const PROCESS_CALLS_ENV: &str = "CODEXSHIM_PROCESS_CALLS";
pub const GREP_MEMORY_BYTES_ENV: &str = "CODEXSHIM_GREP_MEMORY_BYTES";
pub const GLOB_MEMORY_BYTES_ENV: &str = "CODEXSHIM_GLOB_MEMORY_BYTES";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    pub worker_lanes: usize,
    pub scheduler_threads: usize,
    pub blocking_threads: usize,
    pub process_calls: usize,
    pub detached_calls: usize,
    pub output_bytes: usize,
    pub grep_memory_bytes: usize,
    pub glob_memory_bytes: usize,
    pub memory_bytes: usize,
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
        let detached_calls = crate::tools::bash::detached::parse_detached_calls(
            env::var_os(crate::tools::bash::detached::DETACHED_CALLS_ENV).as_deref(),
        )?;
        let grep_memory_bytes = parse_tool_memory_bytes(
            env::var_os(GREP_MEMORY_BYTES_ENV).as_deref(),
            GREP_MEMORY_BYTES_ENV,
            DEFAULT_GREP_MEMORY_BYTES,
        )?;
        let glob_memory_bytes = parse_tool_memory_bytes(
            env::var_os(GLOB_MEMORY_BYTES_ENV).as_deref(),
            GLOB_MEMORY_BYTES_ENV,
            DEFAULT_GLOB_MEMORY_BYTES,
        )?;
        Ok(Self {
            worker_lanes,
            scheduler_threads: default_scheduler_threads(available),
            blocking_threads: blocking_threads(process_calls, detached_calls),
            process_calls,
            detached_calls,
            output_bytes: crate::output::configured_byte_limit()?,
            grep_memory_bytes,
            glob_memory_bytes,
            memory_bytes: global_memory_bytes(grep_memory_bytes, glob_memory_bytes),
        })
    }

    #[must_use]
    pub fn for_tests(worker_lanes: usize) -> Self {
        Self {
            worker_lanes: worker_lanes.clamp(1, MAX_SEARCH_LANES),
            scheduler_threads: 1,
            blocking_threads: blocking_threads(
                DEFAULT_PROCESS_CALLS,
                crate::tools::bash::detached::DEFAULT_DETACHED_CALLS,
            ),
            process_calls: DEFAULT_PROCESS_CALLS,
            detached_calls: crate::tools::bash::detached::DEFAULT_DETACHED_CALLS,
            output_bytes: crate::output::MODEL_BYTE_LIMIT,
            grep_memory_bytes: DEFAULT_GREP_MEMORY_BYTES,
            glob_memory_bytes: DEFAULT_GLOB_MEMORY_BYTES,
            memory_bytes: DEFAULT_MEMORY_BYTES,
        }
    }
}

const fn global_memory_bytes(grep_memory_bytes: usize, glob_memory_bytes: usize) -> usize {
    let search_memory = if grep_memory_bytes > glob_memory_bytes {
        grep_memory_bytes
    } else {
        glob_memory_bytes
    };
    if DEFAULT_MEMORY_BYTES > search_memory {
        DEFAULT_MEMORY_BYTES
    } else {
        search_memory
    }
}

fn parse_tool_memory_bytes(
    value: Option<&OsStr>,
    environment: &str,
    default: usize,
) -> io::Result<usize> {
    match value {
        None => Ok(default),
        Some(value) => value
            .to_str()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| (MIN_TOOL_MEMORY_BYTES..=MAX_TOOL_MEMORY_BYTES).contains(value))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{environment} must be an integer from {MIN_TOOL_MEMORY_BYTES} to \
                         {MAX_TOOL_MEMORY_BYTES}"
                    ),
                )
            }),
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

fn blocking_threads(process_calls: usize, detached_calls: usize) -> usize {
    process_calls + MAX_READ_ONLY_CALLS + detached_calls + TRANSPORT_BLOCKING_THREADS
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
