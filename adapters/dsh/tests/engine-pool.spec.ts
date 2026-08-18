import { mkdtemp } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'
import { canonicalOrUndefined, EnginePool } from '../src/engine-pool.ts'
import type { EngineCloseFailure } from '../src/engine-pool.ts'
import type { NativeEngine, NativeHostRuntime } from '../src/native.ts'

function engine(close: () => Promise<void>, verify: () => void = () => {}): NativeEngine {
  return {
    verifyBash: verify,
    close,
  } as unknown as NativeEngine
}

describe('EnginePool lifecycle', () => {
  it('reports a correlated release failure while other roots still close', async () => {
    const firstRoot = await mkdtemp(join(tmpdir(), 'agentshim-pool-close-a-'))
    const secondRoot = await mkdtemp(join(tmpdir(), 'agentshim-pool-close-b-'))
    let secondSettled = false
    const opened = new Map<string, NativeEngine>([
      [canonicalOrUndefined(firstRoot)!, engine(() => Promise.reject(new Error('injected close failure')))],
      [canonicalOrUndefined(secondRoot)!, engine(async () => {
        await new Promise(resolve => setTimeout(resolve, 10))
        secondSettled = true
      })],
    ])
    const host = {
      openEngine(root: string) {
        return opened.get(root)!
      },
    } satisfies NativeHostRuntime
    const failures: EngineCloseFailure[] = []
    const pool = new EnginePool(host, failure => failures.push(failure))

    const first = pool.acquire(firstRoot)
    pool.acquire(secondRoot)
    pool.release(first.root)
    await pool.dispose()

    expect(secondSettled).toBe(true)
    expect(failures).toHaveLength(1)
    expect(failures[0]).toMatchObject({ root: first.root, phase: 'release' })
    expect(failures[0]?.error).toMatchObject({ message: 'injected close failure' })
  })

  it('can reopen one cwd while its previous engine is still closing', async () => {
    const root = await mkdtemp(join(tmpdir(), 'agentshim-pool-reopen-'))
    let releaseClose!: () => void
    const closing = new Promise<void>(resolve => {
      releaseClose = resolve
    })
    const first = engine(() => closing)
    const second = engine(() => Promise.resolve())
    const engines = [first, second]
    const host = {
      openEngine() {
        const next = engines.shift()
        if (next === undefined) throw new Error('unexpected third engine')
        return next
      },
    } satisfies NativeHostRuntime
    const pool = new EnginePool(host, () => {})

    const acquired = pool.acquire(root)
    pool.release(acquired.root)
    const reopened = pool.acquire(root)
    expect(reopened.engine).toBe(second)
    releaseClose()
    pool.release(reopened.root)
    await pool.dispose()
  })
})
