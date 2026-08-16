import { mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { Context, Service } from '@deepseek-ai/cordis'
import { CallId } from '@deepseek-ai/dsh-llm'
import { createScope } from '@deepseek-ai/dsh-scope'
import type { Scope } from '@deepseek-ai/dsh-scope'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ToolRuntime, { TOOL_ABORTED } from '@deepseek-ai/dsh-tools'
import type { ToolDefinition } from '@deepseek-ai/dsh-tools'
import LocalFileSystem from '@deepseek-ai/dsh-fs-local'
import * as ObservationPolicy from '@deepseek-ai/dsh-fs-observation-policy'
import LocalAttachmentStore from '@deepseek-ai/dsh-attachment-local'
import type { Agent } from '@deepseek-ai/dsh-agent'
import * as codexshim from '../src/index.ts'
import type { Config } from '../src/index.ts'

const fixturePath = fileURLToPath(new URL('./fixture-server.mjs', import.meta.url))
const callSignal = new AbortController().signal

const contexts: Context[] = []
const pluginFibers: Array<{ dispose(): unknown }> = []
const roots: string[] = []

async function removeRoot(root: string): Promise<void> {
  await rm(root, { recursive: true, force: true })
}

async function exists(path: string): Promise<boolean> {
  try {
    await stat(path)
    return true
  } catch {
    return false
  }
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
  const root = await mkdtemp(join(tmpdir(), 'dsh-codexshim-comp-'))
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
    command: process.execPath,
    commandArgs: [fixturePath],
    readScope: 'normal',
    env: {
      FIXTURE_REPORT: join(root, 'report.json'),
      FIXTURE_BOOT_FILE: join(root, 'boot.txt'),
      FIXTURE_EXIT_FILE: join(root, 'exit.txt'),
    },
    toolCallTimeoutMs: 600_000,
    ...configOverrides,
  }
  pluginFibers.push(await ctx.plugin(codexshim, config))
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
  it('replaces the five tools for a root-matched agent, hides pwsh, keeps the rest', async () => {
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
    expect(names).toContain('write')
    expect(names).toContain('edit')
    expect(names).toContain('read_image')
    expect(names).toContain('todo')
    expect(names).not.toContain('pwsh')
    expect(names.filter(name => name.startsWith('mcp__'))).toEqual([])

    const replaced = await runTool(ctx, agent, 'bash', { command: 'true' })
    expect(JSON.parse(replaced)).toEqual({ name: 'bash', arguments: { command: 'true' } })
    const read = ctx.tools.schemas(agent).find(schema => schema.name === 'read')
    expect(read?.description).toContain('numbered lines')
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
  })

  it('leaves an agent whose cwd is not the plugin root completely untouched', async () => {
    const root = await makeRoot()
    const elsewhere = await makeRoot()
    const ctx = await mountComposition(root)
    registerInheritedTools(ctx, ['read', 'grep', 'glob', 'bash', 'write', 'edit'])
    const matching = await mintAgent(ctx, 'match', root)
    const other = await mintAgent(ctx, 'other', elsewhere)
    ctx.emit('agent/created', { agent: matching.agent })
    ctx.emit('agent/created', { agent: other.agent })

    expect(visibleNames(ctx, other.agent)).not.toContain('run_program')
    expect(await runTool(ctx, other.agent, 'read', {})).toBe('inherited:read')
    expect(visibleNames(ctx, matching.agent)).toContain('run_program')
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

  it('waits for the MCP child to exit before plugin teardown completes', async () => {
    const root = await makeRoot()
    await mountComposition(root)
    const fiber = pluginFibers.at(-1)
    expect(fiber).toBeDefined()
    await fiber!.dispose()
    expect(await exists(join(root, 'exit.txt'))).toBe(true)
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

    pluginFibers.push(await ctx.plugin(codexshim, {
      root,
      command: process.execPath,
      commandArgs: [fixturePath],
      readScope: 'normal',
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

    await expect(ctx.plugin(codexshim, {
      root,
      command: process.execPath,
      commandArgs: [fixturePath],
      readScope: 'normal',
      env: {
        FIXTURE_REPORT: join(root, 'report.json'),
        FIXTURE_BOOT_FILE: join(root, 'boot.txt'),
        FIXTURE_EXIT_FILE: join(root, 'exit.txt'),
      },
      toolCallTimeoutMs: 600_000,
    })).rejects.toThrow(/duplicate|already/i)

    expect(visibleNames(ctx, first.agent)).not.toContain('run_program')
    expect(await runTool(ctx, first.agent, 'read', {})).toBe('inherited:read')
    expect(await exists(join(root, 'exit.txt'))).toBe(true)
  })

  it('waits for shell and rebuilds process schemas across executor HMR', async () => {
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
    const config: Config = {
      root,
      command: process.execPath,
      commandArgs: [fixturePath],
      readScope: 'normal',
      env: {
        FIXTURE_REPORT: join(root, 'report.json'),
        FIXTURE_BOOT_FILE: join(root, 'boot.txt'),
        FIXTURE_EXIT_FILE: join(root, 'exit.txt'),
      },
      toolCallTimeoutMs: 600_000,
    }
    const adapterFiber = ctx.plugin(codexshim, config)
    pluginFibers.push(adapterFiber)
    expect(visibleNames(ctx, existing.agent)).not.toContain('run_program')
    expect(await exists(join(root, 'boot.txt'))).toBe(false)

    const unconfined = await ctx.plugin(UnconfinedShell)
    await adapterFiber
    const unconfinedBash = ctx.tools.get('bash', existing.agent)
    expect(unconfinedBash).toBeDefined()
    const unconfinedProperties = (unconfinedBash!.parameters as { properties?: Record<string, unknown> }).properties
    expect(unconfinedProperties).not.toHaveProperty('sandbox_permissions')

    await unconfined.dispose()
    await ctx.plugin(class extends Service {
      constructor(inner: Context) {
        super(inner, 'sandboxPolicy')
      }
      resolve(): { mode: 'workspace-write' } {
        return { mode: 'workspace-write' }
      }
    })
    const confined = await ctx.plugin(class extends Service {
      readonly sandboxMode = 'workspace-write'
      constructor(inner: Context) {
        super(inner, 'shell')
      }
    })
    await adapterFiber
    const confinedBash = ctx.tools.get('bash', existing.agent)
    expect(confinedBash).toBeDefined()
    const confinedProperties = (confinedBash!.parameters as { properties?: Record<string, unknown> }).properties
    expect(confinedProperties).toHaveProperty('sandbox_permissions')
    const denied = await ctx.tools.execute({
      signal: callSignal,
      callId: CallId('hmr-denied'),
      name: 'bash',
      arguments: { command: 'true' },
      agent: existing.agent,
    })
    expect(denied.error?.info).toMatchObject({ code: 'CODEXSHIM_PROCESS_REQUIRES_FULL_ACCESS' })

    await confined.dispose()
    await ctx.plugin(UnconfinedShell)
    await adapterFiber
    const restoredBash = ctx.tools.get('bash', existing.agent)
    expect(restoredBash).toBeDefined()
    const restoredProperties = (restoredBash!.parameters as { properties?: Record<string, unknown> }).properties
    expect(restoredProperties).not.toHaveProperty('sandbox_permissions')
    expect(JSON.parse(await runTool(ctx, existing.agent, 'bash', { command: 'true' })))
      .toEqual({ name: 'bash', arguments: { command: 'true' } })
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
    await expect(ctx.plugin(codexshim, {
      root,
      command: process.execPath,
      commandArgs: [fixturePath],
      readScope: 'normal',
      env: {},
      toolCallTimeoutMs: 600_000,
    })).rejects.toThrow(/local filesystem provider/)
  })
})

describe('DSH contract bridges', () => {
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

  it('maps codexshim tool errors to stable codes through the registry', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root, { env: { FIXTURE_REPORT: '', FIXTURE_BOOT_FILE: '', FIXTURE_CALL_ERROR: '1' } })
    const { agent } = await mintAgent(ctx, 'e1', root)
    ctx.emit('agent/created', { agent })
    const result = await executeTool(ctx, agent, 'bash', { command: 'explode' })
    expect(result.isError).toBe(true)
    expect(result.error?.info).toMatchObject({ code: 'CODEXSHIM_FIXTURE_DENIED' })
  })

  it('bridges the DSH fs observation policy across a codexshim read', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root)
    await ctx.plugin(ObservationPolicy)
    const file = join(root, 'notes.txt')
    await writeFile(file, 'content\n')
    const { agent } = await mintAgent(ctx, 'o1', root)
    ctx.emit('agent/created', { agent })

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

  it('refuses reads with CODEXSHIM_EXECUTION_WORLD_MISMATCH when the provider mapping changes', async () => {
    const root = await makeRoot()
    const ctx = new Context()
    contexts.push(ctx)
    await ctx.plugin(SystemPrompt, {})
    await ctx.plugin(ToolRuntime)
    await ctx.plugin(LocalFileSystem, { cwd: root })
    await ctx.plugin(UnconfinedShell)
    pluginFibers.push(await ctx.plugin(codexshim, {
      root,
      command: process.execPath,
      commandArgs: [fixturePath],
      readScope: 'normal',
      env: { FIXTURE_REPORT: '', FIXTURE_BOOT_FILE: '' },
      toolCallTimeoutMs: 600_000,
    }))
    const { agent } = await mintAgent(ctx, 'w1', root)
    ctx.emit('agent/created', { agent })
    vi.spyOn(ctx.fs, 'processPath').mockReturnValue(join(root, 'other-world'))
    const result = await executeTool(ctx, agent, 'read', { path: 'notes.txt' })
    expect(result.isError).toBe(true)
    expect(result.error?.info).toMatchObject({ code: 'CODEXSHIM_EXECUTION_WORLD_MISMATCH' })
  })

  it('delivers PDF images as durable attachments without raw base64', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root, { env: { FIXTURE_REPORT: '', FIXTURE_BOOT_FILE: '', FIXTURE_IMAGE: '1' } })
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
    const { agent } = await mintAgent(ctx, 'i1', root)
    ctx.emit('agent/created', { agent })

    const result = await executeTool(ctx, agent, 'read', { path: 'doc.pdf', pdf_mode: 'image' })
    expect(result.isError).toBe(false)
    const blocks = result.content as Array<{ type: string; text?: string; attachment?: { attachmentId: string } }>
    expect(blocks).toHaveLength(2)
    expect(blocks[0]).toMatchObject({ type: 'text', text: 'page 1' })
    expect(blocks[1]?.type).toBe('image')
    expect(typeof blocks[1]?.attachment?.attachmentId).toBe('string')
    expect(JSON.stringify(result.content)).not.toContain('iVBORw0KGgo')
  })

  it('defers a durable PDF image into Code Mode context', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root, { env: { FIXTURE_REPORT: '', FIXTURE_BOOT_FILE: '', FIXTURE_IMAGE: '1' } })
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
    const { agent } = await mintAgent(ctx, 'i-code', root)
    ctx.emit('agent/created', { agent })

    const result = await executeTool(
      ctx,
      agent,
      'read',
      { path: 'doc.pdf', pdf_mode: 'image' },
      { parent: Symbol('code-mode') },
    )
    expect(result.isError).toBe(false)
    expect(result.additionalContexts).toHaveLength(1)
    expect(JSON.stringify(result.additionalContexts)).not.toContain('iVBORw0KGgo')
    expect(result.additionalContexts?.[0]?.source).toEqual({ kind: 'plugin', plugin: 'codexshim' })
  })

  it('denies confined process calls before execution and strips fields after one-time approval', async () => {
    const root = await makeRoot()
    const approvalRequests: unknown[] = []
    const ctx = await mountComposition(root, {}, async inner => {
      await inner.plugin(class extends Service {
        readonly sandboxMode = 'workspace-write'
        constructor(ctx: Context) {
          super(ctx, 'shell')
        }
      })
      await inner.plugin(class extends Service {
        constructor(ctx: Context) {
          super(ctx, 'sandboxPolicy')
        }
        resolve(): Promise<{ mode: 'workspace-write' }> {
          return Promise.resolve({ mode: 'workspace-write' })
        }
      })
      await inner.plugin(class extends Service {
        constructor(ctx: Context) {
          super(ctx, 'approval')
        }
        request(request: unknown): Promise<'allowed-once'> {
          approvalRequests.push(request)
          return Promise.resolve('allowed-once')
        }
      })
    })
    const { agent } = await mintAgent(ctx, 'p1', root)
    ctx.emit('agent/created', { agent })

    const denied = await executeTool(ctx, agent, 'bash', { command: 'true' })
    expect(denied.isError).toBe(true)
    expect(denied.error?.info).toMatchObject({ code: 'CODEXSHIM_PROCESS_REQUIRES_FULL_ACCESS' })

    const approved = await executeTool(ctx, agent, 'bash', {
      command: 'true',
      sandbox_permissions: 'danger-full-access',
      justification: 'the command must run through the private local codexshim process',
    })
    expect(approved.isError).toBe(false)
    expect(approvalRequests).toHaveLength(1)
    const text = approved.content[0]
    expect(text?.type).toBe('text')
    expect(JSON.parse(text?.type === 'text' ? text.text : '')).toEqual({ name: 'bash', arguments: { command: 'true' } })
  })

  it('materializes caller cancellation through the DSH tool registry', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root, { env: { FIXTURE_REPORT: '', FIXTURE_BOOT_FILE: '', FIXTURE_CALL_DELAY_MS: '8000' } })
    const { agent } = await mintAgent(ctx, 'cancel', root)
    ctx.emit('agent/created', { agent })
    const controller = new AbortController()
    const pending = executeTool(ctx, agent, 'bash', { command: 'slow' }, { signal: controller.signal })
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
        FIXTURE_CALL_DELAY_MS: '8000',
      },
    })
    const { agent } = await mintAgent(ctx, 'unload-call', root)
    ctx.emit('agent/created', { agent })
    const pending = executeTool(ctx, agent, 'bash', { command: 'slow' })
    await new Promise(resolve => setTimeout(resolve, 100))
    const fiber = pluginFibers.at(-1)
    expect(fiber).toBeDefined()
    await fiber!.dispose()
    const result = await pending
    expect(result.error?.info).toMatchObject({ name: 'AbortError', code: TOOL_ABORTED })
    expect(await exists(join(root, 'exit.txt'))).toBe(true)
  })

  it('fails image delivery on a text-only model route with the pdf_mode hint', async () => {
    const root = await makeRoot()
    const ctx = await mountComposition(root, { env: { FIXTURE_REPORT: '', FIXTURE_BOOT_FILE: '', FIXTURE_IMAGE: '1' } })
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
    const { agent } = await mintAgent(ctx, 'i2', root)
    ctx.emit('agent/created', { agent })

    const result = await executeTool(ctx, agent, 'read', { path: 'doc.pdf', pdf_mode: 'image' })
    expect(result.isError).toBe(true)
    expect(result.error?.info).toMatchObject({ code: 'CODEXSHIM_IMAGE_ROUTE_UNSUPPORTED' })
    expect(result.error?.message).toContain('pdf_mode: "text"')
  })
})
