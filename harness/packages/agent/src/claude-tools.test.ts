import { expect, test } from "bun:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { createToolServer, declaredTools, type ToolHost } from "./claude-tools.ts";

const assistant = { role: "assistant", content: [{ type: "toolCall", id: "tu1", name: "bash", arguments: {} }] };

function host(tool: any, over: Partial<ToolHost> = {}) {
  const events: any[] = [];
  const results: any[] = [];
  const order: string[] = [];
  const turn = new AbortController();
  const h: ToolHost = {
    tools: () => [tool],
    agent: {
      state: { messages: [assistant], tools: [tool] },
      beforeToolCall: async (ctx) => {
        order.push(`before:${ctx.toolCall.id}:${ctx.assistantMessage === assistant}`);
        return undefined;
      },
      afterToolCall: async () => {
        order.push("after");
        return undefined;
      },
    },
    assistantFor: async () => assistant,
    signal: () => turn.signal,
    emit: (e) => events.push(e),
    onResult: (m) => results.push(m),
    ...over,
  };
  return { h, events, results, order, turn };
}

async function connect(h: ToolHost) {
  const [a, b] = InMemoryTransport.createLinkedPair();
  await createToolServer(h).connect(a);
  const client = new Client({ name: "t", version: "1" });
  await client.connect(b);
  return client;
}

const bash = (execute: any) => ({
  name: "bash",
  label: "bash",
  description: "run",
  parameters: { type: "object", properties: { command: { type: "string" } } },
  execute,
});

test("list serves pi's JSON schema and asks Claude Code to load it up front", async () => {
  const { h } = host(bash(async () => ({ content: [] })));
  const c = await connect(h);
  const { tools } = await c.listTools();
  expect(tools).toEqual([
    {
      name: "bash",
      description: "run",
      inputSchema: { type: "object", properties: { command: { type: "string" } } },
      _meta: { "anthropic/alwaysLoad": true },
    },
  ]);
});

test("a call runs before, execute, after in order and emits start, update, end", async () => {
  const { h, events, results, order } = host(
    bash(async (_id: string, args: any, _s: AbortSignal, onUpdate: any) => {
      order.push(`execute:${args.command}`);
      onUpdate({ content: [{ type: "text", text: "partial" }] });
      return { content: [{ type: "text", text: "a.txt", textSignature: "x" }], details: { n: 1 } };
    }),
  );
  const c = await connect(h);
  const r = await c.callTool({ name: "bash", arguments: { command: "ls" }, _meta: { "claudecode/toolUseId": "tu1" } });
  expect(order).toEqual(["before:tu1:true", "execute:ls", "after"]);
  expect(events.map((e) => e.type)).toEqual(["tool_execution_start", "tool_execution_update", "tool_execution_end"]);
  expect(events[0]).toMatchObject({ toolCallId: "tu1", toolName: "bash", args: { command: "ls" } });
  expect(events[2]).toMatchObject({ toolCallId: "tu1", isError: false, result: { content: [{ type: "text", text: "a.txt" }] } });
  expect(r).toEqual({ content: [{ type: "text", text: "a.txt" }], isError: false });
  expect(results[0]).toMatchObject({ role: "toolResult", toolCallId: "tu1", toolName: "bash", isError: false, details: { n: 1 } });
});

test("the prefixed name is stripped", async () => {
  const { h, events } = host(bash(async () => ({ content: [] })));
  const c = await connect(h);
  await c.callTool({ name: "mcp__kl__bash", arguments: {} });
  expect(events[0].toolName).toBe("bash");
});

test("a gate denial and a throw both come back as isError with the reason", async () => {
  let ran = false;
  const denied = host(bash(async () => { ran = true; return { content: [] }; }), {
    agent: { state: { messages: [], tools: [] }, beforeToolCall: async () => ({ block: true, reason: "The user rejected this tool call." }) },
  });
  const c1 = await connect(denied.h);
  expect(await c1.callTool({ name: "bash", arguments: {} })).toEqual({
    content: [{ type: "text", text: "The user rejected this tool call." }],
    isError: true,
  });
  expect(ran).toBe(false);
  const thrown = host(bash(async () => { throw new Error("boom"); }));
  const c2 = await connect(thrown.h);
  expect(await c2.callTool({ name: "bash", arguments: {} })).toEqual({ content: [{ type: "text", text: "boom" }], isError: true });
  expect(thrown.results[0].isError).toBe(true);
});

test("afterToolCall overrides content and the error flag", async () => {
  const { h } = host(bash(async () => ({ content: [{ type: "text", text: "raw" }] })), {
    agent: { state: { messages: [], tools: [] }, afterToolCall: async () => ({ content: [{ type: "text", text: "redacted" }], isError: true }) },
  });
  const c = await connect(h);
  expect(await c.callTool({ name: "bash", arguments: {} })).toEqual({ content: [{ type: "text", text: "redacted" }], isError: true });
});

test("aborting the turn reaches execute's signal", async () => {
  let seen: AbortSignal | undefined;
  const { h, turn } = host(
    bash((_id: string, _a: any, signal: AbortSignal) => {
      seen = signal;
      return new Promise((_r, reject) => signal.addEventListener("abort", () => reject(new Error("aborted"))));
    }),
  );
  const c = await connect(h);
  const call = c.callTool({ name: "bash", arguments: {} });
  await new Promise((r) => setTimeout(r, 20));
  turn.abort();
  expect(await call).toEqual({ content: [{ type: "text", text: "aborted" }], isError: true });
  expect(seen?.aborted).toBe(true);
});

test("an unknown tool is an error result, not a crash", async () => {
  const { h } = host(bash(async () => ({ content: [] })));
  const c = await connect(h);
  expect(await c.callTool({ name: "nope", arguments: {} })).toEqual({ content: [{ type: "text", text: "Tool nope not found" }], isError: true });
});

test("declaredTools drops what codemode hides from requests", () => {
  const s: any = { agent: { state: { tools: [{ name: "codemode" }, { name: "bash" }] } }, _hiddenDeclarations: new Set(["bash"]) };
  expect(declaredTools(s).map((t) => t.name)).toEqual(["codemode"]);
  expect(declaredTools({ agent: { state: { tools: [{ name: "bash" }] } } }).map((t) => t.name)).toEqual(["bash"]);
});

test("aborting while the gate awaits stops execute and returns an error", async () => {
  let ran = false;
  let release!: () => void;
  const gate = new Promise<void>((r) => (release = r));
  const { h, turn } = host(bash(async () => { ran = true; return { content: [] }; }), {
    agent: { state: { messages: [], tools: [] }, beforeToolCall: async () => { await gate; return undefined; } },
  });
  const c = await connect(h);
  const call = c.callTool({ name: "bash", arguments: {} });
  await new Promise((r) => setTimeout(r, 20));
  turn.abort();
  release();
  expect((await call).isError).toBe(true);
  expect(ran).toBe(false);
});

test("schema-invalid arguments are an error result and execute never runs", async () => {
  let ran = false;
  const tool = { ...bash(async () => { ran = true; return { content: [] }; }), parameters: { type: "object", properties: { command: { type: "string" } }, required: ["command"] } };
  const { h } = host(tool);
  const c = await connect(h);
  expect((await c.callTool({ name: "bash", arguments: {} })).isError).toBe(true);
  expect(ran).toBe(false);
});

test("an already-aborted turn never reaches the gate or execute, and still emits start and end once", async () => {
  let ran = false;
  const { h, turn, order, events, results } = host(bash(async () => { ran = true; return { content: [] }; }));
  turn.abort();
  const c = await connect(h);
  const r = await c.callTool({ name: "bash", arguments: {} });
  expect(r.isError).toBe(true);
  expect(order).toEqual([]);
  expect(ran).toBe(false);
  expect(events.map((e) => e.type)).toEqual(["tool_execution_start", "tool_execution_end"]);
  expect(results.length).toBe(1);
});
