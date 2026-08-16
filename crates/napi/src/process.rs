use std::{collections::BTreeMap, sync::Arc, time::Duration};

use napi::{Error, Result, bindgen_prelude::spawn_blocking};
use napi_derive::napi;

use crate::capture::{
    ArtifactRecord, CAPTURE_IO_FAILED_CODE, CAPTURE_LIMIT_EXCEEDED_CODE, CallCapture,
    DEFAULT_CAPTURE_MAX_BYTES, MAX_CAPTURE_MAX_BYTES, MIN_CAPTURE_MAX_BYTES, should_publish,
};
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
}

#[napi(object)]
pub struct ArtifactInfo {
    pub path: String,
    pub bytes: f64,
    pub complete: bool,
    pub stream: String,
}

#[napi(object)]
pub struct ProcessOutcome {
    pub text: String,
    pub child_nonzero: bool,
    pub artifacts: Vec<ArtifactInfo>,
    pub limit_exceeded: bool,
    pub error_code: Option<String>,
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

fn register_artifacts(state: &EngineState, records: &[ArtifactRecord]) {
    let mut table = state
        .artifacts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for record in records {
        table.push(record.clone());
    }
}

fn settle_outcome(
    state: &EngineState,
    capture: &CallCapture,
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
        Ok(output) => Ok(ProcessOutcome {
            text: output.text,
            child_nonzero: output.child_nonzero,
            artifacts,
            limit_exceeded: false,
            error_code: None,
        }),
        Err(error) => {
            let code = if capture.exceeded() {
                CAPTURE_LIMIT_EXCEEDED_CODE.to_owned()
            } else {
                capture
                    .io_failure()
                    .map_or(CAPTURE_IO_FAILED_CODE.to_owned(), |_| {
                        CAPTURE_IO_FAILED_CODE.to_owned()
                    })
            };
            Ok(ProcessOutcome {
                text: error.to_string(),
                child_nonzero: true,
                artifacts,
                limit_exceeded: capture.exceeded(),
                error_code: Some(code.clone()),
            })
        }
    }
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
        settle_outcome(&state, sink.as_ref(), result)
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

    fn take_bash(&self, handle: &str) -> Result<agentshim_core::tools::bash::PreparedBash> {
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
            capture: None,
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
            capture: None,
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
        run_with_capture(
            state,
            &["stdout", "stderr"],
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
            capture: None,
        };
        let ceiling = self.timeout_ceiling_ms;
        let resolver = agentshim_core::tools::exec::resolve::ProcessResolver::capture();
        let timeout = Duration::from_millis(request.timeout_ms(ceiling).min(ceiling));
        let cancellation = self.shutdown.child_token();
        let state = Arc::clone(self);
        run_with_capture(state, &["stdout", "stderr"], move |state, sink| {
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
            capture: None,
            msys_argument_conversion: msys,
        };
        let ceiling = self.timeout_ceiling_ms;
        let timeout = Duration::from_millis(request.timeout_ms(ceiling).min(ceiling));
        let cancellation = self.shutdown.child_token();
        let state = Arc::clone(self);
        run_with_capture(state, &["output"], move |state, sink| {
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
