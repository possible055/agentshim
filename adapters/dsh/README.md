# dsh-agentshim

`dsh-agentshim` exposes `read`, `grep`, `glob`, `run_program`, `bash`, and `bash_status` as native DSH tools. The plugin loads the platform `agentshim-napi` addon in-process; it does not start `agentshim serve` and has no compatibility fallback.

## Requirements

- Node.js `^22.19.0 || >=24.0.0`.
- The DSH `0.1.0-rc.6` or `0.1.0-rc.7` package family.
- A local filesystem provider for the configured root.
- One supported platform: Windows x64 MSVC, macOS ARM64, Linux x64 glibc, or Linux ARM64 glibc.
- Background Bash requires `ctx.jobs` and a job controller.

An unsupported platform, a missing optional native package, a load failure, or an addon API version other than `4` rejects plugin activation.

## Install

Plugins are managed per profile under `$DSH_HOME/profiles/<profile>`. Choose the profile matching your use case (e.g. `web` for the standard Web UI or `headless` for CLI tasks):

```sh
# For DSH Web UI (most common):
dsh plugin --profile web add dsh-agentshim
dsh web --dump-config

# For Headless CLI:
dsh plugin --profile headless add dsh-agentshim
dsh --profile headless --dump-config
```

The entry package exact-pins its platform package. There is no install script or downloaded binary.

## Configuration

```yaml
- id: agentshim
  config:
    root: /absolute/path/to/repo
    readScope: normal
    toolCallTimeoutMs: 600000
    captureRoot: /absolute/private/artifact/root
    captureMaxBytes: 67108864
    captureCleanup: never
    env: {}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `root` | `process.cwd()` | Canonical local root and exact agent-cwd match target. |
| `readScope` | `normal` | `normal` or `unrestricted`. |
| `env` | `{}` | Child variables layered over DSH's credential-scrubbed parent environment. `AGENTSHIM_BASH` set here also directs load-time bash discovery, so plugin config is honored even when the host process never had it. |
| `toolCallTimeoutMs` | `600000` | DSH deadline shelf; values below 600000 are rejected and the process ceiling is 590000 ms. |
| `captureRoot` | Platform data directory | Private persistent process-artifact root; an explicit value must be absolute. |
| `captureMaxBytes` | `67108864` | Aggregate raw bytes per process call; 1 MiB through 1 GiB. |
| `captureCleanup` | `never` | `never` or cleanup of this Engine's session directory at `session-end`. |

## Process and background behavior

Every process call is prepared before DSH sandbox policy wraps the exact argv. `read-only` and `workspace-write` calls use `ctx.sandbox.confine()`; approved `danger-full-access` calls remain one-shot. Confinement failure never retries unconfined.

`run_in_background: true` registers a DSH-owned job. Use `job_output`, `job_list`, and `job_kill`; `bash_status` is a non-consuming lifecycle snapshot. Plugin unload and owner disposal wait for process trees, pipes, capture publication, and native threads to settle.

## PDF images and artifacts

PDF images are validated, routed only to image-capable models, and persisted as DSH attachments. Raw base64 is not stored in the canonical tool value. Code Mode receives deferred attachment context.

Large or non-text process output is published under `captureRoot` as an exact-file capability. Text artifacts support numbered `read` and single-file `grep`. Binary artifacts use `read` with `artifact_offset`, returning base64 pages and `next_artifact_offset`; binary grep is rejected. `glob` cannot enumerate capture storage.

Capture directories are owner-only (`0700`/`0600` on Unix and a verified owner-only DACL on Windows). Manage retention with:

```sh
dsh-agentshim-captures status
dsh-agentshim-captures purge --older-than-days 30
dsh-agentshim-captures purge --all
```

## Development gates

```sh
cargo build --locked -p agentshim-napi
pnpm install --frozen-lockfile
pnpm typecheck
pnpm lint
pnpm test
pnpm build
pnpm test:release-packages
pnpm test:packed
pnpm verify:cutover
```

Source/native tests may stage the locally built library through `AGENTSHIM_DSH_NATIVE_DLL`. The packed smoke does not: it fresh-packs and installs the entry plus the current platform package in a temporary consumer, then executes a native read through the package export. Existing `.tgz` files in this directory are never used as development or release evidence.

## Remove

```sh
# For DSH Web UI:
dsh plugin --profile web remove dsh-agentshim

# For Headless CLI:
dsh plugin --profile headless remove dsh-agentshim
```
