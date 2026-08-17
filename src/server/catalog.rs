use std::sync::{Arc, OnceLock};

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};

use crate::path::ReadScope;
use crate::server::service::{default_timeout_ms, max_timeout_ms};

pub(super) fn tool_catalog(read_scope: ReadScope) -> &'static [Tool; 6] {
    static NORMAL_TOOLS: OnceLock<[Tool; 6]> = OnceLock::new();
    static UNRESTRICTED_TOOLS: OnceLock<[Tool; 6]> = OnceLock::new();
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
            bash_status_tool(),
        ]
    })
}

fn read_tool(read_scope: ReadScope) -> Tool {
    let (description, path_description) = match read_scope {
        ReadScope::Normal => (
            "Read one file as numbered lines. Relative paths resolve against the repository root. If output is truncated, a trailing Partial: next_start_line=N indicates the start line for the next call. For PDFs, returns page Markdown or rendered images based on pdf_mode.",
            "Platform-native path to the file. Relative paths resolve against the repository root.",
        ),
        ReadScope::Unrestricted => (
            "Read one file as numbered lines. Relative paths resolve against the repository root; absolute paths may reach supported locations outside it. If output is truncated, a trailing Partial: next_start_line=N indicates the start line for the next call. For PDFs, returns page Markdown or rendered images based on pdf_mode.",
            "Platform-native path to the file. Relative paths resolve against the repository root; absolute paths may reach supported local filesystems.",
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
                    "description": "Optional WHATWG encoding label (e.g. 'utf-8', 'gbk', 'big5'). Detected automatically if omitted; specify if the returned text is garbled."
                },
                "line_count": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 2000,
                    "description": "Maximum number of lines to return starting from start_line. Omit to read as many lines as the output limit allows."
                },
                "pages": {
                    "type": "string",
                    "pattern": "^[1-9][0-9]*(-[1-9][0-9]*)?$",
                    "description": "PDF only: 1-based page number or continuous page range, such as \"3\" or \"1-5\"."
                },
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": path_description
                },
                "pdf_mode": {
                    "type": "string",
                    "enum": ["auto", "text", "image"],
                    "default": "auto",
                    "description": "PDF only: rendering mode (\"auto\", \"text\", or \"image\"). \"auto\" and \"text\" return page Markdown; \"image\" renders PNG blocks."
                },
                "pdf_cursor": {
                    "type": "string",
                    "minLength": 1,
                    "description": "PDF only: opaque continuation token returned from a previous truncated read."
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 1,
                    "description": "1-based first line to return. Pass next_start_line from a truncated response to continue reading."
                }
            },
            "required": ["path"]
        })),
    )
    .with_title("Read")
    .with_annotations(read_only_annotations())
}

fn grep_tool(read_scope: ReadScope) -> Tool {
    let (description, path_description, glob_description) = match read_scope {
        ReadScope::Normal => (
            "Search file contents under the repository root using Rust regex or fixed strings. If output is truncated, a trailing Partial: next_offset=N indicates the offset for the next call.",
            "Optional platform-native file or directory to search. Relative paths resolve against the repository root.",
            "Optional case-sensitive glob over repository-root-relative paths.",
        ),
        ReadScope::Unrestricted => (
            "Search file contents using Rust regex or fixed strings. Relative paths resolve against the repository root; absolute paths may reach supported locations outside it. If output is truncated, a trailing Partial: next_offset=N indicates the offset for the next call.",
            "Optional platform-native file or directory to search. Relative paths resolve against the repository root; absolute paths may reach supported local filesystems.",
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
                    "default": "smart",
                    "description": "Case sensitivity: 'smart' (case-sensitive if uppercase characters are present), 'sensitive', or 'insensitive'."
                },
                "context_lines": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 20,
                    "default": 0,
                    "description": "Number of context lines before and after each match."
                },
                "encoding": {
                    "type": "string",
                    "description": "Single-file search only: WHATWG encoding label (e.g. 'big5', 'gbk') for decoding the target file."
                },
                "fallback_encoding": {
                    "type": "string",
                    "description": "Directory search only: fallback WHATWG encoding for files whose encoding cannot be determined automatically."
                },
                "fixed_strings": {
                    "type": "boolean",
                    "default": false,
                    "description": "Treat pattern as a literal string instead of a regex."
                },
                "glob": {
                    "type": "string",
                    "description": glob_description
                },
                "include_ignored": {
                    "type": "boolean",
                    "description": "Set true to include files ignored by .gitignore (system directories like .git remain excluded)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "default": 200,
                    "description": "Maximum number of matching entries to return."
                },
                "mode": {
                    "type": "string",
                    "enum": ["content", "files", "count"],
                    "default": "content",
                    "description": "Result projection: 'content' (matching lines), 'files' (matching paths only), or 'count' (path:count summaries)."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Number of matching entries to skip before returning results. Pass next_offset from a truncated response to continue."
                },
                "path": {
                    "type": "string",
                    "description": path_description
                },
                "pattern": {
                    "type": "string",
                    "description": "Search pattern (Rust regex by default, or literal string when fixed_strings is true)."
                }
            },
            "required": ["pattern"]
        })),
    )
    .with_title("Grep")
    .with_annotations(read_only_annotations())
}

fn glob_tool(read_scope: ReadScope) -> Tool {
    let (description, path_description, pattern_description) = match read_scope {
        ReadScope::Normal => (
            "Find paths under the repository root using a glob pattern. Returns files by default; use type to find directories or any entry. If output is truncated, a trailing Partial: next_offset=N indicates the offset for the next call.",
            "Platform-native directory to traverse. Relative paths resolve against the repository root.",
            "Case-sensitive glob over repository-root-relative paths.",
        ),
        ReadScope::Unrestricted => (
            "Find local filesystem paths using a glob pattern. Returns files by default; use type to find directories or any entry. Relative paths resolve against the repository root; absolute paths may reach supported locations outside it. If output is truncated, a trailing Partial: next_offset=N indicates the offset for the next call.",
            "Platform-native directory to traverse. Relative paths resolve against the repository root; absolute paths may reach supported local filesystems.",
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
                    "description": "Set true to include paths ignored by .gitignore (system directories like .git remain excluded)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "default": 200,
                    "description": "Maximum number of paths to return."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Number of matching paths to skip before returning results. Pass next_offset from a truncated response to continue."
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
                },
                "type": {
                    "type": "string",
                    "enum": ["file", "directory", "any"],
                    "default": "file",
                    "description": "Filesystem entry kind to return: 'file', 'directory', or 'any'."
                }
            },
            "required": ["pattern"]
        })),
    )
    .with_title("Glob")
    .with_annotations(read_only_annotations())
}

fn run_program_tool() -> Tool {
    let max = max_timeout_ms();
    let default = default_timeout_ms();
    Tool::new(
        "run_program",
        "Run a local executable directly with literal arguments without a shell. Arguments are passed literally without shell expansion or quoting. Use bash instead if pipelines, redirection, or shell composition are required.",
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
                    "description": "Optional working directory (relative paths resolve against repository root)."
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
                    "description": "Executable name or path."
                },
                "stdin": {
                    "type": ["string", "null"],
                    "maxLength": 1_048_576,
                    "description": "Optional UTF-8 standard input string."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": max,
                    "default": default,
                    "description": "Execution timeout in milliseconds."
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
    .with_title("Run Program")
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            .idempotent(false)
            .open_world(true),
    )
}

fn bash_tool() -> Tool {
    let max = max_timeout_ms();
    let default = default_timeout_ms();
    Tool::new(
        "bash",
        "Run a POSIX bash command line non-interactively and return merged stdout/stderr with the exit code. Write POSIX bash (never PowerShell) on all platforms. For long-running commands, set detach=true with a log_path to run in the background and monitor via bash_status.",
        schema(json!({
            "type": "object",
            "oneOf": [
            {
              "type": "object",
              "additionalProperties": false,
              "properties": {
                "command": {
                    "type": "string",
                    "minLength": 1,
                    "description": "POSIX bash command line to execute."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory (relative paths resolve against repository root)."
                },
                "detach": {
                    "type": "boolean",
                    "default": false,
                    "description": "Run command in background. Requires log_path and returns a job_id for use with bash_status."
                },
                "log_path": {
                    "type": "string",
                    "description": "Output log file path for detached execution. Required when detach is true."
                },
                "msys_argument_conversion": {
                    "type": "string",
                    "enum": ["default", "disabled"],
                    "default": "default",
                    "description": "Windows only: set 'disabled' to prevent MSYS/Git Bash from rewriting POSIX-like switches (e.g. /E) into Windows paths."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": max,
                    "default": default,
                    "description": "Execution timeout in milliseconds. For longer tasks, use detach=true instead."
                }
              },
              "required": ["command"]
            },
            {
              "type": "object",
              "additionalProperties": false,
              "properties": {
                "action": {
                  "type": "string",
                  "const": "terminate",
                  "description": "Action to perform (e.g. 'terminate' to kill a detached background job)."
                },
                "job_id": {
                  "type": "string",
                  "pattern": "^bash-[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-4[0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$",
                  "description": "Background job ID returned by bash(detach=true)."
                }
              },
              "required": ["action", "job_id"]
            }
            ]
        })),
    )
    .with_title("Bash")
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            .idempotent(false)
            .open_world(true),
    )
}

fn bash_status_tool() -> Tool {
    Tool::new(
        "bash_status",
        "Get the status and latest log output of a detached background bash job.",
        schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "job_id": {
                    "type": "string",
                    "pattern": "^bash-[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-4[0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$",
                    "description": "Background job ID returned by bash(detach=true)."
                },
                "tail_bytes": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 16384,
                    "default": 8192,
                    "description": "Maximum number of trailing log bytes to return (set 0 for status only)."
                }
            },
            "required": ["job_id"]
        })),
    )
    .with_title("Bash Status")
    .with_annotations(read_only_annotations())
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
