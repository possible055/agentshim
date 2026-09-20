import { readFile, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it, vi } from 'vitest'
import { Context, Service } from '@deepseek-ai/cordis'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ToolRuntime, { renderToolsSdk, renderToolsSdkPy } from '@deepseek-ai/dsh-tools'
import LocalFileSystem from '@deepseek-ai/dsh-fs-local'
import type { Agent } from '@deepseek-ai/dsh-agent'
import { PUBLIC_TOOL_NAMES } from '../src/contracts.ts'
import * as agentshim from '../src/index.ts'
import {
  CallId,
  callSignal,
  contexts,
  inheritedTool,
  makeRoot,
  mintAgent,
  mintPresetAgent,
  mintStandardAgent,
  mountComposition,
  pluginFibers,
  registerInheritedTools,
  removeRoot,
  roots,
  runTool,
  UnconfinedShell,
  visibleNames,
} from './helpers/composition.ts'

// Plugin assembly without native engine execution paths: catalog topology, prompt
// shadowing, install/rollback lifecycle, and multi-workspace pooling. Activation
// still stages the native addon (see helpers/composition.ts); the specs that drive
// the native engine, background jobs, sandbox, and attachments live in
// composition.spec.ts.

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

describe('agent scope replacement', () => {
  it('loads without a systemPrompt service when no agents service is present', async () => {
    const root = await makeRoot()
    const ctx = new Context()
    contexts.push(ctx)
    await ctx.plugin(LocalFileSystem, { cwd: root })
    pluginFibers.push(await ctx.plugin(agentshim, {
      root,
      captureRoot: join(root, '.dsh-test-captures'),
      env: {},
      toolCallTimeoutMs: 600_000,
    }))
  })

  it('replaces the six tools for a root-matched agent, hides pwsh, keeps the rest', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash', 'pwsh', 'write', 'edit', 'read_image', 'todo'])
    const { agent } = await mintAgent(ctx, 'a1', root)
    ctx.emit('agent/created', { agent, source: 'startup' })

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
    // The description is served by the engine catalog; only machine-readable
    // invariants are pinned here, never wording (see contract tests in the engine).
    expect(read?.description).toBeDefined()
    expect(read?.description).not.toBe('inherited read')
    expect(read?.description).not.toContain('AGENTSHIM_')
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
    ctx.emit('agent/created', { agent, source: 'startup' })

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
    ctx.emit('agent/created', { agent, source: 'startup' })

    expect(visibleNames(ctx, agent)).toEqual(['bash', 'bash_status', 'str_replace_editor'])
    const bash = ctx.tools.get('bash', agent)
    expect(bash?.description).toBeDefined()
    expect(bash?.description).not.toBe('inherited bash')
    expect(bash?.description).not.toContain('AGENTSHIM_')
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
    ctx.emit('agent/created', { agent, source: 'startup' })

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
    ctx.emit('agent/created', { agent: matching.agent, source: 'startup' })
    ctx.emit('agent/created', { agent: other.agent, source: 'startup' })

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
    expect(() => ctx.emit('agent/created', { agent, source: 'startup' })).toThrow(/duplicate|already/i)

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
    ctx.emit('agent/created', { agent, source: 'startup' })
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
    ctx.emit('agent/created', { agent, source: 'startup' })

    expect(visibleNames(ctx, agent)).toEqual([
      'bash', 'bash_status', 'edit', 'glob', 'grep', 'read', 'read_image', 'run_program', 'todo', 'write',
    ])
    expect(ctx.tools.get('read', agent)?.description).not.toBe('inherited read')
    expect(ctx.tools.get('write', agent)?.description).toBe('inherited write')
    expect(await runTool(ctx, agent, 'bash', { command: 'true', description: 'Run successful command' }))
      .toContain('Exit code: 0')
    expect(await runTool(ctx, agent, 'todo', {})).toBe('inherited:todo')
  })

  it('replaces only bash on a minimal-shaped preset', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    const { agent } = await mintPresetAgent(ctx, 'minimal', root, ['bash', 'str_replace_editor'])
    ctx.emit('agent/created', { agent, source: 'startup' })

    expect(visibleNames(ctx, agent)).toEqual(['bash', 'bash_status', 'str_replace_editor'])
    expect(ctx.tools.get('bash', agent)?.description).not.toBe('inherited bash')
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
    ctx.emit('agent/created', { agent, source: 'startup' })

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
    ctx.emit('agent/created', { agent, source: 'startup' })

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
    ctx.emit('agent/created', { agent, source: 'startup' })

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
    ctx.emit('agent/created', { agent: a.agent, source: 'startup' })
    ctx.emit('agent/created', { agent: b.agent, source: 'startup' })

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
    ctx.emit('agent/created', { agent, source: 'startup' })

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
    ctx.emit('agent/created', { agent: agentA.agent, source: 'startup' })
    ctx.emit('agent/created', { agent: agentB.agent, source: 'startup' })

    expect(visibleNames(ctx, agentA.agent)).toContain('run_program')
    expect(visibleNames(ctx, agentB.agent)).toContain('run_program')
    const readA = await runTool(ctx, agentA.agent, 'read', { path: 'marker.txt' })
    const readB = await runTool(ctx, agentB.agent, 'read', { path: 'marker.txt' })
    expect(readA).toContain('workspace A')
    expect(readB).toContain('workspace B')
  })
})
