import { readFileSync, rmSync, writeFileSync } from 'node:fs'
import { Server } from '@modelcontextprotocol/sdk/server/index.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { CallToolRequestSchema, ListToolsRequestSchema } from '@modelcontextprotocol/sdk/types.js'
import { z } from 'zod'

// Stand-in for `agentshim serve` over stdio. Catalog and behavior variants are
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
    inputSchema: { type: 'object', oneOf: [
      {
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
      {
        type: 'object',
        additionalProperties: false,
        required: ['action', 'job_id'],
        properties: {
          action: { type: 'string', const: 'terminate', description: 'Terminate the complete detached tree.' },
          job_id: { type: 'string', pattern: '^bash-', description: 'Opaque instance-bound job id.' },
        },
      },
    ] },
  },
  {
    name: 'bash_status',
    title: 'Bash Status',
    description: 'Return an immediate detached Bash lifecycle snapshot and bounded log tail.',
    annotations: readOnlyHints,
    inputSchema: {
      type: 'object',
      additionalProperties: false,
      required: ['job_id'],
      properties: {
        job_id: { type: 'string', pattern: '^bash-', description: 'Opaque instance-bound job id.' },
        tail_bytes: { type: 'integer', minimum: 0, maximum: 16384, default: 8192, description: 'Bounded tail bytes.' },
      },
    },
  },
]

for (const tool of baseTools) {
  tool._meta = { 'agentshim.dshBridge': { version: mode === 'wrong-bridge' ? 1 : 2 } }
}

let tools = baseTools
if (mode === 'missing') tools = baseTools.filter(tool => tool.name !== 'grep')
if (mode === 'extra') {
  tools = [...baseTools, {
    name: 'invoke',
    description: 'Extra tool outside the fixed six-name contract.',
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

const server = new Server({ name: 'agentshim-fixture', version: '0.0.1' }, { capabilities: { tools: { listChanged: false } } })

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
const fixtureJobs = new Map()
const captureAck = z.object({ nextOffset: z.number().int().nonnegative() })
const captureCompleteAck = z.object({ complete: z.literal(true) })

async function captureOutput(args, streams) {
  const capture = args._agentshimCapture
  if (capture === undefined) return
  const totals = {}
  for (const [stream, bytes] of Object.entries(streams)) {
    const data = Buffer.from(bytes)
    totals[stream] = data.byteLength
    if (data.byteLength > 0) {
      await server.request({
        method: 'agentshim/dsh.capture.append',
        params: {
          bridgeVersion: 2,
          captureId: capture.id,
          stream,
          offset: 0,
          data: data.toString('base64'),
        },
      }, captureAck)
    }
  }
  const failed = process.env.FIXTURE_CAPTURE_ERROR === '1'
  await server.request({
    method: 'agentshim/dsh.capture.complete',
    params: {
      bridgeVersion: 2,
      captureId: capture.id,
      complete: !failed,
      totals,
      ...(failed ? { error: 'AGENTSHIM_CAPTURE_IO_FAILED: injected fixture storage failure' } : {}),
    },
  }, captureCompleteAck)
}

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
      structuredContent: { bridgeVersion: 2, tool: request.params.name },
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
  const args = request.params.arguments ?? {}
  if (request.params.name === 'bash' && args.detach === true) {
    if (process.env.FIXTURE_BACKGROUND_START_ERROR === '1') {
      return {
        content: [{ type: 'text', text: 'fixture background start failed' }],
        isError: true,
        structuredContent: { error: { code: 'fixture_start_failed', message: 'fixture background start failed', retryable: false, details: null } },
      }
    }
    const jobId = `bash-00000000-0000-4000-8000-${String(fixtureJobs.size + 1).padStart(12, '0')}`
    fixtureJobs.set(jobId, {
      body: process.env.FIXTURE_JOB_OUTPUT ?? 'background output\n',
      state: process.env.FIXTURE_JOB_RUNNING === '1' ? 'running' : 'completed',
    })
    await captureOutput(args, { output: process.env.FIXTURE_JOB_OUTPUT ?? 'background output\n' })
    return {
      content: [{ type: 'text', text: `Detached: job_id=${jobId}` }],
      structuredContent: {
        bridgeVersion: 2,
        tool: 'bash',
        job: { jobId, pid: 1234 },
      },
    }
  }
  if (request.params.name === 'bash' && args.action === 'terminate') {
    if (process.env.FIXTURE_TERMINATE_ERROR === '1') {
      return {
        content: [{ type: 'text', text: 'fixture terminate failed' }],
        isError: true,
        structuredContent: { error: { code: 'fixture_terminate_failed', message: 'fixture terminate failed', retryable: false, details: null } },
      }
    }
    const job = fixtureJobs.get(args.job_id)
    if (job !== undefined) job.state = 'terminated'
    return {
      content: [{ type: 'text', text: `terminated ${args.job_id}` }],
      structuredContent: { bridgeVersion: 2, tool: 'bash' },
    }
  }
  if (request.params.name === 'bash_status') {
    if (process.env.FIXTURE_JOB_STATUS_ERROR === '1') {
      return {
        content: [{ type: 'text', text: 'fixture status failed' }],
        isError: true,
        structuredContent: { error: { code: 'fixture_status_failed', message: 'fixture status failed', retryable: false, details: null } },
      }
    }
    const job = fixtureJobs.get(args.job_id) ?? { body: '', state: 'completed' }
    const body = job.body
    const cursor = Number(args.cursor ?? 0)
    const chunk = body.slice(cursor)
    return {
      content: [{ type: 'text', text: `State: ${job.state}\n${chunk}` }],
      structuredContent: {
        bridgeVersion: 2,
        tool: 'bash_status',
        job: {
          jobId: args.job_id,
          state: job.state,
          exitCode: job.state === 'completed' ? '0' : null,
          totalBytes: body.length,
          chunkStart: cursor,
          nextCursor: body.length,
          chunk,
          invalidUtf8Bytes: 0,
          capture: 'remote-spool',
          truncated: process.env.FIXTURE_JOB_TRUNCATED === '1',
          error: null,
        },
      },
    }
  }
  if (request.params.name === 'run_program' || request.params.name === 'bash') {
    const visibleArgs = Object.fromEntries(Object.entries(args).filter(([key]) => !key.startsWith('_agentshim')))
    const text = JSON.stringify({ name: request.params.name, arguments: visibleArgs })
    await captureOutput(args, request.params.name === 'run_program' ? { stdout: text, stderr: '' } : { output: text })
    return {
      content: [{ type: 'text', text }],
      structuredContent: {
        bridgeVersion: 2,
        tool: request.params.name,
        process: {
          exitCode: '0',
          stdout: { text, totalBytes: text.length, shownBytes: text.length, omittedBytes: 0 },
          stderr: { text: '', totalBytes: 0, shownBytes: 0, omittedBytes: 0 },
        },
      },
    }
  }
  return {
    content: [{ type: 'text', text: JSON.stringify({ name: request.params.name, arguments: request.params.arguments ?? null }) }],
    structuredContent: {
      bridgeVersion: 2,
      tool: request.params.name,
    },
  }
})

const connectDelay = Number.parseInt(process.env.FIXTURE_CONNECT_DELAY_MS ?? '0', 10) || 0
if (connectDelay > 0) await new Promise(resolve => setTimeout(resolve, connectDelay))
await server.connect(new StdioServerTransport())
