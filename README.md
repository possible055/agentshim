# codexshim

English | [简体中文](README.zh-CN.md)

`codexshim` extends the `codex` CLI with a small, capability-scoped set of tools for working with source code. It runs as a local stdio server and treats its startup directory as the repository root.

## Why use it

- **Bounded file access.** `read`, `grep`, and `glob` operate inside the repository by default, with optional access to Codex skill and plugin directories.
- **No shell injection.** `run_process` takes an executable and a literal argument list — no pipes, redirections, wildcards, or variable expansion.
- **Cross-platform.** Prebuilt binaries for Linux and Windows 11 (build 22621+).

## Tools

| Tool | Description |
| --- | --- |
| `read` | Read source files with line numbers. Supports UTF-8 and UTF-16. |
| `grep` | Search file contents with Rust regex or literal strings. |
| `glob` | Find files while respecting repository ignore rules. |
| `run_process` | Run one program with a structured argument list. |

Every tool returns a typed `structuredContent` object together with a bounded text rendering. `read` reports `next_start_line`; `grep` and `glob` report `next_offset`, total results, page limits, and skipped-entry counters. Tool failures use a stable `{ error: { code, message, retryable, details } }` envelope.

The repository performance harness uses internal engine access that is excluded from the production API. Run it with `cargo bench --locked --features bench-internals --bench performance`. The harness enforces scale-aware p95 limits for read, glob, and grep; the stdio harness enforces cold-start, p95, and process limits. CI sets the accepted limits explicitly through `CODEXSHIM_BENCH_MAX_*` environment variables.

## Install

Prebuilt binaries are available for Linux and Windows 11 (build 22621+) on a local fixed NTFS drive.

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

Default install locations:

- Windows: `%LOCALAPPDATA%\codexshim\bin\codexshim.exe`
- Linux: `${XDG_DATA_HOME:-$HOME/.local/share}/codexshim/bin/codexshim`

Override with `-InstallDir` (PowerShell) or `--install-dir` (sh). Re-run the same command to update.

### Manual download

Download the archive and `SHA256SUMS` from [GitHub Releases](https://github.com/possible055/codexshim/releases):

- Windows: `codexshim-<version>-x86_64-pc-windows-msvc.zip`
- Linux: `codexshim-<version>-x86_64-unknown-linux-gnu.tar.gz`

Verify, then extract to a stable directory:

- Windows: `Get-FileHash <archive> -Algorithm SHA256`
- Linux: `sha256sum -c SHA256SUMS --ignore-missing`

### Build from source

Requires Rust 1.88.

```console
cargo build --release --locked
cargo run --locked -- doctor
```

The binary is at `target/release/codexshim` (Linux) or `target/release/codexshim.exe` (Windows).

## Configure Codex

Copy the matching example into `~/.codex/config.toml` (user-level) or a project's `.codex/config.toml`, then replace `command` with the absolute path to your `codexshim` binary:

- [Windows example](config/codex.windows.toml.example)
- [Linux example](config/codex.linux.toml.example)

```toml
[mcp_servers.codexshim]
command = "/absolute/path/to/codexshim"
args = ["serve"]
required = true
startup_timeout_sec = 15
tool_timeout_sec = 310
enabled_tools = ["read", "grep", "glob", "run_process"]
default_tools_approval_mode = "writes"
env = { CODEX_MCP_PROTOCOL_VERSION = "2026-07-28" }

[mcp_servers.codexshim.tools.run_process]
approval_mode = "prompt"

[features]
shell_tool = true
mcp_2026_07_28 = true
```

On Windows, use a single-quoted TOML path such as `'C:\Users\me\AppData\Local\codexshim\bin\codexshim.exe'` to avoid escaping backslashes.

Start Codex in the repository you want to work on. `codexshim` treats that working directory as the repository root for the lifetime of the server.

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

## Notes

- Always configure the prebuilt release executable as the long-lived MCP server. Do not use `cargo run` as the MCP command — nested Cargo calls may contend for build locks.
- On Windows, stop the active MCP server before rebuilding or replacing its executable. A running `.exe` cannot be overwritten; restart Codex afterwards.
- Tool paths are interpreted by the platform running the server. Use Windows paths or repository-relative paths with a Windows server; `/mnt/...` paths only with a Linux server inside WSL.
- Developer Mode is not required. Directory symbolic links, junctions, and other reparse points are never followed during traversal; explicit symlink starting paths are rejected for unrestricted external operations.

## License

[Apache-2.0](LICENSE)
