use std::{
    borrow::Cow,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, DiscoverResult, Implementation,
        InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
        ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
    },
    service::{NotificationContext, RequestContext},
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    output::{BurstOutputGate, CallOutputBudget, OutputTokenGate},
    path::{ReadScope, RepositoryRoot},
    runtime::{RuntimeConfig, RuntimeResources},
    tools::{bash::detached::DetachedTrees, exec::ProcessError},
};

#[cfg(test)]
use super::dispatch::ToolAdmission;
#[cfg(test)]
use super::response::tool_error;
use super::{
    ToolsListCorrelation,
    catalog::tool_catalog,
    dispatch::shell_delegate,
    response::{diagnostic_tool_error, resource_busy_with_message},
};

pub const SERVER_INSTRUCTIONS: &str = "Local repository and Codex extension tools for reading source files, searching contents, finding paths, running one program with literal arguments, and running POSIX bash command lines with instance-bound managed detached-job status and termination.";
pub const UNRESTRICTED_SERVER_INSTRUCTIONS: &str = "Local filesystem tools for reading files, searching contents, and finding paths, plus one program with literal arguments and POSIX bash command lines with instance-bound managed detached-job status and termination. Read scope is the structured access range of read, grep, and glob; it does not bound what a spawned process can reach.";

const SUPPORTED_PROTOCOLS: &[ProtocolVersion] = &[
    ProtocolVersion::V_2026_07_28,
    ProtocolVersion::V_2025_11_25,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_03_26,
    ProtocolVersion::V_2024_11_05,
];
const TOOLSET: &str = "read,grep,glob,run_program,bash,bash_status";
const TOOL_COUNT: u64 = 6;
const TOOLS_CACHE_TTL_MS: u64 = 300_000;
const TOOLS_CACHE_SCOPE: &str = "private";

#[derive(Clone, Copy, Debug)]
struct McpHostConfig {
    max_timeout_ms: u64,
    default_timeout_ms: u64,
    background_timeout_max_ms: u64,
    output_bytes: usize,
    read_scope: ReadScope,
    client_profile: crate::ClientProfile,
}

impl McpHostConfig {
    fn new(
        runtime: &RuntimeConfig,
        read_scope: ReadScope,
        client_profile: crate::ClientProfile,
    ) -> io::Result<Self> {
        if !(crate::output::MIN_OUTPUT_BYTES..=crate::output::MAX_OUTPUT_BYTES)
            .contains(&runtime.output_bytes)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "output_bytes must be from {} through {}",
                    crate::output::MIN_OUTPUT_BYTES,
                    crate::output::MAX_OUTPUT_BYTES
                ),
            ));
        }
        let max_timeout_ms =
            agentshim_core::tools::exec::max_timeout_ms_from_shelf(runtime.tool_timeout_shelf);
        Ok(Self {
            max_timeout_ms,
            default_timeout_ms: agentshim_core::tools::exec::DEFAULT_TIMEOUT_MS.min(max_timeout_ms),
            background_timeout_max_ms: u64::try_from(
                runtime.background_job_timeout_max.as_millis(),
            )
            .unwrap_or(u64::MAX),
            output_bytes: runtime.output_bytes,
            read_scope,
            client_profile,
        })
    }
}

#[derive(Clone)]
pub struct AgentShim {
    pub(super) root: Arc<RepositoryRoot>,
    pub(super) tool_engine: agentshim_core::ToolEngine,
    pub(super) resources: RuntimeResources,
    pub(super) detached: DetachedTrees,
    pub(super) output_token_gate: Arc<OutputTokenGate>,
    pub(super) burst_output_gate: Arc<BurstOutputGate>,
    catalog: Arc<[Tool; 6]>,
    host_config: McpHostConfig,
    shutdown_execution: Arc<ShutdownExecution>,
}

/// EOF, transport failure, initialize failure, and explicit shutdown can overlap, but they
/// must share one shutdown run and one report. The first caller starts the transaction on
/// a blocking task; everyone else — including callers whose await is cancelled — observes
/// the same completion through the watch channel, and a dropped wait never aborts the
/// cleanup itself.
struct ShutdownExecution {
    slot: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    completed: tokio::sync::watch::Sender<bool>,
    observed: tokio::sync::watch::Receiver<bool>,
}

impl ShutdownExecution {
    fn new() -> Self {
        let (completed, observed) = tokio::sync::watch::channel(false);
        Self {
            slot: tokio::sync::Mutex::new(None),
            completed,
            observed,
        }
    }
}

/// Signals completion even when the transaction task panics: waiters — including the CLI's
/// final await — must never hang on a cleanup that will never report.
struct CompleteOnDrop(tokio::sync::watch::Sender<bool>);

impl Drop for CompleteOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

pub struct AgentShimBuilder {
    root: PathBuf,
    read_scope: ReadScope,
    runtime: RuntimeConfig,
    client_profile: crate::ClientProfile,
}

impl AgentShimBuilder {
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
            client_profile: crate::ClientProfile::default(),
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

    /// Select client-specific timeout and aggregate output defaults.
    #[must_use]
    pub fn client_profile(mut self, client_profile: crate::ClientProfile) -> Self {
        self.client_profile = client_profile;
        self.runtime.tool_timeout_shelf =
            crate::profile::resolve_tool_timeout_shelf(client_profile);
        self
    }

    /// Open the repository capability and build the MCP service.
    ///
    /// # Errors
    ///
    /// Returns the repository root validation or capability-open error.
    pub fn build(self) -> io::Result<AgentShim> {
        let root = Arc::new(RepositoryRoot::open(self.root)?);
        let mut runtime = self.runtime;
        runtime.idle_timeout =
            crate::profile::resolve_idle_timeout_from(runtime.idle_timeout, self.client_profile);
        let host_config = McpHostConfig::new(&runtime, self.read_scope, self.client_profile)?;
        let catalog = Arc::new(tool_catalog(
            host_config.read_scope,
            host_config.max_timeout_ms,
            host_config.default_timeout_ms,
            host_config.background_timeout_max_ms,
        ));
        let output_token_gate = OutputTokenGate::load_shared().map_err(io::Error::other)?;
        let burst_output_gate =
            BurstOutputGate::new(crate::output::configured_burst_tokens(self.client_profile)?);
        let resources = RuntimeResources::new(runtime);
        let tool_engine =
            agentshim_core::ToolEngine::new(Arc::clone(&root), self.read_scope, resources.clone());
        Ok(AgentShim {
            tool_engine,
            root,
            detached: DetachedTrees::with_log_quota(
                runtime.detached_calls,
                runtime.detached_log_bytes,
            ),
            resources,
            output_token_gate,
            burst_output_gate,
            catalog,
            host_config,
            shutdown_execution: Arc::new(ShutdownExecution::new()),
        })
    }
}

impl AgentShim {
    /// Create a builder using environment-derived runtime defaults.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error when runtime environment settings are invalid.
    pub fn builder(root: impl Into<PathBuf>) -> io::Result<AgentShimBuilder> {
        AgentShimBuilder::from_env(root)
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

    /// Return the fixed token ceiling applied to one tool response.
    #[must_use]
    pub fn tool_output_token_limit(&self) -> usize {
        crate::output::CALL_OUTPUT_TOKEN_LIMIT
    }

    /// Return the selected client output profile.
    #[must_use]
    pub fn client_profile(&self) -> crate::ClientProfile {
        self.host_config.client_profile
    }

    #[must_use]
    pub fn shutdown_token(&self) -> CancellationToken {
        self.resources.shutdown_token()
    }

    /// Whether no foreground tool call is in flight and no detached tree is live. The
    /// idle watchdog requires this before cancelling the shutdown token: in-flight work
    /// counts as activity even while the client itself stays quiet.
    #[must_use]
    pub fn is_idle_quiescent(&self) -> bool {
        !self.resources.has_in_flight_calls() && self.detached.live_tree_count() == 0
    }

    /// The single, re-entrant server-level process shutdown. The first caller runs one
    /// transaction — cancel the global token, stop roster admission, terminate committed
    /// trees in parallel, wait for foreground owners and reservations to drain — under one
    /// shared deadline; later and concurrent callers observe the same result. Cancelling
    /// the awaiting future never cancels the cleanup itself.
    pub async fn shutdown_processes(&self) {
        let mut slot = self.shutdown_execution.slot.lock().await;
        if slot.is_none() {
            let resources = self.resources.clone();
            let detached = self.detached.clone();
            let completed = self.shutdown_execution.completed.clone();
            *slot = Some(tokio::task::spawn_blocking(move || {
                let completion = CompleteOnDrop(completed);
                shutdown_transaction(&resources, &detached);
                drop(completion);
            }));
        }
        let mut observed = self.shutdown_execution.observed.clone();
        drop(slot);
        let _ = observed.wait_for(|done| *done).await;
    }

    #[must_use]
    pub fn read_scope(&self) -> ReadScope {
        self.host_config.read_scope
    }

    /// Verify that the retained repository root remains accessible.
    ///
    /// # Errors
    ///
    /// Returns the capability-relative metadata error when the root is inaccessible.
    pub fn verify_root(&self) -> io::Result<()> {
        self.root.verify()
    }

    /// Probe the bash runtime once so a missing GNU bash surfaces at startup
    /// instead of mid-task. The result is cached on the service's locator, so
    /// the first `bash` tool call reuses it without re-probing.
    ///
    /// # Errors
    ///
    /// Returns the operator-facing explanation when no usable GNU bash was found.
    pub fn verify_bash(&self) -> Result<(), String> {
        self.tool_engine
            .verify_bash()
            .map_err(|error| error.to_string())
    }

    /// Spawn this binary through the platform process lifecycle and verify clean completion.
    ///
    /// # Errors
    ///
    /// Returns a resolution, launch, capture, cleanup, or unexpected-output error.
    pub async fn verify_process_runtime(&self) -> io::Result<()> {
        let executable = std::env::current_exe()?;
        let request = crate::tools::run_program::ProcessRequest {
            program: executable.to_string_lossy().into_owned(),
            args: vec!["--version".to_owned()],
            cwd: None,
            env: std::collections::BTreeMap::new(),
            unset_env: Vec::new(),
            stdin: None,
            timeout_ms: Some(5_000),
        };
        let cancellation = CancellationToken::new();
        let budget = Arc::new(crate::output::CallOutputBudget::standalone(
            self.host_config.output_bytes,
        ));
        let context = agentshim_core::OperationContext::new(cancellation.clone(), budget.clone());
        let prepared = self
            .tool_engine
            .prepare_run_program(&request, self.host_config.max_timeout_ms, &context)
            .map_err(io::Error::other)?;
        let output = self
            .tool_engine
            .spawn_run_program(prepared, None, context, None)
            .await
            .map_err(io::Error::other)?;
        if output.contains("Exit code: 0") && output.contains("agentshim ") {
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
        let max_timeout_ms = agentshim_core::tools::exec::default_max_timeout_ms();
        let default_timeout_ms =
            agentshim_core::tools::exec::DEFAULT_TIMEOUT_MS.min(max_timeout_ms);
        let background_timeout_max_ms =
            u64::try_from(agentshim_core::runtime::DEFAULT_BACKGROUND_JOB_TIMEOUT_MAX.as_millis())
                .unwrap_or(u64::MAX);
        ListToolsResult::with_all_items(
            tool_catalog(
                read_scope,
                max_timeout_ms,
                default_timeout_ms,
                background_timeout_max_ms,
            )
            .to_vec(),
        )
        .with_ttl_ms(TOOLS_CACHE_TTL_MS)
        .with_cache_scope(CacheScope::Private)
    }

    fn configured_tools_result(&self) -> ListToolsResult {
        ListToolsResult::with_all_items(self.catalog.to_vec())
            .with_ttl_ms(TOOLS_CACHE_TTL_MS)
            .with_cache_scope(CacheScope::Private)
    }

    pub(super) fn max_timeout_ms(&self) -> u64 {
        self.host_config.max_timeout_ms
    }

    pub(super) fn background_timeout_max_ms(&self) -> u64 {
        self.host_config.background_timeout_max_ms
    }

    fn call_output_budget(&self) -> CallOutputBudget {
        CallOutputBudget::new(
            self.host_config.output_bytes,
            Arc::clone(&self.output_token_gate),
            self.burst_output_gate.begin_call(),
        )
    }

    fn request_identity(context: &RequestContext<RoleServer>) -> (String, String, String) {
        let protocol = context
            .protocol_version()
            .map_or_else(|| "unknown".to_owned(), |version| version.to_string());
        let Some(client) = context.client_info() else {
            return (protocol, "unknown".to_owned(), "unknown".to_owned());
        };
        (protocol, client.name, client.version)
    }

    fn server_info(read_scope: ReadScope) -> ServerInfo {
        let instructions = match read_scope {
            ReadScope::Normal => SERVER_INSTRUCTIONS,
            ReadScope::Unrestricted => UNRESTRICTED_SERVER_INSTRUCTIONS,
        };
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agentshim", env!("CARGO_PKG_VERSION")))
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_instructions(instructions)
    }
}

impl ServerHandler for AgentShim {
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
        tracing::info!(target: "agentshim", event = "initialize", phase = "protocol", protocol = %request.protocol_version, client_name = %request.client_info.name, client_version = %request.client_info.version);
        context.peer.set_peer_info(request);
        Ok(Self::server_info(self.read_scope()))
    }

    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, McpError> {
        let (protocol, client_name, client_version) = Self::request_identity(&context);
        tracing::info!(target: "agentshim", event = "discover", phase = "protocol", protocol, client_name, client_version);
        Ok(Self::discovery_result_for(self.read_scope()))
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let (protocol, client_name, client_version) = Self::request_identity(&context);
        let has_cursor = request.is_some_and(|request| request.cursor.is_some());
        let result = self.configured_tools_result();
        let request_id = context
            .extensions
            .get::<ToolsListCorrelation>()
            .map(|correlation| correlation.0.clone());
        tracing::info!(
            target: "agentshim",
            event = "tools_list",
            phase = "protocol",
            outcome = "success",
            protocol,
            client_name,
            client_version,
            request_id,
            tool_count = TOOL_COUNT,
            toolset = TOOLSET,
            has_cursor,
            cache_ttl_ms = TOOLS_CACHE_TTL_MS,
            cache_scope = TOOLS_CACHE_SCOPE
        );
        Ok(result)
    }

    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {
        tracing::info!(target: "agentshim", event = "initialized", phase = "protocol");
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.catalog.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let tool = request.name.to_string();
        let budget = self.call_output_budget();
        let result = self.call_tool_inner(request, &context, &budget).await;
        super::response::finalize_tool_response(&tool, &budget, result, &context.ct)
    }
}

impl AgentShim {
    async fn call_tool_inner(
        &self,
        request: CallToolRequestParams,
        context: &RequestContext<RoleServer>,
        budget: &CallOutputBudget,
    ) -> Result<CallToolResponse, McpError> {
        let admission = match self.try_admit_tool(&request) {
            Ok(admission) => admission,
            Err(error) => {
                return Ok(match error {
                    ProcessError::ResourceBusy(message) => {
                        resource_busy_with_message(budget, "bash", "detached", message)
                    }
                    other => diagnostic_tool_error(budget, &other),
                });
            }
        };
        if !tracing::enabled!(target: "agentshim", tracing::Level::INFO) {
            return self
                .dispatch_tool(request, context, admission, budget)
                .await;
        }
        let call_id = Uuid::new_v4().to_string();
        let tool = request.name.to_string();
        let span = if request.name.as_ref() == "bash" {
            let shell_delegate = shell_delegate(&request);
            tracing::info_span!(
                target: "agentshim",
                "tool_call",
                call_id = %call_id,
                tool = %tool,
                shell_delegate
            )
        } else {
            tracing::info_span!(
                target: "agentshim",
                "tool_call",
                call_id = %call_id,
                tool = %tool
            )
        };
        async move {
            tracing::info!(target: "agentshim", event = "tool_start", phase = "request");
            let response = self
                .dispatch_tool(request, context, admission, budget)
                .await?;
            Ok(response)
        }
        .instrument(span)
        .await
    }
}

/// One complete ownership shutdown, bounded by a single deadline counted from the token
/// cancellation rather than one budget per tree. Committed trees are terminated in
/// parallel outside the roster lock; a tree that cannot be confirmed dead is reported by
/// pid, and the RAII handle close stays the last resort instead of the mechanism.
fn shutdown_transaction(resources: &RuntimeResources, detached: &DetachedTrees) {
    let started = std::time::Instant::now();
    let deadline = started + crate::tools::exec::CLEANUP_DEADLINE;
    resources.cancel_shutdown();
    let trees = detached.begin_shutdown(deadline);
    let tree_count = trees.len();
    let mut remaining: Vec<u32> = Vec::new();
    std::thread::scope(|scope| {
        let outcomes = trees
            .into_iter()
            .map(|work| {
                let pid = work.pid();
                scope.spawn(move || {
                    let snapshot = work.run();
                    let verified =
                        snapshot.state == crate::tools::bash::status::JobState::Terminated;
                    (pid, verified)
                })
            })
            .collect::<Vec<_>>();
        for outcome in outcomes {
            match outcome.join() {
                Ok((_, true)) => {}
                Ok((pid, false)) => remaining.push(pid),
                Err(_) => remaining.push(u32::MAX),
            }
        }
    });
    let foreground_quiesced = resources.wait_for_process_quiescence(deadline);
    let reservations_drained = detached.wait_until_quiesced(deadline);
    remaining.extend(detached.shutdown_unverified_pids());
    remaining.sort_unstable();
    remaining.dedup();
    let outcome = if remaining.is_empty() && foreground_quiesced && reservations_drained {
        "verified"
    } else {
        "outcome_uncertain"
    };
    tracing::info!(
        target: "agentshim",
        event = "server_process_shutdown",
        phase = "lifecycle",
        outcome,
        tree_count,
        remaining_pids = ?remaining,
        foreground_quiesced,
        reservations_drained,
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
    );
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
