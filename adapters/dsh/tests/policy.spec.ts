import { describe, expect, it, vi } from 'vitest'
import type { Context } from '@deepseek-ai/cordis'
import type { FsTarget } from '@deepseek-ai/dsh-fs'
import type { ToolExecution } from '@deepseek-ai/dsh-tools'
import { assertExecutionWorld, completeReadObservation, createProcessPolicy } from '../src/policy.ts'

const signal = new AbortController().signal

function stubContext(services: Record<string, unknown>): Context {
  return { get: (name: string) => services[name] } as unknown as Context
}

function stubExec(overrides: Record<string, unknown> = {}): ToolExecution {
  return {
    signal,
    callId: 'c1',
    name: 'bash',
    arguments: {},
    agent: { session: {} },
    ...overrides,
  } as unknown as ToolExecution
}

describe('createProcessPolicy', () => {
  it('runs directly with no sandboxing executor, refusing fields that still arrive', async () => {
    const policy = createProcessPolicy(stubContext({ shell: {} }))
    expect(policy.advertisesEscalation).toBe(false)
    const args = { command: 'true' }
    await expect(policy.prepareArguments('bash', args, stubExec())).resolves.toBe(args)
    await expect(policy.prepareArguments('bash', { command: 'true', sandbox_permissions: 'danger-full-access', justification: 'x' }, stubExec()))
      .rejects.toMatchObject({ code: 'CODEXSHIM_ESCALATION_UNAVAILABLE' })
  })

  it('fails plugin creation when an executor confines without a sandbox policy', () => {
    expect(() => createProcessPolicy(stubContext({ shell: { sandboxMode: 'workspace-write' } }))).toThrow(/sandboxPolicy is missing/)
  })

  it('fails plugin creation when the required shell service is missing', () => {
    expect(() => createProcessPolicy(stubContext({}))).toThrow(/ctx\.shell is missing/)
  })

  function confinedPolicy(mode: string, outcome: string = 'allowed-once') {
    const approval = { request: vi.fn(async () => outcome) }
    const sandboxPolicy = { resolve: vi.fn(async () => ({ mode })) }
    const policy = createProcessPolicy(stubContext({
      shell: { sandboxMode: 'workspace-write' },
      sandboxPolicy,
      approval,
    }))
    return { policy, approval, sandboxPolicy }
  }

  it('refuses a confined first attempt and teaches the exact retry', async () => {
    const { policy } = confinedPolicy('read-only')
    const error = await policy.prepareArguments('bash', { command: 'cargo test' }, stubExec())
      .then(() => { throw new Error('expected rejection') }, reason => reason)
    expect(error).toMatchObject({ code: 'CODEXSHIM_PROCESS_REQUIRES_FULL_ACCESS' })
    expect(error.message).toContain('sandbox_permissions')
    expect(error.message).toContain('danger-full-access')
    expect(error.message).toContain('justification')
  })

  it('runs a standing danger-full-access session without fields', async () => {
    const { policy } = confinedPolicy('danger-full-access')
    const args = { command: 'true' }
    await expect(policy.prepareArguments('bash', args, stubExec())).resolves.toBe(args)
  })

  it('approves a paired retry, strips the adapter-only fields, and forwards the standing mode', async () => {
    const { policy, approval, sandboxPolicy } = confinedPolicy('workspace-write')
    const prepared = await policy.prepareArguments('run_program', {
      program: 'cargo',
      args: ['test'],
      sandbox_permissions: 'danger-full-access',
      justification: 'the suite writes build artifacts',
    }, stubExec())
    expect(prepared).toEqual({ program: 'cargo', args: ['test'] })
    expect(approval.request).toHaveBeenCalledTimes(1)
    expect(sandboxPolicy.resolve).toHaveBeenCalledTimes(1)
  })

  it('rejects a narrower escalation target and unpaired fields', async () => {
    const { policy } = confinedPolicy('workspace-write')
    await expect(policy.prepareArguments('bash', { command: 'x', sandbox_permissions: 'workspace-write', justification: 'n/a' }, stubExec()))
      .rejects.toMatchObject({ code: 'CODEXSHIM_ESCALATION_TARGET_REJECTED' })
    await expect(policy.prepareArguments('bash', { command: 'x', sandbox_permissions: 'danger-full-access' }, stubExec()))
      .rejects.toThrow(/justification/)
    await expect(policy.prepareArguments('bash', { command: 'x', justification: 'orphan' }, stubExec()))
      .rejects.toThrow(/only valid together/)
  })

  it('fails closed when the user rejects the escalation', async () => {
    const { policy } = confinedPolicy('workspace-write', 'rejected')
    await expect(policy.prepareArguments('bash', { command: 'x', sandbox_permissions: 'danger-full-access', justification: 'needed' }, stubExec()))
      .rejects.toThrow()
  })

  it('fails closed when the executor capability changes after registration', async () => {
    const services: Record<string, unknown> = {
      shell: { sandboxMode: 'workspace-write' },
      sandboxPolicy: { resolve: vi.fn(async () => ({ mode: 'workspace-write' })) },
    }
    const policy = createProcessPolicy(stubContext(services))
    services.shell = {}
    await expect(policy.prepareArguments('bash', { command: 'true' }, stubExec()))
      .rejects.toMatchObject({ code: 'CODEXSHIM_PROCESS_POLICY_CHANGED' })
  })

  it('resolves the current sandbox policy for every call and fails if it disappears', async () => {
    const first = { resolve: vi.fn(async () => ({ mode: 'danger-full-access' })) }
    const second = { resolve: vi.fn(async () => ({ mode: 'danger-full-access' })) }
    const services: Record<string, unknown> = {
      shell: { sandboxMode: 'workspace-write' },
      sandboxPolicy: first,
    }
    const policy = createProcessPolicy(stubContext(services))
    await policy.prepareArguments('bash', { command: 'one' }, stubExec())
    services.sandboxPolicy = second
    await policy.prepareArguments('bash', { command: 'two' }, stubExec())
    expect(first.resolve).toHaveBeenCalledTimes(1)
    expect(second.resolve).toHaveBeenCalledTimes(1)

    delete services.sandboxPolicy
    await expect(policy.prepareArguments('bash', { command: 'three' }, stubExec()))
      .rejects.toMatchObject({ code: 'CODEXSHIM_PROCESS_POLICY_CHANGED' })
  })
})

describe('completeReadObservation', () => {
  function observationContext(stats: Array<{ type: string; version: string } | undefined>) {
    const emit = vi.fn()
    const stat = vi.fn(async () => stats.shift())
    const ctx = { emit, fs: { stat } } as unknown as Context
    return { ctx, emit, stat }
  }
  const target = { targetKey: 'k', displayPath: 'p' } as unknown as FsTarget
  const exec = stubExec()

  it('records presence only when the same regular file is still at the same version', async () => {
    const { ctx, emit } = observationContext([{ type: 'file', version: 'v1' }, { type: 'file', version: 'v1' }])
    await completeReadObservation(ctx, exec, { target, pre: { type: 'file', version: 'v1' } as never })
    expect(emit).toHaveBeenCalledWith('fs/observed', target, { kind: 'present', version: 'v1' }, exec)
  })

  it('records nothing when the version moved, the file vanished, or the pre-stat was not a file', async () => {
    const changed = observationContext([{ type: 'file', version: 'v2' }])
    await completeReadObservation(changed.ctx, exec, { target, pre: { type: 'file', version: 'v1' } as never })
    expect(changed.emit).not.toHaveBeenCalled()

    const gone = observationContext([undefined])
    await completeReadObservation(gone.ctx, exec, { target, pre: { type: 'file', version: 'v1' } as never })
    expect(gone.emit).not.toHaveBeenCalled()

    const notFile = observationContext([])
    await completeReadObservation(notFile.ctx, exec, { target, pre: { type: 'directory', version: 'v1' } as never })
    expect(notFile.stat).not.toHaveBeenCalled()
    expect(notFile.emit).not.toHaveBeenCalled()
  })
})

describe('assertExecutionWorld', () => {
  it('fails with CODEXSHIM_EXECUTION_WORLD_MISMATCH for a non-local provider', async () => {
    const ctx = { fs: { processPath: () => 'x' } } as unknown as Context
    await expect(assertExecutionWorld(ctx, '/root')).rejects.toMatchObject({ code: 'CODEXSHIM_EXECUTION_WORLD_MISMATCH' })
  })
})
