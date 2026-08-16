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
    service::{NotificationContext, RequestContext},
};
use serde_json::json;
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
    ToolsListCorrelation,
    catalog::tool_catalog,
    dispatch::{ToolAdmissionFailure, shell_delegate},
    response::{diagnostic_tool_error, resource_busy, resource_busy_with_message},
};
#[cfg(test)]
use super::{
    dispatch::ToolAdmission,
    response::{blocking_response, blocking_response_for_profile, pdf_busy, pdf_timeout},
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
pub(crate) use agentshim_core::dsh_bridge::DSH_BRIDGE_VERSION;

static MAX_TIMEOUT_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Install the MCP shell's process-wide max execution time from the configured shelf.
/// Later calls are ignored because the tool catalog may already have cached schemas
/// built from the first value.
pub(crate) fn install_max_timeout_ms(shelf: std::time::Duration) {
    let _ =
        MAX_TIMEOUT_MS.set(agentshim_core::tools::exec::spawn::max_timeout_ms_from_shelf(shelf));
}

/// The MCP shell's max execution time in milliseconds; before `install_max_timeout_ms`
/// runs this is the default-shelf derivation.
pub(crate) fn max_timeout_ms() -> u64 {
    *MAX_TIMEOUT_MS
        .get()
        .unwrap_or(&agentshim_core::tools::exec::spawn::default_max_timeout_ms())
}

/// The per-call timeout used when the caller omits `timeout_ms`, clamped below the
/// installed maximum.
pub(crate) fn default_timeout_ms() -> u64 {
    agentshim_core::tools::exec::spawn::DEFAULT_TIMEOUT_MS.min(max_timeout_ms())
}

#[derive(Clone)]
pub struct AgentShim {
    pub(super) root: Arc<RepositoryRoot>,
    pub(super) file_access: Arc<FileAccess>,
    pub(super) resources: RuntimeResources,
    pub(super) process_resolver: ProcessResolver,
    pub(super) detached: DetachedTrees,
    pub(super) bash_locator: BashLocator,
    pub(super) output_token_gate: Option<Arc<OutputTokenGate>>,
    pub(super) burst_output_gate: Option<Arc<BurstOutputGate>>,
    shutdown_execution: Arc<ShutdownExecution>,
    client_profile: crate::ClientProfile,
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

    /// Select the client-specific aggregate output defaults.
    #[must_use]
    pub fn client_profile(mut self, client_profile: crate::ClientProfile) -> Self {
        self.client_profile = client_profile;
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
        runtime.tool_timeout_shelf =
            crate::profile::resolve_tool_timeout_shelf(self.client_profile);
        runtime.idle_timeout =
            crate::profile::resolve_idle_timeout_from(runtime.idle_timeout, self.client_profile);
        install_max_timeout_ms(runtime.tool_timeout_shelf);
        crate::output::install_output_profile(self.client_profile);
        let (output_token_gate, burst_output_gate) =
            if self.client_profile == crate::ClientProfile::Dsh {
                (None, None)
            } else {
                (
                    Some(OutputTokenGate::load_shared().map_err(io::Error::other)?),
                    Some(BurstOutputGate::new(
                        crate::output::configured_burst_tokens(self.client_profile)?,
                    )),
                )
            };
        Ok(AgentShim {
            file_access: Arc::new(FileAccess::new(Arc::clone(&root), self.read_scope)),
            root,
            detached: DetachedTrees::new(runtime.detached_calls),
            bash_locator: BashLocator::capture(),
            resources: RuntimeResources::new(runtime),
            process_resolver: ProcessResolver::capture(),
            output_token_gate,
            burst_output_gate,
            shutdown_execution: Arc::new(ShutdownExecution::new()),
            client_profile: self.client_profile,
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
        self.burst_output_gate
            .as_ref()
            .map_or(0, |gate| gate.limit())
    }

    /// Return the fixed token ceiling applied to one tool response.
    #[must_use]
    pub fn tool_output_token_limit(&self) -> usize {
        crate::output::CALL_OUTPUT_TOKEN_LIMIT
    }

    /// Return the selected client output profile.
    #[must_use]
    pub fn client_profile(&self) -> crate::ClientProfile {
        self.client_profile
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
            capture: None,
        };
        let output = crate::tools::run_program::execute_output_with_capture(
            &self.root,
            &self.process_resolver,
            &request,
            Duration::from_secs(5),
            &CancellationToken::new(),
            max_timeout_ms(),
            &crate::output::CallOutputBudget::standalone(),
            None,
        )
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
            Self::server_info(read_scope, None),
        )
    }

    #[must_use]
    pub fn tools_result() -> ListToolsResult {
        Self::tools_result_for(ReadScope::default())
    }

    #[must_use]
    pub fn tools_result_for(read_scope: ReadScope) -> ListToolsResult {
        Self::tools_result_for_profile(read_scope, crate::ClientProfile::Codex)
    }

    fn tools_result_for_profile(
        read_scope: ReadScope,
        client_profile: crate::ClientProfile,
    ) -> ListToolsResult {
        ListToolsResult::with_all_items(tool_catalog(read_scope, client_profile).to_vec())
            .with_ttl_ms(TOOLS_CACHE_TTL_MS)
            .with_cache_scope(CacheScope::Private)
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

    fn server_info(
        read_scope: ReadScope,
        client_profile: Option<crate::ClientProfile>,
    ) -> ServerInfo {
        let instructions = match read_scope {
            ReadScope::Normal => SERVER_INSTRUCTIONS,
            ReadScope::Unrestricted => UNRESTRICTED_SERVER_INSTRUCTIONS,
        };
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agentshim", env!("CARGO_PKG_VERSION")))
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_instructions(instructions);
        if client_profile == Some(crate::ClientProfile::Dsh) {
            let mut meta = serde_json::Map::new();
            meta.insert(
                "agentshim.dshBridge".to_owned(),
                json!({ "version": DSH_BRIDGE_VERSION }),
            );
            info.meta = Some(rmcp::model::MetaObject(meta));
        }
        info
    }
}

impl ServerHandler for AgentShim {
    fn get_info(&self) -> ServerInfo {
        Self::server_info(self.read_scope(), Some(self.client_profile))
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
        Ok(Self::server_info(
            self.read_scope(),
            Some(self.client_profile),
        ))
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
        let result = Self::tools_result_for_profile(self.read_scope(), self.client_profile);
        if let Some(correlation) = context.extensions.get::<ToolsListCorrelation>() {
            tracing::info!(
                target: "agentshim",
                event = "tools_list",
                phase = "protocol",
                outcome = "success",
                protocol,
                client_name,
                client_version,
                request_id = correlation.0,
                tool_count = TOOL_COUNT,
                toolset = TOOLSET,
                has_cursor,
                cache_ttl_ms = TOOLS_CACHE_TTL_MS,
                cache_scope = TOOLS_CACHE_SCOPE
            );
        } else {
            tracing::info!(
                target: "agentshim",
                event = "tools_list",
                phase = "protocol",
                outcome = "success",
                protocol,
                client_name,
                client_version,
                tool_count = TOOL_COUNT,
                toolset = TOOLSET,
                has_cursor,
                cache_ttl_ms = TOOLS_CACHE_TTL_MS,
                cache_scope = TOOLS_CACHE_SCOPE
            );
        }
        Ok(result)
    }

    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {
        tracing::info!(target: "agentshim", event = "initialized", phase = "protocol");
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_catalog(self.read_scope(), self.client_profile)
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
        let budget = match (&self.output_token_gate, &self.burst_output_gate) {
            (Some(token_gate), Some(burst_gate)) => {
                CallOutputBudget::new(Arc::clone(token_gate), burst_gate.begin_call())
            }
            (None, None) => CallOutputBudget::dsh(),
            _ => unreachable!("model token and burst gates are installed together"),
        };
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
    let deadline = started + crate::tools::exec::spawn::CLEANUP_DEADLINE;
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
#[path = "tests.rs"]
mod tests;
