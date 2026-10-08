/**
 * pi's tools, served to the `claude` child as an in-process MCP server.
 *
 * Ruling 2026-10-08: "all tools run in our process", codemode included. Claude
 * Code is only the model loop; each tool call comes back here and runs the
 * same pi tool object pi's own loop would run, behind the same
 * `beforeToolCall` (the TUI's permission gate) and `afterToolCall`, emitting
 * the same `tool_execution_*` events.
 *
 * A low-level MCP `Server`, not the SDK's `createSdkMcpServer`: that one takes
 * zod schemas only, and pi tools carry JSON Schema, served here verbatim.
 * `anthropic/alwaysLoad` keeps Claude Code from deferring our tools behind its
 * tool search.
 *
 * ponytail: arguments are not schema-validated (pi's loop validates; this
 * runs `prepareArguments` only). A bad call fails inside `execute` and comes
 * back as an error result the model reads. Add validation if a tool misbehaves
 * on bad input.
 */
import { randomUUID } from "node:crypto";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { TOOL_PREFIX } from "./claude-history.ts";

export type ToolHost = {
  tools(): any[];
  agent: {
    beforeToolCall?: (ctx: any, signal?: AbortSignal) => Promise<any>;
    afterToolCall?: (ctx: any, signal?: AbortSignal) => Promise<any>;
    state: { messages: any[]; tools: any[] };
  };
  assistantFor(toolCallId: string): Promise<any>;
  signal(): AbortSignal;
  emit(event: any): void;
  onResult(message: any): void;
};

/**
 * The tools pi would declare to the model. In codemode `only` mode pi keeps
 * every tool in `state.tools` and strips the direct ones from each request.
 * ponytail: reads pi's private `_hiddenDeclarations` (pi pinned at 1.0.4); a
 * pin bump re-checks it, and `index.test.ts` fails if it moves.
 */
export function declaredTools(session: { agent: { state: { tools: any[] } } }): any[] {
  const hidden: Set<string> = (session as any)._hiddenDeclarations ?? new Set();
  return session.agent.state.tools.filter((t) => !hidden.has(t.name));
}

// pi content is MCP content already, minus pi's own fields
const mcp = (c: any) => (c.type === "image" ? { type: "image", data: c.data, mimeType: c.mimeType } : { type: "text", text: c.text });

async function call(host: ToolHost, name: string, id: string, raw: any, signal: AbortSignal) {
  const done = (content: any[], isError: boolean, details?: unknown) => {
    host.emit({ type: "tool_execution_end", toolCallId: id, toolName: name, result: { content, details }, isError });
    host.onResult({ role: "toolResult", toolCallId: id, toolName: name, content, details, isError, timestamp: Date.now() });
    return { content: content.map(mcp), isError };
  };
  const fail = (e: unknown) => done([{ type: "text", text: e instanceof Error ? e.message : String(e) }], true);
  const tool = host.tools().find((t) => t.name === name);
  if (!tool) {
    host.emit({ type: "tool_execution_start", toolCallId: id, toolName: name, args: raw });
    return fail(`Tool ${name} not found`);
  }
  try {
    const args = tool.prepareArguments?.(raw) ?? raw;
    host.emit({ type: "tool_execution_start", toolCallId: id, toolName: name, args });
    const assistantMessage = await host.assistantFor(id);
    const toolCall = { type: "toolCall", id, name, arguments: args };
    const context = { messages: host.agent.state.messages, tools: host.agent.state.tools };
    const gate = await host.agent.beforeToolCall?.({ assistantMessage, toolCall, args, context }, signal);
    if (gate?.block) return done([{ type: "text", text: gate.reason ?? "Tool call blocked" }], true);
    const r = await tool.execute(id, args, signal, (partialResult: any) =>
      host.emit({ type: "tool_execution_update", toolCallId: id, toolName: name, args, partialResult }),
    );
    let content = r?.content ?? [];
    let details = r?.details;
    let isError = !!r?.isError;
    const after = await host.agent.afterToolCall?.({ assistantMessage, toolCall, args, result: r, isError, context }, signal);
    if (after?.content) content = after.content;
    if (after?.details !== undefined) details = after.details;
    if (after?.isError !== undefined) isError = after.isError;
    return done(content, isError, details);
  } catch (e) {
    return fail(e);
  }
}

export function createToolServer(host: ToolHost): Server {
  const server = new Server({ name: "kl", version: "1" }, { capabilities: { tools: {} } });
  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: host.tools().map((t) => ({
      name: t.name,
      description: t.description,
      inputSchema: t.parameters,
      _meta: { "anthropic/alwaysLoad": true },
    })),
  }));
  server.setRequestHandler(CallToolRequestSchema, async (req, extra) => {
    const name = req.params.name.startsWith(TOOL_PREFIX) ? req.params.name.slice(TOOL_PREFIX.length) : req.params.name;
    const id = String((req.params._meta as any)?.["claudecode/toolUseId"] ?? randomUUID());
    const signal = AbortSignal.any([host.signal(), extra.signal]);
    return call(host, name, id, req.params.arguments ?? {}, signal) as any;
  });
  return server;
}
