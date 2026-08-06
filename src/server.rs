use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt::Display,
    io,
    path::Path,
    str::FromStr,
    sync::{Arc, OnceLock},
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
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    output::bounded_diagnostic,
    path::{FileAccess, ReadScope, RepositoryRoot},
    runtime::{RuntimeConfig, RuntimeResources},
    tools::process::{ProcessRequest, ProcessResolver},
};

pub const SERVER_INSTRUCTIONS: &str = "Local repository and Codex extension tools for reading source files, searching contents, finding paths, and running programs with structured arguments without PowerShell command strings.";
pub const UNRESTRICTED_SERVER_INSTRUCTIONS: &str = "Local filesystem tools for reading files, searching contents, and finding paths, plus structured program execution without PowerShell command strings. Read scope does not affect process execution.";
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
    protocol_compatibility: ProtocolCompatibility,
}

impl CodexShim {
    /// Open and retain a capability for the process's current directory.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the current directory cannot be resolved or opened.
    pub fn from_current_dir() -> io::Result<Self> {
        Self::from_path(std::env::current_dir()?)
    }

    /// Open the current repository with an explicit read scope.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the current directory or repository root cannot be opened.
    pub fn from_current_dir_with_scope(read_scope: ReadScope) -> io::Result<Self> {
        Self::from_path_with_scope(std::env::current_dir()?, read_scope)
    }

    /// Open the current repository with an already configured shared runtime budget.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the current directory or repository root cannot be opened.
    pub fn from_current_dir_with_resources(resources: RuntimeResources) -> io::Result<Self> {
        Self::from_path_with_resources(std::env::current_dir()?, resources)
    }

    /// Open the current repository with shared runtime resources and an explicit read scope.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the current directory or repository root cannot be opened.
    pub fn from_current_dir_with_resources_and_scope(
        resources: RuntimeResources,
        read_scope: ReadScope,
    ) -> io::Result<Self> {
        Self::from_path_with_resources_and_scope(std::env::current_dir()?, resources, read_scope)
    }

    /// Open and retain a capability for an absolute repository root.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for a relative path, or the root-open I/O error.
    pub fn from_path(path: impl AsRef<Path>) -> io::Result<Self> {
        let config = RuntimeConfig::from_env()?;
        Self::from_path_with_resources(path, RuntimeResources::new(config))
    }

    /// Open an absolute repository root with an explicit read scope.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for a relative path, or the root-open I/O error.
    pub fn from_path_with_scope(path: impl AsRef<Path>, read_scope: ReadScope) -> io::Result<Self> {
        let config = RuntimeConfig::from_env()?;
        Self::from_path_with_resources_and_scope(path, RuntimeResources::new(config), read_scope)
    }

    fn from_path_with_resources(
        path: impl AsRef<Path>,
        resources: RuntimeResources,
    ) -> io::Result<Self> {
        Self::from_path_with_resources_and_scope(path, resources, ReadScope::default())
    }

    fn from_path_with_resources_and_scope(
        path: impl AsRef<Path>,
        resources: RuntimeResources,
        read_scope: ReadScope,
    ) -> io::Result<Self> {
        let root = Arc::new(RepositoryRoot::open(path)?);
        Ok(Self {
            file_access: Arc::new(FileAccess::new(Arc::clone(&root), read_scope)),
            root,
            resources,
            process_resolver: ProcessResolver::capture(),
            protocol_compatibility: ProtocolCompatibility::from_env()?,
        })
    }

    #[must_use]
    pub fn root_path(&self) -> &Path {
        self.root.path()
    }

    #[must_use]
    pub fn resources(&self) -> &RuntimeResources {
        &self.resources
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
        let output = crate::tools::process::execute(
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

    async fn call_read(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
    ) -> CallToolResponse {
        let queued = Instant::now();
        let read_request = match parse_request(arguments, "read") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        let read_only = self.resources.acquire_read_only(request_cancellation).await;
        let worker = self.resources.acquire_worker(request_cancellation).await;
        let open_file = self.resources.acquire_open_file(request_cancellation).await;
        let memory = self
            .resources
            .reserve_memory(256 * 1024, request_cancellation)
            .await;
        let permits = match (read_only, worker, open_file, memory) {
            (Ok(read_only), Ok(worker), Ok(open_file), Ok(memory)) => {
                (read_only, worker, open_file, memory)
            }
            _ => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "read cancelled while waiting for bounded runtime capacity",
                );
            }
        };
        tracing::info!(target: "codexshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let access = self.file_access.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let span = tracing::Span::current();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result =
                    crate::tools::read::execute_detailed(&access, &read_request, &cancellation);
                drop(permits);
                result
            })
        })
        .await;
        cancellation_relay.abort();
        blocking_response("read", result)
    }

    async fn call_glob(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
    ) -> CallToolResponse {
        let queued = Instant::now();
        let glob_request = match parse_request(arguments, "glob") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        let read_only = self.resources.acquire_read_only(request_cancellation).await;
        let worker = self.resources.acquire_worker(request_cancellation).await;
        let memory = self
            .resources
            .reserve_memory(crate::tools::glob::memory_charge(), request_cancellation)
            .await;
        let permits = match (read_only, worker, memory) {
            (Ok(read_only), Ok(worker), Ok(memory)) => (read_only, worker, memory),
            _ => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "glob cancelled while waiting for bounded runtime capacity",
                );
            }
        };
        tracing::info!(target: "codexshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let access = self.file_access.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let span = tracing::Span::current();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result =
                    crate::tools::glob::execute_detailed(&access, &glob_request, &cancellation);
                drop(permits);
                result
            })
        })
        .await;
        cancellation_relay.abort();
        blocking_response("glob", result)
    }

    async fn call_grep(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
    ) -> CallToolResponse {
        let queued = Instant::now();
        let grep_request = match parse_request(arguments, "grep") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        let requested_lanes = self.resources.config().worker_lanes;
        let read_only = self.resources.acquire_read_only(request_cancellation).await;
        let workers = self
            .resources
            .acquire_search_lanes(requested_lanes, request_cancellation)
            .await;
        let lanes = workers.as_ref().map_or(1, crate::runtime::SearchLanes::len);
        let open_files = self
            .resources
            .acquire_open_files(lanes, request_cancellation)
            .await;
        let memory = self
            .resources
            .reserve_memory(
                crate::tools::grep::memory_charge(lanes),
                request_cancellation,
            )
            .await;
        let permits = match (read_only, workers, open_files, memory) {
            (Ok(read_only), Ok(workers), Ok(open_files), Ok(memory)) => {
                (read_only, workers, open_files, memory)
            }
            _ => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "grep cancelled while waiting for bounded runtime capacity",
                );
            }
        };
        tracing::info!(target: "codexshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let access = self.file_access.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let span = tracing::Span::current();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = crate::tools::grep::execute_detailed(
                    &access,
                    &grep_request,
                    lanes,
                    &cancellation,
                );
                drop(permits);
                result
            })
        })
        .await;
        cancellation_relay.abort();
        blocking_response("grep", result)
    }

    async fn call_process(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
    ) -> CallToolResponse {
        let process_request: ProcessRequest = match parse_request(arguments, "run_process") {
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
            let process = self.resources.acquire_process(request_cancellation).await?;
            let memory = self
                .resources
                .reserve_memory(memory_charge, request_cancellation)
                .await?;
            Ok::<_, crate::runtime::AcquireError>((process, memory))
        })
        .await;
        let permits = match permits {
            Ok(Ok(permits)) => permits,
            Ok(Err(_)) => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "run_process cancelled while waiting for process capacity",
                );
            }
            Err(_) => return process_queue_timeout(process_request.timeout_ms()),
        };
        tracing::info!(target: "codexshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let Some(remaining) = timeout.checked_sub(queued.elapsed()) else {
            return process_queue_timeout(process_request.timeout_ms());
        };
        let root = self.root.clone();
        let resolver = self.process_resolver.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let span = tracing::Span::current();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = crate::tools::process::execute_detailed(
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
        blocking_response("run_process", result)
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
    ) -> Result<CallToolResponse, McpError> {
        match request.name.as_ref() {
            "read" => Ok(self.call_read(request.arguments, &context.ct).await),
            "glob" => Ok(self.call_glob(request.arguments, &context.ct).await),
            "grep" => Ok(self.call_grep(request.arguments, &context.ct).await),
            "run_process" => Ok(self.call_process(request.arguments, &context.ct).await),
            _ => {
                tracing::error!(target: "codexshim", event = "tool_unknown", phase = "request", error_class = "validation");
                Err(McpError::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!("unknown tool: {}", request.name),
                    None,
                ))
            }
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
        if !tracing::enabled!(target: "codexshim", tracing::Level::INFO) {
            return self.dispatch_tool(request, &context).await;
        }
        let call_id = Uuid::new_v4().to_string();
        let tool = request.name.to_string();
        let span =
            tracing::info_span!(target: "codexshim", "tool_call", call_id = %call_id, tool = %tool);
        async move {
            tracing::info!(target: "codexshim", event = "tool_start", phase = "request");
            let response = self.dispatch_tool(request, &context).await?;
            Ok(response)
        }
        .instrument(span)
        .await
    }
}

fn parse_request<T: DeserializeOwned>(
    arguments: Option<JsonObject>,
    tool: &str,
) -> Result<T, String> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| format!("invalid {tool} request: {error}"))
}

fn tool_error(message: impl Into<String>) -> CallToolResponse {
    let message = message.into();
    CallToolResult::error(vec![ContentBlock::text(bounded_diagnostic(&message))]).into()
}

trait DiagnosticError: Display {
    fn error_class(&self) -> &'static str;
}

impl DiagnosticError for crate::tools::read::ReadError {
    fn error_class(&self) -> &'static str {
        use crate::tools::read::ReadError;
        match self {
            ReadError::Validation(_) => "validation",
            ReadError::Path(_)
            | ReadError::NonUnicodePath
            | ReadError::Directory
            | ReadError::NotRegular => "path",
            ReadError::Cancelled => "client_cancellation",
            ReadError::Output(_) => "output_invariant",
            ReadError::Io(_) | ReadError::Decode(_) | ReadError::Binary | ReadError::Changed => {
                "io"
            }
        }
    }
}

impl DiagnosticError for crate::tools::glob::GlobError {
    fn error_class(&self) -> &'static str {
        use crate::tools::glob::GlobError;
        match self {
            GlobError::Validation(_) | GlobError::Pattern(_) => "validation",
            GlobError::Path(_) => "path",
            GlobError::Output(_) => "output_invariant",
            GlobError::TooManyMatches | GlobError::Memory => "resource_timeout",
            GlobError::Traversal(_) | GlobError::Io(_) => "io",
        }
    }
}

impl DiagnosticError for crate::tools::grep::GrepError {
    fn error_class(&self) -> &'static str {
        use crate::tools::grep::GrepError;
        match self {
            GrepError::Validation(_) | GrepError::Regex(_) | GrepError::Glob(_) => "validation",
            GrepError::Path(_) => "path",
            GrepError::Cancelled => "client_cancellation",
            GrepError::Output(_) => "output_invariant",
            GrepError::CandidateMemory | GrepError::CaptureMemory => "resource_timeout",
            GrepError::Traversal(_) | GrepError::Io(_) => "io",
        }
    }
}

impl DiagnosticError for crate::tools::process::ProcessError {
    fn error_class(&self) -> &'static str {
        use crate::tools::process::ProcessError;
        match self {
            ProcessError::Validation(_) => "validation",
            ProcessError::Resolve(_) => "path",
            ProcessError::Io(_) => "io",
            ProcessError::Timeout { .. } | ProcessError::TimeoutBeforeSpawn { .. } => {
                "resource_timeout"
            }
            ProcessError::Cancelled => "client_cancellation",
            ProcessError::OutcomeUncertain => "outcome_uncertain",
            ProcessError::Output(_) => "output_invariant",
        }
    }
}

fn classified_tool_error(
    error_class: &'static str,
    message: impl Into<String>,
) -> CallToolResponse {
    tracing::error!(target: "codexshim", event = "tool_error", phase = "response", outcome = "error", error_class);
    tool_error(message)
}

fn blocking_response<E: DiagnosticError>(
    tool: &str,
    result: Result<Result<crate::diagnostics::DetailedExecution, E>, tokio::task::JoinError>,
) -> CallToolResponse {
    match result {
        Ok(Ok(result)) => {
            let outcome = if tool == "run_process" && !result.output.contains("Exit code: 0") {
                "child_nonzero"
            } else {
                "success"
            };
            if outcome == "child_nonzero" {
                tracing::warn!(target: "codexshim", event = "tool_complete", phase = "response", outcome, error_class = "child_nonzero", run_ms = result.run_ms);
            } else {
                tracing::info!(target: "codexshim", event = "tool_complete", phase = "response", outcome, run_ms = result.run_ms);
            }
            CallToolResult::success(vec![ContentBlock::text(result.output)]).into()
        }
        Ok(Err(error)) => classified_tool_error(error.error_class(), error.to_string()),
        Err(error) => {
            classified_tool_error("worker_panic", format!("{tool} worker failed: {error}"))
        }
    }
}

fn process_queue_timeout(timeout_ms: u64) -> CallToolResponse {
    classified_tool_error(
        "resource_timeout",
        process_queue_timeout_message(timeout_ms),
    )
}

fn cancellation_class(request: &CancellationToken, shutdown: &CancellationToken) -> &'static str {
    if shutdown.is_cancelled() && !request.is_cancelled() {
        "shutdown"
    } else {
        "client_cancellation"
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn process_queue_timeout_message(timeout_ms: u64) -> String {
    format!(
        "run_process timed out after {timeout_ms} ms while waiting for process capacity; no child was started"
    )
}

fn relayed_cancellation(
    request: &CancellationToken,
    shutdown: CancellationToken,
) -> (CancellationToken, tokio::task::JoinHandle<()>) {
    let cancellation = CancellationToken::new();
    let signal = cancellation.clone();
    let request = request.clone();
    let relay = tokio::spawn(async move {
        tokio::select! {
            () = request.cancelled() => {
                tracing::warn!(target: "codexshim", event = "tool_cancelled", phase = "execution", error_class = "client_cancellation");
            }
            () = shutdown.cancelled() => {
                tracing::warn!(target: "codexshim", event = "tool_cancelled", phase = "execution", error_class = "shutdown");
            }
        }
        signal.cancel();
    });
    (cancellation, relay)
}

fn tool_catalog(read_scope: ReadScope) -> &'static [Tool; 4] {
    static NORMAL_TOOLS: OnceLock<[Tool; 4]> = OnceLock::new();
    static UNRESTRICTED_TOOLS: OnceLock<[Tool; 4]> = OnceLock::new();
    let tools = match read_scope {
        ReadScope::Normal => &NORMAL_TOOLS,
        ReadScope::Unrestricted => &UNRESTRICTED_TOOLS,
    };
    tools.get_or_init(|| {
        [
            read_tool(read_scope),
            grep_tool(read_scope),
            glob_tool(read_scope),
            run_process_tool(),
        ]
    })
}

fn read_tool(read_scope: ReadScope) -> Tool {
    let (description, path_description) = match read_scope {
        ReadScope::Normal => (
            "Read a local repository or Codex extension source file as numbered text lines. Absolute paths may address configured Codex skill and plugin directories.",
            "Platform-native repository path or absolute path under a configured Codex skill or plugin directory.",
        ),
        ReadScope::Unrestricted => (
            "Read a local filesystem source file as numbered text lines. Relative paths use the repository root; absolute paths may address supported locations outside it.",
            "Platform-native regular file path. Relative paths use the repository root; absolute paths may address supported local filesystems.",
        ),
    };
    Tool::new(
        "read",
        description,
        schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "encoding": {
                    "type": "string",
                    "description": "Optional WHATWG encoding label. A BOM takes precedence; otherwise valid UTF-8 is used when omitted."
                },
                "line_count": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 2000,
                    "description": "Maximum number of lines to return."
                },
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": path_description
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 1,
                    "description": "One-based first line to return."
                }
            },
            "required": ["path"]
        })),
    )
    .with_annotations(read_only_annotations())
}

fn grep_tool(read_scope: ReadScope) -> Tool {
    let (description, path_description, glob_description) = match read_scope {
        ReadScope::Normal => (
            "Search local repository or Codex extension contents using Rust regex or fixed strings. Results are deterministic and continue with an explicit offset.",
            "Optional platform-native repository path or absolute path under a configured Codex skill or plugin directory.",
            "Optional case-sensitive glob relative to the repository or requested Codex extension path.",
        ),
        ReadScope::Unrestricted => (
            "Search local filesystem contents using Rust regex or fixed strings. Relative paths use the repository root; absolute paths may address supported locations outside it.",
            "Optional platform-native file or directory path. Relative paths use the repository root; absolute paths may address supported local filesystems.",
            "Optional case-sensitive glob over repository-root-relative paths, or request-path-relative paths for external absolute inputs.",
        ),
    };
    Tool::new(
        "grep",
        description,
        schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "case": {
                    "type": "string",
                    "enum": ["smart", "sensitive", "insensitive"],
                    "default": "smart"
                },
                "context_lines": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 20,
                    "default": 0
                },
                "fixed_strings": {
                    "type": "boolean",
                    "default": false
                },
                "glob": {
                    "type": "string",
                    "description": glob_description
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "default": 200
                },
                "mode": {
                    "type": "string",
                    "enum": ["content", "files", "count"],
                    "default": "content"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0
                },
                "path": {
                    "type": "string",
                    "description": path_description
                },
                "pattern": {
                    "type": "string",
                    "description": "Rust regex by default, or a literal string when fixed_strings is true."
                }
            },
            "required": ["pattern"]
        })),
    )
    .with_annotations(read_only_annotations())
}

fn glob_tool(read_scope: ReadScope) -> Tool {
    let (description, path_description, pattern_description) = match read_scope {
        ReadScope::Normal => (
            "Find local repository or Codex extension file paths using a glob pattern. Results use native absolute paths and continue with an explicit offset.",
            "Platform-native repository directory or absolute directory under a configured Codex skill or plugin root.",
            "Case-sensitive glob relative to the repository or requested Codex extension directory.",
        ),
        ReadScope::Unrestricted => (
            "Find local filesystem paths using a glob pattern. Relative paths use the repository root; absolute paths may address supported locations outside it.",
            "Platform-native directory to traverse. Relative paths use the repository root; absolute paths may address supported local filesystems.",
            "Case-sensitive glob over repository-root-relative paths, or request-path-relative paths for external absolute inputs.",
        ),
    };
    Tool::new(
        "glob",
        description,
        schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "include_ignored": {
                    "type": "boolean",
                    "default": false
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "default": 200
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0
                },
                "path": {
                    "type": "string",
                    "default": ".",
                    "description": path_description
                },
                "pattern": {
                    "type": "string",
                    "minLength": 1,
                    "description": pattern_description
                }
            },
            "required": ["pattern"]
        })),
    )
    .with_annotations(read_only_annotations())
}

fn run_process_tool() -> Tool {
    Tool::new(
        "run_process",
        "Run one local program with structured arguments without accepting a PowerShell or shell command string. This is an open-world, destructive operation and may require approval.",
        schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "default": [],
                    "description": "Literal argv elements; do not add shell quoting."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional platform-native absolute or repository-root-relative working directory; defaults to the repository root."
                },
                "env": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "default": {},
                    "description": "Environment variables to override."
                },
                "program": {
                    "type": "string",
                    "minLength": 1,
                    "description": "One program name or executable path, never a command string."
                },
                "stdin": {
                    "type": ["string", "null"],
                    "maxLength": 1_048_576,
                    "description": "Optional UTF-8 standard input, closed after writing."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 300_000,
                    "default": 120_000
                },
                "unset_env": {
                    "type": "array",
                    "items": { "type": "string" },
                    "default": [],
                    "description": "Inherited environment variables to remove."
                }
            },
            "required": ["program"]
        })),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            .idempotent(false)
            .open_world(true),
    )
}

fn schema(value: Value) -> Arc<JsonObject> {
    let Value::Object(object) = value else {
        panic!("tool schema must be an object");
    };
    Arc::new(object)
}

fn read_only_annotations() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(true)
        .destructive(false)
        .open_world(false)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rmcp::model::{CallToolResponse, ContentBlock};

    use super::{CodexShim, ProtocolCompatibility, process_queue_timeout_message, tool_error};
    use crate::output::MODEL_BYTE_LIMIT;

    #[test]
    fn protocol_compatibility_accepts_only_explicit_levels() {
        assert_eq!(
            ProtocolCompatibility::default(),
            ProtocolCompatibility::Legacy
        );
        assert_eq!(
            "strict".parse::<ProtocolCompatibility>().expect("strict"),
            ProtocolCompatibility::Strict
        );
        assert_eq!(
            "legacy".parse::<ProtocolCompatibility>().expect("legacy"),
            ProtocolCompatibility::Legacy
        );
        assert!("auto".parse::<ProtocolCompatibility>().is_err());
        assert!("LEGACY".parse::<ProtocolCompatibility>().is_err());
    }

    #[test]
    fn process_queue_timeout_does_not_claim_process_diagnostics() {
        let message = process_queue_timeout_message(25);
        assert!(message.contains("no child was started"));
        for field in ["Resolved program:", "Launcher:", "Cwd:", "Exit code:"] {
            assert!(!message.contains(field));
        }
    }

    #[test]
    fn tool_errors_are_bounded() {
        let CallToolResponse::Complete(result) = tool_error("界".repeat(40_000)) else {
            panic!("tool error must be complete");
        };
        let ContentBlock::Text(content) = &result.content[0] else {
            panic!("tool error must contain text");
        };
        let text = &content.text;
        assert!(text.ends_with("...[diagnostic truncated]"));
        assert!(text.len() <= MODEL_BYTE_LIMIT);
    }

    #[test]
    fn root_capability_blocks_parent_escape() {
        let fixture = tempfile::tempdir().expect("create fixture");
        let root = fixture.path().join("root");
        fs::create_dir(&root).expect("create root");
        fs::write(fixture.path().join("outside.txt"), "outside").expect("write outside");
        let server = CodexShim::from_path(&root).expect("open root");

        let error = server
            .root
            .capability()
            .read_to_string("../outside.txt")
            .expect_err("parent escape must fail");
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
        ));
    }

    #[cfg(unix)]
    #[test]
    fn root_capability_blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("create fixture");
        let root = fixture.path().join("root");
        fs::create_dir(&root).expect("create root");
        let outside = fixture.path().join("outside.txt");
        fs::write(&outside, "outside").expect("write outside");
        symlink(&outside, root.join("escape")).expect("create symlink");
        let server = CodexShim::from_path(&root).expect("open root");

        server
            .root
            .capability()
            .read_to_string("escape")
            .expect_err("symlink escape must fail");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn root_handle_preserves_repository_identity() {
        let fixture = tempfile::tempdir().expect("create fixture");
        let root = fixture.path().join("root");
        let moved = fixture.path().join("moved");
        fs::create_dir(&root).expect("create root");
        fs::write(root.join("identity.txt"), "original").expect("write original");
        let server = CodexShim::from_path(&root).expect("open root");

        #[cfg(unix)]
        {
            fs::rename(&root, &moved).expect("move original root");
            fs::create_dir(&root).expect("create replacement root");
            fs::write(root.join("identity.txt"), "replacement").expect("write replacement");
        }
        #[cfg(windows)]
        {
            let error =
                fs::rename(&root, &moved).expect_err("held Windows root blocks replacement");
            assert!(
                matches!(error.raw_os_error(), Some(5 | 32)),
                "unexpected Windows root rename error: {error}"
            );
        }

        assert_eq!(
            server
                .root
                .capability()
                .read_to_string("identity.txt")
                .expect("read held root"),
            "original"
        );
    }
}
