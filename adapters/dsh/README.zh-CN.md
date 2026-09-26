# dsh-agentshim

适用于 DeepSeek Harness (DSH) 的原生、具备能力感知的仓库工具插件。

`dsh-agentshim` 将 `read`、`grep`、`glob`、`run_program`、`bash` 和 `bash_status` 作为原生 DSH 工具暴露给智能体。该插件在进程内直接加载对应平台的 `agentshim-napi` 原生插件（addon）；它不会启动 `agentshim serve`，也不包含任何协议桥接或内置工具回退机制。

- **直接进程内执行**：调用停留在 Node.js 进程内，只读工具无需启动辅助子进程即可执行。
- **读取结构化文档**：PDF 页面返回文本或渲染图像；Word、Excel 和 PowerPoint 文件返回 Markdown 并附带可重放的接续游标。
- **确定性与有界预算**：文件读取、搜索与命令执行均在明确的字节预算下运作，提供可预测的分页与接续标记。
- **原生生命周期管理**：后台命令由 DSH 任务追踪，并与服务端和智能体生命周期绑定，附带受限的日志保留策略。
- **强化进程执行**：进程调用在 Windows 下运行于私有 Job Object，在 Unix 下运行于独立进程组。DSH 沙箱策略直接包装原生命令行参数。

## 运行环境要求

- Node.js `^22.19.0 || >=24.0.0`。
- 支持的 DSH 套件家族包括 `0.1.5-alpha.1`、`0.1.5-alpha.2`、`0.1.5-rc.1`、`0.1.5-rc.2`、`0.1.6-alpha.1`、`0.1.6-alpha.2`、`0.1.7-alpha.1`、`0.1.7-alpha.2`、`0.1.7-rc.1` 和 `0.1.7-rc.2`。开发与 CI 使用 DSH `0.1.7-rc.2` 及 Cordis `4.0.4`；peer 声明仍保留受支持的既有 DSH 版本。
- 后台 Bash 命令需要 `ctx.jobs` 及任务控制器支持。

不支持的平台、缺失可选的原生包、加载失败或插件 API 版本不为 `5` 均会拒绝插件激活。

## 安装

插件按 Profile 进行管理，位于 `$DSH_HOME/profiles/<profile>`。请选择与您的使用场景相符的 Profile（例如标准 Web UI 使用 `web`，CLI 任务使用 `headless`）：

```sh
# 用于 DSH Web UI（最常用）：
dsh plugin --profile web add dsh-agentshim
dsh web --dump-config

# 用于 Headless CLI：
dsh plugin --profile headless add dsh-agentshim
dsh --profile headless --dump-config
```

入口包对对应平台包进行了精确版本锁定（exact-pin）。无需安装脚本，亦不会下载额外的二进制文件。

## 配置

```yaml
- id: agentshim
  config:
    root: /absolute/path/to/repo
    toolCallTimeoutMs: 600000
    captureRoot: /absolute/private/artifact/root
    captureMaxBytes: 67108864
    captureCleanup: never
    env:
      AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX: '600'
```

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `root` | `process.cwd()` | 仅用于插件激活与配置上下文；每个已安装的 agent process 使用该 agent session 的 canonical cwd；没有 session cwd 的 agent 保持不变。 |
| `env` | `{}` | 叠加在 DSH 凭据清洗（credential-scrubbed）父环境变量之上的子环境变量。在此设定的 `AGENTSHIM_BASH` 也会在加载时驱动 Bash 探测。接受 GNU Bash 可执行文件（Git Bash `bash.exe`）、BusyBox-w32 派发器二进制文件（`busybox64u.exe`）或按 applet 命名的 BusyBox 副本（`sh.exe`/`ash.exe`/`bash.exe`）；shell 类型与调用形式由探针自动判定。`AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX` 在激活合并后解析一次，因此配置会覆盖父环境中的值。 |
| `toolCallTimeoutMs` | `600000` | DSH 工具超时阈值；低于 600000 的值会被拒绝，进程超时上限为 590000 毫秒。 |
| `readScope` | `unrestricted` | 只有显式设置 `normal` 才限制 `read`、`grep` 和 `glob`；它不配置进程 confinement。 |
| `captureRoot` | 平台数据目录 | 私有持久化进程产物根目录；若显式指定则必须为绝对路径。 |
| `captureMaxBytes` | `67108864` | 每次进程调用的原始字节上限；范围为 1 MiB 至 1 GiB。 |
| `captureCleanup` | `never` | 设置为 `never` 或在 `session-end` 时清理当前 Engine 的会话目录。 |

`readScope` 默认为 `unrestricted`，显式设置 `normal` 才限制 `read`、`grep` 和 `glob`。进程 confinement 不由本插件实现：若组合了官方 DSH `ctx.sandbox`／`ctx.sandboxPolicy`，插件只委托 exact argv；若服务缺失则保持原有 unconfined 行为。

## 进程与后台行为

在 DSH 沙箱策略封装确切的 argv 之前，每个进程调用都已完成预先准备。`read-only` 和 `workspace-write` 调用使用 `ctx.sandbox.confine()`；经批准的 `danger-full-access` 调用保持单次生效。沙箱隔离失败绝不会回退至未隔离状态重试。

`run_in_background: true` 会注册一个由 DSH 管理的后台任务。其公开的 `timeoutMs` 从成功创建子进程起算；若省略则使用 `AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX`（默认为 1800 秒，有效范围 600–14400），显式指定的值只能缩短此超时。超时会导致原生 `timed_out` 结果并标记 DSH 任务失败。原生结果 settle 后，`bash_status` 会保留 DSH snapshot，并额外暴露 exit code、failure、artifacts、`limitExceeded`、官方 sandbox attribution 与 denial/runner-failure flags。标准生命周期仍使用 `job_output`、`job_list` 和 `job_kill`；插件卸载与所有者销毁采用竞争机制，安全等待进程树、管道、产物发布与原生线程全部完成。

## PDF 图像与产物

PDF 图像经过格式与安全性校验后，仅路由至具备图像处理能力（image-capable）的模型，并持久化为 DSH 附件。规范的工具返回值中不会存储原始 base64。Code Mode 会接收延迟的附件上下文。

大型或非文本的进程输出将作为精确文件能力发布于 `captureRoot` 目录下。文本产物支持带行号的 `read` 与单文件 `grep`。二进制产物使用带 `artifact_offset` 的 `read`，返回 base64 分页与 `next_artifact_offset`；二进制文件上的 grep 将被拒绝。`glob` 无法枚举产物存储目录。

产物捕获目录仅所有者可访问（Unix 下为 `0700`/`0600`，Windows 下具备经过校验的所有者专用 DACL）。保留策略可通过以下命令管理：

```sh
dsh-agentshim-captures status
dsh-agentshim-captures purge --older-than-days 30
dsh-agentshim-captures purge --all
```

## 开发质量门禁

```sh
cargo build --locked -p agentshim-napi
pnpm install --frozen-lockfile
pnpm typecheck
pnpm lint
pnpm test
pnpm build
pnpm test:release-packages
pnpm test:packed
pnpm test:packed:matrix # Linux：所有当前可发布的 DSH 套件
pnpm verify:cutover
```

源码/原生测试可以通过 `AGENTSHIM_DSH_NATIVE_DLL` 暂存本地构建的库。打包冒烟测试则不同：它会在隔离的临时消费者环境中全新打包并安装入口包及当前平台包，然后通过包导出接口执行原生读取。Linux 矩阵覆盖所有受支持且已发布的 DSH 套件。该目录中现有的 `.tgz` 文件绝不会用作开发或发布的验证证据。

## 卸载

```sh
# 用于 DSH Web UI：
dsh plugin --profile web remove dsh-agentshim

# 用于 Headless CLI：
dsh plugin --profile headless remove dsh-agentshim
```
