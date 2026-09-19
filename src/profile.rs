use std::{fmt::Display, io, str::FromStr, time::Duration};

pub(crate) const CODEX_BURST_TOKENS: usize = 16_384;
pub(crate) const CURSOR_BURST_TOKENS: usize = 32_768;

/// Cursor's MCP client enforces a fixed 120-second `tools/call` timeout with no
/// configuration override, so the server shelf must stay at or below that ceiling
/// for the server's own Timeout response to arrive before the client gives up.
pub(crate) const CURSOR_TOOL_TIMEOUT_SHELF: Duration = Duration::from_secs(120);

/// Client-specific defaults for aggregate tool-response output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ClientProfile {
    /// Bounded aggregate output for Codex tool-call bursts.
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
            Self::Codex => CODEX_BURST_TOKENS,
            Self::Cursor => CURSOR_BURST_TOKENS,
        }
    }

    /// Return the default tool-timeout shelf for this client when
    /// `AGENTSHIM_TOOL_TIMEOUT_SHELF` is not set in the environment.
    #[must_use]
    pub const fn default_tool_timeout_shelf(self) -> Duration {
        match self {
            Self::Codex => crate::runtime::DEFAULT_TOOL_TIMEOUT_SHELF,
            Self::Cursor => CURSOR_TOOL_TIMEOUT_SHELF,
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

/// Gate the idle watchdog by client profile: only the codex profile — whose host is
/// known to leave conversation servers running — arms it. `RuntimeConfig::from_env()`
/// already parsed and validated the value; this runs in `build()` after all builder
/// overrides and the final profile are known.
pub(crate) fn resolve_idle_timeout_from(
    value: Option<Duration>,
    profile: ClientProfile,
) -> Option<Duration> {
    match profile {
        ClientProfile::Codex => value,
        ClientProfile::Cursor => None,
    }
}

/// Resolve the effective tool-timeout shelf: when the environment override is set,
/// `RuntimeConfig::from_env` has already fail-fast parsed it into `configured`, so
/// that value is authoritative; otherwise the client-profile default applies.
pub(crate) fn resolve_tool_timeout_shelf(
    profile: ClientProfile,
    configured: Duration,
    env_override_set: bool,
) -> Duration {
    if env_override_set {
        configured
    } else {
        profile.default_tool_timeout_shelf()
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
                format!("client profile must be `codex` or `cursor`, got `{value}`"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CODEX_BURST_TOKENS, CURSOR_BURST_TOKENS, CURSOR_TOOL_TIMEOUT_SHELF, ClientProfile,
    };
    use crate::runtime::DEFAULT_TOOL_TIMEOUT_SHELF;
    use std::time::Duration;

    #[test]
    fn profile_burst_defaults_match_the_client_contract() {
        assert_eq!(ClientProfile::default(), ClientProfile::Codex);
        assert_eq!(
            ClientProfile::Codex.default_burst_tokens(),
            CODEX_BURST_TOKENS
        );
        assert_eq!(
            ClientProfile::Cursor.default_burst_tokens(),
            CURSOR_BURST_TOKENS
        );
    }

    #[test]
    fn profile_tool_timeout_shelf_defaults_match_the_client_contract() {
        assert_eq!(
            ClientProfile::Codex.default_tool_timeout_shelf(),
            DEFAULT_TOOL_TIMEOUT_SHELF,
        );
        assert_eq!(
            ClientProfile::Cursor.default_tool_timeout_shelf(),
            CURSOR_TOOL_TIMEOUT_SHELF,
        );
        assert_eq!(CURSOR_TOOL_TIMEOUT_SHELF, Duration::from_secs(120));
    }

    #[test]
    fn resolve_tool_timeout_shelf_uses_env_presence_to_choose_configured_or_profile_default() {
        let configured = Duration::from_secs(300);
        assert_eq!(
            super::resolve_tool_timeout_shelf(ClientProfile::Codex, configured, true),
            configured,
        );
        assert_eq!(
            super::resolve_tool_timeout_shelf(ClientProfile::Cursor, configured, true),
            configured,
        );
        assert_eq!(
            super::resolve_tool_timeout_shelf(
                ClientProfile::Cursor,
                DEFAULT_TOOL_TIMEOUT_SHELF,
                false
            ),
            Duration::from_secs(120),
        );
        assert_eq!(
            super::resolve_tool_timeout_shelf(
                ClientProfile::Codex,
                DEFAULT_TOOL_TIMEOUT_SHELF,
                false
            ),
            DEFAULT_TOOL_TIMEOUT_SHELF,
        );
    }

    #[test]
    fn idle_timeout_only_applies_to_the_codex_profile() {
        let timeout = Some(Duration::from_secs(30));
        assert_eq!(
            super::resolve_idle_timeout_from(timeout, ClientProfile::Codex),
            timeout
        );
        assert_eq!(
            super::resolve_idle_timeout_from(timeout, ClientProfile::Cursor),
            None
        );
        assert_eq!(
            super::resolve_idle_timeout_from(None, ClientProfile::Codex),
            None
        );
    }
}
