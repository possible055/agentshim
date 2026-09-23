# dsh-agentshim

`dsh-agentshim` exposes `read`, `grep`, `glob`, `run_program`, `bash`, and `bash_status` as native DSH tools. The plugin loads the platform `agentshim-napi` addon in-process; it does not start `agentshim serve` and has no compatibility fallback.

## Requirements

- Node.js `^22.19.0 || >=24.0.0`.
- The supported DSH package families are `0.1.5-alpha.1`, `0.1.5-alpha.2`, `0.1.5-rc.1`, `0.1.5-rc.2`, `0.1.6-alpha.1`, `0.1.6-alpha.2`, `0.1.7-alpha.1`, `0.1.7-alpha.2`, and `0.1.7-rc.1`. Development and CI use DSH `0.1.7-rc.1` with Cordis `4.0.4`; the peer declaration retains the supported historical DSH releases.
- A local filesystem provider for the configured root.
- One supported platform: Windows x64 MSVC, macOS ARM64, Linux x64 glibc, or Linux ARM64 glibc.
- Background Bash requires `ctx.jobs` and a job controller.

An unsupported platform, a missing optional native package, a load failure, or an addon API version other than `5` rejects plugin activation.

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
    toolCallTimeoutMs: 600000
    captureRoot: /absolute/private/artifact/root
    captureMaxBytes: 67108864
    captureCleanup: never
    env:
      AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX: '600'
```

| Field | Default | Meaning |
| --- | --- | --- |
| `root` | `process.cwd()` | Plugin activation and configuration context. Each installed agent process uses that agent session's canonical cwd; agents without a session cwd are left untouched. |
| `env` | `{}` | Child variables layered over DSH's credential-scrubbed parent environment. `AGENTSHIM_BASH` set here also directs load-time bash discovery. Accepts a GNU Bash executable (Git Bash `bash.exe`), a BusyBox-w32 dispatcher binary (`busybox64u.exe`), or an applet-named BusyBox copy (`sh.exe`/`ash.exe`/`bash.exe`); the shell flavor and invocation form are auto-detected by the probe. `AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX` is parsed once at activation after this merge, so config overrides the parent environment. |
| `toolCallTimeoutMs` | `600000` | DSH deadline shelf; values below 600000 are rejected and the process ceiling is 590000 ms. |
| `readScope` | `unrestricted` | `normal` is opt-in for `read`, `grep`, and `glob`; it does not configure process confinement. |
| `captureRoot` | Platform data directory | Private persistent process-artifact root; an explicit value must be absolute. |
| `captureMaxBytes` | `67108864` | Aggregate raw bytes per process call; 1 MiB through 1 GiB. |
| `captureCleanup` | `never` | `never` or cleanup of this Engine's session directory at `session-end`. |

`read`, `grep`, and `glob` use the configured `readScope` (`unrestricted` by default; `normal` is opt-in). Process confinement is not implemented by this plugin: when composed, the plugin delegates exact argv to the official DSH `ctx.sandbox` and `ctx.sandboxPolicy` services, while an absent service preserves the existing unconfined behavior.

## Process and background behavior

Every process call is prepared before DSH sandbox policy wraps the exact argv. `read-only` and `workspace-write` calls use `ctx.sandbox.confine()`; approved `danger-full-access` calls remain one-shot. Confinement failure never retries unconfined.

`run_in_background: true` registers a DSH-owned job. Its public `timeoutMs` is measured from successful spawn; omitted values use `AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX` (1800 seconds by default, range 600–14400), and explicit values may only shorten it. Expiry becomes a native `timed_out` outcome and a failed DSH job detail. `bash_status` preserves the DSH snapshot and, after native settlement, exposes exit code, failure, artifacts, `limitExceeded`, official sandbox attribution, and denial/runner-failure flags. Use `job_output`, `job_list`, and `job_kill` for the standard DSH job lifecycle. Plugin unload and owner disposal race through the same first-wins tree owner and wait for process trees, pipes, capture publication, and native threads to settle.

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
pnpm test:packed:matrix # Linux: all currently published DSH families
pnpm verify:cutover
```

Source/native tests may stage the locally built library through `AGENTSHIM_DSH_NATIVE_DLL`. The packed smoke does not: it fresh-packs and installs the entry plus the current platform package in isolated temporary consumers, then executes a native read through the package export. The Linux matrix covers every supported published DSH family. Existing `.tgz` files in this directory are never used as development or release evidence.

## Remove

```sh
# For DSH Web UI:
dsh plugin --profile web remove dsh-agentshim

# For Headless CLI:
dsh plugin --profile headless remove dsh-agentshim
```
