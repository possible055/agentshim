# dsh-codexshim

`dsh-codexshim` exposes the five codexshim tools — `read`, `grep`, `glob`,
`run_program`, and `bash` — as native DSH tools. It replaces only those
overlapping tools in agents whose canonical working directory matches the
configured root. Other DSH tools, including `write`, `edit`, and `read_image`,
remain available.

The plugin owns a private MCP session over stdio using the official
`@modelcontextprotocol/sdk`. MCP is transport only: neither Native mode nor
Code Mode exposes an `mcp__*` tool name.

## Prerequisites

- Node.js `^22.19.0 || >=24.0.0` and pnpm.
- A DSH profile compatible with the `0.1.0-rc.5` package family.
- A mounted DSH `shell` service. The adapter waits in Cordis `PENDING` state
  until both the local filesystem and shell services are available.
- codexshim installed at its standard platform path, or an absolute `command`
  override:
  - Windows: `%LOCALAPPDATA%\codexshim\bin\codexshim.exe`
  - Unix: `${XDG_DATA_HOME:-$HOME/.local/share}/codexshim/bin/codexshim`

The plugin never searches `PATH`, downloads codexshim, or replaces an existing
installation. The configured root must use DSH's shipped local filesystem
provider (`LocalFileSystem` or its `SandboxedFileSystem` subclass).

## Install

Install the published package or a tarball into one profile:

```sh
dsh plugin --profile <profile> add dsh-codexshim
# or
dsh plugin --profile <profile> add /absolute/path/to/dsh-codexshim-0.1.0.tgz
```

Confirm that the bundle layer is present:

```sh
dsh --profile <profile> --dump-config
```

The default configuration uses the directory from which DSH starts. A profile
can replace the bundle row's complete configuration in
`$DSH_HOME/profiles/<profile>/cordis.patch.yml`:

```yaml
- id: codexshim
  config:
    root: !!js process.cwd()
    readScope: normal
    toolCallTimeoutMs: 600000
    env: {}
```

Every supplied filesystem path must be absolute after evaluation. A development
checkout can launch through Cargo without installing codexshim by setting
`command` to the resolved absolute Cargo executable and `commandArgs` to
`[run, --locked, --]`. The resulting child command is always:

```text
<command> <commandArgs...> serve --client-profile codex --read-scope <readScope>
```

## Configuration

| Field | Default | Meaning |
| --- | --- | --- |
| `root` | `process.cwd()` | Canonical local root, child cwd, and exact agent-cwd match. |
| `command` | Standard install path above | Absolute codexshim executable path. |
| `commandArgs` | `[]` | Arguments before the fixed `serve` arguments; intended for development wrappers. |
| `readScope` | `normal` | `normal` or `unrestricted`, always passed explicitly to codexshim. |
| `env` | `{}` | Explicit child variables layered over DSH's credential-scrubbed parent environment. |
| `toolCallTimeoutMs` | `600000` | DSH definition timeout and every private MCP request timeout, including initialize and catalog listing; values below 600000 are rejected. |

Executable resolution failure, a missing or mismatched root, an incompatible
filesystem provider, server startup failure, or catalog drift fails plugin
activation. The plugin never falls back silently to DSH's inherited tools.
Replacing the mounted shell provider unloads and reloads the adapter so its
process schemas follow the current executor capability.

## Timeouts and detached commands

All five definitions declare `timeoutMs` (600 seconds by default). DSH enforces
that deadline only when the composition includes
`dsh-tool-call-timeout-policy`; standard DSH bundles include it, but a custom
composition without it has no DSH-side 600-second deadline. The private MCP
request timeout still applies.

The same private timeout covers MCP initialize and every paginated catalog
request. Development wrappers such as `cargo run --locked --` therefore have
the full configured startup budget rather than the SDK's generic 60-second
default.

codexshim's runtime catalog is authoritative for `bash.timeout_ms`, including
its default and maximum. The adapter does not hard-code the server's advertised
cap. Long-running work that exceeds that cap must use `detach: true` with a
repository-local `log_path`, then poll the log with this plugin's `read` tool.
The returned pid and log path are not DSH jobs: `job_output` and `job_kill`
cannot inspect or control them. Unloading the plugin closes the private server,
which cleans up detached process trees it owns.

## Process security

The private codexshim child does not run inside DSH's per-call sandbox executor.
When the composition has a sandboxing executor, `run_program` and `bash` fail
closed unless the standing policy is already `danger-full-access`. Under
`read-only` or `workspace-write`, retry the identical call with both:

```text
sandbox_permissions: "danger-full-access"
justification: "one sentence explaining why this exact command needs full access"
```

Only `danger-full-access` is accepted. A user must grant the one-time approval
before the command is sent to codexshim, and the two adapter-only fields are
removed before transport. Rejection, cancellation, an unavailable approval
channel, or an unpaired field prevents execution. If the mounted shell executor
is unsandboxed, these fields are not advertised and process calls run without
DSH confinement. A missing shell, a changed confinement capability that has not
yet completed Cordis reload, or a missing sandbox policy under a confining
executor fails with `CODEXSHIM_PROCESS_POLICY_CHANGED` before MCP execution.

## Images and filesystem observations

PDF image results are accepted only when an attachment store exists and the
routed model explicitly supports image input. Images are validated before any
are persisted, converted to durable DSH attachment references, and never stored
as raw base64 in the tool result or session log. If the route cannot accept an
image, retry with `pdf_mode: "text"`.

Successful `read` calls bridge DSH's filesystem observation policy so retained
`write` and `edit` tools can use the observed version. If the file changes
during the read, no stale observation is recorded.

## Troubleshooting

- **Executable not found:** install codexshim at the standard path or configure
  an absolute `command`; the plugin intentionally does not search `PATH`.
- **Root or execution-world mismatch:** start DSH from the configured root and
  use the shipped local filesystem provider. Remote/E2B filesystems are not
  supported.
- **Catalog validation or `CODEXSHIM_CATALOG_CHANGED`:** the running codexshim
  contract differs from the one registered with DSH. Reload the plugin after
  installing a compatible codexshim build.
- **Process requires full access:** retry the exact call with the two escalation
  fields shown above and approve it, or use a profile whose standing policy is
  already `danger-full-access`.
- **PDF image refusal:** select an image-capable model with an attachment store,
  or use `pdf_mode: "text"`.
- **Unexpected server close:** the failed in-flight call is never replayed. The
  next new call makes one serialized reconnect attempt.

## Remove

```sh
dsh plugin --profile <profile> remove dsh-codexshim
```

Removal drops the package-managed bundle layer from that profile.
