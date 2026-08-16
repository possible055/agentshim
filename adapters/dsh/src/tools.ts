import type { Context } from '@deepseek-ai/cordis'
import type { ContentBlock } from '@deepseek-ai/dsh-llm'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import { JobId } from '@deepseek-ai/dsh-jobs'
import { defineTool } from '@deepseek-ai/dsh-tools'
import type { ToolCallView, ToolDefinition, ToolResultView, ToolRunContext } from '@deepseek-ai/dsh-tools'
import { createSession } from './session.ts'
import type { CatalogSnapshot, AgentshimSession, ResolvedSessionConfig } from './session.ts'
import { normalizeMcpResult, materializeReadAttachments, normalizedText } from './content.ts'
import {
  beginReadObservation,
  classifyDenial,
  classifyRunnerFailure,
  completeReadObservation,
  createProcessPolicy,
} from './policy.ts'
import type { ProcessPolicy } from './policy.ts'
import {
  assertExactKeys,
  assertIntegerRange,
  assertNonEmpty,
  assertPositive,
  assertStringRecord,
  bashOutputSchema,
  bashParameters,
  bashStatusOutputSchema,
  bashStatusParameters,
  bridgeProcess,
  bridgeText,
  escalationParameters,
  globParameters,
  processOutputSchema,
  PUBLIC_TOOL_NAMES,
  readOutputSchema,
  readParameters,
  runProgramParameters,
  textOutputSchema,
  grepParameters,
} from './contracts.ts'
import type { BridgeProcess } from './contracts.ts'
import { startBackgroundBash } from './jobs.ts'
import type { BackgroundJobManager } from './jobs.ts'
import { boundedProcessText } from './capture.ts'
import type { CaptureCompletion, CaptureStore, CaptureStreamName } from './capture.ts'
import type {
  NativeBashArgs,
  NativeEngine,
  NativeGlobArgs,
  NativeGrepArgs,
  NativeReadArgs,
  NativeRunProgramArgs,
} from './native.ts'

export interface ToolDependencies {
  readonly ctx: Context
  readonly session: AgentshimSession
  readonly snapshot: CatalogSnapshot
  readonly config: ResolvedSessionConfig
  readonly jobs: BackgroundJobManager
  readonly captureStore: CaptureStore
  /** In-process engine; present only when the native addon loaded for this platform. */
  readonly native?: NativeEngine
}

function nativeReadArgs(args: Record<string, unknown>): NativeReadArgs {
  return {
    path: args.path as string,
    ...(args.encoding === undefined ? {} : { encoding: args.encoding as string }),
    ...(args.start_line === undefined ? {} : { startLine: args.start_line as number }),
    ...(args.line_count === undefined ? {} : { lineCount: args.line_count as number }),
    ...(args.pages === undefined ? {} : { pages: args.pages as string }),
    ...(args.pdf_mode === undefined ? {} : { pdfMode: args.pdf_mode as 'auto' | 'text' | 'image' }),
    ...(args.pdf_cursor === undefined ? {} : { pdfCursor: args.pdf_cursor as string }),
  }
}

function nativeGrepArgs(args: Record<string, unknown>): NativeGrepArgs {
  return {
    pattern: args.pattern as string,
    ...(args.path === undefined ? {} : { path: args.path as string }),
    ...(args.glob === undefined ? {} : { glob: args.glob as string }),
    ...(args.mode === undefined ? {} : { mode: args.mode as 'content' | 'files' | 'count' }),
    ...(args.fixed_strings === undefined ? {} : { fixedStrings: args.fixed_strings as boolean }),
    ...(args.case === undefined ? {} : { case: args.case as 'smart' | 'sensitive' | 'insensitive' }),
    ...(args.context_lines === undefined ? {} : { contextLines: args.context_lines as number }),
    ...(args.offset === undefined ? {} : { offset: args.offset as number }),
    ...(args.limit === undefined ? {} : { limit: args.limit as number }),
    ...(args.include_ignored === undefined ? {} : { includeIgnored: args.include_ignored as boolean }),
    ...(args.encoding === undefined ? {} : { encoding: args.encoding as string }),
    ...(args.fallback_encoding === undefined ? {} : { fallbackEncoding: args.fallback_encoding as string }),
  }
}

function nativeGlobArgs(args: Record<string, unknown>): NativeGlobArgs {
  return {
    pattern: args.pattern as string,
    ...(args.path === undefined ? {} : { path: args.path as string }),
    ...(args.include_ignored === undefined ? {} : { includeIgnored: args.include_ignored as boolean }),
    ...(args.type === undefined ? {} : { entryType: args.type as 'file' | 'directory' | 'any' }),
    ...(args.offset === undefined ? {} : { offset: args.offset as number }),
    ...(args.limit === undefined ? {} : { limit: args.limit as number }),
  }
}

/**
 * A read is native-eligible only when it is a plain text page: PDF reads return
 * image attachments through the bridge's structured content, and artifact paths
 * still need the bridge's exact read grant while process tools remain on the MCP
 * transport.
 */
function nativeReadEligible(deps: ToolDependencies, args: Record<string, unknown>): boolean {
  if (deps.native === undefined) return false
  if (args.pages !== undefined || args.pdf_mode !== undefined || args.pdf_cursor !== undefined) return false
  return true
}

async function nativeSearchEligible(deps: ToolDependencies, path: unknown): Promise<boolean> {
  if (deps.native === undefined) return false
  if (typeof path !== 'string') return true
  return await deps.captureStore.grant(path) === undefined
}

function textBlock(text: string): ContentBlock[] {
  return [{ type: 'text', text }]
}

function presentReadCall(args: { path: string; start_line?: number }): ToolCallView {
  return {
    card: 'generic',
    title: `Read ${args.path}`,
    kind: 'read',
    locations: [{ path: args.path, ...(args.start_line === undefined ? {} : { line: args.start_line }) }],
  }
}

function presentSearchCall(kind: 'Grep' | 'Glob'): (args: { pattern: string }) => ToolCallView {
  return args => ({ card: 'generic', title: `${kind} ${args.pattern}`, kind: 'search' })
}

function presentRunProgramCall(args: { program: string; args?: string[]; cwd?: string }): ToolCallView {
  return {
    card: 'terminal',
    title: [args.program, ...(args.args ?? [])].join(' '),
    ...(args.cwd === undefined ? {} : { cwd: args.cwd }),
  }
}

function presentBashCall(args: { command: string; description: string; workdir?: string; run_in_background?: boolean }): ToolCallView {
  if (args.run_in_background === true) {
    return { card: 'generic', title: args.command, kind: 'execute', rawInput: args.command, content: textBlock(args.description) }
  }
  return {
    card: 'terminal',
    title: args.command,
    description: args.description,
    ...(args.workdir === undefined ? {} : { cwd: args.workdir }),
  }
}

function presentBashStatusCall(args: { job_id: string }): ToolCallView {
  return { card: 'generic', title: `Status ${args.job_id}`, kind: 'execute' }
}

function presentTerminalResult(_args: unknown, result: { content: ContentBlock[]; isError: boolean }): ToolResultView | undefined {
  if (result.isError) return undefined
  return {
    card: 'terminal',
    output: result.content.flatMap(block => block.type === 'text' ? [block.text] : []).join('\n'),
  }
}

async function callBridge(
  deps: ToolDependencies,
  name: 'read' | 'grep' | 'glob' | 'run_program' | 'bash',
  args: Record<string, unknown>,
  exec: ToolRunContext,
) {
  const wire = { ...args }
  if ((name === 'read' || name === 'grep') && typeof args.path === 'string') {
    const grant = await deps.captureStore.grant(args.path)
    if (grant !== undefined) wire._agentshimReadGrant = grant
  }
  const normalized = normalizeMcpResult(await deps.session.call(name, wire, exec.signal), name)
  const text = bridgeText(normalized.structuredContent, name, normalizedText(normalized))
  return { normalized, text }
}

function nativeRunProgramArgs(args: Record<string, unknown>): NativeRunProgramArgs {
  const env: Record<string, string> = {}
  if (args.env !== undefined && typeof args.env === 'object') {
    for (const [key, value] of Object.entries(args.env as Record<string, unknown>)) {
      if (typeof value === 'string') env[key] = value
    }
  }
  return {
    program: args.program as string,
    args: Array.isArray(args.args) ? (args.args as string[]) : [],
    ...(args.cwd === undefined ? {} : { cwd: args.cwd as string }),
    ...(Object.keys(env).length === 0 ? {} : { env }),
    ...(Array.isArray(args.unset_env) ? { unsetEnv: args.unset_env as string[] } : {}),
    ...(args.stdin === undefined ? {} : { stdin: args.stdin as string }),
    ...(args.timeout_ms === undefined ? {} : { timeoutMs: args.timeout_ms as number }),
  }
}

function nativeBashArgs(wire: Record<string, unknown>): NativeBashArgs {
  return {
    command: wire.command as string,
    ...(wire.cwd === undefined ? {} : { cwd: wire.cwd as string }),
    ...(wire.timeout_ms === undefined ? {} : { timeoutMs: wire.timeout_ms as number }),
    ...(wire.msys_argument_conversion === undefined ? {} : { msysArgumentConversion: wire.msys_argument_conversion as 'enabled' | 'disabled' }),
  }
}

/**
 * In-process foreground execution for unconfined calls: prepare the final argv,
 * wrap it through the standing sandbox policy (escalation stays one-call), and
 * spawn through the Engine's durable capture. Confined and dedicated-generation
 * calls stay on the MCP bridge until the sandbox-runner E2E migrates.
 */
async function executeProcessNative(
  deps: ToolDependencies,
  policy: ProcessPolicy,
  name: 'run_program' | 'bash',
  args: Record<string, unknown>,
  wire: Record<string, unknown>,
  exec: ToolRunContext,
) {
  const engine = deps.native
  if (engine === undefined) throw new Error('dsh-agentshim: native engine unavailable')
  const prepared = name === 'run_program'
    ? engine.prepareRunProgram(nativeRunProgramArgs(args))
    : engine.prepareBash(nativeBashArgs(wire))
  const decision = await policy.wrapArgv(name, prepared.argv, args, exec)
  if (decision.mode !== 'danger-full-access' && decision.wrappedArgv === undefined) {
    throw new HarnessError('sandbox confinement returned no wrapped argv', 'SANDBOX_UNAVAILABLE')
  }
  if (exec.signal.aborted) {
    throw new HarnessError('tool call aborted before spawn', 'AGENTSHIM_CANCELLED')
  }
  const outcome = await engine.spawnPrepared(
    prepared.handle,
    decision.wrappedArgv === undefined ? undefined : [...decision.wrappedArgv],
  )
  if (outcome.errorCode !== undefined && outcome.errorCode !== 'AGENTSHIM_PROCESS_FAILED') {
    throw new HarnessError(outcome.text, outcome.errorCode)
  }
  const notices = outcome.artifacts
    .map(artifact => `Full raw ${artifact.stream}: ${artifact.path} (${artifact.bytes} bytes${artifact.complete ? '' : ', incomplete'})`)
  const text = [outcome.text, ...notices].join('\n')
  return {
    kind: 'foreground' as const,
    text,
    exitCode: outcome.childNonzero ? '1' : '0',
    stdout: { text: '', totalBytes: 0, shownBytes: 0, omittedBytes: 0 },
    stderr: { text: '', totalBytes: 0, shownBytes: 0, omittedBytes: 0 },
    ...(decision.attribution === undefined ? {} : {
      sandbox: {
        mode: decision.attribution.mode,
        ...(decision.attribution.enforcement === undefined ? {} : { enforcement: decision.attribution.enforcement }),
        denied: false,
        runnerFailed: false,
      },
    }),
  }
}

async function executeProcessBridge(
  deps: ToolDependencies,
  policy: ProcessPolicy,
  name: 'run_program' | 'bash',
  args: Record<string, unknown>,
  exec: ToolRunContext,
  forceDedicated = false,
) {
  const plan = await policy.plan(name, args, exec, deps.config, forceDedicated)
  if (!forceDedicated && plan.sandbox === undefined && deps.native !== undefined && plan.launch === undefined) {
    return executeProcessNative(deps, policy, name, plan.args, plan.args, exec)
  }
  const session = plan.launch === undefined
    ? deps.session
    : createSession(deps.config, {
        launch: plan.launch,
        reconnect: false,
        logger: { info() {}, warn() {}, error() {} },
        captureStore: deps.captureStore,
      })
  const streamNames: readonly CaptureStreamName[] = name === 'run_program' ? ['stdout', 'stderr'] : ['output']
  const capture = await deps.captureStore.begin({
    sessionId: exec.agent?.session.header.id ?? `direct-${globalThis.process.pid}`,
    callId: String(exec.callId),
    streams: streamNames,
  })
  try {
    await session.ready
    const normalized = normalizeMcpResult(await session.call(name, {
      ...plan.args,
      _agentshimCapture: capture.wire,
    }, exec.signal), name)
    const processResult: BridgeProcess = bridgeProcess(normalized.structuredContent, name, normalizedText(normalized))
    const completed = await capture.completion
    const withArtifacts = processArtifacts(processResult, completed, name)
    const runnerFailed = plan.sandbox === undefined
      ? false
      : classifyRunnerFailure(processResult.exitCode, processResult.stderr.text, plan.sandbox.runnerFailureRules)
    const denied = plan.sandbox === undefined || runnerFailed
      ? false
      : classifyDenial(processResult.exitCode, processResult.stderr.text, plan.sandbox.denialSignatures)
    return {
      kind: 'foreground' as const,
      ...withArtifacts,
      ...(plan.sandbox === undefined ? {} : {
        sandbox: {
          mode: plan.sandbox.mode,
          ...(plan.sandbox.enforcement === undefined ? {} : { enforcement: plan.sandbox.enforcement }),
          denied,
          runnerFailed,
        },
      }),
    }
  } catch (error) {
    const partial = Object.values(capture.artifacts())
      .map(artifact => `${artifact.path} (${artifact.bytes} bytes, incomplete)`)
      .join(', ')
    await capture.abort(error instanceof Error ? error.message : String(error))
    if (plan.sandbox !== undefined && classifyRunnerFailure(null, session.stderrTail(), plan.sandbox.runnerFailureRules)) {
      throw new HarnessError(`sandbox runner failed before AgentShim initialized: ${session.stderrTail()}`, 'SANDBOX_UNAVAILABLE')
    }
    if (partial !== '' && error instanceof Error) {
      error.message = `${error.message}; partial raw capture: ${partial}`
    }
    throw error
  } finally {
    if (session !== deps.session) await session.dispose()
  }
}

function processArtifacts(
  process: BridgeProcess,
  completed: CaptureCompletion,
  tool: 'run_program' | 'bash',
): BridgeProcess {
  const stdoutArtifact = tool === 'run_program' ? completed.artifacts.stdout : completed.artifacts.output
  const stderrArtifact = tool === 'run_program' ? completed.artifacts.stderr : undefined
  const stdout = { ...process.stdout, ...(stdoutArtifact === undefined ? {} : { artifact: stdoutArtifact }) }
  const stderr = { ...process.stderr, ...(stderrArtifact === undefined ? {} : { artifact: stderrArtifact }) }
  const notices = [
    ...(stdoutArtifact === undefined ? [] : [`Full raw ${tool === 'bash' ? 'output' : 'stdout'}: ${stdoutArtifact.path} (${stdoutArtifact.bytes} bytes)`]),
    ...(stderrArtifact === undefined ? [] : [`Full raw stderr: ${stderrArtifact.path} (${stderrArtifact.bytes} bytes)`]),
  ]
  return { ...process, text: boundedProcessText(process.text, notices), stdout, stderr }
}

function validateReadArgs(args: Record<string, unknown> & { path: string; line_count?: number; start_line?: number; pages?: string; pdf_cursor?: string }): void {
  assertExactKeys(args, Object.keys(readParameters))
  assertNonEmpty(args.path, 'path')
  assertIntegerRange(args.line_count, 'line_count', 1, 2000)
  assertIntegerRange(args.start_line, 'start_line', 1)
  if (args.pages !== undefined && !/^[1-9][0-9]*(-[1-9][0-9]*)?$/.test(args.pages)) {
    throw new HarnessError('invalid arguments: pages must be a positive page or inclusive range', 'INVALID_ARGS')
  }
  if (args.pdf_cursor !== undefined) assertNonEmpty(args.pdf_cursor, 'pdf_cursor')
}

function validateGrepArgs(args: Record<string, unknown> & { pattern: string; context_lines?: number; limit?: number; offset?: number; encoding?: string; fallback_encoding?: string }): void {
  assertExactKeys(args, Object.keys(grepParameters))
  assertIntegerRange(args.context_lines, 'context_lines', 0, 20)
  assertIntegerRange(args.limit, 'limit', 1, 1000)
  assertIntegerRange(args.offset, 'offset', 0)
  if (args.encoding !== undefined && args.fallback_encoding !== undefined) {
    throw new HarnessError('invalid arguments: encoding and fallback_encoding are mutually exclusive', 'INVALID_ARGS')
  }
}

function validateGlobArgs(args: Record<string, unknown> & { pattern: string; limit?: number; offset?: number }): void {
  assertExactKeys(args, Object.keys(globParameters))
  assertNonEmpty(args.pattern, 'pattern')
  assertIntegerRange(args.limit, 'limit', 1, 1000)
  assertIntegerRange(args.offset, 'offset', 0)
}

function processKeys(base: Record<string, unknown>, escalation: boolean): string[] {
  return [...Object.keys(base), ...(escalation ? Object.keys(escalationParameters) : [])]
}

function renderRead(value: { text: string; attachments: Array<{ attachmentId: string; mediaType: string; bytes: number; width: number; height: number; name?: string }> }): ContentBlock[] {
  return [
    { type: 'text', text: value.text },
    ...value.attachments.map(attachment => ({ type: 'image' as const, attachment: attachment as never })),
  ]
}

export function buildToolDefinitions(deps: ToolDependencies): ReadonlyMap<string, ToolDefinition> {
  const processPolicy = createProcessPolicy(deps.ctx)
  const processParameters = {
    ...runProgramParameters,
    ...(processPolicy.advertisesEscalation ? escalationParameters : {}),
  }
  const shellParameters = {
    ...bashParameters,
    ...(processPolicy.advertisesEscalation ? escalationParameters : {}),
  }

  const read = defineTool({
    name: 'read',
    description: 'Read one local file as numbered lines, including an exact published process artifact path, or render PDF text and images. Native text pages are bounded to 50,000 bytes and continuations are explicit.',
    parameters: readParameters,
    output: { schema: readOutputSchema, render: (_args, value) => renderRead(value) },
    timeoutMs: deps.config.toolCallTimeoutMs,
    isConcurrencySafe: () => true,
    async execute(args, exec) {
      validateReadArgs(args)
      const observation = await beginReadObservation(deps.ctx, exec, deps.config.root, args.path)
      let text: string
      let attachments: Awaited<ReturnType<typeof materializeReadAttachments>> = []
      if (nativeReadEligible(deps, args)) {
        const native = await deps.native?.readText(nativeReadArgs(args))
        if (native === undefined) throw new Error('dsh-agentshim: native engine vanished before read')
        text = native.text
      } else {
        const { normalized } = await callBridge(deps, 'read', args, exec)
        text = bridgeText(normalized.structuredContent, 'read', normalizedText(normalized))
        attachments = await materializeReadAttachments(deps.ctx, exec, normalized.content)
      }
      await completeReadObservation(deps.ctx, exec, observation)
      return { kind: 'read' as const, text, attachments }
    },
    presentCall: presentReadCall,
  })

  const grep = defineTool({
    name: 'grep',
    description: 'Search local file contents, including one exact published process artifact path, with Rust regular expressions or fixed strings. Native result pages are bounded to 50,000 bytes with explicit continuation.',
    parameters: grepParameters,
    output: { schema: textOutputSchema('grep'), render: (_args, value) => textBlock(value.text) },
    timeoutMs: deps.config.toolCallTimeoutMs,
    isConcurrencySafe: () => true,
    async execute(args, exec) {
      validateGrepArgs(args)
      if (await nativeSearchEligible(deps, args.path)) {
        const native = await deps.native?.grepText(nativeGrepArgs(args))
        if (native === undefined) throw new Error('dsh-agentshim: native engine vanished before grep')
        return { kind: 'grep' as const, text: native.text }
      }
      const { text } = await callBridge(deps, 'grep', args, exec)
      return { kind: 'grep' as const, text }
    },
    presentCall: presentSearchCall('Grep'),
  })

  const glob = defineTool({
    name: 'glob',
    description: 'Find local filesystem paths with a glob pattern. Native result pages are bounded to 50,000 bytes with explicit continuation; private capture roots are never enumerable.',
    parameters: globParameters,
    output: { schema: textOutputSchema('glob'), render: (_args, value) => textBlock(value.text) },
    timeoutMs: deps.config.toolCallTimeoutMs,
    isConcurrencySafe: () => true,
    async execute(args, exec) {
      validateGlobArgs(args)
      if (deps.native !== undefined) {
        const native = await deps.native.globText(nativeGlobArgs(args))
        return { kind: 'glob' as const, text: native.text }
      }
      const { text } = await callBridge(deps, 'glob', args, exec)
      return { kind: 'glob' as const, text }
    },
    presentCall: presentSearchCall('Glob'),
  })

  const runProgram = defineTool({
    name: 'run_program',
    description: 'Run one local program with literal arguments. Model text is a 50,000-byte head/tail preview; larger or non-text output is preserved at the returned raw artifact path. File effects follow the per-call DSH sandbox policy.',
    parameters: processParameters,
    output: { schema: processOutputSchema, render: (_args, value) => textBlock(value.text) },
    timeoutMs: deps.config.toolCallTimeoutMs,
    async execute(args, exec) {
      assertExactKeys(args, processKeys(runProgramParameters, processPolicy.advertisesEscalation))
      assertNonEmpty(args.program, 'program')
      assertPositive(args.timeout_ms, 'timeout_ms')
      assertStringRecord(args.env, 'env')
      return executeProcessBridge(deps, processPolicy, 'run_program', args, exec)
    },
    presentCall: presentRunProgramCall,
    presentResult: presentTerminalResult,
  })

  const bash = defineTool({
    name: 'bash',
    description: 'Run a fresh non-interactive POSIX Bash command. Model text is a 50,000-byte head/tail preview and complete larger or non-text output is preserved as a raw artifact. Set run_in_background for a DSH-owned job.',
    parameters: shellParameters,
    output: { schema: bashOutputSchema, render: (_args, value) => value.kind === 'background' ? textBlock(`started background job ${value.jobId}`) : textBlock(value.text) },
    timeoutMs: deps.config.toolCallTimeoutMs,
    async execute(args, exec) {
      assertExactKeys(args, processKeys(bashParameters, processPolicy.advertisesEscalation))
      assertNonEmpty(args.command, 'command')
      assertNonEmpty(args.description, 'description')
      assertPositive(args.timeoutMs, 'timeoutMs')
      const commonWire = {
        command: args.command,
        ...(args.workdir === undefined ? {} : { cwd: args.workdir }),
        ...(args.msys_argument_conversion === undefined ? {} : { msys_argument_conversion: args.msys_argument_conversion }),
        ...(args.sandbox_permissions === undefined ? {} : { sandbox_permissions: args.sandbox_permissions }),
        ...(args.justification === undefined ? {} : { justification: args.justification }),
      }
      if (args.run_in_background === true) {
        if (args.timeoutMs !== undefined) {
          throw new HarnessError('invalid arguments: timeoutMs does not apply to a background job', 'INVALID_ARGS')
        }
        const jobId = await startBackgroundBash(deps.ctx, deps.config, processPolicy, deps.jobs, deps.captureStore, {
          command: args.command,
          wire: { ...commonWire, detach: true },
        }, exec)
        return { kind: 'background' as const, jobId }
      }
      const wire = {
        ...commonWire,
        ...(args.timeoutMs === undefined ? {} : { timeout_ms: args.timeoutMs }),
      }
      return executeProcessBridge(deps, processPolicy, 'bash', wire, exec)
    },
    presentCall: presentBashCall,
    presentResult: presentTerminalResult,
  })

  const bashStatus = defineTool({
    name: 'bash_status',
    description: 'Return the non-consuming DSH lifecycle snapshot for a background Bash job. Available only together with bash.',
    parameters: bashStatusParameters,
    output: {
      schema: bashStatusOutputSchema,
      render: (_args, value) => textBlock([
        `Job: ${value.jobId}`,
        `Status: ${value.status}`,
        `Label: ${value.label}`,
        ...(value.detail === undefined ? [] : [`Detail: ${value.detail}`]),
      ].join('\n')),
    },
    timeoutMs: deps.config.toolCallTimeoutMs,
    isConcurrencySafe: () => true,
    async execute(args, exec) {
      assertExactKeys(args, Object.keys(bashStatusParameters))
      assertNonEmpty(args.job_id, 'job_id')
      const jobs = deps.ctx.get('jobs')
      if (jobs === undefined) {
        throw new Error('background jobs unavailable: load @deepseek-ai/dsh-jobs and @deepseek-ai/dsh-tool-jobs')
      }
      const snapshot = jobs.get(JobId(args.job_id), exec.agent)
      return {
        kind: 'status' as const,
        jobId: snapshot.id,
        status: snapshot.status,
        label: snapshot.label,
        ...(snapshot.detail === undefined ? {} : { detail: snapshot.detail }),
      }
    },
    presentCall: presentBashStatusCall,
  })

  const definitions = { read, grep, glob, run_program: runProgram, bash, bash_status: bashStatus }
  return new Map(PUBLIC_TOOL_NAMES.map(name => [name, definitions[name]]))
}

export function promptSections(): ReadonlyArray<{ readonly name: string; readonly order: number; readonly text: string }> {
  return [
    { name: 'tool:read', order: 100, text: 'Continue truncated reads with the exact continuation argument returned by AgentShim, such as next_start_line passed as start_line.' },
    { name: 'tool:glob', order: 103, text: 'Continue a truncated glob result with its next_offset rather than restarting.' },
    { name: 'tool:grep', order: 104, text: 'Continue a truncated grep result with its next_offset rather than restarting.' },
    { name: 'tool:run_program', order: 104.5, text: 'Prefer run_program for one executable with literal argv; use bash only for shell composition.' },
    { name: 'tool:bash', order: 105, text: 'Each Bash call is fresh. Use run_in_background for long work, then job_output and job_kill with the returned DSH job id.' },
    { name: 'tool:bash_status', order: 105.5, text: 'Use bash_status for a non-consuming lifecycle snapshot of a DSH Bash job; use job_output to consume output and job_kill to stop it.' },
  ]
}

export const RESTRICT_CANDIDATES = [...PUBLIC_TOOL_NAMES, 'pwsh'] as const
