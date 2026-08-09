use std::ops::Deref;

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
}

impl ToolOutput {
    pub(crate) fn new(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
            child_nonzero: false,
        }
    }

    pub(crate) fn with_child_nonzero(text: String, child_nonzero: bool) -> Self {
        Self {
            text,
            images: Vec::new(),
            child_nonzero,
        }
    }

    pub(crate) fn with_images(text: String, images: Vec<ToolImage>) -> Self {
        Self {
            text,
            images,
            child_nonzero: false,
        }
    }

    pub(crate) fn encoded_len(&self) -> usize {
        crate::output::tool_result_encoded_len(&self.text, None, false)
    }

    pub(crate) fn fits_budget(&self) -> bool {
        self.encoded_len() <= crate::output::effective_byte_limit()
    }

    /// Tighter than [`Self::fits_budget`] for CJK-dense output, which the downstream client
    /// tokenizes at roughly half the bytes per token that English costs.
    pub(crate) fn fits_content_budget(&self) -> bool {
        self.encoded_len() <= crate::output::OutputLimits::for_content(&self.text).bytes
    }
}

impl Deref for ToolOutput {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}
