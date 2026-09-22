use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};

use crate::path::ReadScope;
pub(super) fn tool_catalog(
    read_scope: ReadScope,
    max_timeout_ms: u64,
    default_timeout_ms: u64,
    background_timeout_max_ms: u64,
) -> [Tool; 6] {
    [
        read_tool(read_scope),
        grep_tool(read_scope),
        glob_tool(read_scope),
        run_program_tool(max_timeout_ms, default_timeout_ms),
        bash_tool(
            max_timeout_ms,
            default_timeout_ms,
            background_timeout_max_ms,
        ),
        bash_status_tool(),
    ]
}

fn read_tool(read_scope: ReadScope) -> Tool {
    let (description, path_description) = match read_scope {
        ReadScope::Normal => (
            "Read one file as numbered lines. The required path is resolved from the repository root or an approved extension root in normal scope; paths outside configured roots are rejected. For PDFs, use pages and pdf_mode; for Office files, follow office_cursor. If output is truncated, pass the trailing Partial: next_start_line=N as start_line.",
            "Path to the file. Relative paths resolve against the repository root; supported absolute paths must be inside a configured root.",
        ),
        ReadScope::Unrestricted => (
            "Read one file as numbered lines. The required path is resolved from the repository root; absolute paths are supported in unrestricted scope. For PDFs, use pages and pdf_mode; for Office files, follow office_cursor. If output is truncated, pass the trailing Partial: next_start_line=N as start_line.",
            "Path to the file. Relative paths resolve against the repository root; absolute paths are supported.",
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
                    "description": "PDF only: 1-based page number or inclusive range, such as \"3\" or \"1-5\". Use this instead of line arguments."
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
                    "description": "PDF only: \"auto\" or \"text\" returns page Markdown; \"image\" returns PNG blocks. Omit for auto."
                },
                "pdf_cursor": {
                    "type": "string",
                    "minLength": 1,
                    "description": "PDF only: opaque cursor returned by a previous PDF read; pass it unchanged to continue."
                },
                "office_cursor": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Office only: opaque cursor returned by a previous Office read; pass it unchanged to continue."
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
            "Search files under the configured normal read roots with Rust regex; set fixed_strings=true for literal text. The required pattern is bounded to 8,192 Unicode characters. Use glob/type/mode to narrow results, then pass Partial: next_offset=N as a best-effort offset. Results are not sorted.",
            "Optional file or directory to search; omit to search the repository root. Relative paths resolve against the repository root; supported absolute paths must be inside a configured root.",
            "Optional case-sensitive glob filter or array (up to 32 patterns, each up to 1,024 Unicode characters); prefix a pattern with ! to exclude it. Patterns without '/' match basenames recursively.",
        ),
        ReadScope::Unrestricted => (
            "Search files with Rust regex; set fixed_strings=true for literal text. The required pattern is bounded to 8,192 Unicode characters. Relative paths resolve from the repository root and absolute paths are supported in unrestricted scope. Use glob/type/mode to narrow results, then pass Partial: next_offset=N as a best-effort offset. Results are not sorted.",
            "Optional file or directory to search; omit to search the repository root. Relative paths resolve against the repository root; absolute paths are supported.",
            "Optional case-sensitive glob filter or array (up to 32 patterns, each up to 1,024 Unicode characters); prefix a pattern with ! to exclude it. Patterns without '/' match basenames recursively.",
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
                    "description": "Single-file search only: WHATWG encoding label (e.g. 'big5', 'gbk') for decoding the target file; mutually exclusive with fallback_encoding."
                },
                "fallback_encoding": {
                    "type": "string",
                    "description": "Directory search only: fallback WHATWG encoding for files whose encoding cannot be determined automatically; mutually exclusive with encoding."
                },
                "fixed_strings": {
                    "type": "boolean",
                    "default": false,
                    "description": "Treat pattern as a literal string instead of a regex."
                },
                "glob": glob_patterns_schema(glob_description),
                "include_ignored": {
                    "type": "boolean",
                    "description": "Set true to include ignored files; omit to use the server ignore policy. Hard exclusions such as .git remain excluded."
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
                    "description": "Best-effort skip. Results are not sorted; for precise pagination, narrow your pattern or glob."
                },
                "path": {
                    "type": "string",
                    "description": path_description
                },
                "pattern": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 8192,
                    "description": "Search pattern (Rust regex syntax without lookaround/backreferences by default, or literal string when fixed_strings is true)."
                },
                "type": {
                    "type": "string",
                    "description": "Optional file type filter (e.g. 'rust', 'python', 'js', 'ts', 'go', 'java', 'markdown')."
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
            "Find paths under the configured normal read roots with a case-sensitive glob. The required pattern accepts a string or up to 32 patterns (each up to 1,024 Unicode characters); prefix a pattern with ! to exclude it. Files are returned by default. Pass Partial: next_offset=N as a best-effort offset to continue; results are not sorted.",
            "Directory path to traverse. Relative paths resolve against the repository root; supported absolute paths must be inside a configured root (defaults to '.').",
            "Glob pattern or array of patterns (supports '!' negation) relative to the search path. Patterns without '/' match file basenames recursively.",
        ),
        ReadScope::Unrestricted => (
            "Find filesystem paths with a case-sensitive glob. The required pattern accepts a string or up to 32 patterns (each up to 1,024 Unicode characters); prefix a pattern with ! to exclude it. Relative paths resolve from the repository root and absolute paths are supported in unrestricted scope. Pass Partial: next_offset=N as a best-effort offset to continue; results are not sorted.",
            "Directory path to traverse. Relative paths resolve against the repository root; absolute paths are supported (defaults to '.').",
            "Glob pattern or array of patterns (supports '!' negation) relative to the search path. Patterns without '/' match file basenames recursively.",
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
                    "description": "Set true to include ignored paths; omit to use the server ignore policy. Hard exclusions such as .git remain excluded."
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
                    "description": "Best-effort skip. Results are not sorted; for precise pagination, narrow your pattern or path."
                },
                "path": {
                    "type": "string",
                    "default": ".",
                    "description": path_description
                },
                "pattern": glob_patterns_schema(pattern_description),
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

fn run_program_tool(max: u64, default: u64) -> Tool {
    Tool::new(
        "run_program",
        "Run one local executable with literal argv and no shell. The required program and optional args run from the repository root by default; cwd changes the working directory. Environment overrides and stdin are explicit. A nonzero exit is returned with its exit status and output. MCP does not sandbox spawned processes. Use bash for pipelines, redirection, or shell composition.",
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
                    "default": ".",
                    "description": "Working directory; relative paths resolve from the repository root. Omit to use the repository root."
                },
                "env": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "default": {},
                    "description": "String-valued environment overrides; omitted variables are inherited."
                },
                "program": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Executable name or path."
                },
                "stdin": {
                    "type": ["string", "null"],
                    "maxLength": 1_048_576,
                    "description": "Optional UTF-8 stdin, up to 1 MiB; null or omission closes stdin."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": max,
                    "default": default,
                    "description": "Positive execution timeout in milliseconds; omission uses the server default."
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

#[allow(
    clippy::too_many_lines,
    reason = "the three mutually exclusive public Bash variants stay adjacent in one schema"
)]
fn bash_tool(max: u64, default: u64, background_max: u64) -> Tool {
    Tool::new(
        "bash",
        "Run a non-interactive POSIX Bash command and return merged stdout/stderr plus its exit status. Use portable POSIX sh syntax, not PowerShell. Run in the repository root by default; use detach=true with log_path for long work, then poll bash_status. Foreground nonzero exit is a completed command result, not a tool-call error; use action=terminate with a job_id to stop a detached tree. MCP does not sandbox spawned processes.",
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
                    "default": ".",
                    "description": "Working directory; relative paths resolve from the repository root. Omit to use the repository root."
                },
                "detach": {
                    "type": "boolean",
                    "const": false,
                    "default": false,
                    "description": "Run this command in the foreground."
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
                    "description": "Positive foreground timeout in milliseconds; omission uses the server default."
                }
              },
              "required": ["command"]
            },
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
                  "default": ".",
                  "description": "Working directory; relative paths resolve from the repository root. Omit to use the repository root."
                },
                "detach": {
                  "type": "boolean",
                  "const": true,
                  "description": "Run command as an instance-bound managed background job."
                },
                "log_path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Required log path for detached output; it must be a repository-local path. Read it with read after completion."
                },
                "msys_argument_conversion": {
                  "type": "string",
                  "enum": ["default", "disabled"],
                  "default": "default",
                  "description": "Windows only: set 'disabled' to prevent MSYS/Git Bash argument conversion."
                },
                "timeout_ms": {
                  "type": "integer",
                  "minimum": 1,
                  "maximum": background_max,
                  "default": background_max,
                  "description": "Maximum background runtime after spawn; omission uses the configured maximum and an explicit value may only shorten it."
                }
              },
              "required": ["command", "detach", "log_path"]
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
        "Get lifecycle status and bounded log output for the job_id returned by bash(detach=true). Repeat while running, status_unknown, finalizing, or terminating until completed, terminated, timed_out, log_quota_exceeded, or outcome_uncertain; use cursor for incremental output and read(log_path) for the full repository log.",
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
                },
                "cursor": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional byte cursor for incremental log reads."
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 16384,
                    "description": "Maximum bytes returned from cursor; defaults to tail_bytes."
                },
                "wait_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 1000,
                    "default": 0,
                    "description": "Optional wait duration in milliseconds for new output or status change before returning (long-polling, 0-1000)."
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

fn glob_patterns_schema(description: &'static str) -> Value {
    json!({
        "anyOf": [
            {
                "type": "string",
                "minLength": 1,
                "maxLength": 1024,
                "not": { "const": "!" },
                "description": "Single glob pattern relative to the search path. Patterns without '/' match file basenames recursively."
            },
            {
                "type": "array",
                "items": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 1024,
                    "not": { "const": "!" }
                },
                "minItems": 1,
                "maxItems": 32,
                "description": "Array of glob patterns (supports '!' negation) relative to the search path."
            }
        ],
        "description": description
    })
}
