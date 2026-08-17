import { HarnessError } from '@deepseek-ai/dsh-llm'
import type { ParameterSchemaSpec, ValueSchemaSpec } from '@deepseek-ai/dsh-tools'

export const PUBLIC_TOOL_NAMES = ['read', 'grep', 'glob', 'run_program', 'bash', 'bash_status'] as const

export type PublicToolName = (typeof PUBLIC_TOOL_NAMES)[number]

export const readParameters = {
  path: { type: 'string', required: true, description: 'Platform-native path to one file.' },
  artifact_offset: { type: 'integer', description: 'Byte offset for reading binary content.' },
  encoding: { type: 'string', description: 'Optional WHATWG encoding label.' },
  line_count: { type: 'integer', description: 'Maximum lines to return, from 1 through 2000.' },
  pages: { type: 'string', description: 'PDF page or inclusive page range, such as "3" or "1-5".' },
  pdf_mode: { type: 'string', enum: ['auto', 'text', 'image'], description: 'PDF output mode.' },
  pdf_cursor: { type: 'string', description: 'Opaque continuation cursor returned by a previous PDF read.' },
  start_line: { type: 'integer', description: 'One-based first line.' },
} as const satisfies ParameterSchemaSpec

export const grepParameters = {
  pattern: { type: 'string', required: true, description: 'Rust regex, or literal text when fixed_strings is true.' },
  case: { type: 'string', enum: ['smart', 'sensitive', 'insensitive'], description: 'Case-sensitivity policy.' },
  context_lines: { type: 'integer', description: 'Context lines before and after each match, from 0 through 20.' },
  encoding: { type: 'string', description: 'Encoding for a single-file search.' },
  fallback_encoding: { type: 'string', description: 'Fallback encoding for undecodable files in a directory search.' },
  fixed_strings: { type: 'boolean', description: 'Treat pattern as literal text.' },
  glob: { type: 'string', description: 'Case-sensitive path filter.' },
  include_ignored: { type: 'boolean', description: 'Include gitignored paths, except hard exclusions.' },
  limit: { type: 'integer', description: 'Maximum entries, from 1 through 1000.' },
  mode: { type: 'string', enum: ['content', 'files', 'count'], description: 'Result projection.' },
  offset: { type: 'integer', description: 'Zero-based continuation offset.' },
  path: { type: 'string', description: 'File or directory to search.' },
} as const satisfies ParameterSchemaSpec

export const globParameters = {
  pattern: { type: 'string', required: true, description: 'Case-sensitive glob pattern.' },
  include_ignored: { type: 'boolean', description: 'Include gitignored paths, except hard exclusions.' },
  limit: { type: 'integer', description: 'Maximum paths, from 1 through 1000.' },
  offset: { type: 'integer', description: 'Zero-based continuation offset.' },
  path: { type: 'string', description: 'Directory to traverse.' },
  type: { type: 'string', enum: ['file', 'directory', 'any'], description: 'Filesystem entry kind.' },
} as const satisfies ParameterSchemaSpec

export const runProgramParameters = {
  program: { type: 'string', required: true, description: 'Program name or executable path.' },
  args: { type: 'array', items: { type: 'string' }, description: 'Literal argv elements.' },
  cwd: { type: 'string', description: 'Working directory.' },
  env: {
    type: 'object',
    additionalProperties: true,
    description: 'String-valued environment overrides.',
  },
  unset_env: { type: 'array', items: { type: 'string' }, description: 'Inherited environment variables to remove.' },
  stdin: { oneOf: [{ type: 'string' }, { type: 'null' }], description: 'Optional UTF-8 standard input.' },
  timeout_ms: { type: 'integer', description: 'Positive execution timeout in milliseconds.' },
} as const satisfies ParameterSchemaSpec

export const bashParameters = {
  command: { type: 'string', required: true, description: 'POSIX bash command line.' },
  description: { type: 'string', required: true, description: 'Short description of the command purpose.' },
  timeoutMs: { type: 'number', description: 'Positive foreground timeout in milliseconds.' },
  workdir: { type: 'string', description: 'Working directory; relative paths resolve against the workspace.' },
  run_in_background: { type: 'boolean', description: 'Run as a background job.' },
  msys_argument_conversion: { type: 'string', enum: ['default', 'disabled'], description: 'Windows only: Git Bash argument conversion mode.' },
} as const satisfies ParameterSchemaSpec

export const bashStatusParameters = {
  job_id: { type: 'string', required: true, description: 'Background job ID returned by bash.' },
} as const satisfies ParameterSchemaSpec

export const escalationParameters = {
  sandbox_permissions: {
    type: 'string',
    enum: ['workspace-write', 'danger-full-access'],
    description: 'Strictly wider one-shot sandbox mode requiring approval.',
  },
  justification: { type: 'string', description: 'Reason shown in the approval request.' },
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
  },
} as const satisfies ValueSchemaSpec

export function assertExactKeys(args: Record<string, unknown>, allowed: readonly string[]): void {
  const unknown = Object.keys(args).filter(key => !allowed.includes(key))
  if (unknown.length > 0) throw new HarnessError(`invalid arguments: unknown properties: ${unknown.join(', ')}`, 'INVALID_ARGS')
}

export function assertNonEmpty(value: string, name: string): void {
  if (value.trim().length === 0) throw new HarnessError(`invalid arguments: ${name} must be non-empty`, 'INVALID_ARGS')
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

export function assertStringRecord(value: Record<string, unknown> | undefined, name: string): void {
  if (value === undefined) return
  for (const [key, entry] of Object.entries(value)) {
    if (typeof entry !== 'string') throw new HarnessError(`invalid arguments: ${name}.${key} must be a string`, 'INVALID_ARGS')
  }
}
