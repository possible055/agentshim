import type { Context } from '@deepseek-ai/cordis'
import type { FsInfo, FsTarget } from '@deepseek-ai/dsh-fs'
import { LocalFileSystem } from '@deepseek-ai/dsh-fs-local'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import { approveEscalation, validateEscalationArgs } from '@deepseek-ai/dsh-sandbox'
import type {
  ConfinedArgv,
  RunnerFailureRule,
  SandboxExecutionPolicy,
  SandboxMode,
  SandboxPolicy,
} from '@deepseek-ai/dsh-sandbox'
import type { ToolExecution } from '@deepseek-ai/dsh-tools'
import { defaultLaunch, serverCommand } from './session.ts'
import type { ResolvedSessionConfig, SessionLaunch } from './session.ts'

export const ESCALATION_PERMISSION_FIELD = 'sandbox_permissions'
export const ESCALATION_JUSTIFICATION_FIELD = 'justification'

export function sameExecutionPath(left: string, right: string): boolean {
  return process.platform === 'win32' ? left.toLowerCase() === right.toLowerCase() : left === right
}

export async function assertExecutionWorld(ctx: Context, root: string): Promise<void> {
  const fs = ctx.fs
  if (!(fs instanceof LocalFileSystem)) {
    throw new HarnessError('dsh-agentshim: ctx.fs is no longer the local filesystem provider', 'AGENTSHIM_EXECUTION_WORLD_MISMATCH')
  }
  const processed = fs.processPath(await fs.resolve(root))
  if (!sameExecutionPath(processed, root)) {
    throw new HarnessError(`dsh-agentshim: ctx.fs maps ${JSON.stringify(root)} to ${JSON.stringify(processed)}`, 'AGENTSHIM_EXECUTION_WORLD_MISMATCH')
  }
}

export interface ReadObservation {
  readonly target: FsTarget
  readonly pre: FsInfo | undefined
}

export async function beginReadObservation(ctx: Context, exec: ToolExecution, root: string, path: string): Promise<ReadObservation> {
  await assertExecutionWorld(ctx, root)
  const cwd = exec.agent?.session.header.cwd
  const target = await ctx.fs.resolve(path, { ...(cwd === undefined ? {} : { cwd }), signal: exec.signal })
  const pre = await ctx.fs.stat(target, exec.signal)
  if (pre === undefined) ctx.emit('fs/observed', target, { kind: 'absent' }, exec)
  return { target, pre }
}

export async function completeReadObservation(ctx: Context, exec: ToolExecution, observation: ReadObservation): Promise<void> {
  if (observation.pre === undefined || observation.pre.type !== 'file') return
  const post = await ctx.fs.stat(observation.target, exec.signal)
  if (post?.type === 'file' && post.version === observation.pre.version) {
    ctx.emit('fs/observed', observation.target, { kind: 'present', version: post.version }, exec)
  }
}

function stringField(args: Record<string, unknown>, field: string): string | undefined {
  const value = args[field]
  return typeof value === 'string' ? value : undefined
}

function stripEscalationFields(args: Record<string, unknown>): Record<string, unknown> {
  return Object.fromEntries(Object.entries(args).filter(([key]) => key !== ESCALATION_PERMISSION_FIELD && key !== ESCALATION_JUSTIFICATION_FIELD))
}

export interface SandboxAttribution {
  readonly mode: SandboxMode
  readonly enforcement?: 'full' | 'partial'
  readonly denialSignatures: readonly string[]
  readonly runnerFailureRules: readonly RunnerFailureRule[]
}

export interface ProcessExecutionPlan {
  readonly args: Record<string, unknown>
  readonly launch: SessionLaunch | undefined
  readonly sandbox: SandboxAttribution | undefined
}

export interface ProcessPolicy {
  readonly advertisesEscalation: boolean
  plan(
    name: string,
    args: Record<string, unknown>,
    exec: ToolExecution,
    config: ResolvedSessionConfig,
    forceDedicated?: boolean,
  ): Promise<ProcessExecutionPlan>
  /**
   * Standing policy + escalation for one final command argv: resolves the sandbox
   * mode, runs the one-call approval when requested, and returns the wrapped argv
   * (confined) or `undefined` (danger-full-access / no sandbox composed).
   */
  wrapArgv(
    name: string,
    argv: readonly string[],
    args: Record<string, unknown>,
    exec: ToolExecution,
  ): Promise<ConfinementDecision>
}

export interface ConfinementDecision {
  readonly mode: SandboxMode
  readonly wrappedArgv: readonly string[] | undefined
  readonly attribution: SandboxAttribution | undefined
}

function confinedLaunch(config: ResolvedSessionConfig, confined: ConfinedArgv): SessionLaunch {
  const [command, ...args] = confined.argv
  if (command === undefined || command.length === 0) {
    throw new HarnessError('sandbox returned an empty command argv', 'SANDBOX_UNAVAILABLE')
  }
  return { ...defaultLaunch(config), command, args }
}

export function createProcessPolicy(ctx: Context): ProcessPolicy {
  const sandbox = ctx.get('sandbox')
  const sandboxPolicy = ctx.get('sandboxPolicy')
  if ((sandbox === undefined) !== (sandboxPolicy === undefined)) {
    throw new Error('dsh-agentshim: ctx.sandbox and ctx.sandboxPolicy must either both be composed or both be absent')
  }
  const advertisesEscalation = sandbox !== undefined
  return {
    advertisesEscalation,
    async plan(name, args, exec, config, forceDedicated = false) {
      const permissions = stringField(args, ESCALATION_PERMISSION_FIELD)
      const justification = stringField(args, ESCALATION_JUSTIFICATION_FIELD)
      validateEscalationArgs(permissions, justification)
      const currentSandbox = ctx.get('sandbox')
      const currentSandboxPolicy = ctx.get('sandboxPolicy')
      if ((currentSandbox === undefined) !== (currentSandboxPolicy === undefined)
        || (currentSandbox !== undefined) !== advertisesEscalation) {
        throw new HarnessError('dsh-agentshim: sandbox capability changed after tool registration', 'AGENTSHIM_PROCESS_POLICY_CHANGED')
      }
      if (currentSandbox === undefined || currentSandboxPolicy === undefined) {
        if (permissions !== undefined) {
          throw new HarnessError('sandbox_permissions is unavailable without a composed DSH sandbox', 'AGENTSHIM_ESCALATION_UNAVAILABLE')
        }
        return {
          args,
          launch: forceDedicated ? defaultLaunch(config) : undefined,
          sandbox: undefined,
        }
      }
      const session = exec.agent?.session
      const standing = currentSandboxPolicy.resolve(session === undefined ? {} : { session })
      const approvedMode = permissions === undefined
        ? undefined
        : await approveEscalation(
            {
              requestedMode: permissions,
              justification: justification as string,
              effectiveMode: standing.mode,
              subject: 'command',
            },
            {
              approver: ctx.get('approval'),
              agent: exec.agent,
              callId: exec.callId,
              toolName: name,
              signal: exec.signal,
            },
          )
      const policy: SandboxExecutionPolicy = approvedMode === undefined
        ? standing
        : currentSandboxPolicy.resolve(session === undefined ? { mode: approvedMode } : { session, mode: approvedMode })
      const stripped = stripEscalationFields(args)
      if (policy.mode === 'danger-full-access') {
        return {
          args: stripped,
          launch: approvedMode !== undefined || forceDedicated ? defaultLaunch(config) : undefined,
          sandbox: { mode: policy.mode, denialSignatures: [], runnerFailureRules: [] },
        }
      }
      let confined: ConfinedArgv
      try {
        confined = currentSandbox.confine(serverCommand(config), policy as SandboxPolicy)
      } catch (error) {
        throw error instanceof HarnessError
          ? error
          : new HarnessError(`sandbox confinement failed: ${String(error)}`, 'SANDBOX_UNAVAILABLE')
      }
      return {
        args: stripped,
        launch: confinedLaunch(config, confined),
        sandbox: {
          mode: policy.mode,
          enforcement: confined.enforcement,
          denialSignatures: confined.denialSignatures,
          runnerFailureRules: confined.runnerFailureRules,
        },
      }
    },
    async wrapArgv(name, argv, args, exec) {
      const permissions = stringField(args, ESCALATION_PERMISSION_FIELD)
      const justification = stringField(args, ESCALATION_JUSTIFICATION_FIELD)
      validateEscalationArgs(permissions, justification)
      const currentSandbox = ctx.get('sandbox')
      const currentSandboxPolicy = ctx.get('sandboxPolicy')
      if (currentSandbox === undefined || currentSandboxPolicy === undefined) {
        if (permissions !== undefined) {
          throw new HarnessError('sandbox_permissions is unavailable without a composed DSH sandbox', 'AGENTSHIM_ESCALATION_UNAVAILABLE')
        }
        return { mode: 'danger-full-access', wrappedArgv: undefined, attribution: undefined }
      }
      const session = exec.agent?.session
      const standing = currentSandboxPolicy.resolve(session === undefined ? {} : { session })
      const approvedMode = permissions === undefined
        ? undefined
        : await approveEscalation(
            {
              requestedMode: permissions,
              justification: justification as string,
              effectiveMode: standing.mode,
              subject: 'command',
            },
            {
              approver: ctx.get('approval'),
              agent: exec.agent,
              callId: exec.callId,
              toolName: name,
              signal: exec.signal,
            },
          )
      const policy: SandboxExecutionPolicy = approvedMode === undefined
        ? standing
        : currentSandboxPolicy.resolve(session === undefined ? { mode: approvedMode } : { session, mode: approvedMode })
      if (policy.mode === 'danger-full-access') {
        return {
          mode: policy.mode,
          wrappedArgv: undefined,
          attribution: { mode: policy.mode, denialSignatures: [], runnerFailureRules: [] },
        }
      }
      let confined: ConfinedArgv
      try {
        confined = currentSandbox.confine(argv as [string, ...string[]], policy as SandboxPolicy)
      } catch (error) {
        throw error instanceof HarnessError
          ? error
          : new HarnessError(`sandbox confinement failed: ${String(error)}`, 'SANDBOX_UNAVAILABLE')
      }
      return {
        mode: policy.mode,
        wrappedArgv: confined.argv,
        attribution: {
          mode: policy.mode,
          enforcement: confined.enforcement,
          denialSignatures: confined.denialSignatures,
          runnerFailureRules: confined.runnerFailureRules,
        },
      }
    },
  }
}

function relevantLines(stderr: string, rule: RunnerFailureRule): string[] {
  const informational = new Set((rule.informationalLines ?? []).map(line => line.toLowerCase()))
  return stderr.split(/\r?\n/).filter(line => !informational.has(line.toLowerCase()))
}

export function classifyRunnerFailure(exitCode: string | null, stderr: string, rules: readonly RunnerFailureRule[]): boolean {
  const numericExit = exitCode === null ? undefined : Number.parseInt(exitCode, 10)
  if (exitCode === '0') return false
  return rules.some(rule => {
    if (rule.allowedExitCodes !== undefined && (numericExit === undefined || !rule.allowedExitCodes.includes(numericExit))) return false
    return relevantLines(stderr, rule).some(line => rule.fatalSignatures.some(signature => line.toLowerCase().includes(signature.toLowerCase())))
  })
}

export function classifyDenial(exitCode: string | null, stderr: string, signatures: readonly string[]): boolean {
  if (exitCode === '0') return false
  const lowered = stderr.toLowerCase()
  return signatures.some(signature => lowered.includes(signature.toLowerCase()))
}
