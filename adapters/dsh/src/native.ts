import { createRequire as nodeCreateRequire } from 'node:module'
import { scrubbedParentEnv } from '@deepseek-ai/dsh-subprocess'

/** Native API contract version; activation refuses mismatched addons. */
export const REQUIRED_NATIVE_API_VERSION = 1

export interface NativeToolText {
  readonly text: string
  readonly complete: boolean
}

export interface NativeReadArgs {
  readonly path: string
  readonly encoding?: string
  readonly startLine?: number
  readonly lineCount?: number
  readonly pages?: string
  readonly pdfMode?: 'auto' | 'text' | 'image'
  readonly pdfCursor?: string
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

export interface NativeEngineOptions {
  readonly pageBudgetBytes?: number
  readonly timeoutCeilingMs?: number
  readonly env?: ReadonlyArray<{ key: string; value: string }>
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

export interface NativeProcessOutcome {
  readonly text: string
  readonly childNonzero: boolean
  readonly artifacts: NativeArtifactInfo[]
  readonly limitExceeded: boolean
  readonly errorCode?: string
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
}

export interface NativeEngine {
  readText(args: NativeReadArgs): Promise<NativeToolText>
  grepText(args: NativeGrepArgs): Promise<NativeToolText>
  globText(args: NativeGlobArgs): Promise<NativeToolText>
  prepareRunProgram(args: NativeRunProgramArgs): NativePreparedProcess
  prepareBash(args: NativeBashArgs): NativePreparedProcess
  spawnPrepared(handle: string, wrappedArgv?: readonly string[]): Promise<NativeProcessOutcome>
  close(): Promise<void>
}

interface NativeModule {
  readonly apiVersion: () => number
  readonly Engine: new (root: string, options?: NativeEngineOptions) => NativeEngine
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

function requireAddon(triple: string): NativeModule | undefined {
  const devOverride = process.env.AGENTSHIM_DSH_NATIVE_DLL
  if (devOverride !== undefined && devOverride !== '') {
    return requireAddonModule(devOverride) as NativeModule
  }
  const packageName = PLATFORM_PACKAGES[triple]
  if (packageName === undefined) return undefined
  try {
    return requireAddonModule(packageName) as NativeModule
  } catch {
    return undefined
  }
}

export type NativeLoadFailure =
  | { readonly reason: 'unsupported-platform'; readonly detail: string }
  | { readonly reason: 'api-version-mismatch'; readonly detail: string }
  | { readonly reason: 'addon-unavailable'; readonly detail: string }

export type NativeLoadResult =
  | { readonly engine: NativeModule; readonly failure?: undefined }
  | { readonly engine?: undefined; readonly failure: NativeLoadFailure }

/**
 * Load the native engine addon for this platform. Returns a failure instead of
 * throwing so the caller can decide between the hybrid MCP bridge and failing
 * activation loudly.
 */
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
  const addon = requireAddon(triple)
  if (addon === undefined) {
    return { failure: { reason: 'addon-unavailable', detail: `native addon for ${triple} is not installed` } }
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
  if (typeof addon.Engine !== 'function') {
    return { failure: { reason: 'api-version-mismatch', detail: 'native addon does not export the Engine capability' } }
  }
  return { engine: addon }
}

/** Build the Engine's explicit child environment: scrubbed parent plus config overrides. */
export function nativeEngineEnv(configEnv: Record<string, string>): Array<{ key: string; value: string }> {
  const merged: Record<string, string> = { ...scrubbedParentEnv(), ...configEnv }
  return Object.entries(merged).map(([key, value]) => ({ key, value }))
}
