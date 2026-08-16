import { createHash } from 'node:crypto'
import { constants as fsConstants } from 'node:fs'
import { access, realpath, stat } from 'node:fs/promises'
import { homedir } from 'node:os'
import { isAbsolute, join } from 'node:path'
import { Client } from '@modelcontextprotocol/sdk/client/index.js'
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js'
import type { Transport } from '@modelcontextprotocol/sdk/shared/transport.js'
import { ListToolsResultSchema } from '@modelcontextprotocol/sdk/types.js'
import type { Tool } from '@modelcontextprotocol/sdk/types.js'
import { z } from 'zod'
import { scrubbedParentEnv } from '@deepseek-ai/dsh-subprocess'
import { assertSupportedJsonSchema, TOOL_ABORTED } from '@deepseek-ai/dsh-tools'
import { HarnessError } from '@deepseek-ai/dsh-llm'

/** The only tool names this adapter may ever serve, in the server's fixed order. */
export const EXPECTED_TOOL_ORDER = ['read', 'grep', 'glob', 'run_program', 'bash'] as const

export type CodexshimToolName = (typeof EXPECTED_TOOL_ORDER)[number]

/** Lower bound for the per-call timeout: the DSH 600-second tool shelf. */
export const MIN_TOOL_CALL_TIMEOUT_MS = 600_000

/**
 * The raw tools/call result is validated only as a record here, exactly like
 * the DSH bridge; the adapter's own trust boundary validates content blocks
 * later. Owning the raw request also keeps the SDK's per-page output-validator
 * cache from pre-validating a contract it may not support.
 */
const RawCallToolResultSchema = z.record(z.string(), z.unknown())

/**
 * Standard JSON Schema vocabulary the codexshim catalog uses that falls
 * outside the DSH-supported subset: the numeric/string constraint keywords,
 * and schema-form `additionalProperties` (DSH only supports the boolean
 * form). These are omitted from the schema copy the startup gate asserts on,
 * but kept verbatim in the published parameters and covered by the catalog
 * fingerprint, so any other drift still fails loud.
 */
const GATE_OMITTED_KEYWORDS = new Set(['minimum', 'maximum', 'minLength', 'maxLength', 'pattern'])

/** One additional second past the SDK's two two-second termination grace periods. */
const GENERATION_CLOSE_TIMEOUT_MS = 5_000

export interface SessionConfigInput {
  /** Plugin working root; empty string resolves to `process.cwd()`. */
  readonly root: string
  /** codexshim executable; empty string resolves to the platform install path. */
  readonly command: string
  /** Arguments placed before the fixed server argv (development wrappers only). */
  readonly commandArgs: readonly string[]
  readonly readScope: 'normal' | 'unrestricted'
  /** Extra environment merged on top of the scrubbed parent environment. */
  readonly env: Record<string, string>
  readonly toolCallTimeoutMs: number
}

export interface ResolvedSessionConfig extends SessionConfigInput {
  /** Canonical absolute root: also the child cwd and the agent-cwd match value. */
  readonly root: string
  readonly command: string
}

export interface CatalogEntry {
  readonly name: CodexshimToolName
  readonly title: string | undefined
  readonly description: string
  /** Runtime input schema after the deterministic type-array rewrite. */
  readonly parameters: Record<string, unknown>
}

export interface CatalogSnapshot {
  readonly tools: readonly CatalogEntry[]
  readonly fingerprint: string
}

export interface SessionLogger {
  info(message: string): void
  warn(message: string): void
  error(message: string): void
}

export interface SessionOptions {
  /** Test seam; production always builds the official SDK client. */
  readonly createClient?: () => Client
  readonly logger?: SessionLogger
}

export interface CodexshimSession {
  /** Settles with the first validated catalog; rejects on startup failure. */
  readonly ready: Promise<CatalogSnapshot>
  call(name: string, args: Record<string, unknown>, signal: AbortSignal): Promise<Record<string, unknown>>
  dispose(): Promise<void>
}

interface Generation {
  readonly client: Client
  readonly closed: Promise<void>
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

/**
 * Transform every schema-position node (root, `properties` values, `items`,
 * object-form `additionalProperties`, `oneOf` elements) while leaving
 * annotation, enum, and const values untouched — those are data, not schemas.
 */
function transformSchemaValue(
  value: unknown,
  transform: (node: Record<string, unknown>) => Record<string, unknown>,
): unknown {
  if (!isRecord(value)) return value
  const replaced = transform(value)
  const out: Record<string, unknown> = {}
  for (const [key, child] of Object.entries(replaced)) {
    if (key === 'properties' && isRecord(child)) {
      const properties: Record<string, unknown> = {}
      for (const [name, property] of Object.entries(child)) properties[name] = transformSchemaValue(property, transform)
      out[key] = properties
    } else if (key === 'items' || (key === 'additionalProperties' && isRecord(child))) {
      out[key] = transformSchemaValue(child, transform)
    } else if (key === 'oneOf' && Array.isArray(child)) {
      out[key] = child.map(element => transformSchemaValue(element, transform))
    } else {
      out[key] = child
    }
  }
  return out
}

/**
 * Deterministically rewrite `"type": [a, b]` into `oneOf: [{ type: a }, { type: b }]`
 * (the DSH tool-bash idiom). The whole node is replaced — oneOf cannot keep
 * constraint siblings — preserving only the `description` and `default`
 * annotations. The rewrite is part of the published parameters and of the
 * catalog fingerprint.
 */
export function rewriteTypeArrays(schema: unknown): unknown {
  return transformSchemaValue(schema, node => {
    if (!Array.isArray(node.type)) return node
    const variants: unknown[] = node.type
    if (variants.length < 2 || variants.some(variant => typeof variant !== 'string')) {
      throw new Error(`dsh-codexshim: catalog validation failed: type array must list at least two string types, got ${JSON.stringify(variants)}`)
    }
    const replacement: Record<string, unknown> = {
      oneOf: (variants as string[]).map(variant => ({ type: variant })),
    }
    if (typeof node.description === 'string') replacement.description = node.description
    if (Object.hasOwn(node, 'default')) replacement.default = node.default
    return replacement
  })
}

function projectSchemaForGate(schema: unknown): unknown {
  return transformSchemaValue(schema, node => {
    const out: Record<string, unknown> = {}
    for (const [key, value] of Object.entries(node)) {
      if (GATE_OMITTED_KEYWORDS.has(key)) continue
      if (key === 'additionalProperties' && typeof value !== 'boolean') continue
      out[key] = value
    }
    return out
  })
}

/** Stable JSON with recursively sorted object keys; arrays keep their order. */
export function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`
  if (isRecord(value)) {
    const keys = Object.keys(value).sort()
    return `{${keys.map(key => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(',')}}`
  }
  return JSON.stringify(value) ?? 'null'
}

export interface FingerprintRecord {
  readonly name: string
  readonly title?: string
  readonly description: string
  readonly inputSchema: unknown
  readonly annotations?: unknown
  readonly execution?: unknown
  readonly meta?: unknown
}

/**
 * Deterministic identity of the drained catalog over name, description,
 * rewritten schema, and execution metadata. Two generations of the same
 * server must produce the same digest or reconnects fail with
 * `CODEXSHIM_CATALOG_CHANGED`.
 */
export function catalogFingerprint(tools: readonly FingerprintRecord[]): string {
  return createHash('sha256').update(canonicalJson(tools)).digest('hex')
}

function toolExecution(tool: Tool): Record<string, unknown> | undefined {
  const execution = (tool as { execution?: unknown }).execution
  return isRecord(execution) ? execution : undefined
}

/**
 * Validate a drained tools/list against the fixed five-name contract: exact
 * set and order, no duplicates, no required task execution, and every input
 * schema inside the DSH-supported subset after the deterministic rewrites.
 * Returns the published entries plus their fingerprint.
 */
export function validateCatalog(tools: readonly Tool[]): CatalogSnapshot {
  const names = tools.map(tool => tool.name)
  const duplicates = names.filter((name, index) => names.indexOf(name) !== index)
  if (duplicates.length > 0) {
    throw new Error(`dsh-codexshim: catalog validation failed: server listed tools more than once: ${[...new Set(duplicates)].join(', ')}`)
  }
  if (names.join('\0') !== EXPECTED_TOOL_ORDER.join('\0')) {
    const expected = EXPECTED_TOOL_ORDER.join(', ')
    throw new Error(
      `dsh-codexshim: catalog validation failed: server must list exactly [${expected}] in this order, got [${names.join(', ') || 'no tools'}]`,
    )
  }
  const entries: CatalogEntry[] = []
  const records: FingerprintRecord[] = []
  for (const tool of tools) {
    const execution = toolExecution(tool)
    if (execution?.['taskSupport'] === 'required') {
      throw new Error(`dsh-codexshim: catalog validation failed: tool "${tool.name}" requires task-based execution, which this adapter does not support`)
    }
    if (typeof tool.description !== 'string') {
      throw new Error(`dsh-codexshim: catalog validation failed: tool "${tool.name}" has no string description`)
    }
    if (!isRecord(tool.inputSchema)) {
      throw new Error(`dsh-codexshim: catalog validation failed: tool "${tool.name}" inputSchema is not an object`)
    }
    const parameters = rewriteTypeArrays(tool.inputSchema) as Record<string, unknown>
    try {
      assertSupportedJsonSchema(projectSchemaForGate(parameters))
    } catch (error) {
      throw new Error(`dsh-codexshim: catalog validation failed: tool "${tool.name}" inputSchema is outside the supported subset: ${String(error)}`)
    }
    const name = tool.name as CodexshimToolName
    const title = typeof tool.title === 'string' ? tool.title : undefined
    entries.push({ name, title, description: tool.description, parameters })
    records.push({
      name,
      ...(title !== undefined ? { title } : {}),
      description: tool.description,
      inputSchema: parameters,
      ...(isRecord(tool.annotations) ? { annotations: tool.annotations } : {}),
      ...(execution !== undefined ? { execution } : {}),
      ...(isRecord((tool as { _meta?: unknown })._meta) ? { meta: (tool as { _meta: Record<string, unknown> })._meta } : {}),
    })
  }
  return { tools: entries, fingerprint: catalogFingerprint(records) }
}

/** The platform's standard codexshim install path; fails loud when the base env is missing. */
export function defaultInstallCommand(): string {
  if (process.platform === 'win32') {
    const localAppData = process.env.LOCALAPPDATA
    if (localAppData === undefined || localAppData === '') {
      throw new Error("dsh-codexshim: cannot resolve the default codexshim path: LOCALAPPDATA is not set; set the plugin's `command` config to the codexshim executable")
    }
    return join(localAppData, 'codexshim', 'bin', 'codexshim.exe')
  }
  const dataHome = process.env.XDG_DATA_HOME && process.env.XDG_DATA_HOME !== ''
    ? process.env.XDG_DATA_HOME
    : join(homedir(), '.local', 'share')
  return join(dataHome, 'codexshim', 'bin', 'codexshim')
}

/**
 * Resolve and validate the session config: canonical absolute root that must
 * exist, an executable that must be an absolute existing file (executable on
 * POSIX), and a bounded call timeout. Failures name the checked path and the
 * `command` override so misconfiguration is actionable.
 */
export async function resolveSessionConfig(input: SessionConfigInput): Promise<ResolvedSessionConfig> {
  const rawRoot = input.root === '' ? process.cwd() : input.root
  if (!isAbsolute(rawRoot)) {
    throw new Error(`dsh-codexshim: root must be an absolute path, got ${JSON.stringify(input.root)}`)
  }
  let root: string
  try {
    root = await realpath(rawRoot)
  } catch (error) {
    throw new Error(`dsh-codexshim: root ${JSON.stringify(rawRoot)} does not exist: ${String(error)}`)
  }
  const rootStat = await stat(root)
  if (!rootStat.isDirectory()) {
    throw new Error(`dsh-codexshim: root ${JSON.stringify(root)} is not a directory`)
  }
  const defaultCommand = defaultInstallCommand()
  const command = input.command === '' ? defaultCommand : input.command
  if (!isAbsolute(command)) {
    throw new Error(`dsh-codexshim: command must be an absolute path, got ${JSON.stringify(input.command)} (platform default: ${defaultCommand})`)
  }
  try {
    const commandStat = await stat(command)
    if (!commandStat.isFile()) {
      throw new Error(`dsh-codexshim: command ${JSON.stringify(command)} is not a file; set the plugin's \`command\` config to the codexshim executable`)
    }
  } catch (error) {
    throw new Error(
      `dsh-codexshim: codexshim executable not found at ${JSON.stringify(command)}`
      + `${input.command === '' ? ' (resolved platform default)' : ''}: ${String(error)}`
      + " — install codexshim or set the plugin's `command` config",
      { cause: error instanceof Error ? error : undefined },
    )
  }
  if (process.platform !== 'win32') {
    try {
      await access(command, fsConstants.X_OK)
    } catch (error) {
      throw new Error(`dsh-codexshim: command ${JSON.stringify(command)} is not executable: ${String(error)}`)
    }
  }
  if (input.readScope !== 'normal' && input.readScope !== 'unrestricted') {
    throw new Error(`dsh-codexshim: readScope must be "normal" or "unrestricted", got ${JSON.stringify(input.readScope)}`)
  }
  if (!Number.isFinite(input.toolCallTimeoutMs) || input.toolCallTimeoutMs < MIN_TOOL_CALL_TIMEOUT_MS) {
    throw new Error(`dsh-codexshim: toolCallTimeoutMs must be a finite number >= ${MIN_TOOL_CALL_TIMEOUT_MS}, got ${String(input.toolCallTimeoutMs)}`)
  }
  return { ...input, root, command }
}

/** The fixed server argv: `<command> <commandArgs...> serve --client-profile codex --read-scope <mode>`. */
export function serverArgs(config: ResolvedSessionConfig): string[] {
  return [...config.commandArgs, 'serve', '--client-profile', 'codex', '--read-scope', config.readScope]
}

/** Credential-scrubbed parent environment with the config's explicit env layered on top. */
export function childEnv(extra: Record<string, string>): Record<string, string> {
  return { ...scrubbedParentEnv(), ...extra }
}

function awaitClosed(closed: Promise<void>): Promise<boolean> {
  return new Promise(resolve => {
    const timer = setTimeout(() => resolve(false), GENERATION_CLOSE_TIMEOUT_MS)
    timer.unref()
    void closed.then(() => {
      clearTimeout(timer)
      resolve(true)
    })
  })
}

function abortError(): HarnessError {
  const error = new HarnessError('tool call aborted', TOOL_ABORTED)
  error.name = 'AbortError'
  return error
}

/**
 * One private codexshim MCP session: spawns `codexshim serve` over stdio,
 * validates the five-tool catalog before going live, serves raw tools/call
 * requests, and reconnects at most once per new call after an unexpected
 * close — never on a timer, never replaying in-flight calls.
 */
export function createSession(config: ResolvedSessionConfig, options: SessionOptions = {}): CodexshimSession {
  const createClient = options.createClient ?? (() => new Client({ name: 'dsh-codexshim', version: '0.0.1' }, { capabilities: {} }))
  const logger: SessionLogger = options.logger ?? console
  const args = serverArgs(config)
  const env = childEnv(config.env)

  let disposed = false
  let current: Generation | undefined
  let opening: Generation | undefined
  let connecting: Promise<{ generation: Generation; snapshot: CatalogSnapshot }> | undefined
  let expectedFingerprint: string | undefined
  let previousClosed: Promise<unknown> | undefined
  const activeControllers = new Set<AbortController>()
  const activeCallPromises = new Set<Promise<unknown>>()

  function createTransport(): Transport {
    return new StdioClientTransport({ command: config.command, args, cwd: config.root, env })
  }

  async function drainCatalog(client: Client): Promise<CatalogSnapshot> {
    const tools: Tool[] = []
    let cursor: string | undefined
    do {
      const page = await client.request(
        { method: 'tools/list', ...(cursor === undefined ? {} : { params: { cursor } }) },
        ListToolsResultSchema,
        { timeout: config.toolCallTimeoutMs },
      )
      tools.push(...page.tools)
      cursor = page.nextCursor
    } while (cursor !== undefined && cursor !== '')
    return validateCatalog(tools)
  }

  async function connectGeneration(): Promise<{ generation: Generation; snapshot: CatalogSnapshot }> {
    if (previousClosed !== undefined) await previousClosed
    if (disposed) throw new Error('dsh-codexshim: session is disposed')
    const client = createClient()
    const closed = Promise.withResolvers<void>()
    const generation: Generation = { client, closed: closed.promise }
    opening = generation
    let attemptSettled = false
    client.onclose = () => {
      closed.resolve()
      previousClosed = closed.promise
      if (attemptSettled && current === generation) {
        current = undefined
        logger.warn('dsh-codexshim: codexshim server closed unexpectedly; the next tool call attempts one reconnect')
      }
    }
    let snapshot: CatalogSnapshot
    try {
      await client.connect(createTransport(), { timeout: config.toolCallTimeoutMs })
      snapshot = await drainCatalog(client)
      if (expectedFingerprint !== undefined && snapshot.fingerprint !== expectedFingerprint) {
        throw new HarnessError(
          `dsh-codexshim: server catalog changed after reconnect (expected fingerprint ${expectedFingerprint}, got ${snapshot.fingerprint}); reload the plugin to re-register the tools`,
          'CODEXSHIM_CATALOG_CHANGED',
        )
      }
    } catch (error) {
      attemptSettled = true
      if (opening === generation) opening = undefined
      current = undefined
      try { await client.close() } catch { /* transport already gone */ }
      previousClosed = closed.promise
      if (!await awaitClosed(closed.promise)) {
        logger.error(`dsh-codexshim: failed generation did not close within ${GENERATION_CLOSE_TIMEOUT_MS}ms; not starting an overlapping server process`)
      }
      throw error
    }
    attemptSettled = true
    expectedFingerprint ??= snapshot.fingerprint
    if (opening === generation) opening = undefined
    current = generation
    return { generation, snapshot }
  }

  async function ensureGeneration(): Promise<Generation> {
    if (disposed) throw new Error('dsh-codexshim: session is disposed')
    if (current !== undefined) return current
    connecting ??= startConnecting()
    return (await connecting).generation
  }

  async function performCall(name: string, callArgs: Record<string, unknown>, signal: AbortSignal): Promise<Record<string, unknown>> {
    const generation = await ensureGeneration()
    const controller = new AbortController()
    activeControllers.add(controller)
    const forwardAbort = (): void => controller.abort(signal.reason)
    if (signal.aborted) forwardAbort()
    else signal.addEventListener('abort', forwardAbort, { once: true })
    try {
      return await generation.client.request(
        { method: 'tools/call', params: { name, arguments: callArgs } },
        RawCallToolResultSchema,
        { signal: controller.signal, timeout: config.toolCallTimeoutMs },
      ) as Record<string, unknown>
    } catch (error) {
      if (controller.signal.aborted) throw abortError()
      throw error
    } finally {
      signal.removeEventListener('abort', forwardAbort)
      activeControllers.delete(controller)
    }
  }

  function startConnecting(): Promise<{ generation: Generation; snapshot: CatalogSnapshot }> {
    const attempt = connectGeneration().finally(() => {
      if (connecting === attempt) connecting = undefined
    })
    connecting = attempt
    return attempt
  }

  const firstAttempt = startConnecting()

  return {
    ready: firstAttempt.then(attempt => attempt.snapshot),
    call(name, callArgs, signal) {
      const promise = performCall(name, callArgs, signal)
      activeCallPromises.add(promise)
      void promise.then(
        () => activeCallPromises.delete(promise),
        () => activeCallPromises.delete(promise),
      )
      return promise
    },
    async dispose() {
      if (disposed) return
      disposed = true
      for (const controller of activeControllers) controller.abort()
      const generations = new Set<Generation>()
      if (opening !== undefined) generations.add(opening)
      if (current !== undefined) generations.add(current)
      opening = undefined
      current = undefined
      for (const generation of generations) {
        try { await generation.client.close() } catch { /* transport already gone */ }
        if (!await awaitClosed(generation.closed)) {
          logger.error(`dsh-codexshim: server did not close within ${GENERATION_CLOSE_TIMEOUT_MS}ms during teardown; its detached process trees may not have been collected`)
        }
      }
      if (connecting !== undefined) await connecting.catch(() => {})
      await Promise.allSettled(activeCallPromises)
    },
  }
}
