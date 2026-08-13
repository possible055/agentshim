use std::{
    borrow::Cow,
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, DiscoverResult, Implementation,
        InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
        ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    output::{BurstOutputGate, CallOutputBudget, OutputTokenGate},
    path::{FileAccess, ReadScope, RepositoryRoot},
    runtime::{RuntimeConfig, RuntimeResources},
    tools::{
        bash::{detached::DetachedTrees, locate::BashLocator},
        exec::{ProcessError, ProcessResolver},
        run_program::ProcessRequest,
    },
};

#[cfg(test)]
use super::response::{queue_timeout_message, tool_error};
use super::{
    catalog::tool_catalog,
    dispatch::{ToolAdmissionFailure, shell_delegate},
    response::{diagnostic_tool_error, resource_busy, resource_busy_with_message},
};
#[cfg(test)]
use super::{
    dispatch::ToolAdmission,
    response::{blocking_response, pdf_busy, pdf_timeout},
};

pub const SERVER_INSTRUCTIONS: &str = "Local repository and Codex extension tools for reading source files, searching contents, finding paths, running one program with literal arguments, and running POSIX bash command lines.";
pub const UNRESTRICTED_SERVER_INSTRUCTIONS: &str = "Local filesystem tools for reading files, searching contents, and finding paths, plus one program with literal arguments and POSIX bash command lines. Read scope is the structured access range of read, grep, and glob; it does not bound what a spawned process can reach.";

const SUPPORTED_PROTOCOLS: &[ProtocolVersion] = &[
    ProtocolVersion::V_2026_07_28,
    ProtocolVersion::V_2025_11_25,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_03_26,
    ProtocolVersion::V_2024_11_05,
];

#[derive(Clone)]
pub struct CodexShim {
    pub(super) root: Arc<RepositoryRoot>,
    pub(super) file_access: Arc<FileAccess>,
    pub(super) resources: RuntimeResources,
    pub(super) process_resolver: ProcessResolver,
    pub(super) detached: DetachedTrees,
    pub(super) bash_locator: BashLocator,
    pub(super) output_token_gate: Arc<OutputTokenGate>,
    pub(super) burst_output_gate: Arc<BurstOutputGate>,
}

pub struct CodexShimBuilder {
    root: PathBuf,
    read_scope: ReadScope,
    runtime: RuntimeConfig,
}

impl CodexShimBuilder {
    /// Resolve server defaults from the environment for an explicit repository root.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error when runtime environment settings are invalid.
    pub fn from_env(root: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self {
            root: root.into(),
            read_scope: ReadScope::default(),
            runtime: RuntimeConfig::from_env()?,
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

    /// Open the repository capability and build the MCP service.
    ///
    /// # Errors
    ///
    /// Returns the repository root validation or capability-open error.
    pub fn build(self) -> io::Result<CodexShim> {
        let root = Arc::new(RepositoryRoot::open(self.root)?);
        let output_token_gate = OutputTokenGate::load_shared().map_err(io::Error::other)?;
        let burst_output_gate = BurstOutputGate::new(crate::output::configured_burst_tokens()?);
        Ok(CodexShim {
            file_access: Arc::new(FileAccess::new(Arc::clone(&root), self.read_scope)),
            root,
            detached: DetachedTrees::new(self.runtime.detached_calls),
            bash_locator: BashLocator::capture(),
            resources: RuntimeResources::new(self.runtime),
            process_resolver: ProcessResolver::capture(),
            output_token_gate,
            burst_output_gate,
        })
    }
}

impl CodexShim {
    /// Create a builder using environment-derived runtime defaults.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error when runtime environment settings are invalid.
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
    pub fn burst_token_limit(&self) -> usize {
        self.burst_output_gate.limit()
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
        Self::discovery_result_for(ReadScope::default())
    }

    fn discovery_result_for(read_scope: ReadScope) -> DiscoverResult {
        DiscoverResult::from_server_info(
            SUPPORTED_PROTOCOLS.to_vec(),
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
}

impl ServerHandler for CodexShim {
    fn get_info(&self) -> ServerInfo {
        Self::server_info(self.read_scope())
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOLS)
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        tracing::info!(target: "codexshim", event = "initialize", phase = "protocol", protocol = %request.protocol_version, client_name = %request.client_info.name, client_version = %request.client_info.version);
        context.peer.set_peer_info(request);
        Ok(Self::server_info(self.read_scope()))
    }

    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, McpError> {
        tracing::info!(target: "codexshim", event = "discover", phase = "protocol");
        Ok(Self::discovery_result_for(self.read_scope()))
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
        let tool = request.name.to_string();
        let budget = CallOutputBudget::new(
            Arc::clone(&self.output_token_gate),
            self.burst_output_gate.begin_call(),
        );
        let result = self.call_tool_inner(request, &context, &budget).await;
        super::response::finalize_tool_response(&tool, &budget, result, &context.ct)
    }
}

impl CodexShim {
    async fn call_tool_inner(
        &self,
        request: CallToolRequestParams,
        context: &RequestContext<RoleServer>,
        budget: &CallOutputBudget,
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
            return self
                .dispatch_tool(request, context, admission, budget)
                .await;
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
            let response = self
                .dispatch_tool(request, context, admission, budget)
                .await?;
            Ok(response)
        }
        .instrument(span)
        .await
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
