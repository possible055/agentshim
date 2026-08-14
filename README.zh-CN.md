# codexshim

[English](README.md) | 简体中文

`codexshim` 为 Codex 与 Cursor 提供一组小而聚焦的源代码工具，优先支持 Windows x86-64，并为 Linux 与 macOS 提供兼容发行版。它以本地 stdio MCP 服务形式运行，并将启动目录作为仓库根目录。

## 为什么使用

- **受限的文件访问。** `read`、`grep` 和 `glob` 默认仅在仓库内操作，可选访问 Codex skill 和 plugin 目录。
- **两种命令执行方式。** `run_program` 接收一个可执行文件和字面量参数列表——参数不经任何 shell 解析。需要管道、重定向或命令组合时使用 `bash`，它接收 POSIX 命令行，用以替代 Cursor 内置 Shell 在 Windows 上不可靠的 pwsh7。
- **跨平台。** 完全支持 Windows x86-64，并为 Linux x86-64、Linux ARM64 与 macOS Apple Silicon 提供兼容支持的发行资产。
- **可读 PDF。** `read` 依内容识别 PDF（不靠扩展名），返回页面文字或渲染图片，长文档带续读游标。

## 工具

| 工具 | 说明 |
| --- | --- |
| `read` | 读取源文件并附带行号。支持 UTF-8、带 BOM 的 UTF-16，以及 WHATWG 编码标签。也可读取 PDF。 |
| `grep` | 使用 Rust 正则或字面字符串搜索文件内容。 |
| `glob` | 查找文件。默认包含被 gitignore 的文件；`.git` 与常见超大目录仍排除。 |
| `run_program` | 以字面量参数列表运行单个程序，不经 shell。 |
| `bash` | 运行 POSIX bash 命令行，返回合并后的 stdout 与 stderr。 |

## 安装

**Windows (PowerShell):**

```powershell
irm https://github.com/possible055/codexshim/releases/latest/download/install.ps1 | iex
```

**Linux / macOS:**

```sh
curl -fsSL https://github.com/possible055/codexshim/releases/latest/download/install.sh | sh
```

再次运行同一命令即可更新。安装指定版本时，传入 `-Version`（PowerShell）或 `--version`（sh）。

**从源码构建**（需要 Rust 1.88）：

```console
cargo build --release --locked
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
args = ["serve", "--client-profile", "codex"]
required = true
supports_parallel_tool_calls = true
startup_timeout_sec = 15
tool_timeout_sec = 600
enabled_tools = ["read", "grep", "glob", "run_program", "bash"]
default_tools_approval_mode = "writes"

[mcp_servers.codexshim.tools.run_program]
approval_mode = "on-request"

[mcp_servers.codexshim.tools.bash]
approval_mode = "prompt"
```

## 配置 Cursor

将 [Cursor 示例](config/cursor.mcp.json.example)复制到 `~/.cursor/mcp.json`，把 `command` 替换为二进制文件的绝对路径，然后重启 Cursor：

```json
{
  "mcpServers": {
    "codexshim": {
      "type": "stdio",
      "command": "/absolute/path/to/codexshim",
      "args": ["serve", "--client-profile", "cursor"]
    }
  }
}
```

在 Windows 上，JSON 路径必须转义每个反斜杠。

## 选项

### `--client-profile`

选择 aggregate burst 策略。这些层不是同一条限制：

| 层 | 数值 | 含义 |
| --- | ---: | --- |
| Codex 单项 truncation | 10,000 tokens 或 bytes | 包上 `Wall time:` / `Output:` 后的 history 上限 |
| 服务端内容上限 | 9,872 | 10,000 减去 128 wrapper tokens |
| 单次呼叫上限 | 8,192 | 两个 profile 都是；单页目前不能超过它 |
| Burst 合计 | profile 默认值 | 剩余预算在未完成呼叫之间均分 |

| 值 | 单次 token 上限 | 默认 burst token |
| --- | ---: | ---: |
| `codex`（默认） | 8,192 | 16,384 |
| `cursor` | 8,192 | 32,768 |

### `--read-scope`

控制 `read`、`grep` 和 `glob` 在仓库外可访问的路径：

| 取值 | 行为 |
| --- | --- |
| `unrestricted`（默认） | 服务用户可读的任意绝对路径。 |
| `normal` | 仓库路径加上 Codex skill/plugin 目录。`.codex` 下的凭据和历史记录保持不可访问。 |

```toml
args = ["serve", "--read-scope", "normal"]
```

`--read-scope` 只约束 `read`、`grep` 和 `glob`。`run_program` 或 `bash` 启动的程序会继承服务用户的完整文件系统权限——需要真正隔离时请使用 OS sandbox。

### 长时间任务

`bash` 接受 `detach` 与仓库内的 `log_path`。输出写入该文件，调用立即返回 pid 与 log 路径：

```json
{ "command": "cargo test > /dev/null; echo EXIT=$?", "detach": true, "log_path": "local/test.log" }
```

用 `read` 读取 `log_path` 观察进度。同时最多可有 16 棵存活中的 detached 进程树。

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
| `pdf_cursor` | 不透明 token | 原样回带上一轮响应给出的值。它同时携带来源版本，以及响应停在页内时的续读位置。 |

页数限制的是一次调用的工作量：

| 模式 | 未指定 `pages` | 明确范围上限 |
| --- | --- | --- |
| `auto`、`text` | 前 10 页 | 20 页 |
| `image` | 第 1 页 | 4 页 |

响应会说明交付到哪里以及如何继续。同时含可读页与纯图片页的文件算成功：可读页以 Markdown 返回，其余变成 placeholder。单一实例同时最多只跑一个 PDF 呼叫；第二个并行呼叫会返回可重试的 `resource_busy`。

### 环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `CODEX_MCP_PROTOCOL_VERSION` | — | 向 Codex 声明的 MCP 协议版本。 |
| `CODEXSHIM_PROCESS_CALLS` | `16` | 每个实例的进程调用并行上限；1–32。 |
| `CODEXSHIM_DETACHED_CALLS` | `16` | 每个实例存活中的 detached `bash` 进程树数量；1–16。 |
| `CODEXSHIM_OUTPUT_BYTES` | `32000` | 每次呼叫的输出上限（字节）；4096–262144。 |
| `CODEXSHIM_BURST_TOKENS` | profile 默认值 | 共用的预估模型 token 预算；2048–32768。 |
| `CODEXSHIM_TOOL_TIMEOUT_SHELF` | `600` | 服务端会低于此 shelf，以便客户端的 `tool_timeout_sec` 在服务端自身 Timeout 之后触发。有效最长执行时间为 shelf 减 10 秒；15–3600。 |
| `CODEXSHIM_GREP_MEMORY_BYTES` | `268435456` | 每次 `grep` 呼叫保留候选项目的内存硬上限。 |
| `CODEXSHIM_GLOB_MEMORY_BYTES` | `33554432` | 每次 `glob` 呼叫保留匹配项目的内存硬上限。 |
| `CODEXSHIM_PDF_TEXT_MEMORY_BYTES` | `67108864` | `auto`/`text` 模式 PDF 读取的每次呼叫内存预算。 |
| `CODEXSHIM_PDF_IMAGE_MEMORY_BYTES` | `100663296` | `image` 模式 PDF 读取的每次呼叫内存预算。 |
| `CODEXSHIM_BASH` | 自动探测 | GNU bash 的绝对路径。 |
| `CODEXSHIM_LOG_MODE` | `errors` | 取值 `off`、`errors`、`all` 之一。 |
| `CODEXSHIM_LOG_DIR` | 平台默认 | 用绝对路径覆盖日志目录。 |
| `CODEXSHIM_RESPECT_GITIGNORE` | `false` | 设为 `true` 时，`grep` 与 `glob` 才套用 `.gitignore`／`.ignore`。省略 `include_ignored` 时跟随此默认值；由于调用方读不到这项设定，过滤生效且结果为空时，响应末尾会附上一行建议改用 `include_ignored=true`。`.git` 以及 `node_modules`、`target`、`.venv`、`venv`、`dist`、`build`、`__pycache__` 无论开关都排除。binary、输出预算与内存上限仍会挡住内容。 |

## 诊断

日志为按 UTC 日期命名的 JSONL 文件：

- Windows: `%LOCALAPPDATA%\codexshim\logs`
- Linux: `${XDG_STATE_HOME:-$HOME/.local/state}/codexshim/logs`

保留策略：总量 512 MiB，保留 30 天。可查看或清理：

```console
codexshim logs status
codexshim logs purge
```

记录包含标识符、阶段、结果、计时与错误类别——绝不包含 MCP 参数、grep 模式、进程参数、stdin、文件内容或 stdout/stderr。复现工具加载失败时，将 `CODEXSHIM_LOG_MODE=all`。

## 致谢

PDF 读取基于 [PDFOxide](https://github.com/yfedoseev/pdf_oxide)，token 计数基于 [Gigatoken](https://github.com/marcelroed/gigatoken)。感谢这两个项目的工作。

设计和测量 `read`、`grep` 与 `glob` 时，我们也从 [FastCtx](https://github.com/yc-duan/fastctx) 学到很多。

## 许可证

[MIT](LICENSE)
