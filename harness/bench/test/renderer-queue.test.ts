import { test } from "node:test";
import assert from "node:assert/strict";
import { asksOf, exchangesOf, noteConnected, onEvent, queueOf, seedExchanges, setWorkspaceNames, tasks, thread, waitingOn } from "../../src/renderer/live.ts";

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

test("the bench's exchange events are the session's queue, and settle when the answer lands", () => {
  // Nothing rendered these: live.ts had no `exchange` case, so an ask was invisible until it was
  // answered — and the answer, arriving as a prompt, was the first sign it had ever been sent.
  const row = { ts: 1, id: "ask-1", session: "s-1", workspace: "api", dir: "out" as const, text: "[ask ask-1 from session 1] run the tests", state: "queued" };
  onEvent({ type: "exchange", row });
  assert.deepEqual(asksOf("s-1").map((e) => [e.workspace, e.state]), [["api", "queued"]]);

  // A transition carries id and state alone; the row keeps its text.
  onEvent({ type: "exchange", row: { ...row, state: "running" } });
  assert.deepEqual(asksOf("s-1").map((e) => e.state), ["running"]);
  assert.equal(asksOf("s-1")[0].text, row.text, "not overwritten by the transition");

  onEvent({ type: "exchange", row: { ...row, state: "done" } });
  assert.deepEqual(asksOf("s-1"), [], "an answered ask is not a queue any more");
  assert.deepEqual(exchangesOf("api").map((e) => e.id), ["ask-1"], "but the workspace's log keeps it");

  // A window opened mid-conversation starts from what the bench already holds.
  seedExchanges([{ ts: 2, id: "ask-2", session: "s-9", workspace: "web", dir: "out", text: "build it", state: "running" }]);
  assert.deepEqual(asksOf("s-9").map((e) => e.workspace), ["web"]);
});

test("the status line says what the turn is doing, for how long, and what it has spent", () => {
  const t = thread("s-status");
  assert.equal(t.turn(), undefined, "nothing running, nothing to say");

  t.onEvent({ type: "agent_start" });
  assert.equal(t.turn()!.verb, "Thinking");
  const since = t.turn()!.since;

  // The verb follows the work, and the clock does not restart with it.
  t.onEvent({ type: "tool_execution_start", toolCallId: "t1", toolName: "bash", args: { command: "npm test" } });
  assert.equal(t.turn()!.verb, "Running Bash");
  t.onEvent({ type: "tool_execution_start", toolCallId: "t2", toolName: "ask", args: { to: "agent", name: "svelte" } });
  assert.equal(t.turn()!.verb, "Waiting on agent svelte");
  t.onEvent({ type: "tool_execution_start", toolCallId: "t3", toolName: "ask", args: { to: "api" } });
  assert.equal(t.turn()!.verb, "Waiting on api");

  t.onEvent({ type: "message_update", usage: { totalTokens: 1234 }, assistantMessageEvent: { type: "text_delta", delta: "ok" } });
  assert.deepEqual([t.turn()!.verb, t.turn()!.tokens, t.turn()!.since], ["Writing", 1234, since]);

  t.onEvent({ type: "agent_end" });
  assert.equal(t.turn(), undefined, "the turn ended; the line goes with it");
});

/**
 * A question the bench is HOLDING must show the moment a window connects, not only when the event
 * happens to arrive. After the bench pod was recreated, `GET /proposals` had an open question and
 * the desktop showed no card at all — the person was blocked with nothing on screen
 * (owner, 2026-09-17). The bootstrap carried them all along; nothing read it.
 */
test("an open proposal at connect becomes a question in its thread", () => {
  const session = "s-1";
  onEvent({
    type: "proposal",
    row: {
      id: "q-mu5k0jez-b56s",
      session,
      tool: "question",
      summary: "Stuck sandboxes",
      question: { header: "Stuck sandboxes", options: [{ label: "Start both", description: "bring them back up" }] },
    },
  } as never);
  const rows = thread(session).messages.filter((m) => m.role === "question");
  assert.equal(rows.length, 1);
  assert.equal((rows[0] as { id: string }).id, "q-mu5k0jez-b56s");
  assert.equal((rows[0] as { answer?: string }).answer, undefined, "open, so the composer shows it");
  // The same row arriving twice is one question, not two cards.
  onEvent({ type: "proposal", row: { id: "q-mu5k0jez-b56s", session, tool: "question", summary: "Stuck sandboxes" } } as never);
  assert.equal(thread(session).messages.filter((m) => m.role === "question").length, 1);
});

/**
 * A proposal belongs to the session that raised it, whatever a window is looking at. The owner's
 * bench held one for `s-2` while the desktop showed nothing: the card lives in that thread's
 * composer, so every other surface has to SAY the thread is waiting (2026-09-17).
 */
test("an open proposal marks its own thread, whichever session it is", () => {
  onEvent({ type: "proposal", row: { id: "p-1", session: "s-2", tool: "kl_workspace_create", summary: "Create workspace new-workspace" } } as never);
  assert.equal(waitingOn("s-2"), 1, "the session that raised it");
  assert.equal(waitingOn("s-9"), 0, "and no other");
  // Answering it clears the mark, in that thread alone.
  onEvent({ type: "proposal", row: { id: "p-1", session: "s-2", tool: "kl_workspace_create", summary: "Create workspace new-workspace", answer: "yes" } } as never);
  assert.equal(waitingOn("s-2"), 0);
});

test("a turn blocked in a tool is still busy", () => {
  const t = thread("s-3");
  onEvent({ type: "agent_start", pi: "s-3" } as never);
  assert.equal(t.busy(), true);
  // Nothing streams while a proposal waits: the only event is the tool starting.
  onEvent({ type: "tool_execution_start", pi: "s-3", toolCallId: "c1", toolName: "kl_workspace_create", args: {} } as never);
  assert.equal(t.busy(), true, "a person is waiting on this turn, so the composer must not send plainly");
  onEvent({ type: "agent_end", pi: "s-3", messages: [] } as never);
  assert.equal(t.busy(), false);
});

/**
 * A call waiting on the PERSON is not a background task: the owner's panel listed
 * `question {"header":"Clear stuck s…` as "Lost · 1m 11s", because nothing but an answer would
 * ever end it (2026-09-17).
 */
test("a question makes no task row; work that runs does", () => {
  const before = tasks.length;
  onEvent({ type: "tool_execution_start", pi: "s-4", toolCallId: "q1", toolName: "question", args: { header: "Stuck sandboxes" } } as never);
  assert.equal(tasks.length, before, "a question is the card in the composer, not a task");
  onEvent({ type: "tool_execution_start", pi: "s-4", toolCallId: "b1", toolName: "bash", args: { command: "npm run dev" } } as never);
  assert.equal(tasks.length, before + 1);
  assert.equal(tasks[tasks.length - 1].tool, "Bash");
  assert.equal(tasks[tasks.length - 1].session, "s-4", "and it belongs to the session that started it");
});

/**
 * The owner's "frontend" session opened on its last few events: the first prompt — the one that
 * asked for the TS Node project — and everything before the turn summary were simply absent. The
 * replay must keep every real prompt; only a slash line and a card's own bare answer are dropped.
 */
test("a replayed session keeps its first prompt", () => {
  const t = thread("w-visible-1");
  t.replay([
    { role: "user", content: "create a TS Node project", timestamp: 1 },
    { role: "assistant", content: [{ type: "toolCall", id: "c1", name: "bash", arguments: { command: "ls" } }], timestamp: 2 },
    { role: "assistant", content: [{ type: "toolCall", id: "c2", name: "read", arguments: { path: "a.ts" } }], timestamp: 3 },
    { role: "assistant", content: [{ type: "toolCall", id: "c3", name: "write", arguments: { path: "b.ts" } }], timestamp: 4 },
    { role: "assistant", content: [{ type: "text", text: "made it" }], timestamp: 5 },
    { role: "assistant", content: [{ type: "toolCall", id: "c4", name: "question", arguments: { header: "which" } }], timestamp: 6 },
    { role: "user", content: "yes", timestamp: 7 },
  ]);
  assert.equal(t.messages[0].role, "user", "the first row is the prompt that started it");
  assert.equal((t.messages[0] as { text: string }).text, "create a TS Node project");
  assert.equal(t.messages.filter((m) => m.role === "user").length, 1, "the card's own `yes` is not a second row");
  assert.equal(t.messages.filter((m) => m.role === "action").length, 3, "the question is a card, not a fourth tool row");
});

/**
 * Replayed, a `question` drew as ``● Called `question` … 305.4s`` — the question said a second time
 * under a clock nobody was waiting on (owner, 2026-09-18). The live path has suppressed it since
 * b1119b9a; the fold does the same, and the recorded answer becomes the settled card.
 */
test("a replayed question is the answered card, never a tool row", () => {
  const t = thread("w-replay-q");
  t.replay([
    { role: "user", content: "start", timestamp: 1 },
    {
      role: "assistant",
      content: [{ type: "toolCall", id: "q1", name: "question", arguments: { header: "Which one?", question: "Which one?", options: [{ label: "a", description: "" }] } }],
      timestamp: 2,
    },
    { role: "toolResult", toolCallId: "q1", content: [{ type: "text", text: "a" }], timestamp: 3 },
  ]);
  assert.ok(!t.messages.some((m) => m.role === "action" && (m as { tool?: string }).tool === "question"), "no `Called question` row");
  const card = t.messages.find((m) => m.role === "question") as { answer?: string; ask?: { header: string } } | undefined;
  assert.ok(card, "the card is there");
  assert.equal(card?.answer, "a", "settled with what the person answered");
  assert.equal(card?.ask?.header, "Which one?");
});

/**
 * The question card is drawn from the bench's proposal event, which carries only a session id —
 * so it works the same on a WORKSPACE thread as on the bench's own, and the generic
 * `Called \`question\`` row is never drawn beside it.
 */
test("a question on a workspace session is a card, not a tool row", () => {
  const ws = "w-ce63ce4079301bd9";
  onEvent({ type: "tool_execution_start", toolName: "question", toolCallId: "t1", args: { header: "which new project" }, pi: ws } as never);
  onEvent({ type: "proposal", row: { id: "q-ws", session: ws, tool: "question", summary: "which new project?", question: { header: "which", options: [{ label: "a", description: "" }] } } } as never);
  const rows = thread(ws).messages;
  assert.ok(rows.some((m) => m.role === "question"), "the card is drawn on the workspace thread");
  assert.ok(!rows.some((m) => m.role === "action" && (m as { tool?: string }).tool === "question"), "and no generic `Called question` row beside it");
});

/**
 * The `/events` socket was reaped by the Cloudflare edge every 79–136 s (nothing pinged on that
 * path), and each reconnect painted "bench offline" for the fraction of a second it took — about
 * 25 times an hour on a pod that was never down. A drop has to LAST to be an outage.
 */
test("a reconnect never paints offline; a real outage still does", async () => {
  const seen: boolean[] = [];
  const apply = (v: boolean) => void seen.push(v);
  // The flicker: down and straight back up inside the window.
  noteConnected(false, apply, 40);
  noteConnected(true, apply, 40);
  await new Promise((r) => setTimeout(r, 80));
  assert.deepEqual(seen, [true], "up is immediate, and it cancelled the pending offline");

  // A drop that lasts is reported, once.
  seen.length = 0;
  noteConnected(false, apply, 40);
  await new Promise((r) => setTimeout(r, 80));
  assert.deepEqual(seen, [false]);

  // And the default `apply` resolves at call time, so the real signal is reachable.
  assert.doesNotThrow(() => noteConnected(true));
});

/**
 * The inspector's Queue said "This session has not sent anything to a workspace yet" for s-7 while
 * `GET /exchanges?session=s-7` answered a row. It read `workspace.queue`, which `toWorkspace()`
 * always fills with `[]` — so it was empty for EVERY session. The rows live in the exchange store.
 */
test("a session with one out-exchange renders one queue row", () => {
  setWorkspaceNames([{ id: "ws-632cf9f23d9f2fbf", name: "backend" }]);
  seedExchanges([
    { id: "info-1-mu5yn8wz", session: "s-7", workspace: "ws-632cf9f23d9f2fbf", dir: "out", text: "what is in this workspace now", state: "pending", ts: 2 },
    // Another session's row, and an inbound one: neither is this session's queue.
    { id: "x-2", session: "s-8", workspace: "ws-632cf9f23d9f2fbf", dir: "out", text: "not mine", state: "pending", ts: 3 },
    { id: "x-3", session: "s-7", workspace: "ws-632cf9f23d9f2fbf", dir: "in", text: "a reply", state: "done", ts: 4 },
  ]);
  const q = queueOf("s-7");
  assert.equal(q.length, 1, "one row: this session's outbound exchange");
  assert.equal(q[0].text, "what is in this workspace now");
  assert.equal(q[0].workspace, "backend", "named, not an unreadable id");
  assert.equal(q[0].dir, "out");
  assert.match(q[0].at, /^\d\d:\d\d$/, "the store keeps a timestamp; the row renders a clock");
});

/** An `info-*` row is an ask like any other, and a finished one still belongs in the record. */
test("the queue keeps info asks and finished rows, in the order they happened", () => {
  seedExchanges([
    { id: "b", session: "s-71", workspace: "ws-1", dir: "out", text: "second", state: "done", ts: 20 },
    { id: "a", session: "s-71", workspace: "ws-1", dir: "out", text: "first", state: "working", ts: 10 },
    { id: "info-c", session: "s-71", workspace: "ws-1", dir: "out", text: "third", state: "pending", ts: 30 },
  ]);
  const q = queueOf("s-71");
  assert.deepEqual(q.map((x) => x.text), ["first", "second", "third"], "oldest first");
  assert.deepEqual(q.map((x) => x.state), ["working", "done", "pending"]);
});
