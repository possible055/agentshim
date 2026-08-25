# AgentShim

English | [简体中文](README.zh-CN.md)

AgentShim gives coding agents a small, focused set of tools for working with source code. Codex and Cursor connect to its local stdio MCP server; DSH uses the native adapter in this repository. The server treats the directory you start it in as the repository root and provides first-class Windows x86-64 support with compatibility releases for Linux and macOS.

## Why use it

- **Bounded file access.** `read`, `grep`, and `glob` stay inside your repository by default, with optional access to Codex skill and plugin directories.
- **Managed long-running Bash.** `run_program` takes one executable and literal arguments. `bash` handles POSIX composition and can detach work under an instance-bound `job_id`; `bash_status` reports lifecycle, primary exit status, and a bounded log tail, while `bash` can terminate the complete owned tree.
- **Cross-platform.** Full support for Windows x86-64, with compatibility release assets for Linux x86-64, Linux ARM64, and macOS Apple Silicon.
- **Reads PDFs.** `read` detects PDFs by content, not extension, and returns page text or rendered images with continuation cursors for long documents.

## Tools

| Tool | Description |
| --- | --- |
| `read` | Read source files with line numbers. Supports UTF-8, BOM-detected UTF-16, and WHATWG encoding labels. Also reads PDFs. |
| `grep` | Search file contents with Rust regex or literal strings. |
| `glob` | Find files. Gitignored files are included by default; `.git` and common large directories stay excluded. |
| `run_program` | Run one program with a literal argument list, without a shell. |
| `bash` | Run a POSIX bash command line and return merged stdout and stderr. |
| `bash_status` | Inspect one detached Bash job and its bounded log tail. |

## Install

**Windows (PowerShell):**

```powershell
irm https://github.com/possible055/agentshim/releases/latest/download/install.ps1 | iex
```

Installs to `%LOCALAPPDATA%\agentshim\bin\agentshim.exe` (e.g. `C:\Users\<user>\AppData\Local\agentshim\bin\agentshim.exe`).

**Linux / macOS:**

```sh
curl -fsSL https://github.com/possible055/agentshim/releases/latest/download/install.sh | sh
```

Installs to `${XDG_DATA_HOME:-$HOME/.local/share}/agentshim/bin/agentshim` (e.g. `~/.local/share/agentshim/bin/agentshim`).

Re-run the same command to update. Install a specific version with `-Version` (PowerShell) or `--version` (sh).

**Build from source** (requires Rust 1.88):

```console
cargo build --release --locked
```

The binary is at `target/release/agentshim` (Linux and macOS) or `target/release/agentshim.exe` (Windows).

An existing `codexshim` installation is not removed or overwritten. After installing AgentShim, update each client to the new executable and MCP server name, verify the six tools, and then remove the old installation if it is no longer needed.

## Configure Codex

Copy the matching example into `~/.codex/config.toml` (user-level) or a project's `.codex/config.toml`, then replace `command` with the absolute path to your `agentshim` binary:

- [Windows example](config/codex.windows.toml.example)
- [Linux example](config/codex.linux.toml.example)
- [macOS example](config/codex.macos.toml.example)

```toml
[mcp_servers.agentshim]
required = true
command = "/absolute/path/to/agentshim"
args = ["serve", "--client-profile", "codex"]
# Unrestricted mode (default) allows any absolute read/grep/glob path. To
# restrict to repository and Codex skill/plugin paths, use:
# args = ["serve", "--client-profile", "codex", "--read-scope", "normal"]
supports_parallel_tool_calls = true
tool_timeout_sec = 600
enabled_tools = ["read", "grep", "glob", "run_program", "bash", "bash_status"]
default_tools_approval_mode = "approve"
env = { CODEX_MCP_PROTOCOL_VERSION = "2026-07-28" }

[features]
mcp_2026_07_28 = true
```

## Configure Cursor

Copy the [Cursor example](config/cursor.mcp.json.example) to `~/.cursor/mcp.json`, replace `command` with the absolute path to the binary, and restart Cursor:

```json
{
  "mcpServers": {
    "agentshim": {
      "type": "stdio",
      "command": "/absolute/path/to/agentshim",
      "args": ["serve", "--client-profile", "cursor"]
    }
  }
}
```

On Windows, JSON paths must escape each backslash.

## Configure DSH

Install the native adapter and its exact optional platform package into your target DSH profile (e.g. `web` for Web UI, `headless` for CLI):

```sh
dsh plugin --profile web add dsh-agentshim
dsh web --dump-config
```

DSH loads `agentshim-core` through the platform addon in-process; it does not start the MCP server or require an installed `agentshim` executable. Unsupported platforms, missing packages, and native API mismatches fail plugin activation. See the [DSH adapter guide](adapters/dsh/README.md) for configuration, capture retention, sandbox approval behavior, and removal.

## Options

### `--client-profile`

Selects the aggregate burst policy. These layers are separate limits, not a single cap:

| Layer | Value | Meaning |
| --- | ---: | --- |
| Codex per-item truncation | 10,000 tokens or bytes | Client history cap after `Wall time:` / `Output:` |
| Server content ceiling | 9,872 | 10,000 minus 128 wrapper tokens |
| Per-call ceiling | 8,192 | Both profiles; a single page cannot exceed this currently |
| Burst aggregate | profile default | Remaining budget split across in-flight calls |

| Value | Per-call token ceiling | Default burst tokens |
| --- | ---: | ---: |
| `codex` (default) | 8,192 | 16,384 |
| `cursor` | 8,192 | 32,768 |

`AGENTSHIM_IDLE_TIMEOUT` enables idle shutdown for the `codex` profile. The `cursor` profile always disables the watchdog, but setting an invalid value still fails startup.

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

`bash` accepts `detach` with a `log_path` inside the repository. Output goes to that file and the call returns an opaque instance-bound `job_id`, plus the diagnostic pid and log path:

```json
{ "command": "cargo test > /dev/null; echo EXIT=$?", "detach": true, "log_path": "local/test.log" }
```

Use `bash_status` for an immediate state/exit snapshot and a bounded tail (`tail_bytes: 0` returns metadata only):

```json
{ "job_id": "bash-550e8400-e29b-41d4-a716-446655440000", "tail_bytes": 8192 }
```

Terminate the complete server-owned tree through `bash` itself:

```json
{ "action": "terminate", "job_id": "bash-550e8400-e29b-41d4-a716-446655440000" }
```

Up to 16 detached trees may be active. `timeout_ms` is measured from successful process-tree spawn; omitted values use `AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX`, while explicit values may only shorten it. Deadline expiry actively terminates the complete tree and records `timed_out`. A log that exceeds `AGENTSHIM_DETACHED_LOG_BYTES` terminates its tree and records `log_quota_exceeded`. The instance retains the latest 32 terminal records, each with at most 16 KiB of final log tail; IDs do not survive reconnect or restart, and there is no list API. The full bounded log remains available through `read(log_path)` and terminal eviction never deletes it.

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
| `pdf_cursor` | opaque token | Replayed verbatim from a previous response. Carries the source version and, when the response stopped inside a page, where to resume in it. |

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
| `AGENTSHIM_PROCESS_CALLS` | `16` | Per-instance concurrent process-call limit; 1–32. |
| `AGENTSHIM_READ_ONLY_CALLS` | `16` | Per-instance concurrent `read`/`glob`/`grep` call limit; 1–32. |
| `AGENTSHIM_DETACHED_CALLS` | `16` | Per-instance live detached `bash` trees; 1–16. |
| `AGENTSHIM_DETACHED_LOG_BYTES` | `67108864` | Per-job detached log termination threshold; 1048576–4294967296. Exceeding it terminates the owned process tree; an in-flight write block may extend the final file past the threshold. |
| `AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX` | `1800` | Maximum detached/background Bash runtime in seconds; 600–14400. Omitted job timeouts use this value and explicit timeouts may only shorten it. |
| `AGENTSHIM_OUTPUT_BYTES` | `32000` | Per-call output ceiling in bytes; 4096–262144. |
| `AGENTSHIM_BURST_TOKENS` | profile default | Shared projected model-token budget; 2048–32768. |
| `AGENTSHIM_TOOL_TIMEOUT_SHELF` | `600` | The shelf value the server stays below so the client's `tool_timeout_sec` fires after the server's own Timeout. Effective max execution time is shelf minus 10 seconds; 15–3600. |
| `AGENTSHIM_IDLE_TIMEOUT` | off | Idle shutdown in seconds for the `codex` profile only; 1–86400. Inbound JSON-RPC messages reset the deadline, and active foreground calls or detached trees defer shutdown. |
| `AGENTSHIM_GREP_MEMORY_BYTES` | `268435456` | Per-call hard limit for retained `grep` candidates. |
| `AGENTSHIM_GLOB_MEMORY_BYTES` | `67108864` | Per-call hard limit for retained `glob` matches. |
| `AGENTSHIM_PDF_TEXT_MEMORY_BYTES` | `67108864` | Per-call memory budget for `auto`/`text` PDF reads. |
| `AGENTSHIM_PDF_IMAGE_MEMORY_BYTES` | `100663296` | Per-call memory budget for `image` PDF reads. |
| `AGENTSHIM_WINDOWS_ACTIVE_PROCESS_LIMIT` | off | Optional Windows Job Object active-process hard limit per foreground or detached tree; 1–256. |
| `AGENTSHIM_WINDOWS_JOB_MEMORY_BYTES` | off | Optional Windows Job Object aggregate committed-memory hard limit; 67108864–17179869184. |
| `AGENTSHIM_WINDOWS_PROCESS_MEMORY_BYTES` | off | Optional Windows Job Object per-process committed-memory hard limit; 67108864–17179869184. |
| `AGENTSHIM_BASH` | probed | Absolute path to a GNU bash. In the DSH adapter, the same key set in plugin config `env` also drives load-time bash discovery, so it need not be preset on the host process. |
| `AGENTSHIM_LOG_MODE` | `errors` | One of `off`, `errors`, `all`. |
| `AGENTSHIM_LOG_DIR` | platform default | Override the log directory with an absolute path. |
| `AGENTSHIM_RESPECT_GITIGNORE` | `false` | When `true`, `grep` and `glob` apply `.gitignore` / `.ignore` filters. Omitted `include_ignored` follows this default. Because the caller cannot read this setting, an empty result under active filtering ends with a line recommending `include_ignored=true`. `.git` and `node_modules`, `target`, `.venv`, `venv`, `dist`, `build`, `__pycache__` stay excluded either way. Binary, output-budget, and memory limits still apply. |

The shared 256 MiB runtime memory value is an internal soft reservation target, not a process RSS limit. PDF and search code also enforce their documented per-call limits. Child processes are outside that target; on Windows their optional hard limits are the Job Object settings above.

For the DSH adapter on Windows, the three `AGENTSHIM_WINDOWS_*` Job Object settings above are read from the plugin's explicit `env` configuration as host policy as well as being passed to child processes. The native host does not consult its ambient process environment.

The stdio transport admits at most 256 requests and 16 MiB of serialized request data without completed responses. Exceeding either backlog limit shuts down the instance instead of retaining more handlers. Five seconds without stdout write progress also shuts down the instance so the host can restart it.

The idle watchdog rechecks the activity timestamp after confirming quiescence before it
cancels the existing graceful-shutdown token. A request arriving in the final interval
between that check and cancellation can still race with shutdown; enabling the watchdog
accepts that narrow boundary condition.

## Diagnostics

Logs are UTC-dated JSONL files:

- Windows: `%LOCALAPPDATA%\agentshim\logs`
- Linux: `${XDG_STATE_HOME:-$HOME/.local/state}/agentshim/logs`

Retention: 512 MiB total, 30 days. Inspect or purge:

```console
agentshim logs status
agentshim logs purge
```

Records contain identifiers, phases, outcomes, timings, and error classes — never MCP arguments, grep patterns, process arguments, stdin, file contents, or stdout/stderr. Set `AGENTSHIM_LOG_MODE=all` while reproducing tool-loading failures.

## Acknowledgments

- [PDFOxide](https://github.com/yfedoseev/pdf_oxide) — PDF reading backend
- [Gigatoken](https://github.com/marcelroed/gigatoken) — token counting backend
- [FastCtx](https://github.com/yc-duan/fastctx) — design and benchmarking reference for `read`, `grep`, and `glob`
- [Linux Do](https://linux.do/) — forum community that inspired the initial idea for this project

## License

[MIT](LICENSE)
