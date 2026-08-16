import { createHash, randomBytes } from 'node:crypto'
import { constants as fsConstants } from 'node:fs'
import { chmod, lstat, mkdir, open, realpath, rm } from 'node:fs/promises'
import type { FileHandle } from 'node:fs/promises'
import { homedir } from 'node:os'
import { dirname, isAbsolute, join, relative, resolve } from 'node:path'
import { StringDecoder } from 'node:string_decoder'

export const INLINE_OUTPUT_BYTES = 50_000
export const DEFAULT_CAPTURE_MAX_BYTES = 64 * 1024 * 1024
export const MIN_CAPTURE_MAX_BYTES = 1024 * 1024
export const MAX_CAPTURE_MAX_BYTES = 1024 * 1024 * 1024

export type CaptureStreamName = 'stdout' | 'stderr' | 'output'

export interface CaptureArtifact {
  readonly path: string
  readonly bytes: number
  readonly complete: boolean
  readonly mediaType: 'application/octet-stream'
}

export interface CaptureAppendParams {
  readonly bridgeVersion: number
  readonly captureId: string
  readonly stream: CaptureStreamName
  readonly offset: number
  readonly data: string
}

export interface CaptureCompleteParams {
  readonly bridgeVersion: number
  readonly captureId: string
  readonly complete: boolean
  readonly totals: Partial<Record<CaptureStreamName, number>>
  readonly error?: string
}

export interface CaptureStartInput {
  readonly sessionId: string
  readonly callId: string
  readonly streams: readonly CaptureStreamName[]
  readonly background?: boolean
  readonly onLiveText?: (text: string) => void
  readonly onPublished?: (artifacts: readonly CaptureArtifact[]) => void
}

export interface CaptureCompletion {
  readonly complete: boolean
  readonly error?: string
  readonly artifacts: Readonly<Partial<Record<CaptureStreamName, CaptureArtifact>>>
}

export interface CaptureHandle {
  readonly id: string
  readonly completion: Promise<CaptureCompletion>
  readonly wire: {
    readonly version: 1
    readonly id: string
    readonly maxBytes: number
    readonly previewBytes: number
    readonly streams: readonly CaptureStreamName[]
  }
  artifacts(): Readonly<Partial<Record<CaptureStreamName, CaptureArtifact>>>
  abort(reason: string): Promise<void>
}

function prefixAtUtf8Boundary(bytes: Buffer, limit: number): Buffer {
  let end = Math.min(limit, bytes.byteLength)
  while (end > 0) {
    try {
      new TextDecoder('utf-8', { fatal: true }).decode(bytes.subarray(0, end))
      break
    } catch {
      end -= 1
    }
  }
  return bytes.subarray(0, end)
}

function suffixAtUtf8Boundary(bytes: Buffer, limit: number): Buffer {
  let start = Math.max(0, bytes.byteLength - limit)
  while (start < bytes.byteLength && ((bytes[start] ?? 0) & 0xc0) === 0x80) start += 1
  return bytes.subarray(start)
}

export function boundedProcessText(text: string, notices: readonly string[]): string {
  if (notices.length === 0 && Buffer.byteLength(text) <= INLINE_OUTPUT_BYTES) return text
  const notice = notices.join('\n')
  const separator = text === '' ? '' : '\n\n'
  const reserved = Buffer.byteLength(separator + notice)
  const budget = Math.max(0, INLINE_OUTPUT_BYTES - reserved - 64)
  const source = Buffer.from(text)
  if (source.byteLength <= budget) return `${text}${separator}${notice}`
  const headBudget = Math.ceil(budget / 2)
  const tailBudget = Math.floor(budget / 2)
  const head = prefixAtUtf8Boundary(source, headBudget).toString('utf8')
  const tail = suffixAtUtf8Boundary(source, tailBudget).toString('utf8')
  const omitted = source.byteLength - Buffer.byteLength(head) - Buffer.byteLength(tail)
  return `${head}\n... ${omitted} preview bytes omitted ...\n${tail}${separator}${notice}`
}

interface StreamState {
  readonly name: CaptureStreamName
  readonly path: string
  readonly handle: FileHandle
  readonly decoder: StringDecoder
  readonly utf8Probe: TextDecoder
  bytes: number
  invalidText: boolean
}

interface ActiveCapture {
  readonly id: string
  readonly background: boolean
  readonly streams: Map<CaptureStreamName, StreamState>
  readonly onLiveText?: (text: string) => void
  readonly onPublished?: (artifacts: readonly CaptureArtifact[]) => void
  readonly completion: PromiseWithResolvers<CaptureCompletion>
  totalBytes: number
  published: boolean
  settled: boolean
  queue: Promise<void>
}

export class CaptureError extends Error {
  constructor(
    message: string,
    readonly code: 'AGENTSHIM_CAPTURE_LIMIT_EXCEEDED' | 'AGENTSHIM_CAPTURE_IO_FAILED' | 'AGENTSHIM_CAPTURE_PROTOCOL',
  ) {
    super(message)
    this.name = 'CaptureError'
  }
}

function sessionSegment(sessionId: string): string {
  return createHash('sha256').update(sessionId).digest('hex').slice(0, 16)
}

function safeCallLabel(callId: string): string {
  const label = callId.replaceAll(/[^A-Za-z0-9._-]/g, '_').slice(0, 48)
  return label === '' ? 'call' : label
}

export function defaultCaptureRoot(): string {
  if (process.platform === 'win32') {
    const localAppData = process.env.LOCALAPPDATA
    if (localAppData === undefined || localAppData === '') {
      return join(homedir(), 'AppData', 'Local', 'agentshim', 'dsh-captures')
    }
    return join(localAppData, 'agentshim', 'dsh-captures')
  }
  if (process.platform === 'darwin') {
    return join(homedir(), 'Library', 'Application Support', 'agentshim', 'dsh-captures')
  }
  const dataHome = process.env.XDG_DATA_HOME && process.env.XDG_DATA_HOME !== ''
    ? process.env.XDG_DATA_HOME
    : join(homedir(), '.local', 'share')
  return join(dataHome, 'agentshim', 'dsh-captures')
}

export function resolveCaptureRoot(value: string): string {
  const root = value === '' ? defaultCaptureRoot() : value
  if (!isAbsolute(root)) throw new Error(`dsh-agentshim: captureRoot must be absolute, got ${JSON.stringify(value)}`)
  return resolve(root)
}

function validateCaptureMaxBytes(value: number): void {
  if (!Number.isSafeInteger(value) || value < MIN_CAPTURE_MAX_BYTES || value > MAX_CAPTURE_MAX_BYTES) {
    throw new Error(
      `dsh-agentshim: captureMaxBytes must be an integer from ${MIN_CAPTURE_MAX_BYTES} through ${MAX_CAPTURE_MAX_BYTES}`,
    )
  }
}

function artifact(state: StreamState, complete: boolean): CaptureArtifact {
  return {
    path: state.path,
    bytes: state.bytes,
    complete,
    mediaType: 'application/octet-stream',
  }
}

function contained(root: string, path: string): boolean {
  const child = relative(root, path)
  return child !== '' && !child.startsWith('..') && !isAbsolute(child)
}

export class CaptureStore {
  readonly root: string
  readonly maxBytes: number
  private readonly active = new Map<string, ActiveCapture>()
  private ready: Promise<void> | undefined

  constructor(root: string, maxBytes: number) {
    this.root = resolveCaptureRoot(root)
    validateCaptureMaxBytes(maxBytes)
    this.maxBytes = maxBytes
  }

  private ensureRoot(): Promise<void> {
    this.ready ??= (async () => {
      await mkdir(this.root, { recursive: true, mode: 0o700 })
      const info = await lstat(this.root)
      if (!info.isDirectory() || info.isSymbolicLink()) {
        throw new CaptureError(`captureRoot is not a private regular directory: ${this.root}`, 'AGENTSHIM_CAPTURE_IO_FAILED')
      }
      if (process.platform !== 'win32') await chmod(this.root, 0o700)
    })()
    return this.ready
  }

  async begin(input: CaptureStartInput): Promise<CaptureHandle> {
    await this.ensureRoot()
    const directory = join(this.root, `session-${sessionSegment(input.sessionId)}`)
    await mkdir(directory, { recursive: true, mode: 0o700 })
    const directoryInfo = await lstat(directory)
    if (!directoryInfo.isDirectory() || directoryInfo.isSymbolicLink()) {
      throw new CaptureError(`capture session path is not a regular directory: ${directory}`, 'AGENTSHIM_CAPTURE_IO_FAILED')
    }
    if (process.platform !== 'win32') await chmod(directory, 0o700)
    const id = randomBytes(16).toString('hex')
    const states = new Map<CaptureStreamName, StreamState>()
    try {
      for (const stream of input.streams) {
        const path = join(directory, `${id}-${safeCallLabel(input.callId)}-${stream}.raw`)
        const handle = await open(path, 'wx', 0o600)
        states.set(stream, {
          name: stream,
          path,
          handle,
          decoder: new StringDecoder('utf8'),
          utf8Probe: new TextDecoder('utf-8', { fatal: true }),
          bytes: 0,
          invalidText: false,
        })
      }
    } catch (error) {
      await Promise.allSettled([...states.values()].map(async state => {
        await state.handle.close().catch(() => {})
        await rm(state.path, { force: true }).catch(() => {})
      }))
      throw new CaptureError(`cannot create capture artifact: ${String(error)}`, 'AGENTSHIM_CAPTURE_IO_FAILED')
    }
    const completion = Promise.withResolvers<CaptureCompletion>()
    const active: ActiveCapture = {
      id,
      background: input.background === true,
      streams: states,
      ...(input.onLiveText === undefined ? {} : { onLiveText: input.onLiveText }),
      ...(input.onPublished === undefined ? {} : { onPublished: input.onPublished }),
      completion,
      totalBytes: 0,
      published: false,
      settled: false,
      queue: Promise.resolve(),
    }
    this.active.set(id, active)
    return {
      id,
      completion: completion.promise,
      wire: {
        version: 1,
        id,
        maxBytes: this.maxBytes,
        previewBytes: INLINE_OUTPUT_BYTES,
        streams: [...input.streams],
      },
      artifacts: () => this.artifacts(active, active.settled),
      abort: reason => this.abortActive(active, reason),
    }
  }

  async append(params: CaptureAppendParams): Promise<{ nextOffset: number }> {
    if (params.bridgeVersion !== 2) {
      throw new CaptureError(`capture append requires bridge version 2, received ${params.bridgeVersion}`, 'AGENTSHIM_CAPTURE_PROTOCOL')
    }
    const active = this.active.get(params.captureId)
    if (active === undefined || active.settled) {
      throw new CaptureError(`unknown or completed capture ${params.captureId}`, 'AGENTSHIM_CAPTURE_PROTOCOL')
    }
    const state = active.streams.get(params.stream)
    if (state === undefined) {
      throw new CaptureError(`capture ${params.captureId} has no ${params.stream} stream`, 'AGENTSHIM_CAPTURE_PROTOCOL')
    }
    if (!/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(params.data)) {
      throw new CaptureError('capture append data is not canonical base64', 'AGENTSHIM_CAPTURE_PROTOCOL')
    }
    const bytes = Buffer.from(params.data, 'base64')
    let nextOffset = params.offset
    const operation = active.queue.then(async () => {
      if (params.offset !== state.bytes) {
        throw new CaptureError(
          `capture ${params.captureId}/${params.stream} expected offset ${state.bytes}, received ${params.offset}`,
          'AGENTSHIM_CAPTURE_PROTOCOL',
        )
      }
      const available = this.maxBytes - active.totalBytes
      const accepted = bytes.subarray(0, Math.max(0, available))
      if (accepted.byteLength > 0) {
        try {
          await state.handle.writeFile(accepted)
        } catch (error) {
          throw new CaptureError(`capture write failed: ${String(error)}`, 'AGENTSHIM_CAPTURE_IO_FAILED')
        }
        state.bytes += accepted.byteLength
        active.totalBytes += accepted.byteLength
        nextOffset = state.bytes
        if (!state.invalidText) {
          if (accepted.includes(0)) state.invalidText = true
          try {
            state.utf8Probe.decode(accepted, { stream: true })
          } catch {
            state.invalidText = true
          }
        }
        const live = state.decoder.write(accepted)
        if (live !== '') active.onLiveText?.(live)
        this.publishIfNeeded(active)
      }
      if (accepted.byteLength !== bytes.byteLength) {
        this.publish(active)
        throw new CaptureError(
          `capture exceeded ${this.maxBytes} bytes; retained ${active.totalBytes} bytes`,
          'AGENTSHIM_CAPTURE_LIMIT_EXCEEDED',
        )
      }
    })
    active.queue = operation.catch(() => {})
    await operation
    return { nextOffset }
  }

  async complete(params: CaptureCompleteParams): Promise<{ complete: true }> {
    if (params.bridgeVersion !== 2) {
      throw new CaptureError(`capture completion requires bridge version 2, received ${params.bridgeVersion}`, 'AGENTSHIM_CAPTURE_PROTOCOL')
    }
    const active = this.active.get(params.captureId)
    if (active === undefined || active.settled) {
      throw new CaptureError(`unknown or completed capture ${params.captureId}`, 'AGENTSHIM_CAPTURE_PROTOCOL')
    }
    await active.queue
    for (const [name, state] of active.streams) {
      const expected = params.totals[name]
      if (params.complete && (expected === undefined || expected !== state.bytes)) {
        throw new CaptureError(
          `capture ${params.captureId}/${name} completed at ${state.bytes}, server reported ${String(expected)}`,
          'AGENTSHIM_CAPTURE_PROTOCOL',
        )
      }
      if (!state.invalidText) {
        try {
          state.utf8Probe.decode()
        } catch {
          state.invalidText = true
        }
      }
      const tail = state.decoder.end()
      if (tail !== '') active.onLiveText?.(tail)
    }
    if ([...active.streams.values()].some(state => state.invalidText)) this.publish(active)
    if (!params.complete && active.background && active.totalBytes > 0) this.publish(active)
    await this.settle(active, params.complete, params.error)
    return { complete: true }
  }

  async grant(path: string): Promise<string | undefined> {
    if (!isAbsolute(path)) return undefined
    await this.ensureRoot()
    const resolved = resolve(path)
    if (!contained(this.root, resolved) || !/^[0-9a-f]{32}-.+-(stdout|stderr|output)\.raw$/.test(resolved.split(/[\\/]/).at(-1) ?? '')) {
      return undefined
    }
    let info
    try {
      info = await lstat(resolved, { bigint: false })
      const canonicalParent = await realpath(dirname(resolved))
      if (resolve(canonicalParent, resolved.split(/[\\/]/).at(-1) ?? '') !== resolved) return undefined
    } catch {
      return undefined
    }
    if (!info.isFile() || info.isSymbolicLink()) return undefined
    try {
      await open(resolved, fsConstants.O_RDONLY).then(handle => handle.close())
    } catch {
      return undefined
    }
    return resolved
  }

  async dispose(): Promise<void> {
    await Promise.allSettled([...this.active.values()].map(active => this.abortActive(active, 'capture store disposed')))
  }

  private artifacts(active: ActiveCapture, complete: boolean): Partial<Record<CaptureStreamName, CaptureArtifact>> {
    if (!active.published) return {}
    return Object.fromEntries(
      [...active.streams.values()].filter(state => state.bytes > 0).map(state => [state.name, artifact(state, complete)]),
    )
  }

  private publishIfNeeded(active: ActiveCapture): void {
    if (active.totalBytes > INLINE_OUTPUT_BYTES || [...active.streams.values()].some(state => state.invalidText)) {
      this.publish(active)
    }
  }

  private publish(active: ActiveCapture): void {
    if (active.published) return
    active.published = true
    active.onPublished?.(Object.values(this.artifacts(active, false)))
  }

  private async settle(active: ActiveCapture, complete: boolean, error?: string): Promise<void> {
    if (active.settled) return
    active.settled = true
    this.active.delete(active.id)
    await Promise.allSettled([...active.streams.values()].map(state => state.handle.close()))
    if (!active.published) {
      await Promise.allSettled([...active.streams.values()].map(state => rm(state.path, { force: true })))
    }
    const result: CaptureCompletion = {
      complete,
      ...(error === undefined ? {} : { error }),
      artifacts: this.artifacts(active, complete),
    }
    active.completion.resolve(result)
  }

  private async abortActive(active: ActiveCapture, reason: string): Promise<void> {
    if (active.settled) return
    await active.queue
    if (active.background && active.totalBytes > 0) this.publish(active)
    await this.settle(active, false, reason)
  }
}
