import { createInterface } from "node:readline";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = fileURLToPath(new URL(".", import.meta.url));
const projectRoot = resolve(__dirname, "../../../");
const opencodePkg = resolve(projectRoot, "repos/opencode/packages");

const { Effect, Layer, ManagedRuntime } = await import(`file://${opencodePkg}/opencode/node_modules/effect/dist/index.js`.replace(/\\/g, "/"));
const { AppNodeBuilder } = await import(`file://${opencodePkg}/core/src/effect/app-node-builder.ts`.replace(/\\/g, "/"));
const { LayerNode } = await import(`file://${opencodePkg}/core/src/effect/layer-node.ts`.replace(/\\/g, "/"));
const { Location } = await import(`file://${opencodePkg}/core/src/location.ts`.replace(/\\/g, "/"));
const { PermissionV2 } = await import(`file://${opencodePkg}/core/src/permission.ts`.replace(/\\/g, "/"));
const { Config } = await import(`file://${opencodePkg}/core/src/config.ts`.replace(/\\/g, "/"));
const { ToolRegistry } = await import(`file://${opencodePkg}/core/src/tool/registry.ts`.replace(/\\/g, "/"));
const { ToolOutputStore } = await import(`file://${opencodePkg}/core/src/tool-output-store.ts`.replace(/\\/g, "/"));

const readTool = await import(`file://${opencodePkg}/core/src/tool/read.ts`.replace(/\\/g, "/"));
const grepTool = await import(`file://${opencodePkg}/core/src/tool/grep.ts`.replace(/\\/g, "/"));
const globTool = await import(`file://${opencodePkg}/core/src/tool/glob.ts`.replace(/\\/g, "/"));
const bashTool = await import(`file://${opencodePkg}/core/src/tool/bash.ts`.replace(/\\/g, "/"));

const cwd = process.env.BENCH_CORPUS || process.cwd();

const location = Layer.succeed(
	Location.Service,
	Location.Service.of({
		directory: cwd as never,
		workspaceID: "benchmark" as never,
		project: { id: "benchmark" as never, directory: cwd as never },
	}),
);
const permission = Layer.succeed(
	PermissionV2.Service,
	PermissionV2.Service.of({
		assert: () => Effect.void,
		ask: () => Effect.die("unused"),
		reply: () => Effect.die("unused"),
		get: () => Effect.die("unused"),
		forSession: () => Effect.die("unused"),
		list: () => Effect.die("unused"),
	}),
);
const outputStore = Layer.succeed(
	ToolOutputStore.Service,
	ToolOutputStore.Service.of({
		limits: () => Effect.succeed({ maxLines: 50_000, maxBytes: 50 * 1024 * 1024 }),
		bound: ({ output }: { output: string }) => Effect.succeed({ output, outputPaths: [] }),
		cleanup: () => Effect.void,
	}),
);
const config = Layer.succeed(
	Config.Service,
	Config.Service.of({
		entries: () =>
			Effect.succeed([
				{ type: "document", info: { shell: process.env.BENCH_BASH }, path: "benchmark" },
			] as never),
	}),
);

const services = AppNodeBuilder.build(
	LayerNode.group([
		ToolRegistry.node,
		ToolRegistry.toolsNode,
		readTool.node,
		grepTool.node,
		globTool.node,
		bashTool.node,
	]),
	[
		[Location.node, location],
		[PermissionV2.node, permission],
		[ToolOutputStore.node, outputStore],
		[Config.node, config],
	],
);

const runtime = ManagedRuntime.make(services);
const materialized = await runtime.runPromise(
	Effect.gen(function* () {
		return yield* (yield* ToolRegistry.Service).materialize();
	}),
);

function send(response: unknown) {
	process.stdout.write(JSON.stringify(response) + "\n");
}

const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });

for await (const line of lines) {
	if (!line.trim()) continue;
	let request: { id?: number | string; method?: string; params?: { name?: string; arguments?: Record<string, unknown> } };
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
				serverInfo: { name: "opencode-mcp-server", version: "1.0.0" },
			},
		});
	} else if (method === "notifications/initialized" || method === "initialized") {
		// Notification
	} else if (method === "tools/list") {
		send({
			jsonrpc: "2.0",
			id,
			result: {
				tools: materialized.definitions.map((def: { name: string; description: string; schema: unknown }) => ({
					name: def.name,
					description: def.description,
					inputSchema: def.schema,
				})),
			},
		});
	} else if (method === "tools/call") {
		const toolName = params?.name;
		const toolArgs = params?.arguments || {};

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
				if (normalizedArgs.glob !== undefined && normalizedArgs.include === undefined) {
					normalizedArgs.include = normalizedArgs.glob;
				}
			}

			const settlement = await runtime.runPromise(
				materialized.settle({
					sessionID: "ses_benchmark" as never,
					agent: "build" as never,
					assistantMessageID: "msg_benchmark" as never,
					call: {
						type: "tool-call",
						id: `bench-${id}`,
						name: toolName,
						input: normalizedArgs,
					},
				}),
			);

			if (settlement.result.type === "error") {
				send({
					jsonrpc: "2.0",
					id,
					result: {
						content: [{ type: "text", text: String(settlement.result.value) }],
						isError: true,
					},
				});
				continue;
			}

			const text =
				settlement.result.type === "content"
					? settlement.result.value
							.filter((item: { type: string }) => item.type === "text")
							.map((item: { text: string }) => item.text)
							.join("\n")
					: "";

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
					content: [{ type: "text", text: error instanceof Error ? error.stack ?? error.message : String(error) }],
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
