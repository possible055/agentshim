fn parse_request<T: DeserializeOwned>(
    arguments: Option<JsonObject>,
    tool: &str,
) -> Result<T, String> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| format!("invalid {tool} request: {error}"))
}

fn tool_error(
    code: &'static str,
    retryable: bool,
    message: impl Into<String>,
    details: Option<&Value>,
) -> CallToolResponse {
    let mut details = details.cloned();
    if let Some(details) = &mut details {
        bound_detail_strings(details);
    }
    let (message, structured) = bounded_error_payload(
        code,
        retryable,
        &message.into(),
        details.as_ref(),
    );
    let mut result = CallToolResult::error(vec![ContentBlock::text(message)]);
    result.structured_content = Some(structured);
    result.into()
}

fn bounded_error_payload(
    code: &'static str,
    retryable: bool,
    message: &str,
    details: Option<&Value>,
) -> (String, Value) {
    let bounded = bounded_diagnostic(message);
    let structured = crate::output::tool_error_structure(code, retryable, &bounded, details);
    if crate::output::tool_result_fits_budget(&bounded, Some(&structured), true) {
        return (bounded, structured);
    }
    let marker = "...[truncated]";
    let boundaries = bounded
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(bounded.len()))
        .collect::<Vec<_>>();
    let mut low = 0_usize;
    let mut high = boundaries.len();
    let mut best = marker.to_owned();
    while low < high {
        let midpoint = low + (high - low) / 2;
        let end = boundaries[midpoint];
        let candidate = format!("{}{marker}", &bounded[..end]);
        let structured =
            crate::output::tool_error_structure(code, retryable, &candidate, details);
        if crate::output::tool_result_fits_budget(&candidate, Some(&structured), true) {
            best = candidate;
            low = midpoint + 1;
        } else {
            high = midpoint;
        }
    }
    let structured = crate::output::tool_error_structure(code, retryable, &best, details);
    if crate::output::tool_result_fits_budget(&best, Some(&structured), true) {
        return (best, structured);
    }
    let structured = crate::output::tool_error_structure(code, retryable, marker, None);
    debug_assert!(crate::output::tool_result_fits_budget(
        marker,
        Some(&structured),
        true
    ));
    (marker.to_owned(), structured)
}

fn bound_detail_strings(value: &mut Value) {
    const LIMIT: usize = 2_048;
    const MARKER: &str = "...[truncated]";
    match value {
        Value::String(text) if text.len() > LIMIT => {
            let end = floor_char_boundary(text, LIMIT - MARKER.len());
            text.truncate(end);
            text.push_str(MARKER);
        }
        Value::Array(values) => values.iter_mut().for_each(bound_detail_strings),
        Value::Object(values) => values.values_mut().for_each(bound_detail_strings),
        _ => {}
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

trait DiagnosticError: Display {
    fn error_class(&self) -> &'static str;

    fn retryable(&self) -> bool {
        matches!(
            self.error_class(),
            "io" | "resource_timeout" | "resource_busy"
        )
    }

    fn details(&self) -> Option<Value> {
        None
    }
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
            #[cfg(any(test, feature = "bench-internals"))]
            GrepError::CandidateMemory => "resource_timeout",
            GrepError::PoolPoison | GrepError::CaptureMemory => "resource_timeout",
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

    fn details(&self) -> Option<Value> {
        use crate::tools::process::ProcessError;
        match self {
            ProcessError::Timeout { details, .. } => serde_json::to_value(details).ok(),
            ProcessError::TimeoutBeforeSpawn { timeout_ms } => Some(json!({
                "timeout_ms": timeout_ms,
                "termination_outcome": "not_started"
            })),
            ProcessError::OutcomeUncertain => Some(json!({
                "termination_outcome": "uncertain"
            })),
            _ => None,
        }
    }
}

fn classified_tool_error(
    error_class: &'static str,
    message: impl Into<String>,
) -> CallToolResponse {
    tracing::error!(target: "codexshim", event = "tool_error", phase = "response", outcome = "error", error_class);
    let retryable = matches!(error_class, "io" | "resource_timeout" | "resource_busy");
    tool_error(error_class, retryable, message, None)
}

fn blocking_response<E: DiagnosticError>(
    tool: &str,
    run_ms: u64,
    result: Result<Result<crate::tools::ToolOutput, E>, tokio::task::JoinError>,
) -> CallToolResponse {
    match result {
        Ok(Ok(output)) => {
            let outcome = if tool == "run_process" && output.child_nonzero {
                "child_nonzero"
            } else {
                "success"
            };
            if outcome == "child_nonzero" {
                tracing::warn!(target: "codexshim", event = "tool_complete", phase = "response", outcome, error_class = "child_nonzero", run_ms);
            } else {
                tracing::info!(target: "codexshim", event = "tool_complete", phase = "response", outcome, run_ms);
            }
            CallToolResult::success(vec![ContentBlock::text(output.text)]).into()
        }
        Ok(Err(error)) => {
            let error_class = error.error_class();
            let details = error.details();
            tracing::error!(target: "codexshim", event = "tool_error", phase = "response", outcome = "error", error_class);
            tool_error(
                error_class,
                error.retryable(),
                error.to_string(),
                details.as_ref(),
            )
        }
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

fn resource_busy(tool: &str) -> CallToolResponse {
    classified_tool_error(
        "resource_busy",
        format!("{tool} capacity is busy; retry the request later"),
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
            "Search local repository or Codex extension contents using Rust regex or fixed strings. Results are deterministic and expose structured numeric pagination.",
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
            "Find local repository or Codex extension file paths using a glob pattern. Results use native absolute paths and expose structured numeric pagination.",
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

include!("tests.rs");
