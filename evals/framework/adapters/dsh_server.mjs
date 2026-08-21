import { createInterface } from "node:readline";
import { createRequire } from "node:module";
import { resolve, join } from "node:path";
import { fileURLToPath } from "node:url";
import { copyFileSync, existsSync, mkdirSync } from "node:fs";

const __dirname = fileURLToPath(new URL(".", import.meta.url));
const projectRoot = resolve(__dirname, "../../../");
const require = createRequire(import.meta.url);

function loadAddon() {
  const stagedDir = resolve(projectRoot, "local/perf/out/dsh-native-ab");
  const stagedNode = join(stagedDir, "agentshim_napi.node");

  if (!existsSync(stagedNode)) {
    mkdirSync(stagedDir, { recursive: true });
    const dll = process.platform === "win32"
      ? resolve(projectRoot, "target/release/agentshim_napi.dll")
      : process.platform === "darwin"
        ? resolve(projectRoot, "target/release/libagentshim_napi.dylib")
        : resolve(projectRoot, "target/release/libagentshim_napi.so");
    if (existsSync(dll)) {
      copyFileSync(dll, stagedNode);
    }
  }

  const candidates = [
    stagedNode,
    resolve(projectRoot, "adapters/dsh/npm/win32-x64-msvc/agentshim_napi.node"),
  ];

  for (const candidate of candidates) {
    if (existsSync(candidate)) {
      return require(candidate);
    }
  }
  throw new Error("Could not find agentshim_napi.node addon");
}

const addon = loadAddon();
const rootDir = process.env.BENCH_CORPUS || process.cwd();

let engine = new addon.Engine(rootDir, {
  readScope: "normal",
  pageBudgetBytes: 500000,
});

function send(response) {
  process.stdout.write(JSON.stringify(response) + "\n");
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

  if (method === "initialize") {
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
        tools: [
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
        ],
      },
    });
  } else if (method === "tools/call") {
    const toolName = params?.name;
    const args = params?.arguments || {};

    try {
      let text = "";
      if (toolName === "read") {
        const res = await engine.readText({
          path: args.path,
          startLine: args.start_line ?? args.offset,
          lineCount: args.line_count ?? args.limit,
          pages: args.pages,
          pdfMode: args.pdf_mode,
        });
        text = res?.text ?? "";
      } else if (toolName === "grep") {
        const res = await engine.grepText({
          pattern: args.pattern,
          path: args.path,
          glob: args.glob,
          mode: args.mode,
          case: args.case,
          fixedStrings: args.fixed_strings,
          limit: args.limit,
        });
        text = res?.text ?? "";
      } else if (toolName === "glob") {
        const res = await engine.globText({
          pattern: args.pattern,
          path: args.path,
          entryType: args.type,
          limit: args.limit,
        });
        text = res?.text ?? "";
      } else if (toolName === "run_program") {
        const res = await engine.runProgramText({
          program: args.program,
          args: args.args,
          cwd: args.cwd,
        });
        text = res?.text ?? "";
      } else {
        throw new Error(`Unknown tool: ${toolName}`);
      }

      send({
        jsonrpc: "2.0",
        id,
        result: {
          content: [{ type: "text", text }],
          isError: false,
        },
      });
    } catch (error) {
      send({
        jsonrpc: "2.0",
        id,
        result: {
          content: [{ type: "text", text: String(error?.stack || error?.message || error) }],
          isError: true,
        },
      });
    }
  } else if (id !== undefined) {
    send({
      jsonrpc: "2.0",
      id,
      error: { code: -32601, message: `Method '${method}' not supported` },
    });
  }
}
