import { realpathSync } from 'node:fs'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import type { NativeEngine, NativeEngineOptions } from './native.ts'

export function canonicalOrUndefined(path: string): string | undefined {
  try {
    return realpathSync.native(path)
  } catch {
    return undefined
  }
}

interface EnginePoolEntry {
  readonly engine: NativeEngine
  readonly root: string
}

export interface AcquiredEngine {
  readonly engine: NativeEngine
  readonly root: string
}

export type NativeEngineConstructor = new (root: string, options?: NativeEngineOptions) => NativeEngine

export class EnginePool {
  private readonly entries = new Map<string, EnginePoolEntry>()
  private readonly refCounts = new Map<string, number>()
  private readonly pendingCloses: Array<Promise<unknown>> = []

  constructor(
    private readonly engineClass: NativeEngineConstructor,
    private readonly options: NativeEngineOptions,
  ) {}

  acquire(cwd: string): AcquiredEngine {
    const canonical = canonicalOrUndefined(cwd)
    if (canonical === undefined) {
      throw new HarnessError(
        `dsh-agentshim: session cwd cannot be canonicalized: ${cwd}`,
        'AGENTSHIM_CWD_INVALID',
      )
    }
    let entry = this.entries.get(canonical)
    if (entry === undefined) {
      const engine = new this.engineClass(canonical, this.options)
      try {
        engine.verifyBash()
      } catch (error) {
        this.pendingCloses.push(engine.close().catch(() => {}))
        throw error
      }
      entry = { engine, root: canonical }
      this.entries.set(canonical, entry)
      this.refCounts.set(canonical, 0)
    }
    this.refCounts.set(canonical, (this.refCounts.get(canonical) ?? 0) + 1)
    return entry
  }

  release(root: string): void {
    const count = this.refCounts.get(root) ?? 0
    if (count <= 1) {
      this.refCounts.delete(root)
      const entry = this.entries.get(root)
      if (entry !== undefined) {
        this.entries.delete(root)
        this.pendingCloses.push(entry.engine.close().catch(() => {}))
      }
    } else {
      this.refCounts.set(root, count - 1)
    }
  }

  async dispose(): Promise<void> {
    const entries = [...this.entries.values()]
    this.entries.clear()
    this.refCounts.clear()
    await Promise.allSettled([
      ...entries.map(entry => entry.engine.close()),
      ...this.pendingCloses,
    ])
    this.pendingCloses.length = 0
  }
}
