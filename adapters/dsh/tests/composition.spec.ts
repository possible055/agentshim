import { chmod, copyFile, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { Context, Service } from '@deepseek-ai/cordis'
import { CallId } from '@deepseek-ai/dsh-llm'
import { bindScopeParent, createScope } from '@deepseek-ai/dsh-scope'
import type { Scope } from '@deepseek-ai/dsh-scope'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ToolRuntime, { TOOL_ABORTED, renderToolsSdk, renderToolsSdkPy } from '@deepseek-ai/dsh-tools'
import type { ToolDefinition } from '@deepseek-ai/dsh-tools'
import LocalFileSystem from '@deepseek-ai/dsh-fs-local'
import * as ObservationPolicy from '@deepseek-ai/dsh-fs-observation-policy'
import LocalAttachmentStore from '@deepseek-ai/dsh-attachment-local'
import type { Agent } from '@deepseek-ai/dsh-agent'
import { JobId } from '@deepseek-ai/dsh-jobs'
import LocalJobRegistry from '@deepseek-ai/dsh-jobs-local'
import * as agentshim from '../src/index.ts'
import type { Config } from '../src/index.ts'
import { PUBLIC_TOOL_NAMES } from '../src/contracts.ts'

const builtNativeDll = fileURLToPath(new URL(
  process.platform === 'win32'
    ? '../../../target/debug/agentshim_napi.dll'
    : process.platform === 'darwin'
      ? '../../../target/debug/libagentshim_napi.dylib'
      : '../../../target/debug/libagentshim_napi.so',
  import.meta.url,
))
const samplePdf = fileURLToPath(new URL('./fixtures/sample.pdf', import.meta.url))
const callSignal = new AbortController().signal
const sharedConstraints = JSON.parse(await readFile(
  fileURLToPath(new URL('../../../evals/host-constraints.json', import.meta.url)),
  'utf8',
)) as {
  readonly cases: ReadonlyArray<{
    readonly id: string
    readonly tool: string
    readonly args: Record<string, unknown>
  }>
}

const stagedNativeAddon = await (async (): Promise<string | undefined> => {
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

const contexts: Context[] = []
const pluginFibers: Array<{ dispose(): unknown }> = []
const roots: string[] = []

async function removeRoot(root: string): Promise<void> {
  await rm(root, { recursive: true, force: true })
}

class UnconfinedShell extends Service {
  constructor(ctx: Context) {
    super(ctx, 'shell')
  }
}

afterEach(async () => {
  for (const fiber of pluginFibers.splice(0)) await fiber.dispose()
  contexts.splice(0)
  for (const root of roots.splice(0)) await removeRoot(root)
})

function inheritedTool(name: string): ToolDefinition {
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

function registerInheritedTools(ctx: Context, names: readonly string[]): void {
  for (const name of names) ctx.tools.register(inheritedTool(name))
}

async function makeRoot(): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), 'dsh-agentshim-comp-'))
  roots.push(root)
  return root
}

async function mountComposition(
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

interface MintedAgent {
  readonly agent: Agent
  readonly scope: Scope
}

async function mintAgent(ctx: Context, name: string, cwd: string): Promise<MintedAgent> {
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

async function mintStandardAgent(ctx: Context, name: string, cwd: string): Promise<MintedAgent> {
  registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash'])
  const minted = await mintAgent(ctx, name, cwd)
  ctx.emit('agent/created', { agent: minted.agent })
  return minted
}

interface MintedPresetAgent extends MintedAgent {
  readonly standing: Scope
}

/**
 * Mint an agent in the DSH Web shape: the model-facing tools sit in a standing
 * preset mount that is the agent's scope PARENT, not in the host's global
 * layer. `mintAgent` above builds the TUI/headless shape instead, where those
 * tools are global — a registry read that forgets its viewing scope still
 * resolves them there, so only this topology exercises the scoped lookup.
 */
async function mintPresetAgent(
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

async function waitForBackgroundOutput(ctx: Context, agent: Agent, jobId: string): Promise<void> {
  const deadline = Date.now() + 5_000
  for (;;) {
    if (ctx.jobs.read(JobId(jobId), agent).text.includes('background output')) return
    if (Date.now() >= deadline) throw new Error(`background output did not arrive for ${jobId}`)
    await new Promise(resolve => setTimeout(resolve, 10))
  }
}

function visibleNames(ctx: Context, agent: Agent): string[] {
  return ctx.tools.schemas(agent).map(schema => schema.name).sort()
}

async function runTool(ctx: Context, agent: Agent, name: string, args: Record<string, unknown>): Promise<string> {
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

describe('agent scope replacement', () => {
  it('replaces the six tools for a root-matched agent, hides pwsh, keeps the rest', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash', 'pwsh', 'write', 'edit', 'read_image', 'todo'])
    const { agent } = await mintAgent(ctx, 'a1', root)
    ctx.emit('agent/created', { agent })

    const names = visibleNames(ctx, agent)
    expect(names).toContain('read')
    expect(names).toContain('grep')
    expect(names).toContain('glob')
    expect(names).toContain('run_program')
    expect(names).toContain('bash')
    expect(names).toContain('bash_status')
    expect(names).toContain('write')
    expect(names).toContain('edit')
    expect(names).toContain('read_image')
    expect(names).toContain('todo')
    expect(names).not.toContain('pwsh')
    expect(names.filter(name => name.startsWith('mcp__'))).toEqual([])

    const replaced = await runTool(ctx, agent, 'bash', { command: 'true', description: 'Run successful command' })
    expect(replaced).toContain('Exit code: 0')
    const read = ctx.tools.schemas(agent).find(schema => schema.name === 'read')
    expect(read?.description).toContain('numbered lines')
  })

  it('emits fully typed TypeScript and Python Code Mode contracts for all six tools', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    const { agent } = await mintStandardAgent(ctx, 'code-contracts', root)
    const schemas = PUBLIC_TOOL_NAMES.map(name => {
      const definition = ctx.tools.get(name, agent)
      expect(definition, name).toBeDefined()
      return {
        name,
        description: definition!.description,
        parameters: definition!.parameters,
        output: definition!.output.schema,
      }
    })

    const typescript = renderToolsSdk(schemas)
    const python = renderToolsSdkPy(schemas)
    for (const name of PUBLIC_TOOL_NAMES) {
      expect(typescript).toContain(`${name}: {`)
      expect(python).toContain(`async def ${name}`)
    }
    expect(typescript).not.toContain(': unknown')
    expect(python).not.toContain(': Any')
    expect(python).not.toContain('-> Any')
  })

  it('rejects shared and adapter-only argument violations through production validation', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    const { agent } = await mintStandardAgent(ctx, 'invalid-args', root)
    const adapterOnlyCases: ReadonlyArray<{ readonly id: string; readonly tool: string; readonly args: Record<string, unknown> }> = [
      { id: 'read-path-type', tool: 'read', args: { path: 7 } },
      { id: 'bash-enum', tool: 'bash', args: { command: 'true', description: 'Run a command', msys_argument_conversion: 'invalid' } },
      { id: 'run-program-stdin-type', tool: 'run_program', args: { program: 'node', stdin: 7 } },
      { id: 'bash-non-empty-command', tool: 'bash', args: { command: ' ', description: 'Run a command' } },
      { id: 'run-program-env-type', tool: 'run_program', args: { program: 'node', env: { INVALID: 7 } } },
    ]

    for (const { id, tool, args } of [...sharedConstraints.cases, ...adapterOnlyCases]) {
      const result = await ctx.tools.execute({
        signal: callSignal,
        callId: CallId(`invalid-${id}`),
        name: tool,
        arguments: args,
        agent,
      })
      expect(result.isError, `${id}: ${JSON.stringify(args)}`).toBe(true)
      expect(result.error?.info).toMatchObject({ code: 'INVALID_ARGS' })
    }
  })

  it('shadows the inherited prompt sections for the replaced tools', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    ctx.systemPrompt.section({ name: 'tool:read', order: 100, text: 'INHERITED-READ-GUIDANCE' })
    ctx.systemPrompt.section({ name: 'tool:pwsh', order: 105, text: 'INHERITED-PWSH-GUIDANCE' })
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash', 'pwsh', 'write', 'edit'])
    const { agent } = await mintAgent(ctx, 'a2', root)
    ctx.emit('agent/created', { agent })

    const assembly = await ctx.systemPrompt.assemble({ scope: agent })
    const prompt = JSON.stringify(assembly)
    expect(prompt).toContain('next_start_line')
    expect(prompt).toContain('next_offset')
    expect(prompt).not.toContain('INHERITED-READ-GUIDANCE')
    expect(prompt).not.toContain('INHERITED-PWSH-GUIDANCE')
    expect(prompt).toContain('run_in_background=true')
    expect(prompt).toContain('lifecycle status of a background Bash job')
  })

  it('adds bash_status beside bash on a minimal catalog and leaves the editor', async () => {
    const root = await makeRoot()
    const ctx = new Context()
    contexts.push(ctx)
    await ctx.plugin(SystemPrompt, {})
    await ctx.plugin(ToolRuntime)
    await ctx.plugin(LocalFileSystem, { cwd: root })
    registerInheritedTools(ctx, ['bash', 'str_replace_editor'])
    pluginFibers.push(await ctx.plugin(agentshim, {
      root,
      captureRoot: join(root, '.dsh-test-captures'),
      env: { FIXTURE_REPORT: join(root, 'report.json'), FIXTURE_BOOT_FILE: join(root, 'boot.txt') },
      toolCallTimeoutMs: 600_000,
    }))
    const { agent } = await mintAgent(ctx, 'minimal', root)
    ctx.emit('agent/created', { agent })

    expect(visibleNames(ctx, agent)).toEqual(['bash', 'bash_status', 'str_replace_editor'])
    const bash = ctx.tools.get('bash', agent)
    expect(bash?.description).toContain('POSIX')
    const properties = (bash!.parameters as { properties: Record<string, unknown> }).properties
    expect(properties).toHaveProperty('run_in_background')
    expect(properties).not.toHaveProperty('detach')
    expect(properties).not.toHaveProperty('sandbox_permissions')
    expect(await runTool(ctx, agent, 'bash', { command: 'true', description: 'Run successful command' }))
      .toContain('Exit code: 0')
    expect(await runTool(ctx, agent, 'str_replace_editor', { command: 'view', path: 'notes.txt' }))
      .toBe('inherited:str_replace_editor')

    const assembly = await ctx.systemPrompt.assemble({ scope: agent })
    const prompt = JSON.stringify(assembly)
    expect(prompt).toContain('run_in_background=true')
    expect(prompt).toContain('lifecycle status of a background Bash job')
    expect(prompt).not.toContain('next_start_line')
    expect(prompt).not.toContain('Prefer run_program')
  })

  it('hides an isolated inherited bash_status when bash is unavailable', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    registerInheritedTools(ctx, ['bash_status', 'str_replace_editor'])
    const { agent } = await mintAgent(ctx, 'status-without-bash', root)
    ctx.emit('agent/created', { agent })

    expect(visibleNames(ctx, agent)).toEqual(['str_replace_editor'])
    const assembly = await ctx.systemPrompt.assemble({ scope: agent })
    expect(JSON.stringify(assembly)).not.toContain('lifecycle status of a background Bash job')
  })

  it('installs agents whose cwd differs from the plugin root using a per-cwd engine', async () => {
    const root = await makeRoot()
    const elsewhere = await makeRoot()
    const ctx = await mountComposition(root)
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash', 'write', 'edit'])
    const matching = await mintAgent(ctx, 'match', root)
    const other = await mintAgent(ctx, 'other', elsewhere)
    ctx.emit('agent/created', { agent: matching.agent })
    ctx.emit('agent/created', { agent: other.agent })

    expect(visibleNames(ctx, other.agent)).toContain('run_program')
    expect(visibleNames(ctx, matching.agent)).toContain('run_program')
    await writeFile(join(elsewhere, 'file.txt'), 'from elsewhere')
    const read = await runTool(ctx, other.agent, 'read', { path: 'file.txt' })
    expect(read).toContain('from elsewhere')
  })

  it('rolls back the whole installation when one registration conflicts and vetoes publication', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash', 'pwsh', 'write'])
    const { agent } = await mintAgent(ctx, 'a3', root)
    agent.ctx.tools.register(inheritedTool('bash'))
    expect(() => ctx.emit('agent/created', { agent })).toThrow(/duplicate|already/i)

    const names = visibleNames(ctx, agent)
    expect(names).not.toContain('run_program')
    expect(await runTool(ctx, agent, 'read', {})).toBe('inherited:read')
    expect(names).toContain('bash')
  })

  it('removes its contributions when the agent is disposed', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash', 'pwsh', 'write'])
    const { agent } = await mintAgent(ctx, 'a4', root)
    ctx.emit('agent/created', { agent })
    expect(visibleNames(ctx, agent)).toContain('run_program')

    ctx.emit('agent/disposed', { agent })
    const names = visibleNames(ctx, agent)
    expect(names).not.toContain('run_program')
    expect(await runTool(ctx, agent, 'read', {})).toBe('inherited:read')
  })

  it('closes the native engine before plugin teardown completes', async () => {
    const root = await makeRoot()
    await mountComposition(root)
    const fiber = pluginFibers.at(-1)
    expect(fiber).toBeDefined()
    await fiber!.dispose()
    await removeRoot(root)
    roots.splice(roots.indexOf(root), 1)
  })

  it('installs onto existing agents when loaded after them', async () => {
    const root = await makeRoot()
    const ctx = new Context()
    contexts.push(ctx)
    await ctx.plugin(SystemPrompt, {})
    await ctx.plugin(ToolRuntime)
    await ctx.plugin(LocalFileSystem, { cwd: root })
    await ctx.plugin(UnconfinedShell)
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash', 'pwsh', 'write'])

    const first = await mintAgent(ctx, 'existing', root)
    const agents = [first.agent]
    class StubAgentRegistry extends Service {
      constructor(ctx: Context) {
        super(ctx, 'agents')
      }
      list(): Agent[] {
        return agents
      }
    }
    await ctx.plugin(StubAgentRegistry)

    pluginFibers.push(await ctx.plugin(agentshim, {
      root,
      captureRoot: join(root, '.dsh-test-captures'),
      env: { FIXTURE_REPORT: join(root, 'report.json'), FIXTURE_BOOT_FILE: join(root, 'boot.txt') },
      toolCallTimeoutMs: 600_000,
    }))
    expect(visibleNames(ctx, first.agent)).toContain('run_program')
    expect(visibleNames(ctx, first.agent)).not.toContain('pwsh')
  })

  it('fails activation and rolls back every existing-agent installation on a conflict', async () => {
    const root = await makeRoot()
    const ctx = new Context()
    contexts.push(ctx)
    await ctx.plugin(SystemPrompt, {})
    await ctx.plugin(ToolRuntime)
    await ctx.plugin(LocalFileSystem, { cwd: root })
    await ctx.plugin(UnconfinedShell)
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash', 'pwsh', 'write'])
    const first = await mintAgent(ctx, 'existing-ok', root)
    const conflicting = await mintAgent(ctx, 'existing-conflict', root)
    conflicting.agent.ctx.tools.register(inheritedTool('bash'))
    const agents = [first.agent, conflicting.agent]
    class StubAgentRegistry extends Service {
      constructor(inner: Context) {
        super(inner, 'agents')
      }
      list(): Agent[] {
        return agents
      }
    }
    await ctx.plugin(StubAgentRegistry)

    await expect(ctx.plugin(agentshim, {
      root,
      captureRoot: join(root, '.dsh-test-captures'),
      env: {
        FIXTURE_REPORT: join(root, 'report.json'),
        FIXTURE_BOOT_FILE: join(root, 'boot.txt'),
        FIXTURE_EXIT_FILE: join(root, 'exit.txt'),
      },
      toolCallTimeoutMs: 600_000,
    })).rejects.toThrow(/duplicate|already/i)

    expect(visibleNames(ctx, first.agent)).not.toContain('run_program')
    expect(await runTool(ctx, first.agent, 'read', {})).toBe('inherited:read')
  })

  it('activates without a shell executor and fails closed if confinement appears later', async () => {
    const root = await makeRoot()
    const ctx = new Context()
    contexts.push(ctx)
    await ctx.plugin(SystemPrompt, {})
    await ctx.plugin(ToolRuntime)
    await ctx.plugin(LocalFileSystem, { cwd: root })
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash', 'pwsh', 'write'])
    const existing = await mintAgent(ctx, 'hmr-agent', root)
    class StubAgentRegistry extends Service {
      constructor(inner: Context) {
        super(inner, 'agents')
      }
      list(): Agent[] {
        return [existing.agent]
      }
    }
    await ctx.plugin(StubAgentRegistry)
    pluginFibers.push(await ctx.plugin(agentshim, {
      root,
      captureRoot: join(root, '.dsh-test-captures'),
      env: {
        FIXTURE_REPORT: join(root, 'report.json'),
        FIXTURE_BOOT_FILE: join(root, 'boot.txt'),
        FIXTURE_EXIT_FILE: join(root, 'exit.txt'),
      },
      toolCallTimeoutMs: 600_000,
    }))
    const unconfinedBash = ctx.tools.get('bash', existing.agent)
    expect(unconfinedBash).toBeDefined()
    const unconfinedProperties = (unconfinedBash!.parameters as { properties: Record<string, unknown> }).properties
    expect(unconfinedProperties).not.toHaveProperty('sandbox_permissions')
    expect(await runTool(ctx, existing.agent, 'bash', { command: 'true', description: 'Run successful command' }))
      .toContain('Exit code: 0')

    await ctx.plugin(class extends Service {
      constructor(inner: Context) {
        super(inner, 'sandboxPolicy')
      }
      resolve(): { mode: 'workspace-write' } {
        return { mode: 'workspace-write' }
      }
    })
    await ctx.plugin(class extends Service {
      readonly sandboxMode = 'workspace-write'
      constructor(inner: Context) {
        super(inner, 'shell')
      }
    })
    const denied = await ctx.tools.execute({
      signal: callSignal,
      callId: CallId('hmr-denied'),
      name: 'bash',
      arguments: { command: 'true', description: 'Run command after policy change' },
      agent: existing.agent,
    })
    expect(denied.error?.info).toMatchObject({ code: 'AGENTSHIM_PROCESS_POLICY_CHANGED' })
  })

  it('fails loud at load when ctx.fs is not a local filesystem provider', async () => {
    const root = await makeRoot()
    const ctx = new Context()
    contexts.push(ctx)
    await ctx.plugin(SystemPrompt, {})
    await ctx.plugin(ToolRuntime)
    await ctx.plugin(UnconfinedShell)
    class RemoteFs extends Service {
      constructor(ctx: Context) {
        super(ctx, 'fs')
      }
    }
    await ctx.plugin(RemoteFs)
    await expect(ctx.plugin(agentshim, {
      root,
      captureRoot: join(root, '.dsh-test-captures'),
      env: {},
      toolCallTimeoutMs: 600_000,
    })).rejects.toThrow(/local filesystem provider/)
  })
})

describe('preset-scoped catalog (web surface topology)', () => {
  it('replaces the preset-scoped tools for a standard-shaped preset and hides pwsh', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    const { agent } = await mintPresetAgent(ctx, 'standard', root, [
      'read', 'grep', 'glob', 'pwsh', 'write', 'edit', 'read_image', 'todo',
    ])
    ctx.emit('agent/created', { agent })

    expect(visibleNames(ctx, agent)).toEqual([
      'bash', 'bash_status', 'edit', 'glob', 'grep', 'read', 'read_image', 'run_program', 'todo', 'write',
    ])
    expect(ctx.tools.get('read', agent)?.description).toContain('numbered lines')
    expect(ctx.tools.get('write', agent)?.description).toBe('inherited write')
    expect(await runTool(ctx, agent, 'bash', { command: 'true', description: 'Run successful command' }))
      .toContain('Exit code: 0')
    expect(await runTool(ctx, agent, 'todo', {})).toBe('inherited:todo')
  })

  it('replaces only bash on a minimal-shaped preset', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    const { agent } = await mintPresetAgent(ctx, 'minimal', root, ['bash', 'str_replace_editor'])
    ctx.emit('agent/created', { agent })

    expect(visibleNames(ctx, agent)).toEqual(['bash', 'bash_status', 'str_replace_editor'])
    expect(ctx.tools.get('bash', agent)?.description).toContain('POSIX')
    expect(await runTool(ctx, agent, 'str_replace_editor', { command: 'view', path: 'notes.txt' }))
      .toBe('inherited:str_replace_editor')
  })

  it('shadows prompt sections the preset registered in the standing scope', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    const { agent, standing } = await mintPresetAgent(ctx, 'standard-prompt', root, [
      'read', 'grep', 'glob', 'pwsh',
    ])
    standing.ctx.systemPrompt.section({ name: 'tool:read', order: 100, text: 'PRESET-READ-GUIDANCE' })
    standing.ctx.systemPrompt.section({ name: 'tool:pwsh', order: 105, text: 'PRESET-PWSH-GUIDANCE' })
    ctx.emit('agent/created', { agent })

    const prompt = JSON.stringify(await ctx.systemPrompt.assemble({ scope: agent }))
    expect(prompt).not.toContain('PRESET-READ-GUIDANCE')
    expect(prompt).not.toContain('PRESET-PWSH-GUIDANCE')
    expect(prompt).toContain('next_start_line')
    expect(prompt).toContain('lifecycle status of a background Bash job')
  })

  it('hides an isolated preset-scoped bash_status when bash is unavailable', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    const { agent } = await mintPresetAgent(ctx, 'status-only', root, ['bash_status', 'str_replace_editor'])
    ctx.emit('agent/created', { agent })

    expect(visibleNames(ctx, agent)).toEqual(['str_replace_editor'])
    expect(JSON.stringify(await ctx.systemPrompt.assemble({ scope: agent })))
      .not.toContain('lifecycle status of a background Bash job')
  })

  it('leaves a preset carrying no replaceable name untouched', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    const warn = vi.spyOn(ctx.logger, 'warn')
    const missing = join(tmpdir(), `agentshim-no-overlap-${Math.random()}`)
    const { agent } = await mintPresetAgent(ctx, 'no-overlap', missing, ['todo', 'write'])
    ctx.emit('agent/created', { agent })

    expect(visibleNames(ctx, agent)).toEqual(['todo', 'write'])
    expect(warn).not.toHaveBeenCalled()
  })
})

describe('multi-workspace engine pool', () => {
  it('shares one engine between two agents on the same cwd', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash'])
    const a = await mintAgent(ctx, 'shared-a', root)
    const b = await mintAgent(ctx, 'shared-b', root)
    ctx.emit('agent/created', { agent: a.agent })
    ctx.emit('agent/created', { agent: b.agent })

    expect(visibleNames(ctx, a.agent)).toContain('run_program')
    expect(visibleNames(ctx, b.agent)).toContain('run_program')

    ctx.emit('agent/disposed', { agent: a.agent })
    expect(visibleNames(ctx, b.agent)).toContain('run_program')
    const read = await runTool(ctx, b.agent, 'bash', { command: 'true', description: 'still alive' })
    expect(read).toContain('Exit code: 0')
  })

  it('skips an agent whose cwd does not exist and leaves its inherited tools intact', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash'])
    const { agent } = await mintAgent(ctx, 'bad-cwd', join(tmpdir(), 'agentshim-nonexistent-' + Math.random()))
    ctx.emit('agent/created', { agent })

    expect(visibleNames(ctx, agent)).not.toContain('run_program')
    expect(await runTool(ctx, agent, 'read', {})).toBe('inherited:read')
  })

  it('installs agents on two distinct cwds and resolves relative paths independently', async () => {
    const rootA = await makeRoot()
    const rootB = await makeRoot()
    const ctx = await mountComposition(rootA)
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash'])
    await writeFile(join(rootA, 'marker.txt'), 'workspace A')
    await writeFile(join(rootB, 'marker.txt'), 'workspace B')
    const agentA = await mintAgent(ctx, 'ws-a', rootA)
    const agentB = await mintAgent(ctx, 'ws-b', rootB)
    ctx.emit('agent/created', { agent: agentA.agent })
    ctx.emit('agent/created', { agent: agentB.agent })

    expect(visibleNames(ctx, agentA.agent)).toContain('run_program')
    expect(visibleNames(ctx, agentB.agent)).toContain('run_program')
    const readA = await runTool(ctx, agentA.agent, 'read', { path: 'marker.txt' })
    const readB = await runTool(ctx, agentB.agent, 'read', { path: 'marker.txt' })
    expect(readA).toContain('workspace A')
    expect(readB).toContain('workspace B')
  })
})

describe('DSH native contracts', () => {
  async function executeTool(
    ctx: Context,
    agent: Agent,
    name: string,
    args: Record<string, unknown>,
    options: { signal?: AbortSignal; parent?: symbol } = {},
  ) {
    return ctx.tools.execute({
      signal: options.signal ?? callSignal,
      callId: CallId('c9'),
      name,
      arguments: args,
      agent,
      ...(options.parent === undefined ? {} : { parent: options.parent as never }),
    })
  }

  it.skipIf(stagedNativeAddon === undefined)('serves read, grep, glob, and Bash in-process', async () => {
    const previous = process.env.AGENTSHIM_DSH_NATIVE_DLL
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedNativeAddon
    try {
      const root = await makeRoot()
      await writeFile(join(root, 'native-notes.txt'), 'native needle\n'.repeat(4))
      await writeFile(join(root, 'excluded.log'), 'excluded\n')
      const ctx = await mountComposition(root)
      const { agent } = await mintStandardAgent(ctx, 'n1', root)

      const read = await runTool(ctx, agent, 'read', { path: 'native-notes.txt' })
      expect(read).toContain('native needle')

      const glob = await runTool(ctx, agent, 'glob', { pattern: 'native-*.txt' })
      expect(glob).toContain('native-notes.txt')
      expect(glob).not.toContain('excluded.log')

      const grep = await runTool(ctx, agent, 'grep', { pattern: 'needle', path: '.', fixed_strings: true })
      expect(grep).toContain('native-notes.txt')

      const bash = await runTool(ctx, agent, 'bash', { command: 'printf bash-native-exec-ok', description: 'Run successful command' })
      expect(bash).toContain('bash-native-exec-ok')
    } finally {
      if (previous === undefined) {
        delete process.env.AGENTSHIM_DSH_NATIVE_DLL
      } else {
        process.env.AGENTSHIM_DSH_NATIVE_DLL = previous
      }
    }
  })

  it('registers background Bash as a DSH job and exposes bound bash_status', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root, {}, async inner => {
      await inner.plugin(LocalJobRegistry, {})
      inner.jobs.attachController('composition-test')
    })
    const { agent } = await mintStandardAgent(ctx, 'jobs-owner', root)
    await ctx.plugin(class extends Service {
      constructor(inner: Context) {
        super(inner, 'agents')
      }
      list(): Agent[] {
        return [agent]
      }
      get(id: string): Agent | undefined {
        return id === agent.id ? agent : undefined
      }
    })
    const started = await executeTool(ctx, agent, 'bash', {
      command: 'printf background',
      description: 'Print background output',
      run_in_background: true,
    })
    expect(started.isError).toBe(false)
    const value = (started as unknown as { value: { kind: string; jobId: string } }).value
    expect(value).toEqual({ kind: 'background', jobId: 'bash-1' })

    const snapshot = await ctx.jobs.wait(JobId(value.jobId), 5_000, agent)
    expect(snapshot).toMatchObject({ id: 'bash-1', status: 'completed', detail: 'exit code: 0' })
    expect(ctx.jobs.read(JobId(value.jobId), agent).text).toBe('background')

    const status = await executeTool(ctx, agent, 'bash_status', { job_id: value.jobId })
    expect((status as unknown as { value: unknown }).value).toEqual({
      kind: 'status',
      jobId: 'bash-1',
      status: 'completed',
      label: 'printf background',
      detail: 'exit code: 0',
    })
  })

  it('forwards timeoutMs to a background job and settles it as timed out', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root, {
      env: { AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX: '600' },
    }, async inner => {
      await inner.plugin(LocalJobRegistry)
      inner.jobs.attachController('background-timeout-test')
      registerInheritedTools(inner, ['bash'])
    })
    const { agent } = await mintAgent(ctx, 'background-timeout', root)
    ctx.emit('agent/created', { agent })
    await ctx.plugin(class extends Service {
      constructor(inner: Context) {
        super(inner, 'agents')
      }
      list(): Agent[] {
        return [agent]
      }
      get(id: string): Agent | undefined {
        return id === agent.id ? agent : undefined
      }
    })
    const started = await executeTool(ctx, agent, 'bash', {
      command: 'while :; do printf x >> timeout-marker.txt; sleep 0.02; done',
      description: 'Time out a background tree',
      timeoutMs: 100,
      run_in_background: true,
    })
    expect(started.isError, JSON.stringify(started)).toBe(false)
    const jobId = (started as unknown as { value: { jobId: string } }).value.jobId
    const snapshot = await ctx.jobs.wait(JobId(jobId), 5_000, agent)
    expect(snapshot).toMatchObject({ status: 'failed' })
    expect(snapshot.detail).toContain('timed_out')
    const first = (await readFile(join(root, 'timeout-marker.txt'))).length
    await new Promise(resolve => setTimeout(resolve, 150))
    const second = (await readFile(join(root, 'timeout-marker.txt'))).length
    expect(second).toBe(first)
  })

  it.skipIf(stagedNativeAddon === undefined)('registers native background Bash as a DSH job with live output and cancellation', async () => {
    const previous = process.env.AGENTSHIM_DSH_NATIVE_DLL
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedNativeAddon
    try {
      const root = await makeRoot()
      const ctx = await mountComposition(root, {}, async inner => {
        await inner.plugin(LocalJobRegistry, {})
        inner.jobs.attachController('composition-test')
      })
      const { agent } = await mintStandardAgent(ctx, 'native-jobs-owner', root)
      await ctx.plugin(class extends Service {
        constructor(inner: Context) {
          super(inner, 'agents')
        }
        list(): Agent[] {
          return [agent]
        }
        get(id: string): Agent | undefined {
          return id === agent.id ? agent : undefined
        }
      })

      const started = await executeTool(ctx, agent, 'bash', {
        command: 'for i in 1 2 3; do printf "native-bg-%s\\n" "$i"; sleep 0.05; done',
        description: 'Produce native background output',
        run_in_background: true,
      })
      expect(started.isError).toBe(false)
      const value = (started as unknown as { value: { kind: string; jobId: string } }).value
      expect(value).toEqual({ kind: 'background', jobId: 'bash-1' })

      const snapshot = await ctx.jobs.wait(JobId(value.jobId), 10_000, agent)
      expect(snapshot).toMatchObject({ id: 'bash-1', status: 'completed', detail: 'exit code: 0' })
      const output = ctx.jobs.read(JobId(value.jobId), agent).text
      expect(output).toContain('native-bg-1')
      expect(output).toContain('native-bg-3')

      const status = await executeTool(ctx, agent, 'bash_status', { job_id: value.jobId })
      expect((status as unknown as { value: unknown }).value).toMatchObject({
        kind: 'status',
        jobId: 'bash-1',
        status: 'completed',
      })

      const longStarted = await executeTool(ctx, agent, 'bash', {
        command: 'while :; do printf "background output y"; sleep 0.05; done',
        description: 'Run until cancelled through the native engine',
        run_in_background: true,
      })
      const longJobId = (longStarted as unknown as { value: { jobId: string } }).value.jobId
      await waitForBackgroundOutput(ctx, agent, longJobId)
      expect(ctx.jobs.kill(JobId(longJobId), agent, 'native test cancellation')).toBe('requested')
      expect(await ctx.jobs.wait(JobId(longJobId), 10_000, agent)).toMatchObject({ status: 'killed' })
    } finally {
      if (previous === undefined) {
        delete process.env.AGENTSHIM_DSH_NATIVE_DLL
      } else {
        process.env.AGENTSHIM_DSH_NATIVE_DLL = previous
      }
    }
  })

  it.skipIf(stagedNativeAddon === undefined)('cancels and awaits a native background job during plugin unload', async () => {
    const previous = process.env.AGENTSHIM_DSH_NATIVE_DLL
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedNativeAddon
    try {
      const root = await makeRoot()
      const ctx = await mountComposition(root, {}, async inner => {
        await inner.plugin(LocalJobRegistry, {})
        inner.jobs.attachController('composition-test')
      })
      const { agent } = await mintStandardAgent(ctx, 'native-jobs-unload', root)
      await ctx.plugin(class extends Service {
        constructor(inner: Context) {
          super(inner, 'agents')
        }
        list(): Agent[] {
          return [agent]
        }
        get(id: string): Agent | undefined {
          return id === agent.id ? agent : undefined
        }
      })
      const started = await executeTool(ctx, agent, 'bash', {
        command: 'while :; do printf "background output z"; sleep 0.05; done',
        description: 'Run until native plugin unload',
        run_in_background: true,
      })
      const jobId = (started as unknown as { value: { jobId: string } }).value.jobId
      await waitForBackgroundOutput(ctx, agent, jobId)

      const adapter = pluginFibers.pop()
      expect(adapter).toBeDefined()
      await adapter!.dispose()

      expect(await ctx.jobs.wait(JobId(jobId), 10_000, agent)).toMatchObject({ status: 'killed' })
    } finally {
      if (previous === undefined) {
        delete process.env.AGENTSHIM_DSH_NATIVE_DLL
      } else {
        process.env.AGENTSHIM_DSH_NATIVE_DLL = previous
      }
    }
  })

  it('applies the DSH fs observation policy across a native read', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    await ctx.plugin(ObservationPolicy)
    const file = join(root, 'notes.txt')
    await writeFile(file, 'content\n')
    const { agent } = await mintStandardAgent(ctx, 'o1', root)

    const observed = vi.fn()
    ctx.on('fs/observed', (target, observation) => {
      observed(target.displayPath, observation)
    })

    await executeTool(ctx, agent, 'read', { path: 'notes.txt' })
    expect(observed).toHaveBeenCalledWith(expect.stringContaining('notes.txt'), expect.objectContaining({ kind: 'present' }))

    const target = await ctx.fs.resolve('notes.txt', { cwd: root })
    const actor = { agent }
    const intent = await ctx.waterfall('fs/edit-intent', target, actor, () => undefined)
    await ctx.fs.editText(target, { oldString: 'content', newString: 'updated', replaceAll: false }, intent)
    expect(await readFile(file, 'utf8')).toBe('updated\n')

    await executeTool(ctx, agent, 'read', { path: 'missing.txt' })
    expect(observed).toHaveBeenCalledWith(expect.stringContaining('missing.txt'), { kind: 'absent' })
  })

  it('reads via the native engine regardless of ctx.fs.processPath mapping', async () => {
    const root = await makeRoot()
    const ctx = new Context()
    contexts.push(ctx)
    await ctx.plugin(SystemPrompt, {})
    await ctx.plugin(ToolRuntime)
    await ctx.plugin(LocalFileSystem, { cwd: root })
    await ctx.plugin(UnconfinedShell)
    pluginFibers.push(await ctx.plugin(agentshim, {
      root,
      captureRoot: join(root, '.dsh-test-captures'),
      env: { FIXTURE_REPORT: '', FIXTURE_BOOT_FILE: '' },
      toolCallTimeoutMs: 600_000,
    }))
    const { agent } = await mintStandardAgent(ctx, 'w1', root)
    vi.spyOn(ctx.fs, 'processPath').mockReturnValue(join(root, 'other-world'))
    const result = await executeTool(ctx, agent, 'read', { path: 'notes.txt' })
    expect(result.isError).toBe(true)
    expect(result.error?.info).toMatchObject({ code: 'AGENTSHIM_READ_IO_FAILED' })
  })

  it('delivers native PDF images as durable attachments without raw base64', async () => {
    const root = await makeRoot()
    await copyFile(samplePdf, join(root, 'doc.pdf'))
    const ctx = await mountComposition(root)
    await ctx.plugin(LocalAttachmentStore, { dshHome: root })
    class StubLlm extends Service {
      constructor(ctx: Context) {
        super(ctx, 'llm')
      }
      resolveModelInfo(): Promise<{ inputModalities: readonly string[] }> {
        return Promise.resolve({ inputModalities: ['text', 'image'] })
      }
    }
    await ctx.plugin(StubLlm)
    const { agent } = await mintStandardAgent(ctx, 'i1', root)

    const result = await executeTool(ctx, agent, 'read', { path: 'doc.pdf', pdf_mode: 'image' })
    expect(result.isError).toBe(false)
    const blocks = result.content as Array<{ type: string; text?: string; attachment?: { attachmentId: string } }>
    const image = blocks.find(block => block.type === 'image')
    expect(image?.type).toBe('image')
    expect(typeof image?.attachment?.attachmentId).toBe('string')
    expect(JSON.stringify(result.content)).not.toContain('iVBORw0KGgo')
  })

  it('delivers a durable PDF image under nested execution without raw base64', async () => {
    const root = await makeRoot()
    await copyFile(samplePdf, join(root, 'doc.pdf'))
    const ctx = await mountComposition(root)
    await ctx.plugin(LocalAttachmentStore, { dshHome: root })
    class StubLlm extends Service {
      constructor(ctx: Context) {
        super(ctx, 'llm')
      }
      resolveModelInfo(): Promise<{ inputModalities: readonly string[] }> {
        return Promise.resolve({ inputModalities: ['text', 'image'] })
      }
    }
    await ctx.plugin(StubLlm)
    const { agent } = await mintStandardAgent(ctx, 'i-code', root)

    const result = await executeTool(
      ctx,
      agent,
      'read',
      { path: 'doc.pdf', pdf_mode: 'image' },
      { parent: Symbol('code-mode') },
    )
    expect(result.isError).toBe(false)
    const blocks = result.content as Array<{ type: string; text?: string; attachment?: { attachmentId: string } }>
    const image = blocks.find(block => block.type === 'image')
    expect(image?.type).toBe('image')
    expect(typeof image?.attachment?.attachmentId).toBe('string')
    expect(JSON.stringify(result.content)).not.toContain('iVBORw0KGgo')
  })

  interface SandboxStub {
    readonly confineCalls: Array<{ argv: readonly string[]; policy: { mode: string } }>
    readonly approvals: unknown[]
  }

  async function mountSandboxServices(
    ctx: Context,
    standing: 'read-only' | 'workspace-write',
    root: string,
  ): Promise<SandboxStub> {
    const confineCalls: SandboxStub['confineCalls'] = []
    const approvals: unknown[] = []
    await ctx.plugin(class extends Service {
      constructor(inner: Context) {
        super(inner, 'sandbox')
      }
      confine(argv: readonly string[], policy: { mode: string }) {
        confineCalls.push({ argv, policy })
        return {
          argv: [...argv],
          enforcement: 'partial' as const,
          denialSignatures: ['permission denied'],
          runnerFailureRules: [{
            allowedExitCodes: [70],
            fatalSignatures: ['runner failed'],
            informationalLines: ['notice'],
          }],
        }
      }
    })
    await ctx.plugin(class extends Service {
      constructor(inner: Context) {
        super(inner, 'sandboxPolicy')
      }
      resolve(request: { mode?: string } = {}) {
        return { mode: request.mode ?? standing, workspaceRoot: root }
      }
    })
    await ctx.plugin(class extends Service {
      constructor(inner: Context) {
        super(inner, 'approval')
      }
      request(request: unknown): Promise<'allowed-once'> {
        approvals.push(request)
        return Promise.resolve('allowed-once')
      }
    })
    return { confineCalls, approvals }
  }

  it('reads a workspace-external absolute path without process confinement in workspace-write mode', async () => {
    const root = await makeRoot()
    const outside = await makeRoot()
    const outsideFile = join(outside, 'outside.txt')
    await writeFile(outsideFile, 'outside workspace\n')
    let stub: SandboxStub | undefined
    const ctx = await mountComposition(root, {}, async inner => {
      stub = await mountSandboxServices(inner, 'workspace-write', root)
    })
    const { agent } = await mintStandardAgent(ctx, 'read-outside-workspace', root)

    const result = await executeTool(ctx, agent, 'read', { path: outsideFile })

    expect(result.isError).toBe(false)
    expect(result.content[0]).toMatchObject({ type: 'text', text: expect.stringContaining('outside workspace') })
    expect(stub?.confineCalls).toHaveLength(0)
  })

  it.skipIf(stagedNativeAddon === undefined)('confines standing process calls through the native engine and keeps one-time approval in-process', async () => {
    const previous = process.env.AGENTSHIM_DSH_NATIVE_DLL
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedNativeAddon
    try {
      const root = await makeRoot()
      let stub: SandboxStub | undefined
      const ctx = await mountComposition(root, {}, async inner => {
        stub = await mountSandboxServices(inner, 'workspace-write', root)
      })
      const { agent } = await mintStandardAgent(ctx, 'p1', root)

      const confined = await executeTool(ctx, agent, 'bash', { command: 'true', description: 'Run confined command' })
      expect(confined.isError).toBe(false)
      expect((confined as unknown as { value: { sandbox: unknown } }).value.sandbox).toEqual({
        mode: 'workspace-write',
        enforcement: 'partial',
        denied: false,
        runnerFailed: false,
      })
      expect(stub?.confineCalls).toHaveLength(1)
      expect(stub?.confineCalls[0]?.policy.mode).toBe('workspace-write')
      expect(stub?.confineCalls[0]?.argv.length).toBeGreaterThan(1)

      const approved = await executeTool(ctx, agent, 'bash', {
        command: 'true',
        description: 'Run approved command',
        sandbox_permissions: 'danger-full-access',
        justification: 'the command must run without file-effect confinement',
      })
      expect(approved.isError).toBe(false)
      expect(stub?.approvals).toHaveLength(1)
      expect(stub?.confineCalls).toHaveLength(1)
      const approvedValue = (approved as unknown as { value: { sandbox: unknown; text: string } }).value
      expect(approvedValue.sandbox).toEqual({
        mode: 'danger-full-access',
        denied: false,
        runnerFailed: false,
      })
      expect(approvedValue.text).toContain('Exit code: 0')
    } finally {
      if (previous === undefined) {
        delete process.env.AGENTSHIM_DSH_NATIVE_DLL
      } else {
        process.env.AGENTSHIM_DSH_NATIVE_DLL = previous
      }
    }
  })

  it('fails activation when the native engine package is unavailable', async () => {
    const previous = process.env.AGENTSHIM_DSH_NATIVE_DLL
    delete process.env.AGENTSHIM_DSH_NATIVE_DLL
    try {
      const root = await makeRoot()
      await expect(mountComposition(root, {}, async inner => {
        await mountSandboxServices(inner, 'read-only', root)
      })).rejects.toMatchObject({
        code: 'AGENTSHIM_NATIVE_ADDON_UNAVAILABLE',
        details: { reason: 'addon-unavailable' },
      })
    } finally {
      if (previous !== undefined) process.env.AGENTSHIM_DSH_NATIVE_DLL = previous
    }
  })

  it.skipIf(stagedNativeAddon === undefined)('skips agents when GNU bash is unavailable per-cwd', async () => {
    const previousDll = process.env.AGENTSHIM_DSH_NATIVE_DLL
    const previousBash = process.env.AGENTSHIM_BASH
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedNativeAddon
    process.env.AGENTSHIM_BASH = join(tmpdir(), 'definitely-missing-bash.exe')
    try {
      const root = await makeRoot()
      const ctx = await mountComposition(root, {}, async inner => {
        await mountSandboxServices(inner, 'read-only', root)
      })
      registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash'])
      const { agent } = await mintAgent(ctx, 'no-bash', root)
      ctx.emit('agent/created', { agent })
      expect(visibleNames(ctx, agent)).not.toContain('run_program')
      expect(await runTool(ctx, agent, 'read', {})).toBe('inherited:read')
    } finally {
      if (previousDll === undefined) {
        delete process.env.AGENTSHIM_DSH_NATIVE_DLL
      } else {
        process.env.AGENTSHIM_DSH_NATIVE_DLL = previousDll
      }
      if (previousBash === undefined) {
        delete process.env.AGENTSHIM_BASH
      } else {
        process.env.AGENTSHIM_BASH = previousBash
      }
    }
  })

  it.skipIf(stagedNativeAddon === undefined)('classifies denials, runner failures, and framed self-prints through the native engine', async () => {
    const previous = process.env.AGENTSHIM_DSH_NATIVE_DLL
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedNativeAddon
    try {
      const root = await makeRoot()
      const outside = await mkdtemp(join(tmpdir(), 'dsh-agentshim-outside-'))
      const readonlyFile = join(outside, 'readonly.txt').replaceAll('\\', '/')
      await writeFile(readonlyFile, 'kept\n')
      await chmod(readonlyFile, 0o444)
      try {
        const ctx = await mountComposition(root, {}, async inner => {
          await mountSandboxServices(inner, 'workspace-write', root)
        })
        const { agent } = await mintStandardAgent(ctx, 'p-classify', root)

        const withinRoot = await executeTool(ctx, agent, 'bash', {
          command: `printf inside > ${join(root, 'inside.txt').replaceAll('\\', '/')}`,
          description: 'Write inside the workspace root',
        })
        expect(withinRoot.isError).toBe(false)
        expect((withinRoot as unknown as { value: { sandbox: { denied: boolean } } }).value.sandbox.denied).toBe(false)

        const blocked = await executeTool(ctx, agent, 'bash', {
          command: `printf outside > ${readonlyFile}`,
          description: 'Write outside the workspace root',
        })
        expect(blocked.isError).toBe(false)
        const blockedValue = (blocked as unknown as { value: { sandbox: { denied: boolean; runnerFailed: boolean }; exitCode: string } }).value
        expect(blockedValue.exitCode).not.toBe('0')
        expect(blockedValue.sandbox).toEqual({ mode: 'workspace-write', enforcement: 'partial', denied: true, runnerFailed: false })

        const runnerFailed = await executeTool(ctx, agent, 'bash', {
          command: 'echo notice >&2; echo "RUNNER FAILED to start" >&2; exit 70',
          description: 'Report a runner failure',
        })
        expect(runnerFailed.isError).toBe(false)
        expect((runnerFailed as unknown as { value: { sandbox: { runnerFailed: boolean } } }).value.sandbox.runnerFailed).toBe(true)

        const framed = await executeTool(ctx, agent, 'bash', {
          command: 'echo "RUNNER FAILED to start" >&2; exit 1',
          description: 'Frame a runner failure at the wrong exit code',
        })
        expect(framed.isError).toBe(false)
        const framedSandbox = (framed as unknown as { value: { sandbox: { denied: boolean; runnerFailed: boolean } } }).value.sandbox
        expect(framedSandbox.denied).toBe(false)
        expect(framedSandbox.runnerFailed).toBe(false)
      } finally {
        await chmod(readonlyFile, 0o644)
        await rm(outside, { recursive: true, force: true })
      }
    } finally {
      if (previous === undefined) {
        delete process.env.AGENTSHIM_DSH_NATIVE_DLL
      } else {
        process.env.AGENTSHIM_DSH_NATIVE_DLL = previous
      }
    }
  })

  it('materializes caller cancellation through the DSH tool registry', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root, { env: { FIXTURE_REPORT: '', FIXTURE_BOOT_FILE: '' } })
    const { agent } = await mintStandardAgent(ctx, 'cancel', root)
    const controller = new AbortController()
    const pending = executeTool(ctx, agent, 'bash', { command: 'sleep 8', description: 'Run slow command' }, { signal: controller.signal })
    setTimeout(() => controller.abort(), 100)
    const result = await pending
    expect(result.isError).toBe(true)
    expect(result.error?.info).toMatchObject({ name: 'AbortError', code: TOOL_ABORTED })
  })

  it('cancels and settles an in-flight call before plugin teardown returns', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root, {
      env: {
        FIXTURE_REPORT: join(root, 'report.json'),
        FIXTURE_BOOT_FILE: join(root, 'boot.txt'),
        FIXTURE_EXIT_FILE: join(root, 'exit.txt'),
      },
    })
    const { agent } = await mintStandardAgent(ctx, 'unload-call', root)
    const pending = executeTool(ctx, agent, 'bash', { command: 'sleep 8', description: 'Run slow command' })
    await new Promise(resolve => setTimeout(resolve, 100))
    const fiber = pluginFibers.at(-1)
    expect(fiber).toBeDefined()
    await fiber!.dispose()
    const result = await pending
    expect(result.error?.info).toMatchObject({ name: 'AbortError', code: TOOL_ABORTED })
  })

  it('fails image delivery on a text-only model route with the pdf_mode hint', async () => {
    const root = await makeRoot()
    await copyFile(samplePdf, join(root, 'doc.pdf'))
    const ctx = await mountComposition(root)
    await ctx.plugin(LocalAttachmentStore, { dshHome: root })
    class TextOnlyLlm extends Service {
      constructor(ctx: Context) {
        super(ctx, 'llm')
      }
      resolveModelInfo(): Promise<{ inputModalities: readonly string[] }> {
        return Promise.resolve({ inputModalities: ['text'] })
      }
    }
    await ctx.plugin(TextOnlyLlm)
    const { agent } = await mintStandardAgent(ctx, 'i2', root)

    const result = await executeTool(ctx, agent, 'read', { path: 'doc.pdf', pdf_mode: 'image' })
    expect(result.isError).toBe(true)
    expect(result.error?.info).toMatchObject({ code: 'AGENTSHIM_IMAGE_ROUTE_UNSUPPORTED' })
    expect(result.error?.message).toContain('pdf_mode: "text"')
  })
})
