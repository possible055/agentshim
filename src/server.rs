use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt::Display,
    io,
    path::Path,
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

use crate::{
    path::RepositoryRoot,
    runtime::{RuntimeConfig, RuntimeResources},
    tools::process::{ProcessRequest, ProcessResolver},
};

pub const SERVER_INSTRUCTIONS: &str = "Local repository tools for reading source files, searching contents, finding paths, and running programs with structured arguments without PowerShell command strings.";

#[derive(Clone)]
pub struct CodexShim {
    root: Arc<RepositoryRoot>,
    resources: RuntimeResources,
    process_resolver: ProcessResolver,
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

    /// Open the current repository with an already configured shared runtime budget.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the current directory or repository root cannot be opened.
    pub fn from_current_dir_with_resources(resources: RuntimeResources) -> io::Result<Self> {
        Self::from_path_with_resources(std::env::current_dir()?, resources)
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

    fn from_path_with_resources(
        path: impl AsRef<Path>,
        resources: RuntimeResources,
    ) -> io::Result<Self> {
        Ok(Self {
            root: Arc::new(RepositoryRoot::open(path)?),
            resources,
            process_resolver: ProcessResolver::capture(),
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
        DiscoverResult::from_server_info(vec![ProtocolVersion::V_2026_07_28], Self::server_info())
    }

    #[must_use]
    pub fn tools_result() -> ListToolsResult {
        ListToolsResult::with_all_items(tool_catalog().to_vec())
            .with_ttl_ms(300_000)
            .with_cache_scope(CacheScope::Private)
    }

    async fn call_read(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
    ) -> CallToolResponse {
        let read_request = match parse_request(arguments, "read") {
            Ok(request) => request,
            Err(error) => return tool_error(error),
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
                return tool_error("read cancelled while waiting for bounded runtime capacity");
            }
        };
        let root = self.root.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let result = tokio::task::spawn_blocking(move || {
            let result = crate::tools::read::execute(&root, &read_request, &cancellation);
            drop(permits);
            result
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
        let glob_request = match parse_request(arguments, "glob") {
            Ok(request) => request,
            Err(error) => return tool_error(error),
        };
        let read_only = self.resources.acquire_read_only(request_cancellation).await;
        let worker = self.resources.acquire_worker(request_cancellation).await;
        let memory = self
            .resources
            .reserve_memory(32 * 1024 * 1024, request_cancellation)
            .await;
        let permits = match (read_only, worker, memory) {
            (Ok(read_only), Ok(worker), Ok(memory)) => (read_only, worker, memory),
            _ => {
                return tool_error("glob cancelled while waiting for bounded runtime capacity");
            }
        };
        let root = self.root.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let result = tokio::task::spawn_blocking(move || {
            let result = crate::tools::glob::execute(&root, &glob_request, &cancellation);
            drop(permits);
            result
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
        let grep_request = match parse_request(arguments, "grep") {
            Ok(request) => request,
            Err(error) => return tool_error(error),
        };
        let lanes = self.resources.config().worker_lanes;
        let read_only = self.resources.acquire_read_only(request_cancellation).await;
        let workers = self
            .resources
            .acquire_workers(lanes, request_cancellation)
            .await;
        let open_files = self
            .resources
            .acquire_open_files(lanes, request_cancellation)
            .await;
        let memory = self
            .resources
            .reserve_memory(8 * 1024 * 1024, request_cancellation)
            .await;
        let permits = match (read_only, workers, open_files, memory) {
            (Ok(read_only), Ok(workers), Ok(open_files), Ok(memory)) => {
                (read_only, workers, open_files, memory)
            }
            _ => {
                return tool_error("grep cancelled while waiting for bounded runtime capacity");
            }
        };
        let root = self.root.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let result = tokio::task::spawn_blocking(move || {
            let result = crate::tools::grep::execute(&root, &grep_request, lanes, &cancellation);
            drop(permits);
            result
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
            Err(error) => return tool_error(error),
        };
        if let Err(error) = process_request.validate() {
            return tool_error(error.to_string());
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
                return tool_error("run_process cancelled while waiting for process capacity");
            }
            Err(_) => return process_queue_timeout(process_request.timeout_ms()),
        };
        let Some(remaining) = timeout.checked_sub(queued.elapsed()) else {
            return process_queue_timeout(process_request.timeout_ms());
        };
        let root = self.root.clone();
        let resolver = self.process_resolver.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let result = tokio::task::spawn_blocking(move || {
            let result = crate::tools::process::execute(
                &root,
                &resolver,
                &process_request,
                remaining,
                &cancellation,
            );
            drop(permits);
            result
        })
        .await;
        cancellation_relay.abort();
        blocking_response("run_process", result)
    }

    fn server_info() -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("codexshim", env!("CARGO_PKG_VERSION")))
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_instructions(SERVER_INSTRUCTIONS)
    }
}

impl ServerHandler for CodexShim {
    fn get_info(&self) -> ServerInfo {
        Self::server_info()
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }

    async fn initialize(
        &self,
        _request: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        Err(McpError::method_not_found::<InitializeResultMethod>())
    }

    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, McpError> {
        Ok(Self::discovery_result())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(Self::tools_result())
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_catalog()
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let response = match request.name.as_ref() {
            "read" => self.call_read(request.arguments, &context.ct).await,
            "glob" => self.call_glob(request.arguments, &context.ct).await,
            "grep" => self.call_grep(request.arguments, &context.ct).await,
            "run_process" => self.call_process(request.arguments, &context.ct).await,
            _ => {
                return Err(McpError::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!("unknown tool: {}", request.name),
                    None,
                ));
            }
        };
        Ok(response)
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
    CallToolResult::error(vec![ContentBlock::text(message.into())]).into()
}

fn blocking_response<E: Display>(
    tool: &str,
    result: Result<Result<String, E>, tokio::task::JoinError>,
) -> CallToolResponse {
    match result {
        Ok(Ok(text)) => CallToolResult::success(vec![ContentBlock::text(text)]).into(),
        Ok(Err(error)) => tool_error(error.to_string()),
        Err(error) => tool_error(format!("{tool} worker failed: {error}")),
    }
}

fn process_queue_timeout(timeout_ms: u64) -> CallToolResponse {
    tool_error(format!(
        "run_process timed out after {timeout_ms} ms while waiting for process capacity; no child was started"
    ))
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
            () = request.cancelled() => {}
            () = shutdown.cancelled() => {}
        }
        signal.cancel();
    });
    (cancellation, relay)
}

fn tool_catalog() -> &'static [Tool; 4] {
    static TOOLS: OnceLock<[Tool; 4]> = OnceLock::new();
    TOOLS.get_or_init(|| [read_tool(), grep_tool(), glob_tool(), run_process_tool()])
}

fn read_tool() -> Tool {
    Tool::new(
        "read",
        "Read a local repository source file as numbered text lines. Use start_line to continue a partial result without server-side cursor state.",
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
                    "description": "Absolute or repository-root-relative regular file path."
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

fn grep_tool() -> Tool {
    Tool::new(
        "grep",
        "Search local repository file contents using Rust regex or fixed strings. Results are deterministic and continue with an explicit offset.",
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
                    "description": "Optional case-sensitive glob over root-relative paths using / separators."
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
                    "description": "Optional absolute or repository-root-relative file or directory path."
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

fn glob_tool() -> Tool {
    Tool::new(
        "glob",
        "Find local repository file paths using a glob pattern. Results use native absolute paths and continue with an explicit offset.",
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
                    "description": "Absolute or repository-root-relative directory to traverse."
                },
                "pattern": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Case-sensitive glob over root-relative paths using / separators."
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
                    "description": "Optional working directory; defaults to the repository root."
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
                    "maximum": 290_000,
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
    use std::{fs, sync::Arc};

    use super::CodexShim;

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

    #[test]
    fn root_handle_survives_root_replacement() {
        let fixture = tempfile::tempdir().expect("create fixture");
        let root = fixture.path().join("root");
        let moved = fixture.path().join("moved");
        fs::create_dir(&root).expect("create root");
        fs::write(root.join("identity.txt"), "original").expect("write original");
        let server = CodexShim::from_path(&root).expect("open root");

        fs::rename(&root, &moved).expect("move original root");
        fs::create_dir(&root).expect("create replacement root");
        fs::write(root.join("identity.txt"), "replacement").expect("write replacement");

        assert_eq!(
            server
                .root
                .capability()
                .read_to_string("identity.txt")
                .expect("read held root"),
            "original"
        );
        assert!(Arc::strong_count(server.root.capability()) >= 1);
    }
}
