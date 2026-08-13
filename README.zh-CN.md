# codexshim

[English](README.md) | 简体中文

`codexshim` 为 `codex` CLI 提供一组小而聚焦的源代码工具。它以本地 stdio MCP 服务形式运行，并将启动目录作为仓库根目录。

## 为什么使用

- **受限的文件访问。** `read`、`grep`、`glob` 默认仅在仓库内操作，可选访问 Codex skill 和 plugin 目录。
- **两种命令执行方式。** `run_program` 接收一个可执行文件和字面量参数列表，参数不经任何 shell 解析；需要管道、重定向或命令组合时使用 `bash`，它接收 POSIX 命令行。
- **跨平台。** 完全支持 Windows x86-64，并为 Linux x86-64、Linux ARM64 与 macOS Apple Silicon 提供兼容支持的发行资产。
- **可读 PDF。** `read` 依内容识别 PDF（不靠扩展名），返回页面文字或渲染图片，长文档带续读游标。

## 工具

| 工具 | 说明 |
| --- | --- |
| `read` | 读取源文件并附带行号。支持 UTF-8、带 BOM 的 UTF-16，以及通过参数明确指定的 WHATWG 编码标签。也可读取 PDF（见下文）。 |
| `grep` | 使用 Rust 正则或字面字符串搜索文件内容。 |
| `glob` | 查找文件，并遵循仓库忽略规则。 |
| `run_program` | 以字面量参数列表运行单个程序，不经 shell。 |
| `bash` | 运行 POSIX bash 命令行，返回合并后的 stdout 与 stderr。 |

调用成功时返回受大小限制的文本。`read`、`grep`、`glob` 的部分结果会带续读游标，便于接着上次的位置继续。输出预算会随内容调整：CJK 密集文本会被压到更小的字节预算，因为客户端对它的 token 消耗约为英文的两倍。每个实例还以 8192 token 的 burst gate 约束平行及快速连续响应的模型可见成本总和。此保护是服务端的最佳努力：它无法统计 Codex 原生工具、其他 MCP server、模型 reasoning 或工具参数，2 秒安静期也只是 burst heuristic，并非 Codex turn ID。失败时返回统一的 `{ error: { code, message, retryable, details } }` 错误信封。

## 安装

Windows 11（build 22621+）上的 Windows x86-64 属于完全支持平台，每个拉取请求和每次推送到 `main` 都会执行完整的原生验证。Linux x86-64、Linux ARM64 与 macOS Apple Silicon 属于兼容支持的发行目标：每次发行都会在原生 runner 上完成构建、封装、哈希校验及两次安装，但不执行完整的拉取请求测试。Windows ARM64、macOS Intel 与其他 Rust 目标属于可能支持的源码构建平台，不提供正式资产、CI 保证、服务级别或支持承诺。

### 预编译发行版（推荐）

安装或更新到最新发行版，不会修改 `PATH` 或 Codex 配置。

**Windows (PowerShell):**

```powershell
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; irm https://github.com/possible055/codexshim/releases/latest/download/install.ps1 | iex
```

**Linux / macOS:**

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/possible055/codexshim/releases/latest/download/install.sh | sh
```

默认安装位置：

- Windows: `%LOCALAPPDATA%\codexshim\bin\codexshim.exe`
- Linux / macOS: `${XDG_DATA_HOME:-$HOME/.local/share}/codexshim/bin/codexshim`

使用 `-InstallDir`（PowerShell）或 `--install-dir`（sh）覆盖路径。再次运行同一命令即可更新。

安装指定版本或预发布版本时，传入 `-Version`（PowerShell）或 `--version`（sh）：

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/possible055/codexshim/releases/download/v0.1.3-alpha.2/install.sh | sh -s -- --version 0.1.3-alpha.2
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

两级审批对应两个工具的差异：`run_program` 启动单个可执行文件并接收结构化 argv，因此可以按需审批；`bash` 接收任意命令行，一律提示。两个工具都不是安全 sandbox。

`tool_timeout_sec` 必须不小于服务端 590 秒上限并留出余量，否则客户端会先于服务端超时。示例使用 600。

在 Windows 上，使用单引号 TOML 路径，例如 `'C:\Users\me\AppData\Local\codexshim\bin\codexshim.exe'`，以避免转义反斜杠。

在你要处理的仓库中启动 Codex。`codexshim` 在服务生命周期内将该工作目录视为仓库根目录。

`supports_parallel_tool_calls = true` 允许 Codex 并行调用 codexshim 的工具。每个实例独立允许最多 16 个活动进程调用（`run_program` 与前景 `bash`）、16 个活动只读调用（`read`、`grep`、`glob` 合计），以及 16 棵存活中的 detached 进程树。某一类满载时，调用立即失败并返回可重试的 `resource_busy` 错误。codexshim 无法判断两个并行调用在语义上是否冲突——不要对同一工作树并行发出变更性命令。

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

相对路径和仓库内的绝对路径在两种模式下都使用仓库能力域。

`--read-scope` 只约束 `read`、`grep`、`glob` 能打开什么，**不是**进程能碰到什么的边界：`run_program` 或 `bash` 启动的任何程序都继承服务用户的一般文件系统权限。需要真正隔离时请使用 OS sandbox。

### 长时间任务

`bash` 接受 `detach` 与仓库内的 `log_path`。命令的合并输出直接写入该文件，调用立即返回 pid 与 log 路径，而不是阻塞：

```json
{ "command": "cargo test > /dev/null; echo EXIT=$?", "detach": true, "log_path": "local/test.log" }
```

用 `read` 读取 `log_path` 观察进度——它的 `next_start_line` 续读信息即为游标。detached 进程树由 codexshim 实例追踪直到退出。`CODEXSHIM_DETACHED_CALLS` 限制同时存活的数量（1–16，默认 16）；满载会立即返回可重试的 `resource_busy`，并列出存活各笔的 pid 与 log 路径。

### Windows Bash 参数转换

Git Bash 在启动 Windows 原生程序前，会自动转换看起来像 POSIX 路径的参数。运行单个原生程序时应优先使用 `run_program`，因为它会保持 argv 的字面值。必须使用 Bash 组合命令、同时需要原样传递 `robocopy /E` 等斜杠式选项时，将 `msys_argument_conversion` 设为 `disabled`：

```json
{ "command": "robocopy \"$source\" \"$destination\" /E && printf 'copied\\n'", "msys_argument_conversion": "disabled" }
```

此模式会为整段 Bash 命令设置 `MSYS2_ARG_CONV_EXCL=*`。该字段在 macOS 和 Linux 上被接受但无效。

### 读取 PDF

`read` 依据第 0 字节的 `%PDF-` 头识别 PDF，因此路由不依赖扩展名。PDF 输入会拒绝 `encoding`、`start_line` 与 `line_count`。

| 参数 | 取值 | 含义 |
| --- | --- | --- |
| `pdf_mode` | `auto`（默认）、`text`、`image` | `auto` 与 `text` 返回页面 Markdown；`image` 渲染 PNG 内容块。 |
| `pages` | `"7"` 或 `"7-12"` | 单页或一段连续范围，不接受离散页码列表。超出末页的范围会被夹到末页；起点在末页之后则报错。 |
| `pdf_text_offset` | 大于等于 0 的整数 | 以 UTF-8 字节偏移续读同一页的 Markdown。 |
| `pdf_source_id` | 不透明 token | 回带上一轮响应的值，用于证明续读指向同一个来源版本。 |

`auto` 只做文字提取、不渲染，也绝不会在同一次调用中返回图片。`text` 即使文字品质可疑也仍然返回非空文字。`image` 是明确的渲染请求，不提供 OCR。

页数限制的是一次调用的工作量，不是回答的大小：

| 模式 | 未指定 `pages` | 明确范围硬上限 |
| --- | --- | --- |
| `auto`、`text` | 前 10 页 | 20 页 |
| `image` | 第 1 页 | 4 页 |

无论先撞到哪个上限，响应都会说明交付到哪里以及如何继续。下一段范围以本次「实际交付」的页数为准：20 页的请求若只放得下 6 页，续读建议是 `7-12`，不是 `7-26`。

每个交付的页面都会在 `Pages:` 行报告一个状态：

| 状态 | 含义 |
| --- | --- |
| `text_ready` | 有足够且品质可接受的文字。 |
| `text_uncertain` | 有文字，但品质或覆盖可疑。文字**仍会**返回，不会被丢弃。 |
| `image_required` | 没有可用文字，但页面有可见绘制内容——包括完全没有内嵌图片的纯向量页。响应会附一行 `Retry:` 指明能渲染该页的 `pdf_mode="image"` 调用。 |
| `blank` | 既没有绘制内容也没有文字。 |
| `unavailable` | 该页无法处理。刻意与 `blank` 区分：后者会让你以为已经读完。 |

同时含可读页与纯图片页的文件算成功而非失败：可读页以 Markdown 返回，其余变成 placeholder。单一 `unavailable` 页不会让整次呼叫失败。只有在选定范围内**每一页**都没有可用文字时才返回 `pdf_image_required`。

PDF 工作成本高，因此单一实例同时最多只跑一个 PDF 呼叫。第二个并行呼叫会短暂等待，之后返回可重试的 `resource_busy` 并附 `retry_after_ms` 建议。一般文字读取不会排在 PDF 后面，因此慢速 PDF 不会拖慢它们。

每个模式另有 wall-clock 上限：`auto` 与 `text` 为 5 秒，`image` 为 10 秒。超过即返回可重试的 `resource_timeout`。PDF 专属失败（`pdf_invalid`、`pdf_encrypted`、`pdf_unsupported`、`pdf_processing`、`pdf_image_required`、`resource_limit`、`validation`）全部使用统一错误信封。

### 环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `CODEX_MCP_PROTOCOL_VERSION` | — | 向 Codex 声明的 MCP 协议版本。 |
| `CODEXSHIM_MCP_COMPATIBILITY` | `lenient` | 设为 `strict` 以拒绝旧版 `2025-06-18` initialize 客户端。 |
| `CODEXSHIM_PROCESS_CALLS` | `16` | 每个实例的进程调用并行上限，由 `run_program` 与 `bash` 共用；1–32。 |
| `CODEXSHIM_DETACHED_CALLS` | `16` | 每个实例存活中的 detached `bash` 进程树数量；1–16。 |
| `CODEXSHIM_OUTPUT_BYTES` | `32000` | 每次呼叫的输出上限（字节）；4096–262144。不能绕过单次 token 或共用 burst 限制。 |
| `CODEXSHIM_BURST_TOKENS` | `8192` | 平行与快速连续工具响应共用的模型可见 token 预算；2048–8192，只能调低。 |
| `CODEXSHIM_GREP_MEMORY_BYTES` | `268435456` | 每次 `grep` 呼叫保留候选项目的内存硬上限；8388608–1073741824。 |
| `CODEXSHIM_GLOB_MEMORY_BYTES` | `33554432` | 每次 `glob` 呼叫保留匹配项目的内存硬上限；8388608–1073741824。 |
| `CODEXSHIM_PDF_TEXT_MEMORY_BYTES` | `67108864` | `auto` 与 `text` 模式 PDF 读取的每次呼叫内存预算；33554432–134217728。 |
| `CODEXSHIM_PDF_IMAGE_MEMORY_BYTES` | `100663296` | `image` 模式 PDF 读取的每次呼叫内存预算；67108864–201326592。 |
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

记录包含标识符、阶段、结果、计时与错误类别——绝不包含 MCP 参数、grep 模式、进程参数或环境、stdin、文件内容或 stdout/stderr。

## 许可证

[MIT](LICENSE)
