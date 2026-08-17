import { describe, expect, it, vi } from 'vitest'
import type { Context } from '@deepseek-ai/cordis'
import type { FsTarget } from '@deepseek-ai/dsh-fs'
import type { ToolExecution } from '@deepseek-ai/dsh-tools'
import { assertLocalFileSystem, completeReadObservation, createProcessPolicy } from '../src/policy.ts'

const signal = new AbortController().signal

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
    confine: vi.fn((argv: readonly string[]) => ({
      argv: ['sandbox-runner', '--', ...argv],
      enforcement: 'partial',
      denialSignatures: ['permission denied'],
      runnerFailureRules: [{ allowedExitCodes: [70], fatalSignatures: ['runner failed'] }],
    })),
  }
  const sandboxPolicy = {
    resolve: vi.fn((request: { mode?: string } = {}) => ({
      mode: request.mode ?? mode,
      workspaceRoot: 'C:/workspace',
      sessionId: 's1',
    })),
  }
  const policy = createProcessPolicy(stubContext({ sandbox, sandboxPolicy, approval }))
  return { policy, sandbox, sandboxPolicy, approval }
}

describe('native per-call process policy', () => {
  it('returns an unwrapped argv when no sandbox is composed', async () => {
    const policy = createProcessPolicy(stubContext({}))
    await expect(policy.wrapArgv('bash', ['bash', '-c', 'true'], { command: 'true' }, stubExec()))
      .resolves.toEqual({ mode: 'danger-full-access', wrappedArgv: undefined, attribution: undefined })
  })

  it('fails plugin creation when sandbox and policy are not composed together', () => {
    expect(() => createProcessPolicy(stubContext({ sandbox: {} }))).toThrow(/must either both be composed/)
    expect(() => createProcessPolicy(stubContext({ sandboxPolicy: {} }))).toThrow(/must either both be composed/)
  })

  it('confines the exact final command argv and preserves attribution', async () => {
    const { policy, sandbox } = confined('read-only')
    const argv = ['C:/bin/node.exe', 'script.js']
    const decision = await policy.wrapArgv('run_program', argv, { program: 'node' }, stubExec())
    expect(sandbox.confine).toHaveBeenCalledWith(argv, expect.objectContaining({ mode: 'read-only' }))
    expect(decision).toMatchObject({
      mode: 'read-only',
      wrappedArgv: ['sandbox-runner', '--', ...argv],
      attribution: { enforcement: 'partial', denialSignatures: ['permission denied'] },
    })
  })

  it('approves only a strictly wider mode before confinement', async () => {
    const { policy, approval, sandbox } = confined('read-only')
    await policy.wrapArgv('bash', ['bash', '-c', 'touch file'], {
      command: 'touch file',
      sandbox_permissions: 'workspace-write',
      justification: 'create the requested file',
    }, stubExec())
    expect(approval.request).toHaveBeenCalledTimes(1)
    expect(sandbox.confine).toHaveBeenCalledWith(expect.any(Array), expect.objectContaining({ mode: 'workspace-write' }))

    await expect(policy.wrapArgv('bash', ['bash', '-c', 'true'], {
      command: 'true',
      sandbox_permissions: 'read-only',
      justification: 'not wider',
    }, stubExec())).rejects.toThrow(/not strictly wider/)
  })

  it('fails closed when a live sandbox capability changes', async () => {
    const pair = confined('read-only')
    const services: Record<string, unknown> = { sandbox: pair.sandbox, sandboxPolicy: pair.sandboxPolicy }
    const policy = createProcessPolicy(stubContext(services))
    delete services.sandboxPolicy
    await expect(policy.wrapArgv('bash', ['bash', '-c', 'true'], { command: 'true' }, stubExec()))
      .rejects.toMatchObject({ code: 'AGENTSHIM_PROCESS_POLICY_CHANGED' })
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

  it('fails execution-world attribution for a non-local filesystem provider', () => {
    const ctx = { fs: { processPath: () => 'x' } } as unknown as Context
    expect(() => assertLocalFileSystem(ctx)).toThrow(/local filesystem provider/)
  })
})
