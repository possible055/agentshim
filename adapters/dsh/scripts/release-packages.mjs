import { createHash } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import { readFile, readdir, writeFile } from 'node:fs/promises'
import { basename, join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const ENTRY_NAME = 'dsh-agentshim'
const PLATFORM_SPECS = {
  'dsh-agentshim-darwin-arm64': { os: ['darwin'], cpu: ['arm64'] },
  'dsh-agentshim-linux-arm64-gnu': { os: ['linux'], cpu: ['arm64'], libc: ['glibc'] },
  'dsh-agentshim-linux-x64-gnu': { os: ['linux'], cpu: ['x64'], libc: ['glibc'] },
  'dsh-agentshim-win32-x64-msvc': { os: ['win32'], cpu: ['x64'] },
}
const PLATFORM_NAMES = Object.keys(PLATFORM_SPECS)

function invariant(condition, message) {
  if (!condition) throw new Error(message)
}

function sameStrings(actual, expected) {
  return Array.isArray(actual)
    && actual.length === expected.length
    && [...actual].sort().every((value, index) => value === [...expected].sort()[index])
}

function packageFilename(name, version) {
  return `${name}-${version}.tgz`
}

function tar(archive, operation, members = []) {
  const result = spawnSync('tar', [operation, archive, ...members], { encoding: 'utf8' })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) {
    throw new Error(`tar ${operation} ${archive} ${members.join(' ')} failed: ${result.stderr || result.stdout}`)
  }
  return result.stdout
}

function readPackedManifest(archive) {
  return JSON.parse(tar(archive, '-xOzf', ['package/package.json']))
}

function packedFiles(archive) {
  return tar(archive, '-tzf')
    .split(/\r?\n/)
    .filter(path => path.length > 0 && !path.endsWith('/'))
    .sort()
}

async function sha256(path) {
  return createHash('sha256').update(await readFile(path)).digest('hex')
}

export function validatePackageManifests(manifests, { version, nativeApi }) {
  const names = Object.keys(manifests).sort()
  const expectedNames = [ENTRY_NAME, ...PLATFORM_NAMES].sort()
  invariant(sameStrings(names, expectedNames), `package names do not match release set: ${names.join(', ')}`)

  const entry = manifests[ENTRY_NAME]
  invariant(entry.version === version, `${ENTRY_NAME} version ${String(entry.version)} does not match ${version}`)
  invariant(entry.agentshimNativeApi === nativeApi, `${ENTRY_NAME} native API does not match ${nativeApi}`)
  invariant(entry.main === 'lib/index.js', `${ENTRY_NAME} main must be lib/index.js`)
  invariant(entry.exports?.['.']?.default === './lib/index.js', `${ENTRY_NAME} default export must be ./lib/index.js`)

  const optionalNames = Object.keys(entry.optionalDependencies ?? {}).filter(name => name.startsWith('dsh-agentshim-'))
  invariant(sameStrings(optionalNames, PLATFORM_NAMES), 'entry optional platform dependency set is incomplete')
  for (const name of PLATFORM_NAMES) {
    invariant(
      entry.optionalDependencies[name] === version,
      `${ENTRY_NAME} optional dependency ${name} must exact-pin ${version}, got ${String(entry.optionalDependencies[name])}`,
    )
  }

  for (const [name, expected] of Object.entries(PLATFORM_SPECS)) {
    const manifest = manifests[name]
    invariant(manifest.version === version, `${name} version ${String(manifest.version)} does not match ${version}`)
    invariant(manifest.agentshimNativeApi === nativeApi, `${name} native API does not match ${nativeApi}`)
    invariant(manifest.main === 'agentshim_napi.node', `${name} main must be agentshim_napi.node`)
    invariant(sameStrings(manifest.files, ['LICENSE', 'agentshim_napi.node']), `${name} files must contain only LICENSE and agentshim_napi.node`)
    invariant(sameStrings(manifest.os, expected.os), `${name} os does not match its package name`)
    invariant(sameStrings(manifest.cpu, expected.cpu), `${name} cpu does not match its package name`)
    if (expected.libc === undefined) {
      invariant(manifest.libc === undefined, `${name} must not declare libc`)
    } else {
      invariant(sameStrings(manifest.libc, expected.libc), `${name} libc does not match its package name`)
    }
  }
}

export function validateProvenanceRecord(record, expected) {
  invariant(record.schemaVersion === 1, `${expected.archive} provenance schema is unsupported`)
  invariant(record.commit === expected.commit, `${expected.archive} provenance commit does not match`)
  invariant(record.archive === expected.archive, `${expected.archive} provenance archive name does not match`)
  invariant(record.sha256 === expected.sha256, `${expected.archive} provenance digest does not match`)
  invariant(record.package?.name === expected.manifest.name, `${expected.archive} provenance package name does not match`)
  invariant(record.package?.version === expected.manifest.version, `${expected.archive} provenance version does not match`)
  invariant(
    record.package?.agentshimNativeApi === expected.manifest.agentshimNativeApi,
    `${expected.archive} provenance native API does not match`,
  )
}

export async function recordPackage({ archive, commit, output }) {
  invariant(/^[0-9a-f]{40}$/i.test(commit), 'provenance commit must be a full hexadecimal SHA')
  const absoluteArchive = resolve(archive)
  const manifest = readPackedManifest(absoluteArchive)
  const record = {
    schemaVersion: 1,
    commit: commit.toLowerCase(),
    archive: basename(absoluteArchive),
    sha256: await sha256(absoluteArchive),
    package: {
      name: manifest.name,
      version: manifest.version,
      agentshimNativeApi: manifest.agentshimNativeApi,
    },
  }
  const target = resolve(output ?? `${absoluteArchive}.provenance.json`)
  await writeFile(target, `${JSON.stringify(record, null, 2)}\n`)
  return target
}

export async function verifyPackageDirectory({ directory, version, commit, nativeApi }) {
  invariant(/^[0-9a-f]{40}$/i.test(commit), 'expected commit must be a full hexadecimal SHA')
  const expectedArchives = [ENTRY_NAME, ...PLATFORM_NAMES]
    .map(name => packageFilename(name, version))
    .sort()
  const entries = await readdir(directory)
  const archives = entries.filter(name => name.endsWith('.tgz')).sort()
  invariant(sameStrings(archives, expectedArchives), `release tarballs do not match expected set: ${archives.join(', ')}`)
  const provenanceFiles = entries.filter(name => name.endsWith('.tgz.provenance.json')).sort()
  invariant(
    sameStrings(provenanceFiles, expectedArchives.map(name => `${name}.provenance.json`)),
    'release provenance sidecars do not match the tarball set',
  )

  const manifests = {}
  const filesByPackage = {}
  for (const archiveName of archives) {
    const archive = join(directory, archiveName)
    const manifest = readPackedManifest(archive)
    manifests[manifest.name] = manifest
    filesByPackage[manifest.name] = packedFiles(archive)
    const digest = await sha256(archive)
    const record = JSON.parse(await readFile(`${archive}.provenance.json`, 'utf8'))
    validateProvenanceRecord(record, {
      archive: archiveName,
      commit: commit.toLowerCase(),
      sha256: digest,
      manifest,
    })
  }
  validatePackageManifests(manifests, { version, nativeApi })

  const entryFiles = filesByPackage[ENTRY_NAME]
  for (const required of ['package/README.md', 'package/cordis.patch.yml', 'package/lib/index.d.ts', 'package/lib/index.js', 'package/package.json']) {
    invariant(entryFiles.includes(required), `${ENTRY_NAME} tarball is missing ${required}`)
  }
  invariant(
    entryFiles.every(path => !/^package\/(?:scripts|src|tests)\//.test(path)),
    `${ENTRY_NAME} tarball contains development-only source`,
  )
  for (const name of PLATFORM_NAMES) {
    invariant(
      sameStrings(filesByPackage[name], ['package/LICENSE', 'package/agentshim_napi.node', 'package/package.json']),
      `${name} tarball payload is not minimal`,
    )
  }
}

function options(argv) {
  invariant(argv.length % 2 === 0, 'arguments must be --name value pairs')
  const parsed = {}
  for (let index = 0; index < argv.length; index += 2) {
    const key = argv[index]
    invariant(key.startsWith('--'), `unexpected argument ${key}`)
    parsed[key.slice(2)] = argv[index + 1]
  }
  return parsed
}

async function main() {
  const [command, ...argv] = process.argv.slice(2)
  const parsed = options(argv)
  if (command === 'record') {
    invariant(parsed.archive !== undefined && parsed.commit !== undefined, 'record requires --archive and --commit')
    await recordPackage({ archive: parsed.archive, commit: parsed.commit, output: parsed.output })
    return
  }
  if (command === 'verify') {
    invariant(
      parsed.directory !== undefined && parsed.version !== undefined && parsed.commit !== undefined && parsed['native-api'] !== undefined,
      'verify requires --directory, --version, --commit, and --native-api',
    )
    await verifyPackageDirectory({
      directory: resolve(parsed.directory),
      version: parsed.version,
      commit: parsed.commit,
      nativeApi: Number(parsed['native-api']),
    })
    return
  }
  throw new Error('usage: release-packages.mjs <record|verify> [options]')
}

if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  await main()
}
