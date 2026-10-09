import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { LocalBackend, ALWAYS_ASK, asking, baseHandle, mustAsk, roleCard, shareable, installGate, registryFor, sessionCwd, sessionKind } from "./local.ts";
import { toolDiff } from "./diff.ts";
import { PROTOCOL } from "./wire.ts";

test("hello carries what the TUI reads at boot", async () => {
  const h = await new LocalBackend().hello();
  expect(h.protocol).toBe(PROTOCOL);
  expect(h.cwd).toBe(process.cwd());
  expect(h.tools).toContain("workspace_create");
  expect(h.tools).toContain("workspace_ask");
  expect(h.tools).toContain("bash");
  expect(h.tools.slice(0, 2)).toEqual(["web_fetch", "web_search"]);
  expect(h.catalog.length).toBeGreaterThan(0);
  expect(h.catalog[0]).toHaveProperty("input");
  expect(h.defaultModel).toHaveProperty("provider");
});

test("settings write lands in the next hello", async () => {
  const b = new LocalBackend();
  await b.settings.write({ theme: "kloudlite-light" });
  expect((await b.hello()).settings.theme).toBe("kloudlite-light");
});

test("fs wraps git.ts", async () => {
  const b = new LocalBackend();
  expect(typeof (await b.fs.isGitRepo(process.cwd()))).toBe("boolean");
  expect(Array.isArray(await b.fs.listDir(process.cwd(), ""))).toBe(true);
});

test("house actions always ask; file edits never do", () => {
  const house = ["workspace_stop", "workspace_delete", "worktree_drop", "env_delete", "env_stop", "env_restore_in_place", "service_remove", "volume_delete", "snapshot_delete", "container_push", "container_build", "packages_remove", "service_update", "intercept"];
  expect(ALWAYS_ASK.size).toBe(14);
  const walls = { sandbox: "active", network: "fenced" };
  for (const n of house) expect([mustAsk(n, walls, "fenced"), mustAsk(n)]).toEqual([true, true]);
  for (const n of ["write", "edit", "patch", "read"]) expect(mustAsk(n)).toBe(false);
});

test("exec asks unless the pod reports both walls", () => {
  expect(mustAsk("exec")).toBe(true);
  expect(mustAsk("exec", {})).toBe(true);
  expect(mustAsk("exec", { sandbox: "unavailable", network: "fenced" })).toBe(true);
  expect(mustAsk("exec", { sandbox: "active", network: "open" })).toBe(true);
  expect(mustAsk("exec", { sandbox: "active", network: "fenced" })).toBe(false);
});

test("bash and web_fetch ask unless KLOUDLITE_EGRESS is fenced", () => {
  for (const n of ["bash", "web_fetch"]) expect([mustAsk(n, undefined, undefined), mustAsk(n, undefined, "open"), mustAsk(n, undefined, "fenced")]).toEqual([true, true, false]);
});

test("pod edit ({old,new}) has no local diff; pi edit still does", () => {
  expect(toolDiff("edit", { path: "a", edits: [{ old: "x", new: "y" }] })).toBeNull();
});

test("sessionKind", () => {
  expect(sessionKind("main")).toEqual({ kind: "main" });
  expect(sessionKind("main:x")).toEqual({ kind: "main" });
  expect(sessionKind("ws1")).toEqual({ kind: "workspace", ws: "ws1" });
  expect(sessionKind("ws1:x")).toEqual({ kind: "workspace", ws: "ws1" });
  expect(sessionKind("ws1:agent-ab12")).toEqual({ kind: "workspace", ws: "ws1" });
});

test("workspace sessions run in the pod's folder, main does not", () => {
  expect(sessionCwd(sessionKind("ws1"))).toBe("/home/kl/workspace");
  expect(sessionCwd(sessionKind("main"))).toBeUndefined();
});

test("registry per session kind", async () => {
  delete process.env.KL_API_URL; // pod tools fall back to the fixed names
  const deps = { live: new Map(), busy: new Set<string>(), open: async () => null as never, permit: async () => ({}) };
  const opts: any = { tools: [{ name: "question", description: "", inputSchema: {}, run: async () => "" }] };
  const main = (await registryFor({ kind: "main" }, deps, opts)).names();
  expect(main).toContain("workspace_create");
  expect(main).toContain("workspace_ask");
  expect(main).toContain("question");
  for (const n of ["bash", "read", "write"]) expect(main).toContain(n);
  expect(main).not.toContain("exec");
  const wsReg = await registryFor({ kind: "workspace", ws: "w1" }, deps, opts);
  const ws = wsReg.names();
  expect(ws).toContain("exec");
  expect(ws).toContain("packages_add");
  expect(ws).not.toContain("workspace_create");
  expect(ws).not.toContain("workspace_ask");
  expect(wsReg.get("read").description).not.toContain("scratch");
  expect(ws).toContain("main_tell");
  expect((wsReg.get("packages_add").inputSchema as any).properties.workspace).toBeUndefined();
  // the self-stop carries no card fields; main's still names the workspace and says why
  const stop: any = wsReg.get("workspace_stop").inputSchema;
  expect(stop.properties.because).toBeUndefined();
  expect(stop.properties.workspace).toBeUndefined();
  const mstop: any = (await registryFor({ kind: "main" }, deps, opts)).get("workspace_stop").inputSchema;
  expect(mstop.required).toEqual(["workspace", "because"]);
  expect(ws).toContain("intercept");
});

test("main keeps only the orchestrator's tools", async () => {
  delete process.env.KL_API_URL;
  const deps = { live: new Map(), busy: new Set<string>(), open: async () => null as never, permit: async () => ({}) };
  const main = (await registryFor({ kind: "main" }, deps, { tools: [] } as any)).names();
  for (const n of ["packages_add", "packages_remove", "packages_update", "intercept", "release", "subagent"]) expect(main).not.toContain(n);
  for (const n of ["packages_list", "workspace_create", "workspace_delete", "workspace_ask"]) expect(main).toContain(n);
});

test("a workspace's own workspace_stop never asks; main's does", () => {
  expect(mustAsk("workspace_stop", undefined, "open", "workspace")).toBe(false);
  expect(mustAsk("workspace_stop", undefined, "open", "main")).toBe(true);
});

/** An agent that records what reached `prompt`. */
function promptAgent(messages: unknown[]) {
  const seen: string[] = [];
  return { seen, messages, subscribe: () => () => {}, prompt: async (t: string) => void (seen.push(t), messages.push({ role: "user" })), dispose() {} };
}
const handleOf = (agent: any, key: string) => baseHandle(agent, key, { busy: new Set(), onEnd() {}, onDispose() {} });

test("the role card rides the first prompt only, and never a resumed session's", async () => {
  const fresh = promptAgent([]);
  const h = handleOf(fresh, "main");
  await h.prompt("hi");
  await h.prompt("again");
  expect(fresh.seen[0]!.startsWith("[role: main session]")).toBe(true);
  expect(fresh.seen[0]!.endsWith("hi")).toBe(true);
  expect(fresh.seen[1]).toBe("again");
  const resumed = promptAgent([{ role: "user" }]);
  await handleOf(resumed, "main").prompt("hi");
  expect(resumed.seen).toEqual(["hi"]);
  const ws = promptAgent([]);
  await handleOf(ws, "ws-a").prompt("hi");
  expect(ws.seen[0]!.startsWith("[role: workspace session for ws-a]")).toBe(true);
  expect(roleCard("ws-a:x")).toContain("workspace session for ws-a");
});

test("sessions.list offers every key", async () => {
  process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  delete process.env.KL_API_URL;
  const { models } = await import("@kloudlite-tui/agent");
  const m = models.getModels().find((x: any) => x.provider !== "anthropic") as any;
  const b = new LocalBackend();
  const o: any = { model: { provider: m.provider, id: m.id }, fresh: true, tools: [], permission: async () => ({}) };
  const h = await b.session("w9", o);
  const keys = (await b.sessions.list()).map((s) => s.key);
  expect(keys).toContain("w9");
  await h.dispose();
}, 20000);

test("an internal open (no TUI tools) reuses a live session without disposing it", async () => {
  process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  delete process.env.KL_API_URL;
  const { models } = await import("@kloudlite-tui/agent");
  const ms = models.getModels().filter((x: any) => x.provider !== "anthropic") as any[];
  const b = new LocalBackend();
  const o: any = { model: { provider: ms[0].provider, id: ms[0].id }, fresh: true, tools: [], permission: async () => ({}) };
  const h = await b.session("w8", o);
  const events: string[] = [];
  h.subscribe((e) => void events.push(e.type));
  const other = ms.find((x) => x.id !== ms[0].id) ?? ms[0];
  const h2 = await b.session("w8", { model: { provider: other.provider, id: other.id }, tools: [] } as any);
  expect(events).not.toContain("session_closed");
  expect(h2).toBeDefined();
  await h2.dispose();
  await h.dispose();
}, 20000);

test("the gate asks for a codemode script's nested gated call, once for a top-level one", async () => {
  const seen: string[] = [];
  const pi: any = {
    agent: {} as any,
    async _beforeToolCall(_ctx: any, _parent?: string) {
      seen.push("pi");
      return undefined;
    },
  };
  pi.agent.beforeToolCall = (ctx: any) => pi._beforeToolCall(ctx);
  const asked: string[] = [];
  installGate(pi, async (req) => {
    asked.push(req.name);
    return req.name === "exec" ? { block: true, reason: "no" } : {};
  });
  const call = (name: string) => ({ toolCall: { name }, args: {} });
  // nested: pi's runner calls _beforeToolCall with the parent id, never agent.beforeToolCall
  expect(await pi._beforeToolCall(call("exec"), "cm1")).toEqual({ block: true, reason: "no" });
  expect(await pi._beforeToolCall(call("read"), "cm1")).toBeUndefined();
  // top level: asked once, then falls through to pi's own hook
  await pi.agent.beforeToolCall(call("bash"));
  expect(asked).toEqual(["exec", "bash"]);
  expect(seen).toEqual(["pi", "pi"]);
});

test("shareable: a view going away leaves the others; onZero runs each time the count drops to 0", async () => {
  let subs = 0, unsubs = 0, zero = 0;
  const base = { subscribe: () => (subs++, () => void unsubs++) } as any;
  const { view, count } = shareable(base, () => void zero++);
  const a = view(), b = view();
  a.subscribe(() => {});
  b.subscribe(() => {});
  expect(count()).toBe(2);
  await a.dispose();
  expect([subs, unsubs, zero, count()]).toEqual([2, 1, 0, 1]);
  await a.dispose();
  expect([unsubs, zero]).toEqual([1, 0]);
  await b.dispose();
  expect([unsubs, zero]).toEqual([2, 1]);
  view();
  await view().dispose();
  expect(zero).toBe(1);
});

function fakeAgent() {
  const subs = new Set<(e: any) => void>();
  return {
    messages: [],
    disposed: 0,
    subscribe: (cb: any) => (subs.add(cb), () => void subs.delete(cb)),
    dispose() { this.disposed++; },
    emit: (e: any) => subs.forEach((s) => s(e)),
  };
}

test("baseHandle: events reach every subscriber and track busy; dispose says session_closed once", () => {
  const agent = fakeAgent();
  const busy = new Set<string>();
  let ends = 0, disposes = 0;
  const h = baseHandle(agent, "k", { busy, onEnd: () => void ends++, onDispose: () => void disposes++ });
  const a: string[] = [], b: string[] = [];
  h.subscribe((e) => a.push(e.type));
  h.subscribe((e) => b.push(e.type));
  agent.emit({ type: "agent_start" });
  expect(h.busy).toBe(true);
  agent.emit({ type: "agent_end" });
  expect([h.busy, ends]).toEqual([false, 1]);
  void h.dispose();
  void h.dispose();
  expect(a).toEqual(["agent_start", "agent_end", "session_closed"]);
  expect(b).toEqual(a);
  expect([agent.disposed, disposes]).toEqual([1, 1]);
});

test("asking adds a required because and strips it before the tool runs", async () => {
  let got: any;
  const t: any = { name: "workspace_delete", description: "d", inputSchema: { type: "object", properties: { workspace: { type: "string" } }, required: ["workspace"] }, run: async (i: any) => ((got = i), "ok") };
  const a = asking(t);
  expect((a.inputSchema as any).properties.because).toBeDefined();
  expect((a.inputSchema as any).required).toEqual(["workspace", "because"]);
  await a.run({ workspace: "x", because: { reason: "r" } });
  expect(got).toEqual({ workspace: "x" });
  const read: any = { name: "read", inputSchema: { type: "object" }, run: async () => "" };
  expect(asking(read)).toBe(read);
});

test("the gate skips the card for words the person typed, else shows the reason", async () => {
  const gate = (typed: string[]) => {
    const pi: any = { agent: {} };
    const reqs: any[] = [];
    installGate(pi, async (req) => (reqs.push(req), {}), undefined, () => ({ typed }));
    return { pi, reqs };
  };
  const call = (because: any) => ({ toolCall: { name: "workspace_delete" }, args: { workspace: "foo", because } });
  const yes = gate(["please delete workspace foo"]);
  expect(await yes.pi.agent.beforeToolCall(call({ asked: "delete workspace foo" }))).toBeUndefined();
  expect(yes.reqs).toEqual([]);
  const no = gate([]);
  await no.pi.agent.beforeToolCall(call({ asked: "delete workspace foo" }));
  expect(no.reqs.length).toBe(1);
  expect(no.reqs[0].claimed).toBe("delete workspace foo");
  expect(no.reqs[0].args).toEqual({ workspace: "foo" });
  await no.pi.agent.beforeToolCall(call({ reason: "cleanup after test" }));
  expect(no.reqs[1].reason).toBe("cleanup after test");
});

test("sessions.watch answers at once and on every change, and stops after off", async () => {
  process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  delete process.env.KL_API_URL;
  const { models } = await import("@kloudlite-tui/agent");
  const pick = models.getModels().find((x: any) => x.provider !== "anthropic") as any;
  const o: any = { model: { provider: pick.provider, id: pick.id }, fresh: true, tools: [], permission: async () => ({}) };
  const b = new LocalBackend();
  const lists: any[][] = [];
  const off = await b.sessions.watch((l) => lists.push(l));
  expect(lists.length).toBe(1);
  const h = await b.session("main", o);
  expect(lists.length).toBeGreaterThan(1); // opened
  const n = lists.length;
  await b.sessions.name("main", "renamed");
  expect(lists.length).toBe(n + 1);
  expect(lists.at(-1)!.find((m) => m.key === "main")?.name).toBe("renamed");
  expect(lists.at(-1)!.find((m) => m.key === "main")?.cleared).toBeUndefined();
  await b.sessions.clear("main");
  expect(lists.length).toBe(n + 2);
  expect(lists.at(-1)!.find((m) => m.key === "main")?.cleared).toBeNumber();
  await h.dispose();
  await new Promise((r) => setTimeout(r, 10));
  expect(lists.length).toBeGreaterThan(n + 2); // closed
  const m = lists.length;
  off();
  await b.sessions.name("main", "again");
  expect(lists.length).toBe(m);
}, 20000);

test("baseHandle reports turn start and end to its change hook", () => {
  const agent = fakeAgent();
  let changes = 0;
  baseHandle(agent, "main", { busy: new Set(), onEnd() {}, onDispose() {}, onChange: () => void changes++ });
  agent.emit({ type: "agent_start" });
  agent.emit({ type: "agent_end", messages: [] });
  expect(changes).toBe(2);
});
