use std::{env, sync::Arc};

use rmcp::model::{CallToolResult, ContentBlock};
use tokio_util::sync::CancellationToken;

mod burst_gate;
mod token_gate;

pub(crate) use burst_gate::{
    BurstOutputGate, BurstTicket, MAX_CONTROL_RESPONSE_TOKENS, configured_burst_tokens,
};
pub(crate) use token_gate::{GateDecision, OutputTokenGate, structured_result_fits_model_budget};

// Re-exported to keep the crate-internal `crate::output::` paths stable across test and
// non-test builds; not every item is referenced in both.
#[allow(unused_imports)]
pub use agentshim_core::output::{
    CALL_OUTPUT_TOKEN_LIMIT, MAX_OUTPUT_BYTES, MIN_OUTPUT_BYTES, MODEL_BYTE_LIMIT,
    NEXT_OFFSET_FIELD, NEXT_START_LINE_FIELD, OUTPUT_BYTES_ENV, OutputError, OutputFormatter,
    OutputLimits, PARTIAL_MARKER, PDF_CURSOR_FIELD, ProjectedTokenCost, ProjectionDecision,
    SkipNotes, SkipReason, json_string_content_encoded_len, tool_error_structure,
    tool_result_encoded_len,
};

#[derive(Clone)]
pub(crate) struct CallOutputBudget {
    output_bytes: usize,
    model: ModelOutputBudget,
}

#[derive(Clone)]
struct ModelOutputBudget {
    token_gate: Arc<OutputTokenGate>,
    ticket: BurstTicket,
}

impl CallOutputBudget {
    pub(crate) fn new(
        output_bytes: usize,
        token_gate: Arc<OutputTokenGate>,
        ticket: BurstTicket,
    ) -> Self {
        Self {
            output_bytes,
            model: ModelOutputBudget { token_gate, ticket },
        }
    }

    pub(crate) fn standalone(output_bytes: usize) -> Self {
        let token_gate = OutputTokenGate::load_shared().expect("embedded tokenizer ranks");
        let gate = BurstOutputGate::new(CALL_OUTPUT_TOKEN_LIMIT);
        Self::new(output_bytes, token_gate, gate.begin_call())
    }

    pub(crate) fn ceiling(&self) -> usize {
        self.model.ticket.allowance().min(CALL_OUTPUT_TOKEN_LIMIT)
    }

    /// Project one fully assembled MCP result; the neutral engine budget sees the same
    /// numbers through the [`agentshim_core::output::CallBudget`] implementation.
    pub(crate) fn project_call_result(
        &self,
        result: &CallToolResult,
        ceiling: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        self.model
            .token_gate
            .project_result(result, ceiling, cancellation)
    }

    pub(crate) fn project_tool_output(
        &self,
        text: &str,
        image_count: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        self.model
            .token_gate
            .project_tool_output(text, image_count, self.ceiling(), cancellation)
    }

    pub(crate) fn cache_response_cost(&self, cost: ProjectedTokenCost) {
        self.model.ticket.cache_response_cost(cost.tokens);
    }

    pub(crate) fn cached_response_cost(&self) -> Option<usize> {
        self.model.ticket.cached_response_cost()
    }

    pub(crate) fn finish(&self, actual_tokens: usize, limited: bool) {
        self.model.ticket.finish(actual_tokens, limited);
    }

    pub(crate) fn bounded_diagnostic(&self, text: &str) -> String {
        agentshim_core::output::bounded_diagnostic_within(text, self.output_bytes)
    }

    pub(crate) fn tool_result_fits(
        &self,
        text: &str,
        structured: Option<&serde_json::Value>,
        is_error: bool,
    ) -> bool {
        tool_result_encoded_len(text, structured, is_error) <= self.output_bytes
    }
}

impl agentshim_core::output::CallBudget for CallOutputBudget {
    fn page_bytes(&self) -> usize {
        self.output_bytes
    }

    fn wire_bytes(&self) -> usize {
        self.output_bytes
    }

    fn token_gate(&self) -> Option<&dyn agentshim_core::output::TokenGate> {
        Some(self)
    }
}

impl agentshim_core::output::TokenGate for CallOutputBudget {
    fn ceiling(&self) -> usize {
        Self::ceiling(self)
    }

    fn project_tool_output(
        &self,
        text: &str,
        image_count: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        Self::project_tool_output(self, text, image_count, cancellation)
    }

    fn project_result(
        &self,
        text: &str,
        structured: Option<&serde_json::Value>,
        is_error: bool,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        let mut result = if is_error {
            CallToolResult::error(vec![ContentBlock::text(text)])
        } else {
            CallToolResult::success(vec![ContentBlock::text(text)])
        };
        result.structured_content = structured.cloned();
        self.model
            .token_gate
            .project_result(&result, self.ceiling(), cancellation)
    }
}

/// Resolve the configured output ceiling from the current environment.
///
/// # Errors
///
/// Returns invalid input when `AGENTSHIM_OUTPUT_BYTES` is not an integer inside the
/// documented range, so startup fails before any tool call renders output.
pub fn configured_byte_limit() -> std::io::Result<usize> {
    agentshim_core::output::parse_configured_byte_limit(env::var_os(OUTPUT_BYTES_ENV).as_deref())
}

#[must_use]
pub fn bounded_diagnostic(text: &str) -> String {
    let output_bytes = configured_byte_limit().unwrap_or(MODEL_BYTE_LIMIT);
    agentshim_core::output::bounded_diagnostic_within(text, output_bytes)
}
