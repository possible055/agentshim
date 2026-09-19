//! Render helpers shared by the `bash` and `run_program` foreground tools:
//! timeout projection types, output-budget checks, and wrapped-argv application.

use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::{
    ProcessError, ProcessStreamSummary, ProcessTimeoutDetails,
    capture::{Capture, RenderedCapture},
    resolve::{ResolvedProgram, launcher_for},
};

pub(crate) struct TimeoutRender {
    pub(crate) text: String,
    pub(crate) details: ProcessTimeoutDetails,
}

impl std::ops::Deref for TimeoutRender {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

/// Remaining time before the prepared launch's deadline, rejecting a deadline that
/// has already passed so no child is ever spawned into an expired budget.
pub(crate) fn deadline_remaining(
    deadline: Instant,
    timeout_ms: u64,
) -> Result<Duration, ProcessError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(ProcessError::TimeoutBeforeSpawn { timeout_ms })
}

pub(crate) fn expect_one(captures: Vec<Capture>) -> Capture {
    let count = captures.len();
    captures
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("a merged topology always yields one capture, got {count}"))
}

pub(crate) fn expect_two(captures: Vec<Capture>) -> [Capture; 2] {
    let count = captures.len();
    captures
        .try_into()
        .unwrap_or_else(|_| panic!("a separate topology always yields two captures, got {count}"))
}

/// Apply a sandbox-wrapped argv to a resolved launch. The wrapped executable goes
/// through `launcher_for` exactly here, so both tools classify a wrapped command
/// with the same launcher rules.
pub(crate) fn apply_wrapped_argv(
    resolved: ResolvedProgram,
    args: Vec<String>,
    wrapped_argv: Option<&[String]>,
) -> Result<(ResolvedProgram, Vec<String>), ProcessError> {
    let Some(wrapped) = wrapped_argv else {
        return Ok((resolved, args));
    };
    let command = wrapped.first().ok_or_else(|| {
        ProcessError::Validation("wrapped argv must contain at least the executable".to_owned())
    })?;
    let wrapped_resolved = ResolvedProgram {
        absolute: std::path::PathBuf::from(command),
        executable: std::path::PathBuf::from(command),
        launcher: launcher_for(std::path::Path::new(command))?,
    };
    let remaining_args = wrapped[1.min(wrapped.len())..].to_vec();
    Ok((wrapped_resolved, remaining_args))
}

pub(crate) fn stream_summary(
    bytes_read: usize,
    rendered: &RenderedCapture,
) -> ProcessStreamSummary {
    ProcessStreamSummary {
        total: bytes_read,
        shown: rendered.shown_bytes,
        omitted: rendered.omitted_bytes,
        invalid_utf8: rendered.invalid_bytes,
        encoding: rendered.encoding.clone(),
    }
}

pub(crate) fn normalize_burst_render_error(
    error: ProcessError,
    output_budget: &dyn crate::output::CallBudget,
) -> ProcessError {
    if output_budget
        .token_gate()
        .is_some_and(|token_gate| token_gate.ceiling() < crate::output::CALL_OUTPUT_TOKEN_LIMIT)
        && matches!(
            error,
            ProcessError::Output(crate::output::OutputError::RequiredContentTooLarge)
        )
    {
        ProcessError::Output(crate::output::OutputError::BurstLimit)
    } else {
        error
    }
}

pub(crate) fn timeout_render_fits_budget(
    render: &TimeoutRender,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> bool {
    serde_json::to_value(&render.details)
        .ok()
        .is_some_and(|details| {
            let structured = crate::output::tool_error_structure(
                "resource_timeout",
                true,
                &render.text,
                Some(&details),
            );
            crate::output::tool_result_encoded_len(&render.text, Some(&structured), true)
                <= crate::output::OutputLimits::for_content_within(
                    &render.text,
                    output_budget.page_bytes(),
                )
                .bytes
                && output_budget.token_gate().is_none_or(|token_gate| {
                    matches!(
                        token_gate.project_result(
                            &render.text,
                            Some(&structured),
                            true,
                            cancellation
                        ),
                        crate::output::ProjectionDecision::Fits(_)
                    )
                })
        })
}
