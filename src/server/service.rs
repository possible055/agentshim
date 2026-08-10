use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt::Display,
    io,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        DiscoverResult, ErrorCode, Implementation, InitializeRequestParams, InitializeResult,
        InitializeResultMethod, JsonObject, ListToolsResult, PaginatedRequestParams,
        ProtocolVersion, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    output::bounded_diagnostic,
    path::{FileAccess, ReadScope, RepositoryRoot},
    runtime::{RuntimeConfig, RuntimeResources},
    tools::{
        bash::{
            BashRequest,
            detached::{DetachedAdmission, DetachedTrees},
            locate::BashLocator,
        },
        exec::{ProcessError, ProcessResolver},
        run_program::ProcessRequest,
    },
};

pub const SERVER_INSTRUCTIONS: &str = "Local repository and Codex extension tools for reading source files, searching contents, finding paths, running one program with literal arguments, and running POSIX bash command lines.";
pub const UNRESTRICTED_SERVER_INSTRUCTIONS: &str = "Local filesystem tools for reading files, searching contents, and finding paths, plus one program with literal arguments and POSIX bash command lines. Read scope is the structured access range of read, grep, and glob; it does not bound what a spawned process can reach.";
pub const MCP_COMPATIBILITY_ENV: &str = "CODEXSHIM_MCP_COMPATIBILITY";

const STRICT_PROTOCOLS: &[ProtocolVersion] = &[ProtocolVersion::V_2026_07_28];
const LEGACY_PROTOCOLS: &[ProtocolVersion] =
    &[ProtocolVersion::V_2026_07_28, ProtocolVersion::V_2025_06_18];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProtocolCompatibility {
    Strict,
    #[default]
    Legacy,
}

impl ProtocolCompatibility {
    fn from_env() -> io::Result<Self> {
        let Some(value) = std::env::var_os(MCP_COMPATIBILITY_ENV) else {
            return Ok(Self::default());
        };
        let value = value.into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{MCP_COMPATIBILITY_ENV} must be valid Unicode"),
            )
        })?;
        value.parse()
    }

    fn supported_protocol_versions(self) -> &'static [ProtocolVersion] {
        match self {
            Self::Strict => STRICT_PROTOCOLS,
            Self::Legacy => LEGACY_PROTOCOLS,
        }
    }
}

impl Display for ProtocolCompatibility {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Strict => formatter.write_str("strict"),
            Self::Legacy => formatter.write_str("legacy"),
        }
    }
}

impl FromStr for ProtocolCompatibility {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "strict" => Ok(Self::Strict),
            "legacy" => Ok(Self::Legacy),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{MCP_COMPATIBILITY_ENV} must be either `strict` or `legacy`, got `{value}`"
                ),
            )),
        }
    }
}

#[derive(Clone)]
pub struct CodexShim {
    root: Arc<RepositoryRoot>,
    file_access: Arc<FileAccess>,
    resources: RuntimeResources,
    process_resolver: ProcessResolver,
    detached: DetachedTrees,
    bash_locator: BashLocator,
    protocol_compatibility: ProtocolCompatibility,
}

pub struct CodexShimBuilder {
    root: PathBuf,
    read_scope: ReadScope,
    runtime: RuntimeConfig,
    protocol_compatibility: ProtocolCompatibility,
}

enum ToolAdmission {
    ReadOnly(OwnedSemaphorePermit),
    Process(OwnedSemaphorePermit),
    /// A detached `bash` call holds no foreground permit. Its capacity is the detached roster,
    /// which limits living process trees rather than threads and output memory, and the two
    /// must not be able to starve each other.
    Detached(DetachedAdmission),
    None,
}

#[derive(Debug)]
enum ToolAdmissionFailure {
    Capacity(&'static str),
    Process(ProcessError),
}

fn first_shell_token(command: &str) -> Option<&str> {
    let command = command.trim_start();
    let first = command.as_bytes().first().copied()?;
    if first == b'\'' || first == b'"' {
        let end = command[1..].find(char::from(first))? + 1;
        return Some(&command[1..end]);
    }
    command.split_whitespace().next()
}

fn shell_delegate(request: &CallToolRequestParams) -> &'static str {
    if request.name.as_ref() != "bash" {
        return "none";
    }
    let Some(command) = request
        .arguments
        .as_ref()
        .and_then(|arguments| arguments.get("command"))
        .and_then(Value::as_str)
    else {
        return "none";
    };
    let Some(token) = first_shell_token(command) else {
        return "none";
    };
    let file_name = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .to_ascii_lowercase();
    if file_name == "bash.exe" {
        return "wsl";
    }
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name.as_str(), |(stem, _)| stem);
    match stem {
        "pwsh" | "powershell" => "pwsh",
        "cmd" => "cmd",
        "wsl" => "wsl",
        "python" | "node" | "perl" | "ruby" => "other-interpreter",
        _ => "none",
    }
}

impl CodexShimBuilder {
    /// Resolve server defaults from the environment for an explicit repository root.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error when runtime or protocol environment settings are invalid.
    pub fn from_env(root: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self {
            root: root.into(),
            read_scope: ReadScope::default(),
            runtime: RuntimeConfig::from_env()?,
            protocol_compatibility: ProtocolCompatibility::from_env()?,
        })
    }

    #[must_use]
    pub fn read_scope(mut self, read_scope: ReadScope) -> Self {
        self.read_scope = read_scope;
        self
    }

    #[must_use]
    pub fn runtime_limits(mut self, runtime: RuntimeConfig) -> Self {
        self.runtime = runtime;
        self
    }

    #[must_use]
    pub fn protocol_compatibility(mut self, compatibility: ProtocolCompatibility) -> Self {
        self.protocol_compatibility = compatibility;
        self
    }

    /// Open the repository capability and build the MCP service.
    ///
    /// # Errors
    ///
    /// Returns the repository root validation or capability-open error.
    pub fn build(self) -> io::Result<CodexShim> {
        let root = Arc::new(RepositoryRoot::open(self.root)?);
        Ok(CodexShim {
            file_access: Arc::new(FileAccess::new(Arc::clone(&root), self.read_scope)),
            root,
            detached: DetachedTrees::new(self.runtime.detached_calls),
            bash_locator: BashLocator::capture(),
            resources: RuntimeResources::new(self.runtime),
            process_resolver: ProcessResolver::capture(),
            protocol_compatibility: self.protocol_compatibility,
        })
    }
}

impl CodexShim {
    /// Create a builder using environment-derived protocol and runtime defaults.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error when runtime or protocol environment settings are invalid.
    pub fn builder(root: impl Into<PathBuf>) -> io::Result<CodexShimBuilder> {
        CodexShimBuilder::from_env(root)
    }
    /// Open and retain a capability for an absolute repository root.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for a relative path, or the root-open I/O error.
    #[cfg(test)]
    pub(crate) fn from_path(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::builder(path.as_ref().to_owned())?.build()
    }

    #[must_use]
    pub fn root_path(&self) -> &Path {
        self.root.path()
    }

    #[must_use]
    pub fn runtime_limits(&self) -> RuntimeConfig {
        self.resources.config()
    }

    #[must_use]
    pub fn shutdown_token(&self) -> CancellationToken {
        self.resources.shutdown_token()
    }

    /// Terminate every detached tree this instance still owns.
    pub fn terminate_detached(&self) {
        self.detached.terminate_all();
    }

    #[must_use]
    pub fn protocol_compatibility(&self) -> ProtocolCompatibility {
        self.protocol_compatibility
    }

    #[must_use]
    pub fn read_scope(&self) -> ReadScope {
        self.file_access.scope()
    }

    /// Verify that the retained repository root remains accessible.
    ///
    /// # Errors
    ///
    /// Returns the capability-relative metadata error when the root is inaccessible.
    pub fn verify_root(&self) -> io::Result<()> {
        self.root.verify()
    }

    /// Spawn this binary through the platform process lifecycle and verify clean completion.
    ///
    /// # Errors
    ///
    /// Returns a resolution, launch, capture, cleanup, or unexpected-output error.
    pub fn verify_process_runtime(&self) -> io::Result<()> {
        let executable = std::env::current_exe()?;
        let request = ProcessRequest {
            program: executable.to_string_lossy().into_owned(),
            args: vec!["--version".to_owned()],
            cwd: None,
            env: BTreeMap::new(),
            unset_env: Vec::new(),
            stdin: None,
            timeout_ms: Some(5_000),
        };
        let output = crate::tools::run_program::execute(
            &self.root,
            &self.process_resolver,
            &request,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .map_err(io::Error::other)?;
        if output.contains("Exit code: 0") && output.contains("codexshim ") {
            Ok(())
        } else {
            Err(io::Error::other(
                "process lifecycle probe returned unexpected output",
            ))
        }
    }

    #[must_use]
    pub fn discovery_result() -> DiscoverResult {
        Self::discovery_result_for(ProtocolCompatibility::default(), ReadScope::default())
    }

    fn discovery_result_for(
        compatibility: ProtocolCompatibility,
        read_scope: ReadScope,
    ) -> DiscoverResult {
        DiscoverResult::from_server_info(
            compatibility.supported_protocol_versions().to_vec(),
            Self::server_info(read_scope),
        )
    }

    #[must_use]
    pub fn tools_result() -> ListToolsResult {
        Self::tools_result_for(ReadScope::default())
    }

    #[must_use]
    pub fn tools_result_for(read_scope: ReadScope) -> ListToolsResult {
        ListToolsResult::with_all_items(tool_catalog(read_scope).to_vec())
            .with_ttl_ms(300_000)
            .with_cache_scope(CacheScope::Private)
    }

    // Read retries intentionally keep capability and reservation lifetimes in one scope.
    #[allow(clippy::too_many_lines)]
    async fn call_read(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        admission: OwnedSemaphorePermit,
    ) -> CallToolResponse {
        let queued = Instant::now();
        let read_request: crate::tools::read::ReadRequest =
            match parse_request(arguments, "read") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        let worker = self.resources.acquire_worker(request_cancellation).await;
        let open_file = self.resources.acquire_open_file(request_cancellation).await;
        let permits = match (worker, open_file) {
            (Ok(worker), Ok(open_file)) => (admission, worker, open_file),
            _ => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "read cancelled while waiting for runtime capacity",
                );
            }
        };
        tracing::info!(target: "codexshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let access = self.file_access.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let span = tracing::Span::current();
        let running = Instant::now();
        let budgets = crate::tools::read::PdfMemoryBudgets::from_config(&self.resources.config());
        let timed_out: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let mut pdf_admission: Option<PdfAdmission> = None;
        let mut deadline = None;
        let mut result = None;
        for attempt_index in 0..2 {
            let prepare_access = access.clone();
            let prepare_request = read_request.clone();
            let prepare_cancellation = cancellation.clone();
            let prepare_span = span.clone();
            let prepared = tokio::task::spawn_blocking(move || {
                prepare_span.in_scope(|| {
crate::tools::read::prepare(
                        &prepare_access,
                        &prepare_request,
                        &prepare_cancellation,
                        budgets,
                    )
                })
            })
            .await;
            let prepared = match prepared {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(crate::tools::read::ReadError::Io(error)))
                    if attempt_index > 0 && error.kind() == io::ErrorKind::NotFound =>
                {
                    result = Some(Ok(Err(crate::tools::read::ReadError::Changed)));
                    break;
                }
                Ok(Err(error)) => {
                    result = Some(Ok(Err(error)));
                    break;
                }
                Err(error) => {
                    result = Some(Err(error));
                    break;
                }
            };
            // A PDF holds its gate and reservation for the whole retry loop. Releasing
            // between attempts would let another call take the gate, so a caller who hit
            // `file_changed` once would come back with `resource_busy` instead — two
            // unrelated failures reported as one.
            let mut text_memory = None;
            if prepared.pdf_mode().is_some() {
                if pdf_admission.is_none() {
                    match self
                        .acquire_pdf_admission(&prepared, request_cancellation)
                        .await
                    {
                        Ok(acquired) => pdf_admission = Some(acquired),
                        Err(busy) => {
                            cancellation_relay.abort();
                            drop(permits);
                            return busy;
                        }
                    }
                }
                deadline = prepared.runtime_limit();
            } else {
                let Ok(memory) = self
                    .resources
                    .reserve_memory(
                        prepared.memory_charge(),
                        request_cancellation,
                    )
                    .await
                else {
                    cancellation_relay.abort();
                    drop(permits);
                    return classified_tool_error(
                        cancellation_class(
                            request_cancellation,
                            &self.resources.shutdown_token(),
                        ),
                        "read cancelled while waiting for runtime capacity",
                    );
                };
                text_memory = Some(memory);
            }
            let execute_access = access.clone();
            let execute_request = read_request.clone();
            let execute_cancellation = cancellation.clone();
            let execute_span = span.clone();
            let started = Instant::now();
            let timer = deadline.map(|limit| {
                let token = cancellation.clone();
                let expired = Arc::clone(&timed_out);
                tokio::spawn(async move {
                    tokio::time::sleep(limit).await;
                    // `spawn_blocking` cannot be preempted, so the ceiling is enforced by
                    // cancelling at the same checkpoints the client cancellation uses. The
                    // real stop latency is therefore checkpoint density, not this timer.
                    expired.store(true, Ordering::SeqCst);
                    token.cancel();
                })
            });
            let executed = tokio::task::spawn_blocking(move || {
                execute_span.in_scope(|| {
                    crate::tools::read::execute_prepared(
                        &execute_access,
                        &execute_request,
                        prepared,
                        &execute_cancellation,
                    )
                })
            })
            .await;
            if let Some(timer) = timer {
                timer.abort();
            }
            drop(text_memory);
            // The elapsed check is not redundant with the flag: the timer is a spawned
            // task, so under load it can be scheduled after the work already overran. The
            // ceiling is a property of how long the work took, not of when a task ran.
            let overran = deadline.is_some_and(|limit| started.elapsed() > limit);
            if timed_out.load(Ordering::SeqCst) || overran {
                let limit = deadline.unwrap_or_default();
                cancellation_relay.abort();
                drop(pdf_admission);
                drop(permits);
                return pdf_timeout(limit, started.elapsed());
            }
            match executed {
                Ok(Ok(crate::tools::read::Attempt::Stable(output))) => {
                    result = Some(Ok(Ok(output)));
                    break;
                }
                Ok(Ok(crate::tools::read::Attempt::Changed)) if attempt_index == 0 => {
                    tracing::warn!(target: "codexshim", event = "read_retry", phase = "execution", outcome = "degraded_success", reason = "file_changed");
                }
                Ok(Ok(crate::tools::read::Attempt::Changed)) => {
                    result = Some(Ok(Err(crate::tools::read::ReadError::Changed)));
                    break;
                }
                Ok(Err(error)) => {
                    result = Some(Ok(Err(error)));
                    break;
                }
                Err(error) => {
                    result = Some(Err(error));
                    break;
                }
            }
        }
        let result = result.expect("read attempt loop always produces a result");
        drop(permits);
        cancellation_relay.abort();
        // The PDF reservation outlives the response construction on purpose. Base64 image
        // data is copied again into content blocks and again by the JSON serialiser, and
        // that is the single largest allocation on the whole path. Releasing the
        // reservation before it happens would leave the biggest number off the books.
        let response = blocking_response("read", duration_ms(running.elapsed()), result);
        drop(pdf_admission);
        response
    }

    /// Acquire the PDF gate and this mode's memory reservation.
    ///
    /// The gate allows a bounded wait; the reservation does not. A waiting reservation is
    /// exactly the head-of-line block this separation exists to remove — an ordinary text
    /// read must never queue behind a PDF's share of the pool.
    async fn acquire_pdf_admission(
        &self,
        prepared: &crate::tools::read::PreparedRead,
        request_cancellation: &CancellationToken,
    ) -> Result<PdfAdmission, CallToolResponse> {
        let Some(gate) = self.resources.acquire_pdf_gate(request_cancellation).await else {
            return Err(pdf_busy("pdf_concurrency"));
        };
        let Some(memory) = self.resources.try_reserve_memory(prepared.memory_charge())
        else {
            return Err(pdf_busy("memory_budget"));
        };
        Ok(PdfAdmission {
            _gate: gate,
            _memory: memory,
        })
    }

    async fn call_glob(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        admission: OwnedSemaphorePermit,
    ) -> CallToolResponse {
        let queued = Instant::now();
        let glob_request = match parse_request(arguments, "glob") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        let worker = self.resources.acquire_worker(request_cancellation).await;
        let memory = self
            .resources
            .reserve_memory(
                crate::tools::glob::memory_charge(&glob_request),
                request_cancellation,
            )
            .await;
        let permits = match (worker, memory) {
            (Ok(worker), Ok(memory)) => (admission, worker, memory),
            _ => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "glob cancelled while waiting for runtime capacity",
                );
            }
        };
        tracing::info!(target: "codexshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let access = self.file_access.clone();
        let resources = self.resources.clone();
        let reservation = crate::runtime::MemoryReservation::from_initial(
            resources.clone(),
            permits.2,
            crate::tools::glob::memory_charge(&glob_request),
        );
        let permits = (permits.0, permits.1);
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let span = tracing::Span::current();
        let running = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = crate::tools::glob::execute_output(
                    &access,
                    &glob_request,
                    &resources,
                    &cancellation,
                    reservation,
                );
                drop(permits);
                result
            })
        })
        .await;
        cancellation_relay.abort();
        blocking_response("glob", duration_ms(running.elapsed()), result)
    }

    async fn call_grep(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        admission: OwnedSemaphorePermit,
    ) -> CallToolResponse {
        let queued = Instant::now();
        let grep_request: crate::tools::grep::GrepRequest = match parse_request(arguments, "grep") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        let worker = self.resources.acquire_worker(request_cancellation).await;
        let open_file = self
            .resources
            .acquire_open_file(request_cancellation)
            .await;
        let memory = self
            .resources
            .reserve_memory(
                crate::tools::grep::base_memory_charge(),
                request_cancellation,
            )
            .await;
        let permits = match (worker, open_file, memory) {
            (Ok(worker), Ok(open_file), Ok(memory)) => (admission, worker, open_file, memory),
            _ => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "grep cancelled while waiting for runtime capacity",
                );
            }
        };
        tracing::info!(target: "codexshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let access = self.file_access.clone();
        let resources = self.resources.clone();
        let reservation = crate::runtime::MemoryReservation::from_initial(
            resources.clone(),
            permits.3,
            crate::tools::grep::base_memory_charge(),
        );
        let permits = (permits.0, permits.1, permits.2);
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let span = tracing::Span::current();
        let running = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = crate::tools::grep::execute_output(
                    &access,
                    &grep_request,
                    &resources,
                    &cancellation,
                    reservation,
                );
                drop(permits);
                result
            })
        })
        .await;
        cancellation_relay.abort();
        blocking_response("grep", duration_ms(running.elapsed()), result)
    }

    async fn call_process(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        admission: OwnedSemaphorePermit,
    ) -> CallToolResponse {
        let process_request: ProcessRequest = match parse_request(arguments, "run_program") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        if let Err(error) = process_request.validate() {
            return classified_tool_error("validation", error.to_string());
        }
        let timeout = Duration::from_millis(process_request.timeout_ms());
        let memory_charge = process_request.memory_charge();
        let queued = Instant::now();
        let permits = tokio::time::timeout(timeout, async {
            let memory = self
                .resources
                .reserve_memory(memory_charge, request_cancellation)
                .await?;
            Ok::<_, crate::runtime::AcquireError>((admission, memory))
        })
        .await;
        let permits = match permits {
            Ok(Ok(permits)) => permits,
            Ok(Err(_)) => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "run_program cancelled while waiting for process capacity",
                );
            }
            Err(_) => return queue_timeout("run_program", process_request.timeout_ms()),
        };
        tracing::info!(target: "codexshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let Some(remaining) = timeout.checked_sub(queued.elapsed()) else {
            return queue_timeout("run_program", process_request.timeout_ms());
        };
        let root = self.root.clone();
        let resolver = self.process_resolver.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let span = tracing::Span::current();
        let running = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let Some(remaining) = remaining.checked_sub(running.elapsed()) else {
                    drop(permits);
                    return Err(ProcessError::TimeoutBeforeSpawn {
                        timeout_ms: process_request.timeout_ms(),
                    });
                };
                let result = crate::tools::run_program::execute_output(
                    &root,
                    &resolver,
                    &process_request,
                    remaining,
                    &cancellation,
                );
                drop(permits);
                result
            })
        })
        .await;
        cancellation_relay.abort();
        blocking_response("run_program", duration_ms(running.elapsed()), result)
    }

    async fn call_bash(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        admission: ToolAdmission,
    ) -> CallToolResponse {
        let bash_request: BashRequest = match parse_request(arguments, "bash") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        if let Err(error) = bash_request.validate() {
            return classified_tool_error("validation", error.to_string());
        }
        // The parsed request, not the pre-admission peek, decides which resource this call
        // consumes. They can only disagree through a request that failed to parse as detached,
        // and that case must still be capped rather than trusted.
        let (foreground, detached_admission) = match (admission, bash_request.detach) {
            (ToolAdmission::Process(permit), false) => (Some(permit), None),
            (ToolAdmission::Process(permit), true) => {
                drop(permit);
                match self.detached.admit() {
                    Ok(admission) => (None, Some(admission)),
                    Err(ProcessError::ResourceBusy(message)) => {
                        return resource_busy_with_message("bash", "detached", message);
                    }
                    Err(error) => return diagnostic_tool_error(&error),
                }
            }
            (ToolAdmission::Detached(admission), true) => (None, Some(admission)),
            (ToolAdmission::Detached(admission), false) => {
                drop(admission);
                match self.resources.try_admit_process() {
                Some(permit) => (Some(permit), None),
                None => return resource_busy("bash", "process"),
                }
            }
            _ => unreachable!("bash is admitted as a process or a detached call"),
        };
        let timeout = Duration::from_millis(bash_request.timeout_ms());
        let queued = Instant::now();
        let memory_charge = bash_request.memory_charge();
        let permits = match tokio::time::timeout(timeout, async {
            let memory = self
                .resources
                .reserve_memory(memory_charge, request_cancellation)
                .await?;
            Ok::<_, crate::runtime::AcquireError>((foreground, memory))
        })
        .await
        {
            Ok(Ok(permits)) => Some(permits),
            Ok(Err(_)) => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "bash cancelled while waiting for request memory",
                );
            }
            Err(_) => None,
        };
        let Some(permits) = permits else {
            return queue_timeout("bash", bash_request.timeout_ms());
        };
        tracing::info!(target: "codexshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let Some(remaining) = timeout.checked_sub(queued.elapsed()) else {
            return queue_timeout("bash", bash_request.timeout_ms());
        };
        let root = self.root.clone();
        let locator = self.bash_locator.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let span = tracing::Span::current();
        let running = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let remaining = if bash_request.detach {
                    remaining
                } else {
                    let Some(remaining) = remaining.checked_sub(running.elapsed()) else {
                        drop(permits);
                        return Err(ProcessError::TimeoutBeforeSpawn {
                            timeout_ms: bash_request.timeout_ms(),
                        });
                    };
                    remaining
                };
                let result = crate::tools::bash::execute_output(
                    &root,
                    &locator,
                    detached_admission,
                    &bash_request,
                    remaining,
                    &cancellation,
                );
                drop(permits);
                result
            })
        })
        .await;
        cancellation_relay.abort();
        blocking_response("bash", duration_ms(running.elapsed()), result)
    }

    fn server_info(read_scope: ReadScope) -> ServerInfo {
        let instructions = match read_scope {
            ReadScope::Normal => SERVER_INSTRUCTIONS,
            ReadScope::Unrestricted => UNRESTRICTED_SERVER_INSTRUCTIONS,
        };
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("codexshim", env!("CARGO_PKG_VERSION")))
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_instructions(instructions)
    }

    async fn dispatch_tool(
        &self,
        request: CallToolRequestParams,
        context: &RequestContext<RoleServer>,
        admission: ToolAdmission,
    ) -> Result<CallToolResponse, McpError> {
        match (request.name.as_ref(), admission) {
            ("read", ToolAdmission::ReadOnly(admission)) => Ok(self
                .call_read(request.arguments, &context.ct, admission)
                .await),
            ("glob", ToolAdmission::ReadOnly(admission)) => Ok(self
                .call_glob(request.arguments, &context.ct, admission)
                .await),
            ("grep", ToolAdmission::ReadOnly(admission)) => Ok(self
                .call_grep(request.arguments, &context.ct, admission)
                .await),
            ("run_program", ToolAdmission::Process(admission)) => Ok(self
                .call_process(request.arguments, &context.ct, admission)
                .await),
            (
                "bash",
                admission @ (ToolAdmission::Process(_) | ToolAdmission::Detached(_)),
            ) => Ok(self
                .call_bash(request.arguments, &context.ct, admission)
                .await),
            (_, ToolAdmission::None) => {
                tracing::error!(target: "codexshim", event = "tool_unknown", phase = "request", error_class = "validation");
                Err(McpError::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!("unknown tool: {}", request.name),
                    None,
                ))
            }
            _ => unreachable!("tool admission class must match the dispatched tool"),
        }
    }

    fn try_admit_tool(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<ToolAdmission, ToolAdmissionFailure> {
        match request.name.as_ref() {
            "read" | "glob" | "grep" => self
                .resources
                .try_admit_read_only()
                .map(ToolAdmission::ReadOnly)
                .ok_or(ToolAdmissionFailure::Capacity("read_only")),
            "bash" if requests_detach(request.arguments.as_ref()) => self
                .detached
                .admit()
                .map(ToolAdmission::Detached)
                .map_err(ToolAdmissionFailure::Process),
            "run_program" | "bash" => self
                .resources
                .try_admit_process()
                .map(ToolAdmission::Process)
                .ok_or(ToolAdmissionFailure::Capacity("process")),
            _ => Ok(ToolAdmission::None),
        }
    }
}

impl ServerHandler for CodexShim {
    fn get_info(&self) -> ServerInfo {
        Self::server_info(self.read_scope())
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(self.protocol_compatibility.supported_protocol_versions())
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        tracing::info!(target: "codexshim", event = "initialize", phase = "protocol", protocol = %request.protocol_version, client_name = %request.client_info.name, client_version = %request.client_info.version);
        if self.protocol_compatibility != ProtocolCompatibility::Legacy
            || request.protocol_version != ProtocolVersion::V_2025_06_18
        {
            return Err(McpError::method_not_found::<InitializeResultMethod>());
        }

        context.peer.set_peer_info(request);
        Ok(Self::server_info(self.read_scope())
            .with_protocol_version(ProtocolVersion::V_2025_06_18))
    }

    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, McpError> {
        tracing::info!(target: "codexshim", event = "discover", phase = "protocol");
        Ok(Self::discovery_result_for(
            self.protocol_compatibility,
            self.read_scope(),
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(Self::tools_result_for(self.read_scope()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_catalog(self.read_scope())
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let admission = match self.try_admit_tool(&request) {
            Ok(admission) => admission,
            Err(ToolAdmissionFailure::Capacity(class)) => {
                return Ok(resource_busy(request.name.as_ref(), class));
            }
            Err(ToolAdmissionFailure::Process(error)) => {
                return Ok(match error {
                    ProcessError::ResourceBusy(message) => {
                        resource_busy_with_message("bash", "detached", message)
                    }
                    other => diagnostic_tool_error(&other),
                });
            }
        };
        if !tracing::enabled!(target: "codexshim", tracing::Level::INFO) {
            return self.dispatch_tool(request, &context, admission).await;
        }
        let call_id = Uuid::new_v4().to_string();
        let tool = request.name.to_string();
        let span = if request.name.as_ref() == "bash" {
            let shell_delegate = shell_delegate(&request);
            tracing::info_span!(
                target: "codexshim",
                "tool_call",
                call_id = %call_id,
                tool = %tool,
                shell_delegate
            )
        } else {
            tracing::info_span!(
                target: "codexshim",
                "tool_call",
                call_id = %call_id,
                tool = %tool
            )
        };
        async move {
            tracing::info!(target: "codexshim", event = "tool_start", phase = "request");
            let response = self.dispatch_tool(request, &context, admission).await?;
            Ok(response)
        }
        .instrument(span)
        .await
    }
}
