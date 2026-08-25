#[cfg(unix)]
#[path = "process/unix.rs"]
mod implementation;

use std::{ffi::OsStr, io};

pub const WINDOWS_JOB_MEMORY_BYTES_ENV: &str = "AGENTSHIM_WINDOWS_JOB_MEMORY_BYTES";
pub const WINDOWS_PROCESS_MEMORY_BYTES_ENV: &str = "AGENTSHIM_WINDOWS_PROCESS_MEMORY_BYTES";
pub const WINDOWS_ACTIVE_PROCESS_LIMIT_ENV: &str = "AGENTSHIM_WINDOWS_ACTIVE_PROCESS_LIMIT";
const MIN_WINDOWS_MEMORY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_WINDOWS_MEMORY_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_WINDOWS_ACTIVE_PROCESS_LIMIT: u32 = 256;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowsJobLimits {
    pub job_memory_bytes: Option<u64>,
    pub process_memory_bytes: Option<u64>,
    pub active_process_limit: Option<u32>,
}

impl WindowsJobLimits {
    pub fn from_env() -> io::Result<Self> {
        Self::from_values(
            std::env::var_os(WINDOWS_JOB_MEMORY_BYTES_ENV).as_deref(),
            std::env::var_os(WINDOWS_PROCESS_MEMORY_BYTES_ENV).as_deref(),
            std::env::var_os(WINDOWS_ACTIVE_PROCESS_LIMIT_ENV).as_deref(),
        )
    }

    pub fn from_values(
        job_memory: Option<&OsStr>,
        process_memory: Option<&OsStr>,
        active_processes: Option<&OsStr>,
    ) -> io::Result<Self> {
        Ok(Self {
            job_memory_bytes: parse_optional_u64(
                job_memory,
                WINDOWS_JOB_MEMORY_BYTES_ENV,
                MIN_WINDOWS_MEMORY_BYTES,
                MAX_WINDOWS_MEMORY_BYTES,
            )?,
            process_memory_bytes: parse_optional_u64(
                process_memory,
                WINDOWS_PROCESS_MEMORY_BYTES_ENV,
                MIN_WINDOWS_MEMORY_BYTES,
                MAX_WINDOWS_MEMORY_BYTES,
            )?,
            active_process_limit: parse_optional_u32(
                active_processes,
                WINDOWS_ACTIVE_PROCESS_LIMIT_ENV,
                1,
                MAX_WINDOWS_ACTIVE_PROCESS_LIMIT,
            )?,
        })
    }
}

#[cfg(windows)]
pub(crate) fn configured_windows_job_limits() -> io::Result<WindowsJobLimits> {
    if let Some(limits) = WINDOWS_JOB_LIMITS_OVERRIDE.with(std::cell::Cell::get) {
        return Ok(limits);
    }
    WindowsJobLimits::from_env()
}

#[cfg(windows)]
thread_local! {
    static WINDOWS_JOB_LIMITS_OVERRIDE: std::cell::Cell<Option<WindowsJobLimits>> = const { std::cell::Cell::new(None) };
}

#[cfg(windows)]
struct WindowsJobLimitsRestore<'a> {
    current: &'a std::cell::Cell<Option<WindowsJobLimits>>,
    previous: Option<WindowsJobLimits>,
}

#[cfg(windows)]
impl Drop for WindowsJobLimitsRestore<'_> {
    fn drop(&mut self) {
        self.current.set(self.previous);
    }
}

#[cfg(windows)]
pub(crate) fn with_windows_job_limits<T>(limits: WindowsJobLimits, work: impl FnOnce() -> T) -> T {
    WINDOWS_JOB_LIMITS_OVERRIDE.with(|current| {
        let previous = current.replace(Some(limits));
        let _restore = WindowsJobLimitsRestore { current, previous };
        work()
    })
}

#[cfg(not(windows))]
pub(crate) fn with_windows_job_limits<T>(_limits: WindowsJobLimits, work: impl FnOnce() -> T) -> T {
    work()
}

#[cfg(all(test, windows))]
pub(crate) use with_windows_job_limits as with_windows_job_limits_for_test;

fn parse_optional_u64(
    value: Option<&OsStr>,
    name: &str,
    minimum: u64,
    maximum: u64,
) -> io::Result<Option<u64>> {
    value
        .map(|value| {
            value
                .to_str()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| (minimum..=maximum).contains(value))
                .ok_or_else(|| invalid_limit(name, minimum, maximum))
        })
        .transpose()
}

fn parse_optional_u32(
    value: Option<&OsStr>,
    name: &str,
    minimum: u32,
    maximum: u32,
) -> io::Result<Option<u32>> {
    value
        .map(|value| {
            value
                .to_str()
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|value| (minimum..=maximum).contains(value))
                .ok_or_else(|| invalid_limit(name, minimum, maximum))
        })
        .transpose()
}

fn invalid_limit(
    name: &str,
    minimum: impl std::fmt::Display,
    maximum: impl std::fmt::Display,
) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{name} must be an integer from {minimum} to {maximum}"),
    )
}
#[cfg(windows)]
#[path = "process/windows.rs"]
mod implementation;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetachedObservation {
    pub tree_running: bool,
    pub primary_exit: Option<String>,
}

pub use implementation::*;

#[cfg(test)]
mod limits_tests {
    use super::*;

    #[test]
    fn windows_job_limits_are_bounded_and_opt_in() {
        let defaults = WindowsJobLimits::from_values(None, None, None).expect("defaults");
        assert_eq!(defaults, WindowsJobLimits::default());
        assert!(WindowsJobLimits::from_values(None, None, Some(OsStr::new("257"))).is_err());
    }
}
