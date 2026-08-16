import type { Context } from '@deepseek-ai/cordis'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import type { JobId, JobOutcome } from '@deepseek-ai/dsh-jobs'
import { TOOL_ABORTED } from '@deepseek-ai/dsh-tools'
import type { ToolRunContext } from '@deepseek-ai/dsh-tools'
import { bridgeJobStart, bridgeJobStatus } from './contracts.ts'
import { normalizeMcpResult } from './content.ts'
import type { ProcessPolicy } from './policy.ts'
import { createSession } from './session.ts'
import type { ResolvedSessionConfig } from './session.ts'
import type { CaptureCompletion, CaptureHandle, CaptureStore } from './capture.ts'

const JOB_OUTPUT_LIMIT_BYTES = 64 * 1024
const BUFFER_LIMIT_BYTES = 1024 * 1024
const TERMINAL_STATES = new Set(['completed', 'terminated', 'outcome_uncertain'])

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

class SyncOutputBuffer {
  private text = ''
  private droppedBytes = 0

  push(value: string): void {
    this.text += value
    const bytes = Buffer.byteLength(this.text)
    if (bytes <= BUFFER_LIMIT_BYTES) return
    const encoded = Buffer.from(this.text)
    const overflow = encoded.byteLength - BUFFER_LIMIT_BYTES
    this.droppedBytes += overflow
    this.text = encoded.subarray(overflow).toString('utf8')
  }

  read(): string {
    const marker = this.droppedBytes === 0
      ? ''
      : `[agentshim: ${this.droppedBytes} live output bytes were omitted from this view; the raw capture artifact is lossless]\n`
    const value = marker + this.text
    this.text = ''
    this.droppedBytes = 0
    return value
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

export async function startBackgroundBash(
  ctx: Context,
  config: ResolvedSessionConfig,
  policy: ProcessPolicy,
  manager: BackgroundJobManager,
  captureStore: CaptureStore,
  input: BackgroundBashInput,
  exec: ToolRunContext,
): Promise<JobId> {
  const jobs = ctx.get('jobs')
  if (jobs === undefined) {
    throw new Error('background jobs unavailable: load @deepseek-ai/dsh-jobs and @deepseek-ai/dsh-tool-jobs')
  }
  if (exec.signal.aborted) throw abortedError()
  const plan = await policy.plan('bash', input.wire, exec, config, true)
  if (exec.signal.aborted) throw abortedError()
  if (plan.launch === undefined) throw new Error('dsh-agentshim: background execution requires a dedicated generation')
  const launch = plan.launch

  return jobs.start({
    kind: 'bash',
    label: input.command,
    outputLimitBytes: JOB_OUTPUT_LIMIT_BYTES,
    ...(exec.agent === undefined ? {} : { owner: exec.agent }),
    run: () => {
      const controller = new AbortController()
      const output = new SyncOutputBuffer()
      const session = createSession(config, {
        launch,
        reconnect: false,
        logger: { info() {}, warn() {}, error() {} },
        captureStore,
      })
      let cancelRequested = false
      let privateJobId: string | undefined
      let terminal = false
      let capture: CaptureHandle | undefined
      let captureResult: CaptureCompletion | undefined
      const pendingOutput: string[] = []
      const emitOutput = (text: string): void => {
        if (privateJobId === undefined) pendingOutput.push(text)
        else output.push(text)
      }

      const done = (async (): Promise<JobOutcome> => {
        try {
          await session.ready
          if (cancelRequested) return { status: 'killed', detail: 'cancelled before process launch' }
          capture = await captureStore.begin({
            sessionId: `${config.root}:${globalThis.process.pid}`,
            callId: `background-bash-${Date.now()}`,
            streams: ['output'],
            background: true,
            onLiveText: emitOutput,
            onPublished: artifacts => {
              for (const artifact of artifacts) {
                emitOutput(`\n[agentshim raw capture: ${artifact.path} (${artifact.bytes} bytes, incomplete)]\n`)
              }
            },
          })
          void capture.completion.then(result => { captureResult = result })
          const launch = normalizeMcpResult(await session.call('bash', {
            ...plan.args,
            _agentshimCapture: capture.wire,
          }, controller.signal), 'bash')
          privateJobId = bridgeJobStart(launch.structuredContent)
          for (const text of pendingOutput.splice(0)) output.push(text)
          for (;;) {
            if (cancelRequested) {
              normalizeMcpResult(
                await session.call('bash', { action: 'terminate', job_id: privateJobId }, new AbortController().signal),
                'bash',
              )
            }
            const raw = normalizeMcpResult(await session.call('bash_status', {
              job_id: privateJobId,
              wait_ms: 250,
              tail_bytes: 0,
            }, controller.signal), 'bash_status')
            const status = bridgeJobStatus(raw.structuredContent, privateJobId)
            if (captureResult !== undefined && !captureResult.complete) {
              throw new HarnessError(
                captureResult.error ?? 'agentshim capture failed',
                captureResult.error?.includes('LIMIT_EXCEEDED') === true
                  ? 'AGENTSHIM_CAPTURE_LIMIT_EXCEEDED'
                  : 'AGENTSHIM_CAPTURE_IO_FAILED',
              )
            }
            if (!TERMINAL_STATES.has(status.state)) continue
            captureResult = await capture.completion
            if (!captureResult.complete) {
              throw new HarnessError(captureResult.error ?? 'agentshim capture failed', 'AGENTSHIM_CAPTURE_IO_FAILED')
            }
            terminal = true
            if (status.state === 'outcome_uncertain') {
              return { status: 'failed', detail: 'process-tree outcome uncertain' }
            }
            if (cancelRequested || status.state === 'terminated') {
              return { status: 'killed', detail: status.exitCode === null ? 'terminated' : `exit code: ${status.exitCode}` }
            }
            return { status: 'completed', detail: `exit code: ${status.exitCode ?? 'unknown'}` }
          }
        } catch (error) {
          if (cancelRequested && privateJobId === undefined) return { status: 'killed', detail: 'cancelled during startup' }
          output.push(`\n[agentshim job failed: ${error instanceof Error ? error.message : String(error)}]\n`)
          return { status: 'failed', detail: error instanceof Error ? error.message : String(error) }
        } finally {
          if (!terminal && privateJobId !== undefined) {
            try {
              normalizeMcpResult(
                await session.call('bash', { action: 'terminate', job_id: privateJobId }, new AbortController().signal),
                'bash',
              )
            } catch (error) {
              output.push(`\n[agentshim terminate failed: ${error instanceof Error ? error.message : String(error)}]\n`)
            }
          }
          if (capture !== undefined && captureResult === undefined) {
            await capture.abort(cancelRequested ? 'background job cancelled' : 'background job failed')
          }
          await session.dispose()
        }
      })()

      return manager.track({
        cancel: () => {
          if (cancelRequested) return
          cancelRequested = true
          if (privateJobId === undefined) controller.abort('job cancelled before launch')
        },
        done,
        readOutput: () => output.read(),
      })
    },
  })
}
