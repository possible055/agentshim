# codexshim

[English](README.md) | 简体中文

`codexshim` 为 `codex` CLI 扩展了一组小而受能力域约束的源代码工具。它以本地 stdio 服务形式运行，并将启动目录作为仓库根目录。

## 为什么使用

- **受限的文件访问。** `read`、`grep`、`glob` 默认仅在仓库内操作，可选访问 Codex skill 和 plugin 目录。
- **无 shell 注入。** `run_process` 接收可执行文件和字面量参数列表——不支持管道、重定向、通配符或变量展开。
- **跨平台。** 仅原生支持 Windows 和 Linux。

## 工具

| 工具 | 说明 |
| --- | --- |
| `read` | 读取源文件并附带行号。支持 UTF-8 与 UTF-16。 |
| `grep` | 使用 Rust 正则或字面字符串搜索文件内容。 |
| `glob` | 查找文件，并遵循仓库忽略规则。 |
| `run_process` | 以结构化参数列表运行单个程序。 |

所有工具都会同时返回类型明确的 `structuredContent` 与受大小限制的文本视图。`read` 返回 `next_start_line`；`grep` 和 `glob` 返回 `next_offset`、结果总数、分页限制及跳过项目统计。工具错误统一为 `{ error: { code, message, retryable, details } }`。

## 安装

预编译二进制支持 Linux 和 Windows 11（build 22621+，本地固定 NTFS 驱动器）。

### 预编译发行版（推荐）

安装或更新到最新发行版，不会修改 `PATH` 或 Codex 配置。

**Windows (PowerShell):**

```powershell
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; irm https://github.com/possible055/codexshim/releases/latest/download/install.ps1 | iex
```

**Linux:**

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/possible055/codexshim/releases/latest/download/install.sh | sh
```

默认安装位置：

- Windows: `%LOCALAPPDATA%\codexshim\bin\codexshim.exe`
- Linux: `${XDG_DATA_HOME:-$HOME/.local/share}/codexshim/bin/codexshim`

使用 `-InstallDir`（PowerShell）或 `--install-dir`（sh）覆盖路径。再次运行同一命令即可更新。

### 从源码构建

需要 Rust 1.88。

```console
cargo build --release --locked
cargo run --locked -- doctor
```

二进制位于 `target/release/codexshim`（Linux）或 `target/release/codexshim.exe`（Windows）。

## 配置 Codex

将对应的示例复制到 `~/.codex/config.toml`（用户级）或项目的 `.codex/config.toml`，然后将 `command` 替换为 `codexshim` 二进制的绝对路径：

- [Windows 示例](config/codex.windows.toml.example)
- [Linux 示例](config/codex.linux.toml.example)

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
mcp_2026_07_28 = true
```

在 Windows 上，使用单引号 TOML 路径，例如 `'C:\Users\me\AppData\Local\codexshim\bin\codexshim.exe'`，以避免转义反斜杠。

在你要处理的仓库中启动 Codex。`codexshim` 在服务生命周期内将该工作目录视为仓库根目录。

## 选项

### `--read-scope`

控制 `read`、`grep`、`glob` 在仓库外可访问的路径。在 MCP 服务 `args` 中设置：

| 取值 | 行为 |
| --- | --- |
| `normal`（默认） | 仓库路径加上 Codex skill/plugin 目录（`$CODEX_HOME/{skills,plugins}`，或 `~/.codex` 与 `~/.agents` 等价路径）。`.codex` 下的凭据和历史记录保持不可访问。 |
| `unrestricted` | 服务用户可读的任意绝对路径。Windows 上：仅限本地固定 NTFS 卷——拒绝 UNC 共享、可移动卷、设备命名空间、命名管道和驱动器相对路径。 |

```toml
args = ["serve", "--read-scope", "unrestricted"]
```

相对路径和仓库内的绝对路径在两种模式下都使用仓库能力域。`run_process` 不受此选项影响。`codexshim doctor --read-scope <value>` 在诊断时接受同一标志。

### 环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `CODEX_MCP_PROTOCOL_VERSION` | — | 向 Codex 声明的 MCP 协议版本。 |
| `CODEXSHIM_MCP_COMPATIBILITY` | `lenient` | 设为 `strict` 以拒绝旧版 `2025-06-18` initialize 客户端。 |
| `CODEXSHIM_LOG_MODE` | `errors` | 取值 `off`、`errors`、`all` 之一。 |
| `CODEXSHIM_LOG_DIR` | 平台默认 | 用绝对路径覆盖日志目录。 |

## 诊断

日志以 UTC 日期 JSONL 文件写入：

- Windows: `%LOCALAPPDATA%\codexshim\logs`
- Linux: `${XDG_STATE_HOME:-$HOME/.local/state}/codexshim/logs`

保留策略：每个文件分片 64 MiB，每个 UTC 日 128 MiB，总量 512 MiB，保留 30 天。当日文件始终保留。可显式查看或清理：

```console
codexshim logs status
codexshim logs purge
```

记录包含标识符、阶段、结果、计时和错误类别——绝不包含 MCP 参数、grep 模式、进程参数或环境、stdin、文件内容或 stdout/stderr。

## 许可证

[MIT](LICENSE)
