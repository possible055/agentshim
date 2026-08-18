import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import {
  recordPackage,
  validatePackageManifests,
  validateProvenanceRecord,
  verifyPackageDirectory,
} from './release-packages.mjs'

const version = '1.2.3'
const nativeApi = 4
const platformNames = [
  'dsh-agentshim-darwin-arm64',
  'dsh-agentshim-linux-arm64-gnu',
  'dsh-agentshim-linux-x64-gnu',
  'dsh-agentshim-win32-x64-msvc',
]

function manifests() {
  return {
    'dsh-agentshim': {
      name: 'dsh-agentshim',
      version,
      agentshimNativeApi: nativeApi,
      main: 'lib/index.js',
      exports: { '.': { default: './lib/index.js' } },
      optionalDependencies: Object.fromEntries(platformNames.map(name => [name, version])),
    },
    'dsh-agentshim-darwin-arm64': platform('dsh-agentshim-darwin-arm64', ['darwin'], ['arm64']),
    'dsh-agentshim-linux-arm64-gnu': platform('dsh-agentshim-linux-arm64-gnu', ['linux'], ['arm64'], ['glibc']),
    'dsh-agentshim-linux-x64-gnu': platform('dsh-agentshim-linux-x64-gnu', ['linux'], ['x64'], ['glibc']),
    'dsh-agentshim-win32-x64-msvc': platform('dsh-agentshim-win32-x64-msvc', ['win32'], ['x64']),
  }
}

function platform(name, os, cpu, libc) {
  return {
    name,
    version,
    agentshimNativeApi: nativeApi,
    main: 'agentshim_napi.node',
    files: ['agentshim_napi.node', 'LICENSE'],
    os,
    cpu,
    ...(libc === undefined ? {} : { libc }),
  }
}

function rejects(change, pattern) {
  const candidate = manifests()
  change(candidate)
  assert.throws(() => validatePackageManifests(candidate, { version, nativeApi }), pattern)
}

validatePackageManifests(manifests(), { version, nativeApi })
rejects(candidate => { candidate['dsh-agentshim-linux-x64-gnu'].version = '1.2.4' }, /version/)
rejects(candidate => { candidate['dsh-agentshim-win32-x64-msvc'].agentshimNativeApi = 3 }, /native API/)
rejects(candidate => { candidate['dsh-agentshim'].optionalDependencies['dsh-agentshim-linux-x64-gnu'] = '^1.2.3' }, /exact-pin/)

const manifest = manifests()['dsh-agentshim']
const expected = {
  archive: 'dsh-agentshim-1.2.3.tgz',
  commit: 'a'.repeat(40),
  sha256: 'b'.repeat(64),
  manifest,
}
const provenance = {
  schemaVersion: 1,
  commit: expected.commit,
  archive: expected.archive,
  sha256: expected.sha256,
  package: {
    name: manifest.name,
    version: manifest.version,
    agentshimNativeApi: manifest.agentshimNativeApi,
  },
}
validateProvenanceRecord(provenance, expected)
assert.throws(
  () => validateProvenanceRecord({ ...provenance, sha256: 'c'.repeat(64) }, expected),
  /digest/,
)
assert.throws(
  () => validateProvenanceRecord({ ...provenance, commit: 'd'.repeat(40) }, expected),
  /commit/,
)

function run(program, args, cwd) {
  const result = spawnSync(program, args, {
    cwd,
    encoding: 'utf8',
    stdio: 'inherit',
    shell: process.platform === 'win32',
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) throw new Error(`${program} ${args.join(' ')} exited with ${String(result.status)}`)
}

const adapterRoot = fileURLToPath(new URL('../', import.meta.url))
const repositoryRoot = resolve(adapterRoot, '..', '..')
const sourceEntry = JSON.parse(await readFile(join(adapterRoot, 'package.json'), 'utf8'))
const actualCommit = 'e'.repeat(40)
const temporaryRoot = await mkdtemp(join(tmpdir(), 'dsh-agentshim-release-packages-'))
try {
  const dist = join(temporaryRoot, 'dist')
  await mkdir(dist)
  run('pnpm', ['--config.ignore-scripts=true', 'pack', '--pack-destination', dist], adapterRoot)
  const entryArchive = join(dist, `${sourceEntry.name}-${sourceEntry.version}.tgz`)
  await recordPackage({ archive: entryArchive, commit: actualCommit })

  for (const name of platformNames) {
    const platform = name.slice('dsh-agentshim-'.length)
    const stage = join(temporaryRoot, platform)
    await mkdir(stage)
    await copyFile(join(adapterRoot, 'npm', platform, 'package.json'), join(stage, 'package.json'))
    await copyFile(join(repositoryRoot, 'LICENSE'), join(stage, 'LICENSE'))
    await writeFile(join(stage, 'agentshim_napi.node'), 'release verifier fixture')
    run('pnpm', ['pack', '--pack-destination', dist], stage)
    await recordPackage({
      archive: join(dist, `${name}-${sourceEntry.version}.tgz`),
      commit: actualCommit,
    })
  }

  await verifyPackageDirectory({
    directory: dist,
    version: sourceEntry.version,
    commit: actualCommit,
    nativeApi: sourceEntry.agentshimNativeApi,
  })
} finally {
  await rm(temporaryRoot, { recursive: true, force: true })
}
