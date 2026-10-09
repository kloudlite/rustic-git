import { expect, test } from "bun:test";
import { testRender } from "@opentui/react/test-utils";
import { LocalBackend } from "@kloudlite-tui/backend/local";
import { backend, boot, hello } from "./hello.ts";
import { App } from "./app.tsx";

const tick = () => new Promise((r) => setTimeout(r, 60));

/** `watch: "old"` plays a bench without the op; `push` plays the bench pushing a new list. */
function scripted(opts: { watch?: "answers" | "old" } = {}) {
  const b: any = new LocalBackend();
  b.space = backend().space; // as app.test.tsx: the real one reaches for the platform
  const listCalls: string[] = [];
  let watcher: ((l: any[]) => void) | undefined;
  b.sessions = {
    list: async (prefix: string) => (listCalls.push(prefix), []),
    name: async () => {},
    describe: async () => {},
    clear: async () => {},
    watch: async (cb: any) => {
      if (opts.watch === "old") throw new Error("unknown op: sessions.watch");
      watcher = cb;
      cb([]);
      return () => {};
    },
  };
  return { b, listCalls, push: (l: any[]) => watcher?.(l) };
}

async function mount(b: any) {
  const real = backend(), hi = hello(); // other files share this module state: put it back in done()
  boot(b, { ...hello(), settings: { ...hello().settings, vim: "off", sidebarWidth: 42 } });
  const setup = await testRender(<App />, { width: 200, height: 32, kittyKeyboard: true });
  const frame = async () => (await tick(), await setup.renderOnce(), setup.captureCharFrame());
  await frame();
  return { ...setup, frame, done: () => (setup.renderer.destroy(), boot(real, hi)) };
}

test("a session another view names shows up here without a fetch", async () => {
  const s = scripted({ watch: "answers" });
  const ui = await mount(s.b);
  s.push([{ key: "main:notes", name: "release notes", busy: true, updated: Date.now() }]);
  await ui.mockInput.typeText("/session ");
  expect(await ui.frame()).toContain("release notes");
  ui.done();
});

test("a bench that answers sessions.watch is never asked for the list", async () => {
  const s = scripted({ watch: "answers" });
  const ui = await mount(s.b);
  await ui.frame();
  expect(s.listCalls).toEqual([]);
  ui.done();
});

test("an old bench without sessions.watch keeps fetching the list", async () => {
  const s = scripted({ watch: "old" });
  const ui = await mount(s.b);
  await ui.frame();
  expect(s.listCalls.length).toBeGreaterThan(0);
  ui.done();
});

import { transcript, userRow, upsertById, applyState, keepFocus, autoAnswer, grant } from "./sync";

const sum = (n: string) => n;

test("snapshot then the same live event renders one row", () => {
  const msgs = [
    { role: "user", timestamp: 1, content: [{ type: "text", text: "hi" }] },
    { role: "assistant", timestamp: 2, content: [{ type: "text", text: "hello" }] },
  ];
  const { entries, history } = transcript(msgs, false, sum);
  expect(entries.map((e: any) => e.id)).toEqual(["u1", "m2"]);
  expect(history).toEqual(["hi"]);
  const live = userRow({ type: "message_start", message: { role: "user", timestamp: 1 }, shown: "hi" })!;
  expect(upsertById(entries, live).length).toBe(2);
});

test("a tool call with no result while busy is running", () => {
  const msgs = [{ role: "assistant", timestamp: 3, content: [{ type: "toolCall", id: "t1", name: "bash", arguments: {} }] }];
  expect((transcript(msgs, true, sum).entries[0] as any).status).toBe("running");
  expect((transcript(msgs, false, sum).entries[0] as any).status).toBe("ok");
});

test("a user message_start renders one row from shown", () => {
  expect(userRow({ type: "message_start", message: { role: "user", timestamp: 9 }, shown: "go" })).toEqual({ kind: "user", id: "u9", text: "go" });
  expect(userRow({ type: "message_start", message: { role: "assistant", timestamp: 9 } })).toBeNull();
});

test("pushed session_state sets model, tokens and queue", () => {
  const p = applyState({ type: "session_state", model: { provider: "openai", id: "x" }, thinkingLevel: "low", autoCompact: true, codemode: true, queued: { steering: ["a"], followUp: ["b"] }, tokens: 12 });
  expect(p).toEqual({ model: { provider: "openai", id: "x" }, tokens: 12, queued: [{ text: "a", kind: "steer" }, { text: "b", kind: "followUp" }] });
});

test("a space push with error keeps focus; a good push without the workspace clamps", () => {
  const v = (ids: string[], error?: string) => ({ available: true, user: "u", workspaces: ids.map((id) => ({ id })), environments: [], error }) as any;
  expect(keepFocus({ focus: 2, ids: ["a", "b"] }, v([], "timed out"))).toBe(2);
  expect(keepFocus({ focus: 2, ids: ["a", "b"] }, v(["b", "a"]))).toBe(1);
  expect(keepFocus({ focus: 2, ids: ["a", "b"] }, v(["a"]))).toBe(1);
  expect(keepFocus({ focus: 0, ids: ["a"] }, v([]))).toBe(0);
});

test("Allow always answers the next ask for that tool on that key only", () => {
  const g = new Map<string, Set<string>>();
  const ask = (key: string, tool: string) => ({ id: "x", key, kind: "permission", tool, title: "", options: [] }) as any;
  grant(g, ask("k", "bash"));
  expect(autoAnswer(ask("k", "bash"), g)).toBe("once");
  expect(autoAnswer(ask("other", "bash"), g)).toBeNull();
  expect(autoAnswer({ ...ask("k", "bash"), kind: "question" }, g)).toBeNull();
});
