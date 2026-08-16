import { mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { afterEach, describe, expect, it } from 'vitest'
import { Client } from '@modelcontextprotocol/sdk/client/index.js'
import type { Tool } from '@modelcontextprotocol/sdk/types.js'
import type { RequestOptions } from '@modelcontextprotocol/sdk/shared/protocol.js'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import { TOOL_ABORTED } from '@deepseek-ai/dsh-tools'
import {
  EXPECTED_TOOL_ORDER,
  MIN_TOOL_CALL_TIMEOUT_MS,
  canonicalJson,
  catalogFingerprint,
  createSession,
  defaultInstallCommand,
  resolveSessionConfig,
  rewriteTypeArrays,
  serverArgs,
  validateCatalog,
} from '../src/session.ts'
import type { AgentshimSession, ResolvedSessionConfig, SessionConfigInput, SessionLogger } from '../src/session.ts'

const fixturePath = fileURLToPath(new URL('./fixture-server.mjs', import.meta.url))

const noopLogger = { info: () => {}, warn: () => {}, error: () => {} }

const sessions: AgentshimSession[] = []
const roots: string[] = []

afterEach(async () => {
  for (const session of sessions.splice(0)) await session.dispose()
  for (const root of roots.splice(0)) await rm(root, { recursive: true, force: true })
})

function sleep(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms))
}

async function waitFor(predicate: () => Promise<boolean>, timeoutMs = 5_000): Promise<void> {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (await predicate()) return
    await sleep(50)
  }
  throw new Error('condition not reached within timeout')
}

async function exists(path: string): Promise<boolean> {
  try {
    await stat(path)
    return true
  } catch {
    return false
  }
}

interface Fixture {
  readonly root: string
  readonly resolved: ResolvedSessionConfig
  readonly files: {
    readonly report: string
    readonly boot: string
    readonly crash: string
    readonly exit: string
  }
}

async function setupFixture(
  fixtureEnv: Record<string, string> = {},
  overrides: Partial<SessionConfigInput> = {},
): Promise<Fixture> {
  const root = await mkdtemp(join(tmpdir(), 'dsh-agentshim-'))
  roots.push(root)
  const env = {
    FIXTURE_REPORT: join(root, 'report.json'),
    FIXTURE_BOOT_FILE: join(root, 'boot.txt'),
    FIXTURE_CRASH_FILE: join(root, 'crash-now'),
    FIXTURE_EXIT_FILE: join(root, 'exit.txt'),
    ...fixtureEnv,
  }
  const resolved = await resolveSessionConfig({
    root,
    command: process.execPath,
    commandArgs: [fixturePath],
    readScope: 'normal',
    env,
    toolCallTimeoutMs: MIN_TOOL_CALL_TIMEOUT_MS,
    ...overrides,
  })
  return {
    root,
    resolved,
    files: {
      report: env.FIXTURE_REPORT,
      boot: env.FIXTURE_BOOT_FILE,
      crash: join(root, 'crash-now'),
      exit: join(root, 'exit.txt'),
    },
  }
}

function startSession(
  fixture: Fixture,
  env: Record<string, string> = {},
  createClient?: () => Client,
  logger: SessionLogger = noopLogger,
): AgentshimSession {
  const session = createSession(
    { ...fixture.resolved, env: { ...fixture.resolved.env, ...env } },
    { logger, ...(createClient !== undefined ? { createClient } : {}) },
  )
  sessions.push(session)
  return session
}

function syntheticCatalog(mutate: (tools: Tool[]) => void = () => {}): Tool[] {
  const tools = EXPECTED_TOOL_ORDER.map(name => ({
    name,
    description: `${name} description`,
    inputSchema: { type: 'object', additionalProperties: false, properties: {} },
  })) as unknown as Tool[]
  mutate(tools)
  return tools
}

describe('pure schema helpers', () => {
  it('rewrites type arrays to oneOf with the whole node replaced', () => {
    expect(rewriteTypeArrays({
      type: ['string', 'null'],
      maxLength: 5,
      description: 'optional stdin',
      default: null,
    })).toEqual({
      oneOf: [{ type: 'string' }, { type: 'null' }],
      description: 'optional stdin',
      default: null,
    })
  })

  it('descends only into schema positions and leaves enum values untouched', () => {
    const schema = {
      type: 'object',
      properties: {
        mode: { type: 'string', enum: ['auto', 'text'] },
        nested: { type: 'array', items: { type: ['string', 'null'] } },
      },
      additionalProperties: false,
    }
    const rewritten = rewriteTypeArrays(schema) as typeof schema
    expect(rewritten.properties?.mode).toEqual({ type: 'string', enum: ['auto', 'text'] })
    expect(rewritten.properties?.nested).toEqual({ type: 'array', items: { oneOf: [{ type: 'string' }, { type: 'null' }] } })
  })

  it('rejects type arrays that are not at least two strings', () => {
    expect(() => rewriteTypeArrays({ type: ['string'] })).toThrow(/at least two string types/)
    expect(() => rewriteTypeArrays({ type: ['string', 3] })).toThrow(/at least two string types/)
  })

  it('canonicalizes object key order for fingerprints', () => {
    expect(canonicalJson({ b: 1, a: { d: 2, c: 3 } })).toBe(canonicalJson({ a: { c: 3, d: 2 }, b: 1 }))
  })

  it('changes the fingerprint when any catalog field drifts', () => {
    const baseline = catalogFingerprint(syntheticCatalog().map(tool => ({
      name: tool.name,
      description: tool.description ?? '',
      inputSchema: tool.inputSchema,
    })))
    const drift = catalogFingerprint(syntheticCatalog().map(tool => ({
      name: tool.name,
      description: tool.description === 'read description' ? 'drifted' : tool.description ?? '',
      inputSchema: tool.inputSchema,
    })))
    expect(drift).not.toBe(baseline)
  })
})

describe('validateCatalog', () => {
  it('accepts the fixed five-name contract and returns published entries', () => {
    const snapshot = validateCatalog(syntheticCatalog())
    expect(snapshot.tools.map(tool => tool.name)).toEqual([...EXPECTED_TOOL_ORDER])
    expect(snapshot.fingerprint).toMatch(/^[0-9a-f]{64}$/)
  })

  it('keeps non-subset constraint keywords in the published parameters while the gate passes', () => {
    const snapshot = validateCatalog(syntheticCatalog(tools => {
      const read = tools[0] as unknown as { inputSchema: { properties: Record<string, unknown> } }
      read.inputSchema.properties = {
        path: { type: 'string', minLength: 1, description: 'p' },
        count: { type: 'integer', minimum: 1, maximum: 9, description: 'c' },
        named: { type: 'object', properties: { minimum: { type: 'integer', minimum: 3 } } },
        extra: { type: 'object', additionalProperties: { type: 'string' }, default: {} },
      }
    }))
    const parameters = snapshot.tools[0]?.parameters.properties as Record<string, Record<string, unknown>>
    expect(parameters.path).toMatchObject({ minLength: 1 })
    expect(parameters.count).toMatchObject({ minimum: 1, maximum: 9 })
    expect(parameters.named).toEqual({ type: 'object', properties: { minimum: { type: 'integer', minimum: 3 } } })
    expect(parameters.extra).toEqual({ type: 'object', additionalProperties: { type: 'string' }, default: {} })
  })

  it('fails loud on missing, extra, reordered, and duplicate names', () => {
    expect(() => validateCatalog(syntheticCatalog(tools => tools.splice(1, 1)))).toThrow(/exactly \[read, grep, glob, run_program, bash\]/)
    expect(() => validateCatalog(syntheticCatalog(tools => tools.push(tools[0] as Tool)))).toThrow(/more than once/)
    expect(() => validateCatalog(syntheticCatalog(tools => tools.reverse()))).toThrow(/in this order/)
  })

  it('fails loud on required task execution and unsupported vocabulary', () => {
    expect(() => validateCatalog(syntheticCatalog(tools => {
      (tools[3] as unknown as { execution: unknown }).execution = { taskSupport: 'required' }
    }))).toThrow(/task-based execution/)
    expect(() => validateCatalog(syntheticCatalog(tools => {
      const read = tools[0] as unknown as { inputSchema: Record<string, unknown> }
      read.inputSchema = { type: 'object', anyOf: [{ type: 'string' }, { type: 'null' }] }
    }))).toThrow(/outside the supported subset/)
  })
})

describe('config resolution', () => {
  it('builds the fixed server argv', () => {
    expect(serverArgs({
      root: '/root',
      command: '/bin/cargo',
      commandArgs: ['run', '--locked', '--'],
      readScope: 'unrestricted',
      env: {},
      toolCallTimeoutMs: MIN_TOOL_CALL_TIMEOUT_MS,
    })).toEqual(['run', '--locked', '--', 'serve', '--client-profile', 'codex', '--read-scope', 'unrestricted'])
  })

  it('resolves the platform default install path from the environment', () => {
    const saved: Record<string, string | undefined> = { ...process.env }
    try {
      if (process.platform === 'win32') {
        process.env.LOCALAPPDATA = 'C:\\FakeLocal'
        expect(defaultInstallCommand()).toBe(join('C:\\FakeLocal', 'agentshim', 'bin', 'agentshim.exe'))
        delete process.env.LOCALAPPDATA
        expect(() => defaultInstallCommand()).toThrow(/LOCALAPPDATA/)
      } else {
        process.env.XDG_DATA_HOME = '/xdg-data'
        expect(defaultInstallCommand()).toBe('/xdg-data/agentshim/bin/agentshim')
        delete process.env.XDG_DATA_HOME
        process.env.HOME = '/home/tester'
        expect(defaultInstallCommand()).toBe('/home/tester/.local/share/agentshim/bin/agentshim')
      }
    } finally {
      process.env = saved
    }
  })

  it('fails loud on a missing root, a relative command, a missing executable, and a low timeout', async () => {
    const root = await mkdtemp(join(tmpdir(), 'dsh-agentshim-'))
    roots.push(root)
    const base = {
      root,
      commandArgs: [],
      readScope: 'normal' as const,
      env: {},
    }
    await expect(resolveSessionConfig({ ...base, root: join(root, 'missing'), command: process.execPath, toolCallTimeoutMs: MIN_TOOL_CALL_TIMEOUT_MS }))
      .rejects.toThrow(/does not exist/)
    await expect(resolveSessionConfig({ ...base, command: 'agentshim', toolCallTimeoutMs: MIN_TOOL_CALL_TIMEOUT_MS }))
      .rejects.toThrow(/absolute/)
    await expect(resolveSessionConfig({ ...base, command: join(root, 'agentshim-missing.exe'), toolCallTimeoutMs: MIN_TOOL_CALL_TIMEOUT_MS }))
      .rejects.toThrow(/not found/)
    await expect(resolveSessionConfig({ ...base, command: process.execPath, toolCallTimeoutMs: MIN_TOOL_CALL_TIMEOUT_MS - 1 }))
      .rejects.toThrow(/toolCallTimeoutMs/)
    const filePath = join(root, 'plain-file')
    await writeFile(filePath, 'x')
    await expect(resolveSessionConfig({ ...base, root: filePath, command: process.execPath, toolCallTimeoutMs: MIN_TOOL_CALL_TIMEOUT_MS }))
      .rejects.toThrow(/not a directory/)
  })
})

describe('session against the fixture server', () => {
  it('starts, validates the five-tool catalog in order, and publishes runtime schemas', async () => {
    const fixture = await setupFixture()
    const session = startSession(fixture)
    const snapshot = await session.ready
    expect(snapshot.tools.map(tool => tool.name)).toEqual([...EXPECTED_TOOL_ORDER])
    expect(snapshot.tools[0]?.description).toContain('numbered lines')
    const runProgram = snapshot.tools.find(tool => tool.name === 'run_program')
    expect(runProgram).toBeDefined()
    const runProperties = runProgram!.parameters.properties as Record<string, unknown>
    expect(runProperties.stdin).toEqual({
      oneOf: [{ type: 'string' }, { type: 'null' }],
      description: 'Optional UTF-8 stdin.',
    })
    const read = snapshot.tools[0]?.parameters.properties as Record<string, Record<string, unknown>>
    expect(read.line_count).toMatchObject({ minimum: 1, maximum: 2000 })
  })

  it('drains a paginated tools/list to the same catalog', async () => {
    const fixture = await setupFixture({ FIXTURE_PAGINATE: '1' })
    const session = startSession(fixture)
    const snapshot = await session.ready
    expect(snapshot.tools.map(tool => tool.name)).toEqual([...EXPECTED_TOOL_ORDER])
  })

  for (const [mode, pattern] of [
    ['missing', /got \[read, glob, run_program, bash\]/],
    ['extra', /exactly \[read, grep, glob, run_program, bash\].*invoke/],
    ['duplicate', /more than once/],
    ['unsupported', /outside the supported subset/],
    ['task-required', /task-based execution/],
  ] as const) {
    it(`fails loud on startup when the catalog is ${mode}`, async () => {
      const fixture = await setupFixture({ FIXTURE_MODE: mode })
      const session = startSession(fixture)
      await expect(session.ready).rejects.toThrow(pattern)
    })
  }

  it('launches the fixed server argv with the scrubbed env plus config env', async () => {
    const savedSecret = process.env.FIXTURE_SECRET_TOKEN
    const savedDsh = process.env.DSH_TEST_SENTINEL
    process.env.FIXTURE_SECRET_TOKEN = 'leak'
    process.env.DSH_TEST_SENTINEL = 'leak'
    const fixture = await setupFixture()
    const session = startSession(fixture, { FIXTURE_SENTINEL_VIA_CONFIG: '1' })
    if (savedSecret === undefined) delete process.env.FIXTURE_SECRET_TOKEN
    else process.env.FIXTURE_SECRET_TOKEN = savedSecret
    if (savedDsh === undefined) delete process.env.DSH_TEST_SENTINEL
    else process.env.DSH_TEST_SENTINEL = savedDsh
    await session.ready
    await waitFor(async () => exists(fixture.files.report))
    const report = JSON.parse(await readFile(fixture.files.report, 'utf8')) as { argv: string[]; env: Record<string, string> }
    expect(report.argv.slice(1)).toEqual([fixturePath, 'serve', '--client-profile', 'codex', '--read-scope', 'normal'])
    const names = Object.keys(report.env)
    expect(names).not.toContain('FIXTURE_SECRET_TOKEN')
    expect(names.filter(name => name.toUpperCase().startsWith('DSH_'))).toEqual([])
    expect(names.some(name => name.toUpperCase() === 'PATH')).toBe(true)
    expect(report.env.FIXTURE_REPORT).toBe(fixture.files.report)
    expect(report.env.FIXTURE_SENTINEL_VIA_CONFIG).toBe('1')
  })

  it('applies the 600000ms timeout to initialize, every catalog page, and tools/call', async () => {
    const fixture = await setupFixture({ FIXTURE_PAGINATE: '1' })
    const recorded: Array<{ request: unknown; options: unknown }> = []
    const session = startSession(fixture, {}, () => {
      const client = new Client({ name: 'spy', version: '0.0.1' }, { capabilities: {} })
      const original = client.request.bind(client)
      client.request = ((request: unknown, resultSchema: unknown, options?: RequestOptions) => {
        recorded.push({ request, options })
        return original(request as never, resultSchema as never, options)
      }) as Client['request']
      return client
    })
    await session.ready
    const initialize = recorded.find(entry => (entry.request as { method?: string }).method === 'initialize')
    const catalogPages = recorded.filter(entry => (entry.request as { method?: string }).method === 'tools/list')
    expect(initialize).toBeDefined()
    expect((initialize!.options as { timeout?: number }).timeout).toBe(MIN_TOOL_CALL_TIMEOUT_MS)
    expect(catalogPages).toHaveLength(2)
    expect(catalogPages.every(entry => (entry.options as { timeout?: number }).timeout === MIN_TOOL_CALL_TIMEOUT_MS)).toBe(true)
    const controller = new AbortController()
    const result = await session.call('bash', { command: 'true' }, controller.signal)
    const call = recorded.find(entry => (entry.request as { method?: string }).method === 'tools/call')
    expect(call).toBeDefined()
    expect((call!.request as { params: { name: string } }).params.name).toBe('bash')
    expect((call!.options as { timeout?: number }).timeout).toBe(MIN_TOOL_CALL_TIMEOUT_MS)
    expect((call!.options as { signal?: AbortSignal }).signal).toBeInstanceOf(AbortSignal)
    const text = (result.content as Array<{ type: string; text?: string }>)[0]?.text
    expect(JSON.parse(text ?? '')).toEqual({ name: 'bash', arguments: { command: 'true' } })
  })

  it('maps caller cancellation to a TOOL_ABORTED HarnessError', async () => {
    const fixture = await setupFixture({ FIXTURE_CALL_DELAY_MS: '8000' })
    const session = startSession(fixture)
    await session.ready
    const controller = new AbortController()
    setTimeout(() => controller.abort(), 150)
    const error = await session.call('bash', { command: 'sleep 5' }, controller.signal).then(
      () => { throw new Error('expected the call to reject') },
      (reason: unknown) => reason,
    )
    expect(error).toBeInstanceOf(HarnessError)
    expect(error).toMatchObject({ name: 'AbortError', code: TOOL_ABORTED })
  })

  it('marks the generation down after a crash, does not auto-restart, and reconnects on the next call', async () => {
    const fixture = await setupFixture()
    const warnings: string[] = []
    const session = startSession(fixture, {}, undefined, { ...noopLogger, warn: (message: string): void => { warnings.push(message) } })
    await session.ready
    await session.call('read', { path: 'x' }, new AbortController().signal)
    expect(await readFile(fixture.files.boot, 'utf8')).toBe('1')
    await writeFile(fixture.files.crash, 'die')
    await waitFor(async () => exists(fixture.files.exit))
    await waitFor(() => Promise.resolve(warnings.length === 1))
    expect(await readFile(fixture.files.boot, 'utf8')).toBe('1')
    const result = await session.call('grep', { pattern: 'x' }, new AbortController().signal)
    expect(await readFile(fixture.files.boot, 'utf8')).toBe('2')
    const text = (result.content as Array<{ type: string; text?: string }>)[0]?.text
    expect(JSON.parse(text ?? '')).toMatchObject({ name: 'grep' })
    await sleep(100)
    const stable = await session.call('glob', { pattern: 'x' }, new AbortController().signal)
    expect(await readFile(fixture.files.boot, 'utf8')).toBe('2')
    const stableText = (stable.content as Array<{ type: string; text?: string }>)[0]?.text
    expect(JSON.parse(stableText ?? '')).toMatchObject({ name: 'glob' })
  })

  it('fails reconnects with AGENTSHIM_CATALOG_CHANGED when the fingerprint drifts', async () => {
    const fixture = await setupFixture({ FIXTURE_MODE: 'drift' })
    const warnings: string[] = []
    const session = startSession(fixture, {}, undefined, { ...noopLogger, warn: (message: string): void => { warnings.push(message) } })
    await session.ready
    await writeFile(fixture.files.crash, 'die')
    await waitFor(async () => exists(fixture.files.exit))
    await waitFor(() => Promise.resolve(warnings.length === 1))
    const error = await session.call('read', { path: 'x' }, new AbortController().signal).then(
      () => { throw new Error('expected the call to reject') },
      (reason: unknown) => reason,
    )
    expect(error).toMatchObject({ code: 'AGENTSHIM_CATALOG_CHANGED' })
    expect(await readFile(fixture.files.boot, 'utf8')).toBe('2')
  })

  it('tears down the child process and rejects calls after dispose', async () => {
    const fixture = await setupFixture()
    const session = startSession(fixture)
    await session.ready
    await session.dispose()
    await expect(session.call('read', { path: 'x' }, new AbortController().signal)).rejects.toThrow(/disposed/)
    await waitFor(async () => exists(fixture.files.exit))
  })

  it('closes a connecting generation without waiting for the startup timeout', async () => {
    const fixture = await setupFixture({ FIXTURE_CONNECT_DELAY_MS: String(MIN_TOOL_CALL_TIMEOUT_MS) })
    const session = startSession(fixture)
    const ready = session.ready.then(
      () => { throw new Error('expected startup to be interrupted') },
      reason => reason,
    )
    await waitFor(async () => exists(fixture.files.report))
    const started = Date.now()
    await session.dispose()
    expect(Date.now() - started).toBeLessThan(10_000)
    expect(await ready).toBeInstanceOf(Error)
  }, 15_000)
})
