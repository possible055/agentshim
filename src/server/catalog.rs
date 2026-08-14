use std::sync::{Arc, OnceLock};

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};

use crate::{
    path::ReadScope,
    tools::exec::spawn::{DEFAULT_TIMEOUT_MS, max_timeout_ms},
};

pub(super) fn tool_catalog(read_scope: ReadScope) -> &'static [Tool; 5] {
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
                    "description": "PDF only: one-based page or continuous page range, such as \"3\" or \"1-5\". A range extending past the last page is clamped to it; a range starting past it is an error."
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
                    "description": "PDF only: \"auto\" and \"text\" return page Markdown, \"image\" renders PNG content blocks. Without pages, text modes deliver the first 10 pages and image renders page 1, each with a continuation."
                },
                "pdf_source_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "PDF only: opaque token from a previous response, replayed to prove the continuation targets the same source version."
                },
                "pdf_text_offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "PDF only: resume a single page's Markdown at this UTF-8 byte offset. Requires pages to name exactly one page, and a non-zero value requires pdf_source_id."
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
                    "default": "smart",
                    "description": "Case sensitivity. smart is case-sensitive when the pattern contains an uppercase letter, otherwise case-insensitive."
                },
                "context_lines": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 20,
                    "default": 0,
                    "description": "Lines of context to show before and after each match."
                },
                "fixed_strings": {
                    "type": "boolean",
                    "default": false,
                    "description": "Treat pattern as a literal string instead of a Rust regex."
                },
                "glob": {
                    "type": "string",
                    "description": glob_description
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "default": 200,
                    "description": "Maximum matching entries to return."
                },
                "mode": {
                    "type": "string",
                    "enum": ["content", "files", "count"],
                    "default": "content",
                    "description": "content returns matching lines, files returns only paths with matches, count returns path:count summaries."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Skip this many matching entries before returning results, for pagination."
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
            "Find local repository or Codex extension paths using a glob pattern. Returns files by default; use type to find directories or any entry. Results use native absolute paths and expose structured numeric pagination.",
            "Platform-native repository directory or absolute directory under a configured Codex skill or plugin root.",
            "Case-sensitive glob relative to the repository or requested Codex extension directory.",
        ),
        ReadScope::Unrestricted => (
            "Find local filesystem paths using a glob pattern. Returns files by default; use type to find directories or any entry. Relative paths use the repository root; absolute paths may address supported locations outside it.",
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
                    "default": false,
                    "description": "Include entries matched by gitignore; .git internals remain excluded."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "default": 200,
                    "description": "Maximum paths to return."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Skip this many matching paths before returning results, for pagination."
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
                    "description": "Filesystem entry kind to return. file includes regular files and symbolic-link entries; directory returns real directories; any preserves both."
                }
            },
            "required": ["pattern"]
        })),
    )
    .with_annotations(read_only_annotations())
}

fn run_program_tool() -> Tool {
    let max = max_timeout_ms();
    Tool::new(
        "run_program",
        "Run one local program directly with literal arguments and no shell. Use this by default \
         whenever one executable is enough, including an interpreter whose script or command text \
         is passed as one literal argument. Pipes, redirection, globbing, and variable expansion do \
         not happen. Arguments are passed literally — do not add quoting. Prefer this when \
         arguments contain characters a shell would mangle, such as Windows paths, regexes, and \
         JSON. Use bash only when shell composition is required. Do not issue state-changing \
         commands against the same working tree in parallel calls. Cleanup owns a Windows Job \
         Object or the Unix process group it created; a Unix program that starts a new session can \
         escape that group. This is not a sandbox. This is an open-world, destructive operation \
         and may require approval.",
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
                    "maximum": max,
                    "default": DEFAULT_TIMEOUT_MS,
                    "description": "Execution timeout in milliseconds. On timeout the owned process containment is terminated and a Timeout error is returned."
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
    let max = max_timeout_ms();
    let description = format!(
        "Run a POSIX bash command line and return merged stdout and stderr with the exit code. \
         Write POSIX bash, never PowerShell, on every platform. The command runs \
         non-interactively with no TTY and stdin closed, so pass flags such as -y or --no-edit \
         instead of expecting a prompt. A non-zero exit code is a normal result, not a tool \
         error. Both output streams are merged into one pipe, so lines appear in pipe-write \
         order and cannot be attributed to a stream; a program that buffers stdout but not \
         stderr can still interleave them differently from what a terminal would show. Output \
         above the byte budget is truncated in the middle: redirect to a file and page it with \
         read when you need all of it. The default timeout is {DEFAULT_TIMEOUT_MS} ms and the \
         maximum is {max} ms; for work that needs longer, set detach with a log_path \
         and read that file instead of waiting. On Windows, prefer run_program for one native \
         program with literal arguments. When Bash composition must pass slash-style switches \
         such as /E or /C to a native program, set msys_argument_conversion to disabled. This \
         setting applies to the whole Bash command; codexshim does not inspect subcommands or \
         retry failures. Do not issue state-changing commands against the same working tree in \
         parallel calls. Cleanup owns a Windows Job Object or the Unix process group it \
         created; a Unix program that starts a new session can escape that group. This is not \
         a sandbox. This is an open-world, destructive operation and may require approval."
    );
    Tool::new(
        "bash",
        description,
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
                "msys_argument_conversion": {
                    "type": "string",
                    "enum": ["default", "disabled"],
                    "default": "default",
                    "description": "Windows Git Bash only. default leaves inherited MSYS settings and automatic argv path conversion unchanged. disabled sets MSYS2_ARG_CONV_EXCL=* for the whole Bash command so native child processes receive slash-prefixed arguments unchanged. It has no effect on macOS or Linux. Prefer run_program for a single native program."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": max,
                    "default": DEFAULT_TIMEOUT_MS,
                    "description": "Execution timeout in milliseconds. On timeout the owned process group is terminated and a Timeout error is returned. Forbidden when detach is true."
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
