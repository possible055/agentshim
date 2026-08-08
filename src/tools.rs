use std::ops::Deref;

pub(crate) mod glob;
pub(crate) mod grep;
pub(crate) mod process;
pub(crate) mod read;

#[derive(Debug)]
pub(crate) struct ToolOutput {
    pub text: String,
    pub child_nonzero: bool,
}

impl ToolOutput {
    pub(crate) fn new(text: String) -> Self {
        Self {
            text,
            child_nonzero: false,
        }
    }

    pub(crate) fn with_child_nonzero(text: String, child_nonzero: bool) -> Self {
        Self {
            text,
            child_nonzero,
        }
    }

    pub(crate) fn encoded_len(&self) -> usize {
        crate::output::tool_result_encoded_len(&self.text, None, false)
    }

    pub(crate) fn fits_budget(&self) -> bool {
        self.encoded_len() <= crate::output::MODEL_BYTE_LIMIT
    }
}

impl Deref for ToolOutput {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}
