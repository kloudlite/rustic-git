// Tests for the warm-session state machine. See sessions.ts's header for why it's shaped this
// way (one session per workspace, queue semantics, subagent routing, resume-once recovery).
import { test } from "node:test";
import assert from "node:assert/strict";
import { Sessions } from "./sessions.ts";

// A fake query(): yields an init, echoes each user message as one assistant text, then a result.
function fakeQuery(log: unknown[]) {
  return ({ prompt, options }: any) =>
    (async function* () {
      log.push(options);
      yield { type: "system", subtype: "init", session_id: "sid-1" };
      for await (const m of prompt) {
        yield { type: "assistant", parent_tool_use_id: null, message: { content: [{ type: "text", text: `echo ${m.message.content}` }] } };
        yield { type: "result" };
      }
    })();
}

test("a session gets its workspace, the mod, and the built-ins disabled", async () => {
  const log: any[] = [];
  const s = new Sessions({ query: fakeQuery(log), home: "/h", modDir: "/opt/kl/mod", saved: {} });
  s.send("ws-a", "hi");
  await s.settled("ws-a");
  const o = log[0];
  assert.equal(o.env.KL_WORKSPACE, "ws-a");
  assert.equal(o.cwd, "/h/sessions/ws-a");
  assert.deepEqual(o.plugins, [{ type: "local", path: "/opt/kl/mod" }]);
  assert.deepEqual(o.disallowedTools, ["Read", "Write", "Edit", "MultiEdit", "Bash", "Grep", "Glob", "NotebookEdit", "WebFetch"]);
});

test("a second message while busy queues and runs after", async () => {
  const s = new Sessions({ query: fakeQuery([]), home: "/h", modDir: "/m", saved: {} });
  s.send("ws-a", "one");
  s.send("ws-a", "two");
  assert.deepEqual(s.state()["ws-a"].queued, ["two"]);
  await s.settled("ws-a");
  assert.deepEqual(
    s.state()["ws-a"].lines.filter((l: string) => l.startsWith("a:")),
    ["a:echo one", "a:echo two"],
  );
  assert.equal(s.state()["ws-a"].busy, false);
});

test("subagent messages land under agents, not the parent's lines", async () => {
  const q = () =>
    (async function* (this: void) {
      yield { type: "system", subtype: "init", session_id: "s" };
      yield { type: "assistant", parent_tool_use_id: "tu-9", message: { content: [{ type: "tool_use", name: "read", input: { path: "a.rs" } }] } };
      yield { type: "result" };
    })();
  const s = new Sessions({ query: q as any, home: "/h", modDir: "/m", saved: {} });
  s.send("ws-a", "go");
  await s.settled("ws-a");
  const st = s.state()["ws-a"];
  assert.equal(st.agents[0].id, "tu-9");
  assert.deepEqual(st.agents[0].lines, ["t:read(a.rs)"]);
  assert.ok(!st.lines.includes("t:read(a.rs)"));
});

test("a failed resume starts fresh and says why, once", async () => {
  let calls = 0;
  const q = ({ options }: any) =>
    (async function* () {
      calls++;
      if (options.resume) throw new Error("No conversation found with session ID: dead");
      yield { type: "system", subtype: "init", session_id: "new" };
      yield { type: "result" };
    })();
  const s = new Sessions({ query: q as any, home: "/h", modDir: "/m", saved: { "ws-a": { lines: [], sessionId: "dead" } } });
  s.send("ws-a", "hi");
  await s.settled("ws-a");
  assert.equal(calls, 2);
  assert.ok(s.state()["ws-a"].lines.some((l: string) => l.startsWith("s:(error) resume failed")));
});

test("idle: no clients and nothing busy", () => {
  const s = new Sessions({ query: fakeQuery([]), home: "/h", modDir: "/m", saved: {} });
  assert.equal(s.busy(), false);
});

// Ruling 2: workspace tools arrive named mcp__kloudlite__<name>; the prefix is stripped for the
// `t:` line so it reads as the plain tool the workspace exposed, not the MCP plumbing name.
test("a workspace tool's mcp__kloudlite__ prefix is stripped in the t: line", async () => {
  const q = () =>
    (async function* () {
      yield { type: "system", subtype: "init", session_id: "s" };
      yield {
        type: "assistant",
        parent_tool_use_id: null,
        message: { content: [{ type: "tool_use", name: "mcp__kloudlite__read", input: { path: "a.rs" } }] },
      };
      yield { type: "result" };
    })();
  const s = new Sessions({ query: q as any, home: "/h", modDir: "/m", saved: {} });
  s.send("ws-a", "go");
  await s.settled("ws-a");
  assert.ok(s.state()["ws-a"].lines.includes("t:read(a.rs)"));
});

// Ruling 3: the SDK cannot address a running subagent directly, so a /send aimed at one is
// delivered to the parent session with a prefix naming which subagent it was meant for.
test("a send aimed at a subagent is prefixed and delivered to the parent", async () => {
  const log: string[] = [];
  const q = ({ prompt }: any) =>
    (async function* () {
      yield { type: "system", subtype: "init", session_id: "s" };
      yield {
        type: "assistant",
        parent_tool_use_id: null,
        message: { content: [{ type: "tool_use", id: "tu-5", name: "Agent", input: { description: "fixer" } }] },
      };
      yield { type: "result", parent_tool_use_id: "tu-5" };
      for await (const m of prompt) {
        log.push(m.message.content);
        yield { type: "result" };
      }
    })();
  const s = new Sessions({ query: q as any, home: "/h", modDir: "/m", saved: {} });
  s.send("ws-a", "start");
  await s.settled("ws-a");
  s.send("ws-a", "hurry up", "tu-5");
  await s.settled("ws-a");
  assert.deepEqual(log.at(-1), "[for subagent fixer] hurry up");
});
