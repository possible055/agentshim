import { readdirSync, readFileSync } from 'node:fs'
import { extname, join } from 'node:path'
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('../', import.meta.url))
const needles = [
  '@modelcontext' + 'protocol/sdk',
  'ClientProfile::' + 'Dsh',
  'bridge' + 'Version',
  'addon ' + 'fallback',
]

function checkText(label, text) {
  for (const needle of needles) {
    if (text.includes(needle)) throw new Error(`${label} contains forbidden cutover marker ${needle}`)
  }
}

function scan(directory) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) scan(path)
    if (entry.isFile() && ['.ts', '.js', '.json'].includes(extname(entry.name))) {
      checkText(path, readFileSync(path, 'utf8'))
    }
    if (entry.name === 'session.ts' || entry.name === 'session.js') {
      throw new Error(`${path} must not exist after the native cutover`)
    }
  }
}

scan(join(root, 'src'))
try {
  scan(join(root, 'lib'))
} catch (error) {
  if (error?.code !== 'ENOENT') throw error
}

for (const entry of readdirSync(root)) {
  if (!entry.endsWith('.tgz')) continue
  const archive = join(root, entry)
  const listing = spawnSync('tar', ['-tzf', archive], { encoding: 'utf8' })
  if (listing.status !== 0) throw new Error(`cannot inspect ${entry}: ${listing.stderr}`)
  if (listing.stdout.includes('/session.js') || listing.stdout.includes('/session.ts')) {
    throw new Error(`${entry} contains a removed session module`)
  }
  for (const file of listing.stdout.split(/\r?\n/).filter(path => /package\/(?:lib\/.*\.js|package\.json)$/.test(path))) {
    const extracted = spawnSync('tar', ['-xOzf', archive, file], { encoding: 'utf8' })
    if (extracted.status !== 0) throw new Error(`cannot inspect ${entry}:${file}`)
    checkText(`${entry}:${file}`, extracted.stdout)
  }
}
