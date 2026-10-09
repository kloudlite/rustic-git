import { test, expect } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { LocalBackend, baseHandle } from "./local";
import { pair } from "./pair";

async function models() {
  process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  delete process.env.KL_API_URL;
  const { models } = await import("@kloudlite-tui/agent");
  return models.getModels().filter((x: any) => x.provider !== "anthropic") as any[];
}

// No provider key is needed to switch models: the daemon only records what its agent accepted.
const fake = (async () => ({
  messages: [],
  agent: {},
  subscribe: () => () => {},
  setModel: async () => {},
  setThinkingLevel() {},
  setAutoCompactionEnabled() {},
  dispose() {},
})) as any;

test("A's setModel reaches B as session_state; B's open does not change A's model", async () => {
  const ms = await models();
  const [m0, m1] = [ms[0], ms.find((x) => x.id !== ms[0].id) ?? ms[0]];
  const local = new LocalBackend({ create: fake });
  const a = await pair(local), b = await pair(local);
  const ha = await a.session("main", { initial: { model: { provider: m0.provider, id: m0.id } }, fresh: true, tools: [] });
  const hb = await b.session("main", { initial: { model: { provider: m0.provider, id: m0.id } }, tools: [] });
  const seen: any[] = [];
  hb.subscribe((e) => e.type === "session_state" && seen.push(e));
  await ha.setModel({ provider: m1.provider, id: m1.id });
  await Bun.sleep(10);
  expect(seen.at(-1)?.model.id).toBe(m1.id);
  // a third open carrying the old model as `initial` must not move it back
  const hc = await (await pair(local)).session("main", { initial: { model: { provider: m0.provider, id: m0.id } }, tools: [] });
  expect(hc.state.model.id).toBe(m1.id);
  await Promise.all([ha.dispose(), hb.dispose(), hc.dispose()]);
}, 20000);

test("open returns the session state", async () => {
  const [m] = await models();
  const local = new LocalBackend({ create: fake });
  const h = await (await pair(local)).session("w1", { initial: { model: { provider: m.provider, id: m.id }, codemode: false, autoCompact: true, thinkingLevel: "low" }, fresh: true, tools: [] });
  expect(h.state).toMatchObject({ type: "session_state", codemode: false, autoCompact: true, thinkingLevel: "low", tokens: 0, queued: { steering: [], followUp: [] } });
  await h.dispose();
}, 20000);

const counting = () => {
  const c = { builds: 0 };
  const create = (async () => (c.builds++, { messages: [], agent: {}, subscribe: () => () => {}, setModel: async () => {}, setThinkingLevel() {}, setAutoCompactionEnabled() {}, dispose() {} })) as any;
  return { c, create };
};

test("A's /clear: B gets session_closed reopen, and B's reopen is empty", async () => {
  const [m] = await models();
  const local = new LocalBackend({ create: fake });
  const a = await pair(local), b = await pair(local);
  const o = { initial: { model: { provider: m.provider, id: m.id } }, fresh: true, tools: [] };
  const ha = await a.session("main", o);
  const hb = await b.session("main", { ...o, fresh: false });
  const closed: any[] = [];
  hb.subscribe((e) => e.type === "session_closed" && closed.push(e));
  await a.sessions.clear("main");
  await Bun.sleep(10);
  expect(closed).toEqual([{ type: "session_closed", reopen: true }]);
  const again = await b.session("main", { tools: [] });
  expect(again.messages).toEqual([]);
  await Promise.all([ha.dispose(), again.dispose()]);
}, 20000);

test("idle dispose says reopen false", async () => {
  // a disposed view has already unsubscribed, so check the agent's own handle
  const state: any = { type: "session_state", model: { provider: "p", id: "m" }, thinkingLevel: "low", autoCompact: true, codemode: true, queued: { steering: [], followUp: [] }, tokens: 0 };
  const h = baseHandle({ messages: [], subscribe: () => () => {}, dispose() {} }, "k", { busy: new Set(), onEnd() {}, onDispose() {}, state });
  const closed: any[] = [];
  h.subscribe((e) => e.type === "session_closed" && closed.push(e));
  await h.dispose();
  expect(closed).toEqual([{ type: "session_closed", reopen: false }]);
});

test("two opens racing a rebuild build one agent", async () => {
  const [m] = await models();
  const { c, create } = counting();
  const local = new LocalBackend({ create });
  const o = { initial: { model: { provider: m.provider, id: m.id } }, fresh: true, tools: [] };
  const h = await local.session("main", o);
  expect(c.builds).toBe(1);
  const toggle = h.setCodemode(!h.state.codemode); // rebuild
  const [x, y] = await Promise.all([local.session("main", { tools: [] }), local.session("main", { tools: [] })]);
  await toggle;
  expect(c.builds).toBe(2);
  await Promise.all([x.dispose(), y.dispose()]);
}, 20000);

test("setCodemode while busy is refused", async () => {
  const [m] = await models();
  const local = new LocalBackend({ create: fake });
  const h = await local.session("w3", { initial: { model: { provider: m.provider, id: m.id } }, fresh: true, tools: [] });
  local.busyForTest("w3", true);
  await expect(h.setCodemode(!h.state.codemode)).rejects.toThrow("a turn is running");
  local.busyForTest("w3", false);
  await h.dispose();
}, 20000);

const bashReq = { name: "bash", args: { command: "ls" } } as any;

test("a gated call asks both TUIs; B answers; A hears ask_resolved; A's late answer is ignored", async () => {
  const local = new LocalBackend();
  const a = await pair(local), b = await pair(local);
  const ea: any[] = [], eb: any[] = [];
  await a.watch((e) => ea.push(e));
  await b.watch((e) => eb.push(e));
  const decision = local.permit("main", bashReq, new AbortController().signal);
  await Bun.sleep(10);
  const ask = eb.find((e) => e.type === "ask")!.ask;
  expect(ea.find((e) => e.type === "ask")?.ask.id).toBe(ask.id);
  await b.asks.answer(ask.id, "reject");
  expect(await decision).toEqual({ block: true, reason: "The user rejected this tool call." });
  await Bun.sleep(10);
  expect(ea).toContainEqual({ type: "ask_resolved", id: ask.id });
  await a.asks.answer(ask.id, "once"); // ignored, no throw
});

test("aborting the turn resolves the ask as reject", async () => {
  const local = new LocalBackend();
  const ac = new AbortController();
  const d = local.permit("main", bashReq, ac.signal);
  ac.abort();
  expect((await d).block).toBe(true);
});

test("hello on a late connection returns the pending ask and the mode", async () => {
  const local = new LocalBackend();
  const a = await pair(local);
  await a.mode.set("acceptEdits");
  const d = local.permit("main", bashReq, new AbortController().signal);
  const c = await pair(local);
  const h = await c.hello();
  expect(h.asks.map((x) => x.tool)).toEqual(["bash"]);
  expect(h.mode).toBe("acceptEdits");
  await c.asks.answer(h.asks[0]!.id, "once");
  expect(await d).toEqual({});
});

test("mode: plan answers without a card, bypass allows, acceptEdits allows edits only", async () => {
  const local = new LocalBackend();
  const a = await pair(local);
  const seen: any[] = [];
  await a.watch((e) => seen.push(e));
  await a.mode.set("plan");
  expect((await local.permit("main", bashReq, new AbortController().signal)).block).toBe(true);
  await a.mode.set("bypass");
  expect(await local.permit("main", bashReq, new AbortController().signal)).toEqual({});
  await a.mode.set("acceptEdits");
  expect(await local.permit("main", { name: "edit", args: { path: "x" } } as any, new AbortController().signal)).toEqual({});
  expect(seen.filter((e) => e.type === "perm").map((e) => e.mode)).toEqual(["plan", "bypass", "acceptEdits"]);
  expect(seen.some((e) => e.type === "ask")).toBe(false);
});

test("aborting the turn withdraws a pending question card and every watcher hears it", async () => {
  const ms = await models();
  let reg: any;
  const local = new LocalBackend({ create: (async (o: any) => ((reg = o.registry), fake(o))) as any });
  const a = await pair(local), b = await pair(local);
  await a.session("main", { initial: { model: { provider: ms[0].provider, id: ms[0].id } }, fresh: true, tools: [] });
  const ea: any[] = [], eb: any[] = [];
  await a.watch((e) => ea.push(e));
  await b.watch((e) => eb.push(e));
  const ac = new AbortController();
  const p = reg.get("question").run({ question: "which?", options: ["x", "y"] }, ac.signal);
  await Bun.sleep(10);
  const ask = ea.find((e) => e.type === "ask")!.ask;
  ac.abort();
  await expect(p).rejects.toThrow("withdrawn");
  await Bun.sleep(10);
  for (const e of [ea, eb]) expect(e).toContainEqual({ type: "ask_resolved", id: ask.id });
});

test("settings.write reaches every TUI", async () => {
  await models();
  const local = new LocalBackend({ create: fake });
  const a = await pair(local), b = await pair(local);
  const eb: any[] = [];
  const off = await b.watch((e) => eb.push(e));
  await a.settings.write({ thinkingLevel: "high" });
  await Bun.sleep(10);
  expect(eb.find((e) => e.type === "settings")?.settings.thinkingLevel).toBe("high");
  await off();
});

test("forgetSessions tells its caller the list changed", async () => {
  const { forgetSessions } = await import("./forget");
  let n = 0;
  await forgetSessions("ws-9", new Map(), () => n++);
  expect(n).toBe(1);
});

test("a file-touching tool pushes fs_changed once per burst, with the workspace", async () => {
  const ms = await models();
  const m = ms[0];
  let emit!: (e: any) => void;
  const create = (async () => ({
    messages: [], agent: {}, subscribe: (f: any) => ((emit = f), () => {}),
    setModel: async () => {}, setThinkingLevel() {}, setAutoCompactionEnabled() {}, dispose() {},
  })) as any;
  const local = new LocalBackend({ create });
  const a = await pair(local);
  const ev: any[] = [];
  const off = await a.watch((e) => ev.push(e));
  const h = await a.session("ws-1", { initial: { model: { provider: m.provider, id: m.id } }, fresh: true, tools: [] });
  emit({ type: "tool_execution_end", toolName: "write" });
  emit({ type: "tool_execution_end", toolName: "edit" });
  await Bun.sleep(700);
  expect(ev.filter((e) => e.type === "fs_changed")).toEqual([{ type: "fs_changed", ws: "ws-1" }]);
  await off();
  await h.dispose().catch(() => {});
}, 20000);
