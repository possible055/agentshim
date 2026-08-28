import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { copyFile, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const adapterRoot = fileURLToPath(new URL('../', import.meta.url))
const repositoryRoot = resolve(adapterRoot, '..', '..')
const nativeLibrary = join(
  repositoryRoot,
  'target',
  'debug',
  process.platform === 'win32'
    ? 'agentshim_napi.dll'
    : process.platform === 'darwin'
      ? 'libagentshim_napi.dylib'
      : 'libagentshim_napi.so',
)
const platformByRuntime = {
  'darwin-arm64': 'darwin-arm64',
  'linux-arm64': 'linux-arm64-gnu',
  'linux-x64': 'linux-x64-gnu',
  'win32-x64': 'win32-x64-msvc',
}
const platform = platformByRuntime[`${process.platform}-${process.arch}`]
if (platform === undefined) throw new Error(`packed smoke does not support ${process.platform}-${process.arch}`)
const entryManifest = JSON.parse(await readFile(join(adapterRoot, 'package.json'), 'utf8'))
const platformManifest = JSON.parse(await readFile(join(adapterRoot, 'npm', platform, 'package.json'), 'utf8'))

function run(program, args, options = {}) {
  const result = spawnSync(program, args, {
    encoding: 'utf8',
    stdio: 'inherit',
    shell: process.platform === 'win32',
    ...options,
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) throw new Error(`${program} ${args.join(' ')} exited with ${String(result.status)}`)
}

const temporaryRoot = await mkdtemp(join(tmpdir(), 'dsh-agentshim-packed-'))
try {
  const packDirectory = join(temporaryRoot, 'pack')
  const consumer = join(temporaryRoot, 'consumer')
  const platformStage = join(temporaryRoot, 'platform')
  await mkdir(packDirectory)
  await mkdir(consumer)
  await mkdir(platformStage)
  await copyFile(join(adapterRoot, 'npm', platform, 'package.json'), join(platformStage, 'package.json'))
  await copyFile(join(repositoryRoot, 'LICENSE'), join(platformStage, 'LICENSE'))
  await copyFile(nativeLibrary, join(platformStage, 'agentshim_napi.node'))

  run('pnpm', ['--config.ignore-scripts=true', 'pack', '--pack-destination', packDirectory], {
    cwd: adapterRoot,
  })
  run('pnpm', ['pack', '--pack-destination', packDirectory], { cwd: platformStage })
  const archives = (await readdir(packDirectory)).filter(name => name.endsWith('.tgz'))
  assert.equal(archives.length, 2, 'packed smoke requires one fresh entry and one fresh platform tarball')
  const entryArchiveName = `${entryManifest.name}-${entryManifest.version}.tgz`
  const platformArchiveName = `${platformManifest.name}-${platformManifest.version}.tgz`
  assert(archives.includes(entryArchiveName), `missing fresh entry tarball ${entryArchiveName}`)
  assert(archives.includes(platformArchiveName), `missing fresh platform tarball ${platformArchiveName}`)
  const entryArchive = join(packDirectory, entryArchiveName)
  const platformArchive = join(packDirectory, platformArchiveName)

  await writeFile(join(consumer, 'package.json'), `${JSON.stringify({
    private: true,
    type: 'module',
    dependencies: {
      '@deepseek-ai/cordis': '4.0.1',
      '@deepseek-ai/dsh-attachment': '0.1.0-rc.7',
      '@deepseek-ai/dsh-fs': '0.1.0-rc.7',
      '@deepseek-ai/dsh-fs-local': '0.1.0-rc.7',
      '@deepseek-ai/dsh-jobs': '0.1.0-rc.7',
      '@deepseek-ai/dsh-llm': '0.1.0-rc.7',
      '@deepseek-ai/dsh-sandbox': '0.1.0-rc.7',
      '@deepseek-ai/dsh-scope': '0.1.0-rc.7',
      '@deepseek-ai/dsh-shell': '0.1.0-rc.7',
      '@deepseek-ai/dsh-subprocess': '0.1.0-rc.7',
      '@deepseek-ai/dsh-system-prompt': '0.1.0-rc.7',
      '@deepseek-ai/dsh-tools': '0.1.0-rc.7',
      'dsh-agentshim': `file:${entryArchive.replaceAll('\\', '/')}`,
      [platformManifest.name]: `file:${platformArchive.replaceAll('\\', '/')}`,
    },
  }, null, 2)}\n`)
  run('pnpm', ['install', '--prefer-offline', '--ignore-scripts', '--no-optional', '--no-frozen-lockfile'], { cwd: consumer })

  const fixtureRoot = join(temporaryRoot, 'fixture')
  await mkdir(fixtureRoot)
  await writeFile(join(fixtureRoot, 'notes.txt'), 'packed native read\n')
  await writeFile(join(consumer, 'smoke.mjs'), `
import assert from 'node:assert/strict'
import { Context } from '@deepseek-ai/cordis'
import * as llm from '@deepseek-ai/dsh-llm'
const createCallId = llm.ToolCallId ?? llm.CallId ?? (id => id)
import LocalFileSystem from '@deepseek-ai/dsh-fs-local'
import { bindScopeParent, createScope } from '@deepseek-ai/dsh-scope'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ToolRuntime from '@deepseek-ai/dsh-tools'
import * as agentshim from 'dsh-agentshim'

const root = process.env.AGENTSHIM_PACKED_FIXTURE
const resolvedEntry = import.meta.resolve('dsh-agentshim')
assert.match(resolvedEntry, /node_modules\\/dsh-agentshim\\/lib\\/index\\.js$/)
const resolvedPlatform = import.meta.resolve(process.env.AGENTSHIM_PACKED_PLATFORM)
assert.match(resolvedPlatform, /node_modules\\/${platformManifest.name}\\/agentshim_napi\\.node$/)

const ctx = new Context()
await ctx.plugin(SystemPrompt, {})
await ctx.plugin(ToolRuntime)
await ctx.plugin(LocalFileSystem, { cwd: root })

const inherited = {
  name: 'read',
  description: 'inherited read',
  parameters: { type: 'object', properties: {} },
  output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
  execute: () => Promise.resolve('inherited'),
}
const agent = {
  id: 'packed-preset-agent',
  session: { header: { cwd: root }, requestHeader: () => ({ config: {} }) },
  options: { provider: 'stub', model: 'stub' },
}
const presetKey = { preset: 'packed' }
await ctx.plugin(Object.assign(inner => {
  const standing = createScope(inner, presetKey)
  standing.ctx.tools.register(inherited)
  const scope = createScope(inner, agent)
  bindScopeParent(agent, presetKey)
  agent.ctx = scope.ctx
}, { inject: ['tools', 'systemPrompt'] }))

const plugin = await ctx.plugin(agentshim, {
  root,
  env: {},
  toolCallTimeoutMs: 600_000,
  captureRoot: process.env.AGENTSHIM_PACKED_CAPTURE,
})
ctx.emit('agent/created', { agent })

const replacement = agent.ctx.tools.get('read', agent)
assert.notEqual(replacement?.description, 'inherited read')
assert.match(replacement?.description ?? '', /numbered lines/)
const result = await ctx.tools.execute({
  signal: new AbortController().signal,
  callId: createCallId('packed-read'),
  name: 'read',
  arguments: { path: 'notes.txt' },
  agent,
})
assert.equal(result.isError, false)
assert.match(result.content[0]?.text ?? '', /packed native read/)
await plugin.dispose()
`)

  const smokeEnv = {
    ...process.env,
    AGENTSHIM_PACKED_CAPTURE: join(temporaryRoot, 'captures'),
    AGENTSHIM_PACKED_FIXTURE: fixtureRoot,
    AGENTSHIM_PACKED_PLATFORM: platformManifest.name,
  }
  delete smokeEnv.AGENTSHIM_DSH_NATIVE_DLL
  run(process.execPath, [join(consumer, 'smoke.mjs')], {
    cwd: consumer,
    env: smokeEnv,
    shell: false,
  })
} finally {
  await rm(temporaryRoot, { recursive: true, force: true })
}
