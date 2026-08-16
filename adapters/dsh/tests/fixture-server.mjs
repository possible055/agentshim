import { readFileSync, rmSync, writeFileSync } from 'node:fs'
import { Server } from '@modelcontextprotocol/sdk/server/index.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { CallToolRequestSchema, ListToolsRequestSchema } from '@modelcontextprotocol/sdk/types.js'

// Stand-in for `codexshim serve` over stdio. Catalog and behavior variants are
// selected with FIXTURE_MODE so session.spec.ts can exercise every startup
// failure and lifecycle path against a real child process.

const mode = process.env.FIXTURE_MODE ?? 'ok'

function readBootCount() {
  try {
    return Number.parseInt(readFileSync(process.env.FIXTURE_BOOT_FILE, 'utf8'), 10) || 0
  } catch {
    return 0
  }
}

const boot = readBootCount() + 1
if (process.env.FIXTURE_BOOT_FILE) writeFileSync(process.env.FIXTURE_BOOT_FILE, String(boot))
if (process.env.FIXTURE_REPORT) {
  writeFileSync(process.env.FIXTURE_REPORT, JSON.stringify({ argv: process.argv, env: process.env }, null, 2))
}
if (process.env.FIXTURE_EXIT_FILE) {
  process.on('exit', () => {
    try {
      writeFileSync(process.env.FIXTURE_EXIT_FILE, String(boot))
    } catch { /* best effort */ }
  })
}
if (process.env.FIXTURE_CRASH_FILE) {
  const poller = setInterval(() => {
    try {
      rmSync(process.env.FIXTURE_CRASH_FILE)
      process.exit(1)
    } catch (error) {
      if (error?.code !== 'ENOENT') throw error
    }
  }, 25)
  poller.unref()
}
const readOnlyHints = { readOnlyHint: true, destructiveHint: false, openWorldHint: false }
const processHints = { readOnlyHint: false, destructiveHint: true, openWorldHint: true }
const drifted = mode === 'drift' && boot >= 2

const baseTools = [
  {
    name: 'read',
    title: 'Read',
    description: drifted
      ? 'DRIFTED read description that must trip the catalog fingerprint.'
      : 'Read one file as numbered lines. Omit line_count to fill one response.',
    annotations: readOnlyHints,
    inputSchema: {
      type: 'object',
      additionalProperties: false,
      required: ['path'],
      properties: {
        path: { type: 'string', minLength: 1, description: 'Platform-native path to one file.' },
        start_line: { type: 'integer', minimum: 1, default: 1, description: 'One-based first line to return.' },
        line_count: { type: 'integer', minimum: 1, maximum: 2000, description: 'Maximum lines to return.' },
        pdf_mode: { type: 'string', enum: ['auto', 'text', 'image'], default: 'auto', description: 'PDF rendering mode.' },
        pages: { type: 'string', pattern: '^[1-9][0-9]*(-[1-9][0-9]*)?$', description: 'PDF only: page or range.' },
      },
    },
  },
  {
    name: 'grep',
    title: 'Grep',
    description: 'Search file contents using Rust regex or fixed strings.',
    annotations: readOnlyHints,
    inputSchema: {
      type: 'object',
      additionalProperties: false,
      required: ['pattern'],
      properties: {
        pattern: { type: 'string', minLength: 1, description: 'Regex or literal to find.' },
        path: { type: 'string', description: 'Optional file or directory to search.' },
        glob: { type: 'string', description: 'Case-sensitive glob filter.' },
        case: { type: 'string', enum: ['smart', 'sensitive', 'insensitive'], default: 'smart', description: 'Case sensitivity.' },
        context_lines: { type: 'integer', minimum: 0, description: 'Context to show around matches.' },
        limit: { type: 'integer', minimum: 1, maximum: 1000, description: 'Maximum entries.' },
        offset: { type: 'integer', minimum: 0, description: 'Resume offset.' },
      },
    },
  },
  {
    name: 'glob',
    title: 'Glob',
    description: 'Find files by glob pattern.',
    annotations: readOnlyHints,
    inputSchema: {
      type: 'object',
      additionalProperties: false,
      required: ['pattern'],
      properties: {
        pattern: { type: 'string', minLength: 1, description: 'Glob pattern.' },
        type: { type: 'string', enum: ['file', 'directory', 'any'], default: 'file', description: 'Entry kind.' },
        limit: { type: 'integer', minimum: 1, maximum: 1000, description: 'Maximum paths.' },
        offset: { type: 'integer', minimum: 0, description: 'Resume offset.' },
      },
    },
  },
  {
    name: 'run_program',
    title: 'Run Program',
    description: 'Run one program with literal argv and return merged output.',
    annotations: processHints,
    ...(mode === 'task-required' ? { execution: { taskSupport: 'required' } } : {}),
    inputSchema: {
      type: 'object',
      additionalProperties: false,
      required: ['program'],
      properties: {
        program: { type: 'string', minLength: 1, description: 'Program name or executable path.' },
        args: { type: 'array', items: { type: 'string' }, default: [], description: 'Literal argv elements.' },
        cwd: { type: 'string', description: 'Working directory.' },
        env: { type: 'object', additionalProperties: { type: 'string' }, default: {}, description: 'Env overrides.' },
        stdin: { type: ['string', 'null'], maxLength: 1048576, description: 'Optional UTF-8 stdin.' },
        timeout_ms: { type: 'integer', minimum: 1, maximum: 590000, default: 120000, description: 'Execution timeout.' },
        unset_env: { type: 'array', items: { type: 'string' }, default: [], description: 'Env names to remove.' },
      },
    },
  },
  {
    name: 'bash',
    title: 'Bash',
    description: 'Run a POSIX bash command line and return merged output with the exit code.',
    annotations: processHints,
    inputSchema: {
      type: 'object',
      additionalProperties: false,
      required: ['command'],
      properties: {
        command: { type: 'string', minLength: 1, description: 'POSIX bash command line.' },
        cwd: { type: 'string', description: 'Working directory.' },
        detach: { type: 'boolean', default: false, description: 'Run past the end of this call.' },
        log_path: { type: 'string', description: 'Output file for detached runs.' },
        msys_argument_conversion: { type: 'string', enum: ['default', 'disabled'], default: 'default', description: 'Windows Git Bash only.' },
        timeout_ms: { type: 'integer', minimum: 1, maximum: 590000, default: 120000, description: 'Execution timeout.' },
      },
    },
  },
]

let tools = baseTools
if (mode === 'missing') tools = baseTools.filter(tool => tool.name !== 'grep')
if (mode === 'extra') {
  tools = [...baseTools, {
    name: 'invoke',
    description: 'Extra tool outside the fixed five-name contract.',
    inputSchema: { type: 'object', additionalProperties: false, properties: {} },
  }]
}
if (mode === 'duplicate') tools = [...baseTools, { ...baseTools[3] }]
if (mode === 'unsupported') {
  tools = structuredClone(baseTools)
  tools[0].inputSchema.properties.path.anyOf = [{ type: 'string' }, { type: 'null' }]
}

const paginate = process.env.FIXTURE_PAGINATE === '1'
const pageSize = paginate ? 3 : tools.length

const server = new Server({ name: 'codexshim-fixture', version: '0.0.1' }, { capabilities: { tools: { listChanged: false } } })

server.setRequestHandler(ListToolsRequestSchema, request => {
  const cursor = request.params?.cursor === undefined ? 0 : Number.parseInt(request.params.cursor, 10)
  if (!Number.isInteger(cursor) || cursor < 0 || cursor > tools.length) {
    throw new Error(`fixture: invalid list cursor ${request.params?.cursor}`)
  }
  const page = tools.slice(cursor, cursor + pageSize)
  const next = cursor + pageSize
  return next < tools.length ? { tools: page, nextCursor: String(next) } : { tools: page }
})

const PNG_1X1 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=='

server.setRequestHandler(CallToolRequestSchema, async (request, extra) => {
  if (process.env.FIXTURE_CALL_ERROR === '1') {
    return {
      content: [{ type: 'text', text: 'fixture says no' }],
      isError: true,
      structuredContent: { error: { code: 'fixture_denied', message: 'fixture says no', retryable: false, details: null } },
    }
  }
  if (process.env.FIXTURE_IMAGE === '1' || process.env.FIXTURE_IMAGE_BAD_B64 === '1' || process.env.FIXTURE_IMAGE_BAD_MIME === '1') {
    const data = process.env.FIXTURE_IMAGE_BAD_B64 === '1' ? 'not*base64!' : PNG_1X1
    const mimeType = process.env.FIXTURE_IMAGE_BAD_MIME === '1' ? 'image/bmp' : 'image/png'
    return {
      content: [
        { type: 'text', text: 'page 1' },
        { type: 'image', data, mimeType },
      ],
    }
  }
  const delay = Number.parseInt(process.env.FIXTURE_CALL_DELAY_MS ?? '0', 10) || 0
  if (delay > 0) {
    await new Promise((resolve, reject) => {
      const timer = setTimeout(resolve, delay)
      extra.signal.addEventListener('abort', () => {
        clearTimeout(timer)
        reject(extra.signal.reason)
      }, { once: true })
    })
  }
  return {
    content: [{ type: 'text', text: JSON.stringify({ name: request.params.name, arguments: request.params.arguments ?? null }) }],
  }
})

const connectDelay = Number.parseInt(process.env.FIXTURE_CONNECT_DELAY_MS ?? '0', 10) || 0
if (connectDelay > 0) await new Promise(resolve => setTimeout(resolve, connectDelay))
await server.connect(new StdioServerTransport())
