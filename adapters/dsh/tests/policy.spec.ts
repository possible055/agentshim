import { describe, expect, it, vi } from 'vitest'
import type { Context } from '@deepseek-ai/cordis'
import type { FsTarget } from '@deepseek-ai/dsh-fs'
import type { ToolExecution } from '@deepseek-ai/dsh-tools'
import {
  assertExecutionWorld,
  classifyDenial,
  classifyRunnerFailure,
  completeReadObservation,
  createProcessPolicy,
} from '../src/policy.ts'
import type { ResolvedSessionConfig } from '../src/session.ts'

const signal = new AbortController().signal
const config: ResolvedSessionConfig = {
  root: 'C:/workspace',
  command: 'C:/bin/agentshim.exe',
  commandArgs: ['wrapper'],
  readScope: 'normal',
  env: {},
  toolCallTimeoutMs: 600_000,
}

function stubContext(services: Record<string, unknown>): Context {
  return { get: (name: string) => services[name] } as unknown as Context
}

function stubExec(): ToolExecution {
  return {
    signal,
    callId: 'c1',
    name: 'bash',
    arguments: {},
    agent: { session: { id: 's1' } },
  } as unknown as ToolExecution
}

function confined(mode: 'read-only' | 'workspace-write' | 'danger-full-access', outcome = 'allowed-once') {
  const approval = { request: vi.fn(async () => outcome) }
  const sandbox = {
    confine: vi.fn(() => ({
      argv: ['sandbox-runner', '--', config.command, ...config.commandArgs, 'serve', '--client-profile', 'dsh', '--read-scope', 'normal'],
      enforcement: 'partial',
      denialSignatures: ['permission denied'],
      runnerFailureRules: [{ allowedExitCodes: [70], fatalSignatures: ['runner failed'] }],
    })),
  }
  const sandboxPolicy = {
    resolve: vi.fn((request: { mode?: string } = {}) => ({
      mode: request.mode ?? mode,
      workspaceRoot: config.root,
      sessionId: 's1',
    })),
  }
  const policy = createProcessPolicy(stubContext({ sandbox, sandboxPolicy, approval }))
  return { policy, sandbox, sandboxPolicy, approval }
}

describe('per-call process policy', () => {
  it('uses the shared generation only for unconfined or standing full-access calls', async () => {
    const unconfined = createProcessPolicy(stubContext({}))
    await expect(unconfined.plan('bash', { command: 'true' }, stubExec(), config)).resolves.toMatchObject({
      args: { command: 'true' },
      launch: undefined,
      sandbox: undefined,
    })
    const full = confined('danger-full-access')
    const plan = await full.policy.plan('bash', { command: 'true' }, stubExec(), config)
    expect(plan.launch).toBeUndefined()
    expect(plan.sandbox?.mode).toBe('danger-full-access')
    expect(full.sandbox.confine).not.toHaveBeenCalled()
  })

  it('fails plugin creation when sandbox and policy are not composed together', () => {
    expect(() => createProcessPolicy(stubContext({ sandbox: {} }))).toThrow(/must either both be composed/)
    expect(() => createProcessPolicy(stubContext({ sandboxPolicy: {} }))).toThrow(/must either both be composed/)
  })

  it('confines exact AgentShim argv for every confined call', async () => {
    const { policy, sandbox } = confined('read-only')
    const plan = await policy.plan('run_program', { program: 'git', args: ['status'] }, stubExec(), config)
    expect(sandbox.confine).toHaveBeenCalledWith(
      [config.command, 'wrapper', 'serve', '--client-profile', 'dsh', '--read-scope', 'normal'],
      expect.objectContaining({ mode: 'read-only', workspaceRoot: config.root }),
    )
    expect(plan.launch).toMatchObject({ command: 'sandbox-runner' })
    expect(plan.sandbox).toMatchObject({ mode: 'read-only', enforcement: 'partial' })
  })

  it('approves only a strictly wider mode and keeps the grant dedicated', async () => {
    const { policy, approval, sandbox } = confined('read-only')
    const plan = await policy.plan('bash', {
      command: 'touch file',
      sandbox_permissions: 'workspace-write',
      justification: 'create the requested file',
    }, stubExec(), config)
    expect(approval.request).toHaveBeenCalledTimes(1)
    expect(sandbox.confine).toHaveBeenCalledWith(expect.any(Array), expect.objectContaining({ mode: 'workspace-write' }))
    expect(plan.args).toEqual({ command: 'touch file' })
    expect(plan.launch).toBeDefined()

    await expect(policy.plan('bash', {
      command: 'true',
      sandbox_permissions: 'read-only',
      justification: 'not wider',
    }, stubExec(), config)).rejects.toThrow(/not strictly wider/)
  })

  it('executes nothing on malformed or rejected approval requests', async () => {
    const rejected = confined('read-only', 'rejected')
    await expect(rejected.policy.plan('bash', {
      command: 'touch file',
      sandbox_permissions: 'workspace-write',
      justification: 'needed',
    }, stubExec(), config)).rejects.toThrow(/rejected/)
    expect(rejected.sandbox.confine).not.toHaveBeenCalled()

    await expect(rejected.policy.plan('bash', {
      command: 'touch file',
      sandbox_permissions: 'workspace-write',
    }, stubExec(), config)).rejects.toThrow(/justification/)
  })

  it('fails closed when either live capability changes', async () => {
    const services: Record<string, unknown> = {}
    const pair = confined('read-only')
    services.sandbox = pair.sandbox
    services.sandboxPolicy = pair.sandboxPolicy
    const policy = createProcessPolicy(stubContext(services))
    delete services.sandboxPolicy
    await expect(policy.plan('bash', { command: 'true' }, stubExec(), config))
      .rejects.toMatchObject({ code: 'AGENTSHIM_PROCESS_POLICY_CHANGED' })
  })
})

describe('sandbox result classification', () => {
  const rules = [{ allowedExitCodes: [70], fatalSignatures: ['runner failed'], informationalLines: ['notice'] }]

  it('keeps runner failure, denial, and ordinary nonzero independent', () => {
    expect(classifyRunnerFailure('70', 'notice\nRUNNER FAILED to start', rules)).toBe(true)
    expect(classifyRunnerFailure('1', 'runner failed', rules)).toBe(false)
    expect(classifyDenial('1', 'write: Permission denied', ['permission denied'])).toBe(true)
    expect(classifyDenial('1', 'ordinary command error', ['permission denied'])).toBe(false)
    expect(classifyDenial('0', 'permission denied in harmless prose', ['permission denied'])).toBe(false)
  })
})

describe('read observation', () => {
  const target = { targetKey: 'k', displayPath: 'p' } as unknown as FsTarget
  const exec = stubExec()

  it('records presence only when the same regular file version remains', async () => {
    const emit = vi.fn()
    const ctx = { emit, fs: { stat: vi.fn(async () => ({ type: 'file', version: 'v1' })) } } as unknown as Context
    await completeReadObservation(ctx, exec, { target, pre: { type: 'file', version: 'v1' } as never })
    expect(emit).toHaveBeenCalledWith('fs/observed', target, { kind: 'present', version: 'v1' }, exec)
  })

  it('fails execution-world attribution for a non-local filesystem provider', async () => {
    const ctx = { fs: { processPath: () => 'x' } } as unknown as Context
    await expect(assertExecutionWorld(ctx, '/root')).rejects.toMatchObject({ code: 'AGENTSHIM_EXECUTION_WORLD_MISMATCH' })
  })
})
