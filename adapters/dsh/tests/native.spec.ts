import { copyFile, mkdtemp, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { afterEach, describe, expect, it } from 'vitest'
import { loadNativeAddon, nativeEngineEnv, nativePlatformTriple, REQUIRED_NATIVE_API_VERSION } from '../src/native.ts'
import type { NativeSandboxAttribution } from '../src/native.ts'

const builtLibrary = fileURLToPath(new URL(
  `../../../target/debug/${process.platform === 'win32'
    ? 'agentshim_napi.dll'
    : process.platform === 'darwin'
      ? 'libagentshim_napi.dylib'
      : 'libagentshim_napi.so'}`,
  import.meta.url,
))

async function stageAddon(): Promise<string | undefined> {
  try {
    const directory = await mkdtemp(join(tmpdir(), 'agentshim-native-'))
    const staged = join(directory, 'agentshim_napi.node')
    await copyFile(builtLibrary, staged)
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
    const result = loadNativeAddon()
    expect(result.failure?.reason).toBe('addon-unavailable')
    expect(result.failure?.detail).toMatch(/Cannot find module|could not be loaded/i)
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

    const outside = await mkdtemp(join(tmpdir(), 'agentshim-native-unrestricted-'))
    const outsideFile = join(outside, 'outside.txt')
    await writeFile(outsideFile, 'unrestricted read\n')
    const unrestricted = new result.engine.Engine(root, {
      env: nativeEngineEnv({}),
      readScope: 'unrestricted',
    })
    expect((await unrestricted.readText({ path: outsideFile })).text).toContain('unrestricted read')
    await unrestricted.close()

    const prepared = engine.prepareRunProgram({
      program: process.execPath,
      args: ['-e', 'process.stdout.write("two-stage-native")'],
      env: { SYSTEMROOT: process.env.SYSTEMROOT ?? '' },
    })
    expect(prepared.argv.length).toBeGreaterThan(1)
    const outcome = await engine.spawnPrepared(prepared.handle)
    expect(outcome.text).toContain('two-stage-native')
    expect(outcome.stdout.text).toContain('two-stage-native')
    expect(outcome.stdout.totalBytes).toBeGreaterThan(0)
    expect(outcome.failure).toBeUndefined()

    const bashPrepared = engine.prepareBash({ command: 'printf "bash-native"' })
    const bashOutcome = await engine.spawnPrepared(bashPrepared.handle)
    expect(bashOutcome.stdout.text).toContain('bash-native')
    expect(bashOutcome.stderr.text).toContain('bash-native')
    expect(bashOutcome.stdout.totalBytes).toBeGreaterThan(0)
    expect(bashOutcome.stderr.totalBytes).toBeGreaterThan(0)

    await expect(engine.spawnPrepared(prepared.handle)).rejects.toThrow(/unknown|already/i)

    await engine.close()
    await engine.close()
  })

  it.skipIf(stagedAddon === undefined)('classifies denials and runner failures from the settled native outcome', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    if (result.engine === undefined) throw new Error('unreachable')

    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-classify-'))
    const engine = new result.engine.Engine(root, { env: nativeEngineEnv({}) })
    const spawn = async (statement: string, attribution?: NativeSandboxAttribution) => {
      const prepared = engine.prepareRunProgram({
        program: process.execPath,
        args: ['-e', statement],
        env: { SYSTEMROOT: process.env.SYSTEMROOT ?? '' },
      })
      return await engine.spawnPrepared(prepared.handle, undefined, attribution)
    }
    const runnerFailureRules = [{
      allowedExitCodes: [70],
      fatalSignatures: ['runner failed'],
      informationalLines: ['notice'],
    }]

    const denied = await spawn(
      'process.stderr.write("write: Permission denied"); process.exit(1)',
      { denialSignatures: ['permission denied'] },
    )
    expect(denied.exitCode).toBe('1')
    expect(denied.denied).toBe(true)
    expect(denied.runnerFailed).toBe(false)

    const runnerFailed = await spawn(
      'process.stderr.write("notice\\nRUNNER FAILED to start"); process.exit(70)',
      { runnerFailureRules },
    )
    expect(runnerFailed.exitCode).toBe('70')
    expect(runnerFailed.denied).toBe(false)
    expect(runnerFailed.runnerFailed).toBe(true)

    const framed = await spawn(
      'process.stderr.write("RUNNER FAILED to start"); process.exit(1)',
      { denialSignatures: ['permission denied'], runnerFailureRules },
    )
    expect(framed.exitCode).toBe('1')
    expect(framed.denied).toBe(false)
    expect(framed.runnerFailed).toBe(false)

    const clean = await spawn(
      'process.stderr.write("permission denied in harmless prose"); process.exit(0)',
      { denialSignatures: ['permission denied'] },
    )
    expect(clean.exitCode).toBe('0')
    expect(clean.denied).toBe(false)
    expect(clean.runnerFailed).toBe(false)

    await engine.close()
  })

  it.skipIf(stagedAddon === undefined)('grants exact text and binary artifact capabilities without glob exposure', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    if (result.engine === undefined) throw new Error('unreachable')

    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-artifacts-'))
    const captureRoot = join(root, '.captures')
    const engine = new result.engine.Engine(root, {
      env: nativeEngineEnv({}),
      readScope: 'unrestricted',
      captureRoot,
      captureMaxBytes: 1024 * 1024,
    })
    const run = async (script: string) => {
      const prepared = engine.prepareRunProgram({ program: process.execPath, args: ['-e', script] })
      return await engine.spawnPrepared(prepared.handle)
    }

    const textOutcome = await run('process.stdout.write("needle\\n".repeat(9000))')
    const textArtifact = textOutcome.artifacts.find(artifact => artifact.stream === 'stdout')
    expect(textArtifact).toBeDefined()
    const textRead = await engine.readText({ path: textArtifact!.path, lineCount: 2 })
    expect(textRead.text).toContain('needle')
    expect((await engine.grepText({ path: textArtifact!.path, pattern: 'needle', fixedStrings: true })).text)
      .toContain('needle')

    const binaryOutcome = await run('process.stdout.write(Buffer.from([0,255,65,66]))')
    const binaryArtifact = binaryOutcome.artifacts.find(artifact => artifact.stream === 'stdout')
    expect(binaryArtifact).toBeDefined()
    const binaryRead = await engine.readText({ path: binaryArtifact!.path, artifactOffset: 0 })
    expect(binaryRead.text).toContain('Encoding: base64')
    await expect(engine.grepText({ path: binaryArtifact!.path, pattern: 'AB', fixedStrings: true }))
      .rejects.toThrow(/binary artifact.*artifactOffset/i)

    await expect(engine.globText({ path: captureRoot, pattern: '**/*' })).rejects.toThrow(/capture root/i)
    expect((await engine.globText({ pattern: '**/*' })).text).not.toContain('.captures')

    const foreign = new result.engine.Engine(root, {
      env: nativeEngineEnv({}),
      readScope: 'normal',
      captureRoot,
    })
    await expect(foreign.readText({ path: binaryArtifact!.path, artifactOffset: 0 })).rejects.toThrow()
    await foreign.close()
    await engine.close()
  })

  it.skipIf(stagedAddon === undefined)('stops at the aggregate capture ceiling and publishes an exact partial artifact', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    if (result.engine === undefined) throw new Error('unreachable')
    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-ceiling-'))
    const engine = new result.engine.Engine(root, {
      env: nativeEngineEnv({}),
      captureRoot: join(root, '.captures'),
      captureMaxBytes: 1024 * 1024,
    })
    const prepared = engine.prepareRunProgram({
      program: process.execPath,
      args: ['-e', 'process.stdout.write(Buffer.alloc(1024 * 1024 + 1, 0x61))'],
    })
    const outcome = await engine.spawnPrepared(prepared.handle)
    expect(outcome.limitExceeded).toBe(true)
    expect(outcome.failure).toMatchObject({ code: 'AGENTSHIM_CAPTURE_LIMIT_EXCEEDED' })
    const artifact = outcome.artifacts.find(value => value.stream === 'stdout')
    expect(artifact?.complete).toBe(false)
    expect(artifact?.bytes).toBeLessThanOrEqual(1024 * 1024)
    await engine.close()
  })

  it.skipIf(stagedAddon === undefined)('starts, drains, cancels, and settles a native background bash job', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    if (result.engine === undefined) throw new Error('unreachable')

    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-bg-'))
    const engine = new result.engine.Engine(root, { env: nativeEngineEnv({}) })
    const prepared = engine.prepareBash({
      command: 'for i in 1 2 3 4 5 6; do printf "line-%s\\n" "$i"; sleep 0.05; done; exit 0',
    })
    expect(prepared.argv.length).toBeGreaterThan(1)

    const handle = engine.startBackgroundPrepared(prepared.handle)

    const output1 = handle.readOutput()
    expect(output1.length).toBeGreaterThanOrEqual(0)

    const outcome = await handle.done()
    expect(outcome.status).toBe('completed')
    expect(outcome.exitCode).toBe('0')

    const finalOutput = handle.readOutput()
    expect(finalOutput).toContain('line-1')
    expect(finalOutput).toContain('line-6')

    await handle.dispose()
    await engine.close()
  })

  it.skipIf(stagedAddon === undefined)('settles at least three native background jobs concurrently', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    if (result.engine === undefined) throw new Error('unreachable')
    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-bg-concurrent-'))
    const engine = new result.engine.Engine(root, { env: nativeEngineEnv({}) })
    const handles = Array.from({ length: 3 }, (_, index) => {
      const prepared = engine.prepareBash({ command: `sleep 0.1; printf concurrent-${index}` })
      return engine.startBackgroundPrepared(prepared.handle)
    })
    const outcomes = await Promise.all(handles.map(handle => handle.done()))
    expect(outcomes.every(outcome => outcome.status === 'completed')).toBe(true)
    for (const [index, handle] of handles.entries()) {
      expect(handle.readOutput()).toContain(`concurrent-${index}`)
      await handle.dispose()
    }
    await engine.close()
  })

  it.skipIf(stagedAddon === undefined)('cancels a long-running native background bash job and settles as killed', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    if (result.engine === undefined) throw new Error('unreachable')

    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-bg-cancel-'))
    const engine = new result.engine.Engine(root, { env: nativeEngineEnv({}) })

    const prepared = engine.prepareBash({
      command: 'while :; do printf x; sleep 0.05; done',
    })
    const handle = engine.startBackgroundPrepared(prepared.handle)

    await new Promise(resolve => setTimeout(resolve, 200))
    const partial = handle.readOutput()
    expect(partial).toContain('x')

    handle.cancel('test cancellation')
    const outcome = await handle.done()
    expect(outcome.status).toBe('killed')

    await handle.dispose()
    await engine.close()
  })

  it.skipIf(stagedAddon === undefined)('maps structured native failures and relays foreground cancellation', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    if (result.engine === undefined) throw new Error('unreachable')
    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-errors-'))
    const engine = new result.engine.Engine(root, { env: nativeEngineEnv({}) })

    await expect(engine.readText({ path: '../missing.txt' })).rejects.toMatchObject({
      code: 'AGENTSHIM_READ_PATH_FAILED',
      details: { kind: 'path' },
    })

    const controller = new AbortController()
    const prepared = engine.prepareRunProgram({
      program: process.execPath,
      args: ['-e', 'setInterval(() => process.stdout.write("x"), 10)'],
    }, controller.signal)
    controller.abort()
    await expect(engine.spawnPrepared(prepared.handle)).rejects.toMatchObject({
      code: 'ABORTED',
      name: 'AbortError',
    })

    const timeoutPrepared = engine.prepareRunProgram({
      program: process.execPath,
      args: ['-e', 'setTimeout(() => {}, 5_000)'],
      timeoutMs: 100,
    })
    const timeoutOutcome = await engine.spawnPrepared(timeoutPrepared.handle)
    expect(timeoutOutcome.failure).toMatchObject({
      code: 'AGENTSHIM_TIMEOUT',
      details: { kind: 'timeout' },
    })

    await engine.close()
    await expect(engine.readText({ path: 'anything.txt' })).rejects.toMatchObject({
      code: 'AGENTSHIM_ENGINE_CLOSED',
    })
  })
})
