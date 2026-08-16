import { spawnSync } from 'node:child_process'
import { mkdtemp, rm, stat } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

const packageRoot = fileURLToPath(new URL('..', import.meta.url))
const enabled = process.env.AGENTSHIM_PACKAGE_E2E === '1'

function runDsh(dshHome: string, args: string[]): string {
  const pnpmCli = process.env.npm_execpath
  if (pnpmCli === undefined) throw new Error('package smoke must run through a pnpm script')
  const result = spawnSync(process.execPath, [pnpmCli, 'dlx', '@deepseek-ai/dsh@0.1.0-rc.6', ...args], {
    cwd: packageRoot,
    env: { ...process.env, DSH_HOME: dshHome },
    encoding: 'utf8',
    timeout: 180_000,
  })
  if (result.error !== undefined || result.status !== 0) {
    throw new Error(`dsh smoke command failed: ${result.error?.message ?? ''}\n${result.stdout}\n${result.stderr}`)
  }
  return `${result.stdout}\n${result.stderr}`
}

describe.runIf(enabled)('packed adapter in a clean DSH profile', () => {
  it('adds, dumps, and removes the tarball with the pinned rc.6 CLI', async () => {
    const dshHome = await mkdtemp(join(tmpdir(), 'dsh-agentshim-profile-'))
    const tarball = join(packageRoot, 'dsh-agentshim-0.1.0.tgz')
    try {
      await stat(tarball)
      runDsh(dshHome, ['plugin', '--profile', 'smoke', 'add', tarball])
      const installed = runDsh(dshHome, ['--profile', 'smoke', '--dump-config'])
      expect(installed).toContain('dsh-agentshim')
      expect(installed).toMatch(/\bid:\s*agentshim\b/)

      runDsh(dshHome, ['plugin', '--profile', 'smoke', 'remove', 'dsh-agentshim'])
      const removed = runDsh(dshHome, ['--profile', 'smoke', '--dump-config'])
      expect(removed).not.toContain('dsh-agentshim')
      expect(removed).not.toMatch(/\bid:\s*agentshim\b/)
    } finally {
      await rm(dshHome, { recursive: true, force: true })
    }
  }, 590_000)
})
