use std::{fmt::Display, io, str::FromStr};

/// Client-specific defaults for aggregate tool-response output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ClientProfile {
    /// Conservative aggregate output for Codex tool-call bursts.
    #[default]
    Codex,
    /// Larger aggregate output for Cursor's rapidly sequenced tool-call batches.
    Cursor,
}

impl ClientProfile {
    /// Return the aggregate burst-token default for this client.
    #[must_use]
    pub const fn default_burst_tokens(self) -> usize {
        match self {
            Self::Codex => 8_192,
            Self::Cursor => 32_768,
        }
    }
}

impl Display for ClientProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Codex => formatter.write_str("codex"),
            Self::Cursor => formatter.write_str("cursor"),
        }
    }
}

impl FromStr for ClientProfile {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "codex" => Ok(Self::Codex),
            "cursor" => Ok(Self::Cursor),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("client profile must be either `codex` or `cursor`, got `{value}`"),
            )),
        }
    }
}
