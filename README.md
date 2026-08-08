# codexshim

English | [简体中文](README.zh-CN.md)

`codexshim` extends the `codex` CLI with a small, capability-scoped set of tools for working with source code. It runs as a local stdio server and treats its startup directory as the repository root.

## Why use it

- **Bounded file access.** `read`, `grep`, and `glob` operate inside the repository by default, with optional access to Codex skill and plugin directories.
- **No shell injection.** `run_process` takes an executable and a literal argument list — no pipes, redirections, wildcards, or variable expansion.
- **Cross-platform.** Natively supports Windows, Linux, and macOS (Intel and Apple Silicon).

## Tools

| Tool | Description |
| --- | --- |
| `read` | Read source files with line numbers. Supports UTF-8 and UTF-16. |
| `grep` | Search file contents with Rust regex or literal strings. |
| `glob` | Find files while respecting repository ignore rules. |
| `run_process` | Run one program with a structured argument list. |

Every tool returns a typed `structuredContent` object together with a bounded text rendering. `read` reports `next_start_line`; `grep` and `glob` report `next_offset`, total results, page limits, and skipped-entry counters. Tool failures use a stable `{ error: { code, message, retryable, details } }` envelope.

## Install

Prebuilt binaries are available for Linux, macOS, and Windows 11 (build 22621+). Windows uses a local fixed NTFS drive.

### Prebuilt release (recommended)

Install or update the latest release without modifying `PATH` or Codex configuration.

**Windows (PowerShell):**

```powershell
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; irm https://github.com/possible055/codexshim/releases/latest/download/install.ps1 | iex
```

**Linux:**

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/possible055/codexshim/releases/latest/download/install.sh | sh
```

**macOS:**

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/possible055/codexshim/releases/latest/download/install.sh | sh
```

Default install locations:

- Windows: `%LOCALAPPDATA%\codexshim\bin\codexshim.exe`
- Linux: `${XDG_DATA_HOME:-$HOME/.local/share}/codexshim/bin/codexshim`
- macOS: `${XDG_DATA_HOME:-$HOME/.local/share}/codexshim/bin/codexshim`

Override with `-InstallDir` (PowerShell) or `--install-dir` (sh). Re-run the same command to update.

To install a specific or prerelease version, pass `-Version` (PowerShell) or `--version` (sh):

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/possible055/codexshim/releases/download/v0.1.3-alpha.3/install.sh | sh -s -- --version 0.1.3-alpha.3
```

### Build from source

Requires Rust 1.88.

```console
cargo build --release --locked
cargo run --locked -- doctor
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
args = ["serve"]
required = true
supports_parallel_tool_calls = true
startup_timeout_sec = 15
tool_timeout_sec = 310
enabled_tools = ["read", "grep", "glob", "run_process"]
default_tools_approval_mode = "writes"
env = { CODEX_MCP_PROTOCOL_VERSION = "2026-07-28" }

[mcp_servers.codexshim.tools.run_process]
approval_mode = "prompt"

[features]
mcp_2026_07_28 = true
```

On Windows, use a single-quoted TOML path such as `'C:\Users\me\AppData\Local\codexshim\bin\codexshim.exe'` to avoid escaping backslashes.

Start Codex in the repository you want to work on. `codexshim` treats that working directory as the repository root for the lifetime of the server.

`supports_parallel_tool_calls = true` allows Codex to issue all four codexshim tools concurrently. Each codexshim instance independently admits up to 16 active `run_process` calls and 16 active read-only calls (`read`, `grep`, and `glob` combined). Set `CODEXSHIM_PROCESS_CALLS` to an integer from 1 through 32 to override the process limit; invalid values prevent startup. Admission remains fail-fast and returns a retryable `resource_busy` error when a class is full.

## Options

### `--read-scope`

Controls which paths `read`, `grep`, and `glob` may access outside the repository. Set it in the MCP server `args`:

| Value | Behavior |
| --- | --- |
| `normal` (default) | Repository paths plus Codex skill/plugin directories (`$CODEX_HOME/{skills,plugins}`, or `~/.codex` and `~/.agents` equivalents). Credentials and history under `.codex` stay inaccessible. |
| `unrestricted` | Any absolute path readable by the server user. On Windows: local fixed NTFS volumes only — UNC shares, removable volumes, device namespaces, named pipes, and drive-relative paths are rejected. |

```toml
args = ["serve", "--read-scope", "unrestricted"]
```

Relative paths and absolute paths inside the repository always use the repository capability in both modes. `run_process` is unaffected by this option. The same flag is accepted by `codexshim doctor --read-scope <value>` for diagnostics.

### Environment variables

| Variable | Default | Description |
| --- | --- | --- |
| `CODEX_MCP_PROTOCOL_VERSION` | — | MCP protocol version advertised to Codex. |
| `CODEXSHIM_MCP_COMPATIBILITY` | `lenient` | Set to `strict` to reject legacy `2025-06-18` initialize clients. |
| `CODEXSHIM_PROCESS_CALLS` | `16` | Per-instance concurrent `run_process` limit; accepts integers from 1 through 32. |
| `CODEXSHIM_LOG_MODE` | `errors` | One of `off`, `errors`, `all`. |
| `CODEXSHIM_LOG_DIR` | platform default | Override the log directory with an absolute path. |

## Diagnostics

Logs are written as UTC-dated JSONL files:

- Windows: `%LOCALAPPDATA%\codexshim\logs`
- Linux: `${XDG_STATE_HOME:-$HOME/.local/state}/codexshim/logs`

Retention: 64 MiB per file part, 128 MiB per UTC day, 512 MiB total, 30 days. The active day is always preserved. Inspect or purge explicitly:

```console
codexshim logs status
codexshim logs purge
```

Records contain identifiers, phases, outcomes, timings, and error classes — never MCP arguments, grep patterns, process arguments or environment, stdin, file contents, or stdout/stderr.

## License

[MIT](LICENSE)
