# codexshim

`codexshim` is a local MCP server that gives Codex a small set of tools for working with a source repository. It communicates over stdio and uses its startup directory as the repository root.

It provides four tools:

- `read` reads source files with line numbers and supports UTF-8 and UTF-16 text.
- `grep` searches file contents with Rust regular expressions or literal strings.
- `glob` finds files while respecting repository ignore rules.
- `run_process` runs one program with a structured argument list.

## Requirements

- Rust 1.88. The repository pins the exact toolchain in `rust-toolchain.toml`.
- Linux, or Windows 11 build 22621 or newer on a workstation edition and a local fixed NTFS drive.

## Build

From the project directory:

```console
cargo build --release --locked
cargo run -- doctor
```

The release binary is written to `target/release/codexshim` on Linux or `target/release/codexshim.exe` on Windows.

## Configure Codex

Add the server to `~/.codex/config.toml` or a trusted project's `.codex/config.toml`, then replace `command` with the absolute path to the release binary:

```toml
[mcp_servers.codexshim]
command = "/absolute/path/to/codexshim"
args = ["serve"]
required = true
startup_timeout_sec = 15
tool_timeout_sec = 300
enabled_tools = ["read", "grep", "glob", "run_process"]
default_tools_approval_mode = "writes"
env = { CODEX_MCP_PROTOCOL_VERSION = "2026-07-28" }

[mcp_servers.codexshim.tools.run_process]
approval_mode = "prompt"

[features]
shell_tool = true
mcp_2026_07_28 = true
```

Keep `shell_tool = true` while the external release gates remain incomplete. Change it to `false` only after the full Linux/Windows, Codex integration, performance, and 24-hour soak checklist passes.

On Windows, use a single-quoted TOML path such as `'C:\Users\me\AppData\Local\codexshim\codexshim.exe'` to avoid escaping backslashes.

Start Codex in the repository you want to work on. `codexshim` treats that working directory as the repository root for the lifetime of the server.

## Process execution

`run_process` accepts an executable and a separate list of literal arguments. It does not interpret shell syntax such as pipes, redirections, `&&`, wildcards, or variable expansion. It can modify files and access other system resources, so the configuration above requires approval before each call.
