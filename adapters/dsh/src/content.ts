import { createRequire } from 'node:module'
import type { Context } from '@deepseek-ai/cordis'
import { createUserMessage, HarnessError } from '@deepseek-ai/dsh-llm'
import type { ContentBlock } from '@deepseek-ai/dsh-llm'
import type { ToolRunContext } from '@deepseek-ai/dsh-tools'
import type { ImageAttachmentRef } from '@deepseek-ai/dsh-attachment'

const require = createRequire(import.meta.url)

let cachedDshVersion: string | null | undefined = undefined

/**
 * Read the installed @deepseek-ai/dsh-tools (or @deepseek-ai/dsh-agent) version from its manifest.
 */
export function getDshPackageVersion(): string | undefined {
  if (cachedDshVersion !== undefined) return cachedDshVersion ?? undefined
  try {
    const pkg = require('@deepseek-ai/dsh-tools/package.json') as { version?: string }
    cachedDshVersion = typeof pkg?.version === 'string' ? pkg.version : null
  } catch {
    try {
      const pkg = require('@deepseek-ai/dsh-agent/package.json') as { version?: string }
      cachedDshVersion = typeof pkg?.version === 'string' ? pkg.version : null
    } catch {
      cachedDshVersion = null
    }
  }
  return cachedDshVersion ?? undefined
}

/**
 * Determine whether the current DSH host runtime requires manual context deferral
 * for nested image results in Code Mode (DSH 0.1.0-rc.6).
 * In DSH 0.1.0-rc.7+, Code Mode automatically forwards nested image content blocks.
 */
export function requiresManualCodeModeImageDeferral(ctx: Context, exec: ToolRunContext): boolean {
  if (exec.parent === undefined) return false

  // 1. Feature detection: DSH rc.7+ provides `AttachmentStore.saveImages`.
  const attachments = ctx.get('attachments') as { saveImages?: unknown } | undefined
  if (attachments !== undefined) {
    return typeof attachments.saveImages !== 'function'
  }

  // 2. Exact legacy fallback when the attachment capability is not mounted.
  const version = getDshPackageVersion()
  return version === '0.1.0-rc.6'
}

/** One native image block before DSH attachment materialization. */
export interface RawImageBlock {
  readonly type: 'image'
  readonly data: string
  readonly mimeType: string
}

export type RawContentBlock =
  | { readonly type: 'text'; readonly text: string }
  | RawImageBlock

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
 * Convert native blocks into DSH content: text passes through in
 * order, images become durable attachments referenced by ImageBlocks (no
 * base64 reaches the canonical value or the session log). Every gate and
 * preflight runs before the first image persists, so a failure never
 * publishes a partial tool result or session reference.
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

  let savedRefs: readonly ImageAttachmentRef[]
  const batchSave = (attachments as { saveImages?: (items: typeof inputs) => Promise<readonly ImageAttachmentRef[]> }).saveImages
  if (typeof batchSave === 'function') {
    savedRefs = await batchSave.call(attachments, inputs)
  } else {
    for (const input of inputs) {
      await attachments.validateImage(input)
    }
    const refs: ImageAttachmentRef[] = []
    for (const input of inputs) {
      refs.push(await attachments.saveImage(input))
    }
    savedRefs = refs
  }

  const blocks: ContentBlock[] = []
  let imageIndex = 0
  for (const block of content) {
    if (block.type === 'text') {
      blocks.push({ type: 'text', text: block.text })
      continue
    }
    blocks.push({ type: 'image', attachment: savedRefs[imageIndex] as ImageAttachmentRef })
    imageIndex += 1
  }

  if (requiresManualCodeModeImageDeferral(ctx, exec)) {
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
