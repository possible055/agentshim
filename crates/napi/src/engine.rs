use std::{
    collections::HashMap,
    io::{Read as _, Seek as _, SeekFrom},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agentshim_core::output::CallBudget;
use base64::Engine as _;
use napi::{Error, Result, bindgen_prelude::spawn_blocking};
use napi_derive::napi;
use tokio_util::sync::CancellationToken;

use crate::background::{NativeJobHandleResult, start_background_prepared};
use crate::budget::NativeCallBudget;
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
pub struct EngineOptions {
    /// Model-visible page byte budget for one tool response.
    pub page_budget_bytes: Option<u32>,
    /// Repository read scope for this Engine.
    pub read_scope: Option<String>,
    /// DSH tool timeout shelf. The process ceiling is derived below it by the
    /// core cleanup and protocol slack.
    pub tool_timeout_shelf_ms: Option<u32>,
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

#[napi(object)]
pub struct NativeImage {
    pub data: String,
    pub mime_type: String,
}

#[napi(object)]
pub struct ToolText {
    pub text: String,
    pub complete: bool,
    pub images: Vec<NativeImage>,
    pub continuation: Option<NativeContinuation>,
}

#[napi(object)]
pub struct NativeContinuation {
    pub kind: String,
    pub value: String,
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
    pub(crate) resources: agentshim_core::runtime::RuntimeResources,
    pub(crate) budget: NativeCallBudget,
    pub(crate) timeout_ceiling_ms: u64,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) capture_root: std::path::PathBuf,
    pub(crate) capture_max_bytes: u64,
    pub(crate) capture_cleanup_session_end: bool,
    pub(crate) session_key: String,
    pub(crate) locator: agentshim_core::tools::bash::locate::BashLocator,
    pub(crate) artifacts: Arc<std::sync::Mutex<Vec<ArtifactRecord>>>,
    pub(crate) prepared: crate::process::PreparedHandles,
    pub(crate) active_calls: Arc<AtomicUsize>,
    pub(crate) calls: std::sync::Mutex<HashMap<String, CancellationToken>>,
    pub(crate) backgrounds:
        std::sync::Mutex<Vec<std::sync::Weak<crate::background::BackgroundJob>>>,
}

#[napi]
pub struct Engine {
    /// `None` after `close`: dropping the state releases the repository capability,
    /// file access, and runtime resources the engine owned.
    state: std::sync::RwLock<Option<Arc<EngineState>>>,
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
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(state) = state {
            state.shutdown.cancel();
            state.cancel_backgrounds();
        }
    }
}

#[napi]
impl Engine {
    #[napi(constructor)]
    pub fn new(root: String, options: Option<EngineOptions>) -> Result<Self> {
        let options = options.unwrap_or_default();
        #[allow(
            clippy::cast_possible_truncation,
            reason = "u32 page budget widens losslessly to usize on every supported target"
        )]
        let page_budget = options
            .page_budget_bytes
            .map_or_else(crate::budget::default_page_budget, |bytes| bytes as usize);
        let root = Arc::new(
            agentshim_core::path::RepositoryRoot::open(&root)
                .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?,
        );
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
        let access = Arc::new(agentshim_core::path::FileAccess::new(
            Arc::clone(&root),
            read_scope,
        ));
        let mut config = agentshim_core::runtime::RuntimeConfig::for_host_defaults();
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
        config.tool_timeout_shelf = shelf;
        let timeout_ceiling_ms =
            agentshim_core::tools::exec::spawn::max_timeout_ms_from_shelf(shelf);
        let env = options
            .env
            .unwrap_or_default()
            .into_iter()
            .map(|entry| (entry.key, entry.value))
            .collect::<Vec<(String, String)>>();
        // The locator probes at construction time and reads AGENTSHIM_BASH from
        // std::env, which is the host process env rather than the scrubbed child
        // env DSH hands us. Resolve the override from the same env the child will
        // see so plugin config is honored even when the host process never had it.
        let bash_override = env
            .iter()
            .find(|(key, _)| key == agentshim_core::tools::bash::locate::BASH_OVERRIDE_ENV)
            .map(|(_, value)| std::ffi::OsString::from(value));
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
        Ok(Self {
            state: std::sync::RwLock::new(Some(Arc::new(EngineState {
                root,
                access,
                resources: agentshim_core::runtime::RuntimeResources::new(config),
                budget: NativeCallBudget::new(page_budget),
                timeout_ceiling_ms,
                env,
                shutdown: CancellationToken::new(),
                capture_root,
                capture_max_bytes: clamp_capture_ceiling(options.capture_max_bytes),
                capture_cleanup_session_end,
                session_key: uuid::Uuid::new_v4().simple().to_string(),
                locator: agentshim_core::tools::bash::locate::BashLocator::capture_with_override(
                    bash_override,
                ),
                artifacts: Arc::new(std::sync::Mutex::new(Vec::new())),
                prepared: crate::process::PreparedHandles::new(),
                active_calls: Arc::new(AtomicUsize::new(0)),
                calls: std::sync::Mutex::new(HashMap::new()),
                backgrounds: std::sync::Mutex::new(Vec::new()),
            }))),
        })
    }

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
        let result = state.locator.resolve(&state.shutdown);
        Ok(match result {
            Ok(_) => NativeVoidResult {
                value: true,
                failure: None,
            },
            Err(error) => {
                let message = match error {
                    agentshim_core::tools::bash::locate::LocateError::Cancelled => {
                        "bash discovery was cancelled".to_owned()
                    }
                    agentshim_core::tools::bash::locate::LocateError::TimedOut => {
                        "bash discovery timed out".to_owned()
                    }
                    agentshim_core::tools::bash::locate::LocateError::Unavailable(message) => {
                        message.to_string()
                    }
                };
                NativeVoidResult {
                    value: false,
                    failure: Some(NativeFailure::new(
                        "AGENTSHIM_BASH_UNAVAILABLE",
                        message,
                        false,
                        Some(serde_json::json!({ "kind": "preflight" })),
                    )),
                }
            }
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
    pub async fn spawn_prepared(
        &self,
        call_id: String,
        handle: String,
        wrapped_argv: Option<Vec<String>>,
        attribution: Option<crate::classify::SandboxAttribution>,
    ) -> Result<NativeProcessOutcomeResult> {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                return Ok(NativeProcessOutcomeResult {
                    value: None,
                    failure: Some(napi_failure("spawn", error)),
                });
            }
        };
        Ok(process_outcome_result(
            state
                .spawn_prepared(call_id, handle, wrapped_argv.as_deref(), attribution)
                .await,
        ))
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
    pub async fn read_text(&self, call_id: String, args: ReadArgs) -> Result<NativeToolTextResult> {
        match self.read_text_inner(call_id, args).await {
            Ok(value) => Ok(NativeToolTextResult {
                value: Some(value),
                failure: None,
            }),
            Err(error) => Ok(NativeToolTextResult {
                value: None,
                failure: Some(error),
            }),
        }
    }

    async fn read_text_inner(
        &self,
        call_id: String,
        args: ReadArgs,
    ) -> std::result::Result<ToolText, crate::process::NativeFailure> {
        let state = self.state().map_err(|error| napi_failure("read", error))?;
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
        let output = spawn_blocking(move || {
            use agentshim_core::tools::read as read_tool;
            let prepared = read_tool::prepare(
                &access,
                &request,
                &cancellation,
                read_tool::PdfMemoryBudgets::from_config(&state.resources.config()),
            )
            .map_err(read_failure)?;
            let outcome = read_tool::execute_prepared_with_budget(
                &access,
                &request,
                prepared,
                &cancellation,
                &state.budget,
            )
            .map_err(read_failure)?;
            match outcome {
                read_tool::Attempt::Stable(output) => Ok(output),
                read_tool::Attempt::Changed => Err(read_failure(
                    agentshim_core::tools::read::ReadError::Changed,
                )),
            }
        })
        .await
        .map_err(|error| {
            crate::process::NativeFailure::new(
                "AGENTSHIM_NATIVE_THREAD_FAILED",
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "native_thread", "operation": "read" })),
            )
        })??;
        let continuation = continuation_from_text(&output.text);
        Ok(ToolText {
            text: output.text,
            complete: true,
            images: output
                .images
                .into_iter()
                .map(|image| NativeImage {
                    data: image.data,
                    mime_type: image.mime_type.to_owned(),
                })
                .collect(),
            continuation,
        })
    }

    /// One real core grep against this Engine's repository and page budget.
    #[napi(ts_return_type = "Promise<NativeToolTextResult>")]
    pub async fn grep_text(&self, call_id: String, args: GrepArgs) -> Result<NativeToolTextResult> {
        match self.grep_text_inner(call_id, args).await {
            Ok(value) => Ok(NativeToolTextResult {
                value: Some(value),
                failure: None,
            }),
            Err(error) => Ok(NativeToolTextResult {
                value: None,
                failure: Some(error),
            }),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "native grep keeps reservation and blocking execution in one call"
    )]
    async fn grep_text_inner(
        &self,
        call_id: String,
        args: GrepArgs,
    ) -> std::result::Result<ToolText, crate::process::NativeFailure> {
        use agentshim_core::tools::grep;
        let state = self.state().map_err(|error| napi_failure("grep", error))?;
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
        let charge = grep::base_memory_charge(state.resources.config().grep_memory_bytes);
        let permit = state
            .resources
            .reserve_memory(charge, &cancellation)
            .await
            .map_err(|_| {
                if cancellation.is_cancelled() {
                    crate::process::NativeFailure::cancelled("grep")
                } else {
                    crate::process::NativeFailure::new(
                        "AGENTSHIM_RESOURCE_BUSY",
                        "grep memory reservation failed",
                        true,
                        Some(serde_json::json!({ "kind": "resource", "resource": "grep_memory" })),
                    )
                }
            })?;
        let reservation = agentshim_core::runtime::MemoryReservation::from_initial(
            state.resources.clone(),
            permit,
            charge,
        );
        let text = spawn_blocking(move || {
            grep::execute_output_with_budget(
                &access,
                &request,
                &state.resources,
                &cancellation,
                reservation,
                &state.budget,
            )
            .map_err(grep_failure)
            .map(|output| output.text)
        })
        .await
        .map_err(|error| {
            crate::process::NativeFailure::new(
                "AGENTSHIM_NATIVE_THREAD_FAILED",
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "native_thread", "operation": "grep" })),
            )
        })??;
        let continuation = continuation_from_text(&text);
        Ok(ToolText {
            text,
            complete: true,
            images: Vec::new(),
            continuation,
        })
    }

    /// One real core glob against this Engine's repository and page budget.
    #[napi(ts_return_type = "Promise<NativeToolTextResult>")]
    pub async fn glob_text(&self, call_id: String, args: GlobArgs) -> Result<NativeToolTextResult> {
        match self.glob_text_inner(call_id, args).await {
            Ok(value) => Ok(NativeToolTextResult {
                value: Some(value),
                failure: None,
            }),
            Err(error) => Ok(NativeToolTextResult {
                value: None,
                failure: Some(error),
            }),
        }
    }

    async fn glob_text_inner(
        &self,
        call_id: String,
        args: GlobArgs,
    ) -> std::result::Result<ToolText, crate::process::NativeFailure> {
        use agentshim_core::tools::glob;
        let state = self.state().map_err(|error| napi_failure("glob", error))?;
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
        let charge = glob::memory_charge(&request);
        let permit = state
            .resources
            .reserve_memory(charge, &cancellation)
            .await
            .map_err(|_| {
                if cancellation.is_cancelled() {
                    crate::process::NativeFailure::cancelled("glob")
                } else {
                    crate::process::NativeFailure::new(
                        "AGENTSHIM_RESOURCE_BUSY",
                        "glob memory reservation failed",
                        true,
                        Some(serde_json::json!({ "kind": "resource", "resource": "glob_memory" })),
                    )
                }
            })?;
        let reservation = agentshim_core::runtime::MemoryReservation::from_initial(
            state.resources.clone(),
            permit,
            charge,
        );
        let repository_root = state.root.path().to_path_buf();
        let capture_root = state.capture_root.clone();
        let text = spawn_blocking(move || {
            glob::execute_output_with_budget(
                &state.access,
                &request,
                &state.resources,
                &cancellation,
                reservation,
                &state.budget,
            )
            .map_err(glob_failure)
            .map(|output| filter_capture_glob_lines(&output.text, &repository_root, &capture_root))
        })
        .await
        .map_err(|error| {
            crate::process::NativeFailure::new(
                "AGENTSHIM_NATIVE_THREAD_FAILED",
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "native_thread", "operation": "glob" })),
            )
        })??;
        let continuation = continuation_from_text(&text);
        Ok(ToolText {
            text,
            complete: true,
            images: Vec::new(),
            continuation,
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
    pub async fn close(&self) -> Result<NativeVoidResult> {
        match self.close_inner().await {
            Ok(()) => Ok(NativeVoidResult {
                value: true,
                failure: None,
            }),
            Err(error) => Ok(NativeVoidResult {
                value: false,
                failure: Some(napi_failure("close", error)),
            }),
        }
    }

    async fn close_inner(&self) -> Result<()> {
        let mut settled = true;
        if let Ok(state) = self.state() {
            state.shutdown.cancel();
            state.cancel_backgrounds();
            state.prepared.clear();
            let active = Arc::clone(&state.active_calls);
            let backgrounds = state.background_snapshot();
            settled = spawn_blocking(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while (active.load(Ordering::SeqCst) > 0
                    || backgrounds.iter().any(|job| !job.is_settled()))
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                active.load(Ordering::SeqCst) == 0 && backgrounds.iter().all(|job| job.is_settled())
            })
            .await
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
        }
        let dropped = self
            .state
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
        let encoded_budget = self.budget.page_bytes().saturating_sub(wrapper_bytes);
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
        let continuation = continuation_from_text(&text);
        Ok(ToolText {
            text,
            complete: next == record.bytes,
            images: Vec::new(),
            continuation,
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

fn continuation_from_text(text: &str) -> Option<NativeContinuation> {
    let line = text
        .lines()
        .find(|line| line.starts_with("Partial:") || line.starts_with("Retry:"))?;
    for (kind, marker) in [
        ("next_start_line", "next_start_line="),
        ("next_offset", "next_offset="),
        ("next_artifact_offset", "next_artifact_offset="),
    ] {
        if let Some(value) = line
            .strip_prefix("Partial:")
            .and_then(|line| line.trim_start().strip_prefix(marker))
        {
            return Some(NativeContinuation {
                kind: kind.to_owned(),
                value: value.split('.').next()?.to_owned(),
            });
        }
    }
    let marker = "pdf_cursor=\"";
    let start = line.find(marker).map(|index| index + marker.len())?;
    let end = line[start..].find('"')?;
    Some(NativeContinuation {
        kind: "pdf_cursor".to_owned(),
        value: line[start..start + end].to_owned(),
    })
}
