import type { Context } from '@deepseek-ai/cordis'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import type { JobId, JobOutcome } from '@deepseek-ai/dsh-jobs'
import { TOOL_ABORTED } from '@deepseek-ai/dsh-tools'
import type { ToolRunContext } from '@deepseek-ai/dsh-tools'
import type { ProcessPolicy } from './policy.ts'
import { nativeBashArgs } from './native.ts'
import type { NativeEngine } from './native.ts'

const JOB_OUTPUT_LIMIT_BYTES = 64 * 1024

interface ManagedHooks {
  cancel(reason?: string): void
  readonly done: Promise<JobOutcome>
  readOutput(): string
}

export class BackgroundJobManager {
  private readonly active = new Set<ManagedHooks>()

  track(hooks: ManagedHooks): ManagedHooks {
    this.active.add(hooks)
    void hooks.done.finally(() => this.active.delete(hooks)).catch(() => {})
    return hooks
  }

  async dispose(): Promise<void> {
    const active = [...this.active]
    for (const hooks of active) hooks.cancel('dsh-agentshim plugin unloaded')
    await Promise.allSettled(active.map(hooks => hooks.done))
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

    return await jobs.start({
    kind: 'bash',
    label: input.command,
    outputLimitBytes: JOB_OUTPUT_LIMIT_BYTES,
    ...(exec.agent === undefined ? {} : { owner: exec.agent }),
    run: () => {
      const handle = engine.startBackgroundPrepared(prepared.handle, wrappedArgv)
      let cancelled = false
      const done = (async (): Promise<JobOutcome> => {
        try {
          const outcome = await handle.done()
          if (outcome.status === 'killed') return { status: 'killed', detail: outcome.detail }
          if (outcome.status === 'timed_out') return { status: 'failed', detail: `timed_out: ${outcome.detail}` }
          if (outcome.status === 'failed') return { status: 'failed', detail: outcome.detail }
          return { status: 'completed', detail: outcome.detail }
        } catch (error) {
          return { status: 'failed', detail: error instanceof Error ? error.message : String(error) }
        } finally {
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
        readOutput: () => handle.readOutput(),
      })
    },
    })
  } catch (error) {
    engine.discardPrepared(prepared.handle)
    throw error
  }
}
