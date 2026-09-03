# AgentShim

[English](README.md) | 简体中文

AgentShim 为 coding agent 提供一组精简而专注的源代码工具。Codex 与 Cursor 连接本地 stdio MCP 服务，DSH 则使用本仓库提供的原生 adapter。服务将启动目录作为仓库根目录，优先支持 Windows x86-64，并为 Linux 与 macOS 提供兼容性发行资产。

## 为什么使用

- **受限的文件访问。** `read`、`grep` 和 `glob` 默认仅在仓库内操作，可选访问 Codex skill 和 plugin 目录。
- **可管理的长时间 Bash。** `run_program` 接收单一可执行文件和字面量参数。`bash` 处理 POSIX 命令组合，也可用 instance-bound `job_id` detach；`bash_status` 回报生命周期、primary exit status 与 bounded log tail，`bash` 还能终止完整的 server-owned tree。
- **跨平台。** 完全支持 Windows x86-64，并为 Linux x86-64、Linux ARM64 与 macOS Apple Silicon 提供兼容性发行资产。
- **可读结构化文档。** `read` 可返回 PDF 页面文字或渲染图片，也可将 DOCX、XLSX、PPTX、DOC、XLS 与 PPT 转为 Markdown；长文档带续读游标。

## 工具

| 工具 | 说明 |
| --- | --- |
| `read` | 读取源文件并附带行号。支持 UTF-8、带 BOM 的 UTF-16、WHATWG 编码标签、PDF 与六种 Office 格式。 |
| `grep` | 使用 Rust 正则或字面字符串搜索文件内容。 |
| `glob` | 查找文件。默认包含被 gitignore 的文件；`.git` 与常见超大目录仍排除。 |
| `run_program` | 以字面量参数列表运行单个程序，不经 shell。 |
| `bash` | 运行 POSIX bash 命令行，返回合并后的 stdout 与 stderr。 |
| `bash_status` | 检查一笔 detached Bash job 与 bounded log tail。 |

## 安装

**Windows (PowerShell):**

```powershell
irm https://github.com/possible055/agentshim/releases/latest/download/install.ps1 | iex
```

默认安装至 `%LOCALAPPDATA%\agentshim\bin\agentshim.exe`（例如 `C:\Users\<user>\AppData\Local\agentshim\bin\agentshim.exe`）。

**Linux / macOS:**

```sh
curl -fsSL https://github.com/possible055/agentshim/releases/latest/download/install.sh | sh
```

默认安装至 `${XDG_DATA_HOME:-$HOME/.local/share}/agentshim/bin/agentshim`（例如 `~/.local/share/agentshim/bin/agentshim`）。

再次运行同一命令即可更新。安装指定版本时，传入 `-Version`（PowerShell）或 `--version`（sh）。

**从源码构建**（需要 Rust 1.88）：

```console
cargo build --release --locked
```

二进制位于 `target/release/agentshim`（Linux 与 macOS）或 `target/release/agentshim.exe`（Windows）。

现有 `codexshim` 安装不会被删除或覆盖。安装 AgentShim 后，请先将各客户端切换到新的可执行文件与 MCP server 名称，确认六个工具可用，再视需要移除旧安装。

## 配置 Codex

将对应的示例复制到 `~/.codex/config.toml`（用户级）或项目的 `.codex/config.toml`，然后将 `command` 替换为 `agentshim` 二进制的绝对路径：

- [Windows 示例](config/codex.windows.toml.example)
- [Linux 示例](config/codex.linux.toml.example)
- [macOS 示例](config/codex.macos.toml.example)

```toml
[mcp_servers.agentshim]
required = true
command = "/absolute/path/to/agentshim"
args = ["serve", "--client-profile", "codex"]
# 无限制模式（默认）允许任意绝对路径的 read/grep/glob。若要
# 限制在仓库与 Codex skill/plugin 路径内，请使用：
# args = ["serve", "--client-profile", "codex", "--read-scope", "normal"]
supports_parallel_tool_calls = true
tool_timeout_sec = 600
enabled_tools = ["read", "grep", "glob", "run_program", "bash", "bash_status"]
default_tools_approval_mode = "approve"
env = { CODEX_MCP_PROTOCOL_VERSION = "2026-07-28" }

[features]
mcp_2026_07_28 = true
```

## 配置 Cursor

将 [Cursor 示例](config/cursor.mcp.json.example)复制到 `~/.cursor/mcp.json`，把 `command` 替换为二进制文件的绝对路径，然后重启 Cursor：

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

在 Windows 上，JSON 路径必须转义每个反斜杠。

## 配置 DSH

将原生 adapter 及其精确可选平台包安装到目标 DSH profile（如 Web UI 使用 `web`，CLI 任务使用 `headless`）：

```sh
dsh plugin --profile web add dsh-agentshim
dsh web --dump-config
```

DSH 通过平台 addon 在进程内加载 `agentshim-core`；它不会启动 MCP 服务，也不需要已安装的 `agentshim` 可执行文件。不支持的平台、缺失的包或原生 API 不匹配都会导致 plugin activation 失败。配置、capture 保留策略、sandbox 审批行为与移除流程请参阅 [DSH adapter 指南](adapters/dsh/README.md)。

## 选项

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

`--read-scope` 只约束 `read`、`grep` 和 `glob`。`run_program` 或 `bash` 启动的程序会继承服务用户的完整文件系统权限——需要真正隔离时请使用 OS sandbox。

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

同时最多可有 16 棵 active detached 进程树。`timeout_ms` 自进程树成功 spawn 起算；省略时使用 `AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX`，显式值只能缩短它。Deadline 到期会主动终止完整进程树并记录 `timed_out`。实例保留最近 32 笔 terminal record，每笔最多 16 KiB final tail；ID 不跨 reconnect 或 restart，也不提供 list API。完整 log 仍可用 `read(log_path)` 读取，terminal eviction 不会删除该文件。

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
| `AGENTSHIM_FOREGROUND_CALLS` | `16` | 每个实例共享的 `read`、`glob`、`grep`、`run_program` 及前台 `bash` 并行上限；1–32。 |
| `AGENTSHIM_DETACHED_CALLS` | `16` | 每个实例存活中的 detached `bash` 进程树数量；1–16。 |
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
| `AGENTSHIM_BASH` | 自动探测 | GNU bash 的绝对路径。在 DSH adapter 中，plugin config `env` 段里设定的同名键也会在加载时驱动 bash 探测，因此无需在 host process 环境中预设。 |
| `AGENTSHIM_LOG_MODE` | `errors` | 取值 `off`、`errors`、`all` 之一。 |
| `AGENTSHIM_LOG_DIR` | 平台默认 | 用绝对路径覆盖日志目录。 |
| `AGENTSHIM_RESPECT_GITIGNORE` | `false` | 设为 `true` 时，`grep` 与 `glob` 才套用 `.gitignore`／`.ignore`。省略 `include_ignored` 时跟随此默认值。由于调用方读不到这项设定，过滤生效且结果为空时，响应末尾会附上一行建议改用 `include_ignored=true`。`.git` 以及 `node_modules`、`target`、`.venv`、`venv`、`dist`、`build`、`__pycache__` 无论开关都排除。binary、输出预算与内存上限仍会挡住内容。 |

在 Windows 上使用 DSH adapter 时，上述三个 `AGENTSHIM_WINDOWS_*` Job Object 设定会从 plugin 的显式 `env` 配置读取为 host policy，同时也会传给 child process。native host 不会读取自身的 ambient process environment。

空闲看门狗在确认静默后会再次复核活动时间戳，然后才取消既有的优雅关闭 token。在这次复核与取消之间的最后窗口内到达的请求，仍可能与关闭发生竞态；启用看门狗即接受这一狭窄的边界条件。

## 诊断

日志为按 UTC 日期命名的 JSONL 文件：

- Windows: `%LOCALAPPDATA%\agentshim\logs`
- Linux: `${XDG_STATE_HOME:-$HOME/.local/state}/agentshim/logs`

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

[MIT](LICENSE)
