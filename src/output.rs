use tokio_util::sync::CancellationToken;

pub const MODEL_TOKEN_LIMIT: usize = 8_000;
pub const MODEL_BYTE_LIMIT: usize = 32_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputLimits {
    pub tokens: usize,
    pub bytes: usize,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            tokens: MODEL_TOKEN_LIMIT,
            bytes: MODEL_BYTE_LIMIT,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OutputFormatter {
    header: String,
    body: Vec<String>,
    required_tail: Vec<String>,
    conservative_tokens: usize,
    bytes: usize,
    limits: OutputLimits,
}

impl OutputFormatter {
    /// Create a formatter after reserving the header and every required tail line.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError::RequiredContentTooLarge`] when mandatory content alone
    /// exceeds either hard limit.
    pub fn new(
        header: impl Into<String>,
        required_tail: Vec<String>,
        limits: OutputLimits,
    ) -> Result<Self, OutputError> {
        let header = header.into();
        let required = assemble(&header, &[], &required_tail);
        let conservative_tokens = independent_segment_tokens(&header, &required_tail);
        let bytes = required.len();
        if bytes > limits.bytes || conservative_tokens > limits.tokens {
            return Err(OutputError::RequiredContentTooLarge);
        }
        Ok(Self {
            header,
            body: Vec::new(),
            required_tail,
            conservative_tokens,
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
        line: impl Into<String>,
        cancellation: &CancellationToken,
    ) -> Result<bool, OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        let line = line.into();
        let segment = format!("\n{line}");
        let tokens = token_count(&segment);
        let bytes = segment.len();
        if self.bytes.saturating_add(bytes) > self.limits.bytes
            || self.conservative_tokens.saturating_add(tokens) > self.limits.tokens
        {
            return Ok(false);
        }
        self.conservative_tokens += tokens;
        self.bytes += bytes;
        self.body.push(line);
        Ok(true)
    }

    /// Assemble and independently verify the complete model-visible result.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError::Cancelled`] on cancellation or
    /// [`OutputError::InvariantViolation`] if the final output exceeds a hard limit.
    pub fn finish(self, cancellation: &CancellationToken) -> Result<String, OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        let output = assemble(&self.header, &self.body, &self.required_tail);
        if output.len() > self.limits.bytes || token_count(&output) > self.limits.tokens {
            return Err(OutputError::InvariantViolation);
        }
        Ok(output)
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
}

#[must_use]
pub fn token_count(text: &str) -> usize {
    bpe_openai::o200k_base().count(text)
}

fn independent_segment_tokens(header: &str, tail: &[String]) -> usize {
    let mut tokens = token_count(header);
    for line in tail {
        tokens = tokens.saturating_add(token_count(&format!("\n{line}")));
    }
    tokens
}

fn assemble(header: &str, body: &[String], tail: &[String]) -> String {
    let capacity = header.len()
        + body.iter().map(|line| line.len() + 1).sum::<usize>()
        + tail.iter().map(|line| line.len() + 1).sum::<usize>();
    let mut output = String::with_capacity(capacity);
    output.push_str(header);
    for line in body.iter().chain(tail) {
        output.push('\n');
        output.push_str(line);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{OutputError, OutputFormatter, OutputLimits, token_count};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn formatter_preserves_required_tail_at_both_limits() {
        let limits = OutputLimits {
            tokens: 80,
            bytes: 180,
        };
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
        assert!(token_count(&output) <= limits.tokens);
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
}
