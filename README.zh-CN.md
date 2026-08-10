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

用 `read` 读取 `log_path` 观察进度——它的 `next_start_line` 续读信息即为游标。detached 行程树由 codexshim 实例而非本次调用追踪。Windows 上的生命周期所有权涵盖 Job Object；Unix 上则涵盖 codexshim 建立的 process group，程序若调用 `setsid()` 或 daemonize 可能逃离该群组，因此这是 best-effort lifecycle cleanup，并非 sandbox。`CODEXSHIM_DETACHED_CALLS` 限制同时存活的数量（1–16，默认 16）；roster 在 blocking scheduling 前就保留名额，因此满载会立即返回可重试的 `resource_busy` 错误，并列出存活各笔的 pid 与 log 路径。`log_path` 先做词法 admission，再透过保留的 repository capability 开启，以阻挡 symlink／junction escape。

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

### 读取 PDF

`read` 依据第 0 字节的 `%PDF-` 头识别 PDF，因此路由不依赖扩展名。PDF 输入会拒绝 `encoding`、`start_line` 与 `line_count`。

| 参数 | 取值 | 含义 |
| --- | --- | --- |
| `pdf_mode` | `auto`（默认）、`text`、`image` | `auto` 与 `text` 返回页面 Markdown；`image` 渲染 PNG 内容块。 |
| `pages` | `"7"` 或 `"7-12"` | 单页或一段连续范围，不接受离散页码列表。超出末页的范围会被夹到末页；起点在末页之后则报错。 |
| `pdf_text_offset` | 大于等于 0 的整数 | 以 UTF-8 字节偏移续读同一页的 Markdown。 |
| `pdf_source_id` | 不透明 token | 回带上一轮响应的值，用于证明续读指向同一个来源版本。 |

`auto` 只做文字擷取、不渲染，也绝不会在同一次调用中返回图片。`text` 即使文字品质可疑也仍然返回非空文字。`image` 是明确的渲染请求，不提供 OCR。

页数限制的是一次调用的工作量，不是回答的大小：

| 模式 | 未指定 `pages` | 明确范围硬上限 |
| --- | --- | --- |
| `auto`、`text` | 前 10 页 | 20 页 |
| `image` | 第 1 页 | 4 页 |

无论先撞到哪个上限，响应都会说明交付到哪里以及如何继续。下一段范围以本次「实际交付」的页数为准：20 页的请求若只放得下 6 页，续读建议是 `7-12`，不是 `7-26`。

图片响应的 base64 总量上限为 8 MiB，与文字输出预算彼此独立。这个额度只花在完整页面上——放不下的页面会变成 continuation，而不是被截断的图片。内嵌图片若宣告超过 8000 万像素或单边超过 20000，会在配置前就被拒绝，因此不合理的 `/Width`、`/Height` 永远不会碰到缓冲区。

每个成功的 PDF 响应都会带一个 `Source:` token。把它当作 `pdf_source_id` 回传到下一次调用，来源若在两轮之间被替换就会以 `file_changed` 失败，而不是静默拼接两个版本。不带这个值时，该次调用退化为既有的 fingerprint 检查。

PDF 专属错误码，全部使用统一的 `{ error: { code, message, retryable, details } }` 信封：

| 错误码 | 情境 | `retryable` |
| --- | --- | --- |
| `pdf_image_required` | 选定范围内没有任何一页有可用文字，`details.retry_with` 给出可行的确切参数。 | `false` |
| `pdf_unsupported` | 不支持的 filter、结构或格式能力。 | `false` |
| `pdf_invalid` | 结构上不是有效的 PDF。 | `false` |
| `pdf_encrypted` | 仅凭读取端能力无法开启。 | `false` |
| `pdf_processing` | 擷取或渲染失败，且并非上限所致。 | `false` |
| `resource_limit` | 超出文件、stream、cache、像素或 payload 预算，`details` 会指出 `resource`、上限与实测值。 | `false` |
| `validation` | 模式、页面范围或 offset 的组合不合法。 | `false` |

每个交付的页面都会在 `Pages:` 行报告一个状态：

| 状态 | 含义 |
| --- | --- |
| `text_ready` | 有足够且品质可接受的文字。 |
| `text_uncertain` | 有文字，但品质或覆盖可疑。文字**仍会**返回，不会被丢弃。 |
| `image_required` | 没有可用文字，但页面有可见绘制内容——包括完全没有内嵌图片的纯向量页。 |
| `blank` | 既没有绘制内容也没有文字。 |
| `unavailable` | 该页无法处理。刻意与 `blank` 区分：后者会让你以为已经读完。 |

这些状态描述的是「下一步能做什么」，而不是文件的来源。没有 `scanned` 旗标：PDF 格式没有可靠的来源信号，而同一份文件经常同时含有可擷取文字、纯点阵、向量图、空白页与不完整文字层。判断某页是否需要渲染本身不会解任何图片。

同时含可读页与纯图片页的文件算成功而非失败：可读页以 Markdown 返回，其余变成 placeholder，每个 `image_required` 页各附一行 `Retry:` 指明能渲染该页的 `pdf_mode="image"` 调用。单一 `unavailable` 页不会让整次呼叫失败。只有在选定范围内**每一页**都没有可用文字时才返回 `pdf_image_required`；只有每一页都处理失败时才返回 `pdf_processing`。

**准入。** PDF 工作成本高，因此单一实例同时最多只跑一个 PDF 呼叫。第二个并行呼叫最多等待 300 ms，之后返回可重试的 `resource_busy`，并指明是哪一种许可（`pdf_concurrency` 或 `memory_budget`）以及 `retry_after_ms` 延迟建议。一般文字读取完全不接触这个 gate，也不会排在 PDF 的记忆体预留后面，因此慢速 PDF 不会拖慢它们。

每个模式另有涵盖 PDF 工作本身（不含排队时间）的 wall-clock 上限：`auto` 与 `text` 为 5 秒，`image` 为 10 秒。超过即返回可重试的 `resource_timeout`，`details` 会带上上限、已耗用时间，并注明工作已停止且未产生部分输出。两个上限都是内部常数。

`file_changed` 触发的重试会继续持有已取得的 slot 与预留，因此撞到一次 `file_changed` 绝不会在第二次尝试变成 `resource_busy`。

**预留同时就是上限。** 一次呼叫向共用池预留的位元组，正是解析器被约束的位元组：物件快取、解压后的串流、保留的串流快取、单页 Markdown、以及交叉参照重建暂存，各自以该数值的一个比例为上限，而它们的即时总和以该数值本身为上限。因此设定 `CODEXSHIM_PDF_TEXT_MEMORY_BYTES` 改变的是解析器可以配置多少，而不只是排程器如何记帐。

其中两道上限是数量而非位元组，因为它们约束的是大量小配置而非单一大配置：单页可累积的文字 span 数，以及单一内容流一次可解析的运算子数。两者都由预留推导而来，会随其变动 —— `codexshim doctor` 会报告推导出的每页 span 上限。超过任一上限的页面回报为 `unavailable`，选取范围内的其余页面照常送出；这是该页的性质而非整次呼叫的性质，所以绝不会连带丢弃呼叫端仍读得到的页面。

### 环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `CODEX_MCP_PROTOCOL_VERSION` | — | 向 Codex 声明的 MCP 协议版本。 |
| `CODEXSHIM_MCP_COMPATIBILITY` | `lenient` | 设为 `strict` 以拒绝旧版 `2025-06-18` initialize 客户端。 |
| `CODEXSHIM_PROCESS_CALLS` | `16` | 每个实例的进程调用并行上限，由 `run_program` 与 `bash` 共用；接受 1 到 32 的整数。 |
| `CODEXSHIM_DETACHED_CALLS` | `16` | 每个实例存活中的 detached `bash` 行程树数量；接受 1 到 16 的整数。 |
| `CODEXSHIM_ALLOW_PROGRAMS` | 空 | 逗号分隔的 `run_program` 白名单；`--allow-programs` 旗标优先。 |
| `CODEXSHIM_OUTPUT_BYTES` | `32000` | 每次呼叫的输出上限（位元组）；接受 4096 到 262144 的整数。 |
| `CODEXSHIM_GREP_MEMORY_BYTES` | `268435456` | 每次 `grep` 呼叫保留候选项目的记忆体硬上限；接受 8388608 到 1073741824 的整数。 |
| `CODEXSHIM_GLOB_MEMORY_BYTES` | `33554432` | 每次 `glob` 呼叫保留匹配项目的记忆体硬上限；接受 8388608 到 1073741824 的整数。 |
| `CODEXSHIM_PDF_TEXT_MEMORY_BYTES` | `67108864` | `auto` 与 `text` 模式 PDF 读取的每次呼叫预留**兼强制上限**；接受 33554432 到 134217728 的整数。 |
| `CODEXSHIM_PDF_IMAGE_MEMORY_BYTES` | `100663296` | `image` 模式 PDF 读取的每次呼叫预留**兼强制上限**；接受 67108864 到 201326592 的整数。 |
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

记录包含标识符、阶段、结果、计时、错误类别，以及为 `bash` 调用推导的五值 `shell_delegate` 分类——绝不包含 MCP 参数、grep 模式、进程参数或环境、stdin、文件内容或 stdout/stderr。`shell_delegate` 仅由命令的第一个 token 推导，不保留该 token 原文、命令、参数或路径。

## 许可证

[MIT](LICENSE)
