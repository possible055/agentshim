import { chmod, copyFile, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Context, Service } from '@deepseek-ai/cordis'
import AgentRegistry from '@deepseek-ai/dsh-agent'
import type { Agent } from '@deepseek-ai/dsh-agent'
import LocalFileSystem from '@deepseek-ai/dsh-fs-local'
import WorkerThreadCodeRuntime from '@deepseek-ai/dsh-code-runtime-worker-thread'
import LocalJobRegistry from '@deepseek-ai/dsh-jobs-local'
import * as llm from '@deepseek-ai/dsh-llm'
const createCallId = (llm as { ToolCallId?: (id: string) => any; CallId?: (id: string) => any }).ToolCallId
  ?? (llm as { ToolCallId?: (id: string) => any; CallId?: (id: string) => any }).CallId
  ?? ((id: string) => id)
const CallId = createCallId
import { createScope } from '@deepseek-ai/dsh-scope'
import ShellExecutor from '@deepseek-ai/dsh-shell'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import * as TimeoutPolicy from '@deepseek-ai/dsh-tool-call-timeout-policy'
import * as ToolJobs from '@deepseek-ai/dsh-tool-jobs'
import ToolRuntime from '@deepseek-ai/dsh-tools'
import type { ToolDefinition, ToolPresentationMode } from '@deepseek-ai/dsh-tools'
import { describe, expect, it } from 'vitest'
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
  override resolve(): never {
    throw new Error('native E2E marker shell must not execute')
  }

  override run(): Promise<never> {
    return Promise.reject(new Error('native E2E marker shell must not execute'))
  }

  override start(): never {
    throw new Error('native E2E marker shell must not execute')
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
) {
  const captureRoot = await mkdtemp(join(tmpdir(), 'agentshim-real-captures-'))
  const ctx = new Context()
  await ctx.plugin(SystemPrompt, {})
  if (mode !== 'native') await ctx.plugin(WorkerThreadCodeRuntime, {})
  await ctx.plugin(ToolRuntime, { mode })
  registerInheritedCatalog(ctx)
  await ctx.plugin(TimeoutPolicy)
  await ctx.plugin(AgentRegistry)
  await ctx.plugin(LocalJobRegistry, {})
  await ctx.plugin(ToolJobs, { completionDelivery: 'quiet' })
  await ctx.plugin(LocalFileSystem, { cwd: repoRoot })
  await ctx.plugin(UnconfinedShell)
  await beforeAdapter?.(ctx)
  const adapter = await ctx.plugin(agentshim, {
    root: repoRoot,
    captureRoot,
    env: {},
    toolCallTimeoutMs: MIN_TOOL_CALL_TIMEOUT_MS,
  })

  const id = 'dsh-agentshim-real-e2e'
  let agent!: Agent
  await ctx.plugin(Object.assign((inner: Context) => {
    const draft = {
      id,
      session: {
        id,
        header: { cwd: repoRoot },
        requestHeader: () => ({ config: {} }),
        append: () => undefined,
      },
      options: { provider: 'stub-provider', model: 'stub-model' },
      status: 'busy',
      inject: () => undefined,
      followup: () => undefined,
    } as unknown as Agent
    const scope = createScope(inner, draft)
    ;(draft as { ctx?: Context }).ctx = scope.ctx
    agent = draft
  }, { inject: ['tools', 'systemPrompt'] }))
  const unregisterAgent = ctx.agents.register(agent)

  return {
    ctx,
    agent,
    async dispose() {
      unregisterAgent()
      await adapter.dispose()
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
      const jobId = started.match(/started background job (bash-\d+)/)?.[1]
      expect(jobId).toMatch(/^bash-\d+$/)
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

  it('dispatches typed canonical values through the real Code and Both run_code surfaces', async () => {
    for (const mode of ['code', 'both'] as const) {
      const composition = await startRealComposition(mode)
      try {
        const output = await callText(composition.ctx, composition.agent, 'run_code', {
          description: `Read the package through ${mode} mode`,
          code: [
            'const value = await tools.read({ path: "adapters/dsh/package.json" })',
            'return { kind: value.kind, found: value.text.includes("dsh-agentshim") }',
          ].join('\n'),
        })
        expect(JSON.parse(output)).toEqual({ kind: 'read', found: true })
      } finally {
        await composition.dispose()
      }
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
