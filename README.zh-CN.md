# codexshim

[English](README.md) | 简体中文

`codexshim` 为 `codex` CLI 扩展了一组小而受能力域约束的源代码工具。它以本地 stdio 服务形式运行，并将启动目录作为仓库根目录。

## 为什么使用

- **受限的文件访问。** `read`、`grep`、`glob` 默认仅在仓库内操作，可选访问 Codex skill 和 plugin 目录。
- **两种执行形态。** `run_program` 接收可执行文件与字面量参数列表，参数不经任何 shell 解析；需要组合命令时使用 `bash`，它接收 POSIX 命令行。
- **程序白名单。** 在运维方指定可启动的程序之前，`run_program` 一律拒绝；裸名称是便利的调用名称政策，绝对路径项目则钉住 canonical executable identity。
- **跨平台。** 原生支持 Windows、Linux 和 macOS（Intel 与 Apple Silicon）。

## 工具

| 工具 | 说明 |
| --- | --- |
| `read` | 读取源文件并附带行号。支持 UTF-8、带 BOM 的 UTF-16，以及通过参数明确指定的 WHATWG 编码标签。 |
| `grep` | 使用 Rust 正则或字面字符串搜索文件内容。 |
| `glob` | 查找文件，并遵循仓库忽略规则。 |
| `run_program` | 以字面量参数列表运行单个白名单内程序，不经 shell。 |
| `bash` | 运行 POSIX bash 命令行，返回合并后的 stdout 与 stderr。 |

工具成功时返回受大小限制的文本结果。`read`、`grep` 和 `glob` 的部分结果会在文本中提供续读信息；`run_program` 报告退出状态及分离的 stdout/stderr 字节统计，`bash` 则只有一段合并输出。输出预算会随内容调整：CJK 密集的文本会被压到更小的字节预算，因为客户端对它的 token 消耗约为英文的两倍。工具失败时还会返回统一的 `{ error: { code, message, retryable, details } }` 结构化错误。

## 安装

预编译二进制支持 Linux、macOS 和 Windows 11（build 22621+）。Windows 使用本地固定 NTFS 驱动器。

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

**macOS:**

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/possible055/codexshim/releases/latest/download/install.sh | sh
```

默认安装位置：

- Windows: `%LOCALAPPDATA%\codexshim\bin\codexshim.exe`
- Linux: `${XDG_DATA_HOME:-$HOME/.local/share}/codexshim/bin/codexshim`
- macOS: `${XDG_DATA_HOME:-$HOME/.local/share}/codexshim/bin/codexshim`

使用 `-InstallDir`（PowerShell）或 `--install-dir`（sh）覆盖路径。再次运行同一命令即可更新。

安装指定版本或预发布版本时，传入 `-Version`（PowerShell）或 `--version`（sh）：

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/possible055/codexshim/releases/download/v0.1.3-alpha.3/install.sh | sh -s -- --version 0.1.3-alpha.3
```

### 从源码构建

需要 Rust 1.88。

```console
cargo build --release --locked
cargo run --locked -- doctor
```

二进制位于 `target/release/codexshim`（Linux 与 macOS）或 `target/release/codexshim.exe`（Windows）。

## 配置 Codex

将对应的示例复制到 `~/.codex/config.toml`（用户级）或项目的 `.codex/config.toml`，然后将 `command` 替换为 `codexshim` 二进制的绝对路径：

- [Windows 示例](config/codex.windows.toml.example)
- [Linux 示例](config/codex.linux.toml.example)
- [macOS 示例](config/codex.macos.toml.example)

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

两级审批对应两个工具的差异：`run_program` 受白名单约束且参数是结构化 argv，因此可以按需审批；`bash` 接收任意命令行，一律提示。白名单与 `bash` 都不是安全 sandbox。

`tool_timeout_sec` 必须不小于服务端 600000 ms 上限并留出余量，否则客户端会先于服务端超时。

在 Windows 上，使用单引号 TOML 路径，例如 `'C:\Users\me\AppData\Local\codexshim\bin\codexshim.exe'`，以避免转义反斜杠。

在你要处理的仓库中启动 Codex。`codexshim` 在服务生命周期内将该工作目录视为仓库根目录。

`supports_parallel_tool_calls = true` 允许 Codex 并行调用 codexshim 的工具。每个 codexshim 实例独立允许最多 16 个活动中的前景进程调用（`run_program` 与前景 `bash` 共用此类），16 个活动中的只读调用（`read`、`grep`、`glob` 合计），以及 16 棵存活中的 detached 行程树。可将 `CODEXSHIM_PROCESS_CALLS` 设为 1 到 32 的整数以覆盖进程上限；无效值会阻止启动。类别满载时仍立即失败，并返回可重试的 `resource_busy` 错误。codexshim 无法判断两个并行调用在语义上是否冲突——不要对同一工作树并行发出变更性命令。

## 选项

### `--allow-programs`

以逗号分隔列出 `run_program` 可启动的程序。**默认为空，即默认拒绝**；不提供万用字元，因为「什么都能跑」正是 `bash` 的职责。

```toml
args = ["serve", "--allow-programs", "git,cargo,rustup,gh"]
```

清单项可以是裸程序名或绝对路径。裸名称会与解析后的 invocation path 或 canonical executable target 任一方的 file stem 比对，因此 Unix 上的 `cargo -> rustup` multicall proxy 可正常使用，`git` 也覆盖任何安装前缀下的 `git.exe`。这项便利政策也表示：经白名单别名调用的任意 target 会被允许，不能把裸名称当作 identity 保证。绝对路径项目只与 canonical executable 比对，用来钉住该 identity。每次调用都在 PATH 解析后重新检查，包含 resolution cache 命中。Windows 上的比对为 ASCII 大小写不敏感。空项目、以及含路径分隔符的相对项目都会阻止启动。

`CODEXSHIM_ALLOW_PROGRAMS` 是语法相同的次要来源；两者同时存在时以启动旗标为准。`codexshim doctor --allow-programs <value>` 接受同一旗标并打印解析后的清单。

被拒绝的调用返回不可重试的 `not_permitted` 错误，其中列出解析到的路径并指向 `bash`。

Windows 上，codexshim 会从探测到的 Git for Windows bash 推导其工具链目录，并前置到继承的 `PATH`，让 shell 找得到自身附带的 `sleep`、`grep`、`sed` 与 `locale`。这不会过滤继承项目，也不是可设置的 curated `PATH`。

### 长时间任务

`bash` 接受 `detach` 与仓库内的 `log_path`。命令的合并输出直接写入该文件，调用立即返回 pid 与 log 路径，而不是阻塞最长十分钟：

```json
{ "command": "cargo test > /dev/null; echo EXIT=$?", "detach": true, "log_path": "local/test.log" }
```

用 `read` 读取 `log_path` 观察进度——它的 `next_start_line` 续读信息即为游标。detached 行程树绑定的是 codexshim 实例而非本次调用：它在调用返回后继续存活，并在服务停止时被终止，因此不会有任何东西活过 Codex session。`CODEXSHIM_DETACHED_CALLS` 限制同时存活的数量（1–16，默认 16）；roster 在 blocking scheduling 前就保留名额，因此满载会立即返回可重试的 `resource_busy` 错误，并列出存活各笔的 pid 与 log 路径。`log_path` 先做词法 admission，再透过保留的 repository capability 开启，以阻挡 symlink／junction escape。

### `--read-scope`

控制 `read`、`grep`、`glob` 在仓库外可访问的路径。在 MCP 服务 `args` 中设置：

| 取值 | 行为 |
| --- | --- |
| `normal`（默认） | 仓库路径加上 Codex skill/plugin 目录（`$CODEX_HOME/{skills,plugins}`，或 `~/.codex` 与 `~/.agents` 等价路径）。`.codex` 下的凭据和历史记录保持不可访问。 |
| `unrestricted` | 服务用户可读的任意绝对路径。Windows 上：仅限本地固定 NTFS 卷——拒绝 UNC 共享、可移动卷、设备命名空间、命名管道和驱动器相对路径。 |

```toml
args = ["serve", "--read-scope", "unrestricted"]
```

相对路径和仓库内的绝对路径在两种模式下都使用仓库能力域。

`--read-scope` 是 `read`、`grep`、`glob` 的结构化访问范围，**不是**行程能碰到什么的边界：`run_program` 或 `bash` 启动的任何程序都继承服务用户的一般文件系统权限。白名单降低误用，审批模式提供政策闸门，但两者都不隔离子行程；需要隔离时应使用 OS sandbox。`codexshim doctor --read-scope <value>` 在诊断时接受同一标志。

### 环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `CODEX_MCP_PROTOCOL_VERSION` | — | 向 Codex 声明的 MCP 协议版本。 |
| `CODEXSHIM_MCP_COMPATIBILITY` | `lenient` | 设为 `strict` 以拒绝旧版 `2025-06-18` initialize 客户端。 |
| `CODEXSHIM_PROCESS_CALLS` | `16` | 每个实例的进程调用并行上限，由 `run_program` 与 `bash` 共用；接受 1 到 32 的整数。 |
| `CODEXSHIM_DETACHED_CALLS` | `16` | 每个实例存活中的 detached `bash` 行程树数量；接受 1 到 16 的整数。 |
| `CODEXSHIM_ALLOW_PROGRAMS` | 空 | 逗号分隔的 `run_program` 白名单；`--allow-programs` 旗标优先。 |
| `CODEXSHIM_OUTPUT_BYTES` | `32000` | 每次呼叫的输出上限（位元组）；接受 4096 到 262144 的整数。 |
| `CODEXSHIM_BASH` | 自动探测 | GNU bash 的绝对路径。显式覆写若验证失败即为错误，不会退回自动探测。 |
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
