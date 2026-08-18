import { homedir } from 'node:os'
import { isAbsolute, join, resolve } from 'node:path'
import { execFile } from 'node:child_process'
import { chmod, lstat, mkdir, readdir, rm } from 'node:fs/promises'
import { promisify } from 'node:util'

export const DEFAULT_CAPTURE_MAX_BYTES = 64 * 1024 * 1024
export const MIN_CAPTURE_MAX_BYTES = 1024 * 1024
export const MAX_CAPTURE_MAX_BYTES = 1024 * 1024 * 1024

export function defaultCaptureRoot(): string {
  if (process.platform === 'win32') {
    const localAppData = process.env.LOCALAPPDATA
    return join(
      localAppData && localAppData !== '' ? localAppData : join(homedir(), 'AppData', 'Local'),
      'agentshim',
      'dsh-captures',
    )
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
  if (!isAbsolute(root)) {
    throw new Error(`dsh-agentshim: captureRoot must be absolute, got ${JSON.stringify(value)}`)
  }
  return resolve(root)
}

const execFileAsync = promisify(execFile)

// Bare names resolve through PATH, and a Git Bash / MSYS launch puts its own
// coreutils ahead of System32: `whoami` there is the GNU build, which rejects
// `/user` and exits non-zero, failing plugin activation and taking the whole
// cordis plugin tree down with it. These two must be the Windows executables.
const system32 = join(process.env.SystemRoot ?? 'C:\\Windows', 'System32')
const whoamiExe = join(system32, 'whoami.exe')
const icaclsExe = join(system32, 'icacls.exe')

async function currentUserSid(): Promise<string> {
  const result = await execFileAsync(whoamiExe, ['/user', '/fo', 'csv', '/nh'], { windowsHide: true })
  const match = result.stdout.match(/S-1-5-[0-9-]+/)
  if (!match) throw new Error('dsh-agentshim: could not resolve current user SID')
  return match[0]
}

async function secureWindowsDirectory(path: string): Promise<void> {
  const sid = await currentUserSid()
  await execFileAsync(icaclsExe, [path, '/reset'], { windowsHide: true })
  await execFileAsync(icaclsExe, [path, '/inheritance:r'], { windowsHide: true })
  await execFileAsync(icaclsExe, [path, '/grant:r', `*${sid}:(OI)(CI)F`], { windowsHide: true })
  let sidPresent = false
  try {
    await execFileAsync(icaclsExe, [path, '/findsid', `*${sid}`], { windowsHide: true })
    sidPresent = true
  } catch {
    sidPresent = false
  }
  if (!sidPresent) {
    throw new Error(`dsh-agentshim: owner-only DACL verification failed for ${path}`)
  }
  const listing = await execFileAsync(icaclsExe, [path], { windowsHide: true })
  const aceCount = (listing.stdout.match(/:\(/g) ?? []).length
  const hasInherited = listing.stdout.includes('(I)')
  const hasFullControl = listing.stdout.includes('(F)')
  if (aceCount !== 1 || hasInherited || !hasFullControl) {
    throw new Error(`dsh-agentshim: owner-only DACL verification failed for ${path}`)
  }
}

export async function ensureCaptureRoot(path: string): Promise<void> {
  await mkdir(path, { recursive: true, mode: 0o700 })
  const info = await lstat(path)
  if (!info.isDirectory() || info.isSymbolicLink()) {
    throw new Error(`dsh-agentshim: captureRoot is not a regular directory: ${path}`)
  }
  if (process.platform === 'win32') {
    await secureWindowsDirectory(path)
  } else {
    await chmod(path, 0o700)
  }
}

export interface CaptureStatus {
  readonly root: string
  readonly sessions: number
  readonly files: number
  readonly bytes: number
}

export async function captureStatus(root: string): Promise<CaptureStatus> {
  await ensureCaptureRoot(root)
  let sessions = 0
  let files = 0
  let bytes = 0
  for (const session of await readdir(root, { withFileTypes: true })) {
    if (!session.isDirectory() || session.isSymbolicLink()) continue
    sessions += 1
    const calls = join(root, session.name)
    for (const call of await readdir(calls, { withFileTypes: true })) {
      if (!call.isDirectory() || call.isSymbolicLink()) continue
      for (const artifact of await readdir(join(calls, call.name), { withFileTypes: true })) {
        if (!artifact.isFile() || artifact.isSymbolicLink()) continue
        files += 1
        bytes += (await lstat(join(calls, call.name, artifact.name))).size
      }
    }
  }
  return { root, sessions, files, bytes }
}

export async function purgeCaptures(
  root: string,
  options: { readonly all: boolean; readonly olderThanDays?: number },
): Promise<number> {
  await ensureCaptureRoot(root)
  const threshold = options.all ? Number.POSITIVE_INFINITY : Date.now() - (options.olderThanDays ?? 0) * 86_400_000
  let removed = 0
  for (const session of await readdir(root, { withFileTypes: true })) {
    if (!session.isDirectory() || session.isSymbolicLink()) continue
    const path = join(root, session.name)
    if (!options.all && (await lstat(path)).mtimeMs > threshold) continue
    await rm(path, { recursive: true, force: true })
    removed += 1
  }
  return removed
}
