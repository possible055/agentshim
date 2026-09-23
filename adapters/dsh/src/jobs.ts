import type { Context } from '@deepseek-ai/cordis'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import type { JobHandle, JobId, JobOutcome } from '@deepseek-ai/dsh-jobs'
import { TOOL_ABORTED } from '@deepseek-ai/dsh-tools'
import type { ToolRunContext } from '@deepseek-ai/dsh-tools'
import type { ProcessPolicy, SandboxAttribution } from './policy.ts'
import { nativeBashArgs } from './native.ts'
import type { NativeEngine, NativeJobOutcome, NativeSandboxAttribution } from './native.ts'

const JOB_OUTPUT_LIMIT_BYTES = 64 * 1024
const JOB_OUTPUT_POLL_MS = 10

interface ManagedHooks {
  cancel(reason?: string): void
  readonly done: Promise<JobOutcome>
}

export interface BackgroundSandboxObservation {
  readonly mode: string
  readonly enforcement?: 'full' | 'partial'
}

export interface BackgroundOutcomeSnapshot {
  readonly outcome?: NativeJobOutcome
  readonly sandbox?: BackgroundSandboxObservation
  readonly error?: string
}

interface BackgroundOutcomeRecord {
  readonly sandbox?: BackgroundSandboxObservation
  outcome?: NativeJobOutcome
  error?: string
}

const MAX_OUTCOME_RECORDS = 256

export class BackgroundJobManager {
  private readonly active = new Set<ManagedHooks>()
  private readonly outcomes = new Map<string, BackgroundOutcomeRecord>()

  track(hooks: ManagedHooks): ManagedHooks {
    this.active.add(hooks)
    void hooks.done.finally(() => this.active.delete(hooks)).catch(() => {})
    return hooks
  }

  rememberOutcome(
    jobId: string,
    promise: Promise<NativeJobOutcome>,
    sandbox: BackgroundSandboxObservation | undefined,
  ): void {
    const record: BackgroundOutcomeRecord = sandbox === undefined ? {} : { sandbox }
    this.outcomes.set(jobId, record)
    while (this.outcomes.size > MAX_OUTCOME_RECORDS) {
      const oldest = this.outcomes.keys().next().value
      if (oldest === undefined) break
      this.outcomes.delete(oldest)
    }
    void promise.then(
      outcome => { record.outcome = outcome },
      error => { record.error = error instanceof Error ? error.message : String(error) },
    )
  }

  outcome(jobId: string): BackgroundOutcomeSnapshot | undefined {
    const record = this.outcomes.get(jobId)
    if (record === undefined) return undefined
    return record
  }

  async dispose(): Promise<void> {
    const active = [...this.active]
    for (const hooks of active) hooks.cancel('dsh-agentshim plugin unloaded')
    await Promise.allSettled(active.map(hooks => hooks.done))
    this.outcomes.clear()
  }
}

function abortedError(): HarnessError {
  const error = new HarnessError('tool call aborted', TOOL_ABORTED)
  error.name = 'AbortError'
  return error
}

export interface BackgroundBashInput {
  readonly command: string
  readonly wire: Record<string, unknown>
}

function nativeAttribution(attribution: SandboxAttribution | undefined): NativeSandboxAttribution | undefined {
  if (attribution === undefined) return undefined
  const denialSignatures = attribution.denialSignatures.length === 0 ? undefined : [...attribution.denialSignatures]
  const runnerFailureRules = attribution.runnerFailureRules.length === 0
    ? undefined
    : attribution.runnerFailureRules.map(rule => ({
        ...(rule.allowedExitCodes === undefined ? {} : { allowedExitCodes: [...rule.allowedExitCodes] }),
        fatalSignatures: [...rule.fatalSignatures],
        ...(rule.informationalLines === undefined ? {} : { informationalLines: [...rule.informationalLines] }),
      }))
  if (denialSignatures === undefined && runnerFailureRules === undefined) return undefined
  return {
    ...(denialSignatures === undefined ? {} : { denialSignatures }),
    ...(runnerFailureRules === undefined ? {} : { runnerFailureRules }),
  }
}

/**
 * Prepare and confine before entering the DSH jobs registry, but do not spawn
 * until the registry's synchronous `run()` hook executes after its preflight.
 */
export async function startBackgroundBashNative(
  ctx: Context,
  engine: NativeEngine,
  policy: ProcessPolicy,
  manager: BackgroundJobManager,
  input: BackgroundBashInput,
  exec: ToolRunContext,
): Promise<JobId> {
  const jobs = ctx.get('jobs')
  if (jobs === undefined) {
    throw new HarnessError('background jobs unavailable: load @deepseek-ai/dsh-jobs and @deepseek-ai/dsh-tool-jobs', 'AGENTSHIM_BACKGROUND_UNAVAILABLE')
  }
  if (exec.signal.aborted) throw abortedError()
  const prepared = engine.prepareBash(nativeBashArgs(input.wire, true), exec.signal)
  try {
    const decision = await policy.wrapArgv('bash', prepared.argv, input.wire, exec)
    if (decision.mode !== 'danger-full-access' && decision.wrappedArgv === undefined) {
      throw new HarnessError('sandbox confinement returned no wrapped argv', 'SANDBOX_UNAVAILABLE')
    }
    if (exec.signal.aborted) throw abortedError()
    const wrappedArgv = decision.wrappedArgv === undefined ? undefined : [...decision.wrappedArgv]
    let nativeOutcomePromise: Promise<NativeJobOutcome> | undefined

    const jobId = await jobs.start({
      kind: 'bash',
      label: input.command,
      outputLimitBytes: JOB_OUTPUT_LIMIT_BYTES,
      ...(exec.agent === undefined ? {} : { owner: exec.agent.id }),
      run: (job: JobHandle) => {
        const handle = engine.startBackgroundPrepared(prepared.handle, wrappedArgv, nativeAttribution(decision.attribution))
        let cancelled = false
        let outputError: unknown
        const outcomePromise = handle.done()
        nativeOutcomePromise = outcomePromise

        const appendOutput = (): void => {
          if (outputError !== undefined) return
          try {
            const output = handle.readOutput()
            if (output.length > 0) job.append(output)
          } catch (error) {
            outputError = error
            try {
              handle.cancel('background output collection failed')
            } catch {
              // The native outcome still drives settlement and reports the read failure.
            }
          }
        }

        const drainOutput = (): void => {
          appendOutput()
        }

        const outputTimer = setInterval(appendOutput, JOB_OUTPUT_POLL_MS)
        appendOutput()
        const done = (async (): Promise<JobOutcome> => {
          let outcome: NativeJobOutcome | undefined
          let outcomeError: unknown
          try {
            outcome = await outcomePromise
          } catch (error) {
            outcomeError = error
          }
          try {
            drainOutput()
            if (outputError !== undefined) {
              return {
                status: 'failed',
                detail: `background output collection failed: ${outputError instanceof Error ? outputError.message : String(outputError)}`,
              }
            }
            if (outcomeError !== undefined) {
              return { status: 'failed', detail: outcomeError instanceof Error ? outcomeError.message : String(outcomeError) }
            }
            if (outcome === undefined) return { status: 'failed', detail: 'background job produced no native outcome' }
            if (outcome.status === 'killed') return { status: 'killed', detail: outcome.detail }
            if (outcome.status === 'timed_out') return { status: 'failed', detail: `timed_out: ${outcome.detail}` }
            if (outcome.status === 'failed') return { status: 'failed', detail: outcome.detail }
            return { status: 'completed', detail: outcome.detail }
          } catch (error) {
            return { status: 'failed', detail: error instanceof Error ? error.message : String(error) }
          } finally {
            clearInterval(outputTimer)
            await handle.dispose().catch(() => {})
          }
        })()
        return manager.track({
          cancel: (reason?: string) => {
            if (cancelled) return
            cancelled = true
            handle.cancel(reason ?? 'dsh job cancelled')
          },
          done,
        })
      },
    })
    if (nativeOutcomePromise === undefined) {
      throw new HarnessError('background job did not create a native outcome promise', 'AGENTSHIM_BACKGROUND_FAILED')
    }
    manager.rememberOutcome(
      jobId,
      nativeOutcomePromise,
      decision.attribution === undefined
        ? undefined
        : {
            mode: decision.attribution.mode,
            ...(decision.attribution.enforcement === undefined ? {} : { enforcement: decision.attribution.enforcement }),
          },
    )
    return jobId
  } catch (error) {
    engine.discardPrepared(prepared.handle)
    throw error
  }
}
