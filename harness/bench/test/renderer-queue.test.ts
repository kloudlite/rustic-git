import { test } from "node:test";
import assert from "node:assert/strict";
import { thread } from "../../src/renderer/live.ts";

/**
 * A queued prompt is echoed where pi TAKES it, not where pi gets round to reporting its queue:
 * `queue_update` arrives after the turn has already streamed its first words, so echoing there
 * put the answer above the question and dated the question when it was delivered.
 */
const at = (ts: number) => new Date(ts).toTimeString().slice(0, 5);

test("a queued prompt lands before the answer to it, with its own time", () => {
  const t = thread("s-queue-1");
  const typed = 1789600000000;
  t.queued("run the tests", "queue");
  assert.deepEqual(t.queue.map((q) => q.text), ["run the tests"]);

  t.onEvent({ type: "agent_start" });
  t.onEvent({ type: "message_start", message: { role: "user", content: "run the tests", timestamp: typed } });
  t.onEvent({ type: "message_update", assistantMessageEvent: { type: "text_delta", delta: "running them" } });
  t.onEvent({ type: "queue_update", steering: [], followUp: [] });
  t.onEvent({ type: "agent_end" });

  assert.deepEqual(t.messages.map((m) => [m.role, m.text]), [["user", "run the tests"], ["assistant", "running them"]]);
  assert.equal(t.messages[0].ts, typed, "its own time, not when pi reported the queue");
  assert.equal(t.messages[0].at, at(typed));
  assert.deepEqual(t.queue.map((q) => q.text), [], "pi holds it no longer");
});

test("the local echo of a prompt is stamped by pi's own message, never doubled", () => {
  const t = thread("s-queue-2");
  t.sent("hello");
  assert.equal(t.messages.length, 1);
  assert.equal((t.messages[0] as { local?: true }).local, true);
  const took = 1789600100000;
  t.onEvent({ type: "message_start", message: { role: "user", content: [{ type: "text", text: "hello" }], timestamp: took } });
  assert.deepEqual(t.messages.map((m) => m.text), ["hello"], "one row, not two");
  assert.equal(t.messages[0].ts, took);
  assert.equal((t.messages[0] as { local?: true }).local, undefined);
  // message_end repeats the same message (rpc.md calls it authoritative); it must not place a second.
  t.onEvent({ type: "message_end", message: { role: "user", content: "hello", timestamp: took } });
  assert.equal(t.messages.length, 1);
});

test("a user message pi reports only at message_end is still placed", () => {
  const t = thread("s-queue-3");
  const took = 1789600200000;
  t.onEvent({ type: "message_start", message: { role: "user", content: "", timestamp: took } });
  t.onEvent({ type: "message_end", message: { role: "user", content: "[from workspace api] it is done", timestamp: took } });
  assert.deepEqual(t.messages.map((m) => [m.role, m.text]), [["user", "[from workspace api] it is done"]]);
  assert.equal(t.messages[0].ts, took);
});
