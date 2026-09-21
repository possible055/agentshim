use std::{cell::Cell, ops::Deref};

pub mod bash;
pub mod exec;
pub mod filter;
pub mod glob;
pub mod grep;
pub mod read;
pub mod run_program;

#[derive(Debug)]
pub struct ToolImage {
    pub data: String,
    pub mime_type: &'static str,
}

#[derive(Debug)]
pub struct ToolOutput {
    pub text: String,
    pub images: Vec<ToolImage>,
    pub child_nonzero: bool,
    pub structured: Option<serde_json::Value>,
    projected_cost: Cell<Option<crate::output::ProjectedTokenCost>>,
    retained_resources: Option<crate::runtime::resources::OutputLease>,
}

/// Typed view over the stream summaries one foreground process result carries in
/// its structured content, so hosts read fields instead of JSON pointers.
#[derive(Clone, Copy, Debug)]
pub struct ProcessStreamFacts<'a> {
    pub text: &'a str,
    pub total_bytes: u64,
    pub shown_bytes: u64,
    pub omitted_bytes: u64,
}

/// Typed view over the process facts of one foreground result: the exit label and
/// both bounded stream summaries.
#[derive(Clone, Copy, Debug)]
pub struct ProcessFacts<'a> {
    pub exit_code: Option<&'a str>,
    pub stdout: Option<ProcessStreamFacts<'a>>,
    pub stderr: Option<ProcessStreamFacts<'a>>,
}

impl ToolOutput {
    /// The process facts carried in this result's structured content, or `None`
    /// when the result is not a completed foreground process output.
    #[must_use]
    pub fn process_facts(&self) -> Option<ProcessFacts<'_>> {
        fn stream<'a>(
            process: &'a serde_json::Value,
            name: &str,
        ) -> Option<ProcessStreamFacts<'a>> {
            let stream = process.get(name)?;
            Some(ProcessStreamFacts {
                text: stream
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
                total_bytes: stream
                    .get("totalBytes")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                shown_bytes: stream
                    .get("shownBytes")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                omitted_bytes: stream
                    .get("omittedBytes")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            })
        }

        let process = self.structured.as_ref()?.get("process")?;
        Some(ProcessFacts {
            exit_code: process.get("exitCode").and_then(serde_json::Value::as_str),
            stdout: stream(process, "stdout"),
            stderr: stream(process, "stderr"),
        })
    }

    pub fn new(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
            child_nonzero: false,
            structured: None,
            projected_cost: Cell::new(None),
            retained_resources: None,
        }
    }

    pub fn with_child_nonzero(text: String, child_nonzero: bool) -> Self {
        Self {
            text,
            images: Vec::new(),
            child_nonzero,
            structured: None,
            projected_cost: Cell::new(None),
            retained_resources: None,
        }
    }

    pub fn with_images(text: String, images: Vec<ToolImage>) -> Self {
        Self {
            text,
            images,
            child_nonzero: false,
            structured: None,
            projected_cost: Cell::new(None),
            retained_resources: None,
        }
    }

    #[must_use]
    pub fn with_structured(mut self, structured: serde_json::Value) -> Self {
        self.structured = Some(structured);
        self
    }

    pub fn encoded_len(&self) -> usize {
        crate::output::tool_result_encoded_len(&self.text, None, false)
    }

    #[cfg(test)]
    pub fn fits_budget(&self, budget: &dyn crate::output::CallBudget) -> bool {
        self.encoded_len() <= budget.page_bytes()
    }

    /// Tighter than [`Self::fits_budget`] for CJK-dense output, which the downstream client
    /// tokenizes at roughly half the bytes per token that English costs.
    #[cfg(test)]
    pub fn fits_content_budget(&self, budget: &dyn crate::output::CallBudget) -> bool {
        self.encoded_len()
            <= crate::output::OutputLimits::for_content_within(&self.text, budget.page_bytes())
                .bytes
    }

    pub fn fits_call_budget(
        &self,
        budget: &dyn crate::output::CallBudget,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        let Some(token_gate) = budget.token_gate() else {
            return !cancellation.is_cancelled();
        };
        if let Some(cost) = self.projected_cost.get() {
            return cost.tokens <= token_gate.ceiling() && !cancellation.is_cancelled();
        }
        match token_gate.project_tool_output(&self.text, self.images.len(), cancellation) {
            crate::output::ProjectionDecision::Fits(cost) => {
                self.projected_cost.set(Some(cost));
                true
            }
            crate::output::ProjectionDecision::Exceeded
            | crate::output::ProjectionDecision::Cancelled => false,
        }
    }

    pub fn fits_budget_and_call(
        &self,
        budget: &dyn crate::output::CallBudget,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        self.encoded_len() <= budget.wire_bytes() && self.fits_call_budget(budget, cancellation)
    }

    pub fn fits_content_and_call(
        &self,
        budget: &dyn crate::output::CallBudget,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        self.encoded_len()
            <= crate::output::OutputLimits::for_content_within(&self.text, budget.page_bytes())
                .bytes
            && self.fits_call_budget(budget, cancellation)
    }

    pub fn projected_cost(&self) -> Option<crate::output::ProjectedTokenCost> {
        self.projected_cost.get()
    }

    pub(crate) fn retain_resources(
        mut self,
        resources: crate::runtime::resources::OutputLease,
    ) -> Self {
        self.retained_resources = Some(resources);
        self
    }
}

impl Deref for ToolOutput {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}
