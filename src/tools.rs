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
    model_budget_verified: Cell<bool>,
}

impl ToolOutput {
    pub(crate) fn new(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
            child_nonzero: false,
            model_budget_verified: Cell::new(false),
        }
    }

    pub(crate) fn with_child_nonzero(text: String, child_nonzero: bool) -> Self {
        Self {
            text,
            images: Vec::new(),
            child_nonzero,
            model_budget_verified: Cell::new(false),
        }
    }

    pub(crate) fn with_images(text: String, images: Vec<ToolImage>) -> Self {
        Self {
            text,
            images,
            child_nonzero: false,
            model_budget_verified: Cell::new(false),
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

    pub(crate) fn fits_model_budget(
        &self,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        let Ok(gate) = crate::output::OutputTokenGate::load_shared() else {
            return false;
        };
        let fits = matches!(
            gate.evaluate_tool_text(&self.text, !self.images.is_empty(), cancellation),
            crate::output::GateDecision::FitsByBytes | crate::output::GateDecision::FitsExactly(_)
        );
        self.model_budget_verified.set(fits);
        fits
    }

    pub(crate) fn fits_budget_and_model(
        &self,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        let encoded_len = self.encoded_len();
        encoded_len <= crate::output::effective_byte_limit()
            && self.fits_model_after_wire_bound(encoded_len, cancellation)
    }

    pub(crate) fn fits_content_and_model(
        &self,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        let encoded_len = self.encoded_len();
        encoded_len <= crate::output::OutputLimits::for_content(&self.text).bytes
            && self.fits_model_after_wire_bound(encoded_len, cancellation)
    }

    fn fits_model_after_wire_bound(
        &self,
        encoded_len: usize,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        if encoded_len <= crate::output::TOOL_CONTENT_TOKEN_LIMIT {
            let fits = !cancellation.is_cancelled();
            self.model_budget_verified.set(fits);
            return fits;
        }
        self.fits_model_budget(cancellation)
    }

    pub(crate) fn model_budget_verified(&self) -> bool {
        self.model_budget_verified.get()
    }
}

impl Deref for ToolOutput {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}
