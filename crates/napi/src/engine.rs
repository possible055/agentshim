use std::{
    collections::HashMap,
    ffi::c_void,
    future::Future,
    io::{Read as _, Seek as _, SeekFrom},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agentshim_core::output::CallBudget;
use base64::Engine as _;
use napi::{
    Env, Error, Result, Unknown,
    bindgen_prelude::{ToNapiValue, spawn_blocking},
};
use napi_derive::napi;
use tokio_util::sync::CancellationToken;

use crate::background::{NativeJobHandleResult, start_background_prepared};
use crate::budget::NativeOutputLimits;
use crate::capture::ArtifactRecord;
use crate::process::{
    BashArgs, NativeFailure, NativePreparedProcessResult, NativeProcessOutcomeResult, NativeResult,
    NativeVoidResult, ProcessArgs, clamp_capture_ceiling, napi_failure, prepared_result,
    process_outcome_result,
};

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

struct NativeEngineConfig {
    output_limits: NativeOutputLimits,
    read_scope: agentshim_core::path::ReadScope,
    timeout_ceiling_ms: u64,
    background_timeout_max_ms: u64,
    process_environment: agentshim_core::ProcessEnvironment,
    capture_root: std::path::PathBuf,
    capture_max_bytes: u64,
    capture_cleanup_session_end: bool,
}

#[napi]
pub struct NativeHostRuntime {
    lifetime: Arc<NativeHostLifetime>,
    config: Arc<NativeEngineConfig>,
}

struct NativeHostLifetime {
    capacity: Arc<agentshim_core::runtime::RuntimeCapacity>,
    engines: std::sync::Mutex<Vec<std::sync::Weak<EngineState>>>,
}

struct NativeCleanupHook {
    lifetime: Arc<NativeHostLifetime>,
}

#[napi(object)]
pub struct NativeImage {
    pub data: String,
    pub mime_type: String,
}

#[napi(object)]
pub struct ToolText {
    pub text: String,
    pub images: Vec<NativeImage>,
}

#[napi(object)]
pub struct NativeToolTextResult {
    pub value: Option<ToolText>,
    pub failure: Option<crate::process::NativeFailure>,
}

#[napi(object)]
pub struct ReadArgs {
    pub path: String,
    pub encoding: Option<String>,
    pub start_line: Option<u32>,
    pub line_count: Option<u32>,
    pub pages: Option<String>,
    pub pdf_mode: Option<String>,
    pub pdf_cursor: Option<String>,
    pub artifact_offset: Option<f64>,
}

#[napi(object)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub mode: Option<String>,
    pub fixed_strings: Option<bool>,
    pub case: Option<String>,
    pub context_lines: Option<u32>,
    pub offset: Option<u32>,
    pub limit: Option<u32>,
    pub include_ignored: Option<bool>,
    pub encoding: Option<String>,
    pub fallback_encoding: Option<String>,
}

#[napi(object)]
pub struct GlobArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub include_ignored: Option<bool>,
    pub entry_type: Option<String>,
    pub offset: Option<u32>,
    pub limit: Option<u32>,
}

pub(crate) struct EngineState {
    pub(crate) root: Arc<agentshim_core::path::RepositoryRoot>,
    pub(crate) access: Arc<agentshim_core::path::FileAccess>,
    pub(crate) tool_engine: agentshim_core::ToolEngine,
    pub(crate) output_limits: NativeOutputLimits,
    pub(crate) timeout_ceiling_ms: u64,
    pub(crate) background_timeout_max_ms: u64,
    pub(crate) shutdown: CancellationToken,
    pub(crate) capture_root: std::path::PathBuf,
    pub(crate) capture_max_bytes: u64,
    pub(crate) capture_cleanup_session_end: bool,
    pub(crate) session_key: String,
    pub(crate) artifacts: Arc<std::sync::Mutex<Vec<ArtifactRecord>>>,
    pub(crate) prepared: crate::process::PreparedHandles,
    pub(crate) active_calls: Arc<AtomicUsize>,
    native_work: Arc<AtomicUsize>,
    pub(crate) calls: std::sync::Mutex<HashMap<String, CancellationToken>>,
    pub(crate) backgrounds:
        std::sync::Mutex<Vec<std::sync::Weak<crate::background::BackgroundJob>>>,
}

#[napi]
pub struct Engine {
    /// `None` after `close`: dropping the state releases the repository capability,
    /// file access, and runtime resources the engine owned.
    state: Arc<std::sync::RwLock<Option<Arc<EngineState>>>>,
}

pub(crate) struct NativeWorkGuard(Arc<AtomicUsize>);

impl Drop for NativeWorkGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) fn detached_native_work() -> NativeWorkGuard {
    NativeWorkGuard(Arc::new(AtomicUsize::new(1)))
}

pub(crate) fn native_promise<T, F>(
    env: Env,
    work: NativeWorkGuard,
    future: F,
) -> Result<Unknown<'static>>
where
    T: ToNapiValue + Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    // Keep the pending future independent of the JavaScript class borrow so a
    // Worker teardown can cancel and drain it before releasing the environment.
    let raw_env = env.raw();
    let promise = napi::bindgen_prelude::execute_tokio_future_with_finalize_callback(
        raw_env,
        future,
        |env, value| unsafe { T::to_napi_value(env, value) },
        Some(Box::new(move |_| drop(work))),
    )?;
    Ok(unsafe { Unknown::from_raw_unchecked(raw_env, promise) })
}

impl Engine {
    fn state(&self) -> Result<Arc<EngineState>> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| Error::new(napi::Status::GenericFailure, "engine is closed"))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(state) = state {
            state.shutdown.cancel();
            state.cancel_backgrounds();
        }
    }
}

impl NativeHostLifetime {
    fn register(&self, state: &Arc<EngineState>) {
        let mut engines = self
            .engines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        engines.retain(|engine| engine.strong_count() > 0);
        engines.push(Arc::downgrade(state));
    }

    fn begin_cleanup(&self) -> Vec<Arc<EngineState>> {
        let engines = self
            .engines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect::<Vec<_>>();
        for state in &engines {
            state.shutdown.cancel();
            state.cancel_backgrounds();
            state.prepared.clear();
        }
        engines
    }

    fn wait_for_cleanup(&self, engines: &[Arc<EngineState>]) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let backgrounds = engines
            .iter()
            .flat_map(|state| state.background_snapshot())
            .collect::<Vec<_>>();
        let resources = agentshim_core::runtime::RuntimeResources::from_capacity(
            Arc::clone(&self.capacity),
            CancellationToken::new(),
        );
        let resources_settled = resources.wait_for_quiescence(deadline);
        while (engines
            .iter()
            .any(|state| state.native_work.load(Ordering::SeqCst) > 0)
            || backgrounds.iter().any(|job| !job.is_settled()))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        resources_settled
            && engines
                .iter()
                .all(|state| state.native_work.load(Ordering::SeqCst) == 0)
            && backgrounds.iter().all(|job| job.is_settled())
    }
}

unsafe extern "C" fn start_native_cleanup(
    hook: napi::sys::napi_async_cleanup_hook_handle,
    data: *mut c_void,
) {
    let cleanup = unsafe { Box::from_raw(data.cast::<NativeCleanupHook>()) };
    let engines = cleanup.lifetime.begin_cleanup();
    let hook = hook as usize;
    std::thread::spawn(move || {
        assert!(
            cleanup.lifetime.wait_for_cleanup(&engines),
            "native environment cleanup did not quiesce within 10 seconds"
        );
        let status = unsafe {
            napi::sys::napi_remove_async_cleanup_hook(
                hook as napi::sys::napi_async_cleanup_hook_handle,
            )
        };
        assert_eq!(
            status,
            napi::sys::Status::napi_ok,
            "failed to complete native environment cleanup"
        );
    });
}

fn register_native_cleanup(env: &Env, lifetime: Arc<NativeHostLifetime>) -> Result<()> {
    // napi-rs' safe wrapper completes the hook when its callback returns. This
    // hook must stay pending while cancellation drains native work, so it owns
    // the raw remove handle and completes from the draining thread instead.
    let cleanup = Box::new(NativeCleanupHook { lifetime });
    let cleanup = Box::into_raw(cleanup);
    let status = unsafe {
        napi::sys::napi_add_async_cleanup_hook(
            env.raw(),
            Some(start_native_cleanup),
            cleanup.cast(),
            ptr::null_mut(),
        )
    };
    if status != napi::sys::Status::napi_ok {
        drop(unsafe { Box::from_raw(cleanup) });
        return Err(Error::new(
            napi::Status::GenericFailure,
            format!("failed to register native cleanup: {status:?}"),
        ));
    }
    Ok(())
}

impl NativeEngineConfig {
    fn new(options: NativeHostOptions) -> Result<(Self, agentshim_core::runtime::RuntimeConfig)> {
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

#[napi]
impl NativeHostRuntime {
    #[napi(constructor)]
    pub fn new(env: Env, options: Option<NativeHostOptions>) -> Result<Self> {
        let (config, runtime) = NativeEngineConfig::new(options.unwrap_or_default())?;
        let lifetime = Arc::new(NativeHostLifetime {
            capacity: Arc::new(agentshim_core::runtime::RuntimeCapacity::new(runtime)),
            engines: std::sync::Mutex::new(Vec::new()),
        });
        register_native_cleanup(&env, Arc::clone(&lifetime))?;
        Ok(Self {
            lifetime,
            config: Arc::new(config),
        })
    }

    #[napi]
    pub fn open_engine(&self, root: String) -> Result<Engine> {
        Engine::open(root, Arc::clone(&self.lifetime), Arc::clone(&self.config))
    }
}

impl Engine {
    fn open(
        root: String,
        lifetime: Arc<NativeHostLifetime>,
        config: Arc<NativeEngineConfig>,
    ) -> Result<Self> {
        let root = Arc::new(
            agentshim_core::path::RepositoryRoot::open(&root)
                .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?,
        );
        let access = Arc::new(agentshim_core::path::FileAccess::new(
            Arc::clone(&root),
            config.read_scope,
        ));
        let shutdown = CancellationToken::new();
        let resources = agentshim_core::runtime::RuntimeResources::from_capacity(
            Arc::clone(&lifetime.capacity),
            shutdown.clone(),
        );
        let tool_engine =
            agentshim_core::ToolEngine::new(Arc::clone(&root), config.read_scope, resources)
                .with_process_environment(config.process_environment.clone());
        let state = Arc::new(EngineState {
            root,
            access,
            tool_engine,
            output_limits: config.output_limits.clone(),
            timeout_ceiling_ms: config.timeout_ceiling_ms,
            background_timeout_max_ms: config.background_timeout_max_ms,
            shutdown,
            capture_root: config.capture_root.clone(),
            capture_max_bytes: config.capture_max_bytes,
            capture_cleanup_session_end: config.capture_cleanup_session_end,
            session_key: uuid::Uuid::new_v4().simple().to_string(),
            artifacts: Arc::new(std::sync::Mutex::new(Vec::new())),
            prepared: crate::process::PreparedHandles::new(),
            active_calls: Arc::new(AtomicUsize::new(0)),
            native_work: Arc::new(AtomicUsize::new(0)),
            calls: std::sync::Mutex::new(HashMap::new()),
            backgrounds: std::sync::Mutex::new(Vec::new()),
        });
        lifetime.register(&state);
        Ok(Self {
            state: Arc::new(std::sync::RwLock::new(Some(state))),
        })
    }
}

#[napi]
impl Engine {
    #[napi]
    pub fn begin_call(&self, call_id: String) -> Result<NativeVoidResult> {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                return Ok(NativeVoidResult {
                    value: false,
                    failure: Some(napi_failure("call", error)),
                });
            }
        };
        Ok(void_result(state.begin_call(&call_id)))
    }

    #[napi]
    pub fn cancel_call(&self, call_id: String) -> Result<NativeVoidResult> {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                return Ok(NativeVoidResult {
                    value: false,
                    failure: Some(napi_failure("call", error)),
                });
            }
        };
        Ok(void_result(state.cancel_call(&call_id)))
    }

    #[napi]
    pub fn release_call(&self, call_id: String) -> Result<NativeVoidResult> {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                return Ok(NativeVoidResult {
                    value: false,
                    failure: Some(napi_failure("call", error)),
                });
            }
        };
        Ok(void_result(state.release_call(&call_id)))
    }

    /// Probe the bash runtime once at load time so a missing GNU bash surfaces
    /// at plugin installation instead of mid-task. The result is cached on the
    /// engine's locator, so the first `bash` tool call reuses it without
    /// re-probing.
    #[napi]
    pub fn verify_bash(&self) -> Result<NativeVoidResult> {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                return Ok(NativeVoidResult {
                    value: false,
                    failure: Some(napi_failure("call", error)),
                });
            }
        };
        let result = state.tool_engine.verify_bash();
        Ok(match result {
            Ok(()) => NativeVoidResult {
                value: true,
                failure: None,
            },
            Err(error) => NativeVoidResult {
                value: false,
                failure: Some(NativeFailure::new(
                    "AGENTSHIM_BASH_UNAVAILABLE",
                    error.to_string(),
                    false,
                    Some(serde_json::json!({ "kind": "preflight" })),
                )),
            },
        })
    }

    /// Resolve one `run_program` launch to its final argv without spawning, so
    /// the host can wrap that argv in a sandbox before spawning.
    #[napi]
    pub fn prepare_run_program(
        &self,
        call_id: String,
        args: ProcessArgs,
    ) -> Result<NativePreparedProcessResult> {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                return Ok(NativePreparedProcessResult {
                    value: None,
                    failure: Some(napi_failure("prepare", error)),
                });
            }
        };
        let cancellation = match state.call_token(&call_id) {
            Ok(token) => token,
            Err(failure) => {
                return Ok(NativePreparedProcessResult {
                    value: None,
                    failure: Some(failure),
                });
            }
        };
        Ok(prepared_result(
            state.prepare_run_program(args, &cancellation),
        ))
    }

    /// Resolve one foreground bash launch to its final argv without spawning.
    #[napi]
    pub fn prepare_bash(
        &self,
        call_id: String,
        args: BashArgs,
    ) -> Result<NativePreparedProcessResult> {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                return Ok(NativePreparedProcessResult {
                    value: None,
                    failure: Some(napi_failure("prepare", error)),
                });
            }
        };
        let cancellation = match state.call_token(&call_id) {
            Ok(token) => token,
            Err(failure) => {
                return Ok(NativePreparedProcessResult {
                    value: None,
                    failure: Some(failure),
                });
            }
        };
        Ok(prepared_result(state.prepare_bash(args, &cancellation)))
    }

    /// Spawn one prepared launch. `wrapped_argv` replaces the prepared argv
    /// wholesale when a sandbox wrapped it; `None` runs the resolved argv as-is.
    /// `attribution` classifies the settled outcome against the sandbox
    /// backend's denial dialect and runner-failure rules.
    #[napi(ts_return_type = "Promise<NativeProcessOutcomeResult>")]
    pub fn spawn_prepared(
        &self,
        env: Env,
        call_id: String,
        handle: String,
        wrapped_argv: Option<Vec<String>>,
        attribution: Option<crate::classify::SandboxAttribution>,
    ) -> Result<Unknown<'static>> {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                let result = NativeProcessOutcomeResult {
                    value: None,
                    failure: Some(napi_failure("spawn", error)),
                };
                return native_promise(env, detached_native_work(), async move { Ok(result) });
            }
        };
        let work = state.start_native_work();
        native_promise(env, work, async move {
            Ok(process_outcome_result(
                state
                    .spawn_prepared(call_id, handle, wrapped_argv.as_deref(), attribution)
                    .await,
            ))
        })
    }

    fn pdf_mode(value: Option<&str>) -> Result<Option<agentshim_core::tools::read::PdfMode>> {
        match value {
            None => Ok(None),
            Some("auto") => Ok(Some(agentshim_core::tools::read::PdfMode::Auto)),
            Some("text") => Ok(Some(agentshim_core::tools::read::PdfMode::Text)),
            Some("image") => Ok(Some(agentshim_core::tools::read::PdfMode::Image)),
            Some(other) => Err(Error::new(
                napi::Status::InvalidArg,
                format!("pdf_mode must be auto, text, or image, got {other}"),
            )),
        }
    }

    /// One real core read, computed on the blocking pool against this Engine's
    /// repository capability and page budget.
    #[napi(ts_return_type = "Promise<NativeToolTextResult>")]
    pub fn read_text(&self, env: Env, call_id: String, args: ReadArgs) -> Result<Unknown<'static>> {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                let result = NativeToolTextResult {
                    value: None,
                    failure: Some(napi_failure("read", error)),
                };
                return native_promise(env, detached_native_work(), async move { Ok(result) });
            }
        };
        let work = state.start_native_work();
        native_promise(env, work, async move {
            Ok(match Self::read_text_inner(state, call_id, args).await {
                Ok(value) => NativeToolTextResult {
                    value: Some(value),
                    failure: None,
                },
                Err(error) => NativeToolTextResult {
                    value: None,
                    failure: Some(error),
                },
            })
        })
    }

    async fn read_text_inner(
        state: Arc<EngineState>,
        call_id: String,
        args: ReadArgs,
    ) -> std::result::Result<ToolText, crate::process::NativeFailure> {
        let cancellation = state.call_token(&call_id)?;
        let artifact = state.artifact(&args.path);
        if artifact.is_none() && state.is_capture_path(&args.path) {
            return Err(crate::process::NativeFailure::invalid(
                "capture files require an exact artifact capability from this Engine",
            ));
        }
        if args.artifact_offset.is_some() && artifact.is_none() {
            return Err(crate::process::NativeFailure::invalid(
                "artifactOffset applies only to a published native artifact",
            ));
        }
        if let Some(record) = artifact.as_ref()
            && (!record.valid_text || args.artifact_offset.is_some())
        {
            return state.read_artifact_page(record, args.artifact_offset);
        }
        let access = if let Some(record) = artifact.as_ref() {
            Arc::new(
                state
                    .access
                    .with_exact_grant(&record.path)
                    .map_err(|error| {
                        crate::process::NativeFailure::new(
                            "AGENTSHIM_READ_PATH_FAILED",
                            error.to_string(),
                            false,
                            Some(serde_json::json!({ "kind": "path" })),
                        )
                    })?,
            )
        } else {
            Arc::clone(&state.access)
        };
        let tool_engine = state
            .tool_engine
            .with_file_access(access)
            .map_err(|error| {
                crate::process::NativeFailure::new(
                    "AGENTSHIM_READ_PATH_FAILED",
                    error.to_string(),
                    false,
                    Some(serde_json::json!({ "kind": "path" })),
                )
            })?;
        let request = agentshim_core::tools::read::ReadRequest {
            path: args.path,
            start_line: args.start_line.map(|line| line as usize),
            line_count: args.line_count.map(|count| count as usize),
            encoding: args.encoding,
            pdf_mode: Self::pdf_mode(args.pdf_mode.as_deref())
                .map_err(|error| napi_failure("read", error))?,
            pages: args.pages,
            pdf_cursor: args.pdf_cursor,
        };
        let output = tool_engine
            .read(
                request,
                agentshim_core::OperationContext::new(
                    cancellation,
                    Arc::new(state.output_limits.clone()),
                ),
            )
            .await
            .map_err(read_failure)?;
        Ok(ToolText {
            text: output.text,
            images: output
                .images
                .into_iter()
                .map(|image| NativeImage {
                    data: image.data,
                    mime_type: image.mime_type.to_owned(),
                })
                .collect(),
        })
    }

    /// One real core grep against this Engine's repository and page budget.
    #[napi(ts_return_type = "Promise<NativeToolTextResult>")]
    pub fn grep_text(&self, env: Env, call_id: String, args: GrepArgs) -> Result<Unknown<'static>> {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                let result = NativeToolTextResult {
                    value: None,
                    failure: Some(napi_failure("grep", error)),
                };
                return native_promise(env, detached_native_work(), async move { Ok(result) });
            }
        };
        let work = state.start_native_work();
        native_promise(env, work, async move {
            Ok(match Self::grep_text_inner(state, call_id, args).await {
                Ok(value) => NativeToolTextResult {
                    value: Some(value),
                    failure: None,
                },
                Err(error) => NativeToolTextResult {
                    value: None,
                    failure: Some(error),
                },
            })
        })
    }

    async fn grep_text_inner(
        state: Arc<EngineState>,
        call_id: String,
        args: GrepArgs,
    ) -> std::result::Result<ToolText, crate::process::NativeFailure> {
        use agentshim_core::tools::grep;
        let cancellation = state.call_token(&call_id)?;
        let artifact = args.path.as_deref().and_then(|path| state.artifact(path));
        if artifact.is_none()
            && args
                .path
                .as_deref()
                .is_some_and(|path| state.is_capture_path(path))
        {
            return Err(crate::process::NativeFailure::invalid(
                "capture files require an exact artifact capability from this Engine",
            ));
        }
        if let Some(record) = artifact.as_ref() {
            if args.glob.is_some() {
                return Err(crate::process::NativeFailure::invalid(
                    "artifact grep requires one exact file path and no glob",
                ));
            }
            if !record.valid_text {
                return Err(crate::process::NativeFailure::invalid(
                    "binary artifact cannot be searched as text; retry read with artifactOffset",
                ));
            }
        }
        let access = if let Some(record) = artifact.as_ref() {
            Arc::new(
                state
                    .access
                    .with_exact_grant(&record.path)
                    .map_err(|error| {
                        crate::process::NativeFailure::new(
                            "AGENTSHIM_GREP_PATH_FAILED",
                            error.to_string(),
                            false,
                            Some(serde_json::json!({ "kind": "path" })),
                        )
                    })?,
            )
        } else {
            Arc::clone(&state.access)
        };
        let tool_engine = state
            .tool_engine
            .with_file_access(access)
            .map_err(|error| {
                crate::process::NativeFailure::new(
                    "AGENTSHIM_GREP_PATH_FAILED",
                    error.to_string(),
                    false,
                    Some(serde_json::json!({ "kind": "path" })),
                )
            })?;
        let mode =
            parse_grep_mode(args.mode.as_deref()).map_err(|error| napi_failure("grep", error))?;
        let case =
            parse_grep_case(args.case.as_deref()).map_err(|error| napi_failure("grep", error))?;
        let request = grep::GrepRequest {
            pattern: args.pattern,
            path: args.path,
            glob: args.glob,
            mode,
            fixed_strings: args.fixed_strings,
            case,
            context_lines: args.context_lines.map(|lines| lines as usize),
            offset: args.offset.map(|offset| offset as usize),
            limit: args.limit.map(|limit| limit as usize),
            include_ignored: args.include_ignored,
            encoding: args.encoding,
            fallback_encoding: args.fallback_encoding,
        };
        let text = tool_engine
            .grep(
                request,
                agentshim_core::OperationContext::new(
                    cancellation,
                    Arc::new(state.output_limits.clone()),
                ),
            )
            .await
            .map_err(grep_failure)?
            .text;
        Ok(ToolText {
            text,
            images: Vec::new(),
        })
    }

    /// One real core glob against this Engine's repository and page budget.
    #[napi(ts_return_type = "Promise<NativeToolTextResult>")]
    pub fn glob_text(&self, env: Env, call_id: String, args: GlobArgs) -> Result<Unknown<'static>> {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                let result = NativeToolTextResult {
                    value: None,
                    failure: Some(napi_failure("glob", error)),
                };
                return native_promise(env, detached_native_work(), async move { Ok(result) });
            }
        };
        let work = state.start_native_work();
        native_promise(env, work, async move {
            Ok(match Self::glob_text_inner(state, call_id, args).await {
                Ok(value) => NativeToolTextResult {
                    value: Some(value),
                    failure: None,
                },
                Err(error) => NativeToolTextResult {
                    value: None,
                    failure: Some(error),
                },
            })
        })
    }

    async fn glob_text_inner(
        state: Arc<EngineState>,
        call_id: String,
        args: GlobArgs,
    ) -> std::result::Result<ToolText, crate::process::NativeFailure> {
        use agentshim_core::tools::glob;
        let cancellation = state.call_token(&call_id)?;
        if args
            .path
            .as_deref()
            .is_some_and(|path| state.is_capture_path(path))
        {
            return Err(crate::process::NativeFailure::invalid(
                "glob cannot enumerate the capture root",
            ));
        }
        let entry_type = match args.entry_type.as_deref() {
            None => None,
            Some("file") => Some(agentshim_core::tools::glob::GlobEntryType::File),
            Some("directory") => Some(agentshim_core::tools::glob::GlobEntryType::Directory),
            Some("any") => Some(agentshim_core::tools::glob::GlobEntryType::Any),
            Some(other) => {
                return Err(crate::process::NativeFailure::invalid(format!(
                    "type must be file, directory, or any, got {other}"
                )));
            }
        };
        let request = glob::GlobRequest {
            pattern: args.pattern,
            path: args.path,
            include_ignored: args.include_ignored,
            entry_type,
            offset: args.offset.map(|offset| offset as usize),
            limit: args.limit.map(|limit| limit as usize),
        };
        let repository_root = state.root.path().to_path_buf();
        let capture_root = state.capture_root.clone();
        let output = state
            .tool_engine
            .glob(
                request,
                agentshim_core::OperationContext::new(
                    cancellation,
                    Arc::new(state.output_limits.clone()),
                ),
            )
            .await
            .map_err(glob_failure)?;
        let text = filter_capture_glob_lines(&output.text, &repository_root, &capture_root);
        Ok(ToolText {
            text,
            images: Vec::new(),
        })
    }

    /// Spawn one prepared bash launch as a background job. The spawn is
    /// synchronous: a failure to launch throws immediately and no handle is
    /// returned. `wrapped_argv` replaces the prepared argv when a sandbox wrapped
    /// it; `None` runs the resolved argv as-is. The handle owns the process tree,
    /// durable capture, and live buffer.
    #[napi]
    pub fn start_background_prepared(
        &self,
        call_id: String,
        handle: String,
        wrapped_argv: Option<Vec<String>>,
    ) -> Result<NativeJobHandleResult> {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                return Ok(NativeJobHandleResult {
                    value: None,
                    failure: Some(napi_failure("background", error)),
                });
            }
        };
        match start_background_prepared(&state, call_id, handle, wrapped_argv.as_deref()) {
            Ok(value) => Ok(NativeJobHandleResult {
                value: Some(value),
                failure: None,
            }),
            Err(failure) => Ok(NativeJobHandleResult {
                value: None,
                failure: Some(failure),
            }),
        }
    }

    /// Stop admission, cancel foreground and background work, and await settlement.
    /// Async, idempotent, and safe to call from any Engine state.
    #[napi(ts_return_type = "Promise<NativeVoidResult>")]
    pub fn close(&self, env: Env) -> Result<Unknown<'static>> {
        let state = Arc::clone(&self.state);
        native_promise(env, detached_native_work(), async move {
            Ok(match Self::close_state(&state).await {
                Ok(()) => NativeVoidResult {
                    value: true,
                    failure: None,
                },
                Err(error) => NativeVoidResult {
                    value: false,
                    failure: Some(napi_failure("close", error)),
                },
            })
        })
    }

    async fn close_state(state_owner: &std::sync::RwLock<Option<Arc<EngineState>>>) -> Result<()> {
        let mut settled = true;
        let state = {
            state_owner
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        };
        if let Some(state) = state {
            state.shutdown.cancel();
            state.cancel_backgrounds();
            state.prepared.clear();
            let active = Arc::clone(&state.active_calls);
            let native_work = Arc::clone(&state.native_work);
            let backgrounds = state.background_snapshot();
            settled = spawn_blocking(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while (active.load(Ordering::SeqCst) > 0
                    || native_work.load(Ordering::SeqCst) > 0
                    || backgrounds.iter().any(|job| !job.is_settled()))
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                active.load(Ordering::SeqCst) == 0
                    && native_work.load(Ordering::SeqCst) == 0
                    && backgrounds.iter().all(|job| job.is_settled())
            })
            .await
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
        }
        let dropped = state_owner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(state) = dropped {
            let cleanup_error = if settled && state.capture_cleanup_session_end {
                let session = state.capture_root.join(&state.session_key);
                std::fs::remove_dir_all(session)
                    .err()
                    .filter(|error| error.kind() != std::io::ErrorKind::NotFound)
            } else {
                None
            };
            spawn_blocking(move || drop(state))
                .await
                .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
            if let Some(error) = cleanup_error {
                return Err(Error::new(
                    napi::Status::GenericFailure,
                    format!("AGENTSHIM_CAPTURE_CLEANUP_FAILED: {error}"),
                ));
            }
        }
        if !settled {
            return Err(Error::new(
                napi::Status::GenericFailure,
                "AGENTSHIM_TEARDOWN_TIMEOUT: native work did not settle within 10 seconds",
            ));
        }
        Ok(())
    }
}

impl EngineState {
    fn start_native_work(&self) -> NativeWorkGuard {
        self.native_work.fetch_add(1, Ordering::SeqCst);
        NativeWorkGuard(Arc::clone(&self.native_work))
    }

    pub(crate) fn begin_call(&self, call_id: &str) -> NativeResult<()> {
        if call_id.is_empty() {
            return NativeResult::failure(crate::process::NativeFailure::invalid(
                "native call id must not be empty",
            ));
        }
        if self.shutdown.is_cancelled() {
            return NativeResult::failure(crate::process::NativeFailure::engine_closed());
        }
        let mut calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if calls.contains_key(call_id) {
            return NativeResult::failure(crate::process::NativeFailure::new(
                "AGENTSHIM_CALL_ALREADY_ACTIVE",
                "native call id is already active",
                false,
                Some(serde_json::json!({ "callId": call_id })),
            ));
        }
        calls.insert(call_id.to_owned(), self.shutdown.child_token());
        self.active_calls.fetch_add(1, Ordering::SeqCst);
        NativeResult::success(())
    }

    pub(crate) fn call_token(
        &self,
        call_id: &str,
    ) -> std::result::Result<CancellationToken, crate::process::NativeFailure> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(call_id)
            .cloned()
            .ok_or_else(|| {
                crate::process::NativeFailure::new(
                    "AGENTSHIM_CALL_INVALID",
                    "native call id is not active",
                    false,
                    Some(serde_json::json!({ "callId": call_id })),
                )
            })
    }

    pub(crate) fn cancel_call(&self, call_id: &str) -> NativeResult<bool> {
        let token = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(call_id)
            .cloned();
        if let Some(token) = token {
            token.cancel();
            NativeResult::success(true)
        } else {
            NativeResult::success(false)
        }
    }

    pub(crate) fn release_call(&self, call_id: &str) -> NativeResult<bool> {
        let removed = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(call_id)
            .is_some();
        if removed {
            self.active_calls.fetch_sub(1, Ordering::SeqCst);
        }
        NativeResult::success(removed)
    }

    pub(crate) fn artifact(&self, requested: &str) -> Option<ArtifactRecord> {
        let requested = std::fs::canonicalize(requested).ok()?;
        self.artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|record| {
                std::fs::canonicalize(&record.path).is_ok_and(|published| published == requested)
            })
            .cloned()
    }

    fn is_capture_path(&self, requested: &str) -> bool {
        let requested = std::path::Path::new(requested);
        let absolute = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.path().join(requested)
        };
        std::fs::canonicalize(absolute).is_ok_and(|path| path.starts_with(&self.capture_root))
    }

    fn read_artifact_page(
        &self,
        record: &ArtifactRecord,
        offset: Option<f64>,
    ) -> std::result::Result<ToolText, crate::process::NativeFailure> {
        let offset = offset.unwrap_or(0.0);
        if !offset.is_finite() || offset < 0.0 || offset.fract() != 0.0 {
            return Err(crate::process::NativeFailure::invalid(
                "artifactOffset must be a non-negative integer",
            ));
        }
        let offset = offset as u64;
        if offset > record.bytes {
            return Err(crate::process::NativeFailure::invalid(
                "artifactOffset is beyond the artifact snapshot",
            ));
        }
        let metadata = std::fs::symlink_metadata(&record.path).map_err(|error| {
            crate::process::NativeFailure::new(
                "AGENTSHIM_READ_IO_FAILED",
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "io", "operation": "artifact_metadata" })),
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(crate::process::NativeFailure::new(
                "AGENTSHIM_READ_TARGET_INVALID",
                "published artifact is no longer a regular file",
                false,
                Some(serde_json::json!({ "kind": "artifact" })),
            ));
        }
        let wrapper_bytes = 512_usize;
        let encoded_budget = self
            .output_limits
            .page_bytes()
            .saturating_sub(wrapper_bytes);
        let raw_budget = encoded_budget / 4 * 3;
        let remaining = record.bytes.saturating_sub(offset);
        let to_read = remaining.min(raw_budget as u64) as usize;
        let mut file = std::fs::File::open(&record.path).map_err(|error| {
            crate::process::NativeFailure::new(
                "AGENTSHIM_READ_IO_FAILED",
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "io", "operation": "artifact_open" })),
            )
        })?;
        file.seek(SeekFrom::Start(offset)).map_err(|error| {
            crate::process::NativeFailure::new(
                "AGENTSHIM_READ_IO_FAILED",
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "io", "operation": "artifact_seek" })),
            )
        })?;
        let mut bytes = vec![0_u8; to_read];
        file.read_exact(&mut bytes).map_err(|error| {
            crate::process::NativeFailure::new(
                "AGENTSHIM_READ_IO_FAILED",
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "io", "operation": "artifact_read" })),
            )
        })?;
        let next = offset.saturating_add(to_read as u64);
        let mut text = format!(
            "Artifact: {}\nByte range: {offset}..{next} of {}\nEncoding: base64\nOutput:\n{}",
            record.path.display(),
            record.bytes,
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        if next < record.bytes {
            use std::fmt::Write as _;
            write!(text, "\nPartial: next_artifact_offset={next}.")
                .expect("writing to a String cannot fail");
        }
        Ok(ToolText {
            text,
            images: Vec::new(),
        })
    }

    pub(crate) fn cancel_backgrounds(&self) {
        for job in self.background_snapshot() {
            job.cancel_from_engine();
        }
    }

    pub(crate) fn background_snapshot(&self) -> Vec<Arc<crate::background::BackgroundJob>> {
        self.backgrounds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect()
    }
}

fn void_result<T>(result: NativeResult<T>) -> NativeVoidResult {
    let failure = result.failure;
    NativeVoidResult {
        value: failure.is_none(),
        failure,
    }
}

fn read_failure(error: agentshim_core::tools::read::ReadError) -> crate::process::NativeFailure {
    use agentshim_core::tools::read::ReadError;

    match error {
        ReadError::Validation(message) => crate::process::NativeFailure::new(
            "INVALID_ARGS",
            message,
            false,
            Some(serde_json::json!({ "kind": "validation" })),
        ),
        ReadError::ResourceLimit {
            message,
            resource,
            limit_bytes,
            observed_bytes,
        } => crate::process::NativeFailure::new(
            "AGENTSHIM_READ_RESOURCE_LIMIT",
            message,
            false,
            Some(serde_json::json!({
                "kind": "resource",
                "resource": resource,
                "limitBytes": limit_bytes,
                "observedBytes": observed_bytes,
            })),
        ),
        ReadError::Cancelled => crate::process::NativeFailure::cancelled("read"),
        ReadError::ResourceBusy { resource, .. } => crate::process::NativeFailure::new(
            "AGENTSHIM_RESOURCE_BUSY",
            format!("read resource {resource} is busy"),
            true,
            Some(serde_json::json!({ "kind": "resource", "resource": resource })),
        ),
        ReadError::ResourceTimeout { limit, elapsed } => crate::process::NativeFailure::new(
            "AGENTSHIM_TIMEOUT",
            "PDF read exceeded its mode runtime limit",
            true,
            Some(serde_json::json!({
                "kind": "timeout",
                "operation": "read",
                "limitMs": u64::try_from(limit.as_millis()).unwrap_or(u64::MAX),
                "elapsedMs": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                "workStopped": true,
            })),
        ),
        ReadError::Worker(message) => crate::process::NativeFailure::new(
            "AGENTSHIM_NATIVE_THREAD_FAILED",
            message,
            true,
            Some(serde_json::json!({ "kind": "native_thread", "operation": "read" })),
        ),
        ReadError::Changed => crate::process::NativeFailure::new(
            "AGENTSHIM_FILE_CHANGED",
            "file changed during read",
            true,
            Some(serde_json::json!({ "kind": "consistency" })),
        ),
        error @ (ReadError::Directory | ReadError::NotRegular | ReadError::Binary) => {
            crate::process::NativeFailure::new(
                "AGENTSHIM_READ_TARGET_INVALID",
                error.to_string(),
                false,
                Some(serde_json::json!({ "kind": "target" })),
            )
        }
        ReadError::PdfImageRequired { pages, cursor } => crate::process::NativeFailure::new(
            "AGENTSHIM_PDF_IMAGE_REQUIRED",
            "selected PDF pages require image mode",
            false,
            Some(serde_json::json!({ "kind": "pdf", "pages": pages, "cursor": cursor })),
        ),
        ReadError::Path(error) => crate::process::NativeFailure::new(
            "AGENTSHIM_READ_PATH_FAILED",
            error.to_string(),
            false,
            Some(serde_json::json!({ "kind": "path" })),
        ),
        ReadError::Io(error) => crate::process::NativeFailure::new(
            "AGENTSHIM_READ_IO_FAILED",
            error.to_string(),
            true,
            Some(serde_json::json!({ "kind": "io", "ioKind": format!("{:?}", error.kind()) })),
        ),
        other => crate::process::NativeFailure::new(
            "AGENTSHIM_READ_FAILED",
            other.to_string(),
            true,
            Some(serde_json::json!({ "kind": "read" })),
        ),
    }
}

fn grep_failure(error: agentshim_core::tools::grep::GrepError) -> crate::process::NativeFailure {
    use agentshim_core::tools::grep::GrepError;

    match error {
        GrepError::Validation(message) => crate::process::NativeFailure::new(
            "INVALID_ARGS",
            message,
            false,
            Some(serde_json::json!({ "kind": "validation" })),
        ),
        GrepError::Regex(message) | GrepError::Glob(message) => crate::process::NativeFailure::new(
            "AGENTSHIM_GREP_PATTERN_INVALID",
            message,
            false,
            Some(serde_json::json!({ "kind": "pattern" })),
        ),
        error @ GrepError::CandidateMemory => crate::process::NativeFailure::new(
            "AGENTSHIM_GREP_RESOURCE_LIMIT",
            error.to_string(),
            false,
            Some(serde_json::json!({ "kind": "resource", "resource": "candidate_memory" })),
        ),
        error @ GrepError::MemoryBusy => crate::process::NativeFailure::new(
            "AGENTSHIM_RESOURCE_BUSY",
            error.to_string(),
            true,
            Some(serde_json::json!({ "kind": "resource", "resource": "grep_memory" })),
        ),
        GrepError::Cancelled => crate::process::NativeFailure::cancelled("grep"),
        GrepError::ResourceBusy(resource) => crate::process::NativeFailure::new(
            "AGENTSHIM_RESOURCE_BUSY",
            format!("grep resource {resource} is busy"),
            true,
            Some(serde_json::json!({ "kind": "resource", "resource": resource })),
        ),
        GrepError::Worker(message) => crate::process::NativeFailure::new(
            "AGENTSHIM_NATIVE_THREAD_FAILED",
            message,
            true,
            Some(serde_json::json!({ "kind": "native_thread", "operation": "grep" })),
        ),
        GrepError::Path(error) => crate::process::NativeFailure::new(
            "AGENTSHIM_GREP_PATH_FAILED",
            error.to_string(),
            false,
            Some(serde_json::json!({ "kind": "path" })),
        ),
        GrepError::Io(error) => crate::process::NativeFailure::new(
            "AGENTSHIM_GREP_IO_FAILED",
            error.to_string(),
            true,
            Some(serde_json::json!({ "kind": "io", "ioKind": format!("{:?}", error.kind()) })),
        ),
        other => crate::process::NativeFailure::new(
            "AGENTSHIM_GREP_FAILED",
            other.to_string(),
            true,
            Some(serde_json::json!({ "kind": "grep" })),
        ),
    }
}

fn glob_failure(error: agentshim_core::tools::glob::GlobError) -> crate::process::NativeFailure {
    use agentshim_core::tools::glob::GlobError;

    match error {
        GlobError::Validation(message) | GlobError::Pattern(message) => {
            crate::process::NativeFailure::new(
                "INVALID_ARGS",
                message,
                false,
                Some(serde_json::json!({ "kind": "pattern" })),
            )
        }
        error @ GlobError::Memory => crate::process::NativeFailure::new(
            "AGENTSHIM_GLOB_RESOURCE_LIMIT",
            error.to_string(),
            false,
            Some(serde_json::json!({ "kind": "resource", "resource": "glob_memory" })),
        ),
        error @ GlobError::MemoryBusy => crate::process::NativeFailure::new(
            "AGENTSHIM_RESOURCE_BUSY",
            error.to_string(),
            true,
            Some(serde_json::json!({ "kind": "resource", "resource": "glob_memory" })),
        ),
        GlobError::ResourceBusy(resource) => crate::process::NativeFailure::new(
            "AGENTSHIM_RESOURCE_BUSY",
            format!("glob resource {resource} is busy"),
            true,
            Some(serde_json::json!({ "kind": "resource", "resource": resource })),
        ),
        GlobError::Cancelled => crate::process::NativeFailure::cancelled("glob"),
        GlobError::Worker(message) => crate::process::NativeFailure::new(
            "AGENTSHIM_NATIVE_THREAD_FAILED",
            message,
            true,
            Some(serde_json::json!({ "kind": "native_thread", "operation": "glob" })),
        ),
        GlobError::Path(error) => crate::process::NativeFailure::new(
            "AGENTSHIM_GLOB_PATH_FAILED",
            error.to_string(),
            false,
            Some(serde_json::json!({ "kind": "path" })),
        ),
        GlobError::Io(error) => crate::process::NativeFailure::new(
            "AGENTSHIM_GLOB_IO_FAILED",
            error.to_string(),
            true,
            Some(serde_json::json!({ "kind": "io", "ioKind": format!("{:?}", error.kind()) })),
        ),
        other => crate::process::NativeFailure::new(
            "AGENTSHIM_GLOB_FAILED",
            other.to_string(),
            true,
            Some(serde_json::json!({ "kind": "glob" })),
        ),
    }
}

fn parse_grep_mode(value: Option<&str>) -> Result<Option<agentshim_core::tools::grep::GrepMode>> {
    use agentshim_core::tools::grep::GrepMode;

    match value {
        None => Ok(None),
        Some("content") => Ok(Some(GrepMode::Content)),
        Some("files") => Ok(Some(GrepMode::Files)),
        Some("count") => Ok(Some(GrepMode::Count)),
        Some(other) => Err(Error::new(
            napi::Status::InvalidArg,
            format!("mode must be content, files, or count, got {other}"),
        )),
    }
}

fn parse_grep_case(value: Option<&str>) -> Result<Option<agentshim_core::tools::grep::CaseMode>> {
    use agentshim_core::tools::grep::CaseMode;

    match value {
        None => Ok(None),
        Some("smart") => Ok(Some(CaseMode::Smart)),
        Some("sensitive") => Ok(Some(CaseMode::Sensitive)),
        Some("insensitive") => Ok(Some(CaseMode::Insensitive)),
        Some(other) => Err(Error::new(
            napi::Status::InvalidArg,
            format!("case must be smart, sensitive, or insensitive, got {other}"),
        )),
    }
}

fn filter_capture_glob_lines(
    text: &str,
    repository_root: &std::path::Path,
    capture_root: &std::path::Path,
) -> String {
    text.lines()
        .filter(|line| {
            let candidate = std::path::Path::new(line);
            if line.starts_with("Partial:") || line.starts_with("Retry:") {
                return true;
            }
            let absolute = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                repository_root.join(candidate)
            };
            match std::fs::canonicalize(absolute) {
                Ok(path) => !path.starts_with(capture_root),
                Err(_) => true,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
