use std::io;
use std::ops::Deref;

use serde::Serialize;
use serde_json::Value;

pub(crate) mod glob;
pub(crate) mod grep;
pub(crate) mod process;
pub(crate) mod read;

#[derive(Debug)]
pub(crate) struct ToolOutput {
    pub text: String,
    pub structured: Option<Value>,
    pub child_nonzero: bool,
}

impl ToolOutput {
    pub(crate) fn new(text: String) -> Self {
        Self {
            text,
            structured: None,
            child_nonzero: false,
        }
    }

    pub(crate) fn process<T: Serialize>(
        text: String,
        structured: &T,
        child_nonzero: bool,
    ) -> io::Result<Self> {
        let structured = serde_json::to_value(structured)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Self {
            text,
            structured: Some(structured),
            child_nonzero,
        })
    }

    pub(crate) fn encoded_len(&self) -> usize {
        crate::output::tool_result_encoded_len(&self.text, self.structured.as_ref(), false)
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
