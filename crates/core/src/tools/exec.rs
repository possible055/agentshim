//! Façade over the process vocabulary that lives in `platform::process`. The tool-facing
//! names are re-exported here so request/result paths stay stable for hosts and tests.

pub(crate) mod report;

pub use crate::platform::process::{
    CaptureFailureKind, CaptureSinkError, ProcessError, ProcessStreamSummary,
    ProcessTimeoutDetails, capture, containment_scope, resolve, spawn,
    spawn::{
        CLEANUP_DEADLINE, CaptureSink, DEFAULT_TIMEOUT_MS, default_max_timeout_ms,
        max_timeout_ms_from_shelf,
    },
};

#[cfg(test)]
mod tests;
