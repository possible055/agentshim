use std::{collections::BTreeMap, sync::Arc};

use napi::{Error, bindgen_prelude::spawn_blocking};
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
    pub background: Option<bool>,
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
    run_program:
        std::sync::Mutex<std::collections::HashMap<String, agentshim_core::PreparedRunProgram>>,
    bash: std::sync::Mutex<std::collections::HashMap<String, agentshim_core::PreparedBash>>,
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
#[derive(Clone)]
pub struct ArtifactInfo {
    pub path: String,
    pub bytes: f64,
    pub complete: bool,
    pub stream: String,
}

#[napi(object)]
#[derive(Clone)]
pub struct ProcessStreamOutcome {
    pub text: String,
    pub total_bytes: f64,
    pub shown_bytes: f64,
    pub omitted_bytes: f64,
    pub artifact: Option<ArtifactInfo>,
}

#[napi(object)]
#[derive(Clone)]
pub struct NativeFailure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details: Option<serde_json::Value>,
}

pub struct NativeResult<T> {
    pub value: Option<T>,
    pub failure: Option<NativeFailure>,
}

#[napi(object)]
pub struct NativePreparedProcessResult {
    pub value: Option<PreparedProcess>,
    pub failure: Option<NativeFailure>,
}

#[napi(object)]
pub struct NativeProcessOutcomeResult {
    pub value: Option<ProcessOutcome>,
    pub failure: Option<NativeFailure>,
}

#[napi(object)]
pub struct NativeVoidResult {
    pub value: bool,
    pub failure: Option<NativeFailure>,
}

impl NativeFailure {
    pub(crate) fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        details: Option<serde_json::Value>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
            details,
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(
            "INVALID_ARGS",
            message,
            false,
            Some(serde_json::json!({ "kind": "validation" })),
        )
    }

    pub(crate) fn cancelled(phase: &'static str) -> Self {
        Self::new(
            "AGENTSHIM_CANCELLED",
            "native call was cancelled",
            false,
            Some(serde_json::json!({ "phase": phase })),
        )
    }

    pub(crate) fn engine_closed() -> Self {
        Self::new(
            "AGENTSHIM_ENGINE_CLOSED",
            "native engine is closed",
            false,
            Some(serde_json::json!({ "kind": "lifecycle" })),
        )
    }
}

pub(crate) fn napi_failure(operation: &'static str, error: Error) -> NativeFailure {
    NativeFailure::new(
        match operation {
            "read" => "AGENTSHIM_READ_FAILED",
            "grep" => "AGENTSHIM_GREP_FAILED",
            "glob" => "AGENTSHIM_GLOB_FAILED",
            "close" => "AGENTSHIM_TEARDOWN_FAILED",
            "background" => "AGENTSHIM_BACKGROUND_FAILED",
            "call" => "AGENTSHIM_ENGINE_CLOSED",
            "prepare" => "AGENTSHIM_PREPARE_FAILED",
            "spawn" => "AGENTSHIM_SPAWN_FAILED",
            _ => "AGENTSHIM_NATIVE_FAILED",
        },
        error.to_string(),
        true,
        Some(serde_json::json!({ "operation": operation })),
    )
}

pub(crate) fn process_failure(error: &agentshim_core::tools::exec::ProcessError) -> NativeFailure {
    use agentshim_core::tools::exec::{CaptureFailureKind, ProcessError};

    match error {
        ProcessError::Validation(message) => NativeFailure::new(
            "INVALID_ARGS",
            message.clone(),
            false,
            Some(serde_json::json!({ "kind": "validation" })),
        ),
        ProcessError::Resolve(message) => NativeFailure::new(
            "AGENTSHIM_PROCESS_RESOLVE_FAILED",
            message.clone(),
            false,
            Some(serde_json::json!({ "kind": "resolve" })),
        ),
        ProcessError::Unavailable(message) => NativeFailure::new(
            "AGENTSHIM_PROCESS_UNAVAILABLE",
            message.clone(),
            false,
            Some(serde_json::json!({ "kind": "unavailable" })),
        ),
        ProcessError::Io(error) => NativeFailure::new(
            "AGENTSHIM_PROCESS_IO",
            error.to_string(),
            true,
            Some(serde_json::json!({
                "kind": "io",
                "ioKind": format!("{:?}", error.kind()),
            })),
        ),
        ProcessError::Capture { kind, message } => {
            let failure_kind = match kind {
                CaptureFailureKind::LimitExceeded => "limit_exceeded",
                CaptureFailureKind::Io => "io",
                CaptureFailureKind::Protocol => "protocol",
            };
            NativeFailure::new(
                match kind {
                    CaptureFailureKind::LimitExceeded => CAPTURE_LIMIT_EXCEEDED_CODE,
                    CaptureFailureKind::Io | CaptureFailureKind::Protocol => CAPTURE_IO_FAILED_CODE,
                },
                message.clone(),
                !matches!(kind, CaptureFailureKind::LimitExceeded),
                Some(serde_json::json!({ "kind": failure_kind })),
            )
        }
        ProcessError::ResourceBusy(message) => NativeFailure::new(
            "AGENTSHIM_RESOURCE_BUSY",
            message.clone(),
            true,
            Some(serde_json::json!({ "kind": "resource" })),
        ),
        ProcessError::Timeout {
            timeout_ms,
            report,
            details,
        } => NativeFailure::new(
            "AGENTSHIM_TIMEOUT",
            report.clone(),
            true,
            Some(serde_json::json!({
                "kind": "timeout",
                "timeoutMs": timeout_ms,
                "report": report,
                "process": serde_json::to_value(details.as_ref()).unwrap_or(serde_json::Value::Null),
            })),
        ),
        ProcessError::TimeoutBeforeSpawn { timeout_ms } => NativeFailure::new(
            "AGENTSHIM_TIMEOUT",
            "native call timed out before spawn",
            true,
            Some(serde_json::json!({
                "kind": "timeout_before_spawn",
                "timeoutMs": timeout_ms,
                "terminationOutcome": "not_started",
                "containmentScope": agentshim_core::tools::exec::containment_scope(),
            })),
        ),
        ProcessError::Cancelled => NativeFailure::cancelled("process"),
        ProcessError::OutcomeUncertain => NativeFailure::new(
            "AGENTSHIM_OUTCOME_UNCERTAIN",
            "process cleanup did not complete before its deadline",
            true,
            Some(serde_json::json!({ "kind": "teardown" })),
        ),
        ProcessError::Worker(message) => native_thread_failure(message),
        ProcessError::Output(error) => NativeFailure::new(
            "AGENTSHIM_OUTPUT_FAILED",
            error.to_string(),
            false,
            Some(serde_json::json!({ "kind": "output" })),
        ),
    }
}

impl<T> NativeResult<T> {
    pub(crate) fn success(value: T) -> Self {
        Self {
            value: Some(value),
            failure: None,
        }
    }

    pub(crate) fn failure(failure: NativeFailure) -> Self {
        Self {
            value: None,
            failure: Some(failure),
        }
    }
}

pub(crate) fn prepared_result(
    result: NativeResult<PreparedProcess>,
) -> NativePreparedProcessResult {
    NativePreparedProcessResult {
        value: result.value,
        failure: result.failure,
    }
}

pub(crate) fn process_outcome_result(
    result: NativeResult<ProcessOutcome>,
) -> NativeProcessOutcomeResult {
    NativeProcessOutcomeResult {
        value: result.value,
        failure: result.failure,
    }
}

#[napi(object)]
#[allow(clippy::struct_excessive_bools)] // mirrors the TS NativeProcessOutcome wire contract
pub struct ProcessOutcome {
    pub text: String,
    pub child_nonzero: bool,
    /// Core exit label ("0", "42", "signal 9"); absent when the process never
    /// settled (launch failure, timeout).
    pub exit_code: Option<String>,
    pub stdout: ProcessStreamOutcome,
    pub stderr: ProcessStreamOutcome,
    pub artifacts: Vec<ArtifactInfo>,
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
fn stream_fact(
    output: Option<&agentshim_core::tools::ToolOutput>,
    stream: &str,
    artifacts: &[ArtifactInfo],
) -> ProcessStreamOutcome {
    let value = output
        .and_then(|output| output.structured.as_ref())
        .and_then(|structured| structured.pointer(&format!("/process/{stream}")));
    let artifact = artifacts
        .iter()
        .find(|artifact| {
            artifact.stream == stream
                || artifact.stream == "output" && matches!(stream, "stdout" | "stderr")
        })
        .cloned();
    ProcessStreamOutcome {
        text: value
            .and_then(|value| value.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        total_bytes: value
            .and_then(|value| value.get("totalBytes"))
            .and_then(serde_json::Value::as_u64)
            .map_or_else(
                || artifact.as_ref().map_or(0.0, |artifact| artifact.bytes),
                |bytes| bytes as f64,
            ),
        shown_bytes: value
            .and_then(|value| value.get("shownBytes"))
            .and_then(serde_json::Value::as_u64)
            .map_or(0.0, |bytes| bytes as f64),
        omitted_bytes: value
            .and_then(|value| value.get("omittedBytes"))
            .and_then(serde_json::Value::as_u64)
            .map_or(0.0, |bytes| bytes as f64),
        artifact,
    }
}

fn outcome_facts(
    output: &agentshim_core::tools::ToolOutput,
    artifacts: &[ArtifactInfo],
) -> (
    Option<String>,
    String,
    ProcessStreamOutcome,
    ProcessStreamOutcome,
) {
    let Some(structured) = &output.structured else {
        return (
            None,
            String::new(),
            stream_fact(None, "stdout", artifacts),
            stream_fact(None, "stderr", artifacts),
        );
    };
    let exit = structured
        .pointer("/process/exitCode")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let stderr = structured
        .pointer("/process/stderr/text")
        .and_then(serde_json::Value::as_str)
        .map_or_else(String::new, str::to_owned);
    (
        exit,
        stderr,
        stream_fact(Some(output), "stdout", artifacts),
        stream_fact(Some(output), "stderr", artifacts),
    )
}

fn settle_outcome(
    state: &EngineState,
    capture: &CallCapture,
    attribution: Option<&SandboxAttribution>,
    result: std::result::Result<
        agentshim_core::tools::ToolOutput,
        agentshim_core::tools::exec::ProcessError,
    >,
) -> NativeResult<ProcessOutcome> {
    let complete = !capture.exceeded() && result.is_ok();
    let records = match capture.publish(complete) {
        Ok(records) => records,
        Err(error) => {
            capture.discard();
            return NativeResult::failure(NativeFailure::new(
                CAPTURE_IO_FAILED_CODE,
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "capture_publish" })),
            ));
        }
    };
    let publish = should_publish(
        &records,
        complete,
        state.output_limits.capture_publish_bytes(),
    );
    let artifacts = if publish {
        register_artifacts(state, &records);
        artifact_infos(&records)
    } else {
        capture.discard();
        Vec::new()
    };
    match result {
        Ok(output) => {
            let (exit_code, stderr, stdout, stderr_stream) = outcome_facts(&output, &artifacts);
            let classification = attribution.map_or(
                Classification {
                    denied: false,
                    runner_failed: false,
                },
                |attribution| classify(exit_code.as_deref(), &stderr, attribution),
            );
            NativeResult::success(ProcessOutcome {
                text: output.text,
                child_nonzero: output.child_nonzero,
                exit_code,
                stdout,
                stderr: stderr_stream,
                artifacts,
                failure: None,
                denied: classification.denied,
                runner_failed: classification.runner_failed,
            })
        }
        Err(error) => {
            let classification = attribution.map_or(
                Classification {
                    denied: false,
                    runner_failed: false,
                },
                |attribution| classify(None, &error.to_string(), attribution),
            );
            let failure = if capture.exceeded() {
                NativeFailure::new(
                    CAPTURE_LIMIT_EXCEEDED_CODE,
                    "capture limit exceeded",
                    false,
                    Some(serde_json::json!({
                        "kind": "capture",
                        "limitBytes": capture.max_bytes,
                    })),
                )
            } else if let Some(message) = capture.io_failure() {
                NativeFailure::new(
                    CAPTURE_IO_FAILED_CODE,
                    message,
                    true,
                    Some(serde_json::json!({ "kind": "capture" })),
                )
            } else {
                process_failure(&error)
            };
            NativeResult::success(ProcessOutcome {
                text: error.to_string(),
                child_nonzero: true,
                exit_code: None,
                stdout: stream_fact(None, "stdout", &artifacts),
                stderr: stream_fact(None, "stderr", &artifacts),
                artifacts,
                failure: Some(failure),
                denied: classification.denied,
                runner_failed: classification.runner_failed,
            })
        }
    }
}

enum Either {
    RunProgram(agentshim_core::PreparedRunProgram),
    Bash(agentshim_core::PreparedBash),
}

async fn create_capture(
    state: &EngineState,
    streams: &'static [&'static str],
) -> std::result::Result<Arc<CallCapture>, NativeFailure> {
    let capture_root = state.capture_root.clone();
    let session_key = state.session_key.clone();
    let capture_max_bytes = state.capture_max_bytes;
    spawn_blocking(move || {
        let call_key = uuid::Uuid::new_v4().simple().to_string();
        match CallCapture::create(
            &capture_root,
            &session_key,
            &call_key,
            streams,
            capture_max_bytes,
        ) {
            Ok(capture) => Ok(Arc::new(capture)),
            Err(error) => Err(NativeFailure::new(
                CAPTURE_IO_FAILED_CODE,
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "capture_create" })),
            )),
        }
    })
    .await
    .map_err(native_thread_failure)?
}

fn native_thread_failure(message: impl std::fmt::Display) -> NativeFailure {
    NativeFailure::new(
        "AGENTSHIM_NATIVE_THREAD_FAILED",
        message.to_string(),
        true,
        Some(serde_json::json!({ "kind": "native_thread" })),
    )
}

async fn settle_capture(
    state: Arc<EngineState>,
    capture: Arc<CallCapture>,
    attribution: Option<SandboxAttribution>,
    result: std::result::Result<
        agentshim_core::tools::ToolOutput,
        agentshim_core::tools::exec::ProcessError,
    >,
) -> NativeResult<ProcessOutcome> {
    match spawn_blocking(move || {
        settle_outcome(&state, capture.as_ref(), attribution.as_ref(), result)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => NativeResult::failure(native_thread_failure(error)),
    }
}

impl EngineState {
    fn take_prepared<T>(
        lock: &std::sync::Mutex<std::collections::HashMap<String, T>>,
        handle: &str,
    ) -> std::result::Result<T, NativeFailure> {
        let mut prepared = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        prepared.remove(handle).ok_or_else(|| {
            NativeFailure::new(
                "AGENTSHIM_PREPARED_HANDLE_INVALID",
                "prepared handle is unknown or already spawned",
                false,
                Some(serde_json::json!({ "kind": "prepared_handle" })),
            )
        })
    }

    fn take_run_program(
        &self,
        handle: &str,
    ) -> std::result::Result<agentshim_core::PreparedRunProgram, NativeFailure> {
        Self::take_prepared(&self.prepared.run_program, handle)
    }

    pub(crate) fn take_bash(
        &self,
        handle: &str,
    ) -> std::result::Result<agentshim_core::PreparedBash, NativeFailure> {
        Self::take_prepared(&self.prepared.bash, handle)
    }

    pub(crate) fn prepare_run_program(
        &self,
        args: ProcessArgs,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> NativeResult<PreparedProcess> {
        if cancellation.is_cancelled() {
            return NativeResult::failure(NativeFailure::cancelled("prepare"));
        }
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
        let context = agentshim_core::OperationContext::new(
            cancellation.clone(),
            Arc::new(self.output_limits.clone()),
        );
        let prepared =
            match self
                .tool_engine
                .prepare_run_program(&request, self.timeout_ceiling_ms, &context)
            {
                Ok(prepared) => prepared,
                Err(error) => return NativeResult::failure(process_failure(&error)),
            };
        let handle = uuid::Uuid::new_v4().simple().to_string();
        let final_argv = prepared.argv();
        if cancellation.is_cancelled() {
            return NativeResult::failure(NativeFailure::cancelled("prepare"));
        }
        self.prepared
            .run_program
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(handle.clone(), prepared);
        NativeResult::success(PreparedProcess {
            handle,
            argv: final_argv,
        })
    }

    pub(crate) fn prepare_bash(
        &self,
        args: BashArgs,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> NativeResult<PreparedProcess> {
        if cancellation.is_cancelled() {
            return NativeResult::failure(NativeFailure::cancelled("prepare"));
        }
        let msys = match args.msys_argument_conversion.as_deref() {
            None | Some("enabled" | "default") => {
                agentshim_core::tools::bash::MsysArgumentConversion::Default
            }
            Some("disabled") => agentshim_core::tools::bash::MsysArgumentConversion::Disabled,
            Some(other) => {
                return NativeResult::failure(NativeFailure::invalid(format!(
                    "msys_argument_conversion must be enabled or disabled, got {other}"
                )));
            }
        };
        let request = agentshim_core::tools::bash::BashRequest {
            command: args.command,
            cwd: args.cwd,
            timeout_ms: args.timeout_ms.map(u64::from),
            detach: false,
            log_path: None,
            msys_argument_conversion: msys,
        };
        let context = agentshim_core::OperationContext::new(
            cancellation.clone(),
            Arc::new(self.output_limits.clone()),
        );
        let prepared = match if args.background.unwrap_or(false) {
            self.tool_engine.prepare_background_bash(
                &request,
                self.background_timeout_max_ms,
                self.timeout_ceiling_ms,
                &context,
            )
        } else {
            self.tool_engine
                .prepare_bash(&request, self.timeout_ceiling_ms, &context)
        } {
            Ok(prepared) => prepared,
            Err(error) => return NativeResult::failure(process_failure(&error)),
        };
        let handle = uuid::Uuid::new_v4().simple().to_string();
        let final_argv = prepared.argv();
        if cancellation.is_cancelled() {
            return NativeResult::failure(NativeFailure::cancelled("prepare"));
        }
        self.prepared
            .bash
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(handle.clone(), prepared);
        NativeResult::success(PreparedProcess {
            handle,
            argv: final_argv,
        })
    }

    pub(crate) async fn spawn_prepared(
        self: &Arc<Self>,
        call_id: String,
        handle: String,
        wrapped_argv: Option<&[String]>,
        attribution: Option<SandboxAttribution>,
    ) -> NativeResult<ProcessOutcome> {
        let wrapped_argv = wrapped_argv.map(<[String]>::to_vec);
        let cancellation = match self.call_token(&call_id) {
            Ok(token) => token,
            Err(failure) => return NativeResult::failure(failure),
        };
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
                        Err(_) => return NativeResult::failure(run_error),
                    }
                }
            }
        };
        let streams: &'static [&'static str] = match prepared {
            Either::Bash(_) => &["output"],
            Either::RunProgram(_) => &["stdout", "stderr"],
        };
        if cancellation.is_cancelled() {
            return NativeResult::failure(NativeFailure::cancelled("spawn"));
        }
        let capture = match create_capture(&state, streams).await {
            Ok(capture) => capture,
            Err(failure) => return NativeResult::failure(failure),
        };
        let sink = Arc::clone(&capture) as Arc<dyn agentshim_core::tools::exec::CaptureSink>;
        let context = agentshim_core::OperationContext::new(
            cancellation,
            Arc::new(state.output_limits.clone()),
        );
        let result = match prepared {
            Either::RunProgram(prepared) => {
                state
                    .tool_engine
                    .spawn_run_program(prepared, wrapped_argv, context, Some(sink))
                    .await
            }
            Either::Bash(prepared) => {
                state
                    .tool_engine
                    .spawn_bash(prepared, wrapped_argv, context, Some(sink))
                    .await
            }
        };
        settle_capture(state, capture, attribution, result).await
    }
}
