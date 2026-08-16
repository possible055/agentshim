# dsh-agentshim

`dsh-agentshim` exposes the six agentshim tools — `read`, `grep`, `glob`,
`run_program`, `bash`, and `bash_status` — as native DSH tools. It replaces overlapping
names the agent already has (plus `pwsh`, when present) in agents whose canonical working
directory matches the configured root. `run_program` accompanies filesystem tools, and
`bash_status` always accompanies `bash`, so a minimal catalog becomes `bash`, `bash_status`,
and `str_replace_editor`; standard presets keep `write`, `edit`, and `read_image`.

The plugin runs a private agentshim MCP session over stdio. MCP is transport
only; no `mcp__*` tool name is exposed to the agent.

## Prerequisites

- Node.js `^22.19.0 || >=24.0.0` and pnpm.
- A DSH profile using the exact `0.1.0-rc.6` package family.
- A mounted local filesystem service. A `shell` executor is optional: the
  minimal preset has none, and process tools then run unconfined.
- Background Bash additionally requires `ctx.jobs` and a job controller,
  normally supplied by `@deepseek-ai/dsh-jobs-local` and
  `@deepseek-ai/dsh-tool-jobs` in standard compositions.
- agentshim installed at its standard platform path, or an absolute `command`
  override:
  - Windows: `%LOCALAPPDATA%\agentshim\bin\agentshim.exe`
  - Unix: `${XDG_DATA_HOME:-$HOME/.local/share}/agentshim/bin/agentshim`

The plugin never searches `PATH`, downloads agentshim, or replaces an existing
installation. The configured root must be served by DSH's local filesystem
provider; remote or E2B filesystems are not supported.

## Install

Install the published package or a tarball into one profile:

```sh
dsh plugin --profile <profile> add dsh-agentshim
# or
dsh plugin --profile <profile> add /absolute/path/to/dsh-agentshim-0.1.0.tgz
```

Confirm the plugin layer is present:

```sh
dsh --profile <profile> --dump-config
```

By default the plugin uses the directory from which DSH starts as its root. A
profile can override the configuration in
`$DSH_HOME/profiles/<profile>/cordis.patch.yml`:

```yaml
- id: agentshim
  config:
    root: /absolute/path/to/repo
    readScope: normal
    toolCallTimeoutMs: 600000
    captureRoot: /absolute/private/artifact/root
    captureMaxBytes: 67108864
    env: {}
```

Every filesystem path in the configuration must be absolute. A development
checkout can run agentshim through Cargo without installing it by setting
`command` to the absolute `cargo` executable and `commandArgs` to
`["run", "--locked", "--"]`. The spawned command is always:

```text
<command> <commandArgs...> serve --client-profile dsh --read-scope <readScope>
```

## Configuration

| Field | Default | Meaning |
| --- | --- | --- |
| `root` | `process.cwd()` | Canonical local root, child cwd, and the exact agent-cwd match target. |
| `command` | Standard install path above | Absolute agentshim executable path. |
| `commandArgs` | `[]` | Arguments before the fixed `serve` arguments; intended for development wrappers. |
| `readScope` | `normal` | `normal` or `unrestricted`, always passed explicitly to agentshim. |
| `env` | `{}` | Extra child variables layered over DSH's credential-scrubbed parent environment. |
| `toolCallTimeoutMs` | `600000` | DSH tool-call deadline and every private MCP request timeout, including initialize and catalog listing; values below 600000 are rejected. |
| `captureRoot` | Stable platform data directory | Private persistent process-artifact root; an explicit value must be absolute. |
| `captureMaxBytes` | `67108864` | Aggregate raw bytes for all streams in one process call; configurable from 1 MiB through 1 GiB. |

A missing or mismatched root, an incompatible filesystem provider, an
unresolvable executable, server startup failure, missing private operation, or
DSH bridge-version mismatch fails plugin activation. Extra private operations
and catalog reordering are allowed. The plugin never falls back silently to
DSH's inherited tools. If `ctx.sandbox` and `ctx.sandboxPolicy` are not both
present or both absent, activation fails. Changing that capability composition
after activation makes later process calls fail with
`AGENTSHIM_PROCESS_POLICY_CHANGED` until the plugin is reloaded.

## Timeouts and background jobs

All six definitions declare `timeoutMs` (600 seconds by default). DSH enforces
that deadline only when the composition includes
`dsh-tool-call-timeout-policy`; standard DSH bundles include it, but a custom
composition without it has no DSH-side 600-second deadline. The private MCP
request timeout always applies, including during initialize and paginated
catalog requests, so development wrappers get the full configured startup
budget.

Foreground Bash uses `timeoutMs`; long-running work uses
`run_in_background: true`. The latter registers a DSH-owned job and returns a
DSH `JobId` such as `bash-1`. Use `job_output`, `job_list`, and `job_kill` for
output, discovery, and termination. `bash_status` is published only beside
`bash` and returns a non-consuming DSH lifecycle snapshot for the same public
id. It does not consume the `job_output` cursor.

The adapter keeps AgentShim's instance-bound UUID, remote capture protocol, and
terminate operation private. The confined process writes only inherited pipes;
the adapter, outside the sandbox, persists byte-exact output under `captureRoot`.
`bash_status` polls lifecycle only. Its 1 MiB live buffer and DSH's 64 KiB job
view may omit old text with an explicit marker, while the raw artifact remains
complete. Exceeding `captureMaxBytes`, storage failure, or protocol drift stops
the process tree and settles the job as failed with a partial artifact locator.
Owner disposal, plugin unload, and job cancellation wait for the tree, pipe
drain, capture flush, and dedicated generation to quiesce.

## Output capture and retention

Model-visible process text is limited to 50,000 bytes including headings,
omission markers, and artifact notices. Output at or below that threshold is not
retained when it is valid UTF-8 without NUL. Larger or non-text output is stored
as a permanent `application/octet-stream` artifact. `read` and single-file
`grep` can use an exact published path without broadening `readScope`; the grant
does not expose its parent, adjacent files, or `glob` enumeration.

Artifacts are deliberately not garbage-collected so locators remain valid in
durable logs, resumed sessions, and forks. They can contain credentials or other
sensitive process output and accumulate indefinitely; operators must secure and
clean `captureRoot` according to local retention policy. Unix directories use
mode `0700` and files `0600`. `danger-full-access` retains its existing trust
boundary: a process running as the same OS user may discover host data.

The DSH server profile has no token or burst gate. `AGENTSHIM_OUTPUT_BYTES` and
`AGENTSHIM_BURST_TOKENS` do not affect it; the native preview is 50,000 bytes and
the encoded text/structured transport safety cap is 1 MiB. Codex and Cursor
output behavior is unchanged.

## Process security

When `ctx.sandbox` and `ctx.sandboxPolicy` are composed, every `run_program`,
foreground `bash`, and background `bash` resolves the authoritative policy for
that call. `read-only` and `workspace-write` launch a dedicated private MCP
generation through `ctx.sandbox.confine()` using the exact AgentShim server
argv. `danger-full-access` foreground calls may use the shared generation;
background work always owns a dedicated generation.

A call may request a strictly wider one-shot mode with both:

```text
sandbox_permissions: "workspace-write" | "danger-full-access"
justification: "one sentence explaining why this exact command needs wider access"
```

Approval completes before the generation starts, and its authority lasts only
for that foreground call or background job. The adapter-only fields are removed
before MCP transport. Rejection, cancellation, an unavailable approval channel,
or an unpaired field prevents execution. If neither sandbox service is mounted,
the escalation fields are not advertised and process calls run explicitly
unconfined.

DSH sandbox modes constrain filesystem effects; they are not a confidentiality
boundary and do not promise network or process-visibility isolation.
`readScope` is independent: it controls which paths AgentShim's structured
`read`, `grep`, and `glob` can reach. Keep `readScope: normal` unless broader
local reads are intentional. A backend may report partial enforcement; the
canonical result preserves that attribution. Runner or confinement failure is
`SANDBOX_UNAVAILABLE` and never retries unconfined.

## Images and filesystem observations

PDF image results are accepted only when an attachment store exists and the
routed model explicitly supports image input. Images are validated before any
are persisted and are stored as durable DSH attachment references, not as raw
base64. If the route cannot accept an image, retry with `pdf_mode: "text"`.

Successful `read` calls bridge DSH's filesystem observation policy so retained
`write` and `edit` tools can use the observed version. If the file changes
during the read, no stale observation is recorded.

## Troubleshooting

- **Executable not found:** install agentshim at the standard path or configure
  an absolute `command`; the plugin intentionally does not search `PATH`.
- **Root or execution-world mismatch:** start DSH from the configured root and
  use the local filesystem provider. Remote/E2B filesystems are not supported.
- **Bridge validation or `AGENTSHIM_CATALOG_CHANGED`:** the running AgentShim
  lacks a required private operation or advertises an incompatible DSH bridge
  version. Reload the plugin after installing a compatible build.
- **Background jobs unavailable:** compose `@deepseek-ai/dsh-jobs` with a
  controller such as `@deepseek-ai/dsh-tool-jobs`; `bash_status` cannot replace
  the controller required by `ctx.jobs.start()`.
- **Sandbox unavailable:** inspect the backend/runner diagnostic. The adapter
  deliberately does not retry the command outside confinement.
- **PDF image refusal:** select an image-capable model with an attachment store,
  or use `pdf_mode: "text"`.
- **Unexpected server close:** the failed in-flight call is never replayed. The
  next new call makes one reconnect attempt.

## Remove

```sh
dsh plugin --profile <profile> remove dsh-agentshim
```

Removal drops the plugin layer from that profile.
