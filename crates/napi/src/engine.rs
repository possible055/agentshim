use std::{
    ffi::c_void,
    io::Write,
    ptr,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use napi::{Env, Error, Result, Unknown, bindgen_prelude::spawn_blocking};
use napi_derive::napi;
use tokio_util::sync::CancellationToken;

use crate::background::{NativeJobHandleResult, start_background_prepared};

use crate::config::NativeEngineConfig;
pub use crate::config::{EnvEntry, NativeHostOptions};
use crate::process::{
    BashArgs, NativeFailure, NativePreparedProcessResult, NativeProcessOutcomeResult, NativeResult,
    NativeVoidResult, ProcessArgs, napi_failure, prepared_result, process_outcome_result,
};
use crate::state::{EngineState, detached_native_work, native_promise};
pub use crate::tools::{GlobArgs, GrepArgs, ReadArgs};

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

#[napi]
pub struct Engine {
    /// `None` after `close`: dropping the state releases the repository capability,
    /// file access, and runtime resources the engine owned.
    state: Arc<std::sync::RwLock<Option<Arc<EngineState>>>>,
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
                .all(|state| state.native_work_count().load(Ordering::SeqCst) == 0)
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
    let drain = std::thread::Builder::new()
        .name("agentshim-native-cleanup".to_owned())
        .spawn(move || {
            let settled = cleanup.lifetime.wait_for_cleanup(&engines);
            complete_native_cleanup(settled, || {
                // Safety: the handle was issued by the runtime that invoked this hook,
                // and removing it from the draining thread is the documented way to
                // complete an async cleanup hook.
                unsafe {
                    napi::sys::napi_remove_async_cleanup_hook(
                        hook as napi::sys::napi_async_cleanup_hook_handle,
                    )
                }
            });
        });
    if let Err(error) = drain {
        complete_native_cleanup(false, || {
            // Safety: the handle was issued by the runtime that invoked this hook,
            // and completing immediately is the only available fail-safe after
            // the cleanup thread could not be created.
            unsafe {
                napi::sys::napi_remove_async_cleanup_hook(
                    hook as napi::sys::napi_async_cleanup_hook_handle,
                )
            }
        });
        write_cleanup_diagnostic(format_args!(
            "agentshim: failed to start native cleanup thread: {error}; outcome uncertain"
        ));
    }
}

fn complete_native_cleanup(settled: bool, remove: impl FnOnce() -> napi::sys::napi_status) {
    let status = remove();
    if !settled {
        write_cleanup_diagnostic(format_args!(
            "agentshim: native environment cleanup did not quiesce within 10 seconds; outcome uncertain"
        ));
    }
    if status != napi::sys::Status::napi_ok {
        write_cleanup_diagnostic(format_args!(
            "agentshim: failed to complete native environment cleanup: {status:?}"
        ));
    }
}

fn write_cleanup_diagnostic(arguments: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr().lock(), "{arguments}");
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
        // The capture root may sit inside the repository; wholesale listing of its
        // contents is suppressed while exact-granted artifacts stay readable.
        let capture_containment = std::fs::canonicalize(&config.capture_root)
            .unwrap_or_else(|_| config.capture_root.clone());
        let access = Arc::new(
            agentshim_core::path::FileAccess::new(Arc::clone(&root), config.read_scope)
                .with_excluded_root(capture_containment),
        );
        let shutdown = CancellationToken::new();
        let resources = agentshim_core::runtime::RuntimeResources::from_capacity(
            Arc::clone(&lifetime.capacity),
            shutdown.clone(),
        );
        let tool_engine = agentshim_core::ToolEngine::new(
            Arc::clone(&root),
            config.read_scope,
            resources.clone(),
        )
        .with_file_access(Arc::clone(&access))
        .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?
        .with_process_environment(config.process_environment.clone());
        let state = Arc::new(EngineState {
            root,
            access,
            tool_engine,
            resources,
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
            active_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            native_work: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            calls: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        attribution: Option<crate::classify::SandboxAttribution>,
    ) -> Result<NativeJobHandleResult> {
        self.settled(
            "background",
            |f| Ok(job_handle_failed(f)),
            |state| match start_background_prepared(
                &state,
                call_id,
                handle,
                wrapped_argv.as_deref(),
                attribution,
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
            let native_work = state.native_work_count();
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

#[cfg(test)]
mod tests {
    use super::complete_native_cleanup;

    #[test]
    fn cleanup_completion_runs_after_an_uncertain_wait() {
        let mut removed = false;
        complete_native_cleanup(false, || {
            removed = true;
            napi::sys::Status::napi_ok
        });
        assert!(removed);
    }
}
