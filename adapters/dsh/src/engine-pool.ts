import { realpathSync } from 'node:fs'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import type { NativeEngine, NativeHostRuntime } from './native.ts'

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

export interface EngineCloseFailure {
  readonly root: string
  readonly phase: 'preflight' | 'release' | 'dispose'
  readonly error: unknown
}

export class EnginePool {
  private readonly entries = new Map<string, EnginePoolEntry>()
  private readonly refCounts = new Map<string, number>()
  private readonly pendingCloses = new Set<Promise<void>>()
  private disposed = false

  constructor(
    private readonly host: NativeHostRuntime,
    private readonly reportCloseFailure: (failure: EngineCloseFailure) => void,
  ) {}

  acquire(cwd: string): AcquiredEngine {
    if (this.disposed) {
      throw new HarnessError('dsh-agentshim: native engine pool is disposed', 'AGENTSHIM_ENGINE_CLOSED')
    }
    const canonical = canonicalOrUndefined(cwd)
    if (canonical === undefined) {
      throw new HarnessError(
        `dsh-agentshim: session cwd cannot be canonicalized: ${cwd}`,
        'AGENTSHIM_CWD_INVALID',
      )
    }
    let entry = this.entries.get(canonical)
    if (entry === undefined) {
      const engine = this.host.openEngine(canonical)
      try {
        engine.verifyBash()
      } catch (error) {
        this.trackClose({ engine, root: canonical }, 'preflight')
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
        this.trackClose(entry, 'release')
      }
    } else {
      this.refCounts.set(root, count - 1)
    }
  }

  async dispose(): Promise<void> {
    if (this.disposed) {
      await Promise.allSettled(this.pendingCloses)
      return
    }
    this.disposed = true
    const entries = [...this.entries.values()]
    this.entries.clear()
    this.refCounts.clear()
    for (const entry of entries) this.trackClose(entry, 'dispose')
    await Promise.allSettled(this.pendingCloses)
  }

  private trackClose(entry: EnginePoolEntry, phase: EngineCloseFailure['phase']): void {
    let tracked!: Promise<void>
    tracked = entry.engine.close()
      .catch((error: unknown) => {
        try {
          this.reportCloseFailure({ root: entry.root, phase, error })
        } catch {
          // A diagnostic sink must not turn one close failure into an unhandled rejection.
        }
      })
      .finally(() => this.pendingCloses.delete(tracked))
    this.pendingCloses.add(tracked)
  }
}
