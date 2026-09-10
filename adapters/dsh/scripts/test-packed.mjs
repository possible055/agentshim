import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { copyFile, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const TARGET_DSH_VERSION = '0.1.5-rc.1'
const PUBLISHED_DSH_VERSIONS = [
  '0.1.0-rc.6',
  '0.1.0-rc.7',
  '0.1.0-rc.8',
  '0.1.1-rc.1',
  '0.1.1-rc.2',
  '0.1.2-alpha.2',
  '0.1.2-alpha.3',
  '0.1.2-alpha.4',
  '0.1.2-alpha.5',
  '0.1.2-rc.1',
  '0.1.3-alpha.2',
  '0.1.5-alpha.1',
  '0.1.5-alpha.2',
  TARGET_DSH_VERSION,
]
const UNAVAILABLE_DSH_VERSIONS = ['0.1.2-alpha.1', '0.1.3-alpha.1']
const DSH_FAMILY_PACKAGES = [
  '@deepseek-ai/dsh-agent',
  '@deepseek-ai/dsh-attachment-local',
  '@deepseek-ai/dsh-attachment',
  '@deepseek-ai/dsh-brand',
  '@deepseek-ai/dsh-code-runtime-worker-thread',
  '@deepseek-ai/dsh-code-runtime',
  '@deepseek-ai/dsh-fs-local',
  '@deepseek-ai/dsh-fs-observation-policy',
  '@deepseek-ai/dsh-fs',
  '@deepseek-ai/dsh-home-paths',
  '@deepseek-ai/dsh-invariants',
  '@deepseek-ai/dsh-jobs-local',
  '@deepseek-ai/dsh-jobs',
  '@deepseek-ai/dsh-llm',
  '@deepseek-ai/dsh-output-retention',
  '@deepseek-ai/dsh-sandbox',
  '@deepseek-ai/dsh-scope',
  '@deepseek-ai/dsh-session',
  '@deepseek-ai/dsh-session-projection',
  '@deepseek-ai/dsh-settings',
  '@deepseek-ai/dsh-shell',
  '@deepseek-ai/dsh-subprocess',
  '@deepseek-ai/dsh-system-prompt',
  '@deepseek-ai/dsh-timeout',
  '@deepseek-ai/dsh-tool-call-timeout-policy',
  '@deepseek-ai/dsh-tool-jobs',
  '@deepseek-ai/dsh-tools',
  '@deepseek-ai/dsh-typert-protocol',
  '@deepseek-ai/dsh-user-approval',
]
const MODERN_DSH_PACKAGES = [
  '@deepseek-ai/dsh-util-crypto',
  '@deepseek-ai/dsh-util-values',
]
const HTTP_PROXY_PACKAGE = '@deepseek-ai/dsh-http-proxy'

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

const corepack = process.platform === 'win32' ? 'corepack.cmd' : 'corepack'

function runPnpm(args, options = {}) {
  run(corepack, ['pnpm@11.21.0', ...args], options)
}

function cordisVersionFor(version) {
  return version.startsWith('0.1.0-') || version.startsWith('0.1.1-') ? '4.0.1' : '4.0.2'
}

function packagesFor(version) {
  const packages = [...DSH_FAMILY_PACKAGES]
  if (!version.startsWith('0.1.0-') && !version.startsWith('0.1.1-')) packages.push(...MODERN_DSH_PACKAGES)
  if (version === '0.1.3-alpha.2' || version.startsWith('0.1.5-')) packages.push(HTTP_PROXY_PACKAGE)
  return packages
}

function localArchive(path) {
  return `file:${path.replaceAll('\\', '/')}`
}

async function runPackedSmoke({ entryArchive, platformArchive, platformName, version, temporaryRoot }) {
  const consumer = join(temporaryRoot, `consumer-${version}`)
  const fixtureRoot = join(temporaryRoot, `fixture-${version}`)
  await mkdir(consumer)
  await mkdir(fixtureRoot)
  await writeFile(join(fixtureRoot, 'notes.txt'), `packed native read ${version}\n`)

  const dependencies = {
    '@deepseek-ai/cordis': cordisVersionFor(version),
    ...Object.fromEntries(packagesFor(version).map(name => [name, version])),
    'dsh-agentshim': localArchive(entryArchive),
    [platformName]: localArchive(platformArchive),
  }
  await writeFile(join(consumer, 'package.json'), `${JSON.stringify({
    private: true,
    type: 'module',
    dependencies,
  }, null, 2)}\n`)
  runPnpm([
    'install',
    '--prefer-offline',
    '--ignore-scripts',
    '--no-optional',
    '--no-frozen-lockfile',
    '--strict-peer-dependencies',
  ], { cwd: consumer })

  const expectedModern = !version.startsWith('0.1.0-') && !version.startsWith('0.1.1-')
  const expectedVersion = JSON.stringify(version)
  const expectedModernLiteral = JSON.stringify(expectedModern)
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

const version = ${expectedVersion}
const expectedModern = ${expectedModernLiteral}
const root = process.env.AGENTSHIM_PACKED_FIXTURE
const resolvedEntry = import.meta.resolve('dsh-agentshim')
assert.match(resolvedEntry, /node_modules\\/dsh-agentshim\\/lib\\/index\\.js$/)
const resolvedPlatform = import.meta.resolve(process.env.AGENTSHIM_PACKED_PLATFORM)
assert.match(resolvedPlatform, /node_modules\\/${platformManifest.name}\\/agentshim_napi\\.node$/)

const noPromptCtx = new Context()
await noPromptCtx.plugin(LocalFileSystem, { cwd: root })
const noPromptPlugin = await noPromptCtx.plugin(agentshim, {
  root,
  env: {},
  toolCallTimeoutMs: 600_000,
  captureRoot: process.env.AGENTSHIM_PACKED_CAPTURE,
})
await noPromptPlugin.dispose()

const ctx = new Context()
await ctx.plugin(SystemPrompt, {})
const systemPrompt = ctx.get('systemPrompt')
assert(systemPrompt !== undefined)
if (expectedModern) {
  assert.equal(typeof systemPrompt.getSectionOrder, 'function')
  assert.equal(typeof systemPrompt.getSectionOrder('TOOL_BASH'), 'number')
  assert.equal(typeof systemPrompt.getSectionOrder('TOOL_READ'), 'number')
  assert.equal(systemPrompt.getSectionOrder('TOOL_BASH'), 1000)
  assert.equal(systemPrompt.getSectionOrder('TOOL_READ'), 1100)
}
await ctx.plugin(ToolRuntime)
await ctx.plugin(LocalFileSystem, { cwd: root })

const inherited = {
  name: 'read',
  description: 'inherited read',
  parameters: { type: 'object', properties: {} },
  output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
  execute: () => Promise.resolve('inherited'),
}
const inheritedBash = {
  ...inherited,
  name: 'bash',
  description: 'inherited bash',
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
  standing.ctx.tools.register(inheritedBash)
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
const bashReplacement = agent.ctx.tools.get('bash', agent)
assert.notEqual(bashReplacement?.description, 'inherited bash')
const sectionNames = (await systemPrompt.assemble({ scope: agent })).sections.map(section => section.name)
const bashSection = sectionNames.indexOf('tool:bash')
const readSection = sectionNames.indexOf('tool:read')
assert.notEqual(bashSection, -1)
assert.notEqual(readSection, -1)
assert(expectedModern ? bashSection < readSection : readSection < bashSection)
const result = await ctx.tools.execute({
  signal: new AbortController().signal,
  callId: createCallId('packed-read'),
  name: 'read',
  arguments: { path: 'notes.txt' },
  agent,
})
assert.equal(result.isError, false)
assert((result.content[0]?.text ?? '').includes('packed native read ' + version))
await plugin.dispose()
`)

  const smokeEnv = {
    ...process.env,
    AGENTSHIM_PACKED_CAPTURE: join(temporaryRoot, `captures-${version}`),
    AGENTSHIM_PACKED_FIXTURE: fixtureRoot,
    AGENTSHIM_PACKED_PLATFORM: platformManifest.name,
  }
  delete smokeEnv.AGENTSHIM_DSH_NATIVE_DLL
  run(process.execPath, [join(consumer, 'smoke.mjs')], {
    cwd: consumer,
    env: smokeEnv,
    shell: false,
  })
}

const temporaryRoot = await mkdtemp(join(tmpdir(), 'dsh-agentshim-packed-'))
try {
  const packDirectory = join(temporaryRoot, 'pack')
  const platformStage = join(temporaryRoot, 'platform')
  await mkdir(packDirectory)
  await mkdir(platformStage)
  await copyFile(join(adapterRoot, 'npm', platform, 'package.json'), join(platformStage, 'package.json'))
  await copyFile(join(repositoryRoot, 'LICENSE'), join(platformStage, 'LICENSE'))
  await copyFile(nativeLibrary, join(platformStage, 'agentshim_napi.node'))

  runPnpm(['--config.ignore-scripts=true', 'pack', '--pack-destination', packDirectory], {
    cwd: adapterRoot,
  })
  runPnpm(['pack', '--pack-destination', packDirectory], { cwd: platformStage })
  const archives = (await readdir(packDirectory)).filter(name => name.endsWith('.tgz'))
  assert.equal(archives.length, 2, 'packed smoke requires one fresh entry and one fresh platform tarball')
  const entryArchiveName = `${entryManifest.name}-${entryManifest.version}.tgz`
  const platformArchiveName = `${platformManifest.name}-${platformManifest.version}.tgz`
  assert(archives.includes(entryArchiveName), `missing fresh entry tarball ${entryArchiveName}`)
  assert(archives.includes(platformArchiveName), `missing fresh platform tarball ${platformArchiveName}`)
  const entryArchive = join(packDirectory, entryArchiveName)
  const platformArchive = join(packDirectory, platformArchiveName)

  const matrix = process.argv.includes('--matrix')
  const versions = matrix ? PUBLISHED_DSH_VERSIONS : [TARGET_DSH_VERSION]
  for (const version of versions) {
    console.log(`packed smoke: DSH ${version}`)
    await runPackedSmoke({
      entryArchive,
      platformArchive,
      platformName: platformManifest.name,
      version,
      temporaryRoot,
    })
  }
  if (matrix) {
    for (const version of UNAVAILABLE_DSH_VERSIONS) {
      console.log(`packed smoke: DSH ${version} unavailable (not published by the registry)`)
    }
  }
} finally {
  await rm(temporaryRoot, { recursive: true, force: true })
}
