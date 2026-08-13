use std::{
    env,
    ffi::OsStr,
    io,
    sync::{Arc, OnceLock},
};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

mod burst_gate;
mod token_gate;

pub(crate) use burst_gate::{
    BurstOutputGate, BurstTicket, MAX_CONTROL_RESPONSE_TOKENS, configured_burst_tokens,
};

pub(crate) use token_gate::{
    GateDecision, OutputTokenGate, ProjectedTokenCost, ProjectionDecision,
    structured_result_fits_model_budget,
};

pub(crate) const CALL_OUTPUT_TOKEN_LIMIT: usize = 8_192;
pub const MODEL_BYTE_LIMIT: usize = 32_000;
pub const MIN_OUTPUT_BYTES: usize = 4_096;
pub const MAX_OUTPUT_BYTES: usize = 262_144;
pub const OUTPUT_BYTES_ENV: &str = "CODEXSHIM_OUTPUT_BYTES";
const DIAGNOSTIC_TRUNCATION_MARKER: &str = "\n...[diagnostic truncated]";

/// Codex CLI's default `tool_output_token_limit`. Output larger than this is discarded by the
/// client, so the byte budget is derived from it rather than from a fixed byte count.
const TARGET_TOKENS: f64 = 10_000.0;
const ENGLISH_BYTES_PER_TOKEN: f64 = 5.17;
const CJK_BYTES_PER_TOKEN: f64 = 2.17;

#[derive(Clone)]
pub(crate) struct CallOutputBudget {
    token_gate: Arc<OutputTokenGate>,
    ticket: BurstTicket,
}

impl CallOutputBudget {
    pub(crate) fn new(token_gate: Arc<OutputTokenGate>, ticket: BurstTicket) -> Self {
        Self { token_gate, ticket }
    }

    pub(crate) fn standalone() -> Self {
        let token_gate = OutputTokenGate::load_shared().expect("embedded tokenizer ranks");
        let gate = BurstOutputGate::new(CALL_OUTPUT_TOKEN_LIMIT);
        Self::new(token_gate, gate.begin_call())
    }

    pub(crate) fn ceiling(&self) -> usize {
        self.ticket.allowance().min(CALL_OUTPUT_TOKEN_LIMIT)
    }

    pub(crate) fn project_tool_output(
        &self,
        text: &str,
        image_count: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        self.token_gate
            .project_tool_output(text, image_count, self.ceiling(), cancellation)
    }

    pub(crate) fn project_result(
        &self,
        result: &rmcp::model::CallToolResult,
        ceiling: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        self.token_gate
            .project_result(result, ceiling, cancellation)
    }

    pub(crate) fn cache_response_cost(&self, cost: ProjectedTokenCost) {
        self.ticket.cache_response_cost(cost.tokens);
    }

    pub(crate) fn cached_response_cost(&self) -> Option<usize> {
        self.ticket.cached_response_cost()
    }

    pub(crate) fn finish(&self, actual_tokens: usize, limited: bool) {
        self.ticket.finish(actual_tokens, limited);
    }
}

/// Resolve the configured output ceiling once per process.
///
/// # Errors
///
/// Returns invalid input when `CODEXSHIM_OUTPUT_BYTES` is not an integer inside the
/// documented range, so startup fails before any tool call renders output.
pub fn configured_byte_limit() -> io::Result<usize> {
    parse_configured_byte_limit(env::var_os(OUTPUT_BYTES_ENV).as_deref())
}

fn parse_configured_byte_limit(value: Option<&OsStr>) -> io::Result<usize> {
    value.map_or(Ok(MODEL_BYTE_LIMIT), |value| {
        value
            .to_str()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| (MIN_OUTPUT_BYTES..=MAX_OUTPUT_BYTES).contains(value))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{OUTPUT_BYTES_ENV} must be an integer from {MIN_OUTPUT_BYTES} to \
                     {MAX_OUTPUT_BYTES}"
                    ),
                )
            })
    })
}

#[must_use]
pub fn effective_byte_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| configured_byte_limit().unwrap_or(MODEL_BYTE_LIMIT))
}

fn cjk_ratio<'a>(parts: impl IntoIterator<Item = &'a str>) -> f64 {
    let mut total = 0_usize;
    let mut cjk = 0_usize;
    for part in parts {
        for character in part.chars() {
            total += 1;
            if matches!(character, '\u{4E00}'..='\u{9FFF}') {
                cjk += 1;
            }
        }
    }
    if total == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss, reason = "ratio only needs f64 precision")]
    {
        cjk as f64 / total as f64
    }
}

pub(crate) fn tool_result_encoded_len(
    text: &str,
    structured: Option<&Value>,
    is_error: bool,
) -> usize {
    const PREFIX: usize = br#"{"resultType":"complete","content":[{"type":"text","text":""#.len();
    const CONTENT_SUFFIX: usize = br#""}]"#.len();
    const STRUCTURED_PREFIX: usize = br#","structuredContent":"#.len();
    const ERROR_PREFIX: usize = br#","isError":"#.len();

    let structured_len = structured.map_or(0, |structured| {
        STRUCTURED_PREFIX.saturating_add(serialized_json_len(structured))
    });
    PREFIX
        .saturating_add(json_string_content_encoded_len(text))
        .saturating_add(CONTENT_SUFFIX)
        .saturating_add(structured_len)
        .saturating_add(ERROR_PREFIX)
        .saturating_add(if is_error { 4 } else { 5 })
        .saturating_add(1)
}

pub(crate) fn json_string_content_encoded_len(text: &str) -> usize {
    text.bytes().fold(0, |length, byte| {
        let encoded = match byte {
            b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0C' | b'\r' => 2,
            0x00..=0x1F => 6,
            _ => 1,
        };
        length.saturating_add(encoded)
    })
}

fn serialized_json_len(value: &Value) -> usize {
    #[derive(Default)]
    struct LengthWriter(usize);

    impl io::Write for LengthWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(buffer.len());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut writer = LengthWriter::default();
    serde_json::to_writer(&mut writer, value).map_or(usize::MAX, |()| writer.0)
}

pub(crate) fn tool_result_fits_budget(
    text: &str,
    structured: Option<&Value>,
    is_error: bool,
) -> bool {
    tool_result_encoded_len(text, structured, is_error) <= effective_byte_limit()
}

pub(crate) fn tool_error_structure(
    code: &'static str,
    retryable: bool,
    message: &str,
    details: Option<&Value>,
) -> Value {
    serde_json::json!({
        "error": {
            "code": code,
            "message": message,
            "retryable": retryable,
            "details": details,
        }
    })
}

#[cfg(test)]
pub(crate) fn tool_error_result_fits_budget(
    code: &'static str,
    retryable: bool,
    message: &str,
    details: Option<&Value>,
) -> bool {
    let structured = tool_error_structure(code, retryable, message, details);
    tool_result_fits_budget(message, Some(&structured), true)
}

pub(crate) fn tool_error_result_fits_content_budget(
    code: &'static str,
    retryable: bool,
    message: &str,
    details: Option<&Value>,
) -> bool {
    let structured = tool_error_structure(code, retryable, message, details);
    tool_result_encoded_len(message, Some(&structured), true)
        <= OutputLimits::for_content(message).bytes
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputLimits {
    pub bytes: usize,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            bytes: effective_byte_limit(),
        }
    }
}

impl OutputLimits {
    /// Narrow the byte budget for content the downstream client would tokenize densely.
    /// CJK text costs roughly 2.17 bytes per token against 5.17 for English, so a byte
    /// budget that is fine for English produces output the client silently truncates.
    #[must_use]
    pub fn for_content(text: &str) -> Self {
        Self::for_content_parts(std::iter::once(text))
    }

    #[must_use]
    pub fn for_content_parts<'a>(parts: impl IntoIterator<Item = &'a str>) -> Self {
        let ratio = cjk_ratio(parts);
        let bytes_per_token =
            ENGLISH_BYTES_PER_TOKEN - ratio * (ENGLISH_BYTES_PER_TOKEN - CJK_BYTES_PER_TOKEN);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the product is positive and far below usize::MAX"
        )]
        let aware = (TARGET_TOKENS * bytes_per_token) as usize;
        Self {
            bytes: effective_byte_limit().min(aware),
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
            if !self.output.is_empty() {
                self.output.push('\n');
            }
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
    #[error("output does not fit the current burst token allowance")]
    BurstLimit,
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
    fn cjk_ratio_is_script_agnostic_between_simplified_and_traditional() {
        assert!((super::cjk_ratio(["abcdef"]) - 0.0).abs() < f64::EPSILON);
        assert!((super::cjk_ratio(["设备网络转换"]) - 1.0).abs() < f64::EPSILON);
        assert!((super::cjk_ratio(["設備網路轉換"]) - 1.0).abs() < f64::EPSILON);
        assert!((super::cjk_ratio(["abcd", "設備"]) - 1.0 / 3.0).abs() < 1e-9);
        assert!((super::cjk_ratio(std::iter::empty()) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn content_budget_shrinks_with_cjk_density_and_never_exceeds_the_maximum() {
        let maximum = super::effective_byte_limit();
        let english = OutputLimits::for_content("plain ascii output").bytes;
        let mixed = OutputLimits::for_content("plain ascii 設備網路轉換測試").bytes;
        let chinese = OutputLimits::for_content("設備網路轉換").bytes;

        assert_eq!(english, maximum);
        assert!(chinese < mixed, "{chinese} must be below {mixed}");
        assert!(mixed <= english);
        assert!(chinese <= maximum);
        assert_eq!(
            OutputLimits::for_content("設備網路轉換").bytes,
            OutputLimits::for_content_parts(["設備", "網路", "轉換"]).bytes
        );
    }

    #[test]
    fn configured_byte_limit_rejects_values_outside_the_documented_range() {
        for value in ["0", "4095", "262145", "many", "-1"] {
            assert!(
                super::parse_configured_byte_limit(Some(std::ffi::OsStr::new(value))).is_err(),
                "{value} must be rejected"
            );
        }
        assert_eq!(
            super::parse_configured_byte_limit(Some(std::ffi::OsStr::new("48000"))).ok(),
            Some(48_000)
        );
        assert_eq!(
            super::parse_configured_byte_limit(None).ok(),
            Some(super::MODEL_BYTE_LIMIT)
        );
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
