import { realpath, stat } from 'node:fs/promises'
import { isAbsolute } from 'node:path'
import { ensureCaptureRoot, resolveCaptureRoot } from './capture.ts'

export const MIN_TOOL_CALL_TIMEOUT_MS = 600_000

export interface PluginConfigInput {
  readonly root: string
  readonly env: Record<string, string>
  readonly toolCallTimeoutMs: number
  readonly captureRoot?: string
  readonly captureMaxBytes?: number
  readonly captureCleanup?: 'never' | 'session-end'
}

export interface ResolvedPluginConfig extends PluginConfigInput {
  readonly root: string
  readonly captureRoot: string
  readonly captureMaxBytes: number
  readonly captureCleanup: 'never' | 'session-end'
}

export async function resolvePluginConfig(input: PluginConfigInput): Promise<ResolvedPluginConfig> {
  const rawRoot = input.root === '' ? process.cwd() : input.root
  if (!isAbsolute(rawRoot)) throw new Error(`dsh-agentshim: root must be absolute, got ${JSON.stringify(input.root)}`)
  let root: string
  try {
    root = await realpath(rawRoot)
  } catch (error) {
    throw new Error(`dsh-agentshim: root ${JSON.stringify(rawRoot)} does not exist: ${String(error)}`)
  }
  if (!(await stat(root)).isDirectory()) throw new Error(`dsh-agentshim: root ${JSON.stringify(root)} is not a directory`)
  if (!Number.isFinite(input.toolCallTimeoutMs) || input.toolCallTimeoutMs < MIN_TOOL_CALL_TIMEOUT_MS) {
    throw new Error(`dsh-agentshim: toolCallTimeoutMs must be >= ${MIN_TOOL_CALL_TIMEOUT_MS}`)
  }
  const captureRoot = resolveCaptureRoot(input.captureRoot ?? '')
  await ensureCaptureRoot(captureRoot)
  return {
    ...input,
    root,
    captureRoot,
    captureMaxBytes: input.captureMaxBytes ?? 64 * 1024 * 1024,
    captureCleanup: input.captureCleanup ?? 'never',
  }
}
