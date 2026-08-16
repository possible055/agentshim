import type { Context } from '@deepseek-ai/cordis'
import type { FsInfo, FsTarget } from '@deepseek-ai/dsh-fs'
import { LocalFileSystem } from '@deepseek-ai/dsh-fs-local'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import { approveEscalation, validateEscalationArgs } from '@deepseek-ai/dsh-sandbox'
import type { SandboxMode } from '@deepseek-ai/dsh-sandbox'
import type { ToolExecution } from '@deepseek-ai/dsh-tools'

/** Adapter-only argument fields stripped before the MCP request (§12.5). */
export const ESCALATION_PERMISSION_FIELD = 'sandbox_permissions'
export const ESCALATION_JUSTIFICATION_FIELD = 'justification'

/**
 * The only escalation target this adapter accepts. DSH's own ladder also
 * offers `workspace-write`, but the agentshim child runs outside the DSH
 * sandbox executor, so a narrower grant would be a false safety claim.
 */
export const ESCALATION_TARGET: SandboxMode = 'danger-full-access'

export function sameExecutionPath(left: string, right: string): boolean {
  if (process.platform === 'win32') return left.toLowerCase() === right.toLowerCase()
  return left === right
}

/**
 * Per-call re-check of the startup execution-world condition: the provider
 * must still be the local one and must map the root to the same canonical
 * path, or a local agentshim read could be mistaken for a provider
 * observation.
 */
export async function assertExecutionWorld(ctx: Context, root: string): Promise<void> {
  const fs = ctx.fs
  if (!(fs instanceof LocalFileSystem)) {
    throw new HarnessError('dsh-agentshim: ctx.fs is no longer the local filesystem provider; refusing to attribute agentshim reads to it', 'AGENTSHIM_EXECUTION_WORLD_MISMATCH')
  }
  const processed = fs.processPath(await fs.resolve(root))
  if (!sameExecutionPath(processed, root)) {
    throw new HarnessError(`dsh-agentshim: ctx.fs maps the root ${JSON.stringify(root)} to ${JSON.stringify(processed)}; execution world mismatch`, 'AGENTSHIM_EXECUTION_WORLD_MISMATCH')
  }
}

export interface ReadObservation {
  readonly target: FsTarget
  readonly pre: FsInfo | undefined
}

/**
 * Resolve the read argument through the agent's session cwd and take the
 * pre-stat. A confirmed-absent target records `{ kind: 'absent' }` immediately
 * (a later create re-validates atomically); a regular file defers its
 * observation until the read completes; directories and special files record
 * nothing and let agentshim produce the user-facing error.
 */
export async function beginReadObservation(ctx: Context, exec: ToolExecution, root: string, path: string): Promise<ReadObservation> {
  await assertExecutionWorld(ctx, root)
  const cwd = exec.agent?.session.header.cwd
  const target = await ctx.fs.resolve(path, {
    ...(cwd === undefined ? {} : { cwd }),
    signal: exec.signal,
  })
  const pre = await ctx.fs.stat(target, exec.signal)
  if (pre === undefined) {
    ctx.emit('fs/observed', target, { kind: 'absent' }, exec)
  }
  return { target, pre }
}

/**
 * After a successful MCP read, record `{ kind: 'present', version }` only
 * when the target is still the same regular file at the same version. A
 * changed file leaves no observation usable by write/edit, so the next
 * mutation demands a fresh read instead of racing a stale version.
 */
export async function completeReadObservation(ctx: Context, exec: ToolExecution, observation: ReadObservation): Promise<void> {
  if (observation.pre === undefined || observation.pre.type !== 'file') return
  const post = await ctx.fs.stat(observation.target, exec.signal)
  if (post === undefined || post.type !== 'file' || post.version !== observation.pre.version) return
  ctx.emit('fs/observed', observation.target, { kind: 'present', version: post.version }, exec)
}

function stringField(args: Record<string, unknown>, field: string): string | undefined {
  const value = args[field]
  return typeof value === 'string' ? value : undefined
}

function stripEscalationFields(args: Record<string, unknown>): Record<string, unknown> {
  const stripped: Record<string, unknown> = {}
  for (const [key, value] of Object.entries(args)) {
    if (key === ESCALATION_PERMISSION_FIELD || key === ESCALATION_JUSTIFICATION_FIELD) continue
    stripped[key] = value
  }
  return stripped
}

export interface ProcessPolicy {
  /** Whether the process schemas advertise the escalation fields (§18.7: gate on a sandboxing executor). */
  readonly advertisesEscalation: boolean
  /**
   * Enforce the standing sandbox policy for one process call and return the
   * arguments to send: a confined first attempt is refused with
   * `AGENTSHIM_PROCESS_REQUIRES_FULL_ACCESS`; an approved retry loses the
   * adapter-only fields before the MCP request.
   */
  prepareArguments(name: string, args: Record<string, unknown>, exec: ToolExecution): Promise<Record<string, unknown>>
}

/**
 * The agentshim child runs outside the DSH per-call sandbox executor, so
 * process tools may only run under a standing `danger-full-access` policy or
 * after a one-shot full-access escalation through the DSH approval channel —
 * never silently under a confined mode.
 *
 * A missing `ctx.shell` is treated as unconfined: the minimal preset has no
 * one-shot executor, only a PTY realm, and already stands on
 * `danger-full-access`. Call time still re-checks the live capability.
 */
export function createProcessPolicy(ctx: Context): ProcessPolicy {
  const shell = ctx.get('shell')
  const advertisesEscalation = shell?.sandboxMode !== undefined
  if (advertisesEscalation && ctx.get('sandboxPolicy') === undefined) {
    throw new Error('dsh-agentshim: the mounted shell executor confines but ctx.sandboxPolicy is missing; refusing to advertise escalation')
  }
  return {
    advertisesEscalation,
    async prepareArguments(name, args, exec) {
      const permissions = stringField(args, ESCALATION_PERMISSION_FIELD)
      const justification = stringField(args, ESCALATION_JUSTIFICATION_FIELD)
      validateEscalationArgs(permissions, justification)
      const currentShell = ctx.get('shell')
      const currentlyConfines = currentShell?.sandboxMode !== undefined
      if (currentlyConfines !== advertisesEscalation) {
        throw new HarnessError(
          'dsh-agentshim: the mounted shell executor capability changed after the process tools were registered; retry after the composition settles or reload the plugin',
          'AGENTSHIM_PROCESS_POLICY_CHANGED',
        )
      }
      if (!currentlyConfines) {
        if (permissions !== undefined) {
          throw new HarnessError('sandbox_permissions is not available in this composition (no sandboxing executor to escalate from)', 'AGENTSHIM_ESCALATION_UNAVAILABLE')
        }
        return args
      }
      const sandboxPolicy = ctx.get('sandboxPolicy')
      if (sandboxPolicy === undefined) {
        throw new HarnessError(
          'dsh-agentshim: ctx.sandboxPolicy disappeared while a sandboxing executor is mounted; refusing to run the private process',
          'AGENTSHIM_PROCESS_POLICY_CHANGED',
        )
      }
      const standing = await sandboxPolicy.resolve(exec.agent === undefined ? {} : { session: exec.agent.session })
      const standingMode = standing?.mode
      if (permissions === undefined) {
        if (standingMode === 'danger-full-access') return args
        throw new HarnessError(
          `agentshim "${name}" runs outside the DSH sandbox and needs full access; the session policy is "${standingMode ?? 'unknown'}". Retry this exact call with the same arguments plus ${ESCALATION_PERMISSION_FIELD}: "${ESCALATION_TARGET}" and a one-sentence ${ESCALATION_JUSTIFICATION_FIELD} for the user to approve.`,
          'AGENTSHIM_PROCESS_REQUIRES_FULL_ACCESS',
        )
      }
      if (permissions !== ESCALATION_TARGET) {
        throw new HarnessError(
          `agentshim "${name}" only accepts ${ESCALATION_PERMISSION_FIELD}: "${ESCALATION_TARGET}"; narrower modes cannot confine the agentshim child process`,
          'AGENTSHIM_ESCALATION_TARGET_REJECTED',
        )
      }
      await approveEscalation(
        { requestedMode: permissions, justification: justification as string, effectiveMode: standingMode as SandboxMode, subject: 'command' },
        {
          approver: ctx.get('approval'),
          agent: exec.agent,
          callId: exec.callId,
          toolName: name,
          signal: exec.signal,
        },
      )
      return stripEscalationFields(args)
    },
  }
}

/**
 * Augment the run_program/bash schemas with the adapter-only escalation
 * fields when (and only when) a sandboxing executor is mounted. The catalog
 * fingerprint deliberately does not cover this composition-local overlay.
 */
export function augmentProcessParameters(parameters: Record<string, unknown>, policy: ProcessPolicy): Record<string, unknown> {
  if (!policy.advertisesEscalation) return parameters
  const augmented = structuredClone(parameters)
  const properties = isRecord(augmented.properties) ? augmented.properties : undefined
  if (properties === undefined) return parameters
  properties[ESCALATION_PERMISSION_FIELD] = {
    type: 'string',
    enum: [ESCALATION_TARGET],
    description: `The wider access this command needs. Only valid as a one-shot retry of a call refused with AGENTSHIM_PROCESS_REQUIRES_FULL_ACCESS; requires a justification and user approval.`,
  }
  properties[ESCALATION_JUSTIFICATION_FIELD] = {
    type: 'string',
    description: `Required with ${ESCALATION_PERMISSION_FIELD}: one sentence for the user explaining why this exact command needs full access.`,
  }
  return augmented
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
