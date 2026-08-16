use std::sync::{Arc, OnceLock};

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};

use crate::path::ReadScope;
use crate::server::service::{default_timeout_ms, max_timeout_ms};

pub(super) fn tool_catalog(
    read_scope: ReadScope,
    _client_profile: crate::ClientProfile,
) -> &'static [Tool; 6] {
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
            "Read one file as numbered lines. Use this by default whenever you need the contents of a known file. Relative paths resolve against the repository root; an absolute path outside it succeeds only where this server was configured to allow it, and otherwise fails immediately with a message naming the limit, so an attempt is cheap. Omit line_count to fill one response. A truncated response ends with a line starting Partial: that names the argument for the next call, such as `Partial: next_start_line=801.` — send that value back rather than re-reading from line 1. PDFs return per-page Markdown or images instead; the pdf_* arguments control that.",
            "Platform-native path to one file. Relative paths resolve against the repository root; an absolute path works only where this server was configured to allow it.",
        ),
        ReadScope::Unrestricted => (
            "Read one file as numbered lines. Use this by default whenever you need the contents of a known file. Relative paths resolve against the repository root; absolute paths may reach supported locations outside it. Omit line_count to fill one response. A truncated response ends with a line starting Partial: that names the argument for the next call, such as `Partial: next_start_line=801.` — send that value back rather than re-reading from line 1. PDFs return per-page Markdown or images instead; the pdf_* arguments control that.",
            "Platform-native path to one file. Relative paths resolve against the repository root; absolute paths may reach supported local filesystems.",
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
                    "description": "Optional WHATWG encoding label. Leave it unset first: a BOM wins, and UTF-8, Big5, and GBK/GB18030 are detected conservatively. Pass a label when the returned text comes back garbled, or when the response reports an Encoding: line you know is wrong for this file."
                },
                "line_count": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 2000,
                    "description": "Maximum lines to return starting at start_line. Omit to fill one response. A large value does not defeat the response size cap — an over-long result still comes back truncated with a Partial: line."
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
                    "description": "PDF only: \"auto\" and \"text\" return page Markdown, \"image\" renders PNG content blocks. Without pages, text modes deliver the first 10 pages and image renders page 1; when more remain, the response ends with a Partial: line."
                },
                "pdf_cursor": {
                    "type": "string",
                    "minLength": 1,
                    "description": "PDF only: copy the pdf_cursor value printed verbatim in the previous response's Partial: or Retry: line, together with the pages it printed. It carries both the source version and any resume point inside a page, so a stale value is rejected rather than silently returning pages from a changed file."
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 1,
                    "description": "One-based first line to return, and the argument to continue with: a truncated response names the next value as next_start_line."
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
            "Search file contents under the repository root using Rust regex or fixed strings. Use this by default whenever you need to search file contents. Files that could not be searched are listed with a path and reason. Omit limit to fill one response. A truncated response ends with `Partial: next_offset=N.` — pass that N back as offset instead of restarting.",
            "Optional platform-native file or directory to search. Relative paths resolve against the repository root; an absolute path works only where this server was configured to allow it.",
            "Optional case-sensitive glob over repository-root-relative paths.",
        ),
        ReadScope::Unrestricted => (
            "Search file contents using Rust regex or fixed strings. Use this by default whenever you need to search file contents. Relative paths resolve against the repository root; absolute paths may reach supported locations outside it. Files that could not be searched are listed with a path and reason. Omit limit to fill one response. A truncated response ends with `Partial: next_offset=N.` — pass that N back as offset instead of restarting.",
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
                    "description": "Case sensitivity. smart is case-sensitive when the pattern contains an uppercase letter, otherwise case-insensitive."
                },
                "context_lines": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 20,
                    "default": 0,
                    "description": "Lines of context to show before and after each match."
                },
                "encoding": {
                    "type": "string",
                    "description": "Single-file path only, and rejected with a directory. WHATWG label such as \"big5\" or \"gbk\" naming how to decode that one file before searching it. Use this after a single-file search failed as undecodable."
                },
                "fallback_encoding": {
                    "type": "string",
                    "description": "Directory path only, and rejected with a single file. WHATWG label applied only to files whose encoding cannot be determined on its own; it never displaces a BOM, valid UTF-8, or a detected encoding, so files that already read correctly are unaffected. Use this when the skip list reports files as undecodable and you know what they are."
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
                "include_ignored": {
                    "type": "boolean",
                    "description": "Set true and retry when a file you had good reason to expect is missing from the results — the server's configured default may be filtering gitignored paths. Leave it unset otherwise. .git internals and common heavy build directories are excluded either way and this flag does not reach them."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "default": 200,
                    "description": "Maximum matching entries to return. Omit to fill one response. A large value does not defeat the response size cap — an over-long result still comes back truncated with a Partial: line."
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
                    "description": "Skip this many matching entries before returning results. To continue a truncated response, pass the N from its `Partial: next_offset=N.` line."
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
    .with_title("Grep")
    .with_annotations(read_only_annotations())
}

fn glob_tool(read_scope: ReadScope) -> Tool {
    let (description, path_description, pattern_description) = match read_scope {
        ReadScope::Normal => (
            "Find paths under the repository root using a glob pattern. Use this by default whenever you need to find files or discover paths. Returns files by default; use type to find directories or any entry. Results are native absolute paths. Entries that could not be traversed are listed with a path and reason. Omit limit to fill one response. A truncated response ends with `Partial: next_offset=N.` — pass that N back as offset instead of restarting.",
            "Platform-native directory to traverse. Relative paths resolve against the repository root; an absolute path works only where this server was configured to allow it.",
            "Case-sensitive glob over repository-root-relative paths.",
        ),
        ReadScope::Unrestricted => (
            "Find local filesystem paths using a glob pattern. Use this by default whenever you need to find files or discover paths. Returns files by default; use type to find directories or any entry. Relative paths resolve against the repository root; absolute paths may reach supported locations outside it. Results are native absolute paths. Entries that could not be traversed are listed with a path and reason. Omit limit to fill one response. A truncated response ends with `Partial: next_offset=N.` — pass that N back as offset instead of restarting.",
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
                    "description": "Set true and retry when a path you had good reason to expect is missing from the results — the server's configured default may be filtering gitignored paths. Leave it unset otherwise. .git internals and common heavy build directories are excluded either way and this flag does not reach them."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "default": 200,
                    "description": "Maximum paths to return. Omit to fill one response. A large value does not defeat the response size cap — an over-long result still comes back truncated with a Partial: line."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Skip this many matching paths before returning results. To continue a truncated response, pass the N from its `Partial: next_offset=N.` line."
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
                    "description": "Filesystem entry kind to return. file includes regular files and symbolic-link entries; directory returns real directories; any returns both."
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
        "Run one local program directly with literal arguments and no shell. Use this by default \
         whenever one executable is enough, including an interpreter whose script or command text \
         is passed as one literal argument. Pipes, redirection, globbing, and variable expansion do \
         not happen. Arguments are passed literally — do not add quoting. Prefer this when \
         arguments contain characters a shell would mangle, such as Windows paths, regexes, and \
         JSON. Use bash only when shell composition is required. Oversized output keeps head and \
         tail with total/shown/omitted and has no continuation cursor; redirect to a file and page \
         it with read. Do not issue state-changing commands against the same working tree in \
         parallel calls. A program that daemonizes itself may keep running after this call \
         returns. This is not a sandbox, and the call may require user approval.",
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
                    "default": default,
                    "description": "Execution timeout in milliseconds. On timeout the program is terminated and a Timeout error is returned."
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
    let description = format!(
        "Run a POSIX bash command line and return merged stdout and stderr with the exit code. \
         Use this by default whenever shell composition is required: pipelines, redirection, \
         globbing, variable expansion, or several steps in one call. \
         Write POSIX bash, never PowerShell, on every platform. The command runs \
         non-interactively with no TTY and stdin closed, so pass flags such as -y or --no-edit \
         instead of expecting a prompt. A non-zero exit code is a normal result, not a tool \
         error. stdout and stderr come back merged, with no way to tell which line came from \
         which, and their relative order is not reliable. Output too large for one response is \
         truncated in the middle and cannot be continued: redirect it to a file and page that \
         with read when you need all of it. The default timeout is {default} ms and \
         the maximum is {max} ms; for \
         work that needs longer, set detach with a log_path and observe its returned instance-bound \
         job_id with bash_status. A detached tree belongs to this server instance and runs until \
         it exits, is terminated with action=terminate and its job_id, or the instance stops. \
         There is no reconnect or list API. On Windows, \
         prefer run_program for one native program with literal arguments. \
         Do not issue state-changing commands against the same working tree in parallel calls. A \
         program that daemonizes itself may keep running after this call returns. This is not a \
         sandbox, and the call may require user approval."
    );
    Tool::new(
        "bash",
        description,
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
                    "description": "POSIX bash command line, run with --noprofile --norc -c."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional platform-native absolute or repository-root-relative working directory; defaults to the repository root."
                },
                "detach": {
                    "type": "boolean",
                    "default": false,
                    "description": "Run the command past the end of this call. Requires log_path and forbids timeout_ms; returns an instance-bound opaque job_id plus the diagnostic pid and log path. Use bash_status for immediate lifecycle snapshots and bounded log tails."
                },
                "log_path": {
                    "type": "string",
                    "description": "Repository-relative or root-absolute file for the detached command's merged output, truncated at start. Its parent directory must already exist, and it must not be in use by another active or reserved detached call in this instance; a duplicate is rejected before the log is truncated. Required when detach is true and rejected otherwise."
                },
                "msys_argument_conversion": {
                    "type": "string",
                    "enum": ["default", "disabled"],
                    "default": "default",
                    "description": "Windows Git Bash only, and ignored elsewhere. Git Bash rewrites arguments that look like POSIX paths before launching a native Windows program, which corrupts slash-style switches such as /E. Set disabled to retry with that rewriting turned off; it applies to the whole command, not just the subcommand that failed."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": max,
                    "default": default,
                    "description": "Execution timeout in milliseconds, capped by deployment configuration below the shown maximum. The client's own request timeout still bounds how long a call can wait, so work expected to run for tens of seconds or longer should use detach with a log_path instead. On timeout the command is terminated and a Timeout error is returned. Forbidden when detach is true."
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
                  "description": "Terminate the complete server-owned process tree for job_id."
                },
                "job_id": {
                  "type": "string",
                  "pattern": "^bash-[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-4[0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$",
                  "description": "Opaque instance-bound identifier returned by bash(detach=true)."
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
        "Return an immediate lifecycle snapshot for one instance-bound detached Bash job, with the primary exit status and a bounded tail from the original log file identity. This does not wait for new output and provides no list, reconnect, or cursor API. Unknown or expired IDs are validation errors.",
        schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "job_id": {
                    "type": "string",
                    "pattern": "^bash-[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-4[0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$",
                    "description": "Opaque instance-bound identifier returned by bash(detach=true)."
                },
                "tail_bytes": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 16384,
                    "default": 8192,
                    "description": "Candidate maximum raw log bytes to show. Set 0 for lifecycle metadata only; output budgets may reduce a non-zero tail."
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
