# codexshim

English | [简体中文](README.zh-CN.md)

`codexshim` gives Codex and Cursor a small, focused set of tools for working with source code, with first-class Windows x86-64 support and compatibility releases for Linux and macOS. It runs as a local stdio MCP server and treats the directory you start it in as the repository root.

## Why use it

- **Bounded file access.** `read`, `grep`, and `glob` stay inside your repository by default, with optional access to Codex skill and plugin directories.
- **Two ways to run commands.** `run_program` takes one executable and a literal argument list — no shell ever parses the arguments. `bash` takes a POSIX command line when you need pipelines, redirection, or composition, replacing the unreliable pwsh7 shell that Cursor's built-in Shell uses on Windows.
- **Cross-platform.** Full support for Windows x86-64, with compatibility release assets for Linux x86-64, Linux ARM64, and macOS Apple Silicon.
- **Reads PDFs.** `read` detects PDFs by content, not extension, and returns page text or rendered images with continuation cursors for long documents.

## Tools

| Tool | Description |
| --- | --- |
| `read` | Read source files with line numbers. Supports UTF-8, BOM-detected UTF-16, and WHATWG encoding labels. Also reads PDFs. |
| `grep` | Search file contents with Rust regex or literal strings. |
| `glob` | Find files while respecting repository ignore rules. |
| `run_program` | Run one program with a literal argument list, without a shell. |
| `bash` | Run a POSIX bash command line and return merged stdout and stderr. |

Successful calls return bounded text. Partial `read`, `grep`, and `glob` results include a continuation cursor so you can pick up where you left off. Failures return a stable `{ error: { code, message, retryable, details } }` envelope.

## Install

**Windows (PowerShell):**

```powershell
irm https://github.com/possible055/codexshim/releases/latest/download/install.ps1 | iex
```

**Linux / macOS:**

```sh
curl -fsSL https://github.com/possible055/codexshim/releases/latest/download/install.sh | sh
```

Re-run the same command to update. Install a specific version with `-Version` (PowerShell) or `--version` (sh).

**Build from source** (requires Rust 1.88):

```console
cargo build --release --locked
```

The binary is at `target/release/codexshim` (Linux and macOS) or `target/release/codexshim.exe` (Windows).

## Configure Codex

Copy the matching example into `~/.codex/config.toml` (user-level) or a project's `.codex/config.toml`, then replace `command` with the absolute path to your `codexshim` binary:

- [Windows example](config/codex.windows.toml.example)
- [Linux example](config/codex.linux.toml.example)
- [macOS example](config/codex.macos.toml.example)

```toml
[mcp_servers.codexshim]
command = "/absolute/path/to/codexshim"
args = ["serve", "--client-profile", "codex"]
required = true
supports_parallel_tool_calls = true
startup_timeout_sec = 15
tool_timeout_sec = 600
enabled_tools = ["read", "grep", "glob", "run_program", "bash"]
default_tools_approval_mode = "writes"
env = { CODEX_MCP_PROTOCOL_VERSION = "2026-07-28" }

[mcp_servers.codexshim.tools.run_program]
approval_mode = "on-request"

[mcp_servers.codexshim.tools.bash]
approval_mode = "prompt"

[features]
mcp_2026_07_28 = true
```

`run_program` can be approved on request; `bash` always prompts. Neither tool is a security sandbox. On Windows, use a single-quoted TOML path to avoid escaping backslashes. `tool_timeout_sec` must match `CODEXSHIM_TOOL_TIMEOUT_SHELF` (both default to 600); the server stays 10 seconds below the shelf so its Timeout response reaches the client before the client's own deadline fires.

## Configure Cursor

Copy the [Cursor example](config/cursor.mcp.json.example) to `~/.cursor/mcp.json`, replace `command` with the absolute path to the binary, and restart Cursor:

```json
{
  "mcpServers": {
    "codexshim": {
      "type": "stdio",
      "command": "/absolute/path/to/codexshim",
      "args": ["serve", "--client-profile", "cursor"]
    }
  }
}
```

On Windows, JSON paths must escape each backslash.

## Options

### `--client-profile`

Selects the aggregate output policy. `codex` is the default; `cursor` permits a larger rapid-response aggregate.

| Value | Per-call token ceiling | Default burst tokens |
| --- | ---: | ---: |
| `codex` (default) | 8,192 | 8,192 |
| `cursor` | 8,192 | 32,768 |

### `--read-scope`

Controls which paths `read`, `grep`, and `glob` may access outside the repository:

| Value | Behavior |
| --- | --- |
| `unrestricted` (default) | Any absolute path readable by the server user. |
| `normal` | Repository paths plus Codex skill/plugin directories. Credentials and history under `.codex` stay inaccessible. |

```toml
args = ["serve", "--read-scope", "normal"]
```

`--read-scope` only bounds `read`, `grep`, and `glob`. Programs launched by `run_program` or `bash` inherit the server user's full filesystem access — use an OS sandbox when you need real isolation.

### Long-running work

`bash` accepts `detach` with a `log_path` inside the repository. Output goes to that file and the call returns the pid and log path immediately:

```json
{ "command": "cargo test > /dev/null; echo EXIT=$?", "detach": true, "log_path": "local/test.log" }
```

Poll progress with `read` on `log_path`. Up to 16 detached trees may be live at once.

### Windows Bash argument conversion

Git Bash converts arguments that look like POSIX paths before launching native Windows programs. When slash-style switches such as `robocopy /E` must stay literal, set `msys_argument_conversion` to `disabled`:

```json
{ "command": "robocopy \"$source\" \"$destination\" /E", "msys_argument_conversion": "disabled" }
```

### Reading PDFs

`read` detects a PDF from its `%PDF-` header, regardless of extension. PDF inputs reject `encoding`, `start_line`, and `line_count`.

| Parameter | Values | Meaning |
| --- | --- | --- |
| `pdf_mode` | `auto` (default), `text`, `image` | `auto`/`text` return page Markdown; `image` renders PNG content blocks. |
| `pages` | `"7"` or `"7-12"` | One page or one continuous range. |
| `pdf_text_offset` | integer ≥ 0 | Resume one page's Markdown at a UTF-8 byte offset. |
| `pdf_source_id` | opaque token | Replayed from a previous response to target the same source version. |

Page counts bound the work of one call:

| Mode | Without `pages` | Explicit range cap |
| --- | --- | --- |
| `auto`, `text` | first 10 pages | 20 pages |
| `image` | page 1 | 4 pages |

The response tells you how far it got and how to continue. A document that mixes readable and image-only pages is a success: readable pages come back as Markdown, the rest become placeholders. One instance runs at most one PDF call at a time; a second concurrent call returns a retryable `resource_busy`.

### Environment variables

| Variable | Default | Description |
| --- | --- | --- |
| `CODEX_MCP_PROTOCOL_VERSION` | — | MCP protocol version advertised to Codex. |
| `CODEXSHIM_PROCESS_CALLS` | `16` | Per-instance concurrent process-call limit; 1–32. |
| `CODEXSHIM_DETACHED_CALLS` | `16` | Per-instance live detached `bash` trees; 1–16. |
| `CODEXSHIM_OUTPUT_BYTES` | `32000` | Per-call output ceiling in bytes; 4096–262144. |
| `CODEXSHIM_BURST_TOKENS` | profile default | Shared projected model-token budget; 2048–32768. |
| `CODEXSHIM_TOOL_TIMEOUT_SHELF` | `600` | Shelf the server stays below so the client's `tool_timeout_sec` fires after the server's own Timeout. Effective max execution time is shelf minus 10 seconds; 15–3600. |
| `CODEXSHIM_GREP_MEMORY_BYTES` | `268435456` | Per-call hard limit for retained `grep` candidates. |
| `CODEXSHIM_GLOB_MEMORY_BYTES` | `33554432` | Per-call hard limit for retained `glob` matches. |
| `CODEXSHIM_PDF_TEXT_MEMORY_BYTES` | `67108864` | Per-call memory budget for `auto`/`text` PDF reads. |
| `CODEXSHIM_PDF_IMAGE_MEMORY_BYTES` | `100663296` | Per-call memory budget for `image` PDF reads. |
| `CODEXSHIM_BASH` | probed | Absolute path to a GNU bash. |
| `CODEXSHIM_LOG_MODE` | `errors` | One of `off`, `errors`, `all`. |
| `CODEXSHIM_LOG_DIR` | platform default | Override the log directory with an absolute path. |

## Diagnostics

Logs are UTC-dated JSONL files:

- Windows: `%LOCALAPPDATA%\codexshim\logs`
- Linux: `${XDG_STATE_HOME:-$HOME/.local/state}/codexshim/logs`

Retention: 512 MiB total, 30 days. Inspect or purge:

```console
codexshim logs status
codexshim logs purge
```

Records contain identifiers, phases, outcomes, timings, and error classes — never MCP arguments, grep patterns, process arguments, stdin, file contents, or stdout/stderr. Set `CODEXSHIM_LOG_MODE=all` while reproducing tool-loading failures.

## License

[MIT](LICENSE)
