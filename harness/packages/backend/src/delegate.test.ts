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
  expect(delegateTools("workspace", "ws-a", D(), caller).map((t) => t.name)).toEqual(["main_tell", "subagent"]);
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

test("main_tell logs kind and the ask it answers, and leaves a row without an ask note alone", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-board-"));
  const tasks = tasksFile("main", dir);
  addTask(tasks, { title: "first" });
  const log = join(mkdtempSync(join(tmpdir(), "kl-msgs-")), "m.json");
  const main = fake();
  const lastAsk = new Map([["w", "abc"]]);
  const tell = tellOf(D({ messages: log, lastAsk, tasks: dir, open: route(fake(), main) }), "w");
  await tell.run({ kind: "done", text: "shipped" });
  await flush();
  expect(main.sent[0]).toBe("prompt:[from w] done: shipped");
  expect(readTasks(tasks)[0]!.state).toBe("queued");
  expect(readMessages(log)[0]).toMatchObject({ from: "w", to: "main", kind: "done", reply: "abc" });
});

test("workspace_ask without `for` adds a running row; a second ask to the same ws replaces it; for adds none", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-board-"));
  const tasks = tasksFile("main", dir);
  const [ask] = delegateTools("main", undefined, D({ messages: board(), tasks: dir, open: route(fake("ok"), fake()) }), caller);
  await ask!.run({ workspace: "w", request: "  first line\nsecond" });
  expect(readTasks(tasks)).toMatchObject([{ title: "first line", note: "ask:w", state: "running" }]);
  await ask!.run({ workspace: "v", request: "other" });
  await ask!.run({ workspace: "w", request: "again" });
  expect(readTasks(tasks).map((t) => `${t.note}:${t.state}`)).toEqual(["ask:w:done", "ask:v:running", "ask:w:running"]);
  await ask!.run({ workspace: "w", request: "x".repeat(80), for: "T2" });
  expect(readTasks(tasks)).toHaveLength(3);
  await ask!.run({ workspace: "u", request: "x".repeat(80) });
  expect(readTasks(tasks)[3]!.title).toBe(`${"x".repeat(60)}…`);
  await flush();
});

test("main_tell done|blocked closes only that ws's open ask row; need leaves it", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-board-"));
  const tasks = tasksFile("main", dir);
  for (const n of ["ask:w", "ask:x"]) addTask(tasks, { title: n, note: n });
  const tell = () => tellOf(D({ messages: board(), tasks: dir, open: route(fake(), fake()) }), "w");
  await tell().run({ kind: "need", text: "?" });
  expect(readTasks(tasks).map((t) => t.state)).toEqual(["queued", "queued"]);
  await tell().run({ kind: "blocked", text: "b" });
  expect(readTasks(tasks).map((t) => t.state)).toEqual(["blocked", "queued"]);
  await tell().run({ kind: "done", text: "d" });
  expect(readTasks(tasks).map((t) => t.state)).toEqual(["done", "queued"]);
  await flush();
});

test("workspace_ask lends the caller's typed words to the workspace key before prompting", async () => {
  const lent: [string, string[]][] = [];
  const ws = fake("ok");
  const [ask] = delegateTools("main", undefined, D({ open: route(ws, fake()), typed: (k) => (k === "main" ? ["run it on port 3000"] : []), lend: (k, w) => lent.push([k, w]) }), caller);
  await ask!.run({ workspace: "w1", request: "start it" });
  await flush();
  expect(lent).toEqual([["w1", ["run it on port 3000"]]]);
});

// ---- subagent in its own clone ----

type Rig = ReturnType<typeof rig>;
function rig(o: { bare?: boolean; branch?: string; head?: string; pushes?: { code: number; stderr: string }[]; clone?: string[]; ready?: boolean } = {}) {
  const calls: string[] = [];
  const execs: { ws: string; cmd: string }[] = [];
  const clone = [...(o.clone ?? ['{"id":"ws-c1"}'])];
  const pushes = [...(o.pushes ?? [{ code: 0, stderr: "" }])];
  let head = o.head ?? "newsha";
  // like the real api: a failed answer throws "<status>: <text>"
  const api = async (m: string, path: string, body?: any) => {
    const r = await answer(m, path, body);
    if (r.startsWith("error ")) throw new Error(r.slice(6));
    return r;
  };
  const answer = async (m: string, path: string, body?: any) => {
    calls.push(`${m} ${path}`);
    if (path.endsWith("/clone")) bodies.push(body);
    if (path.endsWith("/clone")) return clone.length > 1 ? clone.shift()! : clone[0]!;
    if (m === "DELETE") return "ok";
    if (path === "/v1/workspaces/ws-c1") return JSON.stringify({ state: o.ready === false ? "creating" : "ready" });
    if (path === "/v1/workspaces/ws-c1/tools") return JSON.stringify({ address: "10.0.0.9:7788", token: "SECRET" });
    if (path === "/v1/workspaces/P/tools") return JSON.stringify({ address: "10.0.0.5:7788", token: "SECRET" });
    return "error 404: no";
  };
  const exec = async (ws: string, cmd: string) => {
    execs.push({ ws, cmd });
    const ok = (stdout = "") => ({ code: 0, stdout, stderr: "" });
    if (cmd.includes("--abbrev-ref") && o.bare && !execs.some((e) => e.cmd.includes("--allow-empty")))
      return { code: 128, stdout: "", stderr: "fatal: not a git repository (or any of the parent directories): .git" };
    if (cmd.includes("--abbrev-ref")) return ok(`${o.branch ?? "main"}\nbasesha\n`);
    if (cmd.includes("git push")) return { stdout: "", ...pushes.shift()! };
    if (cmd.includes("rev-parse --short")) return ok("abc1234\n");
    if (cmd.includes("rev-parse HEAD")) return ok(head + "\n");
    if (cmd.includes("--name-only")) return ok("a.ts\nb.ts\n");
    return ok();
  };
  const child = fake("did it");
  const opened: string[] = [];
  const forgot: string[] = [];
  const bodies: any[] = [];
  const deps = { ...D(), open: async (k: string) => (opened.push(k), child), api, exec, sleep: async () => {}, forget: async (ws: string) => void forgot.push(ws) };
  const [, sub] = delegateTools("workspace", "P", deps, caller);
  return { calls, execs, child, opened, forgot, bodies, run: (task = "fix the bug\nmore") => sub!.run({ task }) as Promise<string>, setHead: (h: string) => (head = h) };
}
const deleted = (r: Rig) => r.calls.includes("DELETE /v1/workspaces/ws-c1");

test("clone happy path: ready, session on the clone, commit, push to the parent ip, clone deleted", async () => {
  const r = rig();
  const out = await r.run();
  expect(r.calls[0]).toBe("GET /v1/workspaces/P/tools");
  expect(r.calls).toContain("POST /v1/workspaces/P/clone");
  expect(r.opened[0]).toMatch(/^ws-c1:agent-[0-9a-f]{8}$/);
  expect(r.child.sent[0]).toContain("your own clone");
  expect(r.child.sent[0]).toContain("branch main");
  const commit = r.execs.find((e) => e.cmd.includes("commit -q"))!;
  expect(commit.ws).toBe("ws-c1");
  expect(commit.cmd).toContain("-m 'fix the bug'");
  const push = r.execs.find((e) => e.cmd.includes("git push"))!;
  expect(push.cmd).toContain("'ssh://kl@10.0.0.5/home/kl/workspace' HEAD:'main'");
  expect(deleted(r)).toBe(true);
  expect(out).toBe("did it\n\npushed abc1234 to main in P:\na.ts\nb.ts");
  expect(out).not.toContain("SECRET");
  expect(r.child.disposed).toBe(1);
  expect(r.forgot).toEqual(["ws-c1"]);
});

test("clone with no changes: no push, clone deleted", async () => {
  const r = rig({ head: "basesha" });
  expect(await r.run()).toBe("did it\n\nno code changes");
  expect(r.execs.some((e) => e.cmd.includes("git push"))).toBe(false);
  expect(deleted(r)).toBe(true);
});

test("a workspace with no git yet gets a repo and a first commit, then the clone goes ahead", async () => {
  const r = rig({ bare: true });
  expect(await r.run()).toContain("pushed");
  const start = r.execs.find((e) => e.cmd.includes("--allow-empty"))!;
  expect(start.ws).toBe("P");
  expect(start.cmd).toContain("git init -q -b main");
  expect(r.calls.some((c) => c.endsWith("/clone"))).toBe(true);
});

test("detached HEAD in the parent: error and no clone", async () => {
  const r = rig({ branch: "HEAD" });
  expect(await r.run()).toBe("error: subagent needs a git branch checked out in /home/kl/workspace of P");
  expect(r.calls.some((c) => c.endsWith("/clone"))).toBe(false);
});

test("clone 409 'already being cut' is retried", async () => {
  const r = rig({ clone: ["error 409: a snapshot is already being cut for this workspace", '{"id":"ws-c1"}'] });
  expect(await r.run()).toContain("pushed");
  expect(r.calls.filter((c) => c.endsWith("/clone")).length).toBe(2);
});

test("other clone errors come back as is", async () => {
  const r = rig({ clone: ["error 409: quota"] });
  expect(await r.run()).toBe("error 409: quota");
  expect(r.opened.length).toBe(0);
});

test("push rejected, then accepted after one rebase prompt to the same session, which owns the conflicts", async () => {
  const r = rig({ pushes: [{ code: 1, stderr: "! [rejected] HEAD -> main (non-fast-forward)" }, { code: 0, stderr: "" }] });
  expect(await r.run()).toContain("pushed abc1234");
  expect(r.child.sent.length).toBe(2);
  expect(r.child.sent[1]).toContain("git pull --rebase ssh://kl@10.0.0.5/home/kl/workspace main");
  expect(r.child.sent[1]).toContain("resolve every conflict yourself");
  expect(r.opened.length).toBe(1);
  expect(deleted(r)).toBe(true);
});

test("a moved branch is rebased up to 3 rounds by the same session, then the clone is kept and named", async () => {
  const bad = { code: 1, stderr: "x\n! [rejected] (fetch first)" };
  const r = rig({ pushes: [bad, bad, bad, bad] });
  const out = await r.run();
  expect(out).toContain("push failed:");
  expect(out).toContain("clone ws-c1 kept with the commits");
  expect(r.child.sent.length).toBe(4); // the task, then three rebase rounds
  expect(r.execs.filter((e) => e.cmd.includes("git push")).length).toBe(4);
  expect(r.opened.length).toBe(1);
  expect(deleted(r)).toBe(false);
  expect(r.forgot).toEqual([]);
});

test("the third rebase round can still land the push", async () => {
  const bad = { code: 1, stderr: "! [rejected] (non-fast-forward)" };
  const r = rig({ pushes: [bad, bad, bad, { code: 0, stderr: "" }] });
  expect(await r.run()).toContain("pushed abc1234");
  expect(r.child.sent.length).toBe(4);
  expect(deleted(r)).toBe(true);
});

test("a refusal that is not a moved branch (dirty parent) is not retried", async () => {
  const r = rig({ pushes: [{ code: 1, stderr: "remote rejected: Working directory has unstaged changes" }] });
  expect(await r.run()).toContain("clone ws-c1 kept");
  expect(r.child.sent.length).toBe(1);
});

test("clone never ready: deleted, error returned, no session", async () => {
  const r = rig({ ready: false });
  expect(await r.run()).toContain("did not become ready");
  expect(deleted(r)).toBe(true);
  expect(r.opened.length).toBe(0);
});

test("a throw before the push deletes the clone", async () => {
  const r = rig();
  r.child.prompt = async () => { throw new Error("boom"); };
  await expect(r.run()).rejects.toThrow("boom");
  expect(deleted(r)).toBe(true);
  expect(r.child.disposed).toBe(1);
});

test("the clone request carries the task's first line, cut to 72 characters", async () => {
  const r = rig();
  await r.run("fix the bug\nmore");
  expect(r.bodies).toHaveLength(1);
  expect(r.bodies[0]).toMatchObject({ task: "fix the bug" });
  expect(r.bodies[0].name).toMatch(/^sub-/);
  const long = rig();
  await long.run("x".repeat(100));
  expect(long.bodies[0].task).toBe("x".repeat(72));
});

test("no clone, no clone request", async () => {
  const r = rig({ branch: "HEAD" });
  await r.run();
  expect(r.bodies).toEqual([]);
});


test("a subagent session's permission request goes to the workspace session's key and names the subagent", async () => {
  const seen: [string, string | undefined][] = [];
  let given!: SessionOpts;
  const deps: DelegateDeps = {
    ...D({ permit: async (k, q) => (seen.push([k, q.session]), {}) }),
    open: async (_k, o) => ((given = o), fake()),
    api: async (m, p) => (p.endsWith("/clone") ? '{"id":"ws-c1"}' : p.endsWith("/tools") ? '{"address":"10.0.0.5:7788","token":"T"}' : m === "DELETE" ? "ok" : '{"state":"ready"}'),
    exec: async (_w, cmd) => ({ code: 0, stdout: cmd.includes("--abbrev-ref") ? "main\nbasesha\n" : "basesha\n", stderr: "" }),
    sleep: async () => {},
    forget: async () => {},
  };
  const [, sub] = delegateTools("workspace", "P", deps, caller, "P:work");
  await sub!.run({ task: "t" });
  await given.permission!({ name: "exec", args: {} }, new AbortController().signal);
  expect(seen[0]![0]).toBe("P:work");
  expect(seen[0]![1]).toMatch(/^ws-c1:agent-/);
});

test("workspace has subagent, main does not", () => {
  expect(delegateTools("workspace", "ws-a", D(), caller).map((t) => t.name)).toEqual(["main_tell", "subagent"]);
  expect(delegateTools("main", undefined, D(), caller).map((t) => t.name)).not.toContain("subagent");
});
