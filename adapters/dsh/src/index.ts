import type { Context } from '@deepseek-ai/cordis'
import type { Agent } from '@deepseek-ai/dsh-agent'
import type { ToolDefinition } from '@deepseek-ai/dsh-tools'
import z from '@deepseek-ai/schemastery'
import { MIN_TOOL_CALL_TIMEOUT_MS, resolvePluginConfig } from './config.ts'
import type { ResolvedPluginConfig } from './config.ts'
import { assertLocalFileSystem } from './policy.ts'
import { buildToolDefinitions, promptSections, RESTRICT_CANDIDATES } from './tools.ts'
import { PUBLIC_TOOL_NAMES } from './contracts.ts'
import { BackgroundJobManager } from './jobs.ts'
import { loadNativeAddon, nativeEngineEnv, nativeLoadFailureError } from './native.ts'
import type { NativeEngine, NativeEngineOptions } from './native.ts'
import { EnginePool } from './engine-pool.ts'
import {
  DEFAULT_CAPTURE_MAX_BYTES,
  MAX_CAPTURE_MAX_BYTES,
  MIN_CAPTURE_MAX_BYTES,
} from './capture.ts'

/** Cordis plugin name used by loader diagnostics. */
export const name = 'agentshim'

export const inject: readonly string[] = ['fs']

export interface Config {
  /** Plugin working root; defaults to the host's `process.cwd()`. */
  root: string
  readScope: 'normal' | 'unrestricted'
  /** Extra environment merged on top of the scrubbed parent environment, e.g. `AGENTSHIM_*` settings. */
  env: Record<string, string>
  /** DSH tool deadline; the native process ceiling is derived from this shelf. */
  toolCallTimeoutMs: number
  /** Private persistent root for byte-exact process capture artifacts. */
  captureRoot?: string
  /** Aggregate raw capture ceiling for one process call. */
  captureMaxBytes?: number
  /** Artifact retention for the current Engine session. */
  captureCleanup?: 'never' | 'session-end'
}

export const Config = z.object({
  root: z.string().default(''),
  readScope: z.union([z.const('normal'), z.const('unrestricted')]).default('normal'),
  env: z.dict(String).default({}),
  toolCallTimeoutMs: z.number().min(MIN_TOOL_CALL_TIMEOUT_MS).default(MIN_TOOL_CALL_TIMEOUT_MS),
  captureRoot: z.string().default(''),
  captureMaxBytes: z.number().min(MIN_CAPTURE_MAX_BYTES).max(MAX_CAPTURE_MAX_BYTES).default(DEFAULT_CAPTURE_MAX_BYTES),
  captureCleanup: z.union([z.const('never'), z.const('session-end')]).default('never'),
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

interface AgentInstallation {
  readonly disposers: ReadonlyArray<() => void>
  readonly root: string
}

/**
 * Install this adapter's contributions on every agent whose session carries a
 * canonicalizable cwd: restrict only the inherited names that already exist,
 * register the matching agent-local definitions bound to a per-cwd native
 * engine, and shadow those prompt sections. The six-tool catalog stays
 * available; a composition that only mounted `bash` also gets its managed-job
 * `bash_status` companion after replacement.
 * Every step is one transaction — a failure rolls back and (for new agents)
 * vetoes publication. Agents without a session cwd, or whose cwd cannot be
 * canonicalized or fails bash preflight, are left completely untouched.
 */
function installAgentTools(
  ctx: Context,
  resolved: ResolvedPluginConfig,
  pool: EnginePool,
  jobs: BackgroundJobManager,
): void {
  const installed = new Map<Agent, AgentInstallation>()
  const promptByName = new Map(promptSections().map(section => [section.name, section]))

  function uninstall(agent: Agent): void {
    const installation = installed.get(agent)
    if (installation === undefined) return
    for (let index = installation.disposers.length - 1; index >= 0; index--) {
      installation.disposers[index]?.()
    }
    pool.release(installation.root)
    installed.delete(agent)
  }

  function install(agent: Agent): void {
    if (installed.has(agent)) return
    const cwd = agent.session.header.cwd
    if (cwd === undefined) return

    let acquired: { readonly engine: NativeEngine; readonly root: string }
    try {
      acquired = pool.acquire(cwd)
    } catch (error) {
      ctx.logger.warn(
        `dsh-agentshim: skipping agent ${agent.id}: ${error instanceof Error ? error.message : String(error)}`,
      )
      return
    }
    const { engine, root } = acquired

    const disposers: Array<() => void> = []
    try {
      const definitions: ReadonlyMap<string, ToolDefinition> = buildToolDefinitions({ ctx, config: resolved, jobs, native: engine, root })
      // `tools.get()` takes its viewing scope as an argument and falls back to
      // the GLOBAL layer when none is given. An agent preset registers the
      // model-facing tools into a standing mount that is this agent's scope
      // PARENT, so a scope-less read finds nothing and every preset-composed
      // agent would silently keep its inherited tools.
      const present = RESTRICT_CANDIDATES.filter(name => agent.ctx.tools.get(name, agent) !== undefined)
      const replacements = replacementNames(present)
      if (replacements.length === 0 && !present.includes('pwsh') && !present.includes('bash_status')) {
        ctx.logger.info(
          `dsh-agentshim: agent ${agent.id} resolves none of ${RESTRICT_CANDIDATES.join(', ')}; leaving its catalog untouched`,
        )
        pool.release(root)
        return
      }
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
      pool.release(root)
      throw error
    }
    installed.set(agent, { disposers, root })
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

/** Load the native engine pool and replace overlapping tools in every session-cwd-matched agent scope. */
export async function apply(ctx: Context, config: Config): Promise<void> {
  const resolved = await resolvePluginConfig(config)
  assertLocalFileSystem(ctx)
  const jobs = new BackgroundJobManager()
  const loaded = loadNativeAddon()
  if (loaded.engine === undefined) {
    throw nativeLoadFailureError(loaded.failure)
  }
  const engineOptions: NativeEngineOptions = {
    env: nativeEngineEnv(config.env),
    readScope: resolved.readScope,
    pageBudgetBytes: 50_000,
    toolTimeoutShelfMs: resolved.toolCallTimeoutMs,
    captureRoot: resolved.captureRoot,
    captureMaxBytes: resolved.captureMaxBytes,
    captureCleanup: resolved.captureCleanup,
  }
  const pool = new EnginePool(loaded.engine.Engine, engineOptions)
  ctx.effect(() => async () => {
    await jobs.dispose()
    await pool.dispose()
  }, 'agentshim.enginePool')
  installAgentTools(ctx, resolved, pool, jobs)
}
