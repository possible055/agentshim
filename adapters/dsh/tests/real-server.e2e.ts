import { spawnSync } from 'node:child_process'
import { mkdir } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { Context } from '@deepseek-ai/cordis'
import AgentRegistry from '@deepseek-ai/dsh-agent'
import type { Agent } from '@deepseek-ai/dsh-agent'
import LocalFileSystem from '@deepseek-ai/dsh-fs-local'
import WorkerThreadCodeRuntime from '@deepseek-ai/dsh-code-runtime-worker-thread'
import LocalJobRegistry from '@deepseek-ai/dsh-jobs-local'
import { CallId } from '@deepseek-ai/dsh-llm'
import { createScope } from '@deepseek-ai/dsh-scope'
import ShellExecutor from '@deepseek-ai/dsh-shell'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import * as TimeoutPolicy from '@deepseek-ai/dsh-tool-call-timeout-policy'
import * as ToolJobs from '@deepseek-ai/dsh-tool-jobs'
import ToolRuntime from '@deepseek-ai/dsh-tools'
import type { ToolDefinition, ToolPresentationMode } from '@deepseek-ai/dsh-tools'
import { describe, expect, it } from 'vitest'
import * as agentshim from '../src/index.ts'
import { MIN_TOOL_CALL_TIMEOUT_MS } from '../src/session.ts'

const repoRoot = fileURLToPath(new URL('../../..', import.meta.url))
const enabled = process.env.AGENTSHIM_REAL_E2E === '1'

class UnconfinedShell extends ShellExecutor {
  override resolve(): never {
    throw new Error('real-server E2E marker shell must not execute')
  }

  override run(): Promise<never> {
    return Promise.reject(new Error('real-server E2E marker shell must not execute'))
  }

  override start(): never {
    throw new Error('real-server E2E marker shell must not execute')
  }
}

function resolveCargo(): string {
  const locator = process.platform === 'win32' ? 'where.exe' : 'which'
  const result = spawnSync(locator, ['cargo'], { cwd: repoRoot, encoding: 'utf8' })
  const first = result.stdout.split(/\r?\n/).find(line => line.trim() !== '')
  if (result.status !== 0 || first === undefined) {
    throw new Error(`could not resolve cargo for real-server E2E: ${result.stderr || result.stdout}`)
  }
  return first.trim()
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

async function startRealComposition(mode: ToolPresentationMode = 'native') {
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
  const adapter = await ctx.plugin(agentshim, {
    root: repoRoot,
    command: resolveCargo(),
    commandArgs: ['run', '--locked', '--'],
    readScope: 'normal',
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

describe.runIf(enabled)('real DSH composition and cargo server', () => {
  it('executes all six tools and managed Bash termination through DSH, the adapter, MCP, and cargo run', async () => {
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

  it.runIf(process.env.AGENTSHIM_LONG_E2E === '1')('keeps a DSH bash call alive past the generic MCP 60-second default', async () => {
    const composition = await startRealComposition()
    try {
      const output = await callText(composition.ctx, composition.agent, 'bash', {
        command: 'node -e "setTimeout(() => {}, 61000)"',
        description: 'Wait longer than the generic MCP timeout',
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
})
