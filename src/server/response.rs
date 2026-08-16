use std::{fmt::Display, time::Duration};

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock, JsonObject};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

use crate::output::{
    CallOutputBudget, MAX_CONTROL_RESPONSE_TOKENS, ProjectionDecision, bounded_diagnostic,
};

pub(super) fn finalize_tool_response(
    tool: &str,
    budget: &CallOutputBudget,
    response: Result<CallToolResponse, McpError>,
    cancellation: &CancellationToken,
) -> Result<CallToolResponse, McpError> {
    let Ok(CallToolResponse::Complete(result)) = &response else {
        budget.finish(0, false);
        return response;
    };
    let allowance = budget.ceiling();
    if let Some(tokens) = budget.cached_response_cost() {
        debug_assert!(tokens <= allowance);
        budget.finish(tokens, false);
        return response;
    }
    match budget.project_call_result(result, allowance, cancellation) {
        ProjectionDecision::Fits(cost) => {
            budget.finish(cost.tokens, false);
            response
        }
        ProjectionDecision::Exceeded => {
            let completed_side_effect =
                matches!(tool, "run_program" | "bash") && result.is_error != Some(true);
            let replacement = burst_limit_response(completed_side_effect);
            let CallToolResponse::Complete(replacement_result) = &replacement else {
                unreachable!("burst limit response is complete")
            };
            let cost = match budget.project_call_result(
                replacement_result,
                MAX_CONTROL_RESPONSE_TOKENS,
                &CancellationToken::new(),
            ) {
                ProjectionDecision::Fits(cost) => cost.tokens,
                ProjectionDecision::Exceeded | ProjectionDecision::Cancelled => {
                    unreachable!("fixed burst response always has a measurable cost")
                }
            };
            debug_assert!(cost <= MAX_CONTROL_RESPONSE_TOKENS);
            budget.finish(cost, true);
            Ok(replacement)
        }
        ProjectionDecision::Cancelled => {
            budget.finish(0, true);
            Ok(tool_error(
                "client_cancellation",
                false,
                "tool output verification was cancelled",
                None,
            ))
        }
    }
}

fn burst_limit_response(completed_side_effect: bool) -> CallToolResponse {
    if completed_side_effect {
        return tool_error(
            "output_budget",
            false,
            "tool execution completed but its output exceeded the current burst budget; do not retry",
            Some(&json!({
                "reason": "burst_limit",
                "execution": "completed"
            })),
        );
    }
    tool_error(
        "output_budget",
        true,
        "tool output exceeded the current burst budget; retry after the burst resets",
        Some(&json!({
            "reason": "burst_limit",
            "retry_after_ms": 2000
        })),
    )
}

pub(super) fn parse_request<T: DeserializeOwned>(
    arguments: Option<JsonObject>,
    tool: &str,
) -> Result<T, String> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| format!("invalid {tool} request: {error}"))
}

/// Which admission class a `bash` call belongs to has to be known before the request is parsed,
/// because parsing happens after admission. This reads the one field that decides it, exactly as
/// `BashRequest` does: literal `true`, nothing coerced. A disagreement with the parsed request is
/// still caught later, so this only has to be honest, not authoritative.
pub(super) fn requests_detach(arguments: Option<&JsonObject>) -> bool {
    arguments.is_some_and(|arguments| arguments.get("detach") == Some(&Value::Bool(true)))
}

pub(super) fn tool_error(
    code: &'static str,
    retryable: bool,
    message: impl Into<String>,
    details: Option<&Value>,
) -> CallToolResponse {
    let mut details = details.cloned();
    if let Some(details) = &mut details {
        bound_detail_strings(details);
    }
    let (message, structured) =
        bounded_error_payload(code, retryable, &message.into(), details.as_ref());
    let mut result = CallToolResult::error(vec![ContentBlock::text(message)]);
    result.structured_content = Some(structured);
    result.into()
}

fn bounded_error_payload(
    code: &'static str,
    retryable: bool,
    message: &str,
    details: Option<&Value>,
) -> (String, Value) {
    let bounded = bounded_diagnostic(message);
    let cancellation = CancellationToken::new();
    let structured = crate::output::tool_error_structure(code, retryable, &bounded, details);
    if error_payload_fits(code, retryable, &bounded, details, &cancellation) {
        return (bounded, structured);
    }
    let marker = "...[truncated]";
    let boundaries = bounded
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(bounded.len()))
        .collect::<Vec<_>>();
    let mut low = 0_usize;
    let mut high = boundaries.len();
    let mut best = marker.to_owned();
    while low < high {
        let midpoint = low + (high - low) / 2;
        let end = boundaries[midpoint];
        let candidate = format!("{}{marker}", &bounded[..end]);
        if error_payload_fits(code, retryable, &candidate, details, &cancellation) {
            best = candidate;
            low = midpoint + 1;
        } else {
            high = midpoint;
        }
    }
    let structured = crate::output::tool_error_structure(code, retryable, &best, details);
    if error_payload_fits(code, retryable, &best, details, &cancellation) {
        return (best, structured);
    }
    let structured = crate::output::tool_error_structure(code, retryable, marker, None);
    debug_assert!(crate::output::tool_result_fits_budget(
        marker,
        Some(&structured),
        true
    ));
    (marker.to_owned(), structured)
}

fn error_payload_fits(
    code: &'static str,
    retryable: bool,
    message: &str,
    details: Option<&Value>,
    cancellation: &CancellationToken,
) -> bool {
    let structured = crate::output::tool_error_structure(code, retryable, message, details);
    crate::output::tool_result_fits_budget(message, Some(&structured), true)
        && crate::output::structured_result_fits_model_budget(&structured, cancellation)
}

fn bound_detail_strings(value: &mut Value) {
    const LIMIT: usize = 2_048;
    const MARKER: &str = "...[truncated]";
    match value {
        Value::String(text) if text.len() > LIMIT => {
            let end = floor_char_boundary(text, LIMIT - MARKER.len());
            text.truncate(end);
            text.push_str(MARKER);
        }
        Value::Array(values) => values.iter_mut().for_each(bound_detail_strings),
        Value::Object(values) => values.values_mut().for_each(bound_detail_strings),
        _ => {}
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub(super) trait DiagnosticError: Display {
    fn error_class(&self) -> &'static str;

    fn retryable(&self) -> bool {
        matches!(
            self.error_class(),
            "io" | "resource_timeout" | "resource_busy"
        )
    }

    fn details(&self) -> Option<Value> {
        None
    }
}

impl DiagnosticError for crate::tools::read::ReadError {
    fn error_class(&self) -> &'static str {
        use crate::tools::read::ReadError;
        match self {
            ReadError::Validation(_) => "validation",
            ReadError::Path(_)
            | ReadError::NonUnicodePath
            | ReadError::Directory
            | ReadError::NotRegular => "path",
            ReadError::Cancelled => "client_cancellation",
            ReadError::Output(crate::output::OutputError::BurstLimit) => "output_budget",
            ReadError::Output(_) => "output_invariant",
            ReadError::ResourceLimit { .. } => "resource_limit",
            ReadError::PdfImageRequired { .. } => "pdf_image_required",
            ReadError::PdfProcessing(_) => "pdf_processing",
            ReadError::Pdf(error) => match error.kind() {
                agentshim_pdf_read::PdfReadErrorKind::Invalid => "pdf_invalid",
                agentshim_pdf_read::PdfReadErrorKind::Unsupported => "pdf_unsupported",
                agentshim_pdf_read::PdfReadErrorKind::Encrypted => "pdf_encrypted",
                agentshim_pdf_read::PdfReadErrorKind::ResourceLimit => "resource_limit",
                agentshim_pdf_read::PdfReadErrorKind::Processing => "pdf_processing",
                agentshim_pdf_read::PdfReadErrorKind::Io => "io",
                // The mode runtime ceiling cancels the same token the client uses, so
                // the service decides which of the two it was; a core cancellation on
                // its own is the client's.
                agentshim_pdf_read::PdfReadErrorKind::Cancelled => "client_cancellation",
            },
            ReadError::Io(_) | ReadError::Decode(_) | ReadError::Binary | ReadError::Changed => {
                "io"
            }
        }
    }

    fn retryable(&self) -> bool {
        matches!(
            self,
            crate::tools::read::ReadError::Output(crate::output::OutputError::BurstLimit)
        ) || matches!(
            self.error_class(),
            "io" | "resource_timeout" | "resource_busy"
        )
    }

    fn details(&self) -> Option<Value> {
        use crate::tools::read::ReadError;
        match self {
            // Retrying the same parameters can never succeed, so the error is not
            // retryable; the caller still needs the exact parameters that would work.
            ReadError::PdfImageRequired { pages, cursor } => Some(json!({
                "retry_with": [{
                    "pdf_mode": "image",
                    "pages": pages,
                    "pdf_cursor": cursor
                }]
            })),
            ReadError::ResourceLimit {
                resource,
                limit_bytes,
                observed_bytes,
                ..
            } => {
                let mut details = json!({ "resource": resource });
                let map = details.as_object_mut().expect("object literal");
                if let Some(limit) = limit_bytes {
                    map.insert("limit_bytes".to_owned(), json!(limit));
                }
                if let Some(observed) = observed_bytes {
                    map.insert("observed".to_owned(), json!(observed));
                }
                Some(details)
            }
            // A limit raised inside the parser carries the same shape as one raised by
            // the tool, so the caller does not have to know which layer refused.
            ReadError::Pdf(error) => error.limit().map(|limit| {
                json!({
                    "resource": limit.resource,
                    "limit_bytes": limit.limit_bytes,
                    "observed": limit.observed_bytes
                })
            }),
            ReadError::Output(crate::output::OutputError::BurstLimit) => Some(json!({
                "reason": "burst_limit",
                "retry_after_ms": 2000
            })),
            _ => None,
        }
    }
}

impl DiagnosticError for crate::tools::glob::GlobError {
    fn error_class(&self) -> &'static str {
        use crate::tools::glob::GlobError;
        match self {
            GlobError::Validation(_) | GlobError::Pattern(_) => "validation",
            GlobError::Path(_) => "path",
            GlobError::Output(crate::output::OutputError::BurstLimit) => "output_budget",
            GlobError::Output(_) => "output_invariant",
            GlobError::Memory => "resource_limit",
            GlobError::MemoryBusy => "resource_busy",
            GlobError::Traversal(_) | GlobError::Io(_) => "io",
        }
    }

    fn retryable(&self) -> bool {
        matches!(
            self,
            crate::tools::glob::GlobError::MemoryBusy
                | crate::tools::glob::GlobError::Output(crate::output::OutputError::BurstLimit)
        ) || matches!(
            self.error_class(),
            "io" | "resource_timeout" | "resource_busy"
        )
    }

    fn details(&self) -> Option<Value> {
        matches!(
            self,
            crate::tools::glob::GlobError::Output(crate::output::OutputError::BurstLimit)
        )
        .then(|| json!({ "reason": "burst_limit", "retry_after_ms": 2000 }))
    }
}

impl DiagnosticError for crate::tools::grep::GrepError {
    fn error_class(&self) -> &'static str {
        use crate::tools::grep::GrepError;
        match self {
            GrepError::Validation(_) | GrepError::Regex(_) | GrepError::Glob(_) => "validation",
            GrepError::Path(_) => "path",
            GrepError::Cancelled => "client_cancellation",
            GrepError::Output(crate::output::OutputError::BurstLimit) => "output_budget",
            GrepError::Output(_) => "output_invariant",
            GrepError::CandidateMemory => "resource_limit",
            GrepError::MemoryBusy => "resource_busy",
            GrepError::PoolPoison => "resource_timeout",
            GrepError::Unsearchable(_) | GrepError::Traversal(_) | GrepError::Io(_) => "io",
        }
    }

    /// `io` is retryable by default because most of it is transient. A single-file
    /// skip that cannot change without a different target — binary, undecodable,
    /// heap, capture, escaped, non-unicode — is not: advertising a retry invites
    /// the model to spend its turns re-running a call that will fail the same way.
    fn retryable(&self) -> bool {
        use crate::tools::grep::GrepError;
        match self {
            GrepError::Unsearchable(reason) => reason.retryable(),
            GrepError::MemoryBusy | GrepError::Output(crate::output::OutputError::BurstLimit) => {
                true
            }
            other => matches!(
                other.error_class(),
                "io" | "resource_timeout" | "resource_busy"
            ),
        }
    }

    fn details(&self) -> Option<Value> {
        matches!(
            self,
            crate::tools::grep::GrepError::Output(crate::output::OutputError::BurstLimit)
        )
        .then(|| json!({ "reason": "burst_limit", "retry_after_ms": 2000 }))
    }
}

impl DiagnosticError for crate::tools::exec::ProcessError {
    fn error_class(&self) -> &'static str {
        use crate::tools::exec::ProcessError;
        match self {
            ProcessError::Validation(_) => "validation",
            ProcessError::Resolve(_) => "path",
            ProcessError::ResourceBusy(_) => "resource_busy",
            ProcessError::Io(_) | ProcessError::Unavailable(_) => "io",
            ProcessError::Capture { kind, .. } => match kind {
                crate::tools::exec::CaptureFailureKind::LimitExceeded => "capture_limit_exceeded",
                crate::tools::exec::CaptureFailureKind::Io => "capture_io_failed",
                crate::tools::exec::CaptureFailureKind::Protocol => "capture_protocol",
            },
            ProcessError::Timeout { .. } | ProcessError::TimeoutBeforeSpawn { .. } => {
                "resource_timeout"
            }
            ProcessError::Cancelled => "client_cancellation",
            ProcessError::OutcomeUncertain => "outcome_uncertain",
            ProcessError::Output(crate::output::OutputError::BurstLimit) => "output_budget",
            ProcessError::Output(_) => "output_invariant",
        }
    }

    /// `io` is retryable by default because most of it is transient. A missing interpreter is
    /// not: the answer is fixed for the life of this server instance, and advertising a retry
    /// invites the model to spend its turns re-running a call that cannot start.
    fn retryable(&self) -> bool {
        use crate::tools::exec::ProcessError;
        match self {
            ProcessError::Unavailable(_)
            | ProcessError::Output(crate::output::OutputError::BurstLimit) => false,
            other => matches!(
                other.error_class(),
                "io" | "resource_timeout" | "resource_busy"
            ),
        }
    }

    fn details(&self) -> Option<Value> {
        use crate::tools::exec::ProcessError;
        match self {
            ProcessError::Timeout { details, .. } => serde_json::to_value(details).ok(),
            ProcessError::TimeoutBeforeSpawn { timeout_ms } => Some(json!({
                "timeout_ms": timeout_ms,
                "termination_outcome": "not_started",
                "containment_scope": crate::tools::exec::containment_scope()
            })),
            ProcessError::OutcomeUncertain => Some(json!({
                "termination_outcome": "uncertain",
                "containment_scope": crate::tools::exec::containment_scope()
            })),
            ProcessError::Output(crate::output::OutputError::BurstLimit) => Some(json!({
                "reason": "burst_limit",
                "execution": "completed"
            })),
            _ => None,
        }
    }
}

pub(super) fn classified_tool_error(
    error_class: &'static str,
    message: impl Into<String>,
) -> CallToolResponse {
    tracing::error!(target: "agentshim", event = "tool_error", phase = "response", outcome = "error", error_class);
    let retryable = matches!(error_class, "io" | "resource_timeout" | "resource_busy");
    tool_error(error_class, retryable, message, None)
}

pub(super) fn diagnostic_tool_error<E: DiagnosticError + ?Sized>(error: &E) -> CallToolResponse {
    let error_class = error.error_class();
    let details = error.details();
    tracing::error!(target: "agentshim", event = "tool_error", phase = "response", outcome = "error", error_class);
    tool_error(
        error_class,
        error.retryable(),
        error.to_string(),
        details.as_ref(),
    )
}

#[cfg(test)]
pub(super) fn blocking_response<E: DiagnosticError>(
    tool: &str,
    run_ms: u64,
    result: Result<Result<crate::tools::ToolOutput, E>, tokio::task::JoinError>,
    output_token_gate: &crate::output::OutputTokenGate,
    cancellation: &CancellationToken,
    output_budget: &CallOutputBudget,
) -> CallToolResponse {
    blocking_response_for_profile(
        tool,
        run_ms,
        result,
        Some(output_token_gate),
        cancellation,
        output_budget,
        crate::ClientProfile::Codex,
    )
}

pub(super) fn blocking_response_for_profile<E: DiagnosticError>(
    tool: &str,
    run_ms: u64,
    result: Result<Result<crate::tools::ToolOutput, E>, tokio::task::JoinError>,
    output_token_gate: Option<&crate::output::OutputTokenGate>,
    cancellation: &CancellationToken,
    output_budget: &CallOutputBudget,
    _client_profile: crate::ClientProfile,
) -> CallToolResponse {
    match result {
        Ok(Ok(output)) => {
            let outcome = if output.child_nonzero {
                "child_nonzero"
            } else {
                "success"
            };
            if outcome == "child_nonzero" {
                tracing::warn!(target: "agentshim", event = "tool_complete", phase = "response", outcome, error_class = "child_nonzero", run_ms);
            } else {
                tracing::info!(target: "agentshim", event = "tool_complete", phase = "response", outcome, run_ms);
            }
            let call_budget_verified = output.fits_call_budget(output_budget, cancellation);
            let projected_cost = output.projected_cost();
            let mut content = Vec::with_capacity(output.images.len() + 1);
            content.push(ContentBlock::text(output.text));
            content.extend(
                output
                    .images
                    .into_iter()
                    .map(|image| ContentBlock::image(image.data, image.mime_type)),
            );
            let result = CallToolResult::success(content);
            if call_budget_verified {
                if let Some(cost) = projected_cost {
                    output_budget.cache_response_cost(cost);
                }
                tracing::trace!(target: "agentshim", token_gate_path = "verified_renderer");
                return result.into();
            }
            match output_token_gate
                .expect("non-native response fallback has a token gate")
                .evaluate_result(&result, cancellation)
            {
                crate::output::GateDecision::FitsByBytes
                | crate::output::GateDecision::FitsExactly(_) => result.into(),
                crate::output::GateDecision::Exceeded => tool_error(
                    "output_budget",
                    false,
                    "tool output exceeded the model token budget",
                    None,
                ),
                crate::output::GateDecision::Cancelled => tool_error(
                    "client_cancellation",
                    false,
                    "tool output verification was cancelled",
                    None,
                ),
            }
        }
        Ok(Err(error)) => diagnostic_tool_error(&error),
        Err(error) => {
            classified_tool_error("worker_panic", format!("{tool} worker failed: {error}"))
        }
    }
}

/// The PDF gate and the mode reservation, held together for one call.
///
/// Both are released by dropping this, which is what makes every early return, timeout,
/// cancellation, and error path leak-free without a bespoke cleanup branch each.
pub(super) struct PdfAdmission {
    pub(super) _gate: OwnedSemaphorePermit,
    pub(super) _memory: OwnedSemaphorePermit,
}

/// `resource_busy` without a delay hint invites an immediate retry, which is just a spin.
pub(super) fn pdf_busy(permit: &'static str) -> CallToolResponse {
    let retry_after_ms = u64::try_from(crate::runtime::PDF_GATE_WAIT.as_millis()).unwrap_or(300);
    tracing::warn!(target: "agentshim", event = "tool_error", phase = "admission", outcome = "error", error_class = "resource_busy", permit);
    tool_error(
        "resource_busy",
        true,
        format!("read PDF {permit} capacity is busy; retry the request later"),
        Some(&json!({
            "permit": permit,
            "retry_after_ms": retry_after_ms
        })),
    )
}

pub(super) fn pdf_timeout(limit: Duration, elapsed: Duration) -> CallToolResponse {
    tracing::warn!(target: "agentshim", event = "tool_error", phase = "execution", outcome = "error", error_class = "resource_timeout");
    tool_error(
        "resource_timeout",
        true,
        format!(
            "PDF read exceeded its {} ms mode runtime limit",
            limit.as_millis()
        ),
        Some(&json!({
            "limit_ms": u64::try_from(limit.as_millis()).unwrap_or(u64::MAX),
            "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "work_stopped": true,
            "partial_output": false
        })),
    )
}

pub(super) fn queue_timeout(tool: &str, timeout_ms: u64) -> CallToolResponse {
    classified_tool_error("resource_timeout", queue_timeout_message(tool, timeout_ms))
}

/// Admission runs before the per-call tracing span exists, so the tool and the admission class
/// are logged explicitly here; without them a saturated server cannot be told apart from a
/// saturated read-only pool in diagnostics.
pub(super) fn resource_busy(tool: &str, admission: &'static str) -> CallToolResponse {
    resource_busy_with_message(
        tool,
        admission,
        format!("{tool} {admission} capacity is busy; retry the request later"),
    )
}

pub(super) fn resource_busy_with_message(
    tool: &str,
    admission: &'static str,
    message: impl Into<String>,
) -> CallToolResponse {
    tracing::error!(target: "agentshim", event = "tool_error", phase = "request", outcome = "error", error_class = "resource_busy", tool, admission);
    let retryable = true;
    tool_error("resource_busy", retryable, message, None)
}

pub(super) fn cancellation_class(
    request: &CancellationToken,
    shutdown: &CancellationToken,
) -> &'static str {
    if shutdown.is_cancelled() && !request.is_cancelled() {
        "shutdown"
    } else {
        "client_cancellation"
    }
}

pub(super) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn queue_timeout_message(tool: &str, timeout_ms: u64) -> String {
    format!(
        "{tool} timed out after {timeout_ms} ms while waiting for process capacity; no child was started"
    )
}

pub(super) fn relayed_cancellation(
    request: &CancellationToken,
    shutdown: CancellationToken,
) -> (CancellationToken, tokio::task::JoinHandle<()>) {
    let cancellation = CancellationToken::new();
    let signal = cancellation.clone();
    let request = request.clone();
    let relay = tokio::spawn(async move {
        tokio::select! {
            () = request.cancelled() => {
                tracing::warn!(target: "agentshim", event = "tool_cancelled", phase = "execution", error_class = "client_cancellation");
            }
            () = shutdown.cancelled() => {
                tracing::warn!(target: "agentshim", event = "tool_cancelled", phase = "execution", error_class = "shutdown");
            }
        }
        signal.cancel();
    });
    (cancellation, relay)
}
