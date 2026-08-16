use agentshim_core::output::{CallBudget, ProjectedTokenCost, ProjectionDecision};

/// Native byte page limits with no model-token gate.
pub(crate) struct NativeCallBudget {
    page: usize,
    wire: usize,
}

impl NativeCallBudget {
    pub(crate) fn new(page: usize) -> Self {
        Self {
            page: page.max(MIN_PAGE_BYTES),
            wire: NATIVE_WIRE_BYTE_LIMIT,
        }
    }
}

const MIN_PAGE_BYTES: usize = 4_096;
const NATIVE_WIRE_BYTE_LIMIT: usize = 1024 * 1024;
pub(crate) const DEFAULT_PAGE_BUDGET_BYTES: usize = 50_000;

impl CallBudget for NativeCallBudget {
    fn page_bytes(&self) -> usize {
        self.page
    }

    fn wire_bytes(&self) -> usize {
        self.wire
    }

    fn ceiling(&self) -> usize {
        usize::MAX
    }

    fn project_tool_output(
        &self,
        _text: &str,
        _image_count: usize,
        _cancellation: &tokio_util::sync::CancellationToken,
    ) -> ProjectionDecision {
        ProjectionDecision::Fits(ProjectedTokenCost {
            tokens: 0,
            exact: true,
        })
    }

    fn project_result(
        &self,
        _text: &str,
        _structured: Option<&serde_json::Value>,
        _is_error: bool,
        _cancellation: &tokio_util::sync::CancellationToken,
    ) -> ProjectionDecision {
        ProjectionDecision::Fits(ProjectedTokenCost {
            tokens: 0,
            exact: true,
        })
    }
}

pub(crate) fn default_page_budget() -> usize {
    DEFAULT_PAGE_BUDGET_BYTES
}
