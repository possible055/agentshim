import { describe, expect, it } from 'vitest'
import { bridgeJobStatus, bridgeProcess, bridgeText } from '../src/contracts.ts'

function processPayload() {
  return {
    bridgeVersion: 2,
    tool: 'run_program',
    process: {
      exitCode: '0',
      stdout: { text: 'done', totalBytes: 4, shownBytes: 4, omittedBytes: 0 },
      stderr: { text: '', totalBytes: 0, shownBytes: 0, omittedBytes: 0 },
    },
  }
}

function statusPayload() {
  return {
    bridgeVersion: 2,
    tool: 'bash_status',
    job: {
      jobId: 'bash-private',
      state: 'running',
      exitCode: null,
      totalBytes: 4,
      chunkStart: 0,
      nextCursor: 4,
      chunk: 'done',
      invalidUtf8Bytes: 0,
      capture: 'remote-spool',
      truncated: false,
      error: null,
    },
  }
}

describe('versioned bridge DTO parsers', () => {
  it('accepts complete process and job facts', () => {
    expect(bridgeProcess(processPayload(), 'run_program')).toMatchObject({ exitCode: '0' })
    expect(bridgeJobStatus(statusPayload(), 'bash-private')).toMatchObject({
      jobId: 'bash-private',
      nextCursor: 4,
      capture: 'remote-spool',
    })
  })

  it('fails loud on bridge, process, capture, cursor, or job-handle drift', () => {
    expect(() => bridgeText({ bridgeVersion: 2, tool: 'read', text: 'x' }, 'read'))
      .toThrow(expect.objectContaining({ code: 'AGENTSHIM_MALFORMED_RESULT' }))

    const malformedProcess = processPayload()
    malformedProcess.process.stdout.shownBytes = -1
    expect(() => bridgeProcess(malformedProcess, 'run_program'))
      .toThrow(expect.objectContaining({ code: 'AGENTSHIM_MALFORMED_RESULT' }))

    const wrongCapture = statusPayload()
    wrongCapture.job.capture = 'workspace-log'
    expect(() => bridgeJobStatus(wrongCapture, 'bash-private'))
      .toThrow(expect.objectContaining({ code: 'AGENTSHIM_MALFORMED_RESULT' }))

    const invalidCursor = statusPayload()
    invalidCursor.job.nextCursor = 5
    expect(() => bridgeJobStatus(invalidCursor, 'bash-private'))
      .toThrow(expect.objectContaining({ code: 'AGENTSHIM_MALFORMED_RESULT' }))

    expect(() => bridgeJobStatus(statusPayload(), 'bash-other'))
      .toThrow(expect.objectContaining({ code: 'AGENTSHIM_MALFORMED_RESULT' }))
  })
})
