use agentshim_core::output::{
    CallBudget, DSH_NATIVE_PREVIEW_BYTES, DSH_WIRE_BYTE_LIMIT, ProjectedTokenCost,
    ProjectionDecision,
};

/// The `DSH` host's output budget: byte page limits with no model token gate, which
/// is exactly the `CallOutputBudget::dsh()` behaviour the MCP bridge exposed.
pub(crate) struct NativeCallBudget {
    page: usize,
    wire: usize,
}

impl NativeCallBudget {
    pub(crate) fn new(page: usize) -> Self {
        Self {
            page: page.max(MIN_PAGE_BYTES),
            wire: DSH_WIRE_BYTE_LIMIT,
        }
    }
}

const MIN_PAGE_BYTES: usize = 4_096;

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
    DSH_NATIVE_PREVIEW_BYTES
}
