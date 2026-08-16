import { describe, expect, it, vi } from 'vitest'
import type { Context } from '@deepseek-ai/cordis'
import { HarnessError } from '@deepseek-ai/dsh-llm'
import type { ToolRunContext } from '@deepseek-ai/dsh-tools'
import { materializeContent, normalizeMcpResult, serverErrorCode } from '../src/content.ts'

const signal = new AbortController().signal
const PNG_1X1 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=='

function stubContext(services: Record<string, unknown>): Context {
  return { get: (name: string) => services[name] } as unknown as Context
}

function stubExec(overrides: Record<string, unknown> = {}): ToolRunContext {
  return {
    signal,
    callId: 'c1',
    name: 'read',
    arguments: {},
    deferContext: vi.fn(),
    ...overrides,
  } as unknown as ToolRunContext
}

function routedAgent() {
  return {
    session: { requestHeader: () => undefined },
    options: { provider: 'p', model: 'm' },
  }
}

function stubAttachments(overrides: Record<string, unknown> = {}) {
  return {
    imageLimits: {
      maxImageBytes: 1024 * 1024,
      maxImagesPerMessage: 4,
      maxMessageImageBytes: 2 * 1024 * 1024,
      maxImagePixels: 4 * 1000 * 1000,
      mediaTypes: ['image/png', 'image/jpeg', 'image/webp', 'image/gif'],
    },
    validateImage: vi.fn(async () => {}),
    saveImage: vi.fn(async () => ({
      attachmentId: 'att-1',
      mediaType: 'image/png',
      bytes: 70,
      width: 1,
      height: 1,
    })),
    ...overrides,
  }
}

function stubLlm(modalities: readonly string[]) {
  return { resolveModelInfo: vi.fn(async () => ({ inputModalities: modalities })) }
}

describe('serverErrorCode', () => {
  it('maps the structured error code and falls back on a missing payload', () => {
    expect(serverErrorCode({ structuredContent: { error: { code: 'output_budget' } } })).toBe('AGENTSHIM_OUTPUT_BUDGET')
    expect(serverErrorCode({ structuredContent: { error: { code: 'client_cancellation' } } })).toBe('AGENTSHIM_CLIENT_CANCELLATION')
    expect(serverErrorCode({})).toBe('AGENTSHIM_TOOL_ERROR')
    expect(serverErrorCode({ structuredContent: { error: {} } })).toBe('AGENTSHIM_TOOL_ERROR')
  })
})

describe('normalizeMcpResult', () => {
  it('passes text and image blocks through with optional structured content', () => {
    const normalized = normalizeMcpResult({
      content: [{ type: 'text', text: 'a' }, { type: 'image', data: 'AAAA', mimeType: 'image/png' }],
      structuredContent: { ok: true },
    }, 'read')
    expect(normalized.content).toEqual([
      { type: 'text', text: 'a' },
      { type: 'image', data: 'AAAA', mimeType: 'image/png' },
    ])
    expect(normalized.structuredContent).toEqual({ ok: true })
  })

  it('maps isError results to a stable HarnessError code', () => {
    let error: unknown
    try {
      normalizeMcpResult({
        isError: true,
        content: [{ type: 'text', text: 'fixture says no' }],
        structuredContent: { error: { code: 'fixture_denied' } },
      }, 'bash')
    } catch (caught) {
      error = caught
    }
    expect(error).toBeInstanceOf(HarnessError)
    expect(error).toMatchObject({ code: 'AGENTSHIM_FIXTURE_DENIED', message: 'fixture says no' })
  })

  it('fails loud on malformed or unsupported blocks', () => {
    expect(() => normalizeMcpResult({ content: 'nope' }, 'read')).toThrow(HarnessError)
    expect(() => normalizeMcpResult({ content: [{ type: 'audio', data: 'AAAA', mimeType: 'audio/wav' }] }, 'read'))
      .toThrow(/unsupported content block/)
    expect(() => normalizeMcpResult({ content: [42] }, 'read')).toThrow(/non-object content block/)
  })
})

describe('materializeContent', () => {
  it('passes text-only content through without needing any service', async () => {
    const blocks = await materializeContent(stubContext({}), stubExec(), [{ type: 'text', text: 'plain' }])
    expect(blocks).toEqual([{ type: 'text', text: 'plain' }])
  })

  it('refuses images without an attachment store, pointing at the text-mode retry', async () => {
    const error = await materializeContent(stubContext({}), stubExec(), [
      { type: 'text', text: 'page 1' },
      { type: 'image', data: PNG_1X1, mimeType: 'image/png' },
    ]).then(() => { throw new Error('expected rejection') }, reason => reason)
    expect(error).toMatchObject({ code: 'AGENTSHIM_IMAGE_STORAGE_UNAVAILABLE' })
    expect(String(error.message)).toContain('pdf_mode: "text"')
  })

  it('refuses images when the model route cannot be resolved', async () => {
    const error = await materializeContent(stubContext({ attachments: stubAttachments() }), stubExec(), [
      { type: 'image', data: PNG_1X1, mimeType: 'image/png' },
    ]).then(() => { throw new Error('expected rejection') }, reason => reason)
    expect(error).toMatchObject({ code: 'AGENTSHIM_IMAGE_ROUTE_UNRESOLVED' })
  })

  it('refuses images on a text-only model route', async () => {
    const exec = stubExec({ agent: routedAgent() })
    const error = await materializeContent(stubContext({ attachments: stubAttachments(), llm: stubLlm(['text']) }), exec, [
      { type: 'image', data: PNG_1X1, mimeType: 'image/png' },
    ]).then(() => { throw new Error('expected rejection') }, reason => reason)
    expect(error).toMatchObject({ code: 'AGENTSHIM_IMAGE_ROUTE_UNSUPPORTED' })
  })

  it('persists validated images as attachment references and defers nested context', async () => {
    const attachments = stubAttachments()
    const deferContext = vi.fn()
    const exec = stubExec({ agent: routedAgent(), parent: Symbol('token') as never, deferContext })
    const blocks = await materializeContent(stubContext({ attachments, llm: stubLlm(['text', 'image']) }), exec, [
      { type: 'text', text: 'page 1' },
      { type: 'image', data: PNG_1X1, mimeType: 'image/png' },
      { type: 'text', text: 'end' },
    ])
    expect(blocks).toHaveLength(3)
    expect(blocks[0]).toEqual({ type: 'text', text: 'page 1' })
    expect(blocks[1]).toMatchObject({ type: 'image', attachment: { attachmentId: 'att-1', mediaType: 'image/png' } })
    expect(blocks[2]).toEqual({ type: 'text', text: 'end' })
    expect(JSON.stringify(blocks)).not.toContain(PNG_1X1)
    expect(attachments.validateImage).toHaveBeenCalledTimes(1)
    expect(attachments.saveImage).toHaveBeenCalledTimes(1)
    const saved = (attachments.saveImage as ReturnType<typeof vi.fn>).mock.calls[0]?.[0] as { data: Uint8Array; mediaType: string }
    expect(saved.mediaType).toBe('image/png')
    expect(saved.data).toBeInstanceOf(Uint8Array)
    expect(deferContext).toHaveBeenCalledTimes(1)
  })

  it('validates every image before persisting any', async () => {
    const attachments = stubAttachments({
      validateImage: vi.fn(async input => {
        if (input.mediaType === 'image/jpeg') throw new Error('decode failed')
      }),
    })
    const exec = stubExec({ agent: routedAgent() })
    await expect(materializeContent(stubContext({ attachments, llm: stubLlm(['image']) }), exec, [
      { type: 'image', data: PNG_1X1, mimeType: 'image/png' },
      { type: 'image', data: PNG_1X1, mimeType: 'image/jpeg' },
    ])).rejects.toThrow(/decode failed/)
    expect(attachments.saveImage).not.toHaveBeenCalled()
  })

  it('rejects invalid base64 and unsupported media types', async () => {
    const services = { attachments: stubAttachments(), llm: stubLlm(['image']) }
    const exec = stubExec({ agent: routedAgent() })
    await expect(materializeContent(stubContext(services), exec, [
      { type: 'image', data: 'not*base64!', mimeType: 'image/png' },
    ])).rejects.toMatchObject({ code: 'AGENTSHIM_INVALID_IMAGE_DATA' })
    await expect(materializeContent(stubContext(services), exec, [
      { type: 'image', data: PNG_1X1, mimeType: 'image/bmp' },
    ])).rejects.toMatchObject({ code: 'AGENTSHIM_IMAGE_LIMIT_EXCEEDED' })
  })

  it('enforces the per-image, aggregate, and count limits', async () => {
    const exec = stubExec({ agent: routedAgent() })
    const small = stubAttachments({ imageLimits: { ...stubAttachments().imageLimits, maxImageBytes: 8 } })
    await expect(materializeContent(stubContext({ attachments: small, llm: stubLlm(['image']) }), exec, [
      { type: 'image', data: PNG_1X1, mimeType: 'image/png' },
    ])).rejects.toMatchObject({ code: 'AGENTSHIM_IMAGE_LIMIT_EXCEEDED' })
    const tightAggregate = stubAttachments({ imageLimits: { ...stubAttachments().imageLimits, maxMessageImageBytes: 8 } })
    await expect(materializeContent(stubContext({ attachments: tightAggregate, llm: stubLlm(['image']) }), exec, [
      { type: 'image', data: PNG_1X1, mimeType: 'image/png' },
      { type: 'image', data: PNG_1X1, mimeType: 'image/png' },
    ])).rejects.toMatchObject({ code: 'AGENTSHIM_IMAGE_LIMIT_EXCEEDED' })
    const oneOnly = stubAttachments({ imageLimits: { ...stubAttachments().imageLimits, maxImagesPerMessage: 1 } })
    await expect(materializeContent(stubContext({ attachments: oneOnly, llm: stubLlm(['image']) }), exec, [
      { type: 'image', data: PNG_1X1, mimeType: 'image/png' },
      { type: 'image', data: PNG_1X1, mimeType: 'image/png' },
    ])).rejects.toMatchObject({ code: 'AGENTSHIM_IMAGE_LIMIT_EXCEEDED' })
  })
})
