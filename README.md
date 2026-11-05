# AgentShim

English | [简体中文](README.zh-CN.md)

AgentShim gives coding agents tools to read and search files and run commands. Codex and Cursor connect to its local MCP server; DSH can use the native adapter instead.

Windows x64, Linux x64, and Linux ARM64 are natively supported. A compatibility release is also available for macOS Apple Silicon.

## Quick start

### 1. Install

On Windows, run in PowerShell:

```powershell
irm https://github.com/possible055/agentshim/releases/latest/download/install.ps1 | iex
```

On Linux or macOS Apple Silicon, run:

```sh
curl -fsSL https://github.com/possible055/agentshim/releases/latest/download/install.sh | sh
```

The installer prints the executable's absolute path. By default, it installs to:

| System | Executable |
| --- | --- |
| Windows | `%LOCALAPPDATA%\agentshim\bin\agentshim.exe` |
| Linux | `${XDG_DATA_HOME:-$HOME/.local/share}/agentshim/bin/agentshim` |
| macOS | `${XDG_DATA_HOME:-$HOME/.local/share}/agentshim/bin/agentshim` |

Re-running the command installs the latest stable release. It can downgrade an existing prerelease, so check your current version first. To install a specific release, including an alpha, replace `0.1.7-alpha.3` below with the desired published version:

```powershell
$installer = Invoke-RestMethod https://github.com/possible055/agentshim/releases/latest/download/install.ps1
& ([scriptblock]::Create($installer)) -Version 0.1.7-alpha.3
```

```sh
curl -fsSL https://github.com/possible055/agentshim/releases/latest/download/install.sh | sh -s -- --version 0.1.7-alpha.3
```

These commands install the specified version; they do not automatically track newer alpha releases. If you used a custom directory, pass the same directory with `-InstallDir` (PowerShell) or `--install-dir` (sh) so the MCP configuration continues to point to the updated executable. Restart your MCP client after updating.

### 2. Configure your MCP client

#### Codex

Add the configuration for your system to `~/.codex/config.toml` or your project's `.codex/config.toml`. Replace the example `command` path with the absolute path printed by the installer. In particular, a custom installation directory or `XDG_DATA_HOME` changes the path.

If the file already has a `[features]` section, add the feature setting to that section instead of creating another one. Keep any existing MCP server settings.

By default, `read`, `grep`, and `glob` can access any path readable by the server user. Add `"--read-scope", "normal"` to `args` to restrict those tools to the repository and Codex skill/plugin paths. This does not restrict processes started by `run_program` or `bash`.

<details>
<summary>Windows x64</summary>

```toml
[mcp_servers.agentshim]
required = true
command = "C:/Users/<username>/AppData/Local/agentshim/bin/agentshim.exe"
args = ["serve", "--client-profile", "codex"]
supports_parallel_tool_calls = true
tool_timeout_sec = 600
enabled_tools = ["read", "grep", "glob", "run_program", "bash", "bash_status"]
default_tools_approval_mode = "approve"
env = { CODEX_MCP_PROTOCOL_VERSION = "2026-07-28" }

[features]
mcp_2026_07_28 = true
```

</details>

<details>
<summary>Linux x64 / ARM64</summary>

```toml
[mcp_servers.agentshim]
required = true
command = "/home/<username>/.local/share/agentshim/bin/agentshim"
args = ["serve", "--client-profile", "codex"]
supports_parallel_tool_calls = true
tool_timeout_sec = 600
enabled_tools = ["read", "grep", "glob", "run_program", "bash", "bash_status"]
default_tools_approval_mode = "approve"
env = { CODEX_MCP_PROTOCOL_VERSION = "2026-07-28" }

[features]
mcp_2026_07_28 = true
```

</details>

<details>
<summary>macOS Apple Silicon</summary>

```toml
[mcp_servers.agentshim]
required = true
command = "/Users/<username>/.local/share/agentshim/bin/agentshim"
args = ["serve", "--client-profile", "codex"]
supports_parallel_tool_calls = true
tool_timeout_sec = 600
enabled_tools = ["read", "grep", "glob", "run_program", "bash", "bash_status"]
default_tools_approval_mode = "approve"
env = { CODEX_MCP_PROTOCOL_VERSION = "2026-07-28" }

[features]
mcp_2026_07_28 = true
```

</details>

These examples set `default_tools_approval_mode` to `"approve"`. Review this setting against your preferred Codex approval policy before using it.

<details>
<summary>Cursor</summary>

Add the following to `~/.cursor/mcp.json`, replacing `command` with the absolute path printed by the installer. On Windows, escape backslashes in JSON or use forward slashes.

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

See the [Cursor example](config/cursor.mcp.json.example).

</details>

### 3. Check the connection

Restart your client and check that the AgentShim MCP server is connected. It should provide `read`, `grep`, `glob`, `run_program`, `bash`, and `bash_status`. If it does not connect, check the executable path and your client's configuration, then see [Diagnostics](#diagnostics).

## Tools and access

| Tool | Purpose |
| --- | --- |
| `read` | Read text, PDF, and Office files. |
| `grep` | Search file contents using a Rust regex or literal string. |
| `glob` | Find files by pattern. |
| `run_program` | Run an executable with literal arguments, without a shell. |
| `bash` | Run a Bash command, in the foreground or as a detached job. |
| `bash_status` | Check a detached job and read the end of its log. |

The MCP server uses its starting directory as the repository root. `grep` and `glob` include gitignored files by default, but exclude `.git` and common large directories.

## Other ways to use AgentShim

### DSH

The DSH adapter loads a platform native addon in-process; it does not use the MCP server or require the `agentshim` executable. Install it in the profile you use:

```sh
dsh plugin --profile web add dsh-agentshim
dsh web --dump-config
```

See the [DSH adapter guide](adapters/dsh/README.md) for requirements, configuration, and removal.

<details>
<summary>Build from source</summary>

Requires Rust 1.99.1:

```sh
cargo build --release --locked
```

The executable is `target/release/agentshim` on Linux and macOS, or `target/release/agentshim.exe` on Windows. Use its absolute path in your MCP configuration.

</details>

## Reference

<details>
<summary>Client profiles and read scope</summary>

`--client-profile` selects the aggregate burst policy. These layers are separate limits, not a single cap:

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

`--read-scope` only bounds `read`, `grep`, and `glob`. MCP process tools deliberately remain unrestricted and inherit the filesystem access of the server user; agentshim does not add a process sandbox.

The DSH adapter uses the official DSH `ctx.sandbox` and `ctx.sandboxPolicy` services when they are composed. It passes the prepared, exact argv through that service and never substitutes a second sandbox provider or silently retries an unconfined command after confinement fails.

The versioned cross-adapter contract is [`contracts/tool-contract-v1.json`](contracts/tool-contract-v1.json). The generator also checks in the MCP JSON-Schema projection and DSH parameter/output projection under [`contracts/generated/`](contracts/generated/). Validate every projection with `python3 scripts/generate-contracts.py --check`. MCP keeps legacy error classes in `error.code`; classes with a v1 mapping are normalized in `error.canonicalCode`, while unmapped legacy classes retain their class name.

</details>

<details>
<summary>Detached Bash jobs and Windows arguments</summary>

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

</details>

<details>
<summary>Reading PDF and Office documents</summary>

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

The response tells you how far it got and how to continue. A document that mixes readable and image-only pages is a success: readable pages come back as Markdown, the rest become placeholders. One instance runs at most one structured-document call at a time; a second concurrent PDF or Office call returns a retryable `resource_busy`.

### Reading Office documents

`read` accepts DOCX, XLSX, PPTX, DOC, XLS, and PPT paths and returns Markdown. The extension selects the Office candidate family, while the ZIP content type or CFB stream signature determines the actual format. Office inputs reject text and PDF parameters. A partial response carries an opaque `office_cursor`; replay it verbatim with the same path. PDF and Office reads share one structured-document slot, so they cannot hold their large memory reservations concurrently.

</details>

<details>
<summary>Environment variables and runtime limits</summary>

| Variable | Default | Description |
| --- | --- | --- |
| `CODEX_MCP_PROTOCOL_VERSION` | — | MCP protocol version advertised to Codex. |
| `AGENTSHIM_IO_WORKERS` | available CPUs, up to `4` | Worker lanes; 1–16. |
| `AGENTSHIM_FOREGROUND_CALLS` | `16` | Shared per-instance limit for concurrent `read`, `glob`, `grep`, `run_program`, and foreground `bash` calls; 1–32. |
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
| `AGENTSHIM_BASH` | probed | Absolute path to a GNU bash (Git Bash `bash.exe`), a BusyBox-w32 dispatcher (`busybox64u.exe`), or an applet-named BusyBox copy; the shell flavor and invocation form are auto-detected by the probe. In the DSH adapter, the same key set in plugin config `env` also drives load-time bash discovery, so it need not be preset on the host process. |
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

</details>

## Diagnostics

Logs are UTC-dated JSONL files:

- Windows: `%LOCALAPPDATA%\agentshim\logs`
- Linux and macOS: `${XDG_STATE_HOME:-$HOME/.local/state}/agentshim/logs`

Retention: 512 MiB total, 30 days. Inspect or purge:

```console
agentshim logs status
agentshim logs purge
```

Records contain identifiers, phases, outcomes, timings, and error classes — never MCP arguments, grep patterns, process arguments, stdin, file contents, or stdout/stderr. Set `AGENTSHIM_LOG_MODE=all` while reproducing tool-loading failures.

## Acknowledgments

- [PDFOxide](https://github.com/yfedoseev/pdf_oxide) — PDF reading backend
- [OfficeOxide](https://github.com/yfedoseev/office_oxide) — source of the selectively derived Office readers
- [Gigatoken](https://github.com/marcelroed/gigatoken) — token counting backend
- [FastCtx](https://github.com/yc-duan/fastctx) — design and benchmarking reference for `read`, `grep`, and `glob`
- [Linux Do](https://linux.do/) — forum community that inspired the initial idea for this project

## License

AgentShim's own code is licensed under [MIT](LICENSE). Third-party components retain their respective licenses and notices.
