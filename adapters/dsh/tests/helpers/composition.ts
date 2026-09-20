import { copyFile, mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { afterEach } from 'vitest'
import { Context, Service } from '@deepseek-ai/cordis'
import * as llm from '@deepseek-ai/dsh-llm'
const createCallId = (llm as { ToolCallId?: (id: string) => any; CallId?: (id: string) => any }).ToolCallId
  ?? (llm as { ToolCallId?: (id: string) => any; CallId?: (id: string) => any }).CallId
  ?? ((id: string) => id)
export const CallId = createCallId
import { bindScopeParent, createScope } from '@deepseek-ai/dsh-scope'
import type { Scope } from '@deepseek-ai/dsh-scope'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ToolRuntime from '@deepseek-ai/dsh-tools'
import type { ToolDefinition } from '@deepseek-ai/dsh-tools'
import LocalFileSystem from '@deepseek-ai/dsh-fs-local'
import type { Agent } from '@deepseek-ai/dsh-agent'
import { JobId } from '@deepseek-ai/dsh-jobs'
import type { JobSnapshot } from '@deepseek-ai/dsh-jobs'
import * as agentshim from '../../src/index.ts'
import type { Config } from '../../src/index.ts'

// Shared harness for the composition specs. Serialization constraints, previously
// implicit in `--maxWorkers=1` script flags, are explicit here:
// - the native addon is staged once per test file and published through
//   `process.env.AGENTSHIM_DSH_NATIVE_DLL` at module scope, so these specs must
//   never run with file parallelism (see `fileParallelism: false` in
//   vitest.config.ts);
// - plugin activation fails hard when the addon is not built — a missing addon
//   must fail the suite, never silently skip it.

const builtNativeDll = fileURLToPath(new URL(
  process.platform === 'win32'
    ? '../../../../target/debug/agentshim_napi.dll'
    : process.platform === 'darwin'
      ? '../../../../target/debug/libagentshim_napi.dylib'
      : '../../../../target/debug/libagentshim_napi.so',
  import.meta.url,
))
export const callSignal = new AbortController().signal
export const stagedNativeAddon = await (async (): Promise<string | undefined> => {
  try {
    const directory = await mkdtemp(join(tmpdir(), 'agentshim-composition-native-'))
    const staged = join(directory, 'agentshim_napi.node')
    await copyFile(builtNativeDll, staged)
    return staged
  } catch {
    return undefined
  }
})()

if (stagedNativeAddon === undefined) {
  throw new Error('native composition tests require `cargo build -p agentshim-napi` before pnpm test')
}
process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedNativeAddon

export const contexts: Context[] = []
export const pluginFibers: Array<{ dispose(): unknown }> = []
export const roots: string[] = []

export async function removeRoot(root: string): Promise<void> {
  await rm(root, { recursive: true, force: true })
}

export class UnconfinedShell extends Service {
  constructor(ctx: Context) {
    super(ctx, 'shell')
  }
}

afterEach(async () => {
  for (const fiber of pluginFibers.splice(0)) await fiber.dispose()
  contexts.splice(0)
  for (const root of roots.splice(0)) await removeRoot(root)
})

export function inheritedTool(name: string): ToolDefinition {
  return {
    name,
    description: `inherited ${name}`,
    parameters: { type: 'object', properties: {} },
    output: {
      schema: { type: 'string' },
      render: (_args, value) => [{ type: 'text', text: value as string }],
    },
    execute: () => Promise.resolve(`inherited:${name}`),
  }
}

export function registerInheritedTools(ctx: Context, names: readonly string[]): void {
  for (const name of names) ctx.tools.register(inheritedTool(name))
}

export async function makeRoot(): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), 'dsh-agentshim-comp-'))
  roots.push(root)
  return root
}

export async function mountComposition(
  root: string,
  configOverrides: Partial<Config> = {},
  beforeAdapter?: (ctx: Context) => Promise<void>,
): Promise<Context> {
  const ctx = new Context()
  contexts.push(ctx)
  await ctx.plugin(SystemPrompt, {})
  await ctx.plugin(ToolRuntime)
  await ctx.plugin(LocalFileSystem, { cwd: root })
  await beforeAdapter?.(ctx)
  if (ctx.get('shell') === undefined) await ctx.plugin(UnconfinedShell)
  const config: Config = {
    root,
    captureRoot: join(root, '.dsh-test-captures'),
    env: {
      FIXTURE_REPORT: join(root, 'report.json'),
      FIXTURE_BOOT_FILE: join(root, 'boot.txt'),
      FIXTURE_EXIT_FILE: join(root, 'exit.txt'),
    },
    toolCallTimeoutMs: 600_000,
    ...configOverrides,
  }
  pluginFibers.push(await ctx.plugin(agentshim, config))
  return ctx
}

export interface MintedAgent {
  readonly agent: Agent
  readonly scope: Scope
}

export async function mintAgent(ctx: Context, name: string, cwd: string): Promise<MintedAgent> {
  let scope!: Scope
  const agent = {
    id: name,
    session: { header: { cwd }, requestHeader: () => ({ config: {} }) },
    options: { provider: 'stub-provider', model: 'stub-model' },
  } as unknown as Agent
  await ctx.plugin(Object.assign((inner: Context) => {
    scope = createScope(inner, agent)
  }, { inject: ['tools', 'systemPrompt'] }))
  ;(agent as { ctx?: unknown }).ctx = scope.ctx
  return { agent, scope }
}

export async function mintStandardAgent(ctx: Context, name: string, cwd: string): Promise<MintedAgent> {
  registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash'])
  const minted = await mintAgent(ctx, name, cwd)
  ctx.emit('agent/created', { agent: minted.agent, source: 'startup' })
  return minted
}

export interface MintedPresetAgent extends MintedAgent {
  readonly standing: Scope
}

/**
 * Mint an agent in the DSH Web shape: the model-facing tools sit in a standing
 * preset mount that is the agent's scope PARENT, not in the host's global
 * layer. `mintAgent` above builds the TUI/headless shape instead, where those
 * tools are global — a registry read that forgets its viewing scope still
 * resolves them there, so only this topology exercises the scoped lookup.
 */
export async function mintPresetAgent(
  ctx: Context,
  name: string,
  cwd: string,
  presetTools: readonly string[],
): Promise<MintedPresetAgent> {
  const agent = {
    id: name,
    session: { header: { cwd }, requestHeader: () => ({ config: {} }) },
    options: { provider: 'stub-provider', model: 'stub-model' },
  } as unknown as Agent
  const presetKey = { agentPreset: name }
  let standing!: Scope
  let scope!: Scope
  await ctx.plugin(Object.assign((inner: Context) => {
    standing = createScope(inner, presetKey)
    for (const tool of presetTools) standing.ctx.tools.register(inheritedTool(tool))
    scope = createScope(inner, agent)
    bindScopeParent(agent, presetKey)
  }, { inject: ['tools', 'systemPrompt'] }))
  ;(agent as { ctx?: unknown }).ctx = scope.ctx
  return { agent, scope, standing }
}

export async function waitForBackgroundOutput(ctx: Context, agent: Agent, jobId: string): Promise<void> {
  const deadline = Date.now() + 5_000
  for (;;) {
    if (ctx.jobs.read(JobId(jobId), agent).text.includes('background output')) return
    if (Date.now() >= deadline) throw new Error(`background output did not arrive for ${jobId}`)
    await new Promise(resolve => setTimeout(resolve, 10))
  }
}

export async function waitForJobTerminal(
  ctx: Context,
  agent: Agent,
  jobId: string,
  timeoutMs = 10_000,
): Promise<JobSnapshot> {
  const startedAt = performance.now()
  let snapshot = ctx.jobs.get(JobId(jobId), agent)
  while (snapshot.status === 'running' || snapshot.status === 'stopping') {
    const elapsedMs = performance.now() - startedAt
    const remainingMs = Math.ceil(timeoutMs - elapsedMs)
    if (remainingMs <= 0) break
    snapshot = await ctx.jobs.wait(JobId(jobId), remainingMs, agent)
  }
  if (snapshot.status !== 'running' && snapshot.status !== 'stopping') return snapshot

  const elapsedMs = Math.round(performance.now() - startedAt)
  const outputTail = ctx.jobs.read(JobId(jobId), agent).text.slice(-512)
  throw new Error(
    `job ${jobId} did not settle within ${timeoutMs}ms; elapsed=${elapsedMs}ms; snapshot=${JSON.stringify(snapshot)}; outputTail=${JSON.stringify(outputTail)}`,
  )
}

export function visibleNames(ctx: Context, agent: Agent): string[] {
  return ctx.tools.schemas(agent).map(schema => schema.name).sort()
}

export async function runTool(ctx: Context, agent: Agent, name: string, args: Record<string, unknown>): Promise<string> {
  const result = await ctx.tools.execute({
    signal: callSignal,
    callId: CallId('c1'),
    name,
    arguments: args,
    agent,
  })
  const first = result.content[0]
  return first !== undefined && first.type === 'text' ? first.text : JSON.stringify(result.content)
}
