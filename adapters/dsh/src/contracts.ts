import { HarnessError } from '@deepseek-ai/dsh-llm'
import type { ParameterSchemaSpec, ValueSchemaSpec } from '@deepseek-ai/dsh-tools'

export const PUBLIC_TOOL_NAMES = ['read', 'grep', 'glob', 'run_program', 'bash', 'bash_status'] as const

export type PublicToolName = (typeof PUBLIC_TOOL_NAMES)[number]

export const MAX_GLOB_PATTERNS = 32
export const MAX_GLOB_PATTERN_CHARS = 1024
export const MAX_GREP_PATTERN_CHARS = 8192

export const readParameters = {
  path: { type: 'string', required: true, description: 'Required file path, resolved from the current agent session cwd; normal scope may also allow configured extension roots.' },
  artifact_offset: { type: 'integer', description: 'Byte offset for a published binary capture artifact.' },
  encoding: { type: 'string', description: 'Optional WHATWG encoding label; omit for automatic detection.' },
  line_count: { type: 'integer', description: 'Maximum lines to return, from 1 through 2000; omit to use the output budget.' },
  pages: { type: 'string', description: 'PDF page or inclusive page range, such as "3" or "1-5"; use instead of line arguments.' },
  pdf_mode: { type: 'string', enum: ['auto', 'text', 'image'], default: 'auto', description: 'PDF output: auto/text returns Markdown pages; image returns rendered PNG blocks.' },
  pdf_cursor: { type: 'string', description: 'Opaque PDF cursor returned by a previous read; pass it unchanged to continue.' },
  office_cursor: { type: 'string', description: 'Opaque Office cursor returned by a previous read; pass it unchanged to continue.' },
  start_line: { type: 'integer', default: 1, description: 'One-based first line; pass next_start_line from Partial output to continue.' },
} as const satisfies ParameterSchemaSpec

export const grepParameters = {
  pattern: { type: 'string', required: true, description: 'Required Rust regex, or literal text when fixed_strings=true; 1–8192 Unicode characters.' },
  case: { type: 'string', enum: ['smart', 'sensitive', 'insensitive'], default: 'smart', description: 'Case policy: smart is sensitive when the pattern contains uppercase.' },
  context_lines: { type: 'integer', default: 0, description: 'Context lines before and after each match, from 0 through 20.' },
  encoding: { type: 'string', description: 'Encoding for a single-file search; mutually exclusive with fallback_encoding.' },
  fallback_encoding: { type: 'string', description: 'Fallback encoding for undecodable files in a directory search; mutually exclusive with encoding.' },
  fixed_strings: { type: 'boolean', default: false, description: 'Treat pattern as literal text instead of Rust regex.' },
  glob: {
    oneOf: [
      { type: 'string' },
      { type: 'array', items: { type: 'string' } },
    ],
    description: 'Optional case-sensitive path filter or array of up to 32 filters; each is 1–1024 Unicode characters and ! excludes a pattern.',
  },
  include_ignored: { type: 'boolean', description: 'Include gitignored paths; omit to use the configured DSH filesystem policy.' },
  limit: { type: 'integer', default: 200, description: 'Maximum entries, from 1 through 1000.' },
  mode: { type: 'string', enum: ['content', 'files', 'count'], default: 'content', description: 'Result projection: matching lines, paths, or path:count summaries.' },
  offset: { type: 'integer', default: 0, description: 'Zero-based continuation offset; pass next_offset from Partial output.' },
  path: { type: 'string', default: '.', description: 'File or directory, resolved from the current agent session cwd; normal scope may also allow configured extension roots.' },
  type: { type: 'string', description: 'Optional file type filter, for example rust, python, js, ts, go, java, or markdown.' },
} as const satisfies ParameterSchemaSpec

export const globParameters = {
  pattern: {
    oneOf: [
      { type: 'string' },
      { type: 'array', items: { type: 'string' } },
    ],
    required: true,
    description: 'Required case-sensitive glob pattern or array of up to 32 patterns; each is 1–1024 Unicode characters and ! excludes a pattern.',
  },
  include_ignored: { type: 'boolean', description: 'Include gitignored paths; omit to use the configured DSH filesystem policy.' },
  limit: { type: 'integer', default: 200, description: 'Maximum paths, from 1 through 1000.' },
  offset: { type: 'integer', default: 0, description: 'Zero-based continuation offset; pass next_offset from Partial output.' },
  path: { type: 'string', default: '.', description: 'Directory to traverse, resolved from the current agent session cwd; normal scope may also allow configured extension roots.' },
  type: { type: 'string', enum: ['file', 'directory', 'any'], default: 'file', description: 'Filesystem entry kind: file, directory, or any.' },
} as const satisfies ParameterSchemaSpec

export const runProgramParameters = {
  program: { type: 'string', required: true, description: 'Required executable name or path; runs with literal argv and no shell.' },
  args: { type: 'array', items: { type: 'string' }, default: [], description: 'Literal argv elements; do not add shell quoting.' },
  cwd: { type: 'string', default: '.', description: 'Working directory, resolved from the current agent session cwd; omit for the session cwd.' },
  env: {
    type: 'object',
    additionalProperties: true,
    default: {},
    description: 'String-valued environment overrides; omitted variables are inherited.' ,
  },
  unset_env: { type: 'array', items: { type: 'string' }, default: [], description: 'Inherited environment variables to remove.' },
  stdin: { oneOf: [{ type: 'string' }, { type: 'null' }], description: 'Optional UTF-8 stdin, up to 1 MiB; omission or null closes stdin.' },
  timeout_ms: { type: 'integer', description: 'Positive execution timeout in milliseconds; omit for the configured DSH default.' },
} as const satisfies ParameterSchemaSpec

export const bashParameters = {
  command: { type: 'string', required: true, description: 'Required non-interactive POSIX Bash command; use shell syntax here, not PowerShell.' },
  description: { type: 'string', required: true, description: 'Required short purpose shown to the user; describe the side effect in 5–10 words, not the command syntax.' },
  timeoutMs: { type: 'integer', description: 'Positive integer timeout in milliseconds; background values may only shorten the configured maximum.' },
  workdir: { type: 'string', default: '.', description: 'Working directory, resolved from the current agent session cwd; omit for the session cwd.' },
  run_in_background: { type: 'boolean', default: false, description: 'Run as a managed background job; poll bash_status with the returned job ID.' },
  msys_argument_conversion: { type: 'string', enum: ['default', 'disabled'], default: 'default', description: 'Windows only: Git Bash argument conversion mode.' },
} as const satisfies ParameterSchemaSpec

export const bashStatusParameters = {
  job_id: { type: 'string', required: true, description: "Required job ID returned by bash for this agent with run_in_background=true; repeat until terminal status." },
} as const satisfies ParameterSchemaSpec

export const escalationParameters = {
  sandbox_permissions: {
    type: 'string',
    enum: ['workspace-write', 'danger-full-access'],
    description: 'The narrowest wider sandbox mode for a one-shot retry of the exact command the sandbox just denied; '
      + 'the retry asks the user for approval.',
  },
  justification: {
    type: 'string',
    description: 'Required with sandbox_permissions: one sentence for the user explaining why this exact command '
      + 'needs the wider access. Use the language of the user’s current request.',
  },
} as const satisfies ParameterSchemaSpec

const attachmentSchema = {
  type: 'object',
  additionalProperties: false,
  properties: {
    attachmentId: { type: 'string', required: true },
    mediaType: { type: 'string', required: true },
    bytes: { type: 'integer', required: true },
    width: { type: 'integer', required: true },
    height: { type: 'integer', required: true },
    name: { type: 'string' },
    originalDimensions: {
      type: 'object',
      additionalProperties: false,
      properties: {
        width: { type: 'integer', required: true },
        height: { type: 'integer', required: true },
      },
    },
  },
} as const satisfies ValueSchemaSpec

export const readOutputSchema = {
  type: 'object',
  additionalProperties: false,
  properties: {
    kind: { type: 'string', const: 'read', required: true },
    text: { type: 'string', required: true },
    attachments: { type: 'array', items: attachmentSchema, required: true },
  },
} as const satisfies ValueSchemaSpec

export const textOutputSchema = (kind: 'grep' | 'glob') => ({
  type: 'object',
  additionalProperties: false,
  properties: {
    kind: { type: 'string', const: kind, required: true },
    text: { type: 'string', required: true },
  },
} as const satisfies ValueSchemaSpec)

const sandboxOutputSchema = {
  type: 'object',
  additionalProperties: false,
  properties: {
    mode: { type: 'string', required: true },
    enforcement: { type: 'string' },
    denied: { type: 'boolean', required: true },
    runnerFailed: { type: 'boolean', required: true },
  },
} as const satisfies ValueSchemaSpec

const failureOutputSchema = {
  type: 'object',
  additionalProperties: true,
  properties: {
    code: { type: 'string', required: true },
    message: { type: 'string', required: true },
    retryable: { type: 'boolean', required: true },
  },
} as const satisfies ValueSchemaSpec

const artifactOutputSchema = {
  type: 'object',
  additionalProperties: false,
  properties: {
    path: { type: 'string', required: true },
    bytes: { type: 'integer', required: true },
    complete: { type: 'boolean', required: true },
    stream: { type: 'string', required: true },
  },
} as const satisfies ValueSchemaSpec

const processStreamSchema = {
  type: 'object',
  additionalProperties: false,
  properties: {
    text: { type: 'string', required: true },
    totalBytes: { type: 'integer', required: true },
    shownBytes: { type: 'integer', required: true },
    omittedBytes: { type: 'integer', required: true },
    artifact: {
      type: 'object',
      additionalProperties: false,
      properties: {
        path: { type: 'string', required: true },
        bytes: { type: 'integer', required: true },
        complete: { type: 'boolean', required: true },
        mediaType: { type: 'string', const: 'application/octet-stream', required: true },
      },
    },
  },
} as const satisfies ValueSchemaSpec

export const processOutputSchema = {
  type: 'object',
  additionalProperties: false,
  properties: {
    kind: { type: 'string', const: 'foreground', required: true },
    text: { type: 'string', required: true },
    exitCode: { oneOf: [{ type: 'string' }, { type: 'null' }], required: true },
    stdout: { ...processStreamSchema, required: true },
    stderr: { ...processStreamSchema, required: true },
    limitExceeded: { type: 'boolean', required: true },
    outcomeUncertain: { type: 'boolean', required: true },
    sandbox: sandboxOutputSchema,
  },
} as const satisfies ValueSchemaSpec

export const bashOutputSchema = {
  oneOf: [
    processOutputSchema,
    {
      type: 'object',
      additionalProperties: false,
      properties: {
        kind: { type: 'string', const: 'background', required: true },
        jobId: { type: 'string', required: true },
      },
    },
  ],
} as const satisfies ValueSchemaSpec

export const bashStatusOutputSchema = {
  type: 'object',
  additionalProperties: false,
  properties: {
    kind: { type: 'string', const: 'status', required: true },
    jobId: { type: 'string', required: true },
    status: { type: 'string', required: true },
    label: { type: 'string', required: true },
    detail: { type: 'string' },
    exitCode: { oneOf: [{ type: 'string' }, { type: 'null' }] },
    failure: failureOutputSchema,
    artifacts: { type: 'array', items: artifactOutputSchema },
    limitExceeded: { type: 'boolean' },
    sandbox: {
      type: 'object',
      additionalProperties: false,
      properties: {
        mode: { type: 'string', required: true },
        enforcement: { type: 'string' },
        denied: { type: 'boolean', required: true },
        runnerFailed: { type: 'boolean', required: true },
      },
    },
    denied: { type: 'boolean' },
    runnerFailed: { type: 'boolean' },
  },
} as const satisfies ValueSchemaSpec

export function assertExactKeys(args: Record<string, unknown>, allowed: readonly string[]): void {
  const unknown = Object.keys(args).filter(key => !allowed.includes(key))
  if (unknown.length > 0) throw new HarnessError(`invalid arguments: unknown properties: ${unknown.join(', ')}`, 'INVALID_ARGS')
}

export function assertNonEmpty(value: unknown, name: string): asserts value is string {
  if (typeof value !== 'string') throw new HarnessError(`invalid arguments: ${name} must be a string`, 'INVALID_ARGS')
  if (value.trim().length === 0) throw new HarnessError(`invalid arguments: ${name} must be non-empty`, 'INVALID_ARGS')
}

export function unicodeLength(value: string): number {
  return Array.from(value).length
}

export function assertPattern(
  value: unknown,
  name: string,
  maximum: number,
  rejectBareNegation = false,
): asserts value is string {
  assertNonEmpty(value, name)
  if (value.includes('\0')) throw new HarnessError(`invalid arguments: ${name} must not contain NUL`, 'INVALID_ARGS')
  if (rejectBareNegation && value === '!') throw new HarnessError(`invalid arguments: ${name} must contain a pattern after !`, 'INVALID_ARGS')
  if (unicodeLength(value) > maximum) {
    throw new HarnessError(`invalid arguments: ${name} must contain at most ${maximum} Unicode characters`, 'INVALID_ARGS')
  }
}

export function assertOptionalString(value: unknown, name: string): asserts value is string | undefined {
  if (value !== undefined && typeof value !== 'string') {
    throw new HarnessError(`invalid arguments: ${name} must be a string`, 'INVALID_ARGS')
  }
}

export function assertStringArray(value: unknown, name: string): asserts value is readonly string[] | undefined {
  if (value === undefined) return
  if (!Array.isArray(value)) throw new HarnessError(`invalid arguments: ${name} must be an array`, 'INVALID_ARGS')
  for (const [index, entry] of value.entries()) {
    if (typeof entry !== 'string') {
      throw new HarnessError(`invalid arguments: ${name}[${index}] must be a string`, 'INVALID_ARGS')
    }
  }
}

export function assertBoolean(value: unknown, name: string): asserts value is boolean | undefined {
  if (value !== undefined && typeof value !== 'boolean') {
    throw new HarnessError(`invalid arguments: ${name} must be a boolean`, 'INVALID_ARGS')
  }
}

export function assertEnum<T extends string>(value: unknown, name: string, values: readonly T[]): asserts value is T | undefined {
  if (value !== undefined && (typeof value !== 'string' || !values.includes(value as T))) {
    throw new HarnessError(`invalid arguments: ${name} must be one of ${values.join(', ')}`, 'INVALID_ARGS')
  }
}

export function assertIntegerRange(value: number | undefined, name: string, minimum: number, maximum?: number): void {
  if (value === undefined) return
  if (!Number.isInteger(value) || value < minimum || (maximum !== undefined && value > maximum)) {
    throw new HarnessError(`invalid arguments: ${name} must be an integer from ${minimum}${maximum === undefined ? '' : ` through ${maximum}`}`, 'INVALID_ARGS')
  }
}

export function assertPositive(value: number | undefined, name: string): void {
  if (value !== undefined && (!Number.isFinite(value) || value <= 0)) {
    throw new HarnessError(`invalid arguments: ${name} must be a positive finite number`, 'INVALID_ARGS')
  }
}

export function assertStringRecord(value: unknown, name: string): asserts value is Record<string, string> | undefined {
  if (value === undefined) return
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new HarnessError(`invalid arguments: ${name} must be an object`, 'INVALID_ARGS')
  }
  for (const [key, entry] of Object.entries(value)) {
    if (typeof entry !== 'string') throw new HarnessError(`invalid arguments: ${name}.${key} must be a string`, 'INVALID_ARGS')
  }
}
