import { expect, test } from "bun:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { claudeEnv, createClaudeSession, effortFor } from "./claude.ts";

/** A fake `query`: records its calls, and scripts a reply per pushed user message. */
function fake(reply: (n: number, push: (m: any) => void) => void) {
  const calls: any[] = [];
  const interrupts: number[] = [];
  const flags: any[] = [];
  const thinks: any[] = [];
  const query = (p: any) => {
    calls.push(p);
    const out: any[] = [];
    let wake: () => void = () => {};
    const push = (m: any) => {
      out.push(m);
      wake();
    };
    let n = 0;
    void (async () => {
      for await (const _ of p.prompt) reply(++n, push);
    })();
    return {
      async *[Symbol.asyncIterator]() {
        push({ type: "system", subtype: "init", session_id: "sid-1" });
        for (;;) {
          if (out.length) yield out.shift();
          else await new Promise<void>((r) => (wake = r));
        }
      },
      interrupt: async () => {
        interrupts.push(1);
        push({ type: "result", subtype: "error_during_execution", is_error: true });
      },
      setModel: async () => {},
      setMaxThinkingTokens: async (...a: any[]) => {
        thinks.push(a);
      },
      applyFlagSettings: async (x: any) => {
        flags.push(x);
      },
    } as any;
  };
  return { query: query as any, calls, interrupts, flags, thinks };
}

const text = (t: string, i = 0) => [
  { type: "stream_event", event: { type: "message_start", message: { id: `m${t}`, usage: { input_tokens: 3 } } } },
  { type: "stream_event", event: { type: "content_block_start", index: 0, content_block: { type: "thinking" } } },
  { type: "stream_event", event: { type: "content_block_delta", index: 0, delta: { type: "thinking_delta", thinking: "hm" } } },
  { type: "stream_event", event: { type: "content_block_stop", index: 0 } },
  { type: "stream_event", event: { type: "content_block_start", index: 1, content_block: { type: "text" } } },
  { type: "stream_event", event: { type: "content_block_delta", index: 1, delta: { type: "text_delta", text: t } } },
  { type: "stream_event", event: { type: "message_delta", usage: { output_tokens: 4 } } },
  { type: "stream_event", event: { type: "message_stop" } },
  { type: "result", subtype: "success", result: t, num: i },
];

function piHost(messages: any[] = []) {
  const recorded: any[] = [];
  const subs = new Set<(e: any) => void>();
  const tools: any[] = [];
  const host = {
    agent: { state: { messages: [...messages], tools } } as any,
    systemPrompt: "PI PROMPT",
    sessionManager: {
      appendMessage: (m: any) => recorded.push(m),
      buildSessionContext: () => ({ messages: [...messages, ...recorded] }),
    },
    subscribe: (l: (e: any) => void) => (subs.add(l), () => subs.delete(l)),
    emitPi: (e: any) => subs.forEach((l) => l(e)),
  };
  return { host, recorded, tools };
}

function run(f: ReturnType<typeof fake>, extra: object = {}) {
  const p = piHost();
  const s = createClaudeSession({ key: "k", model: { id: "claude-haiku-4-5" }, pi: p.host, query: f.query, ...extra });
  const events: any[] = [];
  s.subscribe((e) => events.push(e));
  return { s, events, p };
}
const tick = () => new Promise((r) => setTimeout(r, 20));

test("two prompts are two turns on ONE query", async () => {
  const f = fake((n, push) => text(n === 1 ? "hi" : "bye").forEach(push));
  const { s, events } = run(f);
  await s.prompt("one");
  await tick();
  await s.prompt("two");
  await tick();
  expect(f.calls.length).toBe(1);
  expect(events.filter((e) => e.type === "agent_start").length).toBe(2);
  expect(events.filter((e) => e.type === "agent_end").length).toBe(2);
  const updates = events.filter((e) => e.type === "message_update");
  expect(updates.some((e) => e.message.content.some((b: any) => b.type === "thinking" && b.thinking === "hm"))).toBe(true);
  const ends = events.filter((e) => e.type === "message_end");
  expect(ends.map((e) => e.message.content.find((b: any) => b.type === "text").text)).toEqual(["hi", "bye"]);
  expect(ends[0].message.usage.totalTokens).toBe(7);
  // unique per message: the TUI keys entries on it
  expect(new Set(ends.map((e) => e.message.timestamp)).size).toBe(2);
  s.dispose();
});

test("options: no built-in tools, our MCP server, pi's prompt, history through sessionStore", async () => {
  const f = fake((_n, push) => text("x").forEach(push));
  const p = piHost([{ role: "user", content: [{ type: "text", text: "earlier" }], timestamp: 1 }]);
  const s = createClaudeSession({ key: "k", model: { id: "m" }, pi: p.host, query: f.query });
  await s.prompt("a");
  await tick();
  const o = f.calls[0].options;
  expect(o.tools).toEqual([]);
  expect(o.mcpServers.kl).toMatchObject({ type: "sdk", name: "kl" });
  expect(o.mcpServers.kl.timeout).toBe(86_400_000);
  expect(o.systemPrompt).toBe("PI PROMPT");
  expect(o.permissionMode).toBe("bypassPermissions");
  expect(o.settingSources).toEqual([]);
  expect(o.includePartialMessages).toBe(true);
  expect(o.extraArgs["thinking-display"]).toBe("summarized");
  expect(typeof o.resume).toBe("string");
  const entries = await o.sessionStore.load({ projectKey: "x", sessionId: o.resume });
  expect(entries.map((e: any) => e.message.content)).toEqual(["earlier"]);
  expect(await o.sessionStore.load({ projectKey: "x", sessionId: "other" })).toBeNull();
  expect(await o.sessionStore.load({ projectKey: "x", sessionId: o.resume, subpath: "agent-1" })).toBeNull();
  s.dispose();
  // nothing recorded yet: no resume
  const empty = piHost();
  await createClaudeSession({ key: "k", model: { id: "m" }, pi: empty.host, query: f.query }).prompt("b");
  expect(f.calls[1].options.resume).toBeUndefined();
});

test("each finished message is recorded in pi's file and pi's state", async () => {
  const f = fake((_n, push) => text("hi").forEach(push));
  const { s, p } = run(f);
  await s.prompt("one");
  await tick();
  expect(p.recorded.map((m) => m.role)).toEqual(["user", "assistant"]);
  expect(p.recorded[0].content).toEqual([{ type: "text", text: "one" }]);
  expect(p.recorded[1]).toMatchObject({ role: "assistant", provider: "anthropic", api: "anthropic-messages", model: "claude-haiku-4-5", stopReason: "stop" });
  expect(p.recorded[1].content.find((b: any) => b.type === "text").text).toBe("hi");
  expect(p.host.agent.state.messages.map((m: any) => m.role)).toEqual(["user", "assistant"]);
  expect(s.messages).toBe(p.host.agent.state.messages);
  expect(s.agent).toBe(p.host.agent);
  s.dispose();
});


async function mcpOf(options: any) {
  const [a, b] = InMemoryTransport.createLinkedPair();
  await options.mcpServers.kl.instance.connect(a);
  const c = new Client({ name: "t", version: "1" });
  await c.connect(b);
  return c;
}

const toolTurn = (ids: string[], push: (m: any) => void, stop = true) => {
  push({ type: "stream_event", event: { type: "message_start", message: { id: `t${ids[0]}`, usage: { input_tokens: 1 } } } });
  ids.forEach((id, i) => {
    push({ type: "stream_event", event: { type: "content_block_start", index: i, content_block: { type: "tool_use", id, name: "mcp__kl__bash" } } });
    push({ type: "stream_event", event: { type: "content_block_delta", index: i, delta: { type: "input_json_delta", partial_json: '{"command":"ls"}' } } });
    push({ type: "stream_event", event: { type: "content_block_stop", index: i } });
  });
  push({ type: "stream_event", event: { type: "message_delta", delta: { stop_reason: "tool_use" }, usage: { output_tokens: 2 } } });
  if (stop) push({ type: "stream_event", event: { type: "message_stop" } });
};

test("tool_use stays in the message as a toolCall; the bridge runs the pi tool and records the result", async () => {
  let pushOut: (m: any) => void = () => {};
  const f = fake((_n, push) => {
    pushOut = push;
    toolTurn(["tu1"], push);
  });
  const { s, events, p } = run(f);
  const ran: string[] = [];
  p.tools.push({ name: "bash", description: "d", parameters: { type: "object" }, execute: async (id: string) => (ran.push(id), { content: [{ type: "text", text: "a.txt" }] }) });
  await s.prompt("go");
  await tick();
  const c = await mcpOf(f.calls[0].options);
  const r = await c.callTool({ name: "bash", arguments: { command: "ls" }, _meta: { "claudecode/toolUseId": "tu1" } });
  expect(r).toEqual({ content: [{ type: "text", text: "a.txt" }], isError: false });
  expect(ran).toEqual(["tu1"]);
  const assistant = p.recorded.find((m) => m.role === "assistant");
  expect(assistant.stopReason).toBe("toolUse");
  expect(assistant.content).toContainEqual({ type: "toolCall", id: "tu1", name: "bash", arguments: { command: "ls" } });
  expect(p.recorded.at(-1)).toMatchObject({ role: "toolResult", toolCallId: "tu1", isError: false });
  expect(events.filter((e) => e.type.startsWith("tool_execution")).map((e) => e.type)).toEqual(["tool_execution_start", "tool_execution_end"]);
  pushOut({ type: "result", subtype: "success" });
  s.dispose();
});

test("parallel calls each get their id and the same recorded assistant message", async () => {
  const f = fake((_n, push) => toolTurn(["tu1", "tu2"], push));
  const { s, p } = run(f);
  const seen: any[] = [];
  p.tools.push({ name: "bash", description: "d", parameters: { type: "object" }, execute: async () => ({ content: [] }) });
  p.host.agent.beforeToolCall = async (ctx: any) => (seen.push([ctx.toolCall.id, ctx.assistantMessage]), undefined);
  await s.prompt("go");
  await tick();
  const c = await mcpOf(f.calls[0].options);
  await Promise.all(["tu1", "tu2"].map((id) => c.callTool({ name: "bash", arguments: {}, _meta: { "claudecode/toolUseId": id } })));
  expect(seen.map((x) => x[0]).sort()).toEqual(["tu1", "tu2"]);
  expect(seen[0][1]).toBe(seen[1][1]);
  expect(seen[0][1].content.filter((b: any) => b.type === "toolCall").length).toBe(2);
  s.dispose();
});

test("a call that overtakes its assistant message waits for it to be recorded", async () => {
  let pushOut: (m: any) => void = () => {};
  const f = fake((_n, push) => {
    pushOut = push;
    toolTurn(["tu1"], push, false); // message_stop held back
  });
  const { s, p } = run(f);
  let gateSaw: any;
  p.tools.push({ name: "bash", description: "d", parameters: { type: "object" }, execute: async () => ({ content: [] }) });
  p.host.agent.beforeToolCall = async (ctx: any) => ((gateSaw = ctx.assistantMessage), undefined);
  await s.prompt("go");
  await tick();
  const c = await mcpOf(f.calls[0].options);
  const call = c.callTool({ name: "bash", arguments: {}, _meta: { "claudecode/toolUseId": "tu1" } });
  await tick();
  expect(gateSaw).toBeUndefined();
  pushOut({ type: "stream_event", event: { type: "message_stop" } });
  await call;
  expect(gateSaw.content).toContainEqual({ type: "toolCall", id: "tu1", name: "bash", arguments: { command: "ls" } });
  expect(p.host.agent.state.messages).toContain(gateSaw);
  s.dispose();
});

test("a call waiting for its assistant message gives up when the turn aborts", async () => {
  const f = fake((_n, push) => toolTurn(["tu1"], push, false)); // message_stop never comes
  const { s, p } = run(f);
  p.tools.push({ name: "bash", description: "d", parameters: { type: "object" }, execute: async () => ({ content: [] }) });
  await s.prompt("go");
  await tick();
  const c = await mcpOf(f.calls[0].options);
  let settled = false;
  const call = c.callTool({ name: "bash", arguments: {}, _meta: { "claudecode/toolUseId": "tu1" } }).then(() => (settled = true), () => (settled = true));
  await tick();
  expect(settled).toBe(false);
  await s.abort();
  await Promise.race([call, new Promise((r) => setTimeout(r, 1000))]);
  expect(settled).toBe(true);
  s.dispose();
});

test("child death mid-tool aborts execute and ends the turn with an error", async () => {
  let die: () => void = () => {};
  const f = fake((_n, push) => toolTurn(["tu1"], push));
  // a query whose iterator throws when told to: the child died
  const query = (p: any) => {
    const q = f.query(p);
    const it = q[Symbol.asyncIterator]();
    let dead: (e: Error) => void = () => {};
    const death = new Promise<never>((_r, reject) => (dead = reject));
    die = () => dead(new Error("claude exited"));
    return Object.assign(q, {
      [Symbol.asyncIterator]: () => ({ next: () => Promise.race([it.next(), death]) }),
    });
  };
  const { s, events, p } = run({ ...f, query: query as any });
  let signal: AbortSignal | undefined;
  p.tools.push({
    name: "bash", description: "d", parameters: { type: "object" },
    execute: (_id: string, _a: any, sig: AbortSignal) => ((signal = sig), new Promise((_r, rej) => sig.addEventListener("abort", () => rej(new Error("aborted"))))),
  });
  await s.prompt("go");
  await tick();
  const c = await mcpOf(f.calls[0].options);
  void c.callTool({ name: "bash", arguments: {}, _meta: { "claudecode/toolUseId": "tu1" } });
  await tick();
  die();
  await tick();
  expect(signal?.aborted).toBe(true);
  expect(events.some((e) => e.type === "message_end" && e.message.stopReason === "error")).toBe(true);
  expect(events.at(-1).type).toBe("agent_end");
  const kinds = events.map((e) => e.type);
  expect(kinds.indexOf("tool_execution_start")).toBeLessThan(kinds.indexOf("tool_execution_end"));
  expect(kinds.indexOf("tool_execution_end")).toBeLessThan(kinds.lastIndexOf("agent_end"));
  // record: user, assistant (tool_use), its one result, then the error
  expect(p.recorded.map((m) => m.role)).toEqual(["user", "assistant", "toolResult", "assistant"]);
  expect(p.recorded.filter((m) => m.role === "toolResult").length).toBe(1);
  expect(p.recorded[3].stopReason).toBe("error");
  // next prompt starts a new query resumed from pi's record
  await s.prompt("again");
  expect(f.calls.length).toBe(2);
  const entries = await f.calls[1].options.sessionStore.load({ projectKey: "x", sessionId: f.calls[1].options.resume });
  expect(entries.some((e: any) => e.type === "assistant")).toBe(true);
  s.dispose();
});

test("codemode's nested tool events from the pi session reach the TUI", async () => {
  const f = fake(() => {});
  const { s, events, p } = run(f);
  (p.host as any).emitPi({ type: "tool_execution_start", toolCallId: "n1", toolName: "bash", args: {}, parentToolCallId: "tu1" });
  (p.host as any).emitPi({ type: "tool_execution_start", toolCallId: "x", toolName: "bash", args: {} });
  (p.host as any).emitPi({ type: "message_end", message: {} });
  expect(events.map((e) => e.toolCallId)).toEqual(["n1"]);
  s.dispose();
});


test("abort interrupts and the turn is reported aborted, as one message holding the partial", async () => {
  const f = fake((_n, push) => text("part").slice(0, 6).forEach(push)); // no message_stop
  const { s, events, p } = run(f);
  await s.prompt("long");
  await tick();
  await s.abort();
  await tick();
  expect(f.interrupts.length).toBe(1);
  expect(events.some((e) => e.type === "message_end" && e.message.stopReason === "aborted")).toBe(true);
  const aborted = p.recorded.filter((m) => m.stopReason === "aborted");
  expect(aborted.length).toBe(1);
  expect(p.recorded.filter((m) => m.role === "assistant").length).toBe(1);
  expect(aborted[0].content.find((b: any) => b.type === "text").text).toBe("part");
  expect(events.at(-1).type).toBe("agent_end");
  s.dispose();
});

test("a steer held across an abort goes out as a new run, never continuing the aborted one", async () => {
  const f = fake((n, push) => (n === 1 ? text("part").slice(0, 6) : text("fresh")).forEach(push));
  const { s, events, p } = run(f);
  await s.prompt("long");
  await tick();
  const ab = s.abort();
  await s.steer("after abort"); // lands while the abort is settling
  await ab;
  await tick();
  await tick();
  const ends = events.map((e, i) => (e.type === "agent_end" ? i : -1)).filter((i) => i >= 0);
  const starts = events.map((e, i) => (e.type === "agent_start" ? i : -1)).filter((i) => i >= 0);
  expect(ends.length).toBe(2);
  expect(starts.length).toBe(2);
  expect(ends[0]).toBeLessThan(starts[1]!); // agent_end of the aborted run comes before the steer's run
  expect(p.recorded.filter((m) => m.role === "user").map((m) => m.content.at(-1).text)).toEqual(["long", "after abort"]);
  s.dispose();
});

test("follow-ups wait for the turn to end, then run on the same query", async () => {
  const f = fake((n, push) => text(`r${n}`).forEach(push));
  const { s, events } = run(f);
  await s.prompt("a");
  await s.followUp("b"); // sent before the first reply is read: held
  expect(events.some((e) => e.type === "queue_update" && e.followUp[0] === "b")).toBe(true);
  await tick();
  await tick();
  expect(events.filter((e) => e.type === "agent_end").length).toBe(2);
  expect(f.calls.length).toBe(1);
  s.dispose();
});

test("an auth failure names the laptop command", async () => {
  const f = fake((_n, push) => push({ type: "assistant", error: "authentication_failed", message: { content: [] } }));
  const { s, events } = run(f);
  await s.prompt("a");
  await tick();
  const err = events.find((e) => e.type === "message_end" && e.message.stopReason === "error");
  expect(err.message.errorMessage).toBe("Not signed in to Claude. On your laptop run: kl-connect claude login");
  s.dispose();
});

test("env drops anthropic credentials and routing, keeps the rest", () => {
  const env = claudeEnv({
    ANTHROPIC_API_KEY: "k",
    ANTHROPIC_BASE_URL: "u",
    CLAUDE_CODE_OAUTH_TOKEN: "t",
    CLAUDE_CODE_USE_BEDROCK: "1",
    CLAUDE_CODE_USE_VERTEX: "1",
    CLAUDE_CODE_USE_FOUNDRY: "1",
    PATH: "/bin",
  });
  expect(Object.keys(env).sort()).toEqual(["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "ENABLE_CLAUDEAI_MCP_SERVERS", "PATH"]);
  expect(env.ENABLE_CLAUDEAI_MCP_SERVERS).toBe("0");
});

test("thinking levels map 1:1; only minimal falls back to low; off disables thinking", () => {
  expect(["off", "minimal", "low", "medium", "high", "xhigh", "max"].map((l) => effortFor(l as never))).toEqual([
    undefined, "low", "low", "medium", "high", "xhigh", "max",
  ]);
});

test("off sends thinking disabled", async () => {
  const f = fake((_n, push) => text("x").forEach(push));
  const { s } = run(f, { thinkingLevel: "off" });
  await s.prompt("a");
  expect(f.calls[0].options.thinking).toEqual({ type: "disabled" });
  expect(f.calls[0].options.effort).toBeUndefined();
  s.dispose();
});

test("token total counts cache reads and writes", async () => {
  const f = fake((_n, push) => {
    push({ type: "stream_event", event: { type: "message_start", message: { id: "c", usage: { input_tokens: 3, cache_read_input_tokens: 10, cache_creation_input_tokens: 5 } } } });
    push({ type: "stream_event", event: { type: "message_delta", usage: { output_tokens: 4 } } });
    push({ type: "stream_event", event: { type: "message_stop" } });
    push({ type: "result", subtype: "success" });
  });
  const { s, events } = run(f);
  await s.prompt("a");
  await tick();
  const end = events.find((e) => e.type === "message_end");
  expect(end.message.usage).toMatchObject({ input: 3, output: 4, cacheRead: 10, cacheWrite: 5, totalTokens: 22 });
  s.dispose();
});

test("a steer is held until a tool result or the turn end, and clearQueue drops it", async () => {
  let pushOut: (m: any) => void = () => {};
  const f = fake((_n, push) => {
    pushOut = push;
    toolTurn(["tu1"], push);
  });
  const { s, events, p } = run(f);
  p.tools.push({ name: "bash", description: "d", parameters: { type: "object" }, execute: async () => ({ content: [] }) });
  await s.prompt("go");
  await tick();
  const pushed = () => p.recorded.filter((m) => m.role === "user").map((m) => m.content.at(-1).text);
  await s.steer("drop me");
  expect(events.at(-1)).toMatchObject({ type: "queue_update", steering: ["drop me"] });
  s.clearQueue();
  expect(events.at(-1)).toMatchObject({ type: "queue_update", steering: [], followUp: [] });
  await s.steer("keep me");
  expect(pushed()).toEqual(["go"]);
  const c = await mcpOf(f.calls[0].options);
  await c.callTool({ name: "bash", arguments: {}, _meta: { "claudecode/toolUseId": "tu1" } });
  expect(pushed()).toEqual(["go", "keep me"]);
  expect(events.filter((e) => e.type === "queue_update").at(-1)).toMatchObject({ steering: [] });
  // record order: the tool call, its result, then the steer
  expect(p.recorded.map((m) => m.role)).toEqual(["user", "assistant", "toolResult", "user"]);
  expect(events.filter((e) => e.type === "message_start" && e.message.role === "user").map((e) => e.message.content.at(-1).text)).toEqual(["keep me"]);
  pushOut({ type: "result", subtype: "success" });
  s.dispose();
});

test("a steer still held at turn end becomes the next turn", async () => {
  const f = fake((n, push) => text(`r${n}`).forEach(push));
  const { s, events, p } = run(f);
  await s.prompt("a");
  await s.steer("b");
  await tick();
  await tick();
  expect(p.recorded.filter((m) => m.role === "user").map((m) => m.content.at(-1).text)).toEqual(["a", "b"]);
  // pi keeps one run: the steer continues it, with one agent_start and one agent_end
  expect(events.filter((e) => e.type === "agent_start").length).toBe(1);
  expect(events.filter((e) => e.type === "agent_end").length).toBe(1);
  expect(events.filter((e) => e.type === "agent_end")[0]).toBe(events.at(-1));
  s.dispose();
});

const steerGate = (ids: string[]) => {
  const f = fake((n, push) => (n === 1 ? toolTurn(ids, push) : text("after").forEach(push)));
  const r = run(f);
  const gates: Record<string, () => void> = {};
  r.p.tools.push({
    name: "bash",
    description: "d",
    parameters: { type: "object" },
    execute: (id: string) => new Promise((res) => (gates[id] = () => res({ content: [{ type: "text", text: id }] }))),
  });
  let client: Promise<Awaited<ReturnType<typeof mcpOf>>> | undefined;
  const call = async (id: string) => (await (client ??= mcpOf(f.calls[0].options))).callTool({ name: "bash", arguments: {}, _meta: { "claudecode/toolUseId": id } });
  return { ...r, f, gates, call };
};

test("a steer waits for every parallel call's result, then follows them", async () => {
  const { s, p, gates, call } = steerGate(["tu1", "tu2"]);
  await s.prompt("go");
  await tick();
  const a = call("tu1");
  const b = call("tu2");
  await tick();
  await s.steer("late");
  gates.tu1();
  await a;
  await tick();
  expect(p.recorded.map((m) => m.role)).toEqual(["user", "assistant", "toolResult"]);
  gates.tu2();
  await b;
  await tick();
  expect(p.recorded.slice(0, 5).map((m) => m.role)).toEqual(["user", "assistant", "toolResult", "toolResult", "user"]);
  expect(p.recorded[4].content.at(-1).text).toBe("late");
  s.dispose();
});

test("a steer waits for sequential calls of one message too", async () => {
  const { s, p, gates, call } = steerGate(["tu1", "tu2"]);
  await s.prompt("go");
  await tick();
  await s.steer("late");
  const a = call("tu1");
  await tick();
  gates.tu1();
  await a;
  await tick();
  expect(p.recorded.map((m) => m.role)).toEqual(["user", "assistant", "toolResult"]);
  const b = call("tu2");
  await tick();
  gates.tu2();
  await b;
  await tick();
  expect(p.recorded.slice(0, 5).map((m) => m.role)).toEqual(["user", "assistant", "toolResult", "toolResult", "user"]);
  s.dispose();
});

test("an open retry is cancelled when the turn is aborted, and never carried over", async () => {
  const f = fake((_n, push) => push({ type: "system", subtype: "api_retry", attempt: 2, max_retries: 3, retry_delay_ms: 1, error_status: 529, error: "overloaded" }));
  const { s, events } = run(f);
  await s.prompt("a");
  await tick();
  expect(events.find((e) => e.type === "auto_retry_start")).toMatchObject({ attempt: 2, errorMessage: "529 overloaded" });
  await s.abort();
  await tick();
  expect(events.filter((e) => e.type === "auto_retry_end")).toEqual([{ type: "auto_retry_end", success: false, attempt: 2, finalError: "Retry cancelled" }]);
  s.dispose();
});

test("thinking off and back on reach the live query", async () => {
  const f = fake((_n, push) => text("x").forEach(push));
  const { s } = run(f);
  await s.prompt("a");
  await tick();
  s.setThinkingLevel("off");
  expect(f.thinks.at(-1)).toEqual([0, undefined]);
  s.setThinkingLevel("high");
  expect(f.thinks.at(-1)).toEqual([null, "summarized"]);
  expect(f.flags.at(-1)).toEqual({ effortLevel: "high" });
  s.dispose();
});

test("a failed compaction ends with the error; one still open at turn end is closed", async () => {
  const f = fake((n, push) => {
    push({ type: "system", subtype: "status", status: "compacting" });
    if (n === 1) push({ type: "system", subtype: "status", status: null, compact_result: "failed", compact_error: "boom" });
    text("ok").forEach(push);
  });
  const { s, events } = run(f);
  await s.prompt("a");
  await tick();
  const ends = () => events.filter((e) => e.type === "compaction_end");
  expect(ends()[0]).toMatchObject({ reason: "threshold", aborted: false, errorMessage: "Auto-compaction failed: boom" });
  await s.prompt("b");
  await tick();
  expect(ends()[1]).toMatchObject({ reason: "threshold", aborted: true });
  s.dispose();
});

test("auto-compaction toggles live and compaction and retries become pi events", async () => {
  const f = fake((_n, push) => {
    push({ type: "system", subtype: "status", status: "compacting" });
    push({ type: "system", subtype: "compact_boundary", compact_metadata: { trigger: "auto", pre_tokens: 9000, post_tokens: 1200 } });
    push({ type: "system", subtype: "api_retry", attempt: 1, max_retries: 3, retry_delay_ms: 500, error_status: 529, error: "overloaded" });
    text("ok").forEach(push);
  });
  const { s, events } = run(f);
  await s.prompt("a");
  await tick();
  expect(events.find((e) => e.type === "compaction_start")).toMatchObject({ reason: "threshold" });
  expect(events.find((e) => e.type === "compaction_end")).toMatchObject({
    reason: "threshold", aborted: false, willRetry: false, result: { tokensBefore: 9000, estimatedTokensAfter: 1200 },
  });
  expect(events.find((e) => e.type === "auto_retry_start")).toMatchObject({ attempt: 1, maxAttempts: 3, delayMs: 500, errorMessage: "529 overloaded" });
  expect(events.find((e) => e.type === "auto_retry_end")).toMatchObject({ success: true, attempt: 1 });
  s.setAutoCompactionEnabled(false);
  await tick();
  expect(f.flags.at(-1)).toEqual({ autoCompactEnabled: false });
  s.dispose();
});

test("each turn appends one timing line", async () => {
  const path = `${require("node:os").tmpdir()}/kl-timing-${process.pid}.log`;
  const f = fake((n, push) => text(n === 1 ? "hi" : "bye").forEach(push));
  const { s } = run(f, { timingLog: path });
  await s.prompt("one");
  await tick();
  await s.prompt("two");
  await tick();
  const lines = (await Bun.file(path).text()).trim().split("\n").map((l) => JSON.parse(l));
  require("node:fs").rmSync(path);
  expect(lines.length).toBe(2);
  expect(lines[0].model).toBe("claude-haiku-4-5");
  expect(lines[0].first_thinking_ms).toBeNumber();
  expect(lines[0].first_text_ms).toBeNumber();
  expect(lines[0].first_tool_ms).toBeNull();
  expect(lines[0].ok).toBe(true);
});
