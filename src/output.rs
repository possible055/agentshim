use std::{
    env,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
};

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
    CALL_OUTPUT_TOKEN_LIMIT, DSH_NATIVE_PREVIEW_BYTES, DSH_WIRE_BYTE_LIMIT, MAX_OUTPUT_BYTES,
    MIN_OUTPUT_BYTES, MODEL_BYTE_LIMIT, NEXT_OFFSET_FIELD, NEXT_START_LINE_FIELD, OUTPUT_BYTES_ENV,
    OutputError, OutputFormatter, OutputLimits, PARTIAL_MARKER, PDF_CURSOR_FIELD,
    ProjectedTokenCost, ProjectionDecision, SkipNotes, SkipReason, json_string_content_encoded_len,
    tool_error_structure, tool_result_encoded_len,
};

#[derive(Clone)]
pub(crate) struct CallOutputBudget {
    model: Option<ModelOutputBudget>,
}

#[derive(Clone)]
struct ModelOutputBudget {
    token_gate: Arc<OutputTokenGate>,
    ticket: BurstTicket,
}

impl CallOutputBudget {
    pub(crate) fn new(token_gate: Arc<OutputTokenGate>, ticket: BurstTicket) -> Self {
        Self {
            model: Some(ModelOutputBudget { token_gate, ticket }),
        }
    }

    pub(crate) const fn dsh() -> Self {
        Self { model: None }
    }

    pub(crate) fn standalone() -> Self {
        let token_gate = OutputTokenGate::load_shared().expect("embedded tokenizer ranks");
        let gate = BurstOutputGate::new(CALL_OUTPUT_TOKEN_LIMIT);
        Self::new(token_gate, gate.begin_call())
    }

    pub(crate) fn ceiling(&self) -> usize {
        self.model.as_ref().map_or(usize::MAX, |model| {
            model.ticket.allowance().min(CALL_OUTPUT_TOKEN_LIMIT)
        })
    }

    /// Project one fully assembled MCP result; the neutral engine budget sees the same
    /// numbers through the [`agentshim_core::output::CallBudget`] implementation.
    pub(crate) fn project_call_result(
        &self,
        result: &CallToolResult,
        ceiling: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        self.model.as_ref().map_or(
            ProjectionDecision::Fits(ProjectedTokenCost {
                tokens: 0,
                exact: true,
            }),
            |model| {
                model
                    .token_gate
                    .project_result(result, ceiling, cancellation)
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn project_tool_output(
        &self,
        text: &str,
        image_count: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        self.model.as_ref().map_or(
            ProjectionDecision::Fits(ProjectedTokenCost {
                tokens: 0,
                exact: true,
            }),
            |model| {
                model.token_gate.project_tool_output(
                    text,
                    image_count,
                    self.ceiling(),
                    cancellation,
                )
            },
        )
    }

    pub(crate) fn cache_response_cost(&self, cost: ProjectedTokenCost) {
        if let Some(model) = &self.model {
            model.ticket.cache_response_cost(cost.tokens);
        }
    }

    pub(crate) fn cached_response_cost(&self) -> Option<usize> {
        self.model
            .as_ref()
            .map_or(Some(0), |model| model.ticket.cached_response_cost())
    }

    pub(crate) fn finish(&self, actual_tokens: usize, limited: bool) {
        if let Some(model) = &self.model {
            model.ticket.finish(actual_tokens, limited);
        }
    }
}

impl agentshim_core::output::CallBudget for CallOutputBudget {
    fn page_bytes(&self) -> usize {
        effective_byte_limit()
    }

    fn wire_bytes(&self) -> usize {
        wire_byte_limit()
    }

    fn ceiling(&self) -> usize {
        Self::ceiling(self)
    }

    fn project_tool_output(
        &self,
        text: &str,
        image_count: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        self.model.as_ref().map_or(
            ProjectionDecision::Fits(ProjectedTokenCost {
                tokens: 0,
                exact: true,
            }),
            |model| {
                model.token_gate.project_tool_output(
                    text,
                    image_count,
                    self.ceiling(),
                    cancellation,
                )
            },
        )
    }

    fn project_result(
        &self,
        text: &str,
        structured: Option<&serde_json::Value>,
        is_error: bool,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        let Some(model) = &self.model else {
            return ProjectionDecision::Fits(ProjectedTokenCost {
                tokens: 0,
                exact: true,
            });
        };
        let mut result = if is_error {
            CallToolResult::error(vec![ContentBlock::text(text)])
        } else {
            CallToolResult::success(vec![ContentBlock::text(text)])
        };
        result.structured_content = structured.cloned();
        model
            .token_gate
            .project_result(&result, self.ceiling(), cancellation)
    }
}

/// Resolve the configured output ceiling once per process.
///
/// # Errors
///
/// Returns invalid input when `AGENTSHIM_OUTPUT_BYTES` is not an integer inside the
/// documented range, so startup fails before any tool call renders output.
pub fn configured_byte_limit() -> std::io::Result<usize> {
    agentshim_core::output::parse_configured_byte_limit(env::var_os(OUTPUT_BYTES_ENV).as_deref())
}

#[must_use]
pub fn effective_byte_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    if output_profile() == crate::ClientProfile::Dsh {
        DSH_NATIVE_PREVIEW_BYTES
    } else {
        *LIMIT.get_or_init(|| configured_byte_limit().unwrap_or(MODEL_BYTE_LIMIT))
    }
}

static OUTPUT_PROFILE: AtomicU8 = AtomicU8::new(0);

pub(crate) fn install_output_profile(profile: crate::ClientProfile) {
    #[cfg(test)]
    let _ = profile;
    #[cfg(not(test))]
    {
        let value = match profile {
            crate::ClientProfile::Codex => 0,
            crate::ClientProfile::Cursor => 1,
            crate::ClientProfile::Dsh => 2,
        };
        OUTPUT_PROFILE.store(value, Ordering::Release);
    }
}

fn output_profile() -> crate::ClientProfile {
    match OUTPUT_PROFILE.load(Ordering::Acquire) {
        1 => crate::ClientProfile::Cursor,
        2 => crate::ClientProfile::Dsh,
        _ => crate::ClientProfile::Codex,
    }
}

pub(crate) fn wire_byte_limit() -> usize {
    if output_profile() == crate::ClientProfile::Dsh {
        DSH_WIRE_BYTE_LIMIT
    } else {
        effective_byte_limit()
    }
}

pub(crate) fn tool_result_fits_budget(
    text: &str,
    structured: Option<&serde_json::Value>,
    is_error: bool,
) -> bool {
    tool_result_encoded_len(text, structured, is_error) <= wire_byte_limit()
}

#[must_use]
pub fn bounded_diagnostic(text: &str) -> String {
    agentshim_core::output::bounded_diagnostic_within(text, effective_byte_limit())
}
