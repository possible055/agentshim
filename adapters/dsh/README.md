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
- A DSH profile compatible with the `0.1.0-rc.5` package family.
- A mounted local filesystem service. A `shell` executor is optional: the
  minimal preset has none, and process tools then run unconfined. A sandboxing
  executor still requires `sandboxPolicy` and fail-closed escalation.
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
    env: {}
```

Every filesystem path in the configuration must be absolute. A development
checkout can run agentshim through Cargo without installing it by setting
`command` to the absolute `cargo` executable and `commandArgs` to
`["run", "--locked", "--"]`. The spawned command is always:

```text
<command> <commandArgs...> serve --client-profile codex --read-scope <readScope>
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

A missing or mismatched root, an incompatible filesystem provider, an
unresolvable executable, server startup failure, or catalog drift fails plugin
activation. The plugin never falls back silently to DSH's inherited tools.
Changing the mounted shell executor's confinement capability after activation
fails later process calls with `AGENTSHIM_PROCESS_POLICY_CHANGED` until the
plugin is reloaded.

## Timeouts and detached commands

All six definitions declare `timeoutMs` (600 seconds by default). DSH enforces
that deadline only when the composition includes
`dsh-tool-call-timeout-policy`; standard DSH bundles include it, but a custom
composition without it has no DSH-side 600-second deadline. The private MCP
request timeout always applies, including during initialize and paginated
catalog requests, so development wrappers get the full configured startup
budget.

agentshim's runtime catalog is authoritative for `bash.timeout_ms`, including
its default and maximum. Long-running work that exceeds that cap must use
`detach: true` with a repository-local `log_path`, retain the returned
instance-bound `job_id`, inspect it with `bash_status`, and stop it with
`bash({ action: "terminate", job_id })`. These are not DSH jobs:
`job_output` and `job_kill` cannot inspect or control them. Unloading the
plugin closes the private server, which cleans up detached process trees it
owns.

## Process security

The private agentshim child does not run inside DSH's per-call sandbox
executor. When the composition has a sandboxing executor, `run_program` and
`bash` fail closed unless the standing policy is already `danger-full-access`.
Under `read-only` or `workspace-write`, retry the identical call with both:

```text
sandbox_permissions: "danger-full-access"
justification: "one sentence explaining why this exact command needs full access"
```

Only `danger-full-access` is accepted. A user must grant the one-time approval
before the command is sent to agentshim, and the two adapter-only fields are
removed before transport. Rejection, cancellation, an unavailable approval
channel, or an unpaired field prevents execution. If the mounted shell executor
is unsandboxed, or no shell executor is mounted, these fields are not
advertised and process calls run without DSH confinement. A changed
confinement capability or a missing sandbox policy under a confining
executor fails with `AGENTSHIM_PROCESS_POLICY_CHANGED`.

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
- **Catalog validation or `AGENTSHIM_CATALOG_CHANGED`:** the running agentshim
  contract differs from the one registered with DSH. Reload the plugin after
  installing a compatible agentshim build.
- **Process requires full access:** retry the exact call with the two escalation
  fields shown above and approve it, or use a profile whose standing policy is
  already `danger-full-access`.
- **PDF image refusal:** select an image-capable model with an attachment store,
  or use `pdf_mode: "text"`.
- **Unexpected server close:** the failed in-flight call is never replayed. The
  next new call makes one reconnect attempt.

## Remove

```sh
dsh plugin --profile <profile> remove dsh-agentshim
```

Removal drops the plugin layer from that profile.
