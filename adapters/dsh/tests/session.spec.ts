import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { afterEach, describe, expect, it } from 'vitest'
import type { Tool } from '@modelcontextprotocol/sdk/types.js'
import { BRIDGE_VERSION, REQUIRED_BRIDGE_OPERATIONS } from '../src/contracts.ts'
import {
  MIN_TOOL_CALL_TIMEOUT_MS,
  createSession,
  resolveSessionConfig,
  serverArgs,
  validateCatalog,
} from '../src/session.ts'
import type { AgentshimSession, ResolvedSessionConfig } from '../src/session.ts'

const fixturePath = fileURLToPath(new URL('./fixture-server.mjs', import.meta.url))
const roots: string[] = []
const sessions: AgentshimSession[] = []

afterEach(async () => {
  for (const session of sessions.splice(0)) await session.dispose()
  for (const root of roots.splice(0)) await rm(root, { recursive: true, force: true })
})

function tool(name: string, version = BRIDGE_VERSION): Tool {
  return {
    name,
    description: `${name} operation`,
    inputSchema: { type: 'object', properties: {} },
    _meta: { 'agentshim.dshBridge': { version } },
  }
}

async function config(env: Record<string, string> = {}): Promise<ResolvedSessionConfig> {
  const root = await mkdtemp(join(tmpdir(), 'dsh-agentshim-session-'))
  roots.push(root)
  return {
    root,
    command: process.execPath,
    commandArgs: [fixturePath],
    readScope: 'normal',
    env,
    toolCallTimeoutMs: MIN_TOOL_CALL_TIMEOUT_MS,
  }
}

describe('versioned DSH bridge catalog', () => {
  it('accepts unrelated operations and arbitrary order', () => {
    const tools = [tool('unrelated'), ...[...REQUIRED_BRIDGE_OPERATIONS].reverse().map(name => tool(name))]
    expect(validateCatalog(tools)).toEqual({
      operations: REQUIRED_BRIDGE_OPERATIONS,
      bridgeVersion: BRIDGE_VERSION,
    })
  })

  it('fails loud on missing, duplicate, or mismatched operations', () => {
    expect(() => validateCatalog(REQUIRED_BRIDGE_OPERATIONS.slice(1).map(name => tool(name)))).toThrow(/missing required operations/)
    expect(() => validateCatalog([...REQUIRED_BRIDGE_OPERATIONS.map(name => tool(name)), tool('read')])).toThrow(/duplicate operation/)
    expect(() => validateCatalog(REQUIRED_BRIDGE_OPERATIONS.map(name => tool(name, name === 'bash' ? 1 : 2)))).toThrow(/expected agentshim\.dshBridge version 2/)
  })
})

describe('private session', () => {
  it('always launches the dsh profile', async () => {
    const resolved = await config()
    expect(serverArgs(resolved)).toEqual([
      fixturePath,
      'serve',
      '--client-profile',
      'dsh',
      '--read-scope',
      'normal',
    ])
  })

  it('connects, validates the bridge, and returns structured success', async () => {
    const session = createSession(await config(), { logger: { info() {}, warn() {}, error() {} } })
    sessions.push(session)
    await expect(session.ready).resolves.toMatchObject({ bridgeVersion: 2 })
    const result = await session.call('grep', { pattern: 'needle' }, new AbortController().signal)
    expect(result.structuredContent).toEqual({
      bridgeVersion: 2,
      tool: 'grep',
    })
  })

  it('rejects a wrong bridge version at startup', async () => {
    const session = createSession(await config({ FIXTURE_MODE: 'wrong-bridge' }), { logger: { info() {}, warn() {}, error() {} } })
    sessions.push(session)
    await expect(session.ready).rejects.toThrow(/expected agentshim\.dshBridge version 2/)
  })
})

describe('configuration', () => {
  it('canonicalizes an absolute root and rejects relative roots', async () => {
    const root = await mkdtemp(join(tmpdir(), 'dsh-agentshim-resolve-'))
    roots.push(root)
    await expect(resolveSessionConfig({
      root,
      command: process.execPath,
      commandArgs: [],
      readScope: 'normal',
      env: {},
      toolCallTimeoutMs: MIN_TOOL_CALL_TIMEOUT_MS,
    })).resolves.toMatchObject({ command: process.execPath })
    await expect(resolveSessionConfig({
      root: '.',
      command: process.execPath,
      commandArgs: [],
      readScope: 'normal',
      env: {},
      toolCallTimeoutMs: MIN_TOOL_CALL_TIMEOUT_MS,
    })).rejects.toThrow(/root must be absolute/)
  })
})
