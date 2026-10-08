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
 * Calls run through pi's own `runToolCall`: argument preparation, schema
 * validation, both hooks and the abort checks are pi's, not re-implemented.
 */
import { randomUUID } from "node:crypto";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { runToolCall } from "@earendil-works/pi-agent-core";
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
  host.emit({ type: "tool_execution_start", toolCallId: id, toolName: name, args: raw });
  let outcome: any;
  try {
    const assistantMessage = await host.assistantFor(id);
    // pi's runToolCall runs the gate before its own abort check; an aborted turn must never show a permission card
    if (signal.aborted) outcome = { result: { content: [{ type: "text", text: "Operation aborted" }], details: {} }, isError: true };
    else
    outcome = await runToolCall(
      { type: "toolCall", id, name, arguments: raw },
      {
        tools: host.tools(),
        assistantMessage,
        context: { messages: host.agent.state.messages, tools: host.agent.state.tools },
        // wrapped, not passed bare: the hooks are methods and need their `this`
        beforeToolCall: (c: any, s?: AbortSignal) => host.agent.beforeToolCall?.(c, s) as any,
        afterToolCall: (c: any, s?: AbortSignal) => host.agent.afterToolCall?.(c, s) as any,
        signal,
        onUpdate: (partialResult: any) => {
          try {
            host.emit({ type: "tool_execution_update", toolCallId: id, toolName: name, args: raw, partialResult });
          } catch {} // a broken listener must not fail the tool
        },
      } as any,
    );
  } catch (e) {
    outcome = { result: { content: [{ type: "text", text: e instanceof Error ? e.message : String(e) }] }, isError: true };
  }
  const content = outcome.result?.content ?? [];
  const details = outcome.result?.details;
  const isError = !!outcome.isError;
  host.emit({ type: "tool_execution_end", toolCallId: id, toolName: name, result: { content, details }, isError });
  host.onResult({ role: "toolResult", toolCallId: id, toolName: name, content, details, isError, timestamp: Date.now() });
  return { content: content.map(mcp), isError };
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
