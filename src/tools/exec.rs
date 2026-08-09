use serde::Serialize;

pub(crate) mod capture;
pub(crate) mod resolve;
pub(crate) mod spawn;

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(unix)]
pub(crate) use unix as platform;

#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows as platform;

#[cfg(test)]
mod tests;

pub use resolve::ProcessResolver;

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("invalid request: {0}")]
    Validation(String),
    #[error("failed to resolve program: {0}")]
    Resolve(String),
    #[error("{0}")]
    NotPermitted(String),
    #[error("failed to launch or communicate with process: {0}")]
    Io(#[from] std::io::Error),
    /// An interpreter this server instance settled on at startup is not present. Retrying the
    /// same call in the same instance cannot change the answer.
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    ResourceBusy(String),
    #[error("{report}")]
    Timeout {
        timeout_ms: u64,
        report: String,
        details: Box<ProcessTimeoutDetails>,
    },
    #[error("process timed out after {timeout_ms} ms before spawn; no child was started")]
    TimeoutBeforeSpawn { timeout_ms: u64 },
    #[error("process was cancelled and its owned process containment was terminated")]
    Cancelled,
    #[error("process cleanup did not complete before its deadline; outcome uncertain")]
    OutcomeUncertain,
    #[error(transparent)]
    Output(#[from] crate::output::OutputError),
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProcessStreamSummary {
    #[serde(rename = "total_bytes")]
    pub total: usize,
    #[serde(rename = "shown_bytes")]
    pub shown: usize,
    #[serde(rename = "omitted_bytes")]
    pub omitted: usize,
    #[serde(rename = "invalid_utf8_bytes")]
    pub invalid_utf8: usize,
    pub encoding: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProcessTimeoutDetails {
    pub timeout_ms: u64,
    pub program: String,
    pub cwd: String,
    pub launcher: String,
    pub duration_ms: u64,
    pub stdout: ProcessStreamSummary,
    pub stderr: ProcessStreamSummary,
    pub termination_outcome: &'static str,
    pub containment_scope: &'static str,
}

#[must_use]
pub(crate) const fn containment_scope() -> &'static str {
    #[cfg(windows)]
    {
        "job"
    }
    #[cfg(not(windows))]
    {
        "process_group"
    }
}
