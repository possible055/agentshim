//! Host-provided engine configuration: option surface, validation, and the
//! derived native runtime knobs.

use std::time::Duration;

use napi::{Error, Result};
use napi_derive::napi;

use crate::budget::NativeOutputLimits;
use crate::process::clamp_capture_ceiling;

/// One environment entry for the Engine's child process configuration.
#[napi(object)]
pub struct EnvEntry {
    pub key: String,
    pub value: String,
}

#[napi(object)]
#[derive(Default)]
pub struct NativeHostOptions {
    /// Model-visible page byte budget for one tool response.
    pub page_budget_bytes: Option<u32>,
    /// Repository read scope for this Engine.
    pub read_scope: Option<String>,
    /// DSH tool timeout shelf. The process ceiling is derived below it by the
    /// core cleanup and protocol slack.
    pub tool_timeout_shelf_ms: Option<u32>,
    /// Maximum background job runtime, resolved by the embedding host.
    pub background_job_timeout_max_ms: Option<u32>,
    /// Read-only admission capacity (read/glob/grep) for the shared runtime.
    pub read_only_calls: Option<u32>,
    /// Explicit child environment; the Engine never reads the host ambient
    /// environment on its own.
    pub env: Option<Vec<EnvEntry>>,
    /// Private persistent root for durable process capture artifacts.
    pub capture_root: Option<String>,
    /// Aggregate raw capture ceiling for one process call, in bytes.
    pub capture_max_bytes: Option<f64>,
    /// Artifact cleanup policy: `never` or `session-end`.
    pub capture_cleanup: Option<String>,
}

pub(crate) struct NativeEngineConfig {
    pub(crate) output_limits: NativeOutputLimits,
    pub(crate) read_scope: agentshim_core::path::ReadScope,
    pub(crate) timeout_ceiling_ms: u64,
    pub(crate) background_timeout_max_ms: u64,
    pub(crate) process_environment: agentshim_core::ProcessEnvironment,
    pub(crate) capture_root: std::path::PathBuf,
    pub(crate) capture_max_bytes: u64,
    pub(crate) capture_cleanup_session_end: bool,
}

fn configured_read_only_calls(value: Option<u32>) -> Result<usize> {
    value.map_or_else(
        || Ok(agentshim_core::runtime::DEFAULT_READ_ONLY_CALLS),
        |value| {
            usize::try_from(value)
                .ok()
                .filter(|configured| {
                    (1..=agentshim_core::runtime::MAX_CONFIGURED_READ_ONLY_CALLS)
                        .contains(configured)
                })
                .ok_or_else(|| {
                    Error::new(
                        napi::Status::InvalidArg,
                        format!(
                            "readOnlyCalls must be an integer from 1 to {}",
                            agentshim_core::runtime::MAX_CONFIGURED_READ_ONLY_CALLS
                        ),
                    )
                })
        },
    )
}

impl NativeEngineConfig {
    pub(crate) fn new(
        options: NativeHostOptions,
    ) -> Result<(Self, agentshim_core::runtime::RuntimeConfig)> {
        let read_scope = match options.read_scope.as_deref() {
            None | Some("normal") => agentshim_core::path::ReadScope::Normal,
            Some("unrestricted") => agentshim_core::path::ReadScope::Unrestricted,
            Some(other) => {
                return Err(Error::new(
                    napi::Status::InvalidArg,
                    format!("readScope must be normal or unrestricted, got {other}"),
                ));
            }
        };
        let shelf_ms = options.tool_timeout_shelf_ms.map_or(600_000_u64, u64::from);
        let shelf = Duration::from_millis(shelf_ms);
        if !(agentshim_core::runtime::MIN_TOOL_TIMEOUT_SHELF
            ..=agentshim_core::runtime::MAX_TOOL_TIMEOUT_SHELF)
            .contains(&shelf)
        {
            return Err(Error::new(
                napi::Status::InvalidArg,
                "toolTimeoutShelfMs must be from 15000 through 3600000",
            ));
        }
        let background_timeout_max = options.background_job_timeout_max_ms.map_or(
            agentshim_core::runtime::DEFAULT_BACKGROUND_JOB_TIMEOUT_MAX,
            |milliseconds| Duration::from_millis(u64::from(milliseconds)),
        );
        if !(agentshim_core::runtime::MIN_BACKGROUND_JOB_TIMEOUT_MAX
            ..=agentshim_core::runtime::MAX_BACKGROUND_JOB_TIMEOUT_MAX)
            .contains(&background_timeout_max)
        {
            return Err(Error::new(
                napi::Status::InvalidArg,
                "backgroundJobTimeoutMaxMs must be from 600000 through 14400000",
            ));
        }
        let env = options
            .env
            .unwrap_or_default()
            .into_iter()
            .map(|entry| (entry.key, entry.value))
            .collect::<Vec<_>>();
        let bash_override = env
            .iter()
            .find(|(key, _)| key == agentshim_core::tools::bash::BASH_OVERRIDE_ENV)
            .map(|(_, value)| std::ffi::OsString::from(value));
        let process_environment = agentshim_core::ProcessEnvironment::new(env, bash_override)
            .map_err(|error| Error::new(napi::Status::InvalidArg, error.to_string()))?;
        let capture_root = options.capture_root.map_or_else(
            || {
                std::env::temp_dir().join(format!(
                    "agentshim-captures-{}",
                    uuid::Uuid::new_v4().simple()
                ))
            },
            std::path::PathBuf::from,
        );
        std::fs::create_dir_all(&capture_root)
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
        let capture_root = std::fs::canonicalize(capture_root)
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
        let capture_cleanup_session_end = match options.capture_cleanup.as_deref() {
            None | Some("never") => false,
            Some("session-end") => true,
            Some(other) => {
                return Err(Error::new(
                    napi::Status::InvalidArg,
                    format!("captureCleanup must be never or session-end, got {other}"),
                ));
            }
        };
        let mut runtime = agentshim_core::runtime::RuntimeConfig::for_host_defaults();
        runtime.tool_timeout_shelf = shelf;
        runtime.background_job_timeout_max = background_timeout_max;
        runtime.read_only_calls = configured_read_only_calls(options.read_only_calls)?;
        Ok((
            Self {
                output_limits: NativeOutputLimits::new(options.page_budget_bytes),
                read_scope,
                timeout_ceiling_ms: agentshim_core::tools::exec::max_timeout_ms_from_shelf(shelf),
                background_timeout_max_ms: u64::try_from(background_timeout_max.as_millis())
                    .unwrap_or(u64::MAX),
                process_environment,
                capture_root,
                capture_max_bytes: clamp_capture_ceiling(options.capture_max_bytes),
                capture_cleanup_session_end,
            },
            runtime,
        ))
    }
}
