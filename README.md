# codexshim

`codexshim` is a local MCP server that gives Codex a small set of tools for working with a source repository. It communicates over stdio and uses its startup directory as the repository root.

It provides four tools:

- `read` reads source files with line numbers and supports UTF-8 and UTF-16 text.
- `grep` searches file contents with Rust regular expressions or literal strings.
- `glob` finds files while respecting repository ignore rules.
- `run_process` runs one program with a structured argument list.

## Requirements

- Linux, or Windows 11 build 22621 or newer on a local fixed NTFS drive.
- Rust 1.88 only when building from source. Prebuilt release archives do not require Rust.

## Symbolic links

Developer Mode is not required to run `codexshim`. `read` may open an existing file symbolic link only through the repository capability, so a target outside the repository remains inaccessible. Repository traversal does not follow directory symbolic links, junctions, or other reparse points.

The manual Windows 11 validation runner must be able to create file and directory symbolic-link fixtures. Configure that test account with either Developer Mode or `SeCreateSymbolicLinkPrivilege`; this is a test requirement, not a runtime requirement.

## Download a release

Install or update the latest prebuilt release without modifying `PATH` or Codex configuration:

```powershell
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; irm https://github.com/possible055/codexshim/releases/latest/download/install.ps1 | iex
```

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/possible055/codexshim/releases/latest/download/install.sh | sh
```

The installers verify the release archive checksum and install only the executable. The default location is `%LOCALAPPDATA%\codexshim\bin\codexshim.exe` on Windows and `${XDG_DATA_HOME:-$HOME/.local/share}/codexshim/bin/codexshim` on Linux. Pass `-InstallDir` to `install.ps1` or `--install-dir` to `install.sh` to choose another directory. Run the same command again to update the installation.

Download the archive for your platform and `SHA256SUMS` from [GitHub Releases](https://github.com/possible055/codexshim/releases):

- Windows: `codexshim-<version>-x86_64-pc-windows-msvc.zip`
- Linux: `codexshim-<version>-x86_64-unknown-linux-gnu.tar.gz`

Verify the archive against `SHA256SUMS`, extract it to a stable directory, and keep the included `codex.toml.example` next to the binary as a configuration reference. The archives also include this README, the license, and `AGENTS.codexshim.md`. Neither the manual archive nor the installer modifies `PATH` or Codex configuration.

On Windows, `Get-FileHash <archive> -Algorithm SHA256` prints the value to compare with `SHA256SUMS`. On Linux, run `sha256sum -c SHA256SUMS --ignore-missing` in the download directory.

## Build from source

From the project directory:

```console
cargo build --release --locked
cargo run --locked -- doctor
```

The release binary is written to `target/release/codexshim` on Linux or `target/release/codexshim.exe` on Windows.

Performance measurement is opt-in and separate from the test and release gates:

```console
cargo bench --locked --bench performance
```

The manual Performance benchmark workflow runs the same repository benchmark on the dedicated Windows 11 NTFS runner.

## Publish a release

Set the package version in `Cargo.toml`, commit the release state, then push a matching annotated tag such as `v0.1.0`. The release workflow rejects tags that do not exactly match the Cargo version. It runs complete Linux and Windows validation on GitHub-hosted runners, builds both platform archives, and verifies each release binary before publishing assets.

The Windows 11 workstation workflow remains available as an independent manual target-environment gate. GitHub-hosted Windows Server 2025 runs the same test paths without a CI-specific platform bypass, while the supported end-user target remains Windows 11 build 22621 or newer.

Successful tag builds publish both platform archives, their checksums, `SHA256SUMS`, and the installers to the matching GitHub Release. The installers do not modify user configuration.

## Configure Codex

Copy the appropriate example into `~/.codex/config.toml` or a trusted project's `.codex/config.toml`, then replace `command` with the extracted binary's absolute path:

- [Windows MCP configuration](config/codex.windows.toml.example)
- [Linux MCP configuration](config/codex.linux.toml.example)

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

Keep `shell_tool = true` while the external release gates remain incomplete. Change it to `false` only after the full Linux/Windows, Codex integration, performance, and 24-hour soak checklist passes.

On Windows, use a single-quoted TOML path such as `'C:\Users\me\AppData\Local\codexshim\bin\codexshim.exe'` to avoid escaping backslashes. Always configure the prebuilt release executable as the long-lived MCP server; do not use `cargo run` as the MCP command because nested Cargo calls may contend for build locks.

Stop the active MCP server before rebuilding or replacing its Windows executable. A running `.exe` cannot be overwritten; restart Codex after the release build completes.

Start Codex in the repository you want to work on. `codexshim` treats that working directory as the repository root for the lifetime of the server.

Tool paths are interpreted by the platform running the server. Use Windows paths or repository-relative paths with a Windows server, and `/mnt/...` paths only with a Linux server running inside WSL. Cargo-based acceptance tests may update `target` build artifacts even when tracked source and configuration files remain unchanged.

## Process execution

`run_process` accepts an executable and a separate list of literal arguments. It does not interpret shell syntax such as pipes, redirections, `&&`, wildcards, or variable expansion. It can modify files and access other system resources, so the configuration above requires approval before each call.
