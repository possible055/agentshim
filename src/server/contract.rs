fn parse_request<T: DeserializeOwned>(
    arguments: Option<JsonObject>,
    tool: &str,
) -> Result<T, String> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| format!("invalid {tool} request: {error}"))
}

/// Which admission class a `bash` call belongs to has to be known before the request is parsed,
/// because parsing happens after admission. This reads the one field that decides it, exactly as
/// `BashRequest` does: literal `true`, nothing coerced. A disagreement with the parsed request is
/// still caught later, so this only has to be honest, not authoritative.
fn requests_detach(arguments: Option<&JsonObject>) -> bool {
    arguments.is_some_and(|arguments| arguments.get("detach") == Some(&Value::Bool(true)))
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
            ReadError::ResourceLimit(_) => "resource_limit",
            ReadError::Pdf(error) => match error.kind() {
                codexshim_pdf_read::PdfReadErrorKind::Invalid => "pdf_invalid",
                codexshim_pdf_read::PdfReadErrorKind::Unsupported => "pdf_unsupported",
                codexshim_pdf_read::PdfReadErrorKind::Encrypted => "pdf_encrypted",
                codexshim_pdf_read::PdfReadErrorKind::ResourceLimit => "resource_limit",
                codexshim_pdf_read::PdfReadErrorKind::Processing => "pdf_processing",
                codexshim_pdf_read::PdfReadErrorKind::Io => "io",
            },
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
            GlobError::TooManyMatches => "resource_timeout",
            GlobError::Memory => "resource_limit",
            GlobError::MemoryBusy => "resource_busy",
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
            GrepError::CandidateMemory => "resource_limit",
            GrepError::MemoryBusy => "resource_busy",
            GrepError::PoolPoison | GrepError::CaptureMemory => "resource_timeout",
            GrepError::Traversal(_) | GrepError::Io(_) => "io",
        }
    }
}

impl DiagnosticError for crate::tools::exec::ProcessError {
    fn error_class(&self) -> &'static str {
        use crate::tools::exec::ProcessError;
        match self {
            ProcessError::Validation(_) => "validation",
            ProcessError::Resolve(_) => "path",
            ProcessError::NotPermitted(_) => "not_permitted",
            ProcessError::ResourceBusy(_) => "resource_busy",
            ProcessError::Io(_) | ProcessError::Unavailable(_) => "io",
            ProcessError::Timeout { .. } | ProcessError::TimeoutBeforeSpawn { .. } => {
                "resource_timeout"
            }
            ProcessError::Cancelled => "client_cancellation",
            ProcessError::OutcomeUncertain => "outcome_uncertain",
            ProcessError::Output(_) => "output_invariant",
        }
    }

    /// `io` is retryable by default because most of it is transient. A missing interpreter is
    /// not: the answer is fixed for the life of this server instance, and advertising a retry
    /// invites the model to spend its turns re-running a call that cannot start.
    fn retryable(&self) -> bool {
        use crate::tools::exec::ProcessError;
        match self {
            ProcessError::Unavailable(_) => false,
            other => matches!(
                other.error_class(),
                "io" | "resource_timeout" | "resource_busy"
            ),
        }
    }

    fn details(&self) -> Option<Value> {
        use crate::tools::exec::ProcessError;
        match self {
            ProcessError::Timeout { details, .. } => serde_json::to_value(details).ok(),
            ProcessError::TimeoutBeforeSpawn { timeout_ms } => Some(json!({
                "timeout_ms": timeout_ms,
                "termination_outcome": "not_started",
                "containment_scope": crate::tools::exec::containment_scope()
            })),
            ProcessError::OutcomeUncertain => Some(json!({
                "termination_outcome": "uncertain",
                "containment_scope": crate::tools::exec::containment_scope()
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

fn diagnostic_tool_error<E: DiagnosticError + ?Sized>(error: &E) -> CallToolResponse {
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

fn blocking_response<E: DiagnosticError>(
    tool: &str,
    run_ms: u64,
    result: Result<Result<crate::tools::ToolOutput, E>, tokio::task::JoinError>,
) -> CallToolResponse {
    match result {
        Ok(Ok(output)) => {
            let outcome = if output.child_nonzero {
                "child_nonzero"
            } else {
                "success"
            };
            if outcome == "child_nonzero" {
                tracing::warn!(target: "codexshim", event = "tool_complete", phase = "response", outcome, error_class = "child_nonzero", run_ms);
            } else {
                tracing::info!(target: "codexshim", event = "tool_complete", phase = "response", outcome, run_ms);
            }
            let mut content = Vec::with_capacity(output.images.len() + 1);
            content.push(ContentBlock::text(output.text));
            content.extend(
                output
                    .images
                    .into_iter()
                    .map(|image| ContentBlock::image(image.data, image.mime_type)),
            );
            CallToolResult::success(content).into()
        }
        Ok(Err(error)) => diagnostic_tool_error(&error),
        Err(error) => {
            classified_tool_error("worker_panic", format!("{tool} worker failed: {error}"))
        }
    }
}

fn queue_timeout(tool: &str, timeout_ms: u64) -> CallToolResponse {
    classified_tool_error(
        "resource_timeout",
        queue_timeout_message(tool, timeout_ms),
    )
}

/// Admission runs before the per-call tracing span exists, so the tool and the admission class
/// are logged explicitly here; without them a saturated server cannot be told apart from a
/// saturated read-only pool in diagnostics.
fn resource_busy(tool: &str, admission: &'static str) -> CallToolResponse {
    resource_busy_with_message(
        tool,
        admission,
        format!("{tool} {admission} capacity is busy; retry the request later"),
    )
}

fn resource_busy_with_message(
    tool: &str,
    admission: &'static str,
    message: impl Into<String>,
) -> CallToolResponse {
    tracing::error!(target: "codexshim", event = "tool_error", phase = "request", outcome = "error", error_class = "resource_busy", tool, admission);
    let retryable = true;
    tool_error("resource_busy", retryable, message, None)
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

fn queue_timeout_message(tool: &str, timeout_ms: u64) -> String {
    format!(
        "{tool} timed out after {timeout_ms} ms while waiting for process capacity; no child was started"
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

fn tool_catalog(read_scope: ReadScope) -> &'static [Tool; 5] {
    static NORMAL_TOOLS: OnceLock<[Tool; 5]> = OnceLock::new();
    static UNRESTRICTED_TOOLS: OnceLock<[Tool; 5]> = OnceLock::new();
    let tools = match read_scope {
        ReadScope::Normal => &NORMAL_TOOLS,
        ReadScope::Unrestricted => &UNRESTRICTED_TOOLS,
    };
    tools.get_or_init(|| {
        [
            read_tool(read_scope),
            grep_tool(read_scope),
            glob_tool(read_scope),
            run_program_tool(),
            bash_tool(),
        ]
    })
}

fn read_tool(read_scope: ReadScope) -> Tool {
    let (description, path_description) = match read_scope {
        ReadScope::Normal => (
            "Read a local repository or Codex extension file. Text files are returned as numbered lines; PDFs default to page-oriented Markdown and can be rendered as images. Absolute paths may address configured Codex skill and plugin directories.",
            "Platform-native repository path or absolute path under a configured Codex skill or plugin directory.",
        ),
        ReadScope::Unrestricted => (
            "Read a local filesystem file. Text files are returned as numbered lines; PDFs default to page-oriented Markdown and can be rendered as images. Relative paths use the repository root; absolute paths may address supported locations outside it.",
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
                    "description": "Optional WHATWG encoding label. A BOM takes precedence; when omitted, UTF-8, Big5, and GBK/GB18030 are detected conservatively."
                },
                "line_count": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 2000,
                    "description": "Maximum number of lines to return."
                },
                "pages": {
                    "type": "string",
                    "pattern": "^[1-9][0-9]*(-[1-9][0-9]*)?$",
                    "description": "PDF only: one-based page or continuous page range, such as \"3\" or \"1-5\"."
                },
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": path_description
                },
                "pdf_mode": {
                    "type": "string",
                    "enum": ["text", "image"],
                    "default": "text",
                    "description": "PDF only: return page Markdown or PNG image content blocks."
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

fn run_program_tool() -> Tool {
    Tool::new(
        "run_program",
        "Run one permitted local program directly with literal arguments and no shell. There is \
         no shell, so pipes, redirection, globbing, and variable expansion do not happen. \
         Arguments are passed literally — do not add quoting. Prefer this when arguments contain \
         characters a shell would mangle, such as Windows paths, regexes, and JSON. If the \
         program is not permitted, this returns a not_permitted error — use bash instead; do not \
         work around it. Cleanup owns a Windows Job Object or the Unix process group it created; \
         a Unix program that starts a new session can escape that group. This is not a sandbox. \
         This is an open-world, destructive operation and may require approval.",
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
                    "maximum": 600_000,
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

fn bash_tool() -> Tool {
    Tool::new(
        "bash",
        "Run a POSIX bash command line and return merged stdout and stderr with the exit code. \
         Write POSIX bash, never PowerShell, on every platform. The command runs \
         non-interactively with no TTY and stdin closed, so pass flags such as -y or --no-edit \
         instead of expecting a prompt. A non-zero exit code is a normal result, not a tool \
         error. Both output streams are merged into one pipe, so lines appear in pipe-write \
         order and cannot be attributed to a stream; a program that buffers stdout but not \
         stderr can still interleave them differently from what a terminal would show. Output \
         above the byte budget is truncated in the middle: redirect to a file and page it with \
         read when you need all of it. The default timeout is 120000 ms and the maximum is \
         600000 ms; for work that needs longer, set detach with a log_path and read that file \
         instead of waiting. Do not issue state-changing commands against the same working \
         tree in parallel calls. Cleanup owns a Windows Job Object or the Unix process group it \
         created; a Unix program that starts a new session can escape that group. This is not a \
         sandbox. This is an open-world, destructive operation and may require approval.",
        schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {
                    "type": "string",
                    "minLength": 1,
                    "description": "POSIX bash command line, run with --noprofile --norc -c."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional platform-native absolute or repository-root-relative working directory; defaults to the repository root."
                },
                "detach": {
                    "type": "boolean",
                    "default": false,
                    "description": "Run the command past the end of this call under server-owned lifecycle tracking. Windows owns a Job Object; Unix owns the created process group, which a program can escape by starting a new session. Requires log_path and forbids timeout_ms; returns the pid and log path instead of output."
                },
                "log_path": {
                    "type": "string",
                    "description": "Repository-relative or root-absolute file for the detached command's merged output, truncated at start. Its parent directory must already exist. Required when detach is true and rejected otherwise."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 600_000,
                    "default": 120_000
                }
            },
            "required": ["command"]
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
