use std::{env, ffi::OsStr, io, time::Duration};

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
pub const DEFAULT_PDF_TEXT_MEMORY_BYTES: usize = 64 * 1024 * 1024;
pub const MIN_PDF_TEXT_MEMORY_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_PDF_TEXT_MEMORY_BYTES: usize = 128 * 1024 * 1024;
pub const DEFAULT_PDF_IMAGE_MEMORY_BYTES: usize = 96 * 1024 * 1024;
pub const MIN_PDF_IMAGE_MEMORY_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PDF_IMAGE_MEMORY_BYTES: usize = 192 * 1024 * 1024;
pub const MEMORY_PERMIT_BYTES: usize = 1024;
pub const MEMORY_GROWTH_BYTES: usize = 1024 * 1024;
/// PDF work is expensive enough that one call at a time is the whole admission policy.
/// Not configurable in this version: there is no demonstrated need to tune it, and an
/// environment variable would be one more way to defeat the resource contract.
pub const MAX_PDF_CALLS: usize = 1;
pub const PDF_GATE_WAIT: Duration = Duration::from_millis(300);
/// Wall-clock ceilings for the PDF work inside `execute_prepared`, excluding queue time.
///
/// Byte ceilings bound how much one call may allocate, not how long it may run. With a
/// single-slot gate, "runs forever" is identical to "no other PDF read can ever start",
/// so a time bound is required for the gate to mean anything.
pub const PDF_TEXT_RUNTIME_LIMIT: Duration = Duration::from_secs(5);
pub const PDF_IMAGE_RUNTIME_LIMIT: Duration = Duration::from_secs(10);
const HOST_BLOCKING_THREADS: usize = 2;
const WORKER_ENV: &str = "AGENTSHIM_IO_WORKERS";
const PROCESS_CALLS_ENV: &str = "AGENTSHIM_PROCESS_CALLS";
const TOOL_TIMEOUT_SHELF_ENV: &str = "AGENTSHIM_TOOL_TIMEOUT_SHELF";
pub const BACKGROUND_JOB_TIMEOUT_MAX_ENV: &str = "AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX";
const IDLE_TIMEOUT_ENV: &str = "AGENTSHIM_IDLE_TIMEOUT";
pub const GREP_MEMORY_BYTES_ENV: &str = "AGENTSHIM_GREP_MEMORY_BYTES";
pub const GLOB_MEMORY_BYTES_ENV: &str = "AGENTSHIM_GLOB_MEMORY_BYTES";
pub const PDF_TEXT_MEMORY_BYTES_ENV: &str = "AGENTSHIM_PDF_TEXT_MEMORY_BYTES";
pub const PDF_IMAGE_MEMORY_BYTES_ENV: &str = "AGENTSHIM_PDF_IMAGE_MEMORY_BYTES";
pub const RESPECT_GITIGNORE_ENV: &str = "AGENTSHIM_RESPECT_GITIGNORE";

/// Default shelf matching the `tool_timeout_sec = 600` documented in every example. The
/// server's own execution ceiling stays below this by the cleanup deadline plus protocol
/// slack so a client configured at the shelf always receives the server's Timeout response
/// before its `tool_timeout_sec` fires.
pub const DEFAULT_TOOL_TIMEOUT_SHELF: Duration = Duration::from_secs(600);
/// Minimum shelf that leaves at least one second of execution time after the cleanup
/// deadline and protocol slack are subtracted.
pub const MIN_TOOL_TIMEOUT_SHELF: Duration = Duration::from_secs(15);
pub const MAX_TOOL_TIMEOUT_SHELF: Duration = Duration::from_secs(3600);
pub const DEFAULT_BACKGROUND_JOB_TIMEOUT_MAX: Duration = Duration::from_secs(1800);
pub const MIN_BACKGROUND_JOB_TIMEOUT_MAX: Duration = Duration::from_secs(600);
pub const MAX_BACKGROUND_JOB_TIMEOUT_MAX: Duration = Duration::from_secs(14400);
/// The floor stays at one second so integration tests can exercise the idle watchdog
/// quickly; the watchdog is opt-in, so the low bound costs nothing in production.
pub const MIN_IDLE_TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(86_400);

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
    pub pdf_text_memory_bytes: usize,
    pub pdf_image_memory_bytes: usize,
    pub memory_bytes: usize,
    pub tool_timeout_shelf: Duration,
    pub background_job_timeout_max: Duration,
    /// Optional host idle-watchdog deadline. `None` disables the watchdog.
    pub idle_timeout: Option<Duration>,
    pub respect_gitignore: bool,
}

impl RuntimeConfig {
    /// Build production defaults for an embedded host without consulting the
    /// process environment. Embedders may adjust the returned value from their
    /// own typed configuration before constructing [`RuntimeResources`].
    #[must_use]
    pub fn for_host_defaults() -> Self {
        let available = std::thread::available_parallelism().map_or(1, usize::from);
        let worker_lanes = default_worker_lanes(available);
        let process_calls = DEFAULT_PROCESS_CALLS;
        let detached_calls = crate::tools::bash::detached::DEFAULT_DETACHED_CALLS;
        Self {
            worker_lanes,
            scheduler_threads: default_scheduler_threads(available),
            blocking_threads: blocking_threads(process_calls, detached_calls),
            process_calls,
            detached_calls,
            output_bytes: crate::output::MODEL_BYTE_LIMIT,
            grep_memory_bytes: DEFAULT_GREP_MEMORY_BYTES,
            glob_memory_bytes: DEFAULT_GLOB_MEMORY_BYTES,
            pdf_text_memory_bytes: DEFAULT_PDF_TEXT_MEMORY_BYTES,
            pdf_image_memory_bytes: DEFAULT_PDF_IMAGE_MEMORY_BYTES,
            memory_bytes: DEFAULT_MEMORY_BYTES,
            tool_timeout_shelf: DEFAULT_TOOL_TIMEOUT_SHELF,
            background_job_timeout_max: DEFAULT_BACKGROUND_JOB_TIMEOUT_MAX,
            idle_timeout: None,
            respect_gitignore: false,
        }
    }

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
        let pdf_text_memory_bytes = parse_memory_bytes_in_range(
            env::var_os(PDF_TEXT_MEMORY_BYTES_ENV).as_deref(),
            PDF_TEXT_MEMORY_BYTES_ENV,
            DEFAULT_PDF_TEXT_MEMORY_BYTES,
            MIN_PDF_TEXT_MEMORY_BYTES,
            MAX_PDF_TEXT_MEMORY_BYTES,
        )?;
        let pdf_image_memory_bytes = parse_memory_bytes_in_range(
            env::var_os(PDF_IMAGE_MEMORY_BYTES_ENV).as_deref(),
            PDF_IMAGE_MEMORY_BYTES_ENV,
            DEFAULT_PDF_IMAGE_MEMORY_BYTES,
            MIN_PDF_IMAGE_MEMORY_BYTES,
            MAX_PDF_IMAGE_MEMORY_BYTES,
        )?;
        let memory_bytes = global_memory_bytes(grep_memory_bytes, glob_memory_bytes);
        let tool_timeout_shelf =
            parse_tool_timeout_shelf(env::var_os(TOOL_TIMEOUT_SHELF_ENV).as_deref())?;
        let background_job_timeout_max = parse_background_job_timeout_max(
            env::var_os(BACKGROUND_JOB_TIMEOUT_MAX_ENV).as_deref(),
        )?;
        let idle_timeout = parse_idle_timeout(env::var_os(IDLE_TIMEOUT_ENV).as_deref())?;
        let respect_gitignore =
            parse_respect_gitignore(env::var_os(RESPECT_GITIGNORE_ENV).as_deref())?;
        // A per-call reservation larger than the pool it is drawn from could never be
        // satisfied, so the call would fail at admission on every attempt.
        for (environment, bytes) in [
            (PDF_TEXT_MEMORY_BYTES_ENV, pdf_text_memory_bytes),
            (PDF_IMAGE_MEMORY_BYTES_ENV, pdf_image_memory_bytes),
        ] {
            if bytes > memory_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{environment} is {bytes} bytes, above the {memory_bytes}-byte shared \
                         runtime memory pool"
                    ),
                ));
            }
        }
        Ok(Self {
            worker_lanes,
            scheduler_threads: default_scheduler_threads(available),
            blocking_threads: blocking_threads(process_calls, detached_calls),
            process_calls,
            detached_calls,
            output_bytes: crate::output::parse_configured_byte_limit(
                env::var_os(crate::output::OUTPUT_BYTES_ENV).as_deref(),
            )?,
            grep_memory_bytes,
            glob_memory_bytes,
            pdf_text_memory_bytes,
            pdf_image_memory_bytes,
            memory_bytes,
            tool_timeout_shelf,
            background_job_timeout_max,
            idle_timeout,
            respect_gitignore,
        })
    }

    #[must_use]
    pub fn for_tests(worker_lanes: usize) -> Self {
        Self {
            worker_lanes: worker_lanes.clamp(1, MAX_SEARCH_LANES),
            scheduler_threads: 1,
            ..Self::for_host_defaults()
        }
    }

    #[must_use]
    pub fn include_ignored(&self, request: Option<bool>) -> bool {
        request.unwrap_or(!self.respect_gitignore)
    }
}

pub const fn global_memory_bytes(grep_memory_bytes: usize, glob_memory_bytes: usize) -> usize {
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

pub fn parse_tool_memory_bytes(
    value: Option<&OsStr>,
    environment: &str,
    default: usize,
) -> io::Result<usize> {
    parse_memory_bytes_in_range(
        value,
        environment,
        default,
        MIN_TOOL_MEMORY_BYTES,
        MAX_TOOL_MEMORY_BYTES,
    )
}

/// The grep/glob helper fixes one 8 MiB–1 GiB range for every caller, which cannot
/// express the narrower per-mode PDF ranges; reusing it would silently widen them.
pub fn parse_memory_bytes_in_range(
    value: Option<&OsStr>,
    environment: &str,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> io::Result<usize> {
    match value {
        None => Ok(default),
        Some(value) => value
            .to_str()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| (minimum..=maximum).contains(value))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{environment} must be an integer from {minimum} to {maximum}"),
                )
            }),
    }
}

pub fn parse_process_calls(value: Option<&OsStr>) -> io::Result<usize> {
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

pub fn parse_respect_gitignore(value: Option<&OsStr>) -> io::Result<bool> {
    match value {
        None => Ok(false),
        Some(value) => value
            .to_str()
            .map(str::trim)
            .and_then(|value| match value.to_ascii_lowercase().as_str() {
                "0" | "false" => Some(false),
                "1" | "true" => Some(true),
                _ => None,
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{RESPECT_GITIGNORE_ENV} must be 0, 1, true, or false"),
                )
            }),
    }
}

pub fn parse_tool_timeout_shelf(value: Option<&OsStr>) -> io::Result<Duration> {
    match value {
        None => Ok(DEFAULT_TOOL_TIMEOUT_SHELF),
        Some(value) => value
            .to_str()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs)
            .filter(|duration| (MIN_TOOL_TIMEOUT_SHELF..=MAX_TOOL_TIMEOUT_SHELF).contains(duration))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{TOOL_TIMEOUT_SHELF_ENV} must be an integer from {} to {} seconds",
                        MIN_TOOL_TIMEOUT_SHELF.as_secs(),
                        MAX_TOOL_TIMEOUT_SHELF.as_secs(),
                    ),
                )
            }),
    }
}

pub fn parse_background_job_timeout_max(value: Option<&OsStr>) -> io::Result<Duration> {
    match value {
        None => Ok(DEFAULT_BACKGROUND_JOB_TIMEOUT_MAX),
        Some(value) => value
            .to_str()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs)
            .filter(|duration| {
                (MIN_BACKGROUND_JOB_TIMEOUT_MAX..=MAX_BACKGROUND_JOB_TIMEOUT_MAX).contains(duration)
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{BACKGROUND_JOB_TIMEOUT_MAX_ENV} must be an integer from {} to {} seconds",
                        MIN_BACKGROUND_JOB_TIMEOUT_MAX.as_secs(),
                        MAX_BACKGROUND_JOB_TIMEOUT_MAX.as_secs(),
                    ),
                )
            }),
    }
}

/// Parse the idle-watchdog deadline. Unset disables the watchdog; a set value outside
/// the documented range fails startup rather than arming a mis-sized watchdog.
pub fn parse_idle_timeout(value: Option<&OsStr>) -> io::Result<Option<Duration>> {
    match value {
        None => Ok(None),
        Some(value) => value
            .to_str()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs)
            .filter(|duration| (MIN_IDLE_TIMEOUT..=MAX_IDLE_TIMEOUT).contains(duration))
            .map(Some)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{IDLE_TIMEOUT_ENV} must be an integer from {} to {} seconds",
                        MIN_IDLE_TIMEOUT.as_secs(),
                        MAX_IDLE_TIMEOUT.as_secs(),
                    ),
                )
            }),
    }
}

pub fn blocking_threads(process_calls: usize, detached_calls: usize) -> usize {
    process_calls + MAX_READ_ONLY_CALLS + detached_calls + HOST_BLOCKING_THREADS
}

#[cfg(windows)]
pub fn default_worker_lanes(available: usize) -> usize {
    available.saturating_mul(4).clamp(1, MAX_SEARCH_LANES)
}

#[cfg(not(windows))]
pub fn default_worker_lanes(available: usize) -> usize {
    available.saturating_mul(2).clamp(1, 8)
}

pub fn default_scheduler_threads(available: usize) -> usize {
    available.clamp(1, 2)
}
