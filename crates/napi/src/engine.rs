use std::{
    collections::HashMap,
    ffi::c_void,
    future::Future,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use napi::{
    Env, Error, Result, Unknown,
    bindgen_prelude::{ToNapiValue, spawn_blocking},
};
use napi_derive::napi;
use tokio_util::sync::CancellationToken;

use crate::background::{NativeJobHandleResult, start_background_prepared};

use crate::budget::NativeOutputLimits;
use crate::capture::ArtifactRecord;
pub use crate::config::{EnvEntry, NativeHostOptions};
use crate::process::{
    BashArgs, NativeFailure, NativePreparedProcessResult, NativeProcessOutcomeResult, NativeResult,
    NativeVoidResult, ProcessArgs, napi_failure, prepared_result, process_outcome_result,
};
pub use crate::tools::{GlobArgs, GrepArgs, ReadArgs};

use crate::config::NativeEngineConfig;

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
        |env, value| {
            // Safety: the finalize callback runs on the environment thread with
            // a live `env`, per the `execute_tokio_future` contract.
            unsafe { T::to_napi_value(env, value) }
        },
        Some(Box::new(move |_| drop(work))),
    )?;
    // Safety: `promise` was produced by this same `raw_env` on this thread.
    Ok(unsafe { Unknown::from_raw_unchecked(raw_env, promise) })
}

impl Engine {
    pub(crate) fn state(&self) -> Result<Arc<EngineState>> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| Error::new(napi::Status::GenericFailure, "engine is closed"))
    }

    /// Run one operation against the live engine state; a closed engine yields
    /// the host's failure-shaped result instead of throwing.
    fn settled<R>(
        &self,
        tool: &'static str,
        failed: impl FnOnce(NativeFailure) -> Result<R>,
        run: impl FnOnce(Arc<EngineState>) -> Result<R>,
    ) -> Result<R> {
        match self.state() {
            Ok(state) => run(state),
            Err(error) => failed(napi_failure(tool, error)),
        }
    }

    /// Shared body for the two prepare endpoints, which differ only in the
    /// request type they resolve against the prepared-handle registry.
    fn prepare_prepared<F>(
        &self,
        call_id: String,
        prepare: F,
    ) -> Result<NativePreparedProcessResult>
    where
        F: FnOnce(&EngineState, CancellationToken) -> NativeResult<crate::process::PreparedProcess>,
    {
        self.settled(
            "prepare",
            |failure| Ok(prepared_failed(failure)),
            |state| {
                let cancellation = match state.call_token(&call_id) {
                    Ok(token) => token,
                    Err(failure) => return Ok(prepared_failed(failure)),
                };
                Ok(prepared_result(prepare(&state, cancellation)))
            },
        )
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
        let work_settled = poll_until_settled(deadline, Duration::from_millis(10), || {
            engines
                .iter()
                .all(|state| state.native_work.load(Ordering::SeqCst) == 0)
                && backgrounds.iter().all(|job| job.is_settled())
        });
        resources_settled && work_settled
    }
}

unsafe extern "C" fn start_native_cleanup(
    hook: napi::sys::napi_async_cleanup_hook_handle,
    data: *mut c_void,
) {
    // Safety: the data pointer came from `Box::into_raw` in
    // `register_native_cleanup` and Node invokes the hook exactly once.
    let cleanup = unsafe { Box::from_raw(data.cast::<NativeCleanupHook>()) };
    let engines = cleanup.lifetime.begin_cleanup();
    let hook = hook as usize;
    std::thread::spawn(move || {
        assert!(
            cleanup.lifetime.wait_for_cleanup(&engines),
            "native environment cleanup did not quiesce within 10 seconds"
        );
        // Safety: the handle was issued by the runtime that invoked this hook,
        // and removing it from the draining thread is the documented way to
        // complete an async cleanup hook.
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
    // Safety: `env.raw()` belongs to the live environment on this thread, the
    // callback/data pairing matches the registration contract, and the failure
    // path retakes the boxed payload before returning.
    let status = unsafe {
        napi::sys::napi_add_async_cleanup_hook(
            env.raw(),
            Some(start_native_cleanup),
            cleanup.cast(),
            ptr::null_mut(),
        )
    };
    if status != napi::sys::Status::napi_ok {
        // Safety: registration failed, so the hook will never run and the box
        // was not consumed.
        drop(unsafe { Box::from_raw(cleanup) });
        return Err(Error::new(
            napi::Status::GenericFailure,
            format!("failed to register native cleanup: {status:?}"),
        ));
    }
    Ok(())
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
        self.settled(
            "call",
            |f| Ok(void_failed(f)),
            |state| Ok(void_result(state.begin_call(&call_id))),
        )
    }

    #[napi]
    pub fn cancel_call(&self, call_id: String) -> Result<NativeVoidResult> {
        self.settled(
            "call",
            |f| Ok(void_failed(f)),
            |state| Ok(void_result(state.cancel_call(&call_id))),
        )
    }

    #[napi]
    pub fn release_call(&self, call_id: String) -> Result<NativeVoidResult> {
        self.settled(
            "call",
            |f| Ok(void_failed(f)),
            |state| Ok(void_result(state.release_call(&call_id))),
        )
    }

    /// Probe the bash runtime once at load time so a missing GNU bash surfaces
    /// at plugin installation instead of mid-task. The result is cached on the
    /// engine's locator, so the first `bash` tool call reuses it without
    /// re-probing.
    #[napi]
    pub fn verify_bash(&self) -> Result<NativeVoidResult> {
        self.settled(
            "call",
            |f| Ok(void_failed(f)),
            |state| {
                Ok(match state.tool_engine.verify_bash() {
                    Ok(()) => NativeVoidResult {
                        value: true,
                        failure: None,
                    },
                    Err(error) => void_failed(NativeFailure::new(
                        "AGENTSHIM_BASH_UNAVAILABLE",
                        error.to_string(),
                        false,
                        Some(serde_json::json!({ "kind": "preflight" })),
                    )),
                })
            },
        )
    }

    /// Resolve one `run_program` launch to its final argv without spawning, so
    /// the host can wrap that argv in a sandbox before spawning.
    #[napi]
    pub fn prepare_run_program(
        &self,
        call_id: String,
        args: ProcessArgs,
    ) -> Result<NativePreparedProcessResult> {
        self.prepare_prepared(call_id, move |state, cancellation| {
            state.prepare_run_program(args, &cancellation)
        })
    }

    /// Resolve one foreground bash launch to its final argv without spawning.
    #[napi]
    pub fn prepare_bash(
        &self,
        call_id: String,
        args: BashArgs,
    ) -> Result<NativePreparedProcessResult> {
        self.prepare_prepared(call_id, move |state, cancellation| {
            state.prepare_bash(args, &cancellation)
        })
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
        self.settled(
            "spawn",
            move |failure| {
                native_promise(env, detached_native_work(), async move {
                    Ok(outcome_failed(failure))
                })
            },
            move |state| {
                let work = state.start_native_work();
                native_promise(env, work, async move {
                    Ok(process_outcome_result(
                        state
                            .spawn_prepared(call_id, handle, wrapped_argv.as_deref(), attribution)
                            .await,
                    ))
                })
            },
        )
    }

    /// One real core read, computed on the blocking pool against this Engine's
    /// repository capability and page budget.
    #[napi(ts_return_type = "Promise<NativeToolTextResult>")]
    pub fn read_text(&self, env: Env, call_id: String, args: ReadArgs) -> Result<Unknown<'static>> {
        self.tool_text_promise(env, "read", move |state| {
            Self::read_text_inner(state, call_id, args)
        })
    }

    /// One real core grep against this Engine's repository and page budget.
    #[napi(ts_return_type = "Promise<NativeToolTextResult>")]
    pub fn grep_text(&self, env: Env, call_id: String, args: GrepArgs) -> Result<Unknown<'static>> {
        self.tool_text_promise(env, "grep", move |state| {
            Self::grep_text_inner(state, call_id, args)
        })
    }

    /// One real core glob against this Engine's repository and page budget.
    #[napi(ts_return_type = "Promise<NativeToolTextResult>")]
    pub fn glob_text(&self, env: Env, call_id: String, args: GlobArgs) -> Result<Unknown<'static>> {
        self.tool_text_promise(env, "glob", move |state| {
            Self::glob_text_inner(state, call_id, args)
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
        self.settled(
            "background",
            |f| Ok(job_handle_failed(f)),
            |state| match start_background_prepared(
                &state,
                call_id,
                handle,
                wrapped_argv.as_deref(),
            ) {
                Ok(value) => Ok(NativeJobHandleResult {
                    value: Some(value),
                    failure: None,
                }),
                Err(failure) => Ok(job_handle_failed(failure)),
            },
        )
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
                poll_until_settled(
                    std::time::Instant::now() + Duration::from_secs(10),
                    Duration::from_millis(20),
                    || {
                        active.load(Ordering::SeqCst) == 0
                            && native_work.load(Ordering::SeqCst) == 0
                            && backgrounds.iter().all(|job| job.is_settled())
                    },
                )
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
    pub(crate) fn start_native_work(&self) -> NativeWorkGuard {
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

fn void_failed(failure: NativeFailure) -> NativeVoidResult {
    NativeVoidResult {
        value: false,
        failure: Some(failure),
    }
}

fn prepared_failed(failure: NativeFailure) -> NativePreparedProcessResult {
    NativePreparedProcessResult {
        value: None,
        failure: Some(failure),
    }
}

fn outcome_failed(failure: NativeFailure) -> NativeProcessOutcomeResult {
    NativeProcessOutcomeResult {
        value: None,
        failure: Some(failure),
    }
}

fn job_handle_failed(failure: NativeFailure) -> NativeJobHandleResult {
    NativeJobHandleResult {
        value: None,
        failure: Some(failure),
    }
}

/// Poll on a blocking thread until every tracked resource settles or the
/// deadline passes. The per-engine close path and the host-wide cleanup hook
/// share this shape; only their settled predicates and intervals differ.
fn poll_until_settled(
    deadline: std::time::Instant,
    interval: Duration,
    mut settled: impl FnMut() -> bool,
) -> bool {
    while !settled() && std::time::Instant::now() < deadline {
        std::thread::sleep(interval);
    }
    settled()
}
