import { realpathSync } from 'node:fs'
import type { Context } from '@deepseek-ai/cordis'
import type { Agent } from '@deepseek-ai/dsh-agent'
import type { ToolDefinition } from '@deepseek-ai/dsh-tools'
import z from '@deepseek-ai/schemastery'
import { createSession, MIN_TOOL_CALL_TIMEOUT_MS, resolveSessionConfig } from './session.ts'
import type { CatalogSnapshot, ResolvedSessionConfig } from './session.ts'
import { assertExecutionWorld, sameExecutionPath } from './policy.ts'
import { buildToolDefinitions, promptSections, RESTRICT_CANDIDATES } from './tools.ts'
import { PUBLIC_TOOL_NAMES } from './contracts.ts'
import { BackgroundJobManager } from './jobs.ts'
import { loadNativeAddon, nativeEngineEnv } from './native.ts'
import type { NativeEngine } from './native.ts'
import {
  CaptureStore,
  DEFAULT_CAPTURE_MAX_BYTES,
  MAX_CAPTURE_MAX_BYTES,
  MIN_CAPTURE_MAX_BYTES,
  resolveCaptureRoot,
} from './capture.ts'

/** Cordis plugin name used by loader diagnostics. */
export const name = 'agentshim'

export const inject: readonly string[] = ['fs']

export interface Config {
  /** Plugin working root; defaults to the host's `process.cwd()`. */
  root: string
  /** agentshim executable; defaults to the platform install path. Must be absolute when set. */
  command: string
  /** Arguments placed before the fixed server argv, e.g. `run --locked --` behind a `cargo` command. */
  commandArgs: string[]
  readScope: 'normal' | 'unrestricted'
  /** Extra environment merged on top of the scrubbed parent environment, e.g. `AGENTSHIM_*` settings. */
  env: Record<string, string>
  /** Applied to both the DSH tool deadline and the private MCP request timeout. */
  toolCallTimeoutMs: number
  /** Private persistent root for byte-exact process capture artifacts. */
  captureRoot?: string
  /** Aggregate raw capture ceiling for one process call. */
  captureMaxBytes?: number
}

export const Config = z.object({
  root: z.string().default(''),
  command: z.string().default(''),
  commandArgs: z.array(String).default([]),
  readScope: z.union([z.const('normal'), z.const('unrestricted')]).default('normal'),
  env: z.dict(String).default({}),
  toolCallTimeoutMs: z.number().min(MIN_TOOL_CALL_TIMEOUT_MS).default(MIN_TOOL_CALL_TIMEOUT_MS),
  captureRoot: z.string().default(''),
  captureMaxBytes: z.number().min(MIN_CAPTURE_MAX_BYTES).max(MAX_CAPTURE_MAX_BYTES).default(DEFAULT_CAPTURE_MAX_BYTES),
}) as unknown as z<Config>

/**
 * Publish only names the agent already had, plus the shell/filesystem
 * companions those names imply: hiding `pwsh` still yields `bash`, `bash`
 * always yields `bash_status`, an isolated inherited `bash_status` is hidden,
 * and a filesystem agent gets `run_program`.
 */
function replacementNames(present: readonly string[]): Array<(typeof PUBLIC_TOOL_NAMES)[number]> {
  const selected = new Set<string>()
  for (const name of present) {
    if (name !== 'bash_status' && (PUBLIC_TOOL_NAMES as readonly string[]).includes(name)) selected.add(name)
  }
  if (present.includes('pwsh')) selected.add('bash')
  if (selected.has('bash')) selected.add('bash_status')
  if (selected.has('read') || selected.has('grep') || selected.has('glob')) {
    selected.add('run_program')
  }
  return PUBLIC_TOOL_NAMES.filter(name => selected.has(name))
}

function canonicalOrUndefined(path: string): string | undefined {
  try {
    return realpathSync.native(path)
  } catch {
    return undefined
  }
}

interface AgentInstallation {
  readonly disposers: ReadonlyArray<() => void>
}

/**
 * Install this adapter's contributions on every root-matched agent: restrict
 * only the inherited names that already exist, register the matching
 * agent-local definitions, and shadow those prompt sections. The six-tool
 * catalog stays available; a composition that only mounted `bash` also gets
 * its managed-job `bash_status` companion after replacement.
 * Every step is one transaction — a failure rolls back and (for new agents)
 * vetoes publication. Agents whose canonical cwd is not exactly the plugin
 * root are left completely untouched.
 */
function installAgentTools(
  ctx: Context,
  resolved: ResolvedSessionConfig,
  definitions: ReadonlyMap<string, ToolDefinition>,
): void {
  const installed = new Map<Agent, AgentInstallation>()
  const promptByName = new Map(promptSections().map(section => [section.name, section]))

  function uninstall(agent: Agent): void {
    const installation = installed.get(agent)
    if (installation === undefined) return
    for (let index = installation.disposers.length - 1; index >= 0; index--) {
      installation.disposers[index]?.()
    }
    installed.delete(agent)
  }

  function install(agent: Agent): void {
    if (installed.has(agent)) return
    const cwd = agent.session.header.cwd
    const canonicalCwd = cwd === undefined ? undefined : canonicalOrUndefined(cwd)
    if (canonicalCwd === undefined || !sameExecutionPath(canonicalCwd, resolved.root)) return
    const disposers: Array<() => void> = []
    try {
      const present = RESTRICT_CANDIDATES.filter(name => agent.ctx.tools.get(name) !== undefined)
      const replacements = replacementNames(present)
      if (replacements.length === 0 && !present.includes('pwsh') && !present.includes('bash_status')) return
      if (present.length > 0) disposers.push(agent.ctx.tools.restrict({ deny: [...present] }))
      for (const name of replacements) {
        const definition = definitions.get(name)
        if (definition === undefined) throw new Error(`dsh-agentshim: internal error: no definition for "${name}"`)
        disposers.push(agent.ctx.tools.register(definition))
      }
      for (const name of replacements) {
        const section = promptByName.get(`tool:${name}`)
        if (section !== undefined) {
          disposers.push(agent.ctx.systemPrompt.section({ name: section.name, order: section.order, text: section.text }))
        }
      }
      if (present.includes('pwsh')) {
        disposers.push(agent.ctx.systemPrompt.section({ name: 'tool:pwsh', order: 105, text: '' }))
      }
    } catch (error) {
      for (let index = disposers.length - 1; index >= 0; index--) {
        disposers[index]?.()
      }
      throw error
    }
    installed.set(agent, { disposers })
  }

  ctx.on('agent/created', ({ agent }) => {
    install(agent)
  })
  ctx.on('agent/disposed', ({ agent }) => {
    uninstall(agent)
  })
  ctx.effect(() => () => {
    for (const agent of installed.keys()) uninstall(agent)
  }, 'agentshim.agentInstallations')

  const agents = ctx.get('agents')
  if (agents === undefined) {
    ctx.logger.info('dsh-agentshim: no agents service at load; installing on agent/created events only')
    return
  }
  for (const agent of agents.list()) {
    install(agent)
  }
}

/**
 * Start the private agentshim MCP session, block activation on a validated
 * six-tool catalog, then replace overlapping tool names in every
 * root-matched agent scope. The catalog is still six tools; replacements and
 * the companions implied by them are published. Any resolution, spawn,
 * validation, or execution-world failure rejects the fiber (Cordis rolls the
 * plugin back) — the adapter never silently falls back to the DSH built-in
 * tools.
 */
export async function apply(ctx: Context, config: Config): Promise<void> {
  const captureRoot = resolveCaptureRoot(config.captureRoot ?? '')
  const captureMaxBytes = config.captureMaxBytes ?? DEFAULT_CAPTURE_MAX_BYTES
  const resolved = await resolveSessionConfig({ ...config, captureRoot, captureMaxBytes })
  await assertExecutionWorld(ctx, resolved.root)
  const captureStore = new CaptureStore(captureRoot, captureMaxBytes)
  ctx.effect(() => () => captureStore.dispose(), 'agentshim.captureStore')
  const session = createSession(resolved, { logger: ctx.logger, captureStore })
  ctx.effect(() => () => session.dispose(), 'agentshim.session')
  const jobs = new BackgroundJobManager()
  ctx.effect(() => () => jobs.dispose(), 'agentshim.jobs')
  const snapshot: CatalogSnapshot = await session.ready
  let native: NativeEngine | undefined
  const loaded = loadNativeAddon()
  if (loaded.engine !== undefined) {
    native = new loaded.engine.Engine(resolved.root, {
      env: nativeEngineEnv(config.env),
      timeoutCeilingMs: MIN_TOOL_CALL_TIMEOUT_MS,
    })
    ctx.effect(() => () => {
      void native?.close().catch(() => {})
    }, 'agentshim.nativeEngine')
  } else {
    ctx.logger.warn(`dsh-agentshim: native engine unavailable (${loaded.failure.reason}: ${loaded.failure.detail}); read/grep/glob stay on the MCP bridge`)
  }
  const definitions = buildToolDefinitions({ ctx, session, snapshot, config: resolved, jobs, captureStore, ...(native === undefined ? {} : { native }) })
  installAgentTools(ctx, resolved, definitions)
}
