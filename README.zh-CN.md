# AgentShim

[English](README.md) | 简体中文

AgentShim 为 coding agent 提供文件读取、搜索和命令执行工具。Codex 与 Cursor 连接本地 MCP 服务；DSH 可使用原生 adapter。

原生支持 Windows x64、Linux x64 和 Linux ARM64；macOS Apple Silicon 提供兼容性发行版。

## 快速开始

### 1. 安装

Windows PowerShell：

```powershell
irm https://github.com/possible055/agentshim/releases/latest/download/install.ps1 | iex
```

Linux 或 macOS Apple Silicon：

```sh
curl -fsSL https://github.com/possible055/agentshim/releases/latest/download/install.sh | sh
```

安装器会输出可执行文件的绝对路径。默认安装位置：

| 系统 | 可执行文件 |
| --- | --- |
| Windows | `%LOCALAPPDATA%\agentshim\bin\agentshim.exe` |
| Linux | `${XDG_DATA_HOME:-$HOME/.local/share}/agentshim/bin/agentshim` |
| macOS | `${XDG_DATA_HOME:-$HOME/.local/share}/agentshim/bin/agentshim` |

再次运行上述命令会安装最新稳定版；如果当前安装的是预发布版本，可能发生降级，请先确认当前版本。要安装指定版本（包括 alpha），将以下示例中的 `0.1.7-alpha.3` 替换为所需的已发布版本：

```powershell
$installer = Invoke-RestMethod https://github.com/possible055/agentshim/releases/latest/download/install.ps1
& ([scriptblock]::Create($installer)) -Version 0.1.7-alpha.3
```

```sh
curl -fsSL https://github.com/possible055/agentshim/releases/latest/download/install.sh | sh -s -- --version 0.1.7-alpha.3
```

这些命令安装指定版本，不会自动跟进更新的 alpha 版本。如果使用了自定义目录，请通过 `-InstallDir`（PowerShell）或 `--install-dir`（sh）指定原目录，确保 MCP 配置仍指向更新后的可执行文件。更新后请重启 MCP 客户端。

### 2. 配置 MCP 客户端

#### Codex

将适用于系统的配置添加到 `~/.codex/config.toml` 或项目的 `.codex/config.toml`，并将 `command` 替换为安装器输出的绝对路径。自定义安装目录或 `XDG_DATA_HOME` 会改变路径。

如果文件已有 `[features]` 段，请将设置添加到该段，不要重复创建；保留已有的 MCP 服务配置。

默认情况下，`read`、`grep` 和 `glob` 可访问服务用户能读取的任意路径。要限制在仓库及 Codex skill/plugin 路径内，请向 `args` 添加 `"--read-scope", "normal"`；这不限制 `run_program` 或 `bash` 启动的进程。

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

示例将 `default_tools_approval_mode` 设为 `"approve"`；使用前请根据自己的 Codex 审批策略检查此设置。

<details>
<summary>Cursor</summary>

将以下配置添加到 `~/.cursor/mcp.json`，并将 `command` 替换为安装器输出的绝对路径。Windows JSON 路径需转义反斜杠，也可使用正斜杠。

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

参见 [Cursor 示例](config/cursor.mcp.json.example)。

</details>

### 3. 检查连接

重启客户端，确认 AgentShim MCP 服务已连接并提供 `read`、`grep`、`glob`、`run_program`、`bash` 和 `bash_status`。如无法连接，请检查可执行文件路径和客户端配置，参见[诊断](#诊断)。

## 工具与访问范围

| 工具 | 用途 |
| --- | --- |
| `read` | 读取文本、PDF 和 Office 文件。 |
| `grep` | 使用 Rust 正则或字面字符串搜索文件内容。 |
| `glob` | 按模式查找文件。 |
| `run_program` | 不经 shell，以字面量参数运行可执行文件。 |
| `bash` | 在前台或作为 detached job 运行 Bash 命令。 |
| `bash_status` | 检查 detached job 并读取日志末尾。 |

MCP 服务以启动目录为仓库根目录。`grep` 和 `glob` 默认包含 gitignore 文件，但排除 `.git` 和常见大型目录。

## 其他使用方式

### DSH

DSH adapter 在进程内加载平台原生 addon，不使用 MCP 服务，也不需要 `agentshim` 可执行文件。将其安装到使用的 profile：

```sh
dsh plugin --profile web add dsh-agentshim
dsh web --dump-config
```

要求、配置和移除方法参见 [DSH adapter 指南](adapters/dsh/README.md)。

<details>
<summary>从源码构建</summary>

需要 Rust 1.99.1：

```sh
cargo build --release --locked
```

可执行文件位于 Linux、macOS 的 `target/release/agentshim` 或 Windows 的 `target/release/agentshim.exe`。在 MCP 配置中使用其绝对路径。

</details>

## 参考

### `--client-profile`

选择 aggregate burst 策略。下列各层是相互独立的限制，而非同一条上限：

| 层 | 数值 | 含义 |
| --- | ---: | --- |
| Codex 单项 truncation | 10,000 tokens 或 bytes | 在包上 `Wall time:` / `Output:` 之后的 history 上限 |
| 服务端内容上限 | 9,872 | 10,000 减去 128 wrapper tokens |
| 单次呼叫上限 | 8,192 | 两个 profile 均为此值；单页目前不能超过它 |
| Burst 合计 | profile 默认值 | 剩余预算在未完成呼叫之间均分 |

| 值 | 单次 token 上限 | 默认 burst token |
| --- | ---: | ---: |
| `codex`（默认） | 8,192 | 16,384 |
| `cursor` | 8,192 | 32,768 |

`AGENTSHIM_IDLE_TIMEOUT` 为 `codex` profile 启用空闲关闭。`cursor` profile 始终禁用看门狗，但设为非法值仍会导致启动失败。

### `--read-scope`

控制 `read`、`grep` 和 `glob` 在仓库外可访问的路径：

| 取值 | 行为 |
| --- | --- |
| `unrestricted`（默认） | 服务用户可读的任意绝对路径。 |
| `normal` | 仓库路径加上 Codex skill/plugin 目录。`.codex` 下的凭据和历史记录仍不可访问。 |

```toml
args = ["serve", "--read-scope", "normal"]
```

`--read-scope` 只约束 `read`、`grep` 和 `glob`。MCP 不提供进程 sandbox；`run_program` 或 `bash` 启动的程序会继承服务用户的完整文件系统权限。DSH 若组合了官方 `ctx.sandbox`／`ctx.sandboxPolicy`，则由该官方服务负责 exact argv 的 confinement。

版本化的跨 adapter contract 是 [`contracts/tool-contract-v1.json`](contracts/tool-contract-v1.json)。生成器还维护 [`contracts/generated/`](contracts/generated/) 中的 MCP JSON Schema 和 DSH 参数／输出投影；可用 `python3 scripts/generate-contracts.py --check` 验证。MCP 保留 `error.code` 的 legacy class；有 v1 对应的 class 才会在 `error.canonicalCode` 中规范化，尚未对应的 class 则保留原名。

### 长时间任务

`bash` 接受 `detach` 与仓库内的 `log_path`。输出写入该文件，调用立即返回仅限当前实例使用的 opaque `job_id`，以及诊断用 pid 与 log 路径：

```json
{ "command": "cargo test > /dev/null; echo EXIT=$?", "detach": true, "log_path": "local/test.log" }
```

使用 `bash_status` 取得即时状态、primary exit status 与 bounded log tail（`tail_bytes=0` 只返回 metadata）：

```json
{ "job_id": "bash-550e8400-e29b-41d4-a716-446655440000", "tail_bytes": 8192 }
```

通过 `bash` 本身终止 server 持有的完整进程树：

```json
{ "action": "terminate", "job_id": "bash-550e8400-e29b-41d4-a716-446655440000" }
```

同时最多可有 16 棵 active detached 进程树。日志超过 `AGENTSHIM_DETACHED_LOG_BYTES` 时会终止进程树并记录 `log_quota_exceeded`。`timeout_ms` 自进程树成功 spawn 起算；省略时使用 `AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX`，显式值只能缩短它。Deadline 到期会主动终止完整进程树并记录 `timed_out`。实例保留最近 32 笔 terminal record，每笔最多 16 KiB final tail；ID 不跨 reconnect 或 restart，也不提供 list API。完整 log 仍可用 `read(log_path)` 读取，terminal eviction 不会删除该文件。

### Windows Bash 参数转换

Git Bash 在启动 Windows 原生程序前，会转换看起来像 POSIX 路径的参数。当 `robocopy /E` 等斜杠式选项必须保持字面值时，将 `msys_argument_conversion` 设为 `disabled`：

```json
{ "command": "robocopy \"$source\" \"$destination\" /E", "msys_argument_conversion": "disabled" }
```

### 读取 PDF

`read` 依据 `%PDF-` 头识别 PDF，与扩展名无关。PDF 输入会拒绝 `encoding`、`start_line` 与 `line_count`。

| 参数 | 取值 | 含义 |
| --- | --- | --- |
| `pdf_mode` | `auto`（默认）、`text`、`image` | `auto`/`text` 返回页面 Markdown；`image` 渲染 PNG 内容块。 |
| `pages` | `"7"` 或 `"7-12"` | 单页或一段连续范围。 |
| `pdf_cursor` | 不透明 token | 原样回传上一轮响应给出的值。它同时携带来源版本，以及响应停在页内时的续读位置。 |

页数限制的是一次调用的工作量：

| 模式 | 未指定 `pages` | 明确范围上限 |
| --- | --- | --- |
| `auto`、`text` | 前 10 页 | 20 页 |
| `image` | 第 1 页 | 4 页 |

响应会说明交付到哪里以及如何继续。同时包含可读页与纯图片页的文档视为成功：可读页以 Markdown 返回，其余变成 placeholder。单一实例同时最多只跑一个结构化文档呼叫；第二个并行 PDF 或 Office 呼叫会返回可重试的 `resource_busy`。

### 读取 Office 文档

`read` 接受 DOCX、XLSX、PPTX、DOC、XLS 与 PPT 路径并返回 Markdown。扩展名只选择 Office 候选格式族，实际格式由 ZIP content type 或 CFB stream signature 确认。Office 输入会拒绝文字与 PDF 参数。部分响应会携带不透明的 `office_cursor`，后续请求应以同一路径原样回传。PDF 与 Office 共用一个结构化文档 slot，因此不会同时持有大额内存预算。

### 环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `CODEX_MCP_PROTOCOL_VERSION` | — | 向 Codex 声明的 MCP 协议版本。 |
| `AGENTSHIM_IO_WORKERS` | 可用 CPU 数，最多 `4` | Worker lane 数量；1–16。 |
| `AGENTSHIM_FOREGROUND_CALLS` | `16` | 每个实例共享的 `read`、`glob`、`grep`、`run_program` 及前台 `bash` 并行上限；1–32。 |
| `AGENTSHIM_DETACHED_CALLS` | `16` | 每个实例存活中的 detached `bash` 进程树数量；1–16。 |
| `AGENTSHIM_DETACHED_LOG_BYTES` | `67108864` | 单个 detached 日志的终止阈值，范围 1048576–4294967296 字节；超过后终止进程树，进行中的写入块可能使文件略大于阈值。 |
| `AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX` | `1800` | detached／background Bash 的最长运行时间（秒）；600–14400。省略的 job timeout 使用此值，显式值只能缩短它。 |
| `AGENTSHIM_OUTPUT_BYTES` | `32000` | 每次呼叫的输出上限（字节）；4096–262144。 |
| `AGENTSHIM_BURST_TOKENS` | profile 默认值 | 共用的预估模型 token 预算；2048–32768。 |
| `AGENTSHIM_TOOL_TIMEOUT_SHELF` | `600` | 服务端会保持低于此 shelf 值，以便客户端的 `tool_timeout_sec` 在服务端自身 Timeout 之后触发。有效最长执行时间为 shelf 减 10 秒；15–3600。 |
| `AGENTSHIM_IDLE_TIMEOUT` | 关闭 | 仅 `codex` profile 的空闲关闭秒数；1–86400。入站 JSON-RPC 消息重置 deadline，活跃的前景呼叫或 detached 进程树会推迟关闭。 |
| `AGENTSHIM_GREP_MEMORY_BYTES` | `268435456` | 每次 `grep` 呼叫保留候选项目的内存硬上限。 |
| `AGENTSHIM_GLOB_MEMORY_BYTES` | `67108864` | 每次 `glob` 呼叫保留匹配项目的内存硬上限。 |
| `AGENTSHIM_PDF_TEXT_MEMORY_BYTES` | `67108864` | `auto`/`text` 模式 PDF 读取的每次呼叫内存预算。 |
| `AGENTSHIM_PDF_IMAGE_MEMORY_BYTES` | `100663296` | `image` 模式 PDF 读取的每次呼叫内存预算。 |
| `AGENTSHIM_WINDOWS_ACTIVE_PROCESS_LIMIT` | 关闭 | 可选的 Windows Job Object 单棵前景或 detached 进程树 active-process 硬上限；1–256。 |
| `AGENTSHIM_WINDOWS_JOB_MEMORY_BYTES` | 关闭 | 可选的 Windows Job Object aggregate committed-memory 硬上限；67108864–17179869184。 |
| `AGENTSHIM_WINDOWS_PROCESS_MEMORY_BYTES` | 关闭 | 可选的 Windows Job Object 单进程 committed-memory 硬上限；67108864–17179869184。 |
| `AGENTSHIM_BASH` | 自动探测 | GNU bash（Git Bash 的 `bash.exe`）、BusyBox-w32 dispatcher（`busybox64u.exe`）或 applet 命名的 BusyBox 副本的绝对路径；shell flavor 与调用形式由探针自动判定。在 DSH adapter 中，plugin config `env` 段里设定的同名键也会在加载时驱动 bash 探测，因此无需在 host process 环境中预设。 |
| `AGENTSHIM_LOG_MODE` | `errors` | 取值 `off`、`errors`、`all` 之一。 |
| `AGENTSHIM_LOG_DIR` | 平台默认 | 用绝对路径覆盖日志目录。 |
| `AGENTSHIM_RESPECT_GITIGNORE` | `false` | 设为 `true` 时，`grep` 与 `glob` 才套用 `.gitignore`／`.ignore`。省略 `include_ignored` 时跟随此默认值。由于调用方读不到这项设定，过滤生效且结果为空时，响应末尾会附上一行建议改用 `include_ignored=true`。`.git` 以及 `node_modules`、`target`、`.venv`、`venv`、`dist`、`build`、`__pycache__` 无论开关都排除。binary、输出预算与内存上限仍会挡住内容。 |

共享的 256 MiB 运行时内存值是内部软预留目标，并非进程 RSS 限制。PDF 和搜索代码还执行各自的单次调用限制；子进程不计入该目标，Windows 可通过上述 Job Object 设置硬限制。

在 Windows 上使用 DSH adapter 时，上述三个 `AGENTSHIM_WINDOWS_*` Job Object 设定会从 plugin 的显式 `env` 配置读取为 host policy，同时也会传给 child process。native host 不会读取自身的 ambient process environment。

stdio 传输最多容纳 256 个未完成请求及 16 MiB 序列化请求数据；超过任一上限会关闭实例。stdout 连续 5 秒无法写入也会关闭实例。

空闲看门狗在确认静默后会再次复核活动时间戳，然后才取消既有的优雅关闭 token。在这次复核与取消之间的最后窗口内到达的请求，仍可能与关闭发生竞态；启用看门狗即接受这一狭窄的边界条件。

## 诊断

日志为按 UTC 日期命名的 JSONL 文件：

- Windows: `%LOCALAPPDATA%\agentshim\logs`
- Linux 和 macOS: `${XDG_STATE_HOME:-$HOME/.local/state}/agentshim/logs`

保留策略：总量 512 MiB，保留 30 天。可查看或清理：

```console
agentshim logs status
agentshim logs purge
```

记录包含标识符、阶段、结果、计时与错误类别——绝不包含 MCP 参数、grep 模式、进程参数、stdin、文件内容或 stdout/stderr。复现工具加载失败时，将 `AGENTSHIM_LOG_MODE` 设为 `all`。

## 致谢

- [PDFOxide](https://github.com/yfedoseev/pdf_oxide) — PDF 读取后端
- [OfficeOxide](https://github.com/yfedoseev/office_oxide) — Office 选择性衍生读取器的来源
- [Gigatoken](https://github.com/marcelroed/gigatoken) — token 计数后端
- [FastCtx](https://github.com/yc-duan/fastctx) — `read`、`grep` 与 `glob` 的设计与基准参考
- [Linux Do](https://linux.do/) — 啟發本項目最初構想的論壇社群

## 许可证

AgentShim 自有代码采用 [MIT](LICENSE) 许可证；第三方组件保留各自的许可证和声明。
