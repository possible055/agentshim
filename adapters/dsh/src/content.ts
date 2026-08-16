import type { Context } from '@deepseek-ai/cordis'
import { createUserMessage, HarnessError } from '@deepseek-ai/dsh-llm'
import type { ContentBlock } from '@deepseek-ai/dsh-llm'
import type { JsonValue, ToolRunContext } from '@deepseek-ai/dsh-tools'
import type { ImageAttachmentRef } from '@deepseek-ai/dsh-attachment'

/** One MCP image block still in wire form (base64 + declared MIME). */
export interface RawImageBlock {
  readonly type: 'image'
  readonly data: string
  readonly mimeType: string
}

export type RawContentBlock =
  | { readonly type: 'text'; readonly text: string }
  | RawImageBlock

export interface NormalizedResult {
  readonly content: readonly RawContentBlock[]
  readonly structuredContent: JsonValue | undefined
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function resultText(content: unknown): string {
  if (!Array.isArray(content)) return ''
  return content
    .filter(isRecord)
    .filter(block => block.type === 'text' && typeof block.text === 'string')
    .map(block => block.text as string)
    .join('\n')
}

/**
 * The server error code as `AGENTSHIM_<SERVER_CODE>`: agentshim tool errors
 * carry `{ error: { code } }` in structuredContent; a missing or malformed
 * payload falls back to `AGENTSHIM_TOOL_ERROR`. Only name/code survive the
 * DSH tool registry, so nothing else is relied upon.
 */
export function serverErrorCode(raw: Record<string, unknown>): string {
  const code = isRecord(raw.structuredContent)
    ? isRecord(raw.structuredContent.error) && typeof raw.structuredContent.error.code === 'string'
      ? raw.structuredContent.error.code
      : undefined
    : undefined
  if (code === undefined) return 'AGENTSHIM_TOOL_ERROR'
  const sanitized = code.toUpperCase().replace(/[^A-Z0-9]+/g, '_').replace(/^_+|_+$/g, '')
  return sanitized === '' ? 'AGENTSHIM_TOOL_ERROR' : `AGENTSHIM_${sanitized}`
}

/** Map an `isError: true` MCP result to the stable HarnessError the adapter throws. */
export function mcpToolError(raw: Record<string, unknown>, toolName: string): HarnessError {
  const message = resultText(raw.content)
  return new HarnessError(
    message === '' ? `agentshim tool "${toolName}" failed without a message` : message,
    serverErrorCode(raw),
  )
}

/**
 * Trust boundary over a raw tools/call result: the request schema validated
 * only a record, so every content block is validated here. First version
 * accepts text and image blocks only; anything else fails loud.
 */
export function normalizeMcpResult(raw: Record<string, unknown>, toolName: string): NormalizedResult {
  if (raw.isError === true) throw mcpToolError(raw, toolName)
  if (!Array.isArray(raw.content)) {
    throw new HarnessError(`agentshim tool "${toolName}" returned no content array`, 'AGENTSHIM_MALFORMED_RESULT')
  }
  const content: RawContentBlock[] = []
  for (const value of raw.content) {
    if (!isRecord(value)) {
      throw new HarnessError(`agentshim tool "${toolName}" returned a non-object content block`, 'AGENTSHIM_UNSUPPORTED_CONTENT_BLOCK')
    }
    if (value.type === 'text' && typeof value.text === 'string') {
      content.push({ type: 'text', text: value.text })
    } else if (value.type === 'image' && typeof value.data === 'string' && typeof value.mimeType === 'string') {
      content.push({ type: 'image', data: value.data, mimeType: value.mimeType })
    } else {
      throw new HarnessError(
        `agentshim tool "${toolName}" returned an unsupported content block (type ${JSON.stringify(value.type)})`,
        'AGENTSHIM_UNSUPPORTED_CONTENT_BLOCK',
      )
    }
  }
  return { content, structuredContent: raw.structuredContent === undefined ? undefined : raw.structuredContent as JsonValue }
}

export function normalizedText(result: NormalizedResult): string {
  return result.content.flatMap(block => block.type === 'text' ? [block.text] : []).join('\n')
}

const PDF_TEXT_RETRY_HINT = ' — retry the same call with pdf_mode: "text" to receive page Markdown instead'

async function assertImageCapableRoute(ctx: Context, exec: ToolRunContext): Promise<void> {
  const routed = exec.agent?.session.requestHeader()?.config
  const provider = routed?.provider ?? exec.agent?.options.provider
  const model = routed?.model ?? exec.agent?.options.model
  const llm = ctx.get('llm')
  if (provider === undefined || model === undefined || llm === undefined) {
    throw new HarnessError(`cannot deliver an image from agentshim: the current model route could not be resolved${PDF_TEXT_RETRY_HINT}`, 'AGENTSHIM_IMAGE_ROUTE_UNRESOLVED')
  }
  const active = await llm.resolveModelInfo(provider, model, exec.signal)
  if (active.inputModalities === undefined || !active.inputModalities.includes('image')) {
    throw new HarnessError(`cannot deliver an image from agentshim: model "${model}" does not declare image input${PDF_TEXT_RETRY_HINT}`, 'AGENTSHIM_IMAGE_ROUTE_UNSUPPORTED')
  }
}

function strictBase64Decode(data: string): Uint8Array {
  if (!/^[A-Za-z0-9+/]*={0,2}$/.test(data) || data.length % 4 !== 0) {
    throw new HarnessError('agentshim returned an image block whose data is not strict base64', 'AGENTSHIM_INVALID_IMAGE_DATA')
  }
  const decoded = Buffer.from(data, 'base64')
  if (decoded.toString('base64') !== data) {
    throw new HarnessError('agentshim returned an image block whose data is not strict base64', 'AGENTSHIM_INVALID_IMAGE_DATA')
  }
  return new Uint8Array(decoded)
}

/**
 * Convert normalized MCP blocks into DSH content: text passes through in
 * order, images become durable attachments referenced by ImageBlocks (no
 * base64 reaches the canonical value or the session log). A nested Code Mode
 * call additionally defers the blocks so the image enters the next model
 * request. Every gate and preflight runs before the first image persists, so
 * a failure never publishes a partial tool result or session reference.
 */
export async function materializeContent(
  ctx: Context,
  exec: ToolRunContext,
  content: readonly RawContentBlock[],
): Promise<ContentBlock[]> {
  const images = content.filter((block): block is RawImageBlock => block.type === 'image')
  if (images.length === 0) {
    const blocks = content as readonly ContentBlock[]
    return [...blocks]
  }
  const attachments = ctx.get('attachments')
  if (attachments === undefined) {
    throw new HarnessError(`cannot deliver an image from agentshim: no attachment service is mounted${PDF_TEXT_RETRY_HINT}`, 'AGENTSHIM_IMAGE_STORAGE_UNAVAILABLE')
  }
  await assertImageCapableRoute(ctx, exec)
  const limits = attachments.imageLimits
  if (images.length > limits.maxImagesPerMessage) {
    throw new HarnessError(`agentshim returned ${images.length} images but this deployment allows at most ${limits.maxImagesPerMessage} per message${PDF_TEXT_RETRY_HINT}`, 'AGENTSHIM_IMAGE_LIMIT_EXCEEDED')
  }
  const decoded = images.map(block => {
    if (!limits.mediaTypes.includes(block.mimeType as never)) {
      throw new HarnessError(`agentshim returned an image with media type ${block.mimeType}, which this deployment does not accept${PDF_TEXT_RETRY_HINT}`, 'AGENTSHIM_IMAGE_LIMIT_EXCEEDED')
    }
    const bytes = strictBase64Decode(block.data)
    if (bytes.byteLength > limits.maxImageBytes) {
      throw new HarnessError(`agentshim returned an image of ${bytes.byteLength} bytes, above this deployment's ${limits.maxImageBytes}-byte per-image limit${PDF_TEXT_RETRY_HINT}`, 'AGENTSHIM_IMAGE_LIMIT_EXCEEDED')
    }
    return bytes
  })
  const aggregate = decoded.reduce((total, bytes) => total + bytes.byteLength, 0)
  if (aggregate > limits.maxMessageImageBytes) {
    throw new HarnessError(`agentshim returned images totalling ${aggregate} bytes, above this deployment's ${limits.maxMessageImageBytes}-byte per-message limit${PDF_TEXT_RETRY_HINT}`, 'AGENTSHIM_IMAGE_LIMIT_EXCEEDED')
  }
  const inputs = images.map((block, index) => ({
    data: decoded[index] as Uint8Array,
    mediaType: block.mimeType as Parameters<typeof attachments.saveImage>[0]['mediaType'],
  }))
  for (const input of inputs) {
    await attachments.validateImage(input)
  }
  const blocks: ContentBlock[] = []
  let imageIndex = 0
  for (const block of content) {
    if (block.type === 'text') {
      blocks.push({ type: 'text', text: block.text })
      continue
    }
    const input = inputs[imageIndex] as Parameters<typeof attachments.saveImage>[0]
    imageIndex += 1
    blocks.push({ type: 'image', attachment: await attachments.saveImage(input) })
  }
  if (exec.parent !== undefined) {
    exec.deferContext(createUserMessage({
      content: blocks,
      source: { kind: 'plugin', plugin: 'agentshim' },
    }))
  }
  return blocks
}

export async function materializeReadAttachments(
  ctx: Context,
  exec: ToolRunContext,
  content: readonly RawContentBlock[],
): Promise<ImageAttachmentRef[]> {
  const blocks = await materializeContent(ctx, exec, content)
  return blocks.flatMap(block => block.type === 'image' ? [block.attachment] : [])
}
