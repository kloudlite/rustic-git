import { expect, test } from "bun:test";
import { existsSync, mkdtempSync, readdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { delegateTools, dispatchAsk, resumeAsks } from "./delegate.ts";
import type { DelegateDeps } from "./delegate.ts";
import type { SessionHandle, SessionOpts } from "./index.ts";
import { addTask, readTasks, tasksFile, updateTask } from "./tasks.ts";
import { readMessages } from "./messages.ts";

const caller = { initial: { model: { provider: "p", id: "m" } }, tools: [] } as unknown as SessionOpts;
const asks = mkdtempSync(join(tmpdir(), "kl-asks-"));
const messages = join(mkdtempSync(join(tmpdir(), "kl-msgs-")), "messages.json");
const D = (o: Partial<DelegateDeps> = {}): DelegateDeps => ({
  live: new Map(),
  busy: new Set(),
  open: async () => null as never,
  permit: async () => ({}),
  asks,
  messages,
  ...o,
});

const flush = () => new Promise((r) => setTimeout(r, 5));
const textMsg = (role: string, text: string) => ({ role, content: [{ type: "text", text }] });

/** A session that, like pi, announces the user message it was sent before answering it. */
function fake(reply = "done", o: { reject?: boolean } = {}) {
  const subs = new Set<(e: any) => void>();
  const sent: string[] = [];
  const emit = (e: any) => { for (const s of [...subs]) s(e); };
  const fire = (t: string) => {
    emit({ type: "agent_end", messages: [] }); // a run that is not ours ends first
    emit({ type: "message_end", message: textMsg("user", t) });
    emit({ type: "message_end", message: textMsg("assistant", reply) });
    emit({ type: "agent_end", messages: [] });
  };
  const h = {
    sent,
    emit,
    disposed: 0,
    prompt: async (t: string) => {
      if (o.reject) throw new Error("busy");
      sent.push(`prompt:${t}`);
      queueMicrotask(() => fire(t));
    },
    followUp: async (t: string) => void (sent.push(`followUp:${t}`), queueMicrotask(() => fire(t))),
    dispose: async () => void h.disposed++,
    subscribe: (cb: any) => (subs.add(cb), () => subs.delete(cb)),
  };
  return h as unknown as SessionHandle & { sent: string[]; disposed: number; emit: (e: any) => void };
}

/** open(): `ws` for the asked workspace key, `main` for the caller's key. */
const route = (ws: SessionHandle, main: SessionHandle) => async (k: string) => (k === "main" ? main : ws);

test("workspace_ask prompts an idle workspace, follows up a busy one, disposes its own view", async () => {
  const busy = new Set<string>();
  const idle = fake("ok");
  const main = fake();
  const [ask] = delegateTools("main", undefined, D({ busy, open: route(idle, main) }), caller);
  await ask!.run({ workspace: "w1", request: "a" });
  await flush();
  expect(idle.sent).toEqual(["prompt:[from main session] a"]);
  expect(idle.disposed).toBe(1);
  busy.add("w1");
  const hot = fake("ok");
  const [ask2] = delegateTools("main", undefined, D({ busy, open: route(hot, main) }), caller);
  await ask2!.run({ workspace: "w1", request: "b" });
  await flush();
  expect(hot.sent).toEqual(["followUp:[from main session] b"]);
  expect(hot.disposed).toBe(1);
});

test("workspace_ask returns at once, then delivers [from ws] into the caller via open()", async () => {
  const ws = fake("built");
  const main = fake();
  const busy = new Set<string>();
  const [ask] = delegateTools("main", undefined, D({ busy, open: route(ws, main) }), caller, "main");
  const r = await ask!.run({ workspace: "ws-a", request: "x" });
  expect(r).toContain("do not wait or poll");
  await flush();
  expect(main.sent).toEqual(["prompt:[from ws-a] built"]);
  expect(main.disposed).toBe(1);
  expect(ws.disposed).toBe(1);
  busy.add("main");
  await ask!.run({ workspace: "ws-a", request: "y" });
  await flush();
  expect(main.sent[1]).toBe("followUp:[from ws-a] built");
});

test("an agent_end before the user message of the ask is ignored (fake fires one first)", async () => {
  const ws = fake("right one");
  const main = fake();
  const [ask] = delegateTools("main", undefined, D({ open: route(ws, main) }), caller);
  await ask!.run({ workspace: "w", request: "q" });
  await flush();
  expect(main.sent).toEqual(["prompt:[from w] right one"]);
});

test("session_closed resolves the ask with a closed note", async () => {
  const ws = fake();
  ws.prompt = async () => void queueMicrotask(() => (ws as any).emit({ type: "session_closed" }));
  const main = fake();
  const [ask] = delegateTools("main", undefined, D({ open: route(ws, main) }), caller);
  await ask!.run({ workspace: "w", request: "q" });
  await flush();
  expect(main.sent).toEqual(["prompt:[from w] (session closed before answering)"]);
});

test("a rejected prompt on the caller falls back to followUp", async () => {
  const ws = fake("r");
  const main = fake("x", { reject: true });
  const [ask] = delegateTools("main", undefined, D({ open: route(ws, main) }), caller);
  await ask!.run({ workspace: "w", request: "q" });
  await flush();
  expect(main.sent).toEqual(["followUp:[from w] r"]);
});

test("the ask file exists while pending and is gone before delivery", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-asks-"));
  const ws = fake("r");
  let atDeliver = -1;
  const main = fake();
  main.prompt = async () => void (atDeliver = readdirSync(dir).length);
  let atOpen = -1;
  const open = async (k: string) => (k === "main" ? main : (atOpen = readdirSync(dir).length, ws));
  await dispatchAsk(D({ asks: dir, open }), { id: "a1", callerKey: "main", key: "w", text: "q", tries: 0, model: caller.initial!.model });
  expect(atOpen).toBe(1);
  expect(atDeliver).toBe(0);
});

test("resumeAsks resends a fresh ask with a prefix and tries+1; the third restart reports the loss", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-asks-"));
  const base = { callerKey: "main", key: "w", model: caller.initial!.model };
  writeFileSync(join(dir, "a.json"), JSON.stringify({ ...base, id: "a", text: "q", tries: 0 }));
  writeFileSync(join(dir, "b.json"), JSON.stringify({ ...base, id: "b", text: "z", tries: 2 }));
  const ws = fake("r");
  const main = fake();
  await Promise.all(resumeAsks(D({ asks: dir, open: route(ws, main) })));
  await flush();
  expect(ws.sent).toEqual(["prompt:[resent after restart] q"]);
  expect(main.sent.some((m) => m.includes("[from w] failed: lost in 3 bench restarts"))).toBe(true);
  expect(main.sent.some((m) => m.includes("[from w] r"))).toBe(true);
  expect(existsSync(join(dir, "a.json")) || existsSync(join(dir, "b.json"))).toBe(false);
});

test("delegateTools: main gets workspace_ask, a workspace gets main_tell", () => {
  expect(delegateTools("main", undefined, D(), caller).map((t) => t.name)).toEqual(["workspace_ask"]);
  expect(delegateTools("workspace", "ws-a", D(), caller).map((t) => t.name)).toEqual(["main_tell"]);
});

const tellOf = (deps: DelegateDeps, ws = "ws-a") => delegateTools("workspace", ws, deps, caller)[0]!;

test("main_tell delivers the tagged message into main: prompt when idle, followUp when busy", async () => {
  const main = fake();
  const busy = new Set<string>();
  const tell = tellOf(D({ busy, open: async () => main }));
  expect(await tell.run({ kind: "need", text: "x" })).toBe("told main");
  await flush();
  expect(main.sent).toEqual(["prompt:[from ws-a] need: x"]);
  busy.add("main");
  await tell.run({ kind: "blocked", text: "y" });
  await flush();
  expect(main.sent[1]).toBe("followUp:[from ws-a] blocked: y");
});

test("main_tell goes to the session that last asked the workspace, else main", async () => {
  const opened: string[] = [];
  const mk = (lastCaller?: Map<string, string>) =>
    tellOf(D({ lastCaller, open: async (k) => (opened.push(k), fake()) }));
  await mk(new Map([["ws-a", "main:2"]])).run({ kind: "need", text: "x" });
  await mk(new Map()).run({ kind: "need", text: "x" });
  await flush();
  expect(opened).toEqual(["main:2", "main"]);
});

test("main_tell with main not live opens it and prompts", async () => {
  const main = fake();
  const opened: string[] = [];
  await tellOf(D({ live: new Map(), open: async (k) => (opened.push(k), main) })).run({ kind: "need", text: "x" });
  await flush();
  expect(opened).toEqual(["main"]);
  expect(main.sent).toEqual(["prompt:[from ws-a] need: x"]);
});

test("an ask whose session calls main_tell done delivers one message, and the next ask answers again", async () => {
  const lastCaller = new Map<string, string>();
  const reported = new Set<string>();
  const main = fake();
  const ws = fake("final");
  const deps = D({ lastCaller, reported, open: route(ws, main) });
  const tell = tellOf(deps, "w");
  // the workspace tells main done in the middle of its turn
  ws.prompt = async (t: string) => {
    (ws as any).sent.push(`prompt:${t}`);
    queueMicrotask(async () => {
      await tell.run({ kind: "done", text: "shipped" });
      (ws as any).emit({ type: "message_end", message: textMsg("user", t) });
      (ws as any).emit({ type: "message_end", message: textMsg("assistant", "final") });
      (ws as any).emit({ type: "agent_end", messages: [] });
    });
  };
  const ask = { id: "a", callerKey: "main", key: "w", text: "q", tries: 0, model: caller.initial!.model };
  await dispatchAsk(deps, ask);
  await flush();
  expect(main.sent).toEqual(["prompt:[from w] done: shipped"]);
  expect(reported.has("w")).toBe(false);
  // reported is cleared: a plain ask answers normally
  const plain = fake("again");
  await dispatchAsk(D({ lastCaller, reported, open: route(plain, main) }), { ...ask, id: "b" });
  await flush();
  expect(main.sent[1]).toContain("[from w] again");
});

test("main_tell need does not suppress the final answer", async () => {
  const reported = new Set<string>();
  const main = fake();
  const ws = fake("final");
  const deps = D({ reported, open: route(ws, main) });
  const tell = tellOf(deps, "w");
  ws.prompt = async (t: string) => {
    queueMicrotask(async () => {
      await tell.run({ kind: "need", text: "a fact" });
      (ws as any).emit({ type: "message_end", message: textMsg("user", t) });
      (ws as any).emit({ type: "message_end", message: textMsg("assistant", "final") });
      (ws as any).emit({ type: "agent_end", messages: [] });
    });
  };
  await dispatchAsk(deps, { id: "a", callerKey: "main", key: "w", text: "q", tries: 0, model: caller.initial!.model });
  await flush();
  expect(main.sent[0]).toBe("prompt:[from w] need: a fact");
  expect(main.sent[1]).toContain("[from w] final");
});

test("a delegated session's permission request goes to the caller's key and names the delegated session", async () => {
  const seen: [string, string | undefined][] = [];
  let given!: SessionOpts;
  const [ask] = delegateTools("main", undefined, D({ permit: async (k, r) => (seen.push([k, r.session]), {}), open: async (_k, o) => (o.permission && (given = o), fake()) }), caller);
  await ask!.run({ workspace: "ws-a", request: "x" });
  await flush();
  await given.permission!({ name: "bash", args: {} }, new AbortController().signal);
  await given.permission!({ name: "bash", args: {}, session: "ws-a:agent-1" }, new AbortController().signal);
  expect(seen).toEqual([["main", "ws-a"], ["main", "ws-a:agent-1"]]);
});

const board = () => join(mkdtempSync(join(tmpdir(), "kl-board-")), "tasks.json");

test("workspace_ask sends only words, logs the caller's task as `for`, and runs only that queued task", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-board-"));
  const tasks = tasksFile("main", dir);
  addTask(tasks, { title: "a" });
  addTask(tasks, { title: "b" });
  updateTask(tasks, "T2", { state: "blocked" });
  const log = join(mkdtempSync(join(tmpdir(), "kl-msgs-")), "m.json");
  const ws = fake("done it");
  const main = fake();
  const [ask] = delegateTools("main", undefined, D({ messages: log, tasks: dir, open: route(ws, main) }), caller);
  await ask!.run({ workspace: "w", request: "x", for: "T1" });
  await flush();
  expect(ws.sent).toEqual(["prompt:[from main session] x"]);
  expect(readTasks(tasks).map((t) => t.state)).toEqual(["running", "blocked"]);
  // a task in any other state, or an unknown id, is left alone
  await ask!.run({ workspace: "w", request: "y", for: "T2" });
  await ask!.run({ workspace: "w", request: "z", for: "T9" });
  await flush();
  expect(readTasks(tasks).map((t) => t.state)).toEqual(["running", "blocked"]);
  const [m1, m2] = readMessages(log);
  expect(m1).toMatchObject({ from: "main", to: "w", text: "[from main session] x", for: "T1" });
  expect(m2).toMatchObject({ from: "w", to: "main", reply: m1!.id });
  expect(main.sent[0]).toBe("prompt:[from w] done it");
});

test("main_tell has no task, logs kind and the ask it answers, and never changes a board", async () => {
  const tasks = board();
  addTask(tasks, { title: "first" });
  const log = join(mkdtempSync(join(tmpdir(), "kl-msgs-")), "m.json");
  const main = fake();
  const lastAsk = new Map([["w", "abc"]]);
  const tell = tellOf(D({ messages: log, lastAsk, open: route(fake(), main) }), "w");
  await tell.run({ kind: "done", text: "shipped" });
  await flush();
  expect(main.sent[0]).toBe("prompt:[from w] done: shipped");
  expect(readTasks(tasks)[0]!.state).toBe("queued");
  expect(readMessages(log)[0]).toMatchObject({ from: "w", to: "main", kind: "done", reply: "abc" });
});

test("workspace_ask lends the caller's typed words to the workspace key before prompting", async () => {
  const lent: [string, string[]][] = [];
  const ws = fake("ok");
  const [ask] = delegateTools("main", undefined, D({ open: route(ws, fake()), typed: (k) => (k === "main" ? ["run it on port 3000"] : []), lend: (k, w) => lent.push([k, w]) }), caller);
  await ask!.run({ workspace: "w1", request: "start it" });
  await flush();
  expect(lent).toEqual([["w1", ["run it on port 3000"]]]);
});
