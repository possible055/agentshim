import type { Context } from '@deepseek-ai/cordis'
import type { ContentBlock } from '@deepseek-ai/dsh-llm'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import { JobId } from '@deepseek-ai/dsh-jobs'
import { defineTool } from '@deepseek-ai/dsh-tools'
import type { ToolCallView, ToolDefinition, ToolResultView, ToolRunContext } from '@deepseek-ai/dsh-tools'
import type { ResolvedPluginConfig } from './config.ts'
import { materializeReadAttachments } from './content.ts'
import {
  beginReadObservation,
  completeReadObservation,
  createProcessPolicy,
} from './policy.ts'
import type { ProcessPolicy, SandboxAttribution } from './policy.ts'
import {
  assertExactKeys,
  assertBoolean,
  assertEnum,
  assertIntegerRange,
  assertNonEmpty,
  assertOptionalString,
  assertPattern,
  assertStringArray,
  assertStringRecord,
  bashOutputSchema,
  bashParameters,
  bashStatusOutputSchema,
  bashStatusParameters,
  escalationParameters,
  globParameters,
  processOutputSchema,
  PUBLIC_TOOL_NAMES,
  readOutputSchema,
  readParameters,
  runProgramParameters,
  textOutputSchema,
  grepParameters,
  MAX_GLOB_PATTERNS,
  MAX_GLOB_PATTERN_CHARS,
  MAX_GREP_PATTERN_CHARS,
} from './contracts.ts'
import { startBackgroundBashNative } from './jobs.ts'
import type { BackgroundJobManager } from './jobs.ts'
import type {
  NativeEngine,
  NativeGlobArgs,
  NativeGrepArgs,
  NativeReadArgs,
  NativeSandboxAttribution,
  NativeProcessStream,
} from './native.ts'
import {
  nativeBashArgs,
  nativeFailureError,
  nativeRunProgramArgs,
} from './native.ts'

export interface ToolDependencies {
  readonly ctx: Context
  readonly config: ResolvedPluginConfig
  readonly jobs: BackgroundJobManager
  readonly native: NativeEngine
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
    ...(args.office_cursor === undefined ? {} : { officeCursor: args.office_cursor as string }),
    ...(args.artifact_offset === undefined ? {} : { artifactOffset: args.artifact_offset as number }),
  }
}

function nativeGrepArgs(args: Record<string, unknown>): NativeGrepArgs {
  return {
    pattern: args.pattern as string,
    ...(args.path === undefined ? {} : { path: args.path as string }),
    ...(args.glob === undefined ? {} : { glob: args.glob as string | readonly string[] }),
    ...(args.type === undefined ? {} : { fileType: args.type as string }),
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
    pattern: args.pattern as string | readonly string[],
    ...(args.path === undefined ? {} : { path: args.path as string }),
    ...(args.include_ignored === undefined ? {} : { includeIgnored: args.include_ignored as boolean }),
    ...(args.type === undefined ? {} : { entryType: args.type as 'file' | 'directory' | 'any' }),
    ...(args.offset === undefined ? {} : { offset: args.offset as number }),
    ...(args.limit === undefined ? {} : { limit: args.limit as number }),
  }
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

function processStream(stream: NativeProcessStream) {
  return {
    text: stream.text,
    totalBytes: stream.totalBytes,
    shownBytes: stream.shownBytes,
    omittedBytes: stream.omittedBytes,
    ...(stream.artifact === undefined ? {} : {
      artifact: {
        path: stream.artifact.path,
        bytes: stream.artifact.bytes,
        complete: stream.artifact.complete,
        mediaType: 'application/octet-stream' as const,
      },
    }),
  }
}

/**
 * Map a confinement decision's attribution to the native classification
 * inputs; `undefined` when the backend supplied no evidence rules at all.
 */
function nativeAttribution(attribution: SandboxAttribution | undefined): NativeSandboxAttribution | undefined {
  if (attribution === undefined) return undefined
  const denialSignatures = attribution.denialSignatures.length === 0
    ? undefined
    : [...attribution.denialSignatures]
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
 * In-process foreground execution: prepare the final argv, wrap it through the
 * standing sandbox policy (escalation stays one-call), spawn through the
 * Engine's durable capture, and let the engine classify the settled outcome
 * against the backend's denial dialect and runner-failure rules — the adapter
 * only consumes the returned flags.
 */
async function executeProcessNative(
  deps: ToolDependencies,
  policy: ProcessPolicy,
  name: 'run_program' | 'bash',
  args: Record<string, unknown>,
  exec: ToolRunContext,
) {
  const engine = deps.native
  const prepared = name === 'run_program'
    ? engine.prepareRunProgram(nativeRunProgramArgs(args), exec.signal)
    : engine.prepareBash(nativeBashArgs(args), exec.signal)
  try {
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
      nativeAttribution(decision.attribution),
    )
    if (outcome.failure !== undefined) {
      if (outcome.runnerFailed) {
        throw new HarnessError(`sandbox runner failed before the command ran: ${outcome.text}`, 'SANDBOX_UNAVAILABLE')
      }
      throw nativeFailureError(outcome.failure)
    }
    const notices = outcome.artifacts
      .map(artifact => `Full raw ${artifact.stream}: ${artifact.path} (${artifact.bytes} bytes${artifact.complete ? '' : ', incomplete'})`)
    const text = [outcome.text, ...notices].join('\n')
    const hasClassification = typeof outcome.denied === 'boolean' && typeof outcome.runnerFailed === 'boolean'
    return {
      kind: 'foreground' as const,
      text,
      exitCode: outcome.exitCode ?? (outcome.childNonzero ? '1' : '0'),
      stdout: processStream(outcome.stdout),
      stderr: processStream(outcome.stderr),
      limitExceeded: outcome.limitExceeded ?? false,
      outcomeUncertain: outcome.outcomeUncertain ?? false,
      ...(decision.attribution === undefined || !hasClassification ? {} : {
        sandbox: {
          mode: decision.attribution.mode,
          ...(decision.attribution.enforcement === undefined ? {} : { enforcement: decision.attribution.enforcement }),
          denied: outcome.denied as boolean,
          runnerFailed: outcome.runnerFailed as boolean,
        },
      }),
    }
  } catch (error) {
    engine.discardPrepared(prepared.handle)
    throw error
  }
}

function validateReadArgs(args: Record<string, unknown> & { path: string; line_count?: number; start_line?: number; pages?: string; pdf_cursor?: string; office_cursor?: string; artifact_offset?: number }): void {
  assertExactKeys(args, Object.keys(readParameters))
  assertNonEmpty(args.path, 'path')
  assertOptionalString(args.encoding, 'encoding')
  assertOptionalString(args.pages, 'pages')
  assertOptionalString(args.pdf_cursor, 'pdf_cursor')
  assertOptionalString(args.office_cursor, 'office_cursor')
  assertEnum(args.pdf_mode, 'pdf_mode', ['auto', 'text', 'image'] as const)
  assertIntegerRange(args.line_count, 'line_count', 1, 2000)
  assertIntegerRange(args.start_line, 'start_line', 1)
  if (args.pages !== undefined && !/^[1-9][0-9]*(-[1-9][0-9]*)?$/.test(args.pages)) {
    throw new HarnessError('invalid arguments: pages must be a positive page or inclusive range', 'INVALID_ARGS')
  }
  if (args.pdf_cursor !== undefined) assertNonEmpty(args.pdf_cursor, 'pdf_cursor')
  if (args.office_cursor !== undefined) assertNonEmpty(args.office_cursor, 'office_cursor')
  assertIntegerRange(args.artifact_offset, 'artifact_offset', 0)
}

function validateGrepArgs(args: Record<string, unknown> & { pattern: string; glob?: string | readonly string[]; type?: string; context_lines?: number; limit?: number; offset?: number; encoding?: string; fallback_encoding?: string }): void {
  assertExactKeys(args, Object.keys(grepParameters))
  assertPattern(args.pattern, 'pattern', MAX_GREP_PATTERN_CHARS)
  assertEnum(args.case, 'case', ['smart', 'sensitive', 'insensitive'] as const)
  assertBoolean(args.fixed_strings, 'fixed_strings')
  assertBoolean(args.include_ignored, 'include_ignored')
  assertEnum(args.mode, 'mode', ['content', 'files', 'count'] as const)
  assertOptionalString(args.path, 'path')
  assertOptionalString(args.type, 'type')
  assertOptionalString(args.encoding, 'encoding')
  assertOptionalString(args.fallback_encoding, 'fallback_encoding')
  assertIntegerRange(args.context_lines, 'context_lines', 0, 20)
  assertIntegerRange(args.limit, 'limit', 1, 1000)
  assertIntegerRange(args.offset, 'offset', 0)
  if (args.glob !== undefined) {
    if (typeof args.glob === 'string') {
      assertPattern(args.glob, 'glob', MAX_GLOB_PATTERN_CHARS, true)
    } else if (Array.isArray(args.glob)) {
      if (args.glob.length === 0 || args.glob.length > MAX_GLOB_PATTERNS) {
        throw new HarnessError(`invalid arguments: glob must contain 1 through ${MAX_GLOB_PATTERNS} entries`, 'INVALID_ARGS')
      }
      for (const pattern of args.glob) {
        if (typeof pattern !== 'string') throw new HarnessError('invalid arguments: glob entries must be strings', 'INVALID_ARGS')
        assertPattern(pattern, 'glob entry', MAX_GLOB_PATTERN_CHARS, true)
      }
    } else {
      throw new HarnessError('invalid arguments: glob must be a string or string array', 'INVALID_ARGS')
    }
  }
  if (args.type !== undefined) assertNonEmpty(args.type, 'type')
  if (args.encoding !== undefined && args.fallback_encoding !== undefined) {
    throw new HarnessError('invalid arguments: encoding and fallback_encoding are mutually exclusive', 'INVALID_ARGS')
  }
}

function validateGlobArgs(args: Record<string, unknown> & { pattern: string | readonly string[]; limit?: number; offset?: number }): void {
  assertExactKeys(args, Object.keys(globParameters))
  assertBoolean(args.include_ignored, 'include_ignored')
  assertOptionalString(args.path, 'path')
  assertEnum(args.type, 'type', ['file', 'directory', 'any'] as const)
  if (typeof args.pattern === 'string') {
    assertPattern(args.pattern, 'pattern', MAX_GLOB_PATTERN_CHARS, true)
  } else if (Array.isArray(args.pattern)) {
    if (args.pattern.length === 0 || args.pattern.length > MAX_GLOB_PATTERNS) {
      throw new HarnessError(`invalid arguments: pattern must contain 1 through ${MAX_GLOB_PATTERNS} entries`, 'INVALID_ARGS')
    }
    for (const pattern of args.pattern) {
      if (typeof pattern !== 'string') throw new HarnessError('invalid arguments: pattern entries must be strings', 'INVALID_ARGS')
      assertPattern(pattern, 'pattern entry', MAX_GLOB_PATTERN_CHARS, true)
    }
  } else {
    throw new HarnessError('invalid arguments: pattern must be a string or string array', 'INVALID_ARGS')
  }
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
  const readScopeGuidance = deps.config.readScope === 'normal'
    ? ' Normal read scope permits the agent session cwd and configured extension roots; other paths are rejected.'
    : ' Unrestricted read scope also permits supported absolute paths.'
  const processScopeGuidance = processPolicy.advertisesEscalation
    ? ' Process access is governed only by the official DSH sandbox services when composed.'
    : ' Process access is unconfined because no official DSH sandbox service is composed.'

  const read = defineTool({
    name: 'read',
    description: `Read one file from the agent session cwd as numbered lines; use PDF/Office cursors to continue structured documents.${readScopeGuidance}`,
    parameters: readParameters,
    output: { schema: readOutputSchema, render: (_args, value) => renderRead(value) },
    timeoutMs: deps.config.toolCallTimeoutMs,
    isConcurrencySafe: () => true,
    async execute(args, exec) {
      validateReadArgs(args)
      const observation = await beginReadObservation(deps.ctx, exec, args.path)
      const native = await deps.native.readText(nativeReadArgs(args), exec.signal)
      const content = [
        { type: 'text' as const, text: native.text },
        ...native.images.map(image => ({ type: 'image' as const, data: image.data, mimeType: image.mimeType })),
      ]
      const attachments = await materializeReadAttachments(deps.ctx, exec, content)
      await completeReadObservation(deps.ctx, exec, observation)
      return { kind: 'read' as const, text: native.text, attachments }
    },
    presentCall: presentReadCall,
  })

  const grep = defineTool({
    name: 'grep',
    description: `Search files under the agent session cwd with Rust regex or fixed text; continue Partial results with offset.${readScopeGuidance}`,
    parameters: grepParameters,
    output: { schema: textOutputSchema('grep'), render: (_args, value) => textBlock(value.text) },
    timeoutMs: deps.config.toolCallTimeoutMs,
    isConcurrencySafe: () => true,
    async execute(args, exec) {
      validateGrepArgs(args)
      const native = await deps.native.grepText(nativeGrepArgs(args), exec.signal)
      return { kind: 'grep' as const, text: native.text }
    },
    presentCall: presentSearchCall('Grep'),
  })

  const glob = defineTool({
    name: 'glob',
    description: `Find files or directories under the agent session cwd with a bounded glob pattern; continue Partial results with offset.${readScopeGuidance}`,
    parameters: globParameters,
    output: { schema: textOutputSchema('glob'), render: (_args, value) => textBlock(value.text) },
    timeoutMs: deps.config.toolCallTimeoutMs,
    isConcurrencySafe: () => true,
    async execute(args, exec) {
      validateGlobArgs(args)
      const native = await deps.native.globText(nativeGlobArgs(args), exec.signal)
      return { kind: 'glob' as const, text: native.text }
    },
    presentCall: presentSearchCall('Glob'),
  })

  const runProgram = defineTool({
    name: 'run_program',
    description: `Run one executable with literal argv from the agent session cwd (override with cwd), without a shell; use bash for composition.${processScopeGuidance}`,
    parameters: processParameters,
    output: { schema: processOutputSchema, render: (_args, value) => textBlock(value.text) },
    timeoutMs: deps.config.toolCallTimeoutMs,
    async execute(args, exec) {
      assertExactKeys(args, processKeys(runProgramParameters, processPolicy.advertisesEscalation))
      assertNonEmpty(args.program, 'program')
      assertStringArray(args.args, 'args')
      assertOptionalString(args.cwd, 'cwd')
      assertStringArray(args.unset_env, 'unset_env')
      if (args.stdin !== undefined && args.stdin !== null) assertOptionalString(args.stdin, 'stdin')
      assertIntegerRange(args.timeout_ms, 'timeout_ms', 1)
      assertStringRecord(args.env, 'env')
      return executeProcessNative(deps, processPolicy, 'run_program', args, exec)
    },
    presentCall: presentRunProgramCall,
    presentResult: presentTerminalResult,
  })

  const bash = defineTool({
    name: 'bash',
    description: `Run non-interactive POSIX Bash in the agent session cwd (override with workdir); provide a short description, and use run_in_background=true for long work before polling bash_status.${processScopeGuidance}`,
    parameters: shellParameters,
    output: { schema: bashOutputSchema, render: (_args, value) => value.kind === 'background' ? textBlock(`started background job ${value.jobId}`) : textBlock(value.text) },
    timeoutMs: deps.config.toolCallTimeoutMs,
    async execute(args, exec) {
      assertExactKeys(args, processKeys(bashParameters, processPolicy.advertisesEscalation))
      assertNonEmpty(args.command, 'command')
      assertNonEmpty(args.description, 'description')
      assertOptionalString(args.workdir, 'workdir')
      assertEnum(args.msys_argument_conversion, 'msys_argument_conversion', ['default', 'disabled'] as const)
      assertBoolean(args.run_in_background, 'run_in_background')
      assertIntegerRange(args.timeoutMs, 'timeoutMs', 1)
      const commonWire = {
        command: args.command,
        ...(args.workdir === undefined ? {} : { cwd: args.workdir }),
        ...(args.msys_argument_conversion === undefined ? {} : { msys_argument_conversion: args.msys_argument_conversion }),
        ...(args.sandbox_permissions === undefined ? {} : { sandbox_permissions: args.sandbox_permissions }),
        ...(args.justification === undefined ? {} : { justification: args.justification }),
      }
      if (args.run_in_background === true) {
        const backgroundInput = {
          command: args.command,
          wire: {
            ...commonWire,
            detach: true,
            ...(args.timeoutMs === undefined ? {} : { timeout_ms: args.timeoutMs }),
          },
        }
        const jobId = await startBackgroundBashNative(deps.ctx, deps.native, processPolicy, deps.jobs, backgroundInput, exec)
        return { kind: 'background' as const, jobId }
      }
      const wire = {
        ...commonWire,
        ...(args.timeoutMs === undefined ? {} : { timeout_ms: args.timeoutMs }),
      }
      return executeProcessNative(deps, processPolicy, 'bash', wire, exec)
    },
    presentCall: presentBashCall,
    presentResult: presentTerminalResult,
  })

  const bashStatus = defineTool({
    name: 'bash_status',
    description: "Get lifecycle and final outcome metadata for a background Bash job returned by this agent's bash; repeat until terminal status.",
    parameters: bashStatusParameters,
    output: {
      schema: bashStatusOutputSchema,
      render: (_args, value) => textBlock([
        `Job: ${value.jobId}`,
        `Status: ${value.status}`,
        `Label: ${value.label}`,
        ...(value.detail === undefined ? [] : [`Detail: ${value.detail}`]),
        ...(value.exitCode === undefined ? [] : [`Exit code: ${value.exitCode ?? 'unknown'}`]),
        ...(value.sandbox === undefined ? [] : [
          `Sandbox: ${value.sandbox.mode}${value.sandbox.enforcement === undefined ? '' : ` (${value.sandbox.enforcement})`}`,
          `Sandbox denied: ${String(value.sandbox.denied)}`,
          `Sandbox runner failed: ${String(value.sandbox.runnerFailed)}`,
        ]),
        ...(value.failure === undefined ? [] : [`Failure: ${value.failure.code}: ${value.failure.message}`]),
        ...(value.artifacts === undefined || value.artifacts.length === 0 ? [] : [`Artifacts: ${value.artifacts.map(artifact => artifact.path).join(', ')}`]),
      ].join('\n')),
    },
    timeoutMs: deps.config.toolCallTimeoutMs,
    isConcurrencySafe: () => true,
    async execute(args, exec) {
      assertExactKeys(args, Object.keys(bashStatusParameters))
      assertNonEmpty(args.job_id, 'job_id')
      const jobs = deps.ctx.get('jobs')
      if (jobs === undefined) {
        throw new HarnessError('background jobs unavailable: load @deepseek-ai/dsh-jobs and @deepseek-ai/dsh-tool-jobs', 'AGENTSHIM_BACKGROUND_UNAVAILABLE')
      }
      const snapshot = jobs.get(JobId(args.job_id), exec.agent?.id)
      const native = deps.jobs.outcome(snapshot.id)
      const nativeOutcome = native?.outcome
      const hasClassification = nativeOutcome !== undefined
        && typeof nativeOutcome.denied === 'boolean'
        && typeof nativeOutcome.runnerFailed === 'boolean'
      const sandbox = nativeOutcome === undefined || native === undefined || native.sandbox === undefined || !hasClassification
        ? undefined
        : {
            mode: native.sandbox.mode,
            ...(native.sandbox.enforcement === undefined ? {} : { enforcement: native.sandbox.enforcement }),
            denied: nativeOutcome.denied as boolean,
            runnerFailed: nativeOutcome.runnerFailed as boolean,
          }
      return {
        kind: 'status' as const,
        jobId: snapshot.id,
        status: snapshot.status,
        label: snapshot.label,
        ...(snapshot.detail === undefined ? {} : { detail: snapshot.detail }),
        ...(nativeOutcome === undefined ? {} : {
          exitCode: nativeOutcome.exitCode ?? null,
          ...(nativeOutcome.failure === undefined ? {} : { failure: {
            code: nativeOutcome.failure.code,
            message: nativeOutcome.failure.message,
            retryable: nativeOutcome.failure.retryable,
          } }),
          artifacts: [...(nativeOutcome.artifacts ?? [])],
          limitExceeded: nativeOutcome.limitExceeded ?? false,
          ...(typeof nativeOutcome.denied !== 'boolean' ? {} : { denied: nativeOutcome.denied }),
          ...(typeof nativeOutcome.runnerFailed !== 'boolean' ? {} : { runnerFailed: nativeOutcome.runnerFailed }),
          ...(sandbox === undefined ? {} : { sandbox }),
        }),
        ...(native?.error === undefined ? {} : { failure: {
          code: 'AGENTSHIM_BACKGROUND_OUTCOME_FAILED',
          message: native.error,
          retryable: true,
        },
          exitCode: null,
          artifacts: [],
          denied: false,
          runnerFailed: false,
          ...(native.sandbox === undefined ? {} : { sandbox: {
            mode: native.sandbox.mode,
            ...(native.sandbox.enforcement === undefined ? {} : { enforcement: native.sandbox.enforcement }),
            denied: false,
            runnerFailed: false,
          } }),
        }),
      }
    },
    presentCall: presentBashStatusCall,
  })

  const definitions = { read, grep, glob, run_program: runProgram, bash, bash_status: bashStatus }
  return new Map(PUBLIC_TOOL_NAMES.map(name => [name, definitions[name]]))
}

interface SystemPromptOrderService {
  readonly getSectionOrder?: (name: string) => unknown
}

function getSystemPromptService(ctx: Context | undefined): SystemPromptOrderService | undefined {
  if (ctx === undefined) return undefined
  try {
    if (typeof ctx.get === 'function') {
      const fromGet = ctx.get('systemPrompt')
      if (fromGet !== undefined && fromGet !== null) return fromGet as SystemPromptOrderService
    }
  } catch {
    // Cordis proxies can throw for absent services; try the property shape too.
  }
  try {
    return (ctx as unknown as { systemPrompt?: SystemPromptOrderService }).systemPrompt ?? undefined
  } catch {
    return undefined
  }
}

function resolveOrder(ctx: Context | undefined, name: string, fallback: number): number {
  const service = getSystemPromptService(ctx)
  try {
    if (typeof service?.getSectionOrder !== 'function') return fallback
    const resolved = service.getSectionOrder(name)
    return typeof resolved === 'number' && Number.isFinite(resolved) ? resolved : fallback
  } catch {
    return fallback
  }
}

export function promptSections(ctx: Context): ReadonlyArray<{ readonly name: string; readonly order: number; readonly text: string }> {
  return [
    { name: 'tool:bash', order: resolveOrder(ctx, 'TOOL_BASH', 1000), text: 'Use run_in_background=true for long-running work.' },
    { name: 'tool:run_program', order: resolveOrder(ctx, 'TOOL_BASH', 1000) + 5, text: 'Prefer run_program for single executables with literal arguments; use bash only when shell composition is required.' },
    { name: 'tool:read', order: resolveOrder(ctx, 'TOOL_READ', 1100), text: 'Continue truncated reads by passing next_start_line as start_line.' },
    { name: 'tool:glob', order: resolveOrder(ctx, 'TOOL_GLOB', 1400), text: 'Continue truncated glob results by passing next_offset as offset.' },
    { name: 'tool:grep', order: resolveOrder(ctx, 'TOOL_GREP', 1500), text: 'Continue truncated grep results by passing next_offset as offset.' },
    { name: 'tool:bash_status', order: resolveOrder(ctx, 'TOOL_JOBS', 1600) + 5, text: 'Use bash_status to check the lifecycle status of a background Bash job.' },
  ]
}

export function pwshSectionOrder(ctx: Context): number {
  return resolveOrder(ctx, 'TOOL_PWSH', 1010)
}

export const RESTRICT_CANDIDATES = [...PUBLIC_TOOL_NAMES, 'pwsh'] as const
