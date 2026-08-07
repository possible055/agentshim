use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub const MODEL_BYTE_LIMIT: usize = 32_000;
const DIAGNOSTIC_TRUNCATION_MARKER: &str = "\n...[diagnostic truncated]";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteToolResult<'a> {
    result_type: &'static str,
    content: [TextContent<'a>; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    structured_content: Option<&'a Value>,
    is_error: bool,
}

#[derive(Serialize)]
struct TextContent<'a> {
    r#type: &'static str,
    text: &'a str,
}

pub(crate) fn tool_result_encoded_len(
    text: &str,
    structured: Option<&Value>,
    is_error: bool,
) -> usize {
    let result = CompleteToolResult {
        result_type: "complete",
        content: [TextContent {
            r#type: "text",
            text,
        }],
        structured_content: structured,
        is_error,
    };
    serde_json::to_vec(&result).map_or(usize::MAX, |encoded| encoded.len())
}

pub(crate) fn tool_result_fits_budget(
    text: &str,
    structured: Option<&Value>,
    is_error: bool,
) -> bool {
    tool_result_encoded_len(text, structured, is_error) <= MODEL_BYTE_LIMIT
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputLimits {
    pub bytes: usize,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            bytes: MODEL_BYTE_LIMIT,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OutputFormatter {
    output: String,
    required_tail: Vec<String>,
    bytes: usize,
    limits: OutputLimits,
}

impl OutputFormatter {
    /// Create a formatter after reserving the header and every required tail line.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError::RequiredContentTooLarge`] when mandatory content alone
    /// exceeds the hard byte limit.
    pub fn new(
        header: impl Into<String>,
        required_tail: Vec<String>,
        limits: OutputLimits,
    ) -> Result<Self, OutputError> {
        let output = header.into();
        let bytes = output.len().saturating_add(
            required_tail
                .iter()
                .map(|line| line.len().saturating_add(1))
                .sum::<usize>(),
        );
        if bytes > limits.bytes {
            return Err(OutputError::RequiredContentTooLarge);
        }
        Ok(Self {
            output,
            required_tail,
            bytes,
            limits,
        })
    }

    /// Append one model-visible body line when all required content still fits.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError::Cancelled`] when cancellation has been requested.
    pub fn try_push_line(
        &mut self,
        line: impl AsRef<str>,
        cancellation: &CancellationToken,
    ) -> Result<bool, OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        let line = line.as_ref();
        let separator = !self.output.is_empty();
        let bytes = line.len().saturating_add(usize::from(separator));
        if self.bytes.saturating_add(bytes) > self.limits.bytes {
            return Ok(false);
        }
        self.bytes += bytes;
        if separator {
            self.output.push('\n');
        }
        self.output.push_str(line);
        Ok(true)
    }

    /// Assemble and independently verify the complete model-visible result.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError::Cancelled`] on cancellation or
    /// [`OutputError::InvariantViolation`] if the final output exceeds a hard limit.
    pub fn finish(mut self, cancellation: &CancellationToken) -> Result<String, OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        for line in self.required_tail {
            self.output.push('\n');
            self.output.push_str(&line);
        }
        if self.output.len() > self.limits.bytes {
            return Err(OutputError::InvariantViolation);
        }
        Ok(self.output)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OutputError {
    #[error("required output content exceeds its hard limit")]
    RequiredContentTooLarge,
    #[error("output rendering cancelled")]
    Cancelled,
    #[error("output formatter limit invariant failed")]
    InvariantViolation,
    #[error("output metadata leaves no room for a result entry")]
    NoProgress,
}

#[must_use]
pub fn bounded_diagnostic(text: &str) -> String {
    let limits = OutputLimits::default();
    if fits(text, limits) {
        return text.to_owned();
    }

    let marker = DIAGNOSTIC_TRUNCATION_MARKER;
    if !fits(marker, limits) {
        return String::new();
    }
    let end = floor_char_boundary(
        text,
        text.len().min(limits.bytes.saturating_sub(marker.len())),
    );
    format!("{}{marker}", &text[..end])
}

fn fits(text: &str, limits: OutputLimits) -> bool {
    text.len() <= limits.bytes
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::{
        DIAGNOSTIC_TRUNCATION_MARKER, OutputError, OutputFormatter, OutputLimits,
        bounded_diagnostic, tool_result_encoded_len,
    };
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn formatter_preserves_required_tail_at_the_byte_limit() {
        let limits = OutputLimits { bytes: 180 };
        let cancellation = CancellationToken::new();
        let mut formatter = OutputFormatter::new(
            "Path: /repo/file.rs",
            vec![
                "Skipped: 2 inaccessible entries.".to_owned(),
                "Partial: continue with {\"start_line\":42}.".to_owned(),
            ],
            limits,
        )
        .expect("create formatter");
        for index in 0..100 {
            if !formatter
                .try_push_line(format!("{index}\tlet value = 界;"), &cancellation)
                .expect("render line")
            {
                break;
            }
        }
        let output = formatter.finish(&cancellation).expect("finish output");
        assert!(output.contains("Skipped: 2 inaccessible entries."));
        assert!(output.ends_with("Partial: continue with {\"start_line\":42}."));
        assert!(output.len() <= limits.bytes);
    }

    #[test]
    fn formatter_cancellation_is_cooperative() {
        let cancellation = CancellationToken::new();
        let mut formatter = OutputFormatter::new(
            "Header",
            vec!["Complete.".to_owned()],
            OutputLimits::default(),
        )
        .expect("create formatter");
        cancellation.cancel();
        assert_eq!(
            formatter.try_push_line("body", &cancellation).unwrap_err(),
            OutputError::Cancelled
        );
    }

    #[test]
    fn diagnostics_are_bounded_by_bytes_and_remain_valid_utf8() {
        for diagnostic in [
            "界".repeat(40_000),
            "abcdefghijklmnopqrstuvwxyz".repeat(10_000),
            "fn parse<T: DeserializeOwned>(input: &[u8]) -> Result<T, Error> { todo!() }\n"
                .repeat(2_000),
            r#"{"path":"src/main.rs","result":"matched"}"#.repeat(2_000),
        ] {
            let bounded = bounded_diagnostic(&diagnostic);
            assert!(bounded.ends_with(DIAGNOSTIC_TRUNCATION_MARKER));
            assert!(bounded.len() <= super::MODEL_BYTE_LIMIT);
            assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());
        }
    }

    #[test]
    fn short_diagnostics_are_unchanged() {
        assert_eq!(bounded_diagnostic("short diagnostic"), "short diagnostic");
    }

    #[test]
    fn tool_result_budget_counts_json_escaping_and_complete_result_fields() {
        let text = "\\\"\u{1}".repeat(1_000);
        let structured = json!({ "text": text });
        let encoded = tool_result_encoded_len(&text, Some(&structured), false);
        assert!(encoded > text.len() + serde_json::to_vec(&structured).unwrap().len());
        assert_eq!(
            encoded,
            serde_json::to_vec(&json!({
                "resultType": "complete",
                "content": [{ "type": "text", "text": text }],
                "structuredContent": structured,
                "isError": false
            }))
            .unwrap()
            .len()
        );
    }
}
