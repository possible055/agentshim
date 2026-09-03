use std::{ffi::OsStr, io};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

mod skip_notes;

pub use skip_notes::{
    GITIGNORE_RETRY_HINT, SkipNotes, SkipReason, UNDECODABLE_RETRY_HINT, search_tail,
};

/// The continuation vocabulary, named verbatim in the tool descriptions.
///
/// A description tells the caller to copy these back from the response, so a renderer
/// that stops emitting one turns the description into a lie the caller cannot detect.
/// Both ends read these constants and `tests/contract_snapshots.rs` asserts the
/// descriptions still contain them, so renaming a field here fails the build until the
/// description follows.
pub const PARTIAL_MARKER: &str = "Partial:";
pub const NEXT_START_LINE_FIELD: &str = "next_start_line";
pub const NEXT_OFFSET_FIELD: &str = "next_offset";
pub const PDF_CURSOR_FIELD: &str = "pdf_cursor";
pub const OFFICE_CURSOR_FIELD: &str = "office_cursor";

pub const CALL_OUTPUT_TOKEN_LIMIT: usize = 8_192;
pub const MODEL_BYTE_LIMIT: usize = 32_000;
pub const MIN_OUTPUT_BYTES: usize = 4_096;
pub const MAX_OUTPUT_BYTES: usize = 262_144;
pub const OUTPUT_BYTES_ENV: &str = "AGENTSHIM_OUTPUT_BYTES";
const DIAGNOSTIC_TRUNCATION_MARKER: &str = "\n...[diagnostic truncated]";

/// Codex CLI's default `tool_output_token_limit`. Output larger than this is discarded by the
/// client, so the byte budget is derived from it rather than from a fixed byte count.
const TARGET_TOKENS: f64 = 10_000.0;
const ENGLISH_BYTES_PER_TOKEN: f64 = 5.17;
const CJK_BYTES_PER_TOKEN: f64 = 2.17;

/// Parse the configured output ceiling from a raw environment value.
///
/// # Errors
///
/// Returns invalid input when the value is not an integer inside the documented
/// range, so startup fails before any tool call renders output.
pub fn parse_configured_byte_limit(value: Option<&OsStr>) -> io::Result<usize> {
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

pub fn tool_result_encoded_len(text: &str, structured: Option<&Value>, is_error: bool) -> usize {
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

pub fn json_string_content_encoded_len(text: &str) -> usize {
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

pub fn tool_error_structure(
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputLimits {
    pub bytes: usize,
}

impl OutputLimits {
    /// Narrow the byte budget for content the downstream client would tokenize densely.
    /// CJK text costs roughly 2.17 bytes per token against 5.17 for English, so a byte
    /// budget that is fine for English produces output the client silently truncates.
    #[must_use]
    pub fn for_content_within(text: &str, ceiling: usize) -> Self {
        Self::for_content_parts_within(std::iter::once(text), ceiling)
    }

    #[must_use]
    pub fn for_content_parts_within<'a>(
        parts: impl IntoIterator<Item = &'a str>,
        ceiling: usize,
    ) -> Self {
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
            bytes: ceiling.min(aware),
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
pub fn bounded_diagnostic_within(text: &str, ceiling: usize) -> String {
    let limits = OutputLimits { bytes: ceiling };
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectedTokenCost {
    pub tokens: usize,
    pub exact: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionDecision {
    Fits(ProjectedTokenCost),
    Exceeded,
    Cancelled,
}

/// Optional per-call token projection supplied by a token-gating host.
pub trait TokenGate: Send + Sync {
    /// Token allowance for this call.
    fn ceiling(&self) -> usize;

    /// Project the token cost of a tool response against the call allowance.
    fn project_tool_output(
        &self,
        text: &str,
        image_count: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision;

    /// Project the token cost of a fully assembled error or structured result.
    fn project_result(
        &self,
        text: &str,
        structured: Option<&Value>,
        is_error: bool,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision;
}

/// Per-call byte budget supplied by the embedding host.
///
/// The engine treats the host-provided budget as opaque.
/// Implementations must be deterministic for the lifetime of one call.
pub trait CallBudget: Send + Sync {
    /// Hard byte ceiling for one tool response's model-visible text.
    fn page_bytes(&self) -> usize;

    /// Byte ceiling for the fully encoded tool result as it leaves the host.
    fn wire_bytes(&self) -> usize;

    /// Token projection when this host enforces a token allowance.
    fn token_gate(&self) -> Option<&dyn TokenGate>;
}

/// Deterministic byte-derived budget for core tests and benches. Projected tokens use
/// a stable byte upper bound:
/// one token per JSON-escaped payload byte plus the wrapper and image model costs.
#[cfg(any(test, feature = "bench-internals"))]
#[derive(Clone, Copy, Debug)]
pub struct TestCallBudget {
    pub page_bytes: usize,
    pub wire_bytes: usize,
    pub ceiling: usize,
}

#[cfg(any(test, feature = "bench-internals"))]
impl Default for TestCallBudget {
    fn default() -> Self {
        Self {
            page_bytes: MODEL_BYTE_LIMIT,
            wire_bytes: MODEL_BYTE_LIMIT,
            ceiling: CALL_OUTPUT_TOKEN_LIMIT,
        }
    }
}

#[cfg(any(test, feature = "bench-internals"))]
impl TestCallBudget {
    pub fn projected_tokens(text: &str, image_count: usize) -> usize {
        const TEXT_BLOCK: usize = b"[{\"type\":\"text\",\"text\":\"\"}]".len();
        const IMAGE_BLOCK: usize = b",{\"type\":\"image\",\"data\":\"\"}".len();
        const IMAGE_MODEL_TOKENS: usize = 1_844;
        const IMAGE_ITEM_TOKEN_RESERVE: usize = 32;
        const CLIENT_WRAPPER_TOKEN_RESERVE: usize = 128;
        let fixed = CLIENT_WRAPPER_TOKEN_RESERVE.saturating_add(
            image_count.saturating_mul(IMAGE_MODEL_TOKENS + IMAGE_ITEM_TOKEN_RESERVE),
        );
        let payload = TEXT_BLOCK
            .saturating_add(json_string_content_encoded_len(text))
            .saturating_add(image_count.saturating_mul(IMAGE_BLOCK));
        // The real gate tokenizes exactly and its density ranges from roughly 1.8
        // bytes per token (newline-dense text) to 7 (long single-character runs).
        // Two bytes per token sits inside that band and keeps the burst ceilings the
        // engine tests exercise on the same side of the boundary as the real gate.
        fixed.saturating_add(payload.div_ceil(2))
    }
}

#[cfg(any(test, feature = "bench-internals"))]
impl TokenGate for TestCallBudget {
    fn ceiling(&self) -> usize {
        self.ceiling
    }

    fn project_tool_output(
        &self,
        text: &str,
        image_count: usize,
        _cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        let tokens = Self::projected_tokens(text, image_count);
        if tokens <= self.ceiling {
            ProjectionDecision::Fits(ProjectedTokenCost {
                tokens,
                exact: false,
            })
        } else {
            ProjectionDecision::Exceeded
        }
    }

    fn project_result(
        &self,
        text: &str,
        structured: Option<&Value>,
        _is_error: bool,
        _cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        let bytes = text
            .len()
            .saturating_add(structured.map_or(0, serialized_json_len));
        let tokens = bytes.div_ceil(4);
        if tokens <= self.ceiling {
            ProjectionDecision::Fits(ProjectedTokenCost {
                tokens,
                exact: false,
            })
        } else {
            ProjectionDecision::Exceeded
        }
    }
}

#[cfg(any(test, feature = "bench-internals"))]
impl CallBudget for TestCallBudget {
    fn page_bytes(&self) -> usize {
        self.page_bytes
    }

    fn wire_bytes(&self) -> usize {
        self.wire_bytes
    }

    fn token_gate(&self) -> Option<&dyn TokenGate> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DIAGNOSTIC_TRUNCATION_MARKER, OutputError, OutputFormatter, OutputLimits,
        bounded_diagnostic_within, parse_configured_byte_limit, tool_result_encoded_len,
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
                "Partial: next_start_line=42. (output budget)".to_owned(),
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
        assert!(output.ends_with("Partial: next_start_line=42. (output budget)"));
        assert!(output.len() <= limits.bytes);
    }

    #[test]
    fn formatter_cancellation_is_cooperative() {
        let cancellation = CancellationToken::new();
        let mut formatter = OutputFormatter::new(
            "Header",
            vec!["Complete.".to_owned()],
            OutputLimits { bytes: 4_096 },
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
            let bounded = bounded_diagnostic_within(&diagnostic, super::MODEL_BYTE_LIMIT);
            assert!(bounded.ends_with(DIAGNOSTIC_TRUNCATION_MARKER));
            assert!(bounded.len() <= super::MODEL_BYTE_LIMIT);
            assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());
        }
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
        let maximum = 32_000_usize;
        let english = OutputLimits::for_content_within("plain ascii output", maximum).bytes;
        let mixed = OutputLimits::for_content_within("plain ascii 設備網路轉換測試", maximum).bytes;
        let chinese = OutputLimits::for_content_within("設備網路轉換", maximum).bytes;

        assert_eq!(english, maximum);
        assert!(chinese < mixed, "{chinese} must be below {mixed}");
        assert!(mixed <= english);
        assert!(chinese <= maximum);
        assert_eq!(
            OutputLimits::for_content_within("設備網路轉換", maximum).bytes,
            OutputLimits::for_content_parts_within(["設備", "網路", "轉換"], maximum).bytes
        );
    }

    #[test]
    fn configured_byte_limit_rejects_values_outside_the_documented_range() {
        for value in ["0", "4095", "262145", "many", "-1"] {
            assert!(
                parse_configured_byte_limit(Some(std::ffi::OsStr::new(value))).is_err(),
                "{value} must be rejected"
            );
        }
        assert_eq!(
            parse_configured_byte_limit(Some(std::ffi::OsStr::new("48000"))).ok(),
            Some(48_000)
        );
        assert_eq!(
            parse_configured_byte_limit(None).ok(),
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
