mod diagnostics;
mod encoding;
mod output;
mod path;
mod platform;
mod runtime;
mod server;
mod sorting;
mod tools;
mod traversal;

pub use diagnostics::{
    DiagnosticsConfig, DiagnosticsGuard, LogMode, LogStatus, PurgeReport, capacity_bytes, purge,
    retention_days, status,
};
pub use output::bounded_diagnostic;
pub use path::ReadScope;

/// Report the probed bash for `codexshim doctor`, so a missing installation surfaces at
/// deployment time instead of mid-task.
///
/// # Errors
///
/// Returns the operator-facing explanation when no usable GNU bash was found.
pub fn bash_report() -> Result<(std::path::PathBuf, String), String> {
    tools::bash::locate::BashLocator::capture()
        .resolve(&tokio_util::sync::CancellationToken::new())
        .map(|runtime| (runtime.executable.clone(), runtime.locale.clone()))
        .map_err(|error| match error {
            tools::bash::locate::LocateError::Cancelled => {
                "bash discovery was cancelled".to_owned()
            }
            tools::bash::locate::LocateError::TimedOut => "bash discovery timed out".to_owned(),
            tools::bash::locate::LocateError::Unavailable(message) => message.to_string(),
        })
}
pub use runtime::{
    DEFAULT_PROCESS_CALLS, MAX_CONFIGURED_PROCESS_CALLS, MAX_READ_ONLY_CALLS,
    RuntimeConfig as RuntimeLimits,
};
pub use server::{CodexShim, CodexShimBuilder};

#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod bench_support;
