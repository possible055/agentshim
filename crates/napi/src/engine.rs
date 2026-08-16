use std::{
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

use crate::background::{EngineJobHandle, start_background_prepared};
use crate::budget::NativeCallBudget;
use crate::capture::ArtifactRecord;
use crate::process::{
    BashArgs, PreparedProcess, ProcessArgs, ProcessOutcome, clamp_capture_ceiling,
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
    pub(crate) backgrounds:
        std::sync::Mutex<Vec<std::sync::Weak<crate::background::BackgroundJob>>>,
}

pub(crate) struct ActiveCall {
    count: Arc<AtomicUsize>,
}

impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
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
            .collect();
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
                locator: agentshim_core::tools::bash::locate::BashLocator::capture(),
                artifacts: Arc::new(std::sync::Mutex::new(Vec::new())),
                prepared: crate::process::PreparedHandles::new(),
                active_calls: Arc::new(AtomicUsize::new(0)),
                backgrounds: std::sync::Mutex::new(Vec::new()),
            }))),
        })
    }

    /// Resolve one `run_program` launch to its final argv without spawning, so
    /// the host can wrap that argv in a sandbox before spawning.
    #[napi]
    pub fn prepare_run_program(&self, args: ProcessArgs) -> Result<PreparedProcess> {
        let state = self.state()?;
        state.prepare_run_program(args)
    }

    /// Resolve one foreground bash launch to its final argv without spawning.
    #[napi]
    pub fn prepare_bash(&self, args: BashArgs) -> Result<PreparedProcess> {
        let state = self.state()?;
        state.prepare_bash(args)
    }

    /// Spawn one prepared launch. `wrapped_argv` replaces the prepared argv
    /// wholesale when a sandbox wrapped it; `None` runs the resolved argv as-is.
    /// `attribution` classifies the settled outcome against the sandbox
    /// backend's denial dialect and runner-failure rules.
    #[napi(ts_return_type = "Promise<ProcessOutcome>")]
    pub async fn spawn_prepared(
        &self,
        handle: String,
        wrapped_argv: Option<Vec<String>>,
        attribution: Option<crate::classify::SandboxAttribution>,
    ) -> Result<ProcessOutcome> {
        let state = self.state()?;
        state
            .spawn_prepared(handle, wrapped_argv.as_deref(), attribution)
            .await
    }

    /// One foreground `run_program` with a bounded model preview and durable
    /// capture artifacts; the model never receives raw capture bytes.
    #[napi(ts_return_type = "Promise<ProcessOutcome>")]
    pub async fn run_program_text(&self, args: ProcessArgs) -> Result<ProcessOutcome> {
        let state = self.state()?;
        state.run_program_outcome(args).await
    }

    /// One foreground bash command with the same preview, artifact, and
    /// termination guarantees as `run_program_text`.
    #[napi(ts_return_type = "Promise<ProcessOutcome>")]
    pub async fn bash_text(&self, args: BashArgs) -> Result<ProcessOutcome> {
        let state = self.state()?;
        state.bash_outcome(args).await
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
    #[napi(ts_return_type = "Promise<ToolText>")]
    pub async fn read_text(&self, args: ReadArgs) -> Result<ToolText> {
        let state = self.state()?;
        let _active = state.enter_call()?;
        let cancellation = state.shutdown.clone();
        let artifact = state.artifact(&args.path);
        if artifact.is_none() && state.is_capture_path(&args.path) {
            return Err(Error::new(
                napi::Status::InvalidArg,
                "capture files require an exact artifact capability from this Engine",
            ));
        }
        if args.artifact_offset.is_some() && artifact.is_none() {
            return Err(Error::new(
                napi::Status::InvalidArg,
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
                    .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?,
            )
        } else {
            Arc::clone(&state.access)
        };
        let request = agentshim_core::tools::read::ReadRequest {
            path: args.path,
            start_line: args.start_line.map(|line| line as usize),
            line_count: args.line_count.map(|count| count as usize),
            encoding: args.encoding,
            pdf_mode: Self::pdf_mode(args.pdf_mode.as_deref())?,
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
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
            let outcome = read_tool::execute_prepared_with_budget(
                &access,
                &request,
                prepared,
                &cancellation,
                &state.budget,
            )
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
            match outcome {
                read_tool::Attempt::Stable(output) => Ok(output),
                read_tool::Attempt::Changed => Err(Error::new(
                    napi::Status::GenericFailure,
                    "file changed during read",
                )),
            }
        })
        .await
        .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))??;
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
        })
    }

    /// One real core grep against this Engine's repository and page budget.
    #[napi(ts_return_type = "Promise<ToolText>")]
    pub async fn grep_text(&self, args: GrepArgs) -> Result<ToolText> {
        use agentshim_core::tools::grep;
        let state = self.state()?;
        let _active = state.enter_call()?;
        let cancellation = state.shutdown.clone();
        let artifact = args.path.as_deref().and_then(|path| state.artifact(path));
        if artifact.is_none()
            && args
                .path
                .as_deref()
                .is_some_and(|path| state.is_capture_path(path))
        {
            return Err(Error::new(
                napi::Status::InvalidArg,
                "capture files require an exact artifact capability from this Engine",
            ));
        }
        if let Some(record) = artifact.as_ref() {
            if args.glob.is_some() {
                return Err(Error::new(
                    napi::Status::InvalidArg,
                    "artifact grep requires one exact file path and no glob",
                ));
            }
            if !record.valid_text {
                return Err(Error::new(
                    napi::Status::InvalidArg,
                    "binary artifact cannot be searched as text; retry read with artifactOffset",
                ));
            }
        }
        let access = if let Some(record) = artifact.as_ref() {
            Arc::new(
                state
                    .access
                    .with_exact_grant(&record.path)
                    .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?,
            )
        } else {
            Arc::clone(&state.access)
        };
        let mode = parse_grep_mode(args.mode.as_deref())?;
        let case = parse_grep_case(args.case.as_deref())?;
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
                Error::new(
                    napi::Status::GenericFailure,
                    "grep memory reservation failed",
                )
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
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))
            .map(|output| output.text)
        })
        .await
        .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))??;
        Ok(ToolText {
            text,
            complete: true,
            images: Vec::new(),
        })
    }

    /// One real core glob against this Engine's repository and page budget.
    #[napi(ts_return_type = "Promise<ToolText>")]
    pub async fn glob_text(&self, args: GlobArgs) -> Result<ToolText> {
        use agentshim_core::tools::glob;
        let state = self.state()?;
        let _active = state.enter_call()?;
        let cancellation = state.shutdown.clone();
        if args
            .path
            .as_deref()
            .is_some_and(|path| state.is_capture_path(path))
        {
            return Err(Error::new(
                napi::Status::InvalidArg,
                "glob cannot enumerate the capture root",
            ));
        }
        let entry_type = match args.entry_type.as_deref() {
            None => None,
            Some("file") => Some(agentshim_core::tools::glob::GlobEntryType::File),
            Some("directory") => Some(agentshim_core::tools::glob::GlobEntryType::Directory),
            Some("any") => Some(agentshim_core::tools::glob::GlobEntryType::Any),
            Some(other) => {
                return Err(Error::new(
                    napi::Status::InvalidArg,
                    format!("type must be file, directory, or any, got {other}"),
                ));
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
                Error::new(
                    napi::Status::GenericFailure,
                    "glob memory reservation failed",
                )
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
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))
            .map(|output| filter_capture_glob_lines(&output.text, &repository_root, &capture_root))
        })
        .await
        .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))??;
        Ok(ToolText {
            text,
            complete: true,
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
        handle: String,
        wrapped_argv: Option<Vec<String>>,
    ) -> Result<EngineJobHandle> {
        let state = self.state()?;
        start_background_prepared(&state, handle, wrapped_argv.as_deref())
    }

    /// Stop admission, cancel foreground and background work, and await settlement.
    /// Async, idempotent, and safe to call from any Engine state.
    #[napi(ts_return_type = "Promise<void>")]
    pub async fn close(&self) -> Result<()> {
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
    pub(crate) fn enter_call(&self) -> Result<ActiveCall> {
        if self.shutdown.is_cancelled() {
            return Err(Error::new(napi::Status::GenericFailure, "engine is closed"));
        }
        self.active_calls.fetch_add(1, Ordering::SeqCst);
        if self.shutdown.is_cancelled() {
            self.active_calls.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::new(napi::Status::GenericFailure, "engine is closed"));
        }
        Ok(ActiveCall {
            count: Arc::clone(&self.active_calls),
        })
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

    fn read_artifact_page(&self, record: &ArtifactRecord, offset: Option<f64>) -> Result<ToolText> {
        let offset = offset.unwrap_or(0.0);
        if !offset.is_finite() || offset < 0.0 || offset.fract() != 0.0 {
            return Err(Error::new(
                napi::Status::InvalidArg,
                "artifactOffset must be a non-negative integer",
            ));
        }
        let offset = offset as u64;
        if offset > record.bytes {
            return Err(Error::new(
                napi::Status::InvalidArg,
                "artifactOffset is beyond the artifact snapshot",
            ));
        }
        let metadata = std::fs::symlink_metadata(&record.path)
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(Error::new(
                napi::Status::GenericFailure,
                "published artifact is no longer a regular file",
            ));
        }
        let wrapper_bytes = 512_usize;
        let encoded_budget = self.budget.page_bytes().saturating_sub(wrapper_bytes);
        let raw_budget = encoded_budget / 4 * 3;
        let remaining = record.bytes.saturating_sub(offset);
        let to_read = remaining.min(raw_budget as u64) as usize;
        let mut file = std::fs::File::open(&record.path)
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
        let mut bytes = vec![0_u8; to_read];
        file.read_exact(&mut bytes)
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
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
            complete: next == record.bytes,
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
