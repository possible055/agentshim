import { resolve } from 'node:path'
import type { Context } from '@deepseek-ai/cordis'
import type { Agent } from '@deepseek-ai/dsh-agent'
import type { ToolDefinition } from '@deepseek-ai/dsh-tools'
import type {} from '@deepseek-ai/dsh-working-directory'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import z from '@deepseek-ai/schemastery'
import { MIN_TOOL_CALL_TIMEOUT_MS, resolvePluginConfig } from './config.ts'
import type { ResolvedPluginConfig } from './config.ts'
import { assertLocalFileSystem } from './policy.ts'
import { buildToolDefinitions, promptSections, pwshSectionOrder, RESTRICT_CANDIDATES } from './tools.ts'
import { PUBLIC_TOOL_NAMES } from './contracts.ts'
import { BackgroundJobManager } from './jobs.ts'
import { backgroundJobTimeoutMaxMs, loadNativeAddon, nativeEngineEnv, nativeLoadFailureError } from './native.ts'
import type { NativeEngine, NativeHostOptions } from './native.ts'
import { canonicalOrUndefined, EnginePool } from './engine-pool.ts'
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
  /** Extra environment merged on top of the scrubbed parent environment, e.g. `AGENTSHIM_*` settings. */
  env: Record<string, string>
  /** DSH tool deadline; the native process ceiling is derived from this shelf. */
  toolCallTimeoutMs: number
  /** Structured read capability; process sandbox policy remains a separate DSH service. */
  readScope?: 'normal' | 'unrestricted'
  /** Private persistent root for byte-exact process capture artifacts. */
  captureRoot?: string
  /** Aggregate raw capture ceiling for one process call. */
  captureMaxBytes?: number
  /** Artifact retention for the current Engine session. */
  captureCleanup?: 'never' | 'session-end'
}

export const Config = z.object({
  root: z.string().default(''),
  env: z.dict(String).default({}),
  toolCallTimeoutMs: z.number().min(MIN_TOOL_CALL_TIMEOUT_MS).default(MIN_TOOL_CALL_TIMEOUT_MS),
  readScope: z.union([z.const('normal'), z.const('unrestricted')]).default('unrestricted'),
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
  readonly roots: ReadonlySet<string>
}

/**
 * Install this adapter's contributions on every agent whose session has a
 * canonicalizable effective cwd: restrict only the inherited names that already exist,
 * register the matching agent-local definitions bound to a per-cwd native
 * engine, and shadow those prompt sections. The six-tool catalog stays
 * available; a composition that only mounted `bash` also gets its managed-job
 * `bash_status` companion after replacement.
 * Every step is one transaction — a failure rolls back and (for new agents)
 * vetoes publication. Agents without an effective cwd, or whose cwd cannot be
 * canonicalized or fails bash preflight, are left completely untouched.
 */
function installAgentTools(
  ctx: Context,
  resolved: ResolvedPluginConfig,
  pool: EnginePool,
  jobs: BackgroundJobManager,
): void {
  const installed = new Map<Agent, AgentInstallation>()
  const promptByName = new Map(promptSections(ctx).map(section => [section.name, section]))

  function uninstall(agent: Agent): void {
    const installation = installed.get(agent)
    if (installation === undefined) return
    for (let index = installation.disposers.length - 1; index >= 0; index--) {
      installation.disposers[index]?.()
    }
    for (const root of installation.roots) pool.release(root)
    installed.delete(agent)
  }

  function install(agent: Agent): void {
    if (installed.has(agent)) return
    const present = RESTRICT_CANDIDATES.filter(name => agent.ctx.tools.get(name, agent) !== undefined)
    const replacements = replacementNames(present)
    if (replacements.length === 0 && !present.includes('pwsh') && !present.includes('bash_status')) {
      ctx.logger.info(
        `dsh-agentshim: agent ${agent.id} resolves none of ${RESTRICT_CANDIDATES.join(', ')}; leaving its catalog untouched`,
      )
      return
    }
    const cwd = ctx.get('workingDirectory')?.get(agent.session) ?? agent.session.header.cwd
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
    const roots = new Set([root])
    try {
      const artifactRoots = new Map<string, string>()

      function buildDefinitions(native: NativeEngine, cwd: string): ReadonlyMap<string, ToolDefinition> {
        return buildToolDefinitions({
          ctx, config: resolved, jobs, native, cwd,
          onArtifacts(artifacts) {
            for (const { path } of artifacts) artifactRoots.set(canonicalOrUndefined(path) ?? path, cwd)
          },
        })
      }

      const definitions = buildDefinitions(engine, root)
      const definitionsByRoot = new Map([[root, definitions]])

      function definitionsFor(cwd: string): ReadonlyMap<string, ToolDefinition> {
        const cached = definitionsByRoot.get(cwd)
        if (cached !== undefined) return cached
        const acquired = pool.acquire(cwd)
        const canonical = definitionsByRoot.get(acquired.root)
        if (canonical !== undefined) {
          pool.release(acquired.root)
          return canonical
        }
        try {
          const next = buildDefinitions(acquired.engine, acquired.root)
          definitionsByRoot.set(acquired.root, next)
          // Keep prior engines alive for their running jobs and session artifacts.
          roots.add(acquired.root)
          return next
        } catch (error) {
          pool.release(acquired.root)
          throw error
        }
      }

      // `tools.get()` takes its viewing scope as an argument and falls back to
      // the GLOBAL layer when none is given. An agent preset registers the
      // model-facing tools into a standing mount that is this agent's scope
      // PARENT, so a scope-less read finds nothing and every preset-composed
      // agent would silently keep its inherited tools.
      if (present.length > 0) disposers.push(agent.ctx.tools.restrict({ deny: [...present] }))
      for (const name of replacements) {
        const definition = definitions.get(name)
        if (definition === undefined) throw new Error(`dsh-agentshim: internal error: no definition for "${name}"`)
        disposers.push(agent.ctx.tools.register({
          ...definition,
          async execute(args, exec) {
            if (name === 'bash_status') return definition.execute(args, exec)
            const directories = ctx.get('workingDirectory')
            const cwd = directories === undefined ? root : await directories.ensure(agent, exec.signal)
            if (!installed.has(agent)) {
              throw new HarnessError('dsh-agentshim: agent tools have been disposed', 'AGENTSHIM_ENGINE_CLOSED')
            }
            if ((name === 'read' || name === 'grep')
              && typeof args === 'object' && args !== null && 'path' in args && typeof args.path === 'string') {
              const path = resolve(cwd, args.path)
              const artifactRoot = artifactRoots.get(canonicalOrUndefined(path) ?? path)
              if (artifactRoot !== undefined) {
                // Only published artifacts return to their owning engine; ordinary paths use this call's cwd.
                return definitionsByRoot.get(artifactRoot)!.get(name)!.execute({ ...args, path }, exec)
              }
            }
            return definitionsFor(cwd).get(name)!.execute(args, exec)
          },
        }))
      }
      for (const name of replacements) {
        const section = promptByName.get(`tool:${name}`)
        if (section !== undefined) {
          disposers.push(agent.ctx.systemPrompt.section({ name: section.name, order: section.order, text: section.text }))
        }
      }
      if (present.includes('pwsh')) {
        disposers.push(agent.ctx.systemPrompt.section({ name: 'tool:pwsh', order: pwshSectionOrder(agent.ctx), text: '' }))
      }
    } catch (error) {
      for (let index = disposers.length - 1; index >= 0; index--) {
        disposers[index]?.()
      }
      pool.release(root)
      throw error
    }
    installed.set(agent, { disposers, roots })
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
  const engineEnv = nativeEngineEnv(config.env)
  const hostOptions: NativeHostOptions = {
    env: engineEnv,
    readScope: resolved.readScope,
    toolTimeoutShelfMs: resolved.toolCallTimeoutMs,
    backgroundJobTimeoutMaxMs: backgroundJobTimeoutMaxMs(engineEnv),
    captureRoot: resolved.captureRoot,
    captureMaxBytes: resolved.captureMaxBytes,
    captureCleanup: resolved.captureCleanup,
  }
  const nativeHost = new loaded.engine.NativeHostRuntime(hostOptions)
  const pool = new EnginePool(nativeHost, ({ root, phase, error }) => {
    ctx.logger.warn(
      `dsh-agentshim: native engine close failed for ${root} during ${phase}: ${error instanceof Error ? error.message : String(error)}`,
    )
  })
  ctx.effect(() => async () => {
    await jobs.dispose()
    await pool.dispose()
  }, 'agentshim.enginePool')
  installAgentTools(ctx, resolved, pool, jobs)
}
