import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { Context } from '@deepseek-ai/cordis'
import AgentRegistry from '@deepseek-ai/dsh-agent'
import type { Agent } from '@deepseek-ai/dsh-agent'
import LocalFileSystem from '@deepseek-ai/dsh-fs-local'
import { CallId } from '@deepseek-ai/dsh-llm'
import { createScope } from '@deepseek-ai/dsh-scope'
import ShellExecutor from '@deepseek-ai/dsh-shell'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import * as TimeoutPolicy from '@deepseek-ai/dsh-tool-call-timeout-policy'
import ToolRuntime from '@deepseek-ai/dsh-tools'
import type { ToolDefinition } from '@deepseek-ai/dsh-tools'
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
  for (const name of ['read', 'grep', 'glob', 'bash']) {
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

async function startRealComposition() {
  const ctx = new Context()
  await ctx.plugin(SystemPrompt, {})
  await ctx.plugin(ToolRuntime)
  registerInheritedCatalog(ctx)
  await ctx.plugin(TimeoutPolicy)
  await ctx.plugin(AgentRegistry)
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
      },
      options: { provider: 'stub-provider', model: 'stub-model' },
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
    const composition = await startRealComposition()
    try {
      expect(await callText(composition.ctx, composition.agent, 'read', { path: 'adapters/dsh/package.json' })).toContain('dsh-agentshim')
      expect(await callText(composition.ctx, composition.agent, 'grep', { pattern: 'dsh-agentshim', path: 'adapters/dsh/package.json' })).toContain('package.json')
      expect(await callText(composition.ctx, composition.agent, 'glob', { pattern: 'adapters/dsh/package.json' })).toContain('adapters/dsh/package.json')
      expect(await callText(composition.ctx, composition.agent, 'run_program', { program: process.execPath, args: ['--version'] })).toMatch(/v\d+/)
      expect(await callText(composition.ctx, composition.agent, 'bash', { command: 'node --version' })).toMatch(/v\d+/)
      const detached = await callText(composition.ctx, composition.agent, 'bash', {
        command: 'while :; do printf x >> local/dsh-real-e2e-marker; sleep 0.05; done',
        detach: true,
        log_path: 'local/dsh-real-e2e-managed.log',
      })
      const jobId = detached.match(/job_id=(bash-[0-9a-f-]+)/)?.[1]
      expect(jobId).toMatch(/^bash-/)
      if (jobId === undefined) throw new Error(`missing detached job_id: ${detached}`)
      expect(await callText(composition.ctx, composition.agent, 'bash_status', {
        job_id: jobId,
        tail_bytes: 0,
      })).toContain('State: running')
      expect(await callText(composition.ctx, composition.agent, 'bash', {
        action: 'terminate',
        job_id: jobId,
      })).toContain('State: terminated')
    } finally {
      await composition.dispose()
    }
  }, 180_000)

  it.runIf(process.env.AGENTSHIM_LONG_E2E === '1')('keeps a DSH bash call alive past the generic MCP 60-second default', async () => {
    const composition = await startRealComposition()
    try {
      const output = await callText(composition.ctx, composition.agent, 'bash', {
        command: 'node -e "setTimeout(() => {}, 61000)"',
      })
      expect(output).toContain('Exit code: 0')
    } finally {
      await composition.dispose()
    }
  }, 120_000)
})
