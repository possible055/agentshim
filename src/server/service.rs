use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt::Display,
    io,
    path::{Path, PathBuf},
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
use tokio::sync::OwnedSemaphorePermit;
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

pub struct CodexShimBuilder {
    root: PathBuf,
    read_scope: ReadScope,
    runtime: RuntimeConfig,
    protocol_compatibility: ProtocolCompatibility,
}

enum ToolAdmission {
    ReadOnly(OwnedSemaphorePermit),
    Process(OwnedSemaphorePermit),
    None,
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
        admission: OwnedSemaphorePermit,
    ) -> CallToolResponse {
        let queued = Instant::now();
        let read_request = match parse_request(arguments, "read") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        let worker = self.resources.acquire_worker(request_cancellation).await;
        let open_file = self.resources.acquire_open_file(request_cancellation).await;
        let memory = self
            .resources
            .reserve_memory(256 * 1024, request_cancellation)
            .await;
        let permits = match (worker, open_file, memory) {
            (Ok(worker), Ok(open_file), Ok(memory)) => (admission, worker, open_file, memory),
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
        let running = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result =
                    crate::tools::read::execute_output(&access, &read_request, &cancellation);
                drop(permits);
                result
            })
        })
        .await;
        cancellation_relay.abort();
        blocking_response("read", duration_ms(running.elapsed()), result)
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
            .reserve_memory(crate::tools::glob::memory_charge(), request_cancellation)
            .await;
        let permits = match (worker, memory) {
            (Ok(worker), Ok(memory)) => (admission, worker, memory),
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
        let running = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result =
                    crate::tools::glob::execute_output(&access, &glob_request, &cancellation);
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
        let grep_request = match parse_request(arguments, "grep") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        let requested_lanes = self.resources.config().worker_lanes;
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
        let permits = match (workers, open_files, memory) {
            (Ok(workers), Ok(open_files), Ok(memory)) => {
                (admission, workers, open_files, memory)
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
        let running = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = crate::tools::grep::execute_output(
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
        blocking_response("grep", duration_ms(running.elapsed()), result)
    }

    async fn call_process(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        admission: OwnedSemaphorePermit,
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
        let running = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = crate::tools::process::execute_output(
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
        blocking_response("run_process", duration_ms(running.elapsed()), result)
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
            ("run_process", ToolAdmission::Process(admission)) => Ok(self
                .call_process(request.arguments, &context.ct, admission)
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

    fn try_admit_tool(&self, name: &str) -> Result<ToolAdmission, ()> {
        match name {
            "read" | "glob" | "grep" => self
                .resources
                .try_admit_read_only()
                .map(ToolAdmission::ReadOnly)
                .ok_or(()),
            "run_process" => self
                .resources
                .try_admit_process()
                .map(ToolAdmission::Process)
                .ok_or(()),
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
        let Ok(admission) = self.try_admit_tool(request.name.as_ref()) else {
            return Ok(resource_busy(request.name.as_ref()));
        };
        if !tracing::enabled!(target: "codexshim", tracing::Level::INFO) {
            return self.dispatch_tool(request, &context, admission).await;
        }
        let call_id = Uuid::new_v4().to_string();
        let tool = request.name.to_string();
        let span =
            tracing::info_span!(target: "codexshim", "tool_call", call_id = %call_id, tool = %tool);
        async move {
            tracing::info!(target: "codexshim", event = "tool_start", phase = "request");
            let response = self.dispatch_tool(request, &context, admission).await?;
            Ok(response)
        }
        .instrument(span)
        .await
    }
}
