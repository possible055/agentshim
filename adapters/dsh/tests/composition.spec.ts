import { chmod, copyFile, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it, vi } from 'vitest'
import { Context, Service } from '@deepseek-ai/cordis'
import * as llm from '@deepseek-ai/dsh-llm'
const createCallId = (llm as { ToolCallId?: (id: string) => any; CallId?: (id: string) => any }).ToolCallId
  ?? (llm as { ToolCallId?: (id: string) => any; CallId?: (id: string) => any }).CallId
  ?? ((id: string) => id)
const CallId = createCallId
import type { Agent } from '@deepseek-ai/dsh-agent'
import LocalAttachmentStore from '@deepseek-ai/dsh-attachment-local'
import * as ObservationPolicy from '@deepseek-ai/dsh-fs-observation-policy'
import { JobId } from '@deepseek-ai/dsh-jobs'
import LocalJobRegistry from '@deepseek-ai/dsh-jobs-local'
import { TOOL_ABORTED } from '@deepseek-ai/dsh-tools'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ToolRuntime from '@deepseek-ai/dsh-tools'
import LocalFileSystem from '@deepseek-ai/dsh-fs-local'
import { promptSections, pwshSectionOrder } from '../src/tools.ts'
import * as agentshim from '../src/index.ts'
import {
  callSignal,
  contexts,
  makeRoot,
  mintAgent,
  mintStandardAgent,
  mountComposition,
  registerInheritedTools,
  runTool,
  stagedNativeAddon,
  pluginFibers,
  UnconfinedShell,
  visibleNames,
  waitForBackgroundOutput,
  waitForJobTerminal,
} from './helpers/composition.ts'
import { sleep, waitForCondition } from './helpers/wait.ts'

// Composition tests that drive the native engine: tool execution, background job
// registration and teardown, sandbox confinement, attachment delivery, and
// cancellation. Assembly-only coverage lives in assembly.spec.ts; both files share
// the staged-addon harness in helpers/composition.ts.

const samplePdf = fileURLToPath(new URL('./fixtures/sample.pdf', import.meta.url))

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

    const snapshot = await waitForJobTerminal(ctx, agent, value.jobId)
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
    ctx.emit('agent/created', { agent, source: 'startup' })
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
    const snapshot = await waitForJobTerminal(ctx, agent, jobId)
    expect(snapshot).toMatchObject({ status: 'failed' })
    expect(snapshot.detail).toContain('timed_out')
    const marker = join(root, 'timeout-marker.txt')
    const first = (await readFile(marker)).length
    // The timed-out tree must stop writing: observe the marker instead of
    // trusting a fixed 150 ms settle pause.
    await waitForCondition(async () => (await readFile(marker)).length === first, 5_000, 'the timed-out tree to stop writing')
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

      const snapshot = await waitForJobTerminal(ctx, agent, value.jobId)
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
      expect(await waitForJobTerminal(ctx, agent, longJobId)).toMatchObject({ status: 'killed' })
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

      const adapter = pluginFibers.pop()
      expect(adapter).toBeDefined()
      const disposeStartedAt = performance.now()
      await adapter!.dispose()
      expect(performance.now() - disposeStartedAt).toBeLessThan(10_000)

      expect(await waitForJobTerminal(ctx, agent, jobId)).toMatchObject({ status: 'killed' })
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
      async confine(argv: readonly string[], policy: { mode: string }) {
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
      ctx.emit('agent/created', { agent, source: 'startup' })
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
    // The abort has to land once the call is in flight; "in flight" has no
    // observable event from outside the tool registry.
    // sleep-allow: mid-call abort trigger
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
    // The dispose has to race an in-flight call; "the call started" has no
    // observable event from outside.
    // sleep-allow: in-flight setup before teardown
    await sleep(100)
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

  it('resolves section orders from the system-prompt service with static fallbacks', async () => {
    const fallbackCtx = {} as unknown as Parameters<typeof promptSections>[0]
    const fallbackMap = new Map(promptSections(fallbackCtx).map(s => [s.name, s.order]))
    expect(fallbackMap.get('tool:bash')).toBe(1000)
    expect(fallbackMap.get('tool:run_program')).toBe(1005)
    expect(fallbackMap.get('tool:read')).toBe(1100)
    expect(fallbackMap.get('tool:glob')).toBe(1400)
    expect(fallbackMap.get('tool:grep')).toBe(1500)
    expect(fallbackMap.get('tool:bash_status')).toBe(1605)
    expect(pwshSectionOrder(fallbackCtx)).toBe(1010)

    // Dynamic getSectionOrder resolution
    const mockCtx = {
      systemPrompt: {
        getSectionOrder: (key: string) => {
          if (key === 'TOOL_BASH') return 2000
          if (key === 'TOOL_READ') return 2100
          if (key === 'TOOL_PWSH') return 2010
          return undefined
        },
      },
    } as unknown as Parameters<typeof promptSections>[0]
    const fromCtx = promptSections(mockCtx)
    const fromCtxMap = new Map(fromCtx.map(s => [s.name, s.order]))
    expect(fromCtxMap.get('tool:bash')).toBe(2000)
    expect(fromCtxMap.get('tool:read')).toBe(2100)
    expect(fromCtxMap.get('tool:glob')).toBe(1400)
    expect(pwshSectionOrder(mockCtx)).toBe(2010)

    const actualCtx = await mountComposition(await makeRoot())
    const actualSystemPrompt = actualCtx.get('systemPrompt') as unknown as {
      getSectionOrder(name: string): number
    }
    expect(typeof actualSystemPrompt.getSectionOrder).toBe('function')
    const actualMap = new Map(promptSections(actualCtx).map(section => [section.name, section.order]))
    expect(actualMap.get('tool:bash')).toBe(actualSystemPrompt.getSectionOrder('TOOL_BASH'))
    expect(actualMap.get('tool:run_program')).toBe(actualSystemPrompt.getSectionOrder('TOOL_BASH') + 5)
    expect(actualMap.get('tool:read')).toBe(actualSystemPrompt.getSectionOrder('TOOL_READ'))
    expect(actualMap.get('tool:glob')).toBe(actualSystemPrompt.getSectionOrder('TOOL_GLOB'))
    expect(actualMap.get('tool:grep')).toBe(actualSystemPrompt.getSectionOrder('TOOL_GREP'))
    expect(actualMap.get('tool:bash_status')).toBe(actualSystemPrompt.getSectionOrder('TOOL_JOBS') + 5)
    expect(pwshSectionOrder(actualCtx)).toBe(actualSystemPrompt.getSectionOrder('TOOL_PWSH'))

    const getThrowsCtx = Object.defineProperty({
      get: () => {
        throw new Error('service lookup failed')
      },
    }, 'systemPrompt', { value: { getSectionOrder: () => 2200 } }) as unknown as Parameters<typeof promptSections>[0]
    expect(promptSections(getThrowsCtx).find(section => section.name === 'tool:bash')?.order).toBe(2200)

    const methodThrowsCtx = {
      get: () => ({ getSectionOrder: () => { throw new Error('order lookup failed') } }),
    } as unknown as Parameters<typeof promptSections>[0]
    expect(promptSections(methodThrowsCtx).find(section => section.name === 'tool:bash')?.order).toBe(1000)

    for (const invalidOrder of [Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY, '100']) {
      const invalidOrderCtx = {
        get: () => ({ getSectionOrder: () => invalidOrder }),
      } as unknown as Parameters<typeof promptSections>[0]
      expect(promptSections(invalidOrderCtx).find(section => section.name === 'tool:bash')?.order).toBe(1000)
    }

    const throwingService = {}
    Object.defineProperty(throwingService, 'getSectionOrder', {
      get: () => {
        throw new Error('section order getter failed')
      },
    })
    const getterThrowsCtx = { get: () => throwingService } as unknown as Parameters<typeof promptSections>[0]
    expect(promptSections(getterThrowsCtx).find(section => section.name === 'tool:bash')?.order).toBe(1000)

    const missingServiceCtx = { get: () => undefined } as unknown as Parameters<typeof promptSections>[0]
    Object.defineProperty(missingServiceCtx, 'systemPrompt', {
      get: () => {
        throw new Error('missing service getter failed')
      },
    })
    expect(promptSections(missingServiceCtx).find(section => section.name === 'tool:bash')?.order).toBe(1000)
  })
})
