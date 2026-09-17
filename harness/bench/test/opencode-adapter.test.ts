import { strict as assert } from "node:assert";
import { test } from "node:test";
import { askAgent, partsOf, platformText, toolName, toParts } from "../../src/renderer/opencode/adapter.ts";
import type { Message } from "../../src/renderer/model.ts";

/**
 * The port's seam (spec §23): our transcript in, opencode's message and part rows out. A canned
 * turn covers every row we can produce, because the pane draws whatever comes out of here and
 * nothing else.
 */
const TURN: Message[] = [
  { role: "user", text: "why does the binding wake so often?", at: "10:00", ts: 1000 },
  { role: "assistant", text: "The watch is unfiltered…", at: "10:00", ts: 1100, kind: "reasoning" },
  { role: "action", kind: "note", tool: "grep", target: "Grep", text: "watches", args: { pattern: "watches" }, at: "10:00", ts: 1200, ok: true, output: "run.rs:24", ms: 40 },
  { role: "action", kind: "run", tool: "bash", target: "Bash", text: "cargo test", args: { command: "cargo test" }, at: "10:00", ts: 1300, pending: true },
  { role: "action", kind: "note", tool: "ask", target: "Agent", text: "audit", args: { to: "agent", name: "audit", task: "check the routes" }, at: "10:00", ts: 1400, ok: true, output: "started", ms: 5 },
  { role: "action", kind: "note", tool: "kl_workspace_create", target: "workspace create", text: "svelte", args: { name: "svelte" }, at: "10:00", ts: 1500, ok: true, output: '{"id":"ws-1"}', ms: 12 },
  { role: "action", kind: "note", tool: "edit", target: "Edit", text: "run.rs", args: { path: "run.rs" }, at: "10:00", ts: 1600, ok: false, output: "Error: edit File not found: run.rs", ms: 3 },
  { role: "question", id: "q1", tool: "write", summary: "Write run.rs", at: "10:00", ts: 1700 },
  { role: "assistant", text: "Done — one line in run.rs.", at: "10:01", ts: 1800 },
  { role: "divider", text: "Session compacted", at: "10:01", ts: 1900 },
];

test("our transcript becomes opencode's messages and parts", () => {
  const b = toParts(TURN, { session: "s-1", model: "deepseek/deepseek-v4-flash", cwd: "/home/kl" });
  // One user message, then one assistant message carrying everything that followed it.
  assert.deepEqual(b.messages.map((m) => m.role), ["user", "assistant"]);
  const user = b.messages[0];
  assert.equal(user.sessionID, "s-1");
  assert.equal(partsOf(b, user.id)[0].type, "text");
  const assistant = b.messages[1];
  assert.equal(assistant.role, "assistant");
  if (assistant.role === "assistant") {
    assert.equal(assistant.providerID, "deepseek");
    assert.equal(assistant.modelID, "deepseek-v4-flash");
    assert.equal(assistant.parentID, user.id, "their turn grouping reads the parent");
    assert.equal(assistant.path.cwd, "/home/kl");
    assert.equal(assistant.time.completed, 1900, "a finished turn carries its end");
  }
  assert.deepEqual(
    partsOf(b, assistant.id).map((p) => p.type),
    ["reasoning", "tool", "tool", "tool", "text", "tool", "tool", "text", "compaction"],
  );
});

test("every tool row lands in one of their four states", () => {
  const b = toParts(TURN, { session: "s-1" });
  const tools = b.parts.filter((p) => p.type === "tool");
  const by = (name: string) => tools.find((t) => t.type === "tool" && t.tool === name)!;
  assert.equal(by("grep").type === "tool" && by("grep").state.status, "completed");
  assert.equal(by("bash").type === "tool" && by("bash").state.status, "running", "a pending row is running, not finished");
  const edit = by("edit");
  assert.ok(edit.type === "tool" && edit.state.status === "error" && edit.state.error.includes("File not found"));
  // A live proposal is running until it is answered; the answered record carries what was said.
  const q = by("question");
  assert.ok(q.type === "tool" && q.state.status === "running");
  const answered = toParts([{ ...TURN[7], answer: "yes" } as Message], { session: "s-1" }).parts[0];
  assert.ok(answered.type === "tool" && answered.state.status === "completed" && answered.state.output === "yes");
});

test("an ask is their subagent, whoever it was sent to", () => {
  assert.equal(toolName("ask"), "task");
  assert.equal(askAgent({ to: "agent", name: "audit" }), "audit");
  assert.equal(askAgent({ to: "svelte-frontend" }), "svelte-frontend", "a workspace ask is a task with the workspace as the agent");
  const b = toParts(TURN, { session: "s-1" });
  const task = b.parts.find((p) => p.type === "tool" && p.tool === "task");
  assert.deepEqual(task?.type === "tool" ? task.metadata : undefined, { agent: "audit", background: true });
});

test("our tools map onto theirs, and a platform answer is text until it earns a part", () => {
  assert.deepEqual(["read", "ls", "find", "grep", "bash", "write", "edit", "plan", "skill"].map(toolName), [
    "read", "list", "glob", "grep", "bash", "write", "edit", "todowrite", "skill",
  ]);
  assert.equal(toolName("kl_workspace_create"), "kl_workspace_create", "an unmapped name keeps itself");
  const kl = toParts(TURN, { session: "s-1" }).parts.find((p) => p.type === "text" && p.synthetic);
  assert.ok(kl?.type === "text" && kl.text.startsWith("```json"));
  assert.equal(platformText({ output: "" } as never), "```json\n{}\n```");
});

test("a person's turn closes the assistant message before it", () => {
  const b = toParts([
    { role: "assistant", text: "one", at: "", ts: 10 },
    { role: "user", text: "stop", at: "", ts: 20 },
    { role: "assistant", text: "two", at: "", ts: 30 },
  ], { session: "s" });
  assert.deepEqual(b.messages.map((m) => m.role), ["assistant", "user", "assistant"]);
  assert.equal(b.messages[0].role === "assistant" && b.messages[0].time.completed, 20);
});

test("an interrupted answer carries their abort error", () => {
  const b = toParts([{ role: "assistant", text: "half a th", at: "", ts: 1, interrupted: true }], { session: "s" });
  const m = b.messages[0];
  assert.equal(m.role === "assistant" && m.error?.name, "MessageAbortedError");
});
