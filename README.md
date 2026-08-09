# codexshim

English | [简体中文](README.zh-CN.md)

`codexshim` extends the `codex` CLI with a small, capability-scoped set of tools for working with source code. It runs as a local stdio server and treats its startup directory as the repository root.

## Why use it

- **Bounded file access.** `read`, `grep`, and `glob` operate inside the repository by default, with optional access to Codex skill and plugin directories.
- **Two execution shapes.** `run_program` takes an executable and a literal argument list, so no shell parses the arguments; `bash` takes a POSIX command line when composition is what you need.
- **Allowlisted programs.** `run_program` denies everything until an operator names the programs it may launch. Bare names are convenient invocation policies; absolute entries pin canonical executable identity.
- **Cross-platform.** Natively supports Windows, Linux, and macOS (Intel and Apple Silicon).

## Tools

| Tool | Description |
| --- | --- |
| `read` | Read source files with line numbers. Supports UTF-8, BOM-detected UTF-16, and explicitly selected WHATWG encoding labels. |
| `grep` | Search file contents with Rust regex or literal strings. |
| `glob` | Find files while respecting repository ignore rules. |
| `run_program` | Run one allowlisted program with a literal argument list, without a shell. |
| `bash` | Run a POSIX bash command line and return merged stdout and stderr. |

Successful tools return bounded text results. Partial `read`, `grep`, and `glob` results include continuation metadata in the text rendering, `run_program` reports its exit status with separate stdout and stderr byte counts, and `bash` reports one merged output section. The output budget adapts to content: CJK-dense text is held to a smaller byte budget because the client tokenizes it about twice as densely as English. Tool failures also return a stable `{ error: { code, message, retryable, details } }` structured envelope.

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
args = ["serve", "--allow-programs", "git,cargo,rustup,gh"]
required = true
supports_parallel_tool_calls = true
startup_timeout_sec = 15
tool_timeout_sec = 610
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

The two approval modes encode the difference between the tools: `run_program` is constrained by the allowlist and takes structured argv, so it can be approved on request, while `bash` accepts an arbitrary command line and always prompts. Neither the allowlist nor `bash` is a security sandbox.

`tool_timeout_sec` must be at least the server's 600000 ms ceiling plus headroom, or the client gives up before the server does.

On Windows, use a single-quoted TOML path such as `'C:\Users\me\AppData\Local\codexshim\bin\codexshim.exe'` to avoid escaping backslashes.

Start Codex in the repository you want to work on. `codexshim` treats that working directory as the repository root for the lifetime of the server.

`supports_parallel_tool_calls = true` allows Codex to issue codexshim tools concurrently. Each codexshim instance independently admits up to 16 active foreground process calls (`run_program` and foreground `bash` share that class), 16 active read-only calls (`read`, `grep`, and `glob` combined), and 16 live detached trees. Set `CODEXSHIM_PROCESS_CALLS` to an integer from 1 through 32 to override the process limit; invalid values prevent startup. Admission remains fail-fast and returns a retryable `resource_busy` error when a class is full. codexshim cannot tell whether two concurrent calls conflict semantically — do not issue state-changing commands against the same working tree in parallel.

## Options

### `--allow-programs`

Names the programs `run_program` may launch, as a comma-separated list. It is **empty by default, which denies every program**; there is no wildcard, because "run anything" is what `bash` is for.

```toml
args = ["serve", "--allow-programs", "git,cargo,rustup,gh"]
```

An entry is either a bare program name or an absolute path. A bare name matches the file stem of either the resolved invocation path or its canonical executable target, so Unix multicall proxies such as `cargo -> rustup` work and `git` covers `git.exe` under any install prefix. This convenience policy also means an executable invoked through an allowlisted alias is permitted; it is not an identity guarantee. An absolute entry is checked only against the canonical executable and pins that identity. Matching happens **after** PATH resolution on every call, including cache hits. Windows comparisons are ASCII case-insensitive. An empty entry, or a relative entry containing a path separator, prevents startup.

`CODEXSHIM_ALLOW_PROGRAMS` is a secondary source with the same syntax; the startup flag wins when both are set. `codexshim doctor --allow-programs <value>` accepts the same flag and prints the resolved list.

A denied call returns a non-retryable `not_permitted` error naming the resolved path and pointing at `bash`.

On Windows, codexshim derives the Git-for-Windows toolchain directories from the bash it probed and prepends those directories to the inherited `PATH`. This lets the shell see its own `sleep`, `grep`, `sed`, and `locale`; it does not filter inherited entries or provide a configurable curated `PATH`.

### Long-running work

`bash` accepts `detach` with a `log_path` inside the repository. The command's merged output goes straight to that file, and the call returns the pid and log path immediately instead of blocking for up to ten minutes:

```json
{ "command": "cargo test > /dev/null; echo EXIT=$?", "detach": true, "log_path": "local/test.log" }
```

Poll progress with `read` on `log_path` — its `next_start_line` continuation metadata is the cursor. A detached tree is tracked by the codexshim instance rather than the call. On Windows, lifecycle ownership covers the Job Object. On Unix, it covers the process group codexshim created; a program that calls `setsid()` or daemonizes can escape that group, so this is best-effort lifecycle cleanup rather than a sandbox. `CODEXSHIM_DETACHED_CALLS` bounds how many may be live at once (1–16, default 16); the roster is reserved before blocking scheduling, so a full roster fails immediately with a retryable `resource_busy` error listing the live pids and their log paths. `log_path` is admitted lexically and then opened through the retained repository capability, which blocks symlink and junction escapes.

### `--read-scope`

Controls which paths `read`, `grep`, and `glob` may access outside the repository. Set it in the MCP server `args`:

| Value | Behavior |
| --- | --- |
| `normal` (default) | Repository paths plus Codex skill/plugin directories (`$CODEX_HOME/{skills,plugins}`, or `~/.codex` and `~/.agents` equivalents). Credentials and history under `.codex` stay inaccessible. |
| `unrestricted` | Any absolute path readable by the server user. On Windows: local fixed NTFS volumes only — UNC shares, removable volumes, device namespaces, named pipes, and drive-relative paths are rejected. |

```toml
args = ["serve", "--read-scope", "unrestricted"]
```

Relative paths and absolute paths inside the repository always use the repository capability in both modes.

`--read-scope` is the structured access range of `read`, `grep`, and `glob`. It is **not** a boundary on what a spawned process can reach: any program `run_program` or `bash` launches inherits the server user's ordinary filesystem access. The allowlist reduces accidental misuse and approval modes provide a policy gate, but neither isolates the child process; use an OS sandbox when isolation is required. The same flag is accepted by `codexshim doctor --read-scope <value>` for diagnostics.

### Environment variables

| Variable | Default | Description |
| --- | --- | --- |
| `CODEX_MCP_PROTOCOL_VERSION` | — | MCP protocol version advertised to Codex. |
| `CODEXSHIM_MCP_COMPATIBILITY` | `lenient` | Set to `strict` to reject legacy `2025-06-18` initialize clients. |
| `CODEXSHIM_PROCESS_CALLS` | `16` | Per-instance concurrent process-call limit shared by `run_program` and `bash`; accepts integers from 1 through 32. |
| `CODEXSHIM_DETACHED_CALLS` | `16` | Per-instance live detached `bash` trees; accepts integers from 1 through 16. |
| `CODEXSHIM_ALLOW_PROGRAMS` | empty | Comma-separated `run_program` allowlist; the `--allow-programs` flag takes precedence. |
| `CODEXSHIM_OUTPUT_BYTES` | `32000` | Per-call output ceiling in bytes; accepts integers from 4096 through 262144. |
| `CODEXSHIM_GREP_MEMORY_BYTES` | `268435456` | Per-call hard limit for retained `grep` candidates; accepts integers from 8388608 through 1073741824. |
| `CODEXSHIM_GLOB_MEMORY_BYTES` | `33554432` | Per-call hard limit for retained `glob` matches; accepts integers from 8388608 through 1073741824. |
| `CODEXSHIM_BASH` | probed | Absolute path to a GNU bash. An explicit override that fails validation is an error, never a fallback. |
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
