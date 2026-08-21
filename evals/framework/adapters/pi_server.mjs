import { createInterface } from "node:readline";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = fileURLToPath(new URL(".", import.meta.url));
const projectRoot = resolve(__dirname, "../../../");
const piModuleRoot = resolve(projectRoot, "local/perf/runtime/pi/node_modules/@earendil-works/pi-coding-agent/dist/core/tools");

const { createReadToolDefinition } = await import(`file://${piModuleRoot}/read.js`.replace(/\\/g, "/"));
const { createGrepToolDefinition } = await import(`file://${piModuleRoot}/grep.js`.replace(/\\/g, "/"));
const { createFindToolDefinition } = await import(`file://${piModuleRoot}/find.js`.replace(/\\/g, "/"));
const { createBashToolDefinition } = await import(`file://${piModuleRoot}/bash.js`.replace(/\\/g, "/"));

const rootDir = process.env.BENCH_CORPUS || process.cwd();
const readTool = createReadToolDefinition(rootDir);
const grepTool = createGrepToolDefinition(rootDir);
const findTool = createFindToolDefinition(rootDir);
const bashTool = createBashToolDefinition(rootDir);

const tools = {
  read: readTool,
  grep: grepTool,
  glob: findTool,
  find: findTool,
  bash: bashTool,
};

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
        serverInfo: { name: "pi-mcp-server", version: "0.84.2" },
      },
    });
  } else if (method === "notifications/initialized" || method === "initialized") {
    // Notification, no response required
  } else if (method === "tools/list") {
    send({
      jsonrpc: "2.0",
      id,
      result: {
        tools: [
          {
            name: "read",
            description: "Read file contents",
            inputSchema: {
              type: "object",
              properties: {
                path: { type: "string" },
                offset: { type: "integer" },
                limit: { type: "integer" },
                start_line: { type: "integer" },
                line_count: { type: "integer" },
              },
              required: ["path"],
            },
          },
          {
            name: "grep",
            description: "Search file contents with grep",
            inputSchema: {
              type: "object",
              properties: {
                path: { type: "string" },
                pattern: { type: "string" },
                glob: { type: "string" },
                literal: { type: "boolean" },
                limit: { type: "integer" },
              },
              required: ["pattern"],
            },
          },
          {
            name: "glob",
            description: "Find files matching glob pattern",
            inputSchema: {
              type: "object",
              properties: {
                path: { type: "string" },
                pattern: { type: "string" },
                limit: { type: "integer" },
              },
              required: ["pattern"],
            },
          },
          {
            name: "bash",
            description: "Execute bash command",
            inputSchema: {
              type: "object",
              properties: {
                command: { type: "string" },
                timeout_ms: { type: "integer" },
              },
              required: ["command"],
            },
          },
        ],
      },
    });
  } else if (method === "tools/call") {
    const toolName = params?.name;
    const toolArgs = params?.arguments || {};
    const toolInstance = tools[toolName];

    if (!toolInstance) {
      send({
        jsonrpc: "2.0",
        id,
        error: { code: -32601, message: `Tool '${toolName}' not found` },
      });
      continue;
    }

    try {
      const normalizedArgs = { ...toolArgs };
      if (toolName === "read") {
        if (normalizedArgs.start_line !== undefined && normalizedArgs.offset === undefined) {
          normalizedArgs.offset = normalizedArgs.start_line;
        }
        if (normalizedArgs.line_count !== undefined && normalizedArgs.limit === undefined) {
          normalizedArgs.limit = normalizedArgs.line_count;
        }
      } else if (toolName === "grep") {
        if (normalizedArgs.fixed_strings !== undefined && normalizedArgs.literal === undefined) {
          normalizedArgs.literal = normalizedArgs.fixed_strings;
        }
      }

      const result = await toolInstance.execute(`bench-${id}`, normalizedArgs);
      const text = result.content
        ?.filter((item) => item.type === "text")
        ?.map((item) => item.text)
        ?.join("\n") || "";

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
