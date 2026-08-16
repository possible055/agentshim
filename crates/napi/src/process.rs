use std::{collections::BTreeMap, sync::Arc, time::Duration};

use napi::{Error, Result, bindgen_prelude::spawn_blocking};
use napi_derive::napi;

use crate::capture::{
    ArtifactRecord, CAPTURE_IO_FAILED_CODE, CAPTURE_LIMIT_EXCEEDED_CODE, CallCapture,
    DEFAULT_CAPTURE_MAX_BYTES, MAX_CAPTURE_MAX_BYTES, MIN_CAPTURE_MAX_BYTES, should_publish,
};
use crate::classify::{Classification, SandboxAttribution, classify};
use crate::engine::EngineState;

#[napi(object)]
pub struct ProcessArgs {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Option<std::collections::HashMap<String, String>>,
    pub unset_env: Option<Vec<String>>,
    pub stdin: Option<String>,
    pub timeout_ms: Option<u32>,
}

#[napi(object)]
pub struct BashArgs {
    pub command: String,
    pub cwd: Option<String>,
    pub timeout_ms: Option<u32>,
    pub msys_argument_conversion: Option<String>,
}

#[napi(object)]
pub struct PreparedProcess {
    /// Opaque single-use handle for the matching `spawn_prepared` call.
    pub handle: String,
    /// Final executable and argv, the exact recipe a sandbox must wrap.
    pub argv: Vec<String>,
}

/// Prepared foreground launches awaiting a sandbox decision. Each handle is
/// single-use: spawning or dropping the engine consumes it.
pub(crate) struct PreparedHandles {
    run_program: std::sync::Mutex<
        std::collections::HashMap<String, agentshim_core::tools::run_program::PreparedRunProgram>,
    >,
    bash: std::sync::Mutex<
        std::collections::HashMap<String, agentshim_core::tools::bash::PreparedBash>,
    >,
}

impl PreparedHandles {
    pub(crate) fn new() -> Self {
        Self {
            run_program: std::sync::Mutex::new(std::collections::HashMap::new()),
            bash: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub(crate) fn clear(&self) {
        self.run_program
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.bash
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

#[napi(object)]
pub struct ArtifactInfo {
    pub path: String,
    pub bytes: f64,
    pub complete: bool,
    pub stream: String,
}

#[napi(object)]
#[derive(Clone)]
pub struct NativeFailure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details: Option<String>,
}

#[napi(object)]
#[allow(clippy::struct_excessive_bools)] // mirrors the TS NativeProcessOutcome wire contract
pub struct ProcessOutcome {
    pub text: String,
    pub child_nonzero: bool,
    /// Core exit label ("0", "42", "signal 9"); absent when the process never
    /// settled (launch failure, timeout).
    pub exit_code: Option<String>,
    pub artifacts: Vec<ArtifactInfo>,
    pub limit_exceeded: bool,
    pub failure: Option<NativeFailure>,
    /// Sandbox classification of this spawn against the passed attribution;
    /// both stay false when no attribution was supplied.
    pub denied: bool,
    pub runner_failed: bool,
}

pub(crate) fn clamp_capture_ceiling(bytes: Option<f64>) -> u64 {
    let value = bytes.unwrap_or(DEFAULT_CAPTURE_MAX_BYTES as f64);
    (value as u64).clamp(MIN_CAPTURE_MAX_BYTES, MAX_CAPTURE_MAX_BYTES)
}

fn artifact_infos(records: &[ArtifactRecord]) -> Vec<ArtifactInfo> {
    records
        .iter()
        .map(|record| ArtifactInfo {
            path: record.path.to_string_lossy().into_owned(),
            bytes: record.bytes as f64,
            complete: record.complete,
            stream: record.stream.clone(),
        })
        .collect()
}

pub(crate) fn register_artifacts(state: &EngineState, records: &[ArtifactRecord]) {
    let mut table = state
        .artifacts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for record in records {
        table.push(record.clone());
    }
}

/// Exit label and bounded stderr preview a completed core process output
/// carries in its bridge structured content — the same evidence the MCP bridge
/// classified on, so native classification sees identical inputs.
fn outcome_facts(output: &agentshim_core::tools::ToolOutput) -> (Option<String>, String) {
    let Some(structured) = &output.structured else {
        return (None, String::new());
    };
    let exit = structured
        .pointer("/process/exitCode")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let stderr = structured
        .pointer("/process/stderr/text")
        .and_then(serde_json::Value::as_str)
        .map_or_else(String::new, str::to_owned);
    (exit, stderr)
}

fn settle_outcome(
    state: &EngineState,
    capture: &CallCapture,
    attribution: Option<&SandboxAttribution>,
    result: std::result::Result<
        agentshim_core::tools::ToolOutput,
        agentshim_core::tools::exec::ProcessError,
    >,
) -> Result<ProcessOutcome> {
    let complete = !capture.exceeded() && result.is_ok();
    let records = capture.publish(complete).map_err(|error| {
        Error::new(
            napi::Status::GenericFailure,
            format!("{CAPTURE_IO_FAILED_CODE}: {error}"),
        )
    })?;
    let publish = should_publish(&records, complete);
    let artifacts = if publish {
        register_artifacts(state, &records);
        artifact_infos(&records)
    } else {
        capture.discard();
        Vec::new()
    };
    match result {
        Ok(output) => {
            let (exit_code, stderr) = outcome_facts(&output);
            let classification = attribution.map_or(
                Classification {
                    denied: false,
                    runner_failed: false,
                },
                |attribution| classify(exit_code.as_deref(), &stderr, attribution),
            );
            Ok(ProcessOutcome {
                text: output.text,
                child_nonzero: output.child_nonzero,
                exit_code,
                artifacts,
                limit_exceeded: false,
                failure: None,
                denied: classification.denied,
                runner_failed: classification.runner_failed,
            })
        }
        Err(error) => {
            let code = if capture.exceeded() {
                CAPTURE_LIMIT_EXCEEDED_CODE.to_owned()
            } else if capture.io_failure().is_some() {
                CAPTURE_IO_FAILED_CODE.to_owned()
            } else {
                process_error_code(&error).to_owned()
            };
            let classification = attribution.map_or(
                Classification {
                    denied: false,
                    runner_failed: false,
                },
                |attribution| classify(None, &error.to_string(), attribution),
            );
            Ok(ProcessOutcome {
                text: error.to_string(),
                child_nonzero: true,
                exit_code: None,
                artifacts,
                limit_exceeded: capture.exceeded(),
                failure: Some(NativeFailure {
                    retryable: process_error_retryable(&error),
                    message: error.to_string(),
                    code,
                    details: None,
                }),
                denied: classification.denied,
                runner_failed: classification.runner_failed,
            })
        }
    }
}

fn process_error_code(error: &agentshim_core::tools::exec::ProcessError) -> &'static str {
    use agentshim_core::tools::exec::{CaptureFailureKind, ProcessError};

    match error {
        ProcessError::Validation(_) => "INVALID_ARGS",
        ProcessError::Resolve(_) | ProcessError::Unavailable(_) => "AGENTSHIM_PROCESS_UNAVAILABLE",
        ProcessError::Io(_) => "AGENTSHIM_PROCESS_IO",
        ProcessError::Capture { kind, .. } => match kind {
            CaptureFailureKind::LimitExceeded => CAPTURE_LIMIT_EXCEEDED_CODE,
            CaptureFailureKind::Io | CaptureFailureKind::Protocol => CAPTURE_IO_FAILED_CODE,
        },
        ProcessError::ResourceBusy(_) => "AGENTSHIM_RESOURCE_BUSY",
        ProcessError::Timeout { .. } | ProcessError::TimeoutBeforeSpawn { .. } => {
            "AGENTSHIM_TIMEOUT"
        }
        ProcessError::Cancelled => "AGENTSHIM_CANCELLED",
        ProcessError::OutcomeUncertain => "AGENTSHIM_OUTCOME_UNCERTAIN",
        ProcessError::Output(_) => "AGENTSHIM_OUTPUT_FAILED",
    }
}

fn process_error_retryable(error: &agentshim_core::tools::exec::ProcessError) -> bool {
    use agentshim_core::tools::exec::ProcessError;

    matches!(
        error,
        ProcessError::Io(_)
            | ProcessError::Capture { .. }
            | ProcessError::ResourceBusy(_)
            | ProcessError::Timeout { .. }
            | ProcessError::TimeoutBeforeSpawn { .. }
            | ProcessError::OutcomeUncertain
    )
}

enum Either {
    RunProgram(agentshim_core::tools::run_program::PreparedRunProgram),
    Bash(agentshim_core::tools::bash::PreparedBash),
}

fn process_error(error: agentshim_core::tools::exec::ProcessError) -> Error {
    Error::new(napi::Status::GenericFailure, error.to_string())
}

/// Run one process call under a private durable capture: the capture object stays
/// concrete so its totals, ceiling, and artifacts survive the spawn unchanged.
pub(crate) async fn run_with_capture<R>(
    state: Arc<EngineState>,
    streams: &'static [&'static str],
    attribution: Option<SandboxAttribution>,
    run: R,
) -> Result<ProcessOutcome>
where
    R: FnOnce(
            &EngineState,
            Option<&Arc<dyn agentshim_core::tools::exec::spawn::CaptureSink>>,
        ) -> std::result::Result<
            agentshim_core::tools::ToolOutput,
            agentshim_core::tools::exec::ProcessError,
        > + Send
        + 'static,
{
    let _active = state.enter_call()?;
    spawn_blocking(move || {
        let call_key = uuid::Uuid::new_v4().simple().to_string();
        let capture = CallCapture::create(
            &state.capture_root,
            &state.session_key,
            &call_key,
            streams,
            state.capture_max_bytes,
        )
        .map_err(|error| {
            Error::new(
                napi::Status::GenericFailure,
                format!("{CAPTURE_IO_FAILED_CODE}: {error}"),
            )
        })?;
        let sink: Arc<CallCapture> = Arc::new(capture);
        let dyn_sink: Arc<dyn agentshim_core::tools::exec::spawn::CaptureSink> =
            Arc::clone(&sink) as Arc<dyn agentshim_core::tools::exec::spawn::CaptureSink>;
        let result = run(&state, Some(&dyn_sink));
        settle_outcome(&state, sink.as_ref(), attribution.as_ref(), result)
    })
    .await
    .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?
}

impl EngineState {
    fn take_run_program(
        &self,
        handle: &str,
    ) -> Result<agentshim_core::tools::run_program::PreparedRunProgram> {
        let mut prepared = self
            .prepared
            .run_program
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        prepared.remove(handle).ok_or_else(|| {
            Error::new(
                napi::Status::GenericFailure,
                "prepared handle is unknown or already spawned",
            )
        })
    }

    pub(crate) fn take_bash(
        &self,
        handle: &str,
    ) -> Result<agentshim_core::tools::bash::PreparedBash> {
        let mut prepared = self
            .prepared
            .bash
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        prepared.remove(handle).ok_or_else(|| {
            Error::new(
                napi::Status::GenericFailure,
                "prepared handle is unknown or already spawned",
            )
        })
    }

    pub(crate) fn prepare_run_program(&self, args: ProcessArgs) -> Result<PreparedProcess> {
        let mut env = BTreeMap::new();
        for (key, value) in args.env.unwrap_or_default() {
            env.insert(key, value);
        }
        let request = agentshim_core::tools::run_program::ProcessRequest {
            program: args.program,
            args: args.args,
            cwd: args.cwd,
            env,
            unset_env: args.unset_env.unwrap_or_default(),
            stdin: args.stdin,
            timeout_ms: args.timeout_ms.map(u64::from),
        };
        let resolver = agentshim_core::tools::exec::resolve::ProcessResolver::capture();
        let timeout = Duration::from_millis(
            request
                .timeout_ms(self.timeout_ceiling_ms)
                .min(self.timeout_ceiling_ms),
        );
        let mut prepared = agentshim_core::tools::run_program::prepare_run_program(
            &self.root,
            &resolver,
            &request,
            timeout,
            self.timeout_ceiling_ms,
        )
        .map_err(process_error)?;
        prepared.environment.base = Some(self.env.clone());
        let handle = uuid::Uuid::new_v4().simple().to_string();
        let mut final_argv = vec![prepared.resolved.executable.to_string_lossy().into_owned()];
        final_argv.extend(prepared.args.iter().cloned());
        self.prepared
            .run_program
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(handle.clone(), prepared);
        Ok(PreparedProcess {
            handle,
            argv: final_argv,
        })
    }

    pub(crate) fn prepare_bash(&self, args: BashArgs) -> Result<PreparedProcess> {
        let msys = match args.msys_argument_conversion.as_deref() {
            None | Some("enabled" | "default") => {
                agentshim_core::tools::bash::MsysArgumentConversion::Default
            }
            Some("disabled") => agentshim_core::tools::bash::MsysArgumentConversion::Disabled,
            Some(other) => {
                return Err(Error::new(
                    napi::Status::InvalidArg,
                    format!("msys_argument_conversion must be enabled or disabled, got {other}"),
                ));
            }
        };
        let request = agentshim_core::tools::bash::BashRequest {
            command: args.command,
            cwd: args.cwd,
            timeout_ms: args.timeout_ms.map(u64::from),
            detach: false,
            log_path: None,
            server_capture: false,
            msys_argument_conversion: msys,
        };
        let timeout = Duration::from_millis(
            request
                .timeout_ms(self.timeout_ceiling_ms)
                .min(self.timeout_ceiling_ms),
        );
        let mut prepared = agentshim_core::tools::bash::prepare_bash_foreground(
            &self.root,
            &self.locator,
            &request,
            timeout,
            self.timeout_ceiling_ms,
            &tokio_util::sync::CancellationToken::new(),
        )
        .map_err(process_error)?;
        prepared.environment.base = Some(self.env.clone());
        let handle = uuid::Uuid::new_v4().simple().to_string();
        let mut final_argv = vec![prepared.resolved.executable.to_string_lossy().into_owned()];
        final_argv.extend(prepared.args.iter().cloned());
        self.prepared
            .bash
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(handle.clone(), prepared);
        Ok(PreparedProcess {
            handle,
            argv: final_argv,
        })
    }

    pub(crate) async fn spawn_prepared(
        self: &Arc<Self>,
        handle: String,
        wrapped_argv: Option<&[String]>,
        attribution: Option<SandboxAttribution>,
    ) -> Result<ProcessOutcome> {
        let wrapped_argv = wrapped_argv.map(<[String]>::to_vec);
        let cancellation = self.shutdown.child_token();
        let state = Arc::clone(self);
        // The handle is consumed before spawning: a lost or double spawn can never
        // produce two process trees from one approval decision.
        let prepared = {
            let run_program = state.take_run_program(&handle);
            match run_program {
                Ok(prepared) => Either::RunProgram(prepared),
                Err(run_error) => {
                    let bash = state.take_bash(&handle);
                    match bash {
                        Ok(prepared) => Either::Bash(prepared),
                        Err(_) => return Err(run_error),
                    }
                }
            }
        };
        let streams: &'static [&'static str] = match prepared {
            Either::Bash(_) => &["output"],
            Either::RunProgram(_) => &["stdout", "stderr"],
        };
        run_with_capture(
            state,
            streams,
            attribution,
            move |state, sink| match prepared {
                Either::RunProgram(prepared) => {
                    agentshim_core::tools::run_program::execute_prepared_run_program(
                        prepared,
                        wrapped_argv.as_deref(),
                        &cancellation,
                        &state.budget,
                        sink,
                    )
                }
                Either::Bash(prepared) => agentshim_core::tools::bash::execute_prepared_bash(
                    prepared,
                    wrapped_argv.as_deref(),
                    &cancellation,
                    &state.budget,
                    sink,
                ),
            },
        )
        .await
    }

    pub(crate) async fn run_program_outcome(
        self: &Arc<Self>,
        args: ProcessArgs,
    ) -> Result<ProcessOutcome> {
        let mut env = BTreeMap::new();
        for (key, value) in args.env.unwrap_or_default() {
            env.insert(key, value);
        }
        let request = agentshim_core::tools::run_program::ProcessRequest {
            program: args.program,
            args: args.args,
            cwd: args.cwd,
            env,
            unset_env: args.unset_env.unwrap_or_default(),
            stdin: args.stdin,
            timeout_ms: args.timeout_ms.map(u64::from),
        };
        let ceiling = self.timeout_ceiling_ms;
        let resolver = agentshim_core::tools::exec::resolve::ProcessResolver::capture();
        let timeout = Duration::from_millis(request.timeout_ms(ceiling).min(ceiling));
        let cancellation = self.shutdown.child_token();
        let state = Arc::clone(self);
        run_with_capture(state, &["stdout", "stderr"], None, move |state, sink| {
            let mut prepared = agentshim_core::tools::run_program::prepare_run_program(
                &state.root,
                &resolver,
                &request,
                timeout,
                state.timeout_ceiling_ms,
            )?;
            prepared.environment.base = Some(state.env.clone());
            agentshim_core::tools::run_program::execute_prepared_run_program(
                prepared,
                None,
                &cancellation,
                &state.budget,
                sink,
            )
        })
        .await
    }

    pub(crate) async fn bash_outcome(self: &Arc<Self>, args: BashArgs) -> Result<ProcessOutcome> {
        let msys = match args.msys_argument_conversion.as_deref() {
            None => agentshim_core::tools::bash::MsysArgumentConversion::default(),
            Some("enabled" | "default") => {
                agentshim_core::tools::bash::MsysArgumentConversion::Default
            }
            Some("disabled") => agentshim_core::tools::bash::MsysArgumentConversion::Disabled,
            Some(other) => {
                return Err(Error::new(
                    napi::Status::InvalidArg,
                    format!("msys_argument_conversion must be enabled or disabled, got {other}"),
                ));
            }
        };
        let request = agentshim_core::tools::bash::BashRequest {
            command: args.command,
            cwd: args.cwd,
            timeout_ms: args.timeout_ms.map(u64::from),
            detach: false,
            log_path: None,
            server_capture: false,
            msys_argument_conversion: msys,
        };
        let ceiling = self.timeout_ceiling_ms;
        let timeout = Duration::from_millis(request.timeout_ms(ceiling).min(ceiling));
        let cancellation = self.shutdown.child_token();
        let state = Arc::clone(self);
        run_with_capture(state, &["output"], None, move |state, sink| {
            let mut prepared = agentshim_core::tools::bash::prepare_bash_foreground(
                &state.root,
                &state.locator,
                &request,
                timeout,
                state.timeout_ceiling_ms,
                &cancellation,
            )?;
            prepared.environment.base = Some(state.env.clone());
            agentshim_core::tools::bash::execute_prepared_bash(
                prepared,
                None,
                &cancellation,
                &state.budget,
                sink,
            )
        })
        .await
    }
}
