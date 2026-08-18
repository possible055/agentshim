import { readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'
import {
  bashParameters,
  bashStatusParameters,
  PUBLIC_TOOL_NAMES,
  readParameters,
} from '../src/contracts.ts'

const divergence = JSON.parse(await readFile(
  fileURLToPath(new URL('../../../evals/host-divergence.json', import.meta.url)),
  'utf8',
)) as {
  readonly bash: { readonly dsh: { readonly fields: readonly string[]; readonly required: readonly string[] } }
  readonly bash_status: { readonly dsh: { readonly fields: readonly string[]; readonly required: readonly string[] } }
}

function publicShape(parameters: Record<string, unknown>) {
  return {
    fields: Object.keys(parameters).sort(),
    required: Object.entries(parameters)
      .filter(([, parameter]) => (parameter as { readonly required?: unknown }).required === true)
      .map(([name]) => name)
      .sort(),
  }
}

describe('native public contracts', () => {
  it('publishes the six stable DSH tool names', () => {
    expect(PUBLIC_TOOL_NAMES).toEqual(['read', 'grep', 'glob', 'run_program', 'bash', 'bash_status'])
  })

  it('exposes byte continuation only through the read artifact offset', () => {
    expect(readParameters.artifact_offset).toMatchObject({ type: 'integer' })
    expect(Object.keys(readParameters)).not.toContain('_agentshimReadGrant')
  })

  it('matches the intentional DSH bash ownership divergence snapshot', () => {
    expect(publicShape(bashParameters)).toEqual({
      fields: [...divergence.bash.dsh.fields].sort(),
      required: [...divergence.bash.dsh.required].sort(),
    })
    expect(publicShape(bashStatusParameters)).toEqual({
      fields: [...divergence.bash_status.dsh.fields].sort(),
      required: [...divergence.bash_status.dsh.required].sort(),
    })
  })
})
