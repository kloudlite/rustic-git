import { expect, test } from "bun:test";
import { claudeEnv, createClaudeSession, effortFor } from "./claude.ts";

/** A fake `query`: records its calls, and scripts a reply per pushed user message. */
function fake(reply: (n: number, push: (m: any) => void) => void) {
  const calls: any[] = [];
  const interrupts: number[] = [];
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
      applyFlagSettings: async () => {},
    } as any;
  };
  return { query: query as any, calls, interrupts };
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

const store = () => {
  let id: string | undefined;
  return { get: () => id, set: (v: string | undefined) => (id = v) };
};

function run(f: ReturnType<typeof fake>, extra: object = {}) {
  const s = createClaudeSession({ key: "k", model: { id: "claude-haiku-4-5" }, store: store(), query: f.query, ...extra });
  const events: any[] = [];
  s.subscribe((e) => events.push(e));
  return { s, events };
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

test("the claude session id is saved and options match the brief", async () => {
  const f = fake((_n, push) => text("x").forEach(push));
  const st = store();
  const s = createClaudeSession({ key: "k", model: { id: "m" }, store: st, query: f.query, thinkingLevel: "xhigh" });
  await s.prompt("a");
  await tick();
  expect(st.get()).toBe("sid-1");
  const o = f.calls[0].options;
  expect(o.includePartialMessages).toBe(true);
  expect(o.permissionMode).toBe("bypassPermissions");
  expect(o.settingSources).toEqual([]);
  expect(o.effort).toBe("max");
  expect(o.extraArgs["thinking-display"]).toBe("summarized");
  expect(o.resume).toBeUndefined();
  s.dispose();
  // a later session resumes it; /clear (fresh) drops it
  createClaudeSession({ key: "k", model: { id: "m" }, store: st, query: f.query }).prompt("b");
  expect(f.calls[1].options.resume).toBe("sid-1");
  createClaudeSession({ key: "k", model: { id: "m" }, store: st, query: f.query, fresh: true }).prompt("c");
  expect(f.calls[2].options.resume).toBeUndefined();
  expect(st.get()).toBeUndefined();
});

test("tool_use and tool_result become tool_execution_start and _end", async () => {
  const f = fake((_n, push) => {
    push({ type: "stream_event", event: { type: "message_start", message: { id: "t" } } });
    push({ type: "stream_event", event: { type: "content_block_start", index: 0, content_block: { type: "tool_use", id: "tu1", name: "Bash" } } });
    push({ type: "stream_event", event: { type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: '{"command":' } } });
    push({ type: "stream_event", event: { type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: '"ls"}' } } });
    push({ type: "stream_event", event: { type: "content_block_stop", index: 0 } });
    push({ type: "stream_event", event: { type: "message_stop" } });
    push({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: "tu1", content: "a.txt", is_error: false }] } });
    push({ type: "result", subtype: "success" });
  });
  const { s, events } = run(f);
  await s.prompt("go");
  await tick();
  const start = events.find((e) => e.type === "tool_execution_start");
  expect(start).toMatchObject({ toolCallId: "tu1", toolName: "bash", args: { command: "ls" } });
  const end = events.find((e) => e.type === "tool_execution_end");
  expect(end).toMatchObject({ toolCallId: "tu1", isError: false, result: { content: [{ type: "text", text: "a.txt" }] } });
  s.dispose();
});

test("abort interrupts and the turn is reported aborted", async () => {
  const f = fake(() => {});
  const { s, events } = run(f);
  await s.prompt("long");
  await tick();
  await s.abort();
  await tick();
  expect(f.interrupts.length).toBe(1);
  expect(events.some((e) => e.type === "message_end" && e.message.stopReason === "aborted")).toBe(true);
  expect(events.at(-1).type).toBe("agent_end");
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

test("thinking levels map to effort", () => {
  expect(["off", "minimal", "low", "medium", "high", "xhigh", "max"].map((l) => effortFor(l as never))).toEqual([
    "low", "low", "low", "medium", "high", "max", "max",
  ]);
});
