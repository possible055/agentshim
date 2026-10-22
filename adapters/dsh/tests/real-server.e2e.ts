import { chmod, copyFile, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Context, Service } from '@deepseek-ai/cordis'
import AgentRegistry from '@deepseek-ai/dsh-agent'
import type { Agent } from '@deepseek-ai/dsh-agent'
import LocalFileSystem from '@deepseek-ai/dsh-fs-local'
import NodePtcRuntime from '@deepseek-ai/dsh-ptc-runtime-node'
import LocalJobRegistry from '@deepseek-ai/dsh-jobs-local'
import { ToolCallId as CallId } from '@deepseek-ai/dsh-llm'
import { createScope } from '@deepseek-ai/dsh-scope'
import SessionStore, { SessionId } from '@deepseek-ai/dsh-session'
import SessionProjections from '@deepseek-ai/dsh-session-projection'
import WorkingDirectory from '@deepseek-ai/dsh-working-directory'
import ShellExecutor from '@deepseek-ai/dsh-shell'
import type { ShellExecRequest, ShellExecSpec, ShellExecution } from '@deepseek-ai/dsh-shell'
import LocalSubprocessRuntime from '@deepseek-ai/dsh-subprocess-local'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import * as TimeoutPolicy from '@deepseek-ai/dsh-tool-call-timeout-policy'
import * as ToolJobs from '@deepseek-ai/dsh-tool-jobs'
import ToolRuntime from '@deepseek-ai/dsh-tools'
import type { ToolDefinition, ToolPresentationMode } from '@deepseek-ai/dsh-tools'
import { describe, expect, it, vi } from 'vitest'
import * as agentshim from '../src/index.ts'
import { MIN_TOOL_CALL_TIMEOUT_MS } from '../src/config.ts'

const repoRoot = fileURLToPath(new URL('../../..', import.meta.url))
const enabled = process.env.AGENTSHIM_REAL_E2E === '1'
const builtNativeDll = fileURLToPath(new URL(
  process.platform === 'win32'
    ? '../../../target/debug/agentshim_napi.dll'
    : process.platform === 'darwin'
      ? '../../../target/debug/libagentshim_napi.dylib'
      : '../../../target/debug/libagentshim_napi.so',
  import.meta.url,
))

const stagedNativeAddon = await (async (): Promise<string | undefined> => {
  try {
    const directory = await mkdtemp(join(tmpdir(), 'agentshim-real-native-'))
    const staged = join(directory, 'agentshim_napi.node')
    await copyFile(builtNativeDll, staged)
    return staged
  } catch {
    return undefined
  }
})()

if (enabled && stagedNativeAddon === undefined) {
  throw new Error('real native E2E requires `cargo build -p agentshim-napi`')
}
if (stagedNativeAddon !== undefined) process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedNativeAddon

class UnconfinedShell extends ShellExecutor {
  override resolve(_request: ShellExecRequest): ShellExecSpec {
    throw new Error('native E2E marker shell must not execute')
  }

  override execute(_spec: ShellExecSpec): Promise<ShellExecution> {
    return Promise.reject(new Error('native E2E marker shell must not execute'))
  }
}

class PassthroughSandbox extends Service {
  constructor(inner: Context) {
    super(inner, 'sandbox')
  }

  async confine(argv: readonly string[]): Promise<{
    argv: string[]
    enforcement: 'full'
    denialSignatures: string[]
    runnerFailureRules: []
  }> {
    return {
      argv: [...argv],
      enforcement: 'full',
      denialSignatures: ['permission denied'],
      runnerFailureRules: [],
    }
  }
}

class WorkspaceWritePolicy extends Service {
  constructor(inner: Context) {
    super(inner, 'sandboxPolicy')
  }

  resolve(request: { mode?: string } = {}): { mode: string; workspaceRoot: string } {
    return { mode: request.mode ?? 'workspace-write', workspaceRoot: repoRoot }
  }
}

function registerInheritedCatalog(ctx: Context): void {
  for (const name of ['read', 'grep', 'glob', 'run_program', 'bash', 'bash_status']) {
    const definition: ToolDefinition = {
      name,
      description: `inherited ${name}`,
      parameters: { type: 'object', properties: {} },
      output: {
        schema: { type: 'string' },
        render: (_args, value) => [{ type: 'text', text: value as string }],
      },
      execute: () => Promise.resolve(`inherited:${name}`),
    }
    ctx.tools.register(definition)
  }
}

async function startRealComposition(
  mode: ToolPresentationMode = 'native',
  beforeAdapter?: (ctx: Context) => Promise<void>,
  root = repoRoot,
  configOverrides: Partial<agentshim.Config> = {},
) {
  const captureRoot = await mkdtemp(join(tmpdir(), 'agentshim-real-captures-'))
  const ctx = new Context()
  await ctx.plugin(SystemPrompt, {})
  if (mode !== 'native') {
    await ctx.plugin(LocalSubprocessRuntime)
    await ctx.plugin(PassthroughSandbox)
    await ctx.plugin(WorkspaceWritePolicy)
    await ctx.plugin(NodePtcRuntime, {})
  }
  await ctx.plugin(ToolRuntime, { mode })
  registerInheritedCatalog(ctx)
  await ctx.plugin(TimeoutPolicy)
  await ctx.plugin(AgentRegistry)
  await ctx.plugin(LocalJobRegistry, {})
  await ctx.plugin(ToolJobs, { completionDelivery: 'quiet' })
  await ctx.plugin(LocalFileSystem, { cwd: root })
  await ctx.plugin(SessionStore)
  await ctx.plugin(SessionProjections)
  await ctx.plugin(WorkingDirectory, { defaultDirectory: root })
  await ctx.plugin(UnconfinedShell)
  await beforeAdapter?.(ctx)
  const adapter = await ctx.plugin(agentshim, {
    root,
    captureRoot,
    env: {},
    toolCallTimeoutMs: MIN_TOOL_CALL_TIMEOUT_MS,
    ...configOverrides,
  })

  const id = 'dsh-agentshim-real-e2e'
  let agent!: Agent
  await ctx.plugin(Object.assign((inner: Context) => {
    const draft = {
      id,
      session: inner.sessions.create(SessionId(id), { meta: { cwd: root } }),
      options: { provider: 'stub-provider', model: 'stub-model' },
      status: 'busy',
      inject: () => undefined,
      followup: () => undefined,
    } as unknown as Agent
    const scope = createScope(inner, draft)
    ;(draft as { ctx?: Context }).ctx = scope.ctx
    agent = draft
  }, { inject: ['tools', 'systemPrompt', 'sessions'] }))
  const unregisterAgent = await ctx.agents.register(agent)

  return {
    ctx,
    agent,
    async dispose() {
      unregisterAgent()
      await adapter.dispose()
      await ctx.fiber.dispose()
      await rm(captureRoot, { recursive: true, force: true })
    },
  }
}

async function callText(ctx: Context, agent: Agent, name: string, args: Record<string, unknown>): Promise<string> {
  const result = await ctx.tools.execute({
    signal: new AbortController().signal,
    callId: CallId(`real-${name}`),
    name,
    arguments: args,
    agent,
  })
  if (result.isError) {
    throw new Error(`real tool ${name} failed: ${JSON.stringify(result)}`)
  }
  return result.content
    .filter(block => block.type === 'text')
    .map(block => block.type === 'text' ? block.text : '')
    .join('\n')
}

interface ForegroundValue {
  readonly kind: 'foreground'
  readonly exitCode: string
  readonly sandbox?: { readonly denied: boolean; readonly runnerFailed: boolean }
}

async function callForeground(ctx: Context, agent: Agent, command: string, description: string): Promise<ForegroundValue> {
  const result = await ctx.tools.execute({
    signal: new AbortController().signal,
    callId: CallId(`real-bash-${description}`),
    name: 'bash',
    arguments: { command, description },
    agent,
  })
  if (result.isError) {
    throw new Error(`real confined bash failed: ${JSON.stringify(result)}`)
  }
  return (result as unknown as { value: ForegroundValue }).value
}

describe.runIf(enabled)('real DSH native composition', () => {
  it('executes all six tools and managed Bash termination without starting agentshim serve', async () => {
    await mkdir(new URL('../../../local/', import.meta.url), { recursive: true })
    const composition = await startRealComposition()
    try {
      expect(await callText(composition.ctx, composition.agent, 'read', { path: 'adapters/dsh/package.json' })).toContain('dsh-agentshim')
      expect(await callText(composition.ctx, composition.agent, 'grep', { pattern: 'dsh-agentshim', path: 'adapters/dsh/package.json' })).toContain('package.json')
      expect(await callText(composition.ctx, composition.agent, 'glob', { pattern: 'adapters/dsh/package.json' })).toContain('adapters/dsh/package.json')
      expect(await callText(composition.ctx, composition.agent, 'run_program', { program: process.execPath, args: ['--version'] })).toMatch(/v\d+/)
      expect(await callText(composition.ctx, composition.agent, 'bash', {
        command: 'node --version',
        description: 'Print the Node.js version',
      })).toMatch(/v\d+/)
      const started = await callText(composition.ctx, composition.agent, 'bash', {
        command: 'while :; do printf x; sleep 0.05; done',
        description: 'Produce output until the DSH job is cancelled',
        run_in_background: true,
      })
      const jobId = started.match(/started background job (\S+)/)?.[1]
      if (jobId === undefined) throw new Error(`missing DSH job id: ${started}`)
      expect(await callText(composition.ctx, composition.agent, 'bash_status', {
        job_id: jobId,
      })).toContain('Status: running')
      expect(await callText(composition.ctx, composition.agent, 'job_list', {})).toContain(`${jobId} [bash] running`)
      expect(await callText(composition.ctx, composition.agent, 'job_output', { job_id: jobId })).toContain('[status: running]')
      expect(await callText(composition.ctx, composition.agent, 'job_kill', {
        job_id: jobId,
        reason: 'real E2E cleanup',
      })).toContain(`requested cancellation of job ${jobId}`)
      expect(await callText(composition.ctx, composition.agent, 'job_output', {
        job_id: jobId,
        wait: true,
        timeout_ms: 10_000,
      })).toContain('[status: killed')
      expect(await callText(composition.ctx, composition.agent, 'bash_status', {
        job_id: jobId,
      })).toContain('Status: killed')
    } finally {
      await composition.dispose()
    }
  }, 180_000)

  it.runIf(process.env.AGENTSHIM_LONG_E2E === '1')('keeps a DSH bash call alive past 60 seconds', async () => {
    const composition = await startRealComposition()
    try {
      const output = await callText(composition.ctx, composition.agent, 'bash', {
        command: 'node -e "setTimeout(() => {}, 61000)"',
        description: 'Wait longer than 60 seconds',
      })
      expect(output).toContain('Exit code: 0')
    } finally {
      await composition.dispose()
    }
  }, 120_000)

  it('dispatches typed canonical values through the real PTC run_code surface', async () => {
    const composition = await startRealComposition('ptc')
    try {
      await composition.ctx.workingDirectory.set(composition.agent, join(repoRoot, 'adapters', 'dsh'))
      const output = await callText(composition.ctx, composition.agent, 'run_code', {
        description: 'Read the package through PTC mode',
        code: [
          'const value = await tools.read({ path: "package.json" })',
          'return { kind: value.kind, found: value.text.includes("dsh-agentshim") }',
        ].join('\n'),
      })
      expect(JSON.parse(output)).toEqual({ kind: 'read', found: true })
    } finally {
      await composition.dispose()
    }
  }, 180_000)

  it('follows committed working directories without moving existing background jobs', async () => {
    const root = await mkdtemp(join(tmpdir(), 'agentshim-real-cwd-'))
    const child = join(root, 'child')
    await mkdir(child)
    await writeFile(join(root, 'notes.txt'), 'original directory\n')
    await writeFile(join(child, 'notes.txt'), 'changed directory\n')
    const composition = await startRealComposition('native', undefined, root)
    try {
      const { ctx, agent } = composition
      const observed: string[] = []
      ctx.on('fs/observed', target => { observed.push(ctx.fs.processPath(target)) })
      expect(await callText(ctx, agent, 'read', { path: 'notes.txt' })).toContain('original directory')
      const started = await callText(ctx, agent, 'bash', {
        command: 'pwd; sleep 30',
        description: 'Keep the original working directory alive',
        run_in_background: true,
      })
      const jobId = started.match(/started background job (\S+)/)?.[1]
      if (jobId === undefined) throw new Error(`missing DSH job id: ${started}`)
      await ctx.workingDirectory.set(agent, child)
      expect(agent.session.header.cwd).toBe(root)
      expect(await callText(ctx, agent, 'read', { path: 'notes.txt' })).toContain('changed directory')
      expect(observed).toContain(join(child, 'notes.txt'))
      expect(await callText(ctx, agent, 'grep', { pattern: 'changed', path: 'notes.txt' })).toContain('changed directory')
      expect(await callText(ctx, agent, 'glob', { pattern: 'notes.txt' })).toContain(join(child, 'notes.txt'))
      expect(await callText(ctx, agent, 'run_program', {
        program: process.execPath,
        args: ['-e', 'console.log(process.cwd())'],
      })).toContain(child)
      expect(await callText(ctx, agent, 'bash', {
        command: 'pwd', description: 'Print the current working directory',
      })).toContain(child)
      expect(await callText(ctx, agent, 'bash_status', { job_id: jobId })).toContain('Status: running')
      await callText(ctx, agent, 'job_kill', { job_id: jobId, reason: 'working directory E2E cleanup' })
      await ctx.workingDirectory.set(agent, root)
      expect(await callText(ctx, agent, 'read', { path: 'notes.txt' })).toContain('original directory')
    } finally {
      await composition.dispose()
      await rm(root, { recursive: true, force: true })
    }
  }, 180_000)

  for (const readScope of ['normal', 'unrestricted'] as const) {
    it(`reads foreground artifacts after directory changes with ${readScope} scope`, async () => {
      const root = await mkdtemp(join(tmpdir(), 'agentshim-real-artifacts-'))
      const child = join(root, 'child')
      await mkdir(child)
      const composition = await startRealComposition('native', undefined, root, { readScope })
      try {
        const { ctx, agent } = composition
        const textOutput = await callText(ctx, agent, 'run_program', {
          program: process.execPath,
          args: ['-e', 'process.stdout.write("needle\\n".repeat(9000))'],
        })
        const textPath = textOutput.match(/Full raw stdout: (.+) \(\d+ bytes/)?.[1]
        if (textPath === undefined) throw new Error(`missing text artifact: ${textOutput}`)
        const binaryOutput = await callText(ctx, agent, 'bash', {
          command: `"${process.execPath.replaceAll('\\', '/')}" -e 'process.stdout.write(Buffer.from([0,255,65,66]))'`,
          description: 'Publish a binary artifact',
        })
        const binaryPath = binaryOutput.match(/Full raw output: (.+) \(\d+ bytes/)?.[1]
        if (binaryPath === undefined) throw new Error(`missing binary artifact: ${binaryOutput}`)
        expect(await callText(ctx, agent, 'read', { path: textPath, line_count: 2 })).toContain('needle')

        await ctx.workingDirectory.set(agent, child)
        expect(await callText(ctx, agent, 'read', { path: textPath, line_count: 2 })).toContain('needle')
        expect(await callText(ctx, agent, 'grep', { path: textPath, pattern: 'needle', fixed_strings: true })).toContain('needle')
        expect(await callText(ctx, agent, 'read', { path: relative(child, textPath), line_count: 2 })).toContain('needle')
        expect(await callText(ctx, agent, 'grep', { path: relative(child, textPath), pattern: 'needle', fixed_strings: true })).toContain('needle')
        expect(await callText(ctx, agent, 'read', { path: binaryPath, artifact_offset: 0 })).toContain('AP9BQg==')

        const unknown = join(binaryPath, '..', 'unknown.bin')
        await writeFile(unknown, Buffer.from([0, 255]))
        const denied = await ctx.tools.execute({
          signal: new AbortController().signal,
          callId: CallId('unknown-artifact'),
          name: 'read',
          arguments: { path: unknown, artifact_offset: 0 },
          agent,
        })
        expect(denied.isError).toBe(true)
        expect(JSON.stringify(denied)).toContain('exact artifact capability')
      } finally {
        await composition.dispose()
        await rm(root, { recursive: true, force: true })
      }
    }, 180_000)
  }

  it('reads background artifacts published after changing directories', async () => {
    const root = await mkdtemp(join(tmpdir(), 'agentshim-real-background-artifacts-'))
    const child = join(root, 'child')
    await mkdir(child)
    const composition = await startRealComposition('native', undefined, root, { readScope: 'normal' })
    try {
      const { ctx, agent } = composition
      const started = await callText(ctx, agent, 'bash', {
        command: `while [ ! -f release.txt ]; do sleep 0.01; done; "${process.execPath.replaceAll('\\', '/')}" -e 'process.stdout.write("background needle\\n".repeat(9000))'`,
        description: 'Publish output after the directory change',
        run_in_background: true,
      })
      const jobId = started.match(/started background job (\S+)/)?.[1]
      if (jobId === undefined) throw new Error(`missing job id: ${started}`)
      await ctx.workingDirectory.set(agent, child)
      await writeFile(join(root, 'release.txt'), 'release')
      expect(await callText(ctx, agent, 'job_output', {
        job_id: jobId, wait: true, timeout_ms: 10_000,
      })).toContain('[status: completed')
      const status = await callText(ctx, agent, 'bash_status', { job_id: jobId })
      const path = status.match(/Artifacts: (.+)/)?.[1]
      if (path === undefined) throw new Error(`missing background artifact: ${status}`)
      expect(await callText(ctx, agent, 'read', { path, line_count: 2 })).toContain('background needle')
      expect(await callText(ctx, agent, 'grep', { path, pattern: 'background needle', fixed_strings: true })).toContain('background needle')
    } finally {
      await composition.dispose()
      await rm(root, { recursive: true, force: true })
    }
  }, 180_000)

  it('observes the same file read when the directory changes after ensure', async () => {
    const root = await mkdtemp(join(tmpdir(), 'agentshim-real-observation-cwd-'))
    const child = join(root, 'child')
    await mkdir(child)
    await writeFile(join(root, 'notes.txt'), 'original file\n')
    await writeFile(join(child, 'notes.txt'), 'unread file\n')
    const composition = await startRealComposition('native', undefined, root)
    const { ctx, agent } = composition
    const directories = ctx.workingDirectory
    const ensure = directories.ensure.bind(directories)
    const changed = vi.spyOn(directories, 'ensure').mockImplementationOnce(async (owner, signal) => {
      const cwd = await ensure(owner, signal)
      // Hold the caller until a direct SDK-style directory change commits.
      await directories.set(owner, child, signal)
      return cwd
    })
    try {
      const observed: string[] = []
      ctx.on('fs/observed', target => { observed.push(ctx.fs.processPath(target)) })
      expect(await callText(ctx, agent, 'read', { path: 'notes.txt' })).toContain('original file')
      expect(directories.get(agent.session)).toBe(child)
      expect(observed).toEqual([join(root, 'notes.txt')])
      expect(await callText(ctx, agent, 'read', { path: 'notes.txt' })).toContain('unread file')
      expect(observed).toEqual([join(root, 'notes.txt'), join(child, 'notes.txt')])
    } finally {
      changed.mockRestore()
      await composition.dispose()
      await rm(root, { recursive: true, force: true })
    }
  }, 180_000)

  it.skipIf(stagedNativeAddon === undefined)('confines foreground bash through the native engine with native classification', async () => {
    const previous = process.env.AGENTSHIM_DSH_NATIVE_DLL
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedNativeAddon
    const outside = await mkdtemp(join(tmpdir(), 'agentshim-real-outside-'))
    const readonlyFile = join(outside, 'readonly.txt').replaceAll('\\', '/')
    await writeFile(readonlyFile, 'kept\n')
    await chmod(readonlyFile, 0o444)
    try {
      const composition = await startRealComposition('native', async ctx => {
        await ctx.plugin(class extends Service {
          constructor(inner: Context) {
            super(inner, 'sandbox')
          }
          confine(argv: readonly string[]) {
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
            return { mode: request.mode ?? 'workspace-write', workspaceRoot: repoRoot }
          }
        })
      })
      try {
        const confined = await callForeground(composition.ctx, composition.agent, 'printf real-confined-ok', 'Run a confined command through the native engine')
        expect(confined.exitCode).toBe('0')
        expect(confined.sandbox).toEqual({
          mode: 'workspace-write',
          enforcement: 'partial',
          denied: false,
          runnerFailed: false,
        })

        const blocked = await callForeground(composition.ctx, composition.agent, `printf denied > ${readonlyFile}`, 'Write a read-only file outside the workspace root')
        expect(blocked.exitCode).not.toBe('0')
        expect(blocked.sandbox?.denied).toBe(true)
        expect(blocked.sandbox?.runnerFailed).toBe(false)

        const runnerFailed = await callForeground(composition.ctx, composition.agent, 'echo notice >&2; echo "RUNNER FAILED to start" >&2; exit 70', 'Report a runner failure at the gated exit code')
        expect(runnerFailed.sandbox?.runnerFailed).toBe(true)
      } finally {
        await composition.dispose()
      }
    } finally {
      if (previous === undefined) {
        delete process.env.AGENTSHIM_DSH_NATIVE_DLL
      } else {
        process.env.AGENTSHIM_DSH_NATIVE_DLL = previous
      }
      await chmod(readonlyFile, 0o644)
      await rm(outside, { recursive: true, force: true })
    }
  }, 180_000)
})
