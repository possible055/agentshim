use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use napi::{
    Error, Result,
    bindgen_prelude::spawn_blocking,
    threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode},
};
use napi_derive::napi;
use tokio_util::sync::CancellationToken;

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
    /// Maximum execution time in milliseconds a process call may request.
    pub timeout_ceiling_ms: Option<u32>,
    /// Explicit child environment; the Engine never reads the host ambient
    /// environment on its own.
    pub env: Option<Vec<EnvEntry>>,
    /// Private persistent root for durable process capture artifacts.
    pub capture_root: Option<String>,
    /// Aggregate raw capture ceiling for one process call, in bytes.
    pub capture_max_bytes: Option<f64>,
}

#[napi(object)]
pub struct ToolText {
    pub text: String,
    pub complete: bool,
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
    live_emitters: Arc<AtomicUsize>,
    pub(crate) capture_root: std::path::PathBuf,
    pub(crate) capture_max_bytes: u64,
    pub(crate) session_key: String,
    pub(crate) locator: agentshim_core::tools::bash::locate::BashLocator,
    pub(crate) artifacts: std::sync::Mutex<Vec<ArtifactRecord>>,
    pub(crate) prepared: crate::process::PreparedHandles,
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
        let access = Arc::new(agentshim_core::path::FileAccess::new(
            Arc::clone(&root),
            agentshim_core::path::ReadScope::Normal,
        ));
        let config = agentshim_core::runtime::RuntimeConfig::for_tests(4);
        let timeout_ceiling_ms = options.timeout_ceiling_ms.map_or_else(
            agentshim_core::tools::exec::spawn::default_max_timeout_ms,
            u64::from,
        );
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
        Ok(Self {
            state: std::sync::RwLock::new(Some(Arc::new(EngineState {
                root,
                access,
                resources: agentshim_core::runtime::RuntimeResources::new(config),
                budget: NativeCallBudget::new(page_budget),
                timeout_ceiling_ms,
                env,
                shutdown: CancellationToken::new(),
                live_emitters: Arc::new(AtomicUsize::new(0)),
                capture_root,
                capture_max_bytes: clamp_capture_ceiling(options.capture_max_bytes),
                session_key: uuid::Uuid::new_v4().simple().to_string(),
                locator: agentshim_core::tools::bash::locate::BashLocator::capture(),
                artifacts: std::sync::Mutex::new(Vec::new()),
                prepared: crate::process::PreparedHandles::new(),
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
    #[napi(ts_return_type = "Promise<ProcessOutcome>")]
    pub async fn spawn_prepared(
        &self,
        handle: String,
        wrapped_argv: Option<Vec<String>>,
    ) -> Result<ProcessOutcome> {
        let state = self.state()?;
        state.spawn_prepared(handle, wrapped_argv.as_deref()).await
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
        let cancellation = state.shutdown.clone();
        let request = agentshim_core::tools::read::ReadRequest {
            path: args.path,
            start_line: args.start_line.map(|line| line as usize),
            line_count: args.line_count.map(|count| count as usize),
            encoding: args.encoding,
            pdf_mode: Self::pdf_mode(args.pdf_mode.as_deref())?,
            pages: args.pages,
            pdf_cursor: args.pdf_cursor,
            read_grant: None,
        };
        let text = spawn_blocking(move || {
            use agentshim_core::tools::read as read_tool;
            let prepared = read_tool::prepare(
                &state.access,
                &request,
                &cancellation,
                read_tool::PdfMemoryBudgets::from_config(&state.resources.config()),
            )
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
            let outcome = read_tool::execute_prepared_with_budget(
                &state.access,
                &request,
                prepared,
                &cancellation,
                &state.budget,
            )
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
            match outcome {
                read_tool::Attempt::Stable(output) => Ok(output.text),
                read_tool::Attempt::Changed => Err(Error::new(
                    napi::Status::GenericFailure,
                    "file changed during read",
                )),
            }
        })
        .await
        .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))??;
        Ok(ToolText {
            text,
            complete: true,
        })
    }

    /// One real core grep against this Engine's repository and page budget.
    #[napi(ts_return_type = "Promise<ToolText>")]
    pub async fn grep_text(&self, args: GrepArgs) -> Result<ToolText> {
        use agentshim_core::tools::grep;
        let state = self.state()?;
        let cancellation = state.shutdown.clone();
        let mode = match args.mode.as_deref() {
            None => None,
            Some("content") => Some(agentshim_core::tools::grep::GrepMode::Content),
            Some("files") => Some(agentshim_core::tools::grep::GrepMode::Files),
            Some("count") => Some(agentshim_core::tools::grep::GrepMode::Count),
            Some(other) => {
                return Err(Error::new(
                    napi::Status::InvalidArg,
                    format!("mode must be content, files, or count, got {other}"),
                ));
            }
        };
        let case = match args.case.as_deref() {
            None => None,
            Some("smart") => Some(agentshim_core::tools::grep::CaseMode::Smart),
            Some("sensitive") => Some(agentshim_core::tools::grep::CaseMode::Sensitive),
            Some("insensitive") => Some(agentshim_core::tools::grep::CaseMode::Insensitive),
            Some(other) => {
                return Err(Error::new(
                    napi::Status::InvalidArg,
                    format!("case must be smart, sensitive, or insensitive, got {other}"),
                ));
            }
        };
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
            read_grant: None,
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
                &state.access,
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
        })
    }

    /// One real core glob against this Engine's repository and page budget.
    #[napi(ts_return_type = "Promise<ToolText>")]
    pub async fn glob_text(&self, args: GlobArgs) -> Result<ToolText> {
        use agentshim_core::tools::glob;
        let state = self.state()?;
        let cancellation = state.shutdown.clone();
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
            .map(|output| output.text)
        })
        .await
        .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))??;
        Ok(ToolText {
            text,
            complete: true,
        })
    }

    /// Start one bounded `TSFN` emitter on a native thread. Events are dropped when
    /// the bounded queue is full; the thread always terminates after its batch or
    /// on Engine shutdown, whichever comes first.
    #[napi]
    pub fn emit_events(&self, callback: ThreadsafeFunction<u32>) -> Result<()> {
        let state = self.state()?;
        if state.shutdown.is_cancelled() {
            return Err(Error::new(napi::Status::GenericFailure, "engine is closed"));
        }
        let tsfn = callback;
        let shutdown = state.shutdown.clone();
        let live = Arc::clone(&state.live_emitters);
        live.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            for value in 0..64_u32 {
                if shutdown.is_cancelled() {
                    break;
                }
                if tsfn.call(Ok(value), ThreadsafeFunctionCallMode::NonBlocking) != napi::Status::Ok
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            live.fetch_sub(1, Ordering::SeqCst);
        });
        Ok(())
    }

    /// Number of emitter threads still running; the close gate waits for zero.
    #[napi]
    pub fn live_emitters(&self) -> u32 {
        self.state().map_or(0, |state| {
            u32::try_from(state.live_emitters.load(Ordering::SeqCst)).unwrap_or(u32::MAX)
        })
    }

    /// Stop admission, cancel foreground work, and drain native emitters.
    /// Async, idempotent, and safe to call from any Engine state.
    #[napi(ts_return_type = "Promise<void>")]
    pub async fn close(&self) -> Result<()> {
        if let Ok(state) = self.state() {
            state.shutdown.cancel();
            let live = Arc::clone(&state.live_emitters);
            spawn_blocking(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while live.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
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
            spawn_blocking(move || drop(state))
                .await
                .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
        }
        Ok(())
    }
}
