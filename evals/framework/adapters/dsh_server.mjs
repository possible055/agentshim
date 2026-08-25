import { createInterface } from "node:readline";
import { createRequire } from "node:module";
import { resolve, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  statSync,
  writeFileSync,
} from "node:fs";

const __dirname = fileURLToPath(new URL(".", import.meta.url));
const projectRoot = resolve(__dirname, "../../../");
const require = createRequire(import.meta.url);

const STAGED_DIR = resolve(projectRoot, "local/perf/out/dsh-native-ab");
const STAGED_NODE = join(STAGED_DIR, "agentshim_napi.node");
const STAGED_META = join(STAGED_DIR, "agentshim_napi.node.meta.json");

function gitCommit() {
  // Read .git directly instead of spawning git: spawning through cmd.exe can
  // fail on config discovery in embedded environments, and startup cost stays
  // at two file reads.
  try {
    const head = readFileSync(join(projectRoot, ".git", "HEAD"), "utf8").trim();
    if (!head.startsWith("ref: ")) {
      return head;
    }
    const ref = head.slice(5);
    try {
      return readFileSync(join(projectRoot, ".git", ref), "utf8").trim();
    } catch {
      const packed = readFileSync(join(projectRoot, ".git", "packed-refs"), "utf8");
      const line = packed.split("\n").find((entry) => entry.endsWith(` ${ref}`));
      return line ? line.slice(0, 40) : null;
    }
  } catch {
    return null;
  }
}

function releaseAddonPath() {
  if (process.platform === "win32") {
    return resolve(projectRoot, "target/release/agentshim_napi.dll");
  }
  const extension = process.platform === "darwin" ? "dylib" : "so";
  return resolve(projectRoot, `target/release/libagentshim_napi.${extension}`);
}

function readStagedMeta() {
  try {
    return JSON.parse(readFileSync(STAGED_META, "utf8"));
  } catch {
    return null;
  }
}

// The staged addon is pinned to the working tree's commit: refresh it whenever
// a newer release build exists for a different commit, and refuse to measure a
// stale binary silently. The sidecar metadata lands in the stderr banner so
// reports can always attribute the engine build.
function loadAddon() {
  mkdirSync(STAGED_DIR, { recursive: true });
  const commit = gitCommit();
  const releasePath = releaseAddonPath();
  const meta = readStagedMeta();

  if (existsSync(releasePath)) {
    const releaseBytes = statSync(releasePath).size;
    if (meta?.source !== "target-release" || meta.commit !== commit || meta.bytes !== releaseBytes) {
      copyFileSync(releasePath, STAGED_NODE);
      writeFileSync(
        STAGED_META,
        JSON.stringify({ source: "target-release", commit, bytes: releaseBytes }, null, 2),
      );
    }
  } else if (meta?.source !== "target-release" || meta?.commit !== commit) {
    throw new Error(
      `No release build for commit ${commit}. Build it first: ` +
        "cargo build -p agentshim-napi --release",
    );
  }

  const addon = require(STAGED_NODE);
  if (typeof addon.Engine !== "function") {
    throw new Error("Staged agentshim_napi.node does not export Engine.");
  }
  const pinned = readStagedMeta();
  process.stderr.write(
    `dsh-server: addon source=${pinned?.source ?? "unknown"} ` +
      `commit=${pinned?.commit ?? "unknown"} bytes=${pinned?.bytes ?? "?"}\n`,
  );
  return addon;
}

const addon = loadAddon();
const rootDir = process.env.BENCH_CORPUS || process.cwd();

const engineOptions = {
  readScope: "normal",
  pageBudgetBytes: 500000,
};
const configuredReadOnlyCalls = Number.parseInt(process.env.BENCH_READ_ONLY_CALLS ?? "", 10);
if (Number.isInteger(configuredReadOnlyCalls)) {
  engineOptions.readOnlyCalls = configuredReadOnlyCalls;
}

// NativeHostRuntime is the single capacity owner; engines are created through
// it because the per-cwd Engine constructor is not on the public surface.
const hostRuntime = new addon.NativeHostRuntime(engineOptions);
const engine = hostRuntime.openEngine(rootDir);

// Functional API probe: a binary predating the (callId, args) engine API fails
// here on argument conversion (the lease call is missing or the first argument
// is read as the args object), instead of silently measuring an old engine.
try {
  const probeCallId = "api-probe";
  const begin = engine.beginCall(probeCallId);
  if (!begin || begin.failure || begin.value !== true) {
    throw new Error("beginCall probe rejected the lease");
  }
  try {
    const probe = engine.readText(probeCallId, {
      path: fileURLToPath(import.meta.url),
      lineCount: 1,
    });
    if (typeof probe?.then !== "function") {
      throw new Error("readText did not return a promise");
    }
  } finally {
    engine.releaseCall(probeCallId);
  }
} catch (error) {
  process.stderr.write(
    `dsh-server: engine API probe failed (${error?.message ?? error}). ` +
      "The staged addon predates the (callId, args) engine API; rebuild with: " +
      "cargo build -p agentshim-napi --release\n",
  );
  process.exit(1);
}

function send(response) {
  process.stdout.write(JSON.stringify(response) + "\n");
}

function unwrapNative(result, label) {
  if (result && result.failure) {
    const message = result.failure.message ?? JSON.stringify(result.failure);
    throw new Error(`${label}: ${message}`);
  }
  return result?.value;
}

const TOOLS = [
  {
    name: "read",
    description: "Read file contents via DSH native engine",
    inputSchema: {
      type: "object",
      properties: {
        path: { type: "string" },
        start_line: { type: "integer" },
        line_count: { type: "integer" },
        pages: { type: "string" },
        pdf_mode: { type: "string" },
      },
      required: ["path"],
    },
  },
  {
    name: "grep",
    description: "Search file contents via DSH native engine",
    inputSchema: {
      type: "object",
      properties: {
        path: { type: "string" },
        pattern: { type: "string" },
        glob: { type: "string" },
        mode: { type: "string" },
        case: { type: "string" },
        fixed_strings: { type: "boolean" },
        limit: { type: "integer" },
      },
      required: ["pattern"],
    },
  },
  {
    name: "glob",
    description: "Find files matching glob pattern via DSH native engine",
    inputSchema: {
      type: "object",
      properties: {
        path: { type: "string" },
        pattern: { type: "string" },
        type: { type: "string" },
        limit: { type: "integer" },
      },
      required: ["pattern"],
    },
  },
  {
    name: "run_program",
    description: "Execute program via DSH native engine",
    inputSchema: {
      type: "object",
      properties: {
        program: { type: "string" },
        args: { type: "array", items: { type: "string" } },
        cwd: { type: "string" },
      },
      required: ["program"],
    },
  },
];

let nextCallId = 1;

async function runTool(callId, toolName, args) {
  if (toolName === "read") {
    const result = await engine.readText(callId, {
      path: args.path,
      startLine: args.start_line ?? args.offset,
      lineCount: args.line_count ?? args.limit,
      pages: args.pages,
      pdfMode: args.pdf_mode,
    });
    return unwrapNative(result, "read")?.text ?? "";
  }
  if (toolName === "grep") {
    const result = await engine.grepText(callId, {
      pattern: args.pattern,
      path: args.path,
      glob: args.glob,
      mode: args.mode,
      case: args.case,
      fixedStrings: args.fixed_strings,
      limit: args.limit,
    });
    return unwrapNative(result, "grep")?.text ?? "";
  }
  if (toolName === "glob") {
    const result = await engine.globText(callId, {
      pattern: args.pattern,
      path: args.path,
      entryType: args.type,
      limit: args.limit,
    });
    return unwrapNative(result, "glob")?.text ?? "";
  }
  if (toolName === "run_program") {
    const prepared = unwrapNative(
      engine.prepareRunProgram(callId, {
        program: args.program,
        args: args.args ?? [],
        cwd: args.cwd,
      }),
      "prepareRunProgram",
    );
    const outcome = unwrapNative(
      await engine.spawnPrepared(callId, prepared.handle, undefined, undefined),
      "spawnPrepared",
    );
    return outcome?.text ?? `exit: ${outcome?.exit_code ?? "unknown"}`;
  }
  throw new Error(`Unknown tool: ${toolName}`);
}

// One lease per tools/call, mirroring the production wrapper's begin/release
// lifecycle. Calls run concurrently and responses are matched by JSON-RPC id,
// matching the production host's isConcurrencySafe dispatch.
async function handleToolCall(id, params) {
  const toolName = params?.name;
  const args = params?.arguments || {};
  const callId = `bench-call-${nextCallId++}`;
  let began = false;
  try {
    const begin = engine.beginCall(callId);
    if (!begin || begin.failure || begin.value !== true) {
      throw new Error(`beginCall failed: ${JSON.stringify(begin?.failure ?? begin)}`);
    }
    began = true;
    const text = await runTool(callId, toolName, args);
    send({
      jsonrpc: "2.0",
      id,
      result: {
        content: [{ type: "text", text }],
        isError: false,
      },
    });
  } catch (error) {
    if (began) {
      try {
        engine.cancelCall(callId);
      } catch {
        // The call already settled; cancellation is best effort.
      }
    }
    send({
      jsonrpc: "2.0",
      id,
      result: {
        content: [{ type: "text", text: String(error?.stack || error?.message || error) }],
        isError: true,
      },
    });
  } finally {
    if (began) {
      try {
        engine.releaseCall(callId);
      } catch {
        // Lease cleanup must never mask the response above.
      }
    }
  }
}

const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });

for await (const line of lines) {
  if (!line.trim()) continue;
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    continue;
  }

  const { id, method, params } = request;

  if (method === "tools/call") {
    void handleToolCall(id, params);
  } else if (method === "initialize") {
    send({
      jsonrpc: "2.0",
      id,
      result: {
        protocolVersion: "2024-11-05",
        capabilities: { tools: {} },
        serverInfo: { name: "dsh-agentshim-napi", version: "0.1.5" },
      },
    });
  } else if (method === "notifications/initialized" || method === "initialized") {
    // Notification
  } else if (method === "tools/list") {
    send({
      jsonrpc: "2.0",
      id,
      result: {
        tools: TOOLS,
      },
    });
  } else if (id !== undefined) {
    send({
      jsonrpc: "2.0",
      id,
      error: { code: -32601, message: `Method '${method}' not supported` },
    });
  }
}
