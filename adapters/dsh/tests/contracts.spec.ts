import { describe, expect, it } from 'vitest'
import { PUBLIC_TOOL_NAMES, readParameters } from '../src/contracts.ts'

describe('native public contracts', () => {
  it('publishes the six stable DSH tool names', () => {
    expect(PUBLIC_TOOL_NAMES).toEqual(['read', 'grep', 'glob', 'run_program', 'bash', 'bash_status'])
  })

  it('exposes byte continuation only through the read artifact offset', () => {
    expect(readParameters.artifact_offset).toMatchObject({ type: 'integer' })
    expect(Object.keys(readParameters)).not.toContain('_agentshimReadGrant')
  })
})
