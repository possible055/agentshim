mod diagnostics;
mod output;
mod platform;
mod profile;
mod server;

// The MCP shell owns transport, catalog, client profiles, and output gating; every
// compute path below lives in the host-neutral core crate.
pub use agentshim_core::{encoding, path, runtime, sorting, tools, traversal};

pub use diagnostics::{
    DiagnosticsConfig, DiagnosticsGuard, LogMode, LogStatus, PurgeReport, capacity_bytes, purge,
    retention_days, status,
};
pub use output::{
    NEXT_OFFSET_FIELD, NEXT_START_LINE_FIELD, PARTIAL_MARKER, PDF_CURSOR_FIELD, bounded_diagnostic,
};
pub use path::ReadScope;
pub use profile::ClientProfile;

/// Report the probed bash for `agentshim doctor`, so a missing installation surfaces at
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
pub use server::{AgentShim, AgentShimBuilder, ToolsListCorrelation};

#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod bench_support;
