# codexshim

English | [简体中文](README.zh-CN.md)

`codexshim` gives the `codex` CLI a small, focused set of tools for working with source code. It runs as a local stdio MCP server and treats the directory you start it in as the repository root.

## Why use it

- **Bounded file access.** `read`, `grep`, and `glob` stay inside your repository by default, with optional access to Codex skill and plugin directories.
- **Two ways to run commands.** `run_program` takes one executable and a literal argument list — no shell ever parses the arguments. `bash` takes a POSIX command line when you need pipelines, redirection, or composition.
- **Cross-platform.** Full support for Windows x86-64, with compatibility release assets for Linux x86-64, Linux ARM64, and macOS Apple Silicon.
- **Reads PDFs.** `read` detects PDFs by content, not extension, and returns page text or rendered images with continuation cursors for long documents.

## Tools

| Tool | Description |
| --- | --- |
| `read` | Read source files with line numbers. Supports UTF-8, BOM-detected UTF-16, and explicitly selected WHATWG encoding labels. Also reads PDFs (see below). |
| `grep` | Search file contents with Rust regex or literal strings. |
| `glob` | Find files while respecting repository ignore rules. |
| `run_program` | Run one program with a literal argument list, without a shell. |
| `bash` | Run a POSIX bash command line and return merged stdout and stderr. |

Successful calls return bounded text. Partial `read`, `grep`, and `glob` results include a continuation cursor so you can pick up where you left off. The output budget adapts to content: CJK-dense text gets a smaller byte budget because the client tokenizes it about twice as densely as English. A per-instance 8,192-token burst gate also bounds the aggregate projected model cost of parallel and rapidly consecutive responses. It is a server-side best effort: it cannot account for Codex native tools, other MCP servers, model reasoning, or tool arguments, and its two-second quiet period is a burst heuristic rather than a Codex turn identifier. Failures return a stable `{ error: { code, message, retryable, details } }` envelope.

## Install

Windows x86-64 on Windows 11 (build 22621+) is fully supported and runs the complete native validation suite on every pull request and every push to `main`. Linux x86-64, Linux ARM64, and macOS Apple Silicon are compatibility-supported release targets: each release is built, packaged, hashed, and installed twice on its native runner, but these platforms do not run the full pull-request suite. Windows ARM64, macOS Intel, and other Rust targets are possible-support source builds with no release assets, CI guarantee, service level, or support commitment.

### Prebuilt release (recommended)

Installs or updates to the latest release without modifying `PATH` or Codex configuration.

**Windows (PowerShell):**

```powershell
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; irm https://github.com/possible055/codexshim/releases/latest/download/install.ps1 | iex
```

**Linux / macOS:**

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/possible055/codexshim/releases/latest/download/install.sh | sh
```

Default install locations:

- Windows: `%LOCALAPPDATA%\codexshim\bin\codexshim.exe`
- Linux / macOS: `${XDG_DATA_HOME:-$HOME/.local/share}/codexshim/bin/codexshim`

Override with `-InstallDir` (PowerShell) or `--install-dir` (sh). Re-run the same command to update.

Install a specific or prerelease version with `-Version` (PowerShell) or `--version` (sh):

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/possible055/codexshim/releases/download/v0.1.3-alpha.2/install.sh | sh -s -- --version 0.1.3-alpha.2
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

The two approval modes reflect the difference between the tools: `run_program` launches one executable with structured argv, so it can be approved on request; `bash` accepts an arbitrary command line and always prompts. Neither tool is a security sandbox.

`tool_timeout_sec` must be at least the server's 590-second ceiling plus headroom, or the client gives up first. The examples use 600.

On Windows, use a single-quoted TOML path such as `'C:\Users\me\AppData\Local\codexshim\bin\codexshim.exe'` to avoid escaping backslashes.

Start Codex in the repository you want to work on. `codexshim` treats that working directory as the repository root for the lifetime of the server.

`supports_parallel_tool_calls = true` lets Codex issue codexshim tools concurrently. Each instance independently admits up to 16 active process calls (`run_program` and foreground `bash`), 16 active read-only calls (`read`, `grep`, `glob` combined), and 16 live detached trees. When a class is full, the call fails fast with a retryable `resource_busy` error. codexshim cannot tell whether two concurrent calls conflict semantically — do not issue state-changing commands against the same working tree in parallel.

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

Relative paths and absolute paths inside the repository always use the repository capability in both modes.

`--read-scope` only bounds what `read`, `grep`, and `glob` can open. It is **not** a boundary on what a spawned process can reach: any program `run_program` or `bash` launches inherits the server user's ordinary filesystem access. Use an OS sandbox when you need real isolation.

### Long-running work

`bash` accepts `detach` with a `log_path` inside the repository. The command's merged output goes straight to that file, and the call returns the pid and log path immediately instead of blocking:

```json
{ "command": "cargo test > /dev/null; echo EXIT=$?", "detach": true, "log_path": "local/test.log" }
```

Poll progress with `read` on `log_path` — its `next_start_line` continuation metadata is the cursor. A detached tree is tracked by the codexshim instance until it exits. `CODEXSHIM_DETACHED_CALLS` bounds how many may be live at once (1–16, default 16); a full roster fails immediately with a retryable `resource_busy` listing the live pids and their log paths.

### Windows Bash argument conversion

Git Bash automatically converts arguments that look like POSIX paths before launching native Windows programs. Prefer `run_program` for one native program because its argv stays literal. When Bash composition is required and slash-style switches such as `robocopy /E` must remain unchanged, set `msys_argument_conversion` to `disabled`:

```json
{ "command": "robocopy \"$source\" \"$destination\" /E && printf 'copied\\n'", "msys_argument_conversion": "disabled" }
```

This sets `MSYS2_ARG_CONV_EXCL=*` for the entire Bash command. The field is accepted but has no effect on macOS and Linux.

### Reading PDFs

`read` detects a PDF from a `%PDF-` header at byte 0, so routing does not depend on the file extension. PDF inputs reject `encoding`, `start_line`, and `line_count`.

| Parameter | Values | Meaning |
| --- | --- | --- |
| `pdf_mode` | `auto` (default), `text`, `image` | `auto` and `text` return page Markdown; `image` renders PNG content blocks. |
| `pages` | `"7"` or `"7-12"` | One page or one continuous range. Discrete page lists are not accepted. A range past the last page is clamped; a range starting past it is an error. |
| `pdf_text_offset` | integer ≥ 0 | Resume one page's Markdown at a UTF-8 byte offset. |
| `pdf_source_id` | opaque token | Replayed from a previous response to prove the continuation targets the same source version. |

`auto` extracts text without rendering and never returns an image in the same call. `text` returns extracted text even when its quality is doubtful. `image` is an explicit render request; there is no OCR.

Page counts bound the work of one call, not the size of the answer:

| Mode | Without `pages` | Explicit range cap |
| --- | --- | --- |
| `auto`, `text` | first 10 pages | 20 pages |
| `image` | page 1 | 4 pages |

Whichever limit binds first, the response tells you how far it got and how to continue. The next range is as wide as what was actually delivered: if 6 of 20 requested pages fit the output budget, the continuation is `7-12`, not `7-26`.

Each delivered page reports a state on the `Pages:` line:

| State | Meaning |
| --- | --- |
| `text_ready` | Usable text of acceptable quality. |
| `text_uncertain` | Text is present but its quality or coverage is doubtful. Returned *with* the text, never instead of it. |
| `image_required` | No usable text, but something is drawn — including pure vector art with no embedded image. The response includes a `Retry:` line naming the `pdf_mode="image"` call that would render it. |
| `blank` | Nothing drawn and no text. |
| `unavailable` | This page could not be processed. Distinct from `blank`, which means there was nothing to read. |

A document that mixes readable and image-only pages is a success, not an error: readable pages come back as Markdown, the rest become placeholders. A single `unavailable` page does not fail the call. `pdf_image_required` is returned only when *every* selected page lacks usable text.

PDF work is expensive, so one instance runs at most one PDF call at a time. A second concurrent call waits briefly and then returns a retryable `resource_busy` with a `retry_after_ms` hint. Plain text reads never queue behind a PDF, so a slow PDF cannot delay them.

Each mode also has a wall-clock ceiling: 5 s for `auto` and `text`, 10 s for `image`. Exceeding it returns a retryable `resource_timeout`. PDF-specific failures (`pdf_invalid`, `pdf_encrypted`, `pdf_unsupported`, `pdf_processing`, `pdf_image_required`, `resource_limit`, `validation`) all use the standard error envelope.

### Environment variables

| Variable | Default | Description |
| --- | --- | --- |
| `CODEX_MCP_PROTOCOL_VERSION` | — | MCP protocol version advertised to Codex. |
| `CODEXSHIM_MCP_COMPATIBILITY` | `lenient` | Set to `strict` to reject legacy `2025-06-18` initialize clients. |
| `CODEXSHIM_PROCESS_CALLS` | `16` | Per-instance concurrent process-call limit shared by `run_program` and `bash`; 1–32. |
| `CODEXSHIM_DETACHED_CALLS` | `16` | Per-instance live detached `bash` trees; 1–16. |
| `CODEXSHIM_OUTPUT_BYTES` | `32000` | Per-call output ceiling in bytes; 4096–262144. This cannot bypass the per-call token or shared burst limits. |
| `CODEXSHIM_BURST_TOKENS` | `8192` | Shared projected model-token budget for parallel and rapidly consecutive tool responses; 2048–8192 and may only be lowered. |
| `CODEXSHIM_GREP_MEMORY_BYTES` | `268435456` | Per-call hard limit for retained `grep` candidates; 8388608–1073741824. |
| `CODEXSHIM_GLOB_MEMORY_BYTES` | `33554432` | Per-call hard limit for retained `glob` matches; 8388608–1073741824. |
| `CODEXSHIM_PDF_TEXT_MEMORY_BYTES` | `67108864` | Per-call memory budget for `auto` and `text` PDF reads; 33554432–134217728. |
| `CODEXSHIM_PDF_IMAGE_MEMORY_BYTES` | `100663296` | Per-call memory budget for `image` PDF reads; 67108864–201326592. |
| `CODEXSHIM_BASH` | probed | Absolute path to a GNU bash. An override that fails validation is an error, never a fallback. |
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
