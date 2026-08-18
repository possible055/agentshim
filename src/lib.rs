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
    let root = std::env::current_dir()
        .map_err(|error| error.to_string())
        .and_then(|path| path::RepositoryRoot::open(path).map_err(|error| error.to_string()))?;
    let engine = agentshim_core::ToolEngine::new(
        std::sync::Arc::new(root),
        ReadScope::default(),
        runtime::RuntimeResources::new(runtime::RuntimeConfig::for_host_defaults()),
    );
    engine.bash_runtime().map_err(|error| error.to_string())
}
pub use runtime::{
    DEFAULT_PROCESS_CALLS, MAX_CONFIGURED_PROCESS_CALLS, MAX_READ_ONLY_CALLS,
    RuntimeConfig as RuntimeLimits,
};
pub use server::{AgentShim, AgentShimBuilder, ToolsListCorrelation};

#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod bench_support;
