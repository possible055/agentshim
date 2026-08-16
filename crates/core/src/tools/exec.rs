use serde::Serialize;

pub mod capture;
pub mod resolve;
pub mod spawn;

#[cfg(test)]
mod tests;

pub use resolve::ProcessResolver;

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("invalid request: {0}")]
    Validation(String),
    #[error("failed to resolve program: {0}")]
    Resolve(String),
    #[error("failed to launch or communicate with process: {0}")]
    Io(std::io::Error),
    #[error("capture failed: {message}")]
    Capture {
        kind: CaptureFailureKind,
        message: String,
    },
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureFailureKind {
    LimitExceeded,
    Io,
    Protocol,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct CaptureSinkError {
    pub kind: CaptureFailureKind,
    pub message: String,
}

impl From<std::io::Error> for ProcessError {
    fn from(error: std::io::Error) -> Self {
        if let Some(capture) = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<CaptureSinkError>())
        {
            return Self::Capture {
                kind: capture.kind,
                message: capture.message.clone(),
            };
        }
        Self::Io(error)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProcessStreamSummary {
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
pub struct ProcessTimeoutDetails {
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
pub const fn containment_scope() -> &'static str {
    #[cfg(windows)]
    {
        "job"
    }
    #[cfg(not(windows))]
    {
        "process_group"
    }
}
