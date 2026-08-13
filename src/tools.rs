use std::{cell::Cell, ops::Deref};

pub(crate) mod bash;
pub(crate) mod exec;
pub(crate) mod glob;
pub(crate) mod grep;
pub(crate) mod read;
pub(crate) mod run_program;

#[derive(Debug)]
pub(crate) struct ToolImage {
    pub data: String,
    pub mime_type: &'static str,
}

#[derive(Debug)]
pub(crate) struct ToolOutput {
    pub text: String,
    pub images: Vec<ToolImage>,
    pub child_nonzero: bool,
    projected_cost: Cell<Option<crate::output::ProjectedTokenCost>>,
}

impl ToolOutput {
    pub(crate) fn new(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
            child_nonzero: false,
            projected_cost: Cell::new(None),
        }
    }

    pub(crate) fn with_child_nonzero(text: String, child_nonzero: bool) -> Self {
        Self {
            text,
            images: Vec::new(),
            child_nonzero,
            projected_cost: Cell::new(None),
        }
    }

    pub(crate) fn with_images(text: String, images: Vec<ToolImage>) -> Self {
        Self {
            text,
            images,
            child_nonzero: false,
            projected_cost: Cell::new(None),
        }
    }

    pub(crate) fn encoded_len(&self) -> usize {
        crate::output::tool_result_encoded_len(&self.text, None, false)
    }

    #[cfg(test)]
    pub(crate) fn fits_budget(&self) -> bool {
        self.encoded_len() <= crate::output::effective_byte_limit()
    }

    /// Tighter than [`Self::fits_budget`] for CJK-dense output, which the downstream client
    /// tokenizes at roughly half the bytes per token that English costs.
    #[cfg(test)]
    pub(crate) fn fits_content_budget(&self) -> bool {
        self.encoded_len() <= crate::output::OutputLimits::for_content(&self.text).bytes
    }

    #[cfg(test)]
    pub(crate) fn fits_model_budget(
        &self,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        let Ok(gate) = crate::output::OutputTokenGate::load_shared() else {
            return false;
        };
        matches!(
            gate.evaluate_tool_text(&self.text, !self.images.is_empty(), cancellation),
            crate::output::GateDecision::FitsByBytes | crate::output::GateDecision::FitsExactly(_)
        )
    }

    pub(crate) fn fits_call_budget(
        &self,
        budget: &crate::output::CallOutputBudget,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        if let Some(cost) = self.projected_cost.get() {
            return cost.tokens <= budget.ceiling() && !cancellation.is_cancelled();
        }
        match budget.project_tool_output(&self.text, self.images.len(), cancellation) {
            crate::output::ProjectionDecision::Fits(cost) => {
                self.projected_cost.set(Some(cost));
                true
            }
            crate::output::ProjectionDecision::Exceeded
            | crate::output::ProjectionDecision::Cancelled => false,
        }
    }

    pub(crate) fn fits_budget_and_call(
        &self,
        budget: &crate::output::CallOutputBudget,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        self.encoded_len() <= crate::output::effective_byte_limit()
            && self.fits_call_budget(budget, cancellation)
    }

    pub(crate) fn fits_content_and_call(
        &self,
        budget: &crate::output::CallOutputBudget,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        self.encoded_len() <= crate::output::OutputLimits::for_content(&self.text).bytes
            && self.fits_call_budget(budget, cancellation)
    }

    pub(crate) fn projected_cost(&self) -> Option<crate::output::ProjectedTokenCost> {
        self.projected_cost.get()
    }
}

impl Deref for ToolOutput {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}
