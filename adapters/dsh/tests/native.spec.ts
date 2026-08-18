import { access, copyFile, mkdtemp, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Worker } from 'node:worker_threads'
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

function pdfFixture(): Buffer {
  const stream = 'BT /F1 18 Tf 20 150 Td (Native image probe) Tj ET\n'
  const bodies = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>',
    `<< /Length ${Buffer.byteLength(stream)} >>\nstream\n${stream}endstream`,
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>',
  ]
  let pdf = '%PDF-1.7\n'
  const offsets = [0]
  for (const [index, body] of bodies.entries()) {
    offsets.push(Buffer.byteLength(pdf))
    pdf += `${index + 1} 0 obj\n${body}\nendobj\n`
  }
  const xref = Buffer.byteLength(pdf)
  pdf += `xref\n0 ${bodies.length + 1}\n0000000000 65535 f \n`
  for (const offset of offsets.slice(1)) pdf += `${offset.toString().padStart(10, '0')} 00000 n \n`
  pdf += `trailer\n<< /Size ${bodies.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`
  return Buffer.from(pdf)
}

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

  it('fails loudly when the exact-version addon lacks NativeHostRuntime', async () => {
    const directory = await mkdtemp(join(tmpdir(), 'agentshim-native-host-stub-'))
    const stub = join(directory, 'stub.cjs')
    await writeFile(stub, `module.exports = { apiVersion: () => ${REQUIRED_NATIVE_API_VERSION}, Engine: function Engine() {} };\n`)
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stub
    const result = loadNativeAddon()
    expect(result.failure?.reason).toBe('api-version-mismatch')
    expect(result.failure?.detail).toContain('NativeHostRuntime')
  })

  it.skipIf(stagedAddon === undefined)('loads the built engine addon and serves read, grep, and glob in-process', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    expect(result.failure).toBeUndefined()
    if (result.engine === undefined) throw new Error('unreachable')

    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-root-'))
    await writeFile(join(root, 'notes.md'), 'alpha needle\n'.repeat(8))
    await writeFile(join(root, 'other.txt'), 'bravo\n'.repeat(8))
    await writeFile(join(root, 'document.pdf'), pdfFixture())
    const host = new result.engine.NativeHostRuntime({ env: nativeEngineEnv({ AGENTSHIM_PROBE: 'native' }) })
    const engine = host.openEngine(root)

    const read = await engine.readText({ path: 'notes.md', lineCount: 2 })
    expect(read.text).toContain('alpha needle')
    expect(read.text).toContain('next_start_line=3')

    const image = await engine.readText({ path: 'document.pdf', pdfMode: 'image' })
    expect(image.images).toHaveLength(1)
    expect(image.images[0]?.mimeType).toBe('image/png')

    const grep = await engine.grepText({ pattern: 'needle', path: '.', fixedStrings: true })
    expect(grep.text).toContain('notes.md')

    const glob = await engine.globText({ pattern: '*.md' })
    expect(glob.text).toContain('notes.md')
    expect(glob.text).not.toContain('other.txt')

    await expect(engine.readText({ path: '../escape.md' })).rejects.toThrow()

    const outside = await mkdtemp(join(tmpdir(), 'agentshim-native-unrestricted-'))
    const outsideFile = join(outside, 'outside.txt')
    await writeFile(outsideFile, 'unrestricted read\n')
    const unrestrictedHost = new result.engine.NativeHostRuntime({
      env: nativeEngineEnv({}),
      readScope: 'unrestricted',
    })
    const unrestricted = unrestrictedHost.openEngine(root)
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
    const host = new result.engine.NativeHostRuntime({ env: nativeEngineEnv({}) })
    const engine = host.openEngine(root)
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

  it.skipIf(stagedAddon === undefined)('shares foreground capacity across cwd engines without sharing shutdown', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    if (result.engine === undefined) throw new Error('unreachable')
    const firstRoot = await mkdtemp(join(tmpdir(), 'agentshim-native-capacity-a-'))
    const secondRoot = await mkdtemp(join(tmpdir(), 'agentshim-native-capacity-b-'))
    const host = new result.engine.NativeHostRuntime({ env: nativeEngineEnv({}) })
    const first = host.openEngine(firstRoot)
    const second = host.openEngine(secondRoot)
    const controllers: AbortController[] = []
    const running: Array<Promise<unknown>> = []

    for (let index = 0; index < 16; index++) {
      const engine = index % 2 === 0 ? first : second
      const controller = new AbortController()
      controllers.push(controller)
      const prepared = engine.prepareRunProgram({
        program: process.execPath,
        args: ['-e', 'setInterval(() => {}, 1000)'],
        timeoutMs: 10_000,
      }, controller.signal)
      running.push(engine.spawnPrepared(prepared.handle))
    }
    await new Promise(resolve => setTimeout(resolve, 300))

    const overflow = second.prepareRunProgram({
      program: process.execPath,
      args: ['-e', 'process.stdout.write("unexpected admission")'],
      timeoutMs: 2_000,
    })
    const rejected = await second.spawnPrepared(overflow.handle)
    expect(rejected.failure).toMatchObject({ code: 'AGENTSHIM_RESOURCE_BUSY' })

    for (const controller of controllers) controller.abort()
    await Promise.all(running)
    await first.close()

    const survivor = second.prepareRunProgram({
      program: process.execPath,
      args: ['-e', 'process.stdout.write("survivor")'],
    })
    expect((await second.spawnPrepared(survivor.handle)).stdout.text).toContain('survivor')
    await second.close()
  })

  it.skipIf(stagedAddon === undefined)('grants exact text and binary artifact capabilities without glob exposure', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    if (result.engine === undefined) throw new Error('unreachable')

    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-artifacts-'))
    const captureRoot = join(root, '.captures')
    const host = new result.engine.NativeHostRuntime({
      env: nativeEngineEnv({}),
      readScope: 'unrestricted',
      captureRoot,
      captureMaxBytes: 1024 * 1024,
    })
    const engine = host.openEngine(root)
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

    const foreignHost = new result.engine.NativeHostRuntime({
      env: nativeEngineEnv({}),
      readScope: 'normal',
      captureRoot,
    })
    const foreign = foreignHost.openEngine(root)
    await expect(foreign.readText({ path: binaryArtifact!.path, artifactOffset: 0 })).rejects.toThrow()
    await foreign.close()
    await engine.close()
  })

  it.skipIf(stagedAddon === undefined)('uses the configured page limit as the capture publish threshold', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    if (result.engine === undefined) throw new Error('unreachable')

    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-output-limit-'))
    const host = new result.engine.NativeHostRuntime({
      env: nativeEngineEnv({}),
      pageBudgetBytes: 80_000,
      captureRoot: join(root, '.captures'),
    })
    const engine = host.openEngine(root)
    const prepared = engine.prepareRunProgram({
      program: process.execPath,
      args: ['-e', 'process.stdout.write("x".repeat(60_000))'],
    })
    const outcome = await engine.spawnPrepared(prepared.handle)

    expect(outcome.text).toContain('x'.repeat(1_000))
    expect(outcome.artifacts).toEqual([])
    await engine.close()
  })

  it.skipIf(stagedAddon === undefined)('stops at the aggregate capture ceiling and publishes an exact partial artifact', async () => {
    process.env.AGENTSHIM_DSH_NATIVE_DLL = stagedAddon!
    const result = loadNativeAddon()
    if (result.engine === undefined) throw new Error('unreachable')
    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-ceiling-'))
    const host = new result.engine.NativeHostRuntime({
      env: nativeEngineEnv({}),
      captureRoot: join(root, '.captures'),
      captureMaxBytes: 1024 * 1024,
    })
    const engine = host.openEngine(root)
    const prepared = engine.prepareRunProgram({
      program: process.execPath,
      args: ['-e', 'process.stdout.write(Buffer.alloc(1024 * 1024 + 1, 0x61))'],
    })
    const outcome = await engine.spawnPrepared(prepared.handle)
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
    const host = new result.engine.NativeHostRuntime({ env: nativeEngineEnv({}) })
    const engine = host.openEngine(root)
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
    const host = new result.engine.NativeHostRuntime({ env: nativeEngineEnv({}) })
    const engine = host.openEngine(root)
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
    const host = new result.engine.NativeHostRuntime({ env: nativeEngineEnv({}) })
    const engine = host.openEngine(root)

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
    const host = new result.engine.NativeHostRuntime({ env: nativeEngineEnv({}) })
    const engine = host.openEngine(root)

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

  it.skipIf(stagedAddon === undefined)('terminates an owned child when its Node Worker exits', async () => {
    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-worker-'))
    const childStarted = join(root, 'child-started.txt')
    const orphanMarker = join(root, 'orphan-marker.txt')
    const childScript = [
      `require('node:fs').writeFileSync(${JSON.stringify(childStarted)}, 'started')`,
      `setTimeout(() => require('node:fs').writeFileSync(${JSON.stringify(orphanMarker)}, 'orphan'), 1500)`,
      'setInterval(() => {}, 1000)',
    ].join(';')
    const worker = new Worker(`
      const { parentPort, workerData } = require('node:worker_threads')
      const addon = require(workerData.addon)
      const host = new addon.NativeHostRuntime({ env: workerData.env })
      const engine = host.openEngine(workerData.root)
      engine.beginCall('worker-call')
      const prepared = engine.prepareRunProgram('worker-call', {
        program: workerData.program,
        args: ['-e', workerData.childScript],
      }).value
      void engine.spawnPrepared('worker-call', prepared.handle)
      parentPort.postMessage('spawn-requested')
    `, {
      eval: true,
      workerData: {
        addon: stagedAddon!,
        root,
        env: nativeEngineEnv({}),
        program: process.execPath,
        childScript,
      },
    })
    await new Promise<void>((resolve, reject) => {
      worker.once('message', () => resolve())
      worker.once('error', reject)
    })
    const deadline = Date.now() + 5_000
    while (true) {
      try {
        await access(childStarted)
        break
      } catch {
        if (Date.now() >= deadline) throw new Error('worker child did not start')
        await new Promise(resolve => setTimeout(resolve, 20))
      }
    }
    await worker.terminate()
    await new Promise(resolve => setTimeout(resolve, 2_000))
    await expect(access(orphanMarker)).rejects.toThrow()
  })

  it.skipIf(stagedAddon === undefined)('terminates a native background child when its Node Worker exits', async () => {
    const root = await mkdtemp(join(tmpdir(), 'agentshim-native-worker-background-'))
    const childStarted = join(root, 'background-started.txt')
    const orphanMarker = join(root, 'background-orphan.txt')
    const worker = new Worker(`
      const { parentPort, workerData } = require('node:worker_threads')
      const addon = require(workerData.addon)
      const host = new addon.NativeHostRuntime({ env: workerData.env })
      const engine = host.openEngine(workerData.root)
      engine.beginCall('worker-background-call')
      const prepared = engine.prepareBash('worker-background-call', {
        command: 'printf started > background-started.txt; sleep 1.5; printf orphan > background-orphan.txt; while :; do sleep 1; done',
      }).value
      const handle = engine.startBackgroundPrepared('worker-background-call', prepared.handle).value
      void handle.done()
      globalThis.backgroundHandle = handle
      parentPort.postMessage('spawn-requested')
    `, {
      eval: true,
      workerData: {
        addon: stagedAddon!,
        root,
        env: nativeEngineEnv({}),
      },
    })
    await new Promise<void>((resolve, reject) => {
      worker.once('message', () => resolve())
      worker.once('error', reject)
    })
    const deadline = Date.now() + 5_000
    while (true) {
      try {
        await access(childStarted)
        break
      } catch {
        if (Date.now() >= deadline) throw new Error('worker background child did not start')
        await new Promise(resolve => setTimeout(resolve, 20))
      }
    }
    await worker.terminate()
    await new Promise(resolve => setTimeout(resolve, 2_000))
    await expect(access(orphanMarker)).rejects.toThrow()
  })
})
