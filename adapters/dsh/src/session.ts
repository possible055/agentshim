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
import { TOOL_ABORTED } from '@deepseek-ai/dsh-tools'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import { BRIDGE_VERSION, REQUIRED_BRIDGE_OPERATIONS } from './contracts.ts'
import type { BridgeOperation } from './contracts.ts'
import type { CaptureAppendParams, CaptureCompleteParams, CaptureStore } from './capture.ts'

export const MIN_TOOL_CALL_TIMEOUT_MS = 600_000
const GENERATION_CLOSE_TIMEOUT_MS = 5_000
const RawCallToolResultSchema = z.record(z.string(), z.unknown())
const CaptureStreamSchema = z.enum(['stdout', 'stderr', 'output'])
const CaptureAppendRequestSchema = z.object({
  method: z.literal('agentshim/dsh.capture.append'),
  params: z.object({
    bridgeVersion: z.literal(BRIDGE_VERSION),
    captureId: z.string().min(1),
    stream: CaptureStreamSchema,
    offset: z.number().int().nonnegative(),
    data: z.string(),
  }),
})
const CaptureCompleteRequestSchema = z.object({
  method: z.literal('agentshim/dsh.capture.complete'),
  params: z.object({
    bridgeVersion: z.literal(BRIDGE_VERSION),
    captureId: z.string().min(1),
    complete: z.boolean(),
    totals: z.object({
      stdout: z.number().int().nonnegative().optional(),
      stderr: z.number().int().nonnegative().optional(),
      output: z.number().int().nonnegative().optional(),
    }),
    error: z.string().optional(),
  }),
})

export interface SessionConfigInput {
  readonly root: string
  readonly command: string
  readonly commandArgs: readonly string[]
  readonly readScope: 'normal' | 'unrestricted'
  readonly env: Record<string, string>
  readonly toolCallTimeoutMs: number
  readonly captureRoot?: string
  readonly captureMaxBytes?: number
}

export interface ResolvedSessionConfig extends SessionConfigInput {
  readonly root: string
  readonly command: string
}

export interface CatalogSnapshot {
  readonly operations: readonly BridgeOperation[]
  readonly bridgeVersion: number
}

export interface SessionLogger {
  info(message: string): void
  warn(message: string): void
  error(message: string): void
}

export interface SessionLaunch {
  readonly command: string
  readonly args: readonly string[]
  readonly cwd: string
  readonly env: Record<string, string>
  readonly stderr?: 'pipe' | 'inherit'
}

export interface SessionOptions {
  readonly createClient?: () => Client
  readonly logger?: SessionLogger
  readonly launch?: SessionLaunch
  readonly reconnect?: boolean
  readonly captureStore?: CaptureStore
}

export interface AgentshimSession {
  readonly ready: Promise<CatalogSnapshot>
  call(name: BridgeOperation, args: Record<string, unknown>, signal: AbortSignal): Promise<Record<string, unknown>>
  stderrTail(): string
  dispose(): Promise<void>
}

interface Generation {
  readonly client: Client
  readonly closed: Promise<void>
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function bridgeVersion(tool: Tool): number | undefined {
  const meta = isRecord(tool._meta) ? tool._meta['agentshim.dshBridge'] : undefined
  return isRecord(meta) && Number.isInteger(meta.version) ? meta.version as number : undefined
}

export function validateCatalog(tools: readonly Tool[]): CatalogSnapshot {
  const byName = new Map<string, Tool>()
  for (const tool of tools) {
    if (byName.has(tool.name)) {
      throw new Error(`dsh-agentshim: bridge validation failed: duplicate operation "${tool.name}"`)
    }
    byName.set(tool.name, tool)
  }
  const missing = REQUIRED_BRIDGE_OPERATIONS.filter(name => !byName.has(name))
  if (missing.length > 0) {
    throw new Error(`dsh-agentshim: bridge validation failed: missing required operations: ${missing.join(', ')}`)
  }
  const versions = REQUIRED_BRIDGE_OPERATIONS.map(name => bridgeVersion(byName.get(name) as Tool))
  if (versions.some(version => version !== BRIDGE_VERSION)) {
    throw new Error(`dsh-agentshim: bridge validation failed: expected agentshim.dshBridge version ${BRIDGE_VERSION} on every required operation`)
  }
  return { operations: [...REQUIRED_BRIDGE_OPERATIONS], bridgeVersion: BRIDGE_VERSION }
}

export function defaultInstallCommand(): string {
  if (process.platform === 'win32') {
    const localAppData = process.env.LOCALAPPDATA
    if (localAppData === undefined || localAppData === '') {
      throw new Error("dsh-agentshim: cannot resolve the default agentshim path: LOCALAPPDATA is not set; set the plugin's `command` config")
    }
    return join(localAppData, 'agentshim', 'bin', 'agentshim.exe')
  }
  const dataHome = process.env.XDG_DATA_HOME && process.env.XDG_DATA_HOME !== ''
    ? process.env.XDG_DATA_HOME
    : join(homedir(), '.local', 'share')
  return join(dataHome, 'agentshim', 'bin', 'agentshim')
}

export async function resolveSessionConfig(input: SessionConfigInput): Promise<ResolvedSessionConfig> {
  const rawRoot = input.root === '' ? process.cwd() : input.root
  if (!isAbsolute(rawRoot)) throw new Error(`dsh-agentshim: root must be absolute, got ${JSON.stringify(input.root)}`)
  let root: string
  try {
    root = await realpath(rawRoot)
  } catch (error) {
    throw new Error(`dsh-agentshim: root ${JSON.stringify(rawRoot)} does not exist: ${String(error)}`)
  }
  if (!(await stat(root)).isDirectory()) throw new Error(`dsh-agentshim: root ${JSON.stringify(root)} is not a directory`)
  const defaultCommand = defaultInstallCommand()
  const command = input.command === '' ? defaultCommand : input.command
  if (!isAbsolute(command)) throw new Error(`dsh-agentshim: command must be absolute, got ${JSON.stringify(input.command)}`)
  try {
    if (!(await stat(command)).isFile()) throw new Error('not a file')
  } catch (error) {
    throw new Error(`dsh-agentshim: agentshim executable not found at ${JSON.stringify(command)}: ${String(error)}`)
  }
  if (process.platform !== 'win32') await access(command, fsConstants.X_OK)
  if (input.readScope !== 'normal' && input.readScope !== 'unrestricted') {
    throw new Error(`dsh-agentshim: readScope must be "normal" or "unrestricted", got ${JSON.stringify(input.readScope)}`)
  }
  if (!Number.isFinite(input.toolCallTimeoutMs) || input.toolCallTimeoutMs < MIN_TOOL_CALL_TIMEOUT_MS) {
    throw new Error(`dsh-agentshim: toolCallTimeoutMs must be >= ${MIN_TOOL_CALL_TIMEOUT_MS}`)
  }
  return { ...input, root, command }
}

export function serverArgs(config: ResolvedSessionConfig): string[] {
  return [...config.commandArgs, 'serve', '--client-profile', 'dsh', '--read-scope', config.readScope]
}

export function childEnv(extra: Record<string, string>): Record<string, string> {
  return { ...scrubbedParentEnv(), ...extra }
}

export function defaultLaunch(config: ResolvedSessionConfig): SessionLaunch {
  return {
    command: config.command,
    args: serverArgs(config),
    cwd: config.root,
    env: childEnv(config.env),
    stderr: 'pipe',
  }
}

export function serverCommand(config: ResolvedSessionConfig): string[] {
  return [config.command, ...serverArgs(config)]
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

export function createSession(config: ResolvedSessionConfig, options: SessionOptions = {}): AgentshimSession {
  const createClient = options.createClient ?? (() => new Client({ name: 'dsh-agentshim', version: '0.1.0' }, { capabilities: {} }))
  const logger = options.logger ?? console
  const launch = options.launch ?? defaultLaunch(config)
  const reconnect = options.reconnect ?? true
  const captureStore = options.captureStore

  let disposed = false
  let current: Generation | undefined
  let opening: Generation | undefined
  let connecting: Promise<{ generation: Generation; snapshot: CatalogSnapshot }> | undefined
  let previousClosed: Promise<unknown> | undefined
  const activeControllers = new Set<AbortController>()
  const activeCallPromises = new Set<Promise<unknown>>()
  let stderrBytes = Buffer.alloc(0)

  function createTransport(): Transport {
    const transport = new StdioClientTransport({
      command: launch.command,
      args: [...launch.args],
      cwd: launch.cwd,
      env: launch.env,
      stderr: launch.stderr ?? 'pipe',
    })
    transport.stderr?.on('data', (chunk: Buffer | string) => {
      const incoming = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk)
      stderrBytes = Buffer.concat([stderrBytes, incoming]).subarray(-32 * 1024)
    })
    return transport
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
    if (disposed) throw new Error('dsh-agentshim: session is disposed')
    const client = createClient()
    if (captureStore !== undefined) {
      client.setRequestHandler(CaptureAppendRequestSchema, request => {
        return captureStore.append(request.params as CaptureAppendParams)
      })
      client.setRequestHandler(CaptureCompleteRequestSchema, request => {
        return captureStore.complete(request.params as CaptureCompleteParams)
      })
    }
    const closed = Promise.withResolvers<void>()
    const generation = { client, closed: closed.promise }
    opening = generation
    let settled = false
    client.onclose = () => {
      closed.resolve()
      previousClosed = closed.promise
      if (settled && current === generation) {
        current = undefined
        logger.warn('dsh-agentshim: agentshim server closed unexpectedly')
      }
    }
    try {
      await client.connect(createTransport(), { timeout: config.toolCallTimeoutMs })
      const snapshot = await drainCatalog(client)
      settled = true
      opening = undefined
      current = generation
      return { generation, snapshot }
    } catch (error) {
      settled = true
      opening = undefined
      current = undefined
      try { await client.close() } catch { /* transport already closed */ }
      previousClosed = closed.promise
      if (!await awaitClosed(closed.promise)) logger.error('dsh-agentshim: failed generation did not close promptly')
      throw error
    }
  }

  function startConnecting(): Promise<{ generation: Generation; snapshot: CatalogSnapshot }> {
    const attempt = connectGeneration().finally(() => {
      if (connecting === attempt) connecting = undefined
    })
    connecting = attempt
    return attempt
  }

  async function ensureGeneration(): Promise<Generation> {
    if (disposed) throw new Error('dsh-agentshim: session is disposed')
    if (current !== undefined) return current
    if (!reconnect && previousClosed !== undefined) throw new Error('dsh-agentshim: dedicated session closed')
    connecting ??= startConnecting()
    return (await connecting).generation
  }

  async function performCall(name: BridgeOperation, args: Record<string, unknown>, signal: AbortSignal): Promise<Record<string, unknown>> {
    const generation = await ensureGeneration()
    const controller = new AbortController()
    activeControllers.add(controller)
    const forwardAbort = (): void => controller.abort(signal.reason)
    if (signal.aborted) forwardAbort()
    else signal.addEventListener('abort', forwardAbort, { once: true })
    try {
      return await generation.client.request(
        { method: 'tools/call', params: { name, arguments: args } },
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

  const firstAttempt = startConnecting()
  return {
    ready: firstAttempt.then(attempt => attempt.snapshot),
    call(name, args, signal) {
      const promise = performCall(name, args, signal)
      activeCallPromises.add(promise)
      void promise.finally(() => activeCallPromises.delete(promise)).catch(() => {})
      return promise
    },
    stderrTail: () => stderrBytes.toString('utf8'),
    async dispose() {
      if (disposed) return
      disposed = true
      for (const controller of activeControllers) controller.abort()
      const generations = new Set([opening, current].filter((value): value is Generation => value !== undefined))
      opening = undefined
      current = undefined
      for (const generation of generations) {
        try { await generation.client.close() } catch { /* transport already closed */ }
        if (!await awaitClosed(generation.closed)) logger.error('dsh-agentshim: server did not close promptly during teardown')
      }
      if (connecting !== undefined) await connecting.catch(() => {})
      await Promise.allSettled(activeCallPromises)
    },
  }
}
