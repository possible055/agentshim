#!/usr/bin/env node
import { captureStatus, purgeCaptures, resolveCaptureRoot } from './capture.ts'

function usage(): never {
  throw new Error('usage: dsh-agentshim-captures status | purge --older-than-days N | purge --all')
}

async function main(args: readonly string[]): Promise<void> {
  const root = resolveCaptureRoot('')
  if (args.length === 1 && args[0] === 'status') {
    const status = await captureStatus(root)
    process.stdout.write(`${JSON.stringify(status)}\n`)
    return
  }
  if (args[0] !== 'purge') usage()
  if (args.length === 2 && args[1] === '--all') {
    process.stdout.write(`${JSON.stringify({ root, removedSessions: await purgeCaptures(root, { all: true }) })}\n`)
    return
  }
  if (args.length === 3 && args[1] === '--older-than-days') {
    const days = Number(args[2])
    if (!Number.isSafeInteger(days) || days < 0) usage()
    process.stdout.write(`${JSON.stringify({ root, removedSessions: await purgeCaptures(root, { all: false, olderThanDays: days }) })}\n`)
    return
  }
  usage()
}

main(process.argv.slice(2)).catch(error => {
  process.stderr.write(`dsh-agentshim-captures: ${error instanceof Error ? error.message : String(error)}\n`)
  process.exitCode = 1
})
