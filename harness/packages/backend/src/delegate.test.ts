import { expect, test } from "bun:test";
import { delegateTools } from "./delegate.ts";
import type { SessionHandle, SessionOpts } from "./index.ts";

const caller = { model: { provider: "p", id: "m" }, tools: [], permission: async () => ({}) } as unknown as SessionOpts;

function fake(reply = "done") {
  const subs = new Set<(e: any) => void>();
  const sent: string[] = [];
  const fire = () => {
    for (const s of subs) {
      s({ type: "message_end", message: { role: "assistant", content: [{ type: "text", text: reply }] } });
      s({ type: "agent_end", messages: [] });
    }
  };
  const h = {
    sent,
    disposed: 0,
    prompt: async (t: string) => void (sent.push(`prompt:${t}`), queueMicrotask(fire)),
    followUp: async (t: string) => void (sent.push(`followUp:${t}`), queueMicrotask(fire)),
    dispose: async () => void h.disposed++,
    subscribe: (cb: any) => (subs.add(cb), () => subs.delete(cb)),
  };
  return h as unknown as SessionHandle & { sent: string[]; disposed: number };
}

test("workspace_ask follows up a busy session, prompts an idle one, disposes only what it opened", async () => {
  const live = new Map<string, SessionHandle>();
  const busy = new Set<string>();
  const h = fake("ok");
  live.set("w1", h);
  const [ask] = delegateTools("main", undefined, { live, busy, open: async () => fake() }, caller);
  await ask!.run({ workspace: "w1", request: "a" });
  busy.add("w1");
  await ask!.run({ workspace: "w1", request: "b" });
  expect(h.sent).toEqual(["prompt:[from main session] a", "followUp:[from main session] b"]);
  expect(h.disposed).toBe(0);
  const opened = fake();
  const [ask2] = delegateTools("main", undefined, { live: new Map(), busy: new Set(), open: async () => opened }, caller);
  await ask2!.run({ workspace: "w2", request: "c" });
  expect(opened.disposed).toBe(1);
});

// ---- subagent in its own clone ----

type Rig = ReturnType<typeof rig>;
function rig(o: { branch?: string; head?: string; pushes?: { code: number; stderr: string }[]; clone?: string[]; ready?: boolean } = {}) {
  const calls: string[] = [];
  const execs: { ws: string; cmd: string }[] = [];
  const clone = [...(o.clone ?? ['{"id":"ws-c1"}'])];
  const pushes = [...(o.pushes ?? [{ code: 0, stderr: "" }])];
  let head = o.head ?? "newsha";
  const api = async (m: string, path: string, body?: any) => {
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
  const deps = { live: new Map<string, SessionHandle>(), busy: new Set<string>(), open: async (k: string) => (opened.push(k), child), api, exec, sleep: async () => {}, forget: async (ws: string) => void forgot.push(ws) };
  const [, sub] = delegateTools("main", undefined, deps, caller);
  return { calls, execs, child, opened, forgot, bodies, run: (task = "fix the bug\nmore") => sub!.run({ workspace: "P", task }) as Promise<string>, setHead: (h: string) => (head = h) };
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

test("push rejected, then accepted after one rebase prompt to the same session", async () => {
  const r = rig({ pushes: [{ code: 1, stderr: "! [rejected] HEAD -> main (non-fast-forward)" }, { code: 0, stderr: "" }] });
  expect(await r.run()).toContain("pushed abc1234");
  expect(r.child.sent.length).toBe(2);
  expect(r.child.sent[1]).toContain("git pull --rebase ssh://kl@10.0.0.5/home/kl/workspace main");
  expect(r.opened.length).toBe(1);
  expect(deleted(r)).toBe(true);
});

test("push failing twice keeps the clone and names it", async () => {
  const bad = { code: 1, stderr: "x\n! [rejected] (fetch first)" };
  const r = rig({ pushes: [bad, bad] });
  const out = await r.run();
  expect(out).toContain("push failed:");
  expect(out).toContain("clone ws-c1 kept with the commits");
  expect(deleted(r)).toBe(false);
  expect(r.forgot).toEqual([]);
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

test("a delegated session's permission request names the delegated session", async () => {
  const seen: (string | undefined)[] = [];
  const c = { ...caller, permission: async (r: any) => (seen.push(r.session), {}) } as unknown as SessionOpts;
  let given!: SessionOpts;
  const [ask] = delegateTools("main", undefined, { live: new Map(), busy: new Set(), open: async (_k, o) => ((given = o), fake()) }, c);
  await ask!.run({ workspace: "ws-a", request: "x" });
  await given.permission({ name: "bash", args: {} }, new AbortController().signal);
  await given.permission({ name: "bash", args: {}, session: "ws-a:agent-1" }, new AbortController().signal);
  expect(seen).toEqual(["ws-a", "ws-a:agent-1"]);
});
