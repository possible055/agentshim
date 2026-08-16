import type { Context } from '@deepseek-ai/cordis'
import type { ContentBlock } from '@deepseek-ai/dsh-llm'
import type { JsonSchemaNode, ToolCallView, ToolDefinition, ToolResultView } from '@deepseek-ai/dsh-tools'
import type { ToolRunContext } from '@deepseek-ai/dsh-tools'
import type { CatalogSnapshot, CodexshimSession, CodexshimToolName, ResolvedSessionConfig } from './session.ts'
import { EXPECTED_TOOL_ORDER } from './session.ts'
import { materializeContent, normalizeMcpResult } from './content.ts'
import { augmentProcessParameters, beginReadObservation, completeReadObservation, createProcessPolicy } from './policy.ts'

/** Canonical output contract shared by all five tools (§9): content blocks plus optional lossless JSON. */
const outputSchema: JsonSchemaNode = {
  type: 'object',
  properties: {
    content: { type: 'array', items: {} },
    structuredContent: {},
  },
  required: ['content'],
  additionalProperties: false,
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function stringArg(args: unknown, key: string): string | undefined {
  const value = isRecord(args) ? args[key] : undefined
  return typeof value === 'string' ? value : undefined
}

function numberArg(args: unknown, key: string): number | undefined {
  const value = isRecord(args) ? args[key] : undefined
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined
}

function presentReadCall(args: unknown): ToolCallView | undefined {
  const path = stringArg(args, 'path')
  if (path === undefined) return undefined
  const line = numberArg(args, 'start_line')
  return {
    card: 'generic',
    title: `Read ${path}`,
    kind: 'read',
    locations: [{ path, ...(line !== undefined ? { line } : {}) }],
  }
}

function presentSearchCall(kind: 'Grep' | 'Glob', argKey: string): (args: unknown) => ToolCallView | undefined {
  return args => {
    const needle = stringArg(args, argKey)
    if (needle === undefined) return undefined
    return { card: 'generic', title: `${kind} ${needle}`, kind: 'search' }
  }
}

function presentRunProgramCall(args: unknown): ToolCallView | undefined {
  const program = stringArg(args, 'program')
  if (program === undefined) return undefined
  const argv = isRecord(args) && Array.isArray(args.args) ? args.args : []
  const title = [program, ...argv.filter(element => typeof element === 'string')].join(' ')
  const cwd = stringArg(args, 'cwd')
  return { card: 'terminal', title, ...(cwd !== undefined ? { cwd } : {}) }
}

function presentBashCall(args: unknown): ToolCallView | undefined {
  const command = stringArg(args, 'command')
  if (command === undefined) return undefined
  const cwd = stringArg(args, 'cwd')
  return { card: 'terminal', title: command, ...(cwd !== undefined ? { cwd } : {}) }
}

function presentTerminalResult(_args: unknown, result: { content: ContentBlock[]; isError: boolean }): ToolResultView | undefined {
  if (result.isError) return undefined
  const output = result.content
    .filter(block => block.type === 'text')
    .map(block => block.type === 'text' ? block.text : '')
    .join('\n')
  return { card: 'terminal', output }
}

export interface ToolDependencies {
  readonly ctx: Context
  readonly session: CodexshimSession
  readonly snapshot: CatalogSnapshot
  readonly config: ResolvedSessionConfig
}

const PROCESS_DESCRIPTION_SUFFIX = ' This tool runs outside the DSH sandbox: a call refused with CODEXSHIM_PROCESS_REQUIRES_FULL_ACCESS must be retried with the exact same arguments plus sandbox_permissions: "danger-full-access" and a justification for the user to approve.'

async function callAndMaterialize(deps: ToolDependencies, name: string, args: Record<string, unknown>, exec: ToolRunContext): Promise<{ content: ContentBlock[]; structuredContent?: unknown }> {
  const raw = await deps.session.call(name, args, exec.signal)
  const normalized = normalizeMcpResult(raw, name)
  const content = await materializeContent(deps.ctx, exec, normalized.content)
  return {
    content,
    ...(normalized.structuredContent !== undefined ? { structuredContent: normalized.structuredContent } : {}),
  }
}

/**
 * Build the five native tool definitions from the validated runtime catalog:
 * names/descriptions/parameters are cloned from the catalog, `timeoutMs`
 * declares the DSH 600-second shelf (enforced only when the composition loads
 * `dsh-tool-call-timeout-policy`), and only `read`/`grep`/`glob` opt into
 * concurrent dispatch. Reads bridge the DSH fs observation policy; process
 * tools enforce the fail-closed full-access escalation. The definition
 * objects are shared across agents; each agent registers them into its own
 * scope.
 */
export function buildToolDefinitions(deps: ToolDependencies): ReadonlyMap<CodexshimToolName, ToolDefinition> {
  const processPolicy = createProcessPolicy(deps.ctx)
  const definitions = new Map<CodexshimToolName, ToolDefinition>()
  for (const entry of deps.snapshot.tools) {
    const isProcessTool = entry.name === 'run_program' || entry.name === 'bash'
    const parameters = isProcessTool
      ? augmentProcessParameters(structuredClone(entry.parameters), processPolicy)
      : structuredClone(entry.parameters)
    const description = isProcessTool && processPolicy.advertisesEscalation
      ? entry.description + PROCESS_DESCRIPTION_SUFFIX
      : entry.description
    const execute = async (args: unknown, exec: ToolRunContext): Promise<unknown> => {
      const argsObj = isRecord(args) ? { ...args } : {}
      if (entry.name === 'read' && typeof argsObj.path === 'string') {
        const observation = await beginReadObservation(deps.ctx, exec, deps.config.root, argsObj.path)
        const result = await callAndMaterialize(deps, entry.name, argsObj, exec)
        await completeReadObservation(deps.ctx, exec, observation)
        return result
      }
      const prepared = isProcessTool
        ? await processPolicy.prepareArguments(entry.name, argsObj, exec)
        : argsObj
      return callAndMaterialize(deps, entry.name, prepared, exec)
    }
    const definition: ToolDefinition = {
      name: entry.name,
      description,
      parameters,
      output: {
        schema: outputSchema,
        render: (_args, value) => (value as unknown as { content: ContentBlock[] }).content,
      },
      timeoutMs: deps.config.toolCallTimeoutMs,
      execute,
      ...(entry.name === 'read' || entry.name === 'grep' || entry.name === 'glob'
        ? { isConcurrencySafe: () => true }
        : {}),
      ...(entry.name === 'read' ? { presentCall: presentReadCall } : {}),
      ...(entry.name === 'grep' ? { presentCall: presentSearchCall('Grep', 'pattern') } : {}),
      ...(entry.name === 'glob' ? { presentCall: presentSearchCall('Glob', 'pattern') } : {}),
      ...(entry.name === 'run_program' ? { presentCall: presentRunProgramCall } : {}),
      ...(entry.name === 'bash' ? { presentCall: presentBashCall } : {}),
      ...(isProcessTool
        ? { presentResult: (args: unknown, result) => presentTerminalResult(args, result) }
        : {}),
    }
    definitions.set(entry.name, definition)
  }
  return definitions
}

/** Prompt sections shadowing the preset's guidance for the replaced tools (§9). */
export function promptSections(): ReadonlyArray<{ readonly name: string; readonly order: number; readonly text: string }> {
  return [
    {
      name: 'tool:read',
      order: 100,
      text: 'Continue a truncated read by passing back the argument named in its trailing "Partial:" line (such as next_start_line) with start_line — never re-read from line 1. For PDFs, drive rendering with pdf_mode and pages, and echo the pdf_cursor from the previous "Partial:" or "Retry:" line to resume.',
    },
    {
      name: 'tool:glob',
      order: 103,
      text: 'Continue a truncated glob result by passing back the next_offset value from its "Partial:" line instead of restarting the scan.',
    },
    {
      name: 'tool:grep',
      order: 104,
      text: 'Continue a truncated grep result by passing back the next_offset value from its "Partial:" line instead of re-scanning from the beginning.',
    },
    {
      name: 'tool:run_program',
      order: 104.5,
      text: 'Prefer run_program with literal argv for a single program; use bash only when shell composition (pipelines, redirection, globbing, variable expansion, or several steps in one call) is required.',
    },
    {
      name: 'tool:bash',
      order: 105,
      text: 'A non-zero exit code is a normal result, not a tool error. For work that needs longer than the timeout, set detach with a log_path and poll that file with read; a detached process belongs to the codexshim server instance, not to DSH jobs (job_output/job_kill cannot see it).',
    },
  ]
}

/** The inherited names this adapter restricts before registering its own (§8.2): DSH has no run_program, so the intersection decides. */
export const RESTRICT_CANDIDATES = [...EXPECTED_TOOL_ORDER, 'pwsh'] as const

export { outputSchema }
