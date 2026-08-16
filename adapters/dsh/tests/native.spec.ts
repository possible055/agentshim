import { copyFile, mkdtemp, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { afterEach, describe, expect, it } from 'vitest'
import { loadNativeAddon, nativeEngineEnv, nativePlatformTriple, REQUIRED_NATIVE_API_VERSION } from '../src/native.ts'

const builtDll = fileURLToPath(new URL('../../../target/debug/agentshim_napi.dll', import.meta.url))

async function stageAddon(): Promise<string | undefined> {
  try {
    const directory = await mkdtemp(join(tmpdir(), 'agentshim-native-'))
    const staged = join(directory, 'agentshim_napi.node')
    await copyFile(builtDll, staged)
    return staged
  } catch {
    return undefined
  }
}

const stagedAddon = await stageAddon()

const originalEnv = process.env.AGENTSHIM_DSH_NATIVE_DLL
afterEach(() => {
  if (originalEnv === undefined) {
    delete process.env.AGENTSHIM_DSH_NATIVE_DLL
  } else {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = originalEnv
  }
})

describe('native addon loading', () => {
  it('reports the platform triple for the supported matrix vocabulary', () => {
    const triple = nativePlatformTriple()
    expect(typeof triple).toBe('string')
    expect(triple.length).toBeGreaterThan(0)
  })

  it('fails loudly when an explicit dev override points nowhere', () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = join(tmpdir(), 'definitely-missing-agentshim.node')
    expect(() => loadNativeAddon()).toThrow(/Cannot find module/)
  })

  it('fails loudly on api version mismatch', async () => {
    const directory = await mkdtemp(join(tmpdir(), 'agentshim-native-stub-'))
    const stub = join(directory, 'stub.cjs')
    await writeFile(stub, `module.exports = { apiVersion: () => ${REQUIRED_NATIVE_API_VERSION + 1}, Engine: function Engine() {} };\n`)
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stub
    const result = loadNativeAddon()
    expect(result.failure?.reason).toBe('api-version-mismatch')
    expect(result.failure?.detail).toContain(String(REQUIRED_NATIVE_API_VERSION + 1))
  })

  it.skipIf(stagedAddon === undefined)('loads the built engine addon and serves read, grep, and glob in-process', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    expect(result.failure).toBeUndefined()
    if (result.engine === undefined) throw new Error('unreachable')

    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-root-'))
    await writeFile(join(root, 'notes.md'), 'alpha needle\n'.repeat(8))
    await writeFile(join(root, 'other.txt'), 'bravo\n'.repeat(8))
    const engine = new result.engine.Engine(root, { env: nativeEngineEnv({ AGENTSHIM_PROBE: 'native' }) })

    const read = await engine.readText({ path: 'notes.md', lineCount: 2 })
    expect(read.complete).toBe(true)
    expect(read.text).toContain('alpha needle')

    const grep = await engine.grepText({ pattern: 'needle', path: '.', fixedStrings: true })
    expect(grep.text).toContain('notes.md')

    const glob = await engine.globText({ pattern: '*.md' })
    expect(glob.text).toContain('notes.md')
    expect(glob.text).not.toContain('other.txt')

    await expect(engine.readText({ path: '../escape.md' })).rejects.toThrow()

    const prepared = engine.prepareRunProgram({
      program: process.execPath,
      args: ['-e', 'process.stdout.write("two-stage-native")'],
      env: { SYSTEMROOT: process.env.SYSTEMROOT ?? '' },
    })
    expect(prepared.argv.length).toBeGreaterThan(1)
    const outcome = await engine.spawnPrepared(prepared.handle)
    expect(outcome.text).toContain('two-stage-native')
    expect(outcome.errorCode).toBeUndefined()
    await expect(engine.spawnPrepared(prepared.handle)).rejects.toThrow(/unknown|already/i)

    await engine.close()
    await engine.close()
  })
})
