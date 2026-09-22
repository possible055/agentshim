import { readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'
import {
  bashParameters,
  bashStatusParameters,
  globParameters,
  grepParameters,
  PUBLIC_TOOL_NAMES,
  readOutputSchema,
  readParameters,
  assertPattern,
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

  it('declares type filtering and multi-pattern oneOf for grep and glob', () => {
    expect(grepParameters.type).toMatchObject({ type: 'string' })
    expect(grepParameters.glob).toHaveProperty('oneOf')
    expect(globParameters.pattern).toHaveProperty('oneOf')
  })

  it('keeps regex punctuation distinct from glob negation', () => {
    expect(() => assertPattern('!', 'pattern', 8192)).not.toThrow()
    expect(() => assertPattern('!', 'pattern', 1024, true)).toThrow(/contain a pattern after/)
  })

  it('gives a cold-start model defaults and continuation instructions', () => {
    expect(readParameters.start_line).toMatchObject({ default: 1 })
    expect(readParameters.pdf_mode).toMatchObject({ default: 'auto' })
    expect(grepParameters.limit).toMatchObject({ default: 200 })
    expect(globParameters.path).toMatchObject({ default: '.' })
    expect(bashParameters.run_in_background).toMatchObject({ default: false })
    expect(bashParameters.timeoutMs).toMatchObject({ type: 'integer' })
    expect(bashStatusParameters.job_id.description).toContain('job ID returned by bash')
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

  it('declares attachment originalDimensions in readOutputSchema', () => {
    const properties = (readOutputSchema as { properties: { attachments: { items: { properties: Record<string, unknown> } } } }).properties
    const attachmentProperties = properties.attachments.items.properties
    expect(attachmentProperties).toHaveProperty('originalDimensions')
    expect(attachmentProperties.originalDimensions).toMatchObject({
      type: 'object',
      properties: {
        width: { type: 'integer' },
        height: { type: 'integer' },
      },
    })
  })
})
