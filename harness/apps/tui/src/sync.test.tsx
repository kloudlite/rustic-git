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
