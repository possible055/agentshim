import { createRequire as nodeCreateRequire } from 'node:module'
import { randomUUID } from 'node:crypto'
import { scrubbedParentEnv } from '@deepseek-ai/dsh-subprocess'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import { TOOL_ABORTED } from '@deepseek-ai/dsh-tools'

/** Native API contract version; activation refuses mismatched addons. */
export const REQUIRED_NATIVE_API_VERSION = 5

export interface NativeFailure {
  readonly code: string
  readonly message: string
  readonly retryable: boolean
  readonly details?: unknown
}

export type NativeResult<T> =
  | { readonly value: T; readonly failure?: undefined }
  | { readonly value?: undefined; readonly failure: NativeFailure }

export function nativeFailureError(failure: NativeFailure): HarnessError {
  const error = new HarnessError(failure.message, failure.code === 'AGENTSHIM_CANCELLED' ? TOOL_ABORTED : failure.code)
  if (failure.code === 'AGENTSHIM_CANCELLED') error.name = 'AbortError'
  ;(error as HarnessError & { details?: unknown }).details = failure.details
  return error
}

function unwrapNativeResult<T>(result: NativeResult<T>): T {
  if (result.failure !== undefined) throw nativeFailureError(result.failure)
  if (result.value === undefined) {
    throw new HarnessError('native engine returned an empty success result', 'AGENTSHIM_NATIVE_CONTRACT_INVALID')
  }
  return result.value
}

export interface NativeImage {
  readonly data: string
  readonly mimeType: string
}

export interface NativeToolText {
  readonly text: string
  readonly images: readonly NativeImage[]
}

export interface NativeReadArgs {
  readonly path: string
  readonly encoding?: string
  readonly startLine?: number
  readonly lineCount?: number
  readonly pages?: string
  readonly pdfMode?: 'auto' | 'text' | 'image'
  readonly pdfCursor?: string
  readonly artifactOffset?: number
}

export interface NativeGrepArgs {
  readonly pattern: string
  readonly path?: string
  readonly glob?: string
  readonly mode?: 'content' | 'files' | 'count'
  readonly fixedStrings?: boolean
  readonly case?: 'smart' | 'sensitive' | 'insensitive'
  readonly contextLines?: number
  readonly offset?: number
  readonly limit?: number
  readonly includeIgnored?: boolean
  readonly encoding?: string
  readonly fallbackEncoding?: string
}

export interface NativeGlobArgs {
  readonly pattern: string
  readonly path?: string
  readonly includeIgnored?: boolean
  readonly entryType?: 'file' | 'directory' | 'any'
  readonly offset?: number
  readonly limit?: number
}

export interface NativeHostOptions {
  readonly pageBudgetBytes?: number
  readonly readScope?: 'normal' | 'unrestricted'
  readonly toolTimeoutShelfMs?: number
  readonly backgroundJobTimeoutMaxMs?: number
  readonly env?: ReadonlyArray<{ key: string; value: string }>
  readonly captureRoot?: string
  readonly captureMaxBytes?: number
  readonly captureCleanup?: 'never' | 'session-end'
}

export interface NativePreparedProcess {
  readonly handle: string
  readonly argv: readonly string[]
}

export interface NativeArtifactInfo {
  readonly path: string
  readonly bytes: number
  readonly complete: boolean
  readonly stream: string
}

export interface NativeProcessStream {
  readonly text: string
  readonly totalBytes: number
  readonly shownBytes: number
  readonly omittedBytes: number
  readonly artifact?: NativeArtifactInfo
}

export interface NativeProcessOutcome {
  readonly text: string
  readonly childNonzero: boolean
  /** Core exit label ("0", "42", "signal 9"); absent when the process never settled. */
  readonly exitCode?: string
  readonly stdout: NativeProcessStream
  readonly stderr: NativeProcessStream
  readonly artifacts: NativeArtifactInfo[]
  readonly failure?: NativeFailure
  readonly denied: boolean
  readonly runnerFailed: boolean
}

/** One runner-failure evidence rule, mirroring the DSH sandbox contract. */
export interface NativeRunnerFailureRule {
  readonly allowedExitCodes?: readonly number[]
  readonly fatalSignatures: readonly string[]
  readonly informationalLines?: readonly string[]
}

/**
 * Sandbox classification inputs for one confined spawn: the backend's denial
 * dialect and runner-failure rules, exactly as `SandboxProvider.confine`
 * produced them for the wrapped argv. The native engine is the single
 * classification authority; the adapter only consumes the outcome flags.
 */
export interface NativeSandboxAttribution {
  readonly denialSignatures?: readonly string[]
  readonly runnerFailureRules?: readonly NativeRunnerFailureRule[]
}

export interface NativeRunProgramArgs {
  readonly program: string
  readonly args: readonly string[]
  readonly cwd?: string
  readonly env?: Readonly<Record<string, string>>
  readonly unsetEnv?: readonly string[]
  readonly stdin?: string
  readonly timeoutMs?: number
}

export interface NativeBashArgs {
  readonly command: string
  readonly cwd?: string
  readonly timeoutMs?: number
  readonly msysArgumentConversion?: 'enabled' | 'disabled'
  readonly background?: boolean
}

export function nativeRunProgramArgs(args: Record<string, unknown>): NativeRunProgramArgs {
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

export function nativeBashArgs(wire: Record<string, unknown>, background = false): NativeBashArgs {
  return {
    command: wire.command as string,
    ...(wire.cwd === undefined ? {} : { cwd: wire.cwd as string }),
    ...(wire.timeout_ms === undefined ? {} : { timeoutMs: wire.timeout_ms as number }),
    ...(wire.msys_argument_conversion === undefined ? {} : { msysArgumentConversion: wire.msys_argument_conversion as 'enabled' | 'disabled' }),
    ...(background ? { background: true } : {}),
  }
}

export interface NativeArtifactPublished {
  readonly path: string
  readonly bytes: number
  readonly complete: boolean
  readonly stream: string
}

export interface NativeJobOutcome {
  readonly status: string
  readonly detail: string
  readonly exitCode?: string
  readonly limitExceeded: boolean
  readonly artifacts: readonly NativeArtifactPublished[]
  readonly failure?: NativeFailure
}

export interface NativeJobHandle {
  cancel(reason: string): void
  readOutput(): string
  done(): Promise<NativeJobOutcome>
  dispose(): Promise<void>
}

interface RawNativeJobHandle {
  cancel(reason: string): NativeResult<boolean>
  readOutput(): NativeResult<string>
  done(): Promise<NativeResult<NativeJobOutcome>>
  dispose(): Promise<NativeResult<true>>
}

interface RawNativeEngine {
  beginCall(callId: string): NativeResult<boolean>
  cancelCall(callId: string): NativeResult<boolean>
  releaseCall(callId: string): NativeResult<boolean>
  verifyBash(): NativeResult<boolean>
  readText(callId: string, args: NativeReadArgs): Promise<NativeResult<NativeToolText>>
  grepText(callId: string, args: NativeGrepArgs): Promise<NativeResult<NativeToolText>>
  globText(callId: string, args: NativeGlobArgs): Promise<NativeResult<NativeToolText>>
  prepareRunProgram(callId: string, args: NativeRunProgramArgs): NativeResult<NativePreparedProcess>
  prepareBash(callId: string, args: NativeBashArgs): NativeResult<NativePreparedProcess>
  spawnPrepared(callId: string, handle: string, wrappedArgv?: readonly string[], attribution?: NativeSandboxAttribution): Promise<NativeResult<NativeProcessOutcome>>
  startBackgroundPrepared(callId: string, handle: string, wrappedArgv?: readonly string[]): NativeResult<RawNativeJobHandle>
  close(): Promise<NativeResult<boolean>>
}

interface RawNativeHostRuntime {
  openEngine(root: string): RawNativeEngine
}

export interface NativeEngine {
  verifyBash(): void
  readText(args: NativeReadArgs, signal?: AbortSignal): Promise<NativeToolText>
  grepText(args: NativeGrepArgs, signal?: AbortSignal): Promise<NativeToolText>
  globText(args: NativeGlobArgs, signal?: AbortSignal): Promise<NativeToolText>
  prepareRunProgram(args: NativeRunProgramArgs, signal?: AbortSignal): NativePreparedProcess
  prepareBash(args: NativeBashArgs, signal?: AbortSignal): NativePreparedProcess
  spawnPrepared(handle: string, wrappedArgv?: readonly string[], attribution?: NativeSandboxAttribution): Promise<NativeProcessOutcome>
  startBackgroundPrepared(handle: string, wrappedArgv?: readonly string[]): NativeJobHandle
  discardPrepared(handle: string): void
  close(): Promise<void>
}

export interface NativeHostRuntime {
  openEngine(root: string): NativeEngine
}

interface NativeModule {
  readonly apiVersion: () => number
  readonly NativeHostRuntime: new (options?: NativeHostOptions) => RawNativeHostRuntime
}

interface LoadedNativeModule {
  readonly apiVersion: () => number
  readonly NativeHostRuntime: new (options?: NativeHostOptions) => NativeHostRuntime
}

class NativeJobHandleAdapter implements NativeJobHandle {
  constructor(private readonly raw: RawNativeJobHandle) {}

  cancel(reason: string): void {
    unwrapNativeResult(this.raw.cancel(reason))
  }

  readOutput(): string {
    return unwrapNativeResult(this.raw.readOutput())
  }

  async done(): Promise<NativeJobOutcome> {
    return unwrapNativeResult(await this.raw.done())
  }

  async dispose(): Promise<void> {
    unwrapNativeResult(await this.raw.dispose())
  }
}

class NativeEngineAdapter implements NativeEngine {
  private readonly pending = new Map<string, { callId: string; signal: AbortSignal | undefined; onAbort: () => void }>()

  constructor(private readonly raw: RawNativeEngine) {}

  verifyBash(): void {
    unwrapNativeResult(this.raw.verifyBash())
  }

  private begin(signal?: AbortSignal): { callId: string; signal: AbortSignal | undefined; onAbort: () => void } {
    const callId = randomUUID()
    unwrapNativeResult(this.raw.beginCall(callId))
    const onAbort = () => {
      unwrapNativeResult(this.raw.cancelCall(callId))
    }
    if (signal?.aborted === true) onAbort()
    else signal?.addEventListener('abort', onAbort, { once: true })
    return { callId, signal, onAbort }
  }

  private release(lease: { callId: string; signal: AbortSignal | undefined; onAbort: () => void }): void {
    lease.signal?.removeEventListener('abort', lease.onAbort)
    unwrapNativeResult(this.raw.releaseCall(lease.callId))
  }

  private discardLease(lease: { callId: string; signal: AbortSignal | undefined; onAbort: () => void }): void {
    try {
      unwrapNativeResult(this.raw.cancelCall(lease.callId))
    } finally {
      this.release(lease)
    }
  }

  async readText(args: NativeReadArgs, signal?: AbortSignal): Promise<NativeToolText> {
    const lease = this.begin(signal)
    try {
      return unwrapNativeResult(await this.raw.readText(lease.callId, args))
    } finally {
      this.release(lease)
    }
  }

  async grepText(args: NativeGrepArgs, signal?: AbortSignal): Promise<NativeToolText> {
    const lease = this.begin(signal)
    try {
      return unwrapNativeResult(await this.raw.grepText(lease.callId, args))
    } finally {
      this.release(lease)
    }
  }

  async globText(args: NativeGlobArgs, signal?: AbortSignal): Promise<NativeToolText> {
    const lease = this.begin(signal)
    try {
      return unwrapNativeResult(await this.raw.globText(lease.callId, args))
    } finally {
      this.release(lease)
    }
  }

  prepareRunProgram(args: NativeRunProgramArgs, signal?: AbortSignal): NativePreparedProcess {
    const lease = this.begin(signal)
    try {
      const prepared = unwrapNativeResult(this.raw.prepareRunProgram(lease.callId, args))
      this.pending.set(prepared.handle, lease)
      return prepared
    } catch (error) {
      this.discardLease(lease)
      throw error
    }
  }

  prepareBash(args: NativeBashArgs, signal?: AbortSignal): NativePreparedProcess {
    const lease = this.begin(signal)
    try {
      const prepared = unwrapNativeResult(this.raw.prepareBash(lease.callId, args))
      this.pending.set(prepared.handle, lease)
      return prepared
    } catch (error) {
      this.discardLease(lease)
      throw error
    }
  }

  async spawnPrepared(
    handle: string,
    wrappedArgv?: readonly string[],
    attribution?: NativeSandboxAttribution,
  ): Promise<NativeProcessOutcome> {
    const lease = this.pending.get(handle)
    if (lease === undefined) {
      throw new HarnessError('prepared handle is unknown or already spawned', 'AGENTSHIM_PREPARED_HANDLE_INVALID')
    }
    this.pending.delete(handle)
    try {
      return unwrapNativeResult(await this.raw.spawnPrepared(lease.callId, handle, wrappedArgv, attribution))
    } finally {
      this.release(lease)
    }
  }

  startBackgroundPrepared(handle: string, wrappedArgv?: readonly string[]): NativeJobHandle {
    const lease = this.pending.get(handle)
    if (lease === undefined) {
      throw new HarnessError('prepared handle is unknown or already started', 'AGENTSHIM_PREPARED_HANDLE_INVALID')
    }
    this.pending.delete(handle)
    try {
      const raw = unwrapNativeResult(this.raw.startBackgroundPrepared(lease.callId, handle, wrappedArgv))
      this.release(lease)
      return new NativeJobHandleAdapter(raw)
    } catch (error) {
      this.release(lease)
      throw error
    }
  }

  discardPrepared(handle: string): void {
    const lease = this.pending.get(handle)
    if (lease === undefined) return
    this.pending.delete(handle)
    this.discardLease(lease)
  }

  async close(): Promise<void> {
    try {
      unwrapNativeResult(await this.raw.close())
    } finally {
      for (const lease of this.pending.values()) lease.signal?.removeEventListener('abort', lease.onAbort)
      this.pending.clear()
    }
  }
}

class NativeHostRuntimeAdapter implements NativeHostRuntime {
  constructor(private readonly raw: RawNativeHostRuntime) {}

  openEngine(root: string): NativeEngine {
    return new NativeEngineAdapter(this.raw.openEngine(root))
  }
}

/** Platform packages are the only supported native distribution channels. */
const PLATFORM_PACKAGES: Readonly<Record<string, string>> = {
  'win32-x64-msvc': 'dsh-agentshim-win32-x64-msvc',
  'darwin-arm64': 'dsh-agentshim-darwin-arm64',
  'linux-x64-gnu': 'dsh-agentshim-linux-x64-gnu',
  'linux-arm64-gnu': 'dsh-agentshim-linux-arm64-gnu',
}

const requireAddonModule = nodeCreateRequire(import.meta.url)

export function nativePlatformTriple(): string {
  if (process.platform === 'win32' && process.arch === 'x64') return 'win32-x64-msvc'
  if (process.platform === 'darwin' && process.arch === 'arm64') return 'darwin-arm64'
  if (process.platform === 'linux') {
    let glibc = false
    try {
      const report = process.report?.getReport() as { header?: { glibcVersionRuntime?: string } } | undefined
      glibc = report?.header?.glibcVersionRuntime !== undefined
    } catch {
      glibc = false
    }
    if (process.arch === 'x64') return glibc ? 'linux-x64-gnu' : 'linux-x64-musl'
    if (process.arch === 'arm64') return glibc ? 'linux-arm64-gnu' : 'linux-arm64-musl'
  }
  return `${process.platform}-${process.arch}`
}

function requireAddon(triple: string): NativeModule {
  const devOverride = process.env.AGENTSHIM_DSH_NATIVE_DLL
  if (devOverride !== undefined && devOverride !== '') {
    return requireAddonModule(devOverride) as NativeModule
  }
  const packageName = PLATFORM_PACKAGES[triple]
  if (packageName === undefined) throw new Error(`no native package is defined for ${triple}`)
  return requireAddonModule(packageName) as NativeModule
}

export type NativeLoadFailure =
  | { readonly reason: 'unsupported-platform'; readonly detail: string }
  | { readonly reason: 'api-version-mismatch'; readonly detail: string }
  | { readonly reason: 'addon-unavailable'; readonly detail: string }

export type NativeLoadResult =
  | { readonly engine: LoadedNativeModule; readonly failure?: undefined }
  | { readonly engine?: undefined; readonly failure: NativeLoadFailure }

export function nativeLoadFailureError(failure: NativeLoadFailure): HarnessError {
  const code = failure.reason === 'unsupported-platform'
    ? 'AGENTSHIM_NATIVE_PLATFORM_UNSUPPORTED'
    : failure.reason === 'api-version-mismatch'
      ? 'AGENTSHIM_NATIVE_API_MISMATCH'
      : 'AGENTSHIM_NATIVE_ADDON_UNAVAILABLE'
  const error = new HarnessError(failure.detail, code)
  ;(error as HarnessError & { details?: unknown }).details = { reason: failure.reason }
  return error
}

/** Load and validate the exact native engine API for this platform. */
export function loadNativeAddon(): NativeLoadResult {
  const triple = nativePlatformTriple()
  if (PLATFORM_PACKAGES[triple] === undefined && process.env.AGENTSHIM_DSH_NATIVE_DLL === undefined) {
    return {
      failure: {
        reason: 'unsupported-platform',
        detail: `platform ${triple} has no supported native engine package; supported: ${Object.keys(PLATFORM_PACKAGES).join(', ')}`,
      },
    }
  }
  let addon: NativeModule
  try {
    addon = requireAddon(triple)
  } catch (error) {
    return {
      failure: {
        reason: 'addon-unavailable',
        detail: `native addon for ${triple} could not be loaded: ${error instanceof Error ? error.message : String(error)}`,
      },
    }
  }
  const version = addon.apiVersion?.()
  if (version !== REQUIRED_NATIVE_API_VERSION) {
    return {
      failure: {
        reason: 'api-version-mismatch',
        detail: `native addon apiVersion ${String(version)} does not match required ${REQUIRED_NATIVE_API_VERSION}`,
      },
    }
  }
  if (typeof addon.NativeHostRuntime !== 'function') {
    return { failure: { reason: 'api-version-mismatch', detail: 'native addon does not export the NativeHostRuntime capability' } }
  }
  const NativeHostRuntime = class extends NativeHostRuntimeAdapter {
    constructor(options?: NativeHostOptions) {
      super(new addon.NativeHostRuntime(options))
    }
  }
  return {
    engine: {
      apiVersion: addon.apiVersion,
      NativeHostRuntime,
    },
  }
}

/** Build the Engine's explicit child environment: scrubbed parent plus config overrides. */
export function nativeEngineEnv(configEnv: Record<string, string>): Array<{ key: string; value: string }> {
  const merged: Record<string, string> = { ...scrubbedParentEnv(), ...configEnv }
  return Object.entries(merged).map(([key, value]) => ({ key, value }))
}

export const BACKGROUND_JOB_TIMEOUT_MAX_ENV = 'AGENTSHIM_BACKGROUND_JOB_TIMEOUT_MAX'
export const DEFAULT_BACKGROUND_JOB_TIMEOUT_MAX_SECONDS = 1800
export const MIN_BACKGROUND_JOB_TIMEOUT_MAX_SECONDS = 600
export const MAX_BACKGROUND_JOB_TIMEOUT_MAX_SECONDS = 14400

export function backgroundJobTimeoutMaxMs(env: ReadonlyArray<{ key: string; value: string }>): number {
  const raw = env.find(entry => entry.key === BACKGROUND_JOB_TIMEOUT_MAX_ENV)?.value
  if (raw === undefined) return DEFAULT_BACKGROUND_JOB_TIMEOUT_MAX_SECONDS * 1000
  if (!/^[0-9]+$/.test(raw)) {
    throw new Error(`dsh-agentshim: ${BACKGROUND_JOB_TIMEOUT_MAX_ENV} must be an integer from ${MIN_BACKGROUND_JOB_TIMEOUT_MAX_SECONDS} to ${MAX_BACKGROUND_JOB_TIMEOUT_MAX_SECONDS} seconds`)
  }
  const seconds = Number(raw)
  if (!Number.isSafeInteger(seconds) || seconds < MIN_BACKGROUND_JOB_TIMEOUT_MAX_SECONDS || seconds > MAX_BACKGROUND_JOB_TIMEOUT_MAX_SECONDS) {
    throw new Error(`dsh-agentshim: ${BACKGROUND_JOB_TIMEOUT_MAX_ENV} must be an integer from ${MIN_BACKGROUND_JOB_TIMEOUT_MAX_SECONDS} to ${MAX_BACKGROUND_JOB_TIMEOUT_MAX_SECONDS} seconds`)
  }
  return seconds * 1000
}
