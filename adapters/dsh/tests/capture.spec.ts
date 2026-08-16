import { lstat, mkdtemp, readFile, rm, stat, symlink } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterEach, describe, expect, it } from 'vitest'
import {
  CaptureStore,
  INLINE_OUTPUT_BYTES,
  MIN_CAPTURE_MAX_BYTES,
} from '../src/capture.ts'

const roots: string[] = []

async function root(): Promise<string> {
  const value = await mkdtemp(join(tmpdir(), 'dsh-capture-test-'))
  roots.push(value)
  return value
}

afterEach(async () => {
  await Promise.allSettled(roots.splice(0).map(path => rm(path, { recursive: true, force: true })))
})

async function append(store: CaptureStore, id: string, data: Buffer, offset = 0) {
  return store.append({
    bridgeVersion: 2,
    captureId: id,
    stream: 'output',
    offset,
    data: data.toString('base64'),
  })
}

describe('sandbox-aware CaptureStore', () => {
  it('deletes small valid text but durably publishes byte-exact binary output', async () => {
    const captureRoot = await root()
    const store = new CaptureStore(captureRoot, MIN_CAPTURE_MAX_BYTES)
    const small = await store.begin({ sessionId: 'small', callId: 'call', streams: ['output'] })
    await append(store, small.id, Buffer.from('plain text'))
    await store.complete({ bridgeVersion: 2, captureId: small.id, complete: true, totals: { output: 10 } })
    expect((await small.completion).artifacts).toEqual({})

    const binary = Buffer.concat([
      Buffer.alloc(64 * 1024 - 1, 0x61),
      Buffer.from([0xf0, 0x9f, 0x99, 0x82, 0, 0xff]),
    ])
    const captured = await store.begin({ sessionId: 'binary', callId: 'call', streams: ['output'] })
    await append(store, captured.id, binary.subarray(0, 64 * 1024))
    await append(store, captured.id, binary.subarray(64 * 1024), 64 * 1024)
    await store.complete({ bridgeVersion: 2, captureId: captured.id, complete: true, totals: { output: binary.byteLength } })
    const artifact = (await captured.completion).artifacts.output
    expect(artifact).toMatchObject({ bytes: binary.byteLength, complete: true, mediaType: 'application/octet-stream' })
    expect(await readFile(artifact!.path)).toEqual(binary)
    expect(await store.grant(artifact!.path)).toBe(artifact!.path)

    const restarted = new CaptureStore(captureRoot, MIN_CAPTURE_MAX_BYTES)
    expect(await restarted.grant(artifact!.path)).toBe(artifact!.path)
  })

  it('accepts the exact ceiling and rejects one additional byte with an exact partial artifact', async () => {
    const store = new CaptureStore(await root(), MIN_CAPTURE_MAX_BYTES)
    const exact = await store.begin({ sessionId: 'exact', callId: 'call', streams: ['output'] })
    const ceiling = Buffer.alloc(MIN_CAPTURE_MAX_BYTES, 0x78)
    await append(store, exact.id, ceiling)
    await store.complete({ bridgeVersion: 2, captureId: exact.id, complete: true, totals: { output: ceiling.byteLength } })
    expect(await readFile((await exact.completion).artifacts.output!.path)).toEqual(ceiling)

    const overflow = await store.begin({ sessionId: 'overflow', callId: 'call', streams: ['output'], background: true })
    await expect(append(store, overflow.id, Buffer.alloc(MIN_CAPTURE_MAX_BYTES + 1, 0x79)))
      .rejects.toMatchObject({ code: 'AGENTSHIM_CAPTURE_LIMIT_EXCEEDED' })
    await store.complete({
      bridgeVersion: 2,
      captureId: overflow.id,
      complete: false,
      totals: { output: 0 },
      error: 'AGENTSHIM_CAPTURE_LIMIT_EXCEEDED',
    })
    const artifact = (await overflow.completion).artifacts.output!
    expect(artifact).toMatchObject({ bytes: MIN_CAPTURE_MAX_BYTES, complete: false })
    expect((await stat(artifact.path)).size).toBe(MIN_CAPTURE_MAX_BYTES)
  })

  it('rejects traversal, adjacent names, and symlinks while retaining owner-only Unix permissions', async () => {
    const captureRoot = await root()
    const store = new CaptureStore(captureRoot, MIN_CAPTURE_MAX_BYTES)
    const capture = await store.begin({ sessionId: 'grant', callId: 'call', streams: ['output'] })
    await append(store, capture.id, Buffer.alloc(INLINE_OUTPUT_BYTES + 1, 0x61))
    await store.complete({ bridgeVersion: 2, captureId: capture.id, complete: true, totals: { output: INLINE_OUTPUT_BYTES + 1 } })
    const path = (await capture.completion).artifacts.output!.path
    expect(await store.grant(join(captureRoot, '..', 'outside.raw'))).toBeUndefined()
    expect(await store.grant(join(captureRoot, 'adjacent.raw'))).toBeUndefined()

    if (process.platform !== 'win32') {
      const link = `${path}.link`
      await symlink(path, link)
      expect(await store.grant(link)).toBeUndefined()
      expect((await lstat(link)).isSymbolicLink()).toBe(true)
    }
    if (process.platform !== 'win32') {
      expect((await stat(captureRoot)).mode & 0o777).toBe(0o700)
      expect((await stat(path)).mode & 0o777).toBe(0o600)
    }
  })
})
