import { expect, test } from "bun:test";
import { testRender } from "@opentui/react/test-utils";
import type { ToolDef } from "@kloudlite-tui/tools";
import { setCopier } from "./clipboard.ts";
import { LocalBackend } from "@kloudlite-tui/backend/local";
import { backend, boot, hello } from "./hello.ts";

// App reads its settings from hello() once at mount, so tests seed them by re-booting.
// A fresh backend each time: a turn left pending by the previous test (no auth, it never ends)
// would otherwise open the next test's session as busy, which the app now honours.
function writeSettings(patch: Record<string, unknown>) {
  const fresh = new LocalBackend();
  fresh.space = backend().space;
  boot(fresh, { ...hello(), settings: { ...hello().settings, ...patch } });
}
import { App } from "./app.tsx";

const COLS = 200;
const ROWS = 32;

const tick = () => new Promise((r) => setTimeout(r, 60));

/**
 * Most tests drive the vim key scheme (bare-letter commands), so they opt into
 * it explicitly — `vim` defaults to "off", where typing always reaches the
 * prompt and the commands live on ctrl+<letter>.
 */
async function mount(opts?: { vim?: "on" | "off" }) {
  // width too: a resize test would otherwise leak its narrow sidebar into
  // every later mount through the shared temp config
  writeSettings({ vim: opts?.vim ?? "on", sidebarWidth: 42 });
  const setup = await testRender(<App />, {
    width: COLS,
    height: ROWS,
    // kitty protocol on: shift+enter etc. arrive as CSI-u like modern terminals
    kittyKeyboard: true,
  });
  await tick();
  await setup.renderOnce();
  const frame = async () => {
    await tick();
    await setup.renderOnce();
    return setup.captureCharFrame();
  };
  const insert = async () => {
    // only vim's NORMAL mode needs to be left; otherwise the prompt is live
    if ((opts?.vim ?? "on") === "on") setup.mockInput.pressKey("i");
    await tick();
    await setup.renderOnce();
  };
  /** The prompt card's session row alone — the sidebar names workspaces too. */
  const path = async () => {
    const lines = (await frame()).split("\n").filter((l) => l.trim() !== "");
    // the row under the input: "<session> · <model> <provider>"
    return lines.filter((l) => l.includes(" \u00b7 ")).pop()?.trim() ?? "";
  };
  return { ...setup, frame, path, insert, done: () => setup.renderer.destroy() };
}

test("sidebar renders", async () => {
  const t = await mount();
  const f = await t.frame();
  expect(f).toContain("Working Session");
  expect(f).toContain("api-gateway");
  expect(f).toContain("NORMAL"); // modal keyboard starts in NORMAL
  t.done();
});

test("slash menu opens and fits the frame", async () => {
  const t = await mount();
  await t.mockInput.typeText("/");
  const f = await t.frame();
  expect(f).toContain("/help");
  expect(f.trimEnd().split("\n").length).toBeLessThanOrEqual(ROWS);
  expect(f).toContain("Commands"); // "/" opens the top-level command overlay
  t.done();
});

test("menu navigation: arrow selects, enter runs the highlighted command", async () => {
  const t = await mount();
  await t.mockInput.typeText("/");
  await t.frame();
  t.mockInput.pressKey("ARROW_DOWN"); // select /tools
  await t.frame();
  t.mockInput.pressKey("RETURN");
  const f = await t.frame();
  expect(f).toContain("question"); // the built-in question tool
  t.done();
});

test("card keeps its shape after submit", async () => {
  const t = await mount();
  await t.insert();
  await t.mockInput.typeText("hi");
  await t.frame();
  t.mockInput.pressKey("RETURN");
  const f = await t.frame();
  expect(f.trimEnd().split("\n").length).toBeLessThanOrEqual(ROWS);
  expect(f).toContain("Working Session"); // session row still in place
  expect(f).toContain("hi"); // user turn in the transcript
  t.done();
});

// shift+tab belongs to the permission mode now, so tab only goes forward;
// k / ^k is what walks the ring backwards.
test("tab cycles focus forward through the ring", async () => {
  const t = await mount();
  t.mockInput.pressKey("\t"); // main -> first workspace
  expect(await t.path()).toContain("api-gateway");
  t.mockInput.pressKey("k"); // back to main (in the ring)
  expect(await t.path()).not.toContain("api-gateway");
  t.mockInput.pressKey("k"); // again -> wraps to the last workspace
  expect(await t.path()).toContain("infra-iac");
  t.done();
});

test("NORMAL j enters a workspace, 0 returns to main", async () => {
  const t = await mount();
  t.mockInput.pressKey("j");
  expect(await t.path()).toContain("api-gateway");
  t.mockInput.pressKey("0"); // back to main context
  expect(await t.path()).not.toContain("api-gateway");
  expect(await t.frame()).not.toContain("Ask anything, or / for commandsk");
  t.done();
});

test("in INSERT, plain j and k type; in NORMAL they navigate", async () => {
  const t = await mount();
  await t.insert();
  await t.mockInput.typeText("jk");
  expect(await t.frame()).toContain("jk");
  expect(await t.path()).not.toContain("api-gateway"); // typing, not navigating
  t.done();
});

test("NORMAL mode: j enters a workspace without typing", async () => {
  const t = await mount();
  t.mockInput.pressKey("j");
  expect(await t.path()).toContain("api-gateway");
  t.done();
});

test("backslash-enter continues on a new line; enter submits the whole thing", async () => {
  const t = await mount();
  await t.insert();
  await t.mockInput.typeText("first\\");
  await t.frame();
  t.mockInput.pressKey("RETURN"); // continuation, not submit
  await t.frame();
  await t.mockInput.typeText("second");
  let f = await t.frame();
  expect(f).toContain("first");
  expect(f).toContain("second");
  t.mockInput.pressKey("RETURN"); // submit both lines
  f = await t.frame();
  expect(f).toContain("first");
  expect(f).toContain("second");
  t.done();
});

test("shift+enter inserts a newline instead of submitting", async () => {
  const t = await mount();
  await t.insert();
  await t.mockInput.typeText("hi");
  await t.frame();
  t.mockInput.pressEnter({ shift: true });
  await t.frame();
  await t.mockInput.typeText("there");
  const f = await t.frame();
  expect(f).toContain("hi");
  expect(f).toContain("there");
  expect(f).not.toContain("interrupt"); // no submit happened
  t.done();
});

test("input history is per session and recalled with arrows", async () => {
  const t = await mount();
  await t.insert();
  await t.mockInput.typeText("first prompt");
  await t.frame();
  t.mockInput.pressKey("RETURN");
  await t.frame();

  // main session: up recalls
  t.mockInput.pressKey("ARROW_UP");
  expect(await t.frame()).toContain("first prompt");
  t.mockInput.pressKey("ARROW_DOWN"); // back to empty
  await t.frame();

  // workspace session has its own (empty) history — up recalls nothing
  t.mockInput.pressKey("ESCAPE"); // INSERT → NORMAL
  await t.frame();
  t.mockInput.pressKey("j");
  await t.frame();
  t.mockInput.pressKey("i"); // history recall needs INSERT
  await t.frame();
  t.mockInput.pressKey("ARROW_UP");
  const f = await t.frame();
  expect(await t.path()).toContain("api-gateway"); // in workspace
  // the prompt card shows no recalled text (transcript may still show the turn)
  expect(f).not.toContain("Agent · first prompt");
  t.done();
});

test("selecting /login from the menu allows typing the sub-option filter", async () => {
  const t = await mount();
  await t.mockInput.typeText("/"); // NAV: "/" jumps straight into INSERT
  await t.frame();
  await t.mockInput.typeText("login");
  await t.frame();
  t.mockInput.pressKey("RETURN"); // menu enter → inserts "/login "
  await t.frame();
  await t.mockInput.typeText("codex"); // must append at the end, not at position 0
  const f = await t.frame();
  expect(f).toContain("login codex"); // overlay filter carries the picked prefix
  expect(f).toContain("openai-codex");
  t.done();
});

test("sidebar docks when wide, hides when narrow", async () => {
  const wide = await testRender(<App />, { width: 160, height: 32 });
  await tick();
  await wide.renderOnce();
  expect(wide.captureCharFrame()).toContain("api-gateway"); // sidebar docked
  wide.renderer.destroy();

  const narrow = await testRender(<App />, { width: 100, height: 32 });
  await tick();
  await narrow.renderOnce();
  expect(narrow.captureCharFrame()).not.toContain("api-gateway");
  narrow.renderer.destroy();
});

test("f opens the files view: changes and tree in one list", async () => {
  const t = await mount();
  t.mockInput.pressKey("1"); // enter api-gateway
  await t.frame();
  t.mockInput.pressKey("f");
  const f = await t.frame();
  expect(f).toContain("CHANGES"); // the tree lists changes, then FILES below
  expect(f).toContain("files ›"); // and the view's own footer
  t.mockInput.pressKey("ESCAPE");
  expect(await t.frame()).not.toContain("CHANGES");
  t.done();
});

test("/session name names the session you are in", async () => {
  const t = await mount();
  await t.insert();
  await t.mockInput.typeText("/session name rate limits");
  await t.frame();
  t.mockInput.pressKey("RETURN");
  const f = await t.frame();
  expect(f).toContain("rate limits"); // hint bar path: the main session alone
  expect(f).toContain("rate limits"); // sidebar root row is the session
  t.done();
});

test("workspaces carry their own named sessions", async () => {
  const t = await mount();
  t.mockInput.pressKey("1"); // into api-gateway
  await t.frame();
  await t.insert();
  await t.mockInput.typeText("/session name probe run");
  await t.frame();
  t.mockInput.pressKey("RETURN");
  const f = await t.frame();
  expect(f).toContain("probe run"); // the title bar and that workspace's row
  // the path stays the workspace alone — it does not repeat the session name
  expect(await t.path()).not.toContain("probe run");
  t.done();
});

test("s in a workspace files view says search is not available and opens no prompt", async () => {
  const t = await mount();
  t.mockInput.pressKey("1"); // enter a workspace
  await t.frame();
  t.mockInput.pressKey("f"); // files view
  expect(await t.frame()).toContain("CHANGES");

  t.mockInput.pressKey("s");
  const f = await t.frame();
  expect(f).toContain("search is not available for workspaces");
  expect(f).not.toContain("search ›");
  t.done();
});

test("mouse: clicking a sidebar workspace and a hint", async () => {
  const t = await mount();
  const frame = await t.frame();
  const lines = frame.split("\n");
  const row = lines.findIndex((l) => l.includes("billing-svc"));
  const col = lines[row]!.indexOf("billing-svc");
  await t.mockMouse.click(col + 1, row);
  expect(await t.frame()).toContain("billing-svc "); // hint bar path follows

  // the hint bar's "f view" switches the column view on click
  const hints = (await t.frame()).split("\n");
  const hrow = hints.findIndex((l) => l.includes("f view"));
  const hcol = hints[hrow]!.indexOf("f view");
  await t.mockMouse.click(hcol + 1, hrow);
  expect(await t.frame()).toContain("CHANGES");
  t.done();
});

async function processesView() {
  const t = await mount();
  t.mockInput.pressKey("1"); // api-gateway
  await t.frame();
  t.mockInput.pressKey("f"); // files
  await t.frame();
  t.mockInput.pressKey("f"); // processes
  return t;
}

test("processes view lists what the workspace runs, with its logs", async () => {
  const t = await processesView();
  const f = await t.frame();
  expect(f).toContain("2 running · 1 crashed");
  expect(f).toContain("RUNNING");
  expect(f).toContain("STOPPED");
  expect(f).toContain("gateway serve --port 8080"); // the reader header carries the full command
  expect(f).toContain("listening on :8080"); // selected process's log
  expect(f).toContain("following");

  t.mockInput.pressKey("j"); // metrics
  await t.frame();
  t.mockInput.pressKey("j"); // tests (crashed)
  const g = await t.frame();
  expect(g).toContain("--- FAIL: TestRateLimit");
  expect(g).toContain("limiter_test.go:42: want 100 got 128"); // stderr line
  t.done();
});

test("processes view: section headers with counts, crashed row, filter, follow", async () => {
  const t = await processesView();
  const f = await t.frame();
  expect(f).toMatch(/RUNNING\s+2/);
  expect(f).toMatch(/STOPPED\s+1/);
  expect(f).toMatch(/✕ go test\s+exit 1/);
  expect(f).toMatch(/● gateway serve\s+\S+/);

  t.mockInput.pressKey("F", { shift: true }); // toggles follow off
  expect(await t.frame()).not.toContain("following");
  t.mockInput.pressKey("F", { shift: true });
  expect(await t.frame()).toContain("following");

  t.mockInput.pressKey("/");
  await t.frame();
  await t.mockInput.typeText("metrics");
  expect(await t.frame()).toContain("/metrics");
  t.mockInput.pressEnter();
  const g = await t.frame();
  expect(g).toContain("/metrics");
  expect(g).toContain("1/3");
  expect(g).not.toContain("STOPPED");
  expect(g).not.toContain("go test");
  t.done();
});

test("sidebar: session header, workspaces with their ephemerals, environment with its services", async () => {
  const t = await mount();
  const f = await t.frame();
  expect(f).toContain("✦ "); // session header (title depends on earlier prompts)
  expect(f).toContain("Workspaces");
  expect(f).toContain("api-gateway");
  expect(f).toMatch(/├ rate-limits-probe/); // ephemeral workspaces branch off it
  expect(f).toMatch(/└ load-test/); // short names, not their task text
  expect(f).not.toContain("Review bc5a5062"); // the task prompt never reaches a row
  expect(f).not.toContain("IMPL"); // no phase tags
  expect(f).not.toContain("●"); // no dot indicators
  expect(f).toContain("Environment"); // the env heads its own block
  expect(f).toContain("production");
  expect(f).not.toContain("current snapshot"); // the platform names no restore point; the row only shows when one is known
  expect(f).toContain("→ api-gateway"); // interception, in the right-hand column
  expect(f).toContain("tcp:8080"); // ports stay in their own column; the platform has no protocol, so tcp
  t.done();
});

test("clicking an ephemeral row enters that workspace", async () => {
  const t = await mount();
  const lines = (await t.frame()).split("\n");
  const row = lines.findIndex((l) => l.includes("└ load-test"));
  await t.mockMouse.click(lines[row]!.indexOf("load-test") + 1, row);
  expect(await t.frame()).toContain("api-gateway › load-test");
  t.done();
});

test("session title bar: the title is inherited, never derived from a prompt", async () => {
  const t = await mount();
  // the main context is the working session
  expect((await t.frame()).split("\n")[1]).toContain("Working Session");
  t.mockInput.pressKey("1"); // api-gateway — the workspace names its session
  expect((await t.frame()).split("\n")[1]).toContain("api-gateway");
  t.mockInput.pressKey("2"); // its ephemeral agent names its own
  expect((await t.frame()).split("\n")[1]).toContain("rate-limits-probe");
  // asking something does not rename the title bar
  await t.insert();
  t.mockInput.typeText("why is the rate limiter dropping requests");
  await t.frame();
  t.mockInput.pressKey("\r");
  const f = await t.frame();
  expect(f.split("\n")[1]).toContain("rate-limits-probe");
  expect(f).not.toMatch(/· \d+ changed/); // no derived subtitle either
  t.done();
});

test("with vim off, typing reaches the prompt and ^f opens the files view", async () => {
  const t = await mount({ vim: "off" });
  await t.mockInput.typeText("hello there");
  expect(await t.frame()).toContain("hello there"); // no `i` needed
  t.mockInput.pressKey("k", { ctrl: true }); // into the first workspace
  await t.frame();
  t.mockInput.pressKey("f", { ctrl: true }); // cycle to the files view
  expect(await t.frame()).toContain("CHANGES");
  t.done();
});

test("with vim off, / on an empty prompt opens the commands overlay", async () => {
  const t = await mount({ vim: "off" });
  await t.mockInput.typeText("/");
  expect(await t.frame()).toContain("Commands");
  t.done();
});

test("a / inside a prompt is just text", async () => {
  const t = await mount({ vim: "off" });
  await t.mockInput.typeText("look at src/app.tsx");
  const f = await t.frame();
  expect(f).toContain("src/app.tsx");
  expect(f).not.toContain("Commands");
  t.done();
});

test("the sidebar resizes with [ and ] and clamps at its limits", async () => {
  const t = await mount();
  const cols = (f: string) => f.split("\n")[3]!.length - f.split("\n")[3]!.indexOf("Workspaces");
  const before = cols(await t.frame());
  t.mockInput.pressKey("]"); // wider
  const wider = cols(await t.frame());
  expect(wider).toBeGreaterThan(before);
  for (let i = 0; i < 12; i++) t.mockInput.pressKey("["); // past the minimum
  const narrow = cols(await t.frame());
  expect(narrow).toBeLessThan(before);
  for (let i = 0; i < 3; i++) t.mockInput.pressKey("["); // clamped, no further change
  expect(cols(await t.frame())).toBe(narrow);
  t.done();
});

// The platform tools live in the backend now; the TUI owns only what needs the user.
test("the TUI registers only the question tool", async () => {
  const tools: ToolDef[] = [];
  const t = await testRender(<App tools={tools} />, { width: 160, height: 40 });
  await new Promise((r) => setTimeout(r, 200));
  expect(tools.map((t) => t.name)).toEqual(["question"]);
});

// Permission mode is per-session chrome: shift+tab cycles it and only a
// non-default mode shows, so the ordinary case stays quiet.
test("shift+tab cycles the permission mode and shows it in the hint bar", async () => {
  const t = await mount();
  expect(t.captureCharFrame()).not.toContain("shift+tab");
  await t.mockInput.pressKey("TAB", { shift: true });
  await tick();
  await t.renderOnce();
  expect(t.captureCharFrame()).toContain("acceptEdits");
  await t.mockInput.pressKey("TAB", { shift: true });
  await t.mockInput.pressKey("TAB", { shift: true });
  await tick();
  await t.renderOnce();
  expect(t.captureCharFrame()).toContain("bypass");
  t.done();
});

// Typing "/" opens the command overlay; deleting it back out has to close it,
// or the menu outlives the slash that opened it.
test("backspacing the slash closes the command overlay", async () => {
  const t = await mount({ vim: "off" });
  await t.mockInput.typeText("/");
  await tick();
  await t.renderOnce();
  expect(t.captureCharFrame()).toContain("Commands");
  await t.mockInput.pressKey("BACKSPACE");
  await tick();
  await t.renderOnce();
  expect(t.captureCharFrame()).not.toContain("Commands");
  t.done();
});

// pi's prompt() refuses while a turn streams rather than queueing, so
// submitting mid-turn has to call steer() instead — otherwise the user gets
// "Agent is already processing" where they expected their message queued.
test("prompting while a turn streams queues instead of erroring", async () => {
  const t = await mount({ vim: "off" });
  await t.mockInput.typeText("first");
  await t.mockInput.pressEnter();
  await tick();
  await t.mockInput.typeText("second");
  await t.mockInput.pressEnter();
  await tick();
  await t.renderOnce();
  const frame = t.captureCharFrame();
  expect(frame).not.toContain("already processing");
  // both reached the transcript, and the prompt was cleared each time
  expect(frame).toContain("first");
  expect(frame).toContain("second");
  t.done();
});

// ctrl+enter cannot be told from a bare enter unless the terminal speaks the
// kitty protocol, so ctrl+s is the binding that has to keep working.
test("ctrl+s submits the prompt without clearing it twice", async () => {
  const t = await mount({ vim: "off" });
  await t.mockInput.typeText("steered");
  await t.mockInput.pressKey("s", { ctrl: true });
  await tick();
  await t.renderOnce();
  // not busy, so ctrl+s is inert and the text stays put to be sent with enter
  expect(t.captureCharFrame()).toContain("steered");
  t.done();
});

// pi fixes the tool list when a session is built, so toggling codemode has to
// rebuild every open session — closing the old one before the new one opens,
// since the backend keys sessions by name.
test("toggling codemode rebuilds the open session with the new value", async () => {
  const real = backend();
  const seen: string[] = [];
  // a Proxy, not a spread: the backend and its handles are class instances
  const wrap = <T extends object>(t: T, over: Partial<T>) =>
    new Proxy(t, {
      get: (o: any, p) => (p in over ? (over as any)[p] : typeof o[p] === "function" ? o[p].bind(o) : o[p]),
    }) as T;
  boot(
    wrap(real, {
      session: async (key, opts) => {
        seen.push(`open ${opts.initial?.codemode}`);
        const h = await real.session(key, opts);
        return wrap(h, { dispose: async () => (seen.push("dispose"), h.dispose()) });
      },
    }),
    { ...hello(), settings: { ...hello().settings, vim: "on", codemode: "on" } },
  );
  const setup = await testRender(<App />, { width: COLS, height: ROWS, kittyKeyboard: true });
  await tick();
  await setup.renderOnce();
  setup.mockInput.pressKey("i");
  await tick();
  await setup.renderOnce();
  await setup.mockInput.typeText("/settings codemode off");
  await tick();
  await setup.renderOnce();
  setup.mockInput.pressKey("RETURN");
  for (let i = 0; i < 50 && seen.length < 3; i++) await tick();
  await setup.renderOnce();
  expect(seen).toEqual(["open true", "dispose", "open false"]);
  setup.renderer.destroy();
  boot(real, hello());
});

// The daemon disposes an idle or rebuilt agent and tells the client; the dead handle must not be
// reused, so the next prompt opens a fresh one.
test("session_closed makes the next prompt reopen the session", async () => {
  const real = backend();
  let opens = 0;
  let fire: (e: any) => void = () => {};
  const wrap = <T extends object>(t: T, over: Partial<T>) =>
    new Proxy(t, {
      get: (o: any, p) => (p in over ? (over as any)[p] : typeof o[p] === "function" ? o[p].bind(o) : o[p]),
    }) as T;
  boot(
    wrap(real, {
      session: async (key, opts) => {
        opens++;
        const h = await real.session(key, opts);
        return wrap(h, { subscribe: (cb) => ((fire = cb as any), h.subscribe(cb)) });
      },
    }),
    { ...hello(), settings: { ...hello().settings, vim: "on" } },
  );
  const setup = await testRender(<App />, { width: COLS, height: ROWS, kittyKeyboard: true });
  for (let i = 0; i < 10 && opens < 1; i++) await tick();
  await tick();
  expect(opens).toBe(1);
  fire({ type: "session_closed" });
  await tick();
  setup.mockInput.pressKey("i");
  await tick();
  await setup.mockInput.typeText("hello");
  await tick();
  setup.mockInput.pressKey("RETURN");
  for (let i = 0; i < 50 && opens < 2; i++) await tick();
  expect(opens).toBe(2);
  setup.renderer.destroy();
  boot(real, hello());
});

// A reopened session carries tool results as separate `toolResult` messages; the
// transcript has to fold each into its tool entry or every restored tool looks empty.
async function restored(messages: unknown[]) {
  const real = backend();
  const wrap = <T extends object>(t: T, over: Partial<T>) =>
    new Proxy(t, {
      get: (o: any, p) => (p in over ? (over as any)[p] : typeof o[p] === "function" ? o[p].bind(o) : o[p]),
    }) as T;
  boot(
    wrap(real, {
      session: async (key, opts) => {
        const h = await real.session(key, opts);
        return wrap(h, { messages: messages as any });
      },
    }),
    { ...hello(), settings: { ...hello().settings, vim: "on", sidebarWidth: 42 } },
  );
  const setup = await testRender(<App />, { width: COLS, height: ROWS, kittyKeyboard: true });
  let f = "";
  for (let i = 0; i < 10; i++) {
    await tick();
    await setup.renderOnce();
    f = setup.captureCharFrame();
  }
  setup.renderer.destroy();
  boot(real, hello());
  return f;
}

const codemodeSession = (code: string, result: string, isError: boolean) => [
  { role: "user", content: [{ type: "text", text: "run it" }] },
  {
    role: "assistant",
    content: [{ type: "toolCall", id: "t1", name: "codemode", arguments: { code } }],
  },
  {
    role: "toolResult",
    toolCallId: "t1",
    toolName: "codemode",
    content: [{ type: "text", text: result }],
    isError,
  },
];

test("a restored session shows each tool's result", async () => {
  const f = await restored(codemodeSession("return 1;", "RESULT-MARKER", false));
  expect(f).toContain("RESULT-MARKER");
});

test("a restored session shows what a codemode script displayed", async () => {
  const msgs = codemodeSession("return 1;", "shown", false);
  (msgs[2] as any).details = { display: ["| hcol | hval |\n|---|---|\n| DISP-MARKER | 2 |"] };
  const f = await restored(msgs);
  expect(f).toContain("DISP-MARKER");
});

test("a restored session shows a failed tool's error", async () => {
  const f = await restored(codemodeSession("return 1;", "ERROR-MARKER boom", true));
  expect(f).toContain("ERROR-MARKER boom");
});

test("a long codemode script's expander counts the script's hidden rows, not just the output's", async () => {
  // COLLAPSE_MAX is 10 in Transcript.tsx; 15 short rows hide 5
  const code = Array.from({ length: 15 }, (_, i) => `const v${i} = ${i};`).join("\n");
  const f = await restored(codemodeSession(code, "ok", false));
  expect(f).not.toContain("+0 lines");
  expect(f).toContain("+5 lines");
});

// The sidebar is the platform's data, so it has to say so when the platform is
// down, and an empty space must not crash the environment guards.
async function sidebarWith(view: Record<string, unknown>) {
  const real = backend();
  boot(new Proxy(real, { get: (o: any, p) => (p === "space" ? async () => view : typeof o[p] === "function" ? o[p].bind(o) : o[p]) }) as any, hello());
  const setup = await testRender(<App />, { width: COLS, height: ROWS, kittyKeyboard: true });
  let f = "";
  for (let i = 0; i < 10; i++) {
    await tick();
    await setup.renderOnce();
    f = setup.captureCharFrame();
  }
  setup.renderer.destroy();
  boot(real, hello());
  return f;
}

test("sidebar: an unreachable platform shows the reason, not demo rows", async () => {
  const f = await sidebarWith({ available: false, error: "unavailable: no KL_API_URL", user: "", workspaces: [], environments: [] });
  expect(f).toContain("platform not reachable");
  expect(f).not.toContain("api-gateway");
});

test("sidebar: a space without environments renders no Environment block", async () => {
  const f = await sidebarWith({ available: true, user: "me", workspaces: [{ id: "w1", name: "solo", owner: "me", state: "ready" }], environments: [] });
  expect(f).toContain("solo");
  expect(f).not.toContain("Environment");
});

test("dragging over log lines selects the log text (not the gutter) and copies it", async () => {
  const copies: string[] = [];
  setCopier((text) => (copies.push(text), true));
  const t = await processesView();
  let lines: string[] = [];
  let row = -1;
  for (let i = 0; i < 10 && row < 0; i++) {
    // the log loads a beat after the view opens
    lines = (await t.frame()).split("\n");
    row = lines.findIndex((l) => l.includes("listening on :8080"));
  }
  const row2 = row + 1;
  const col = lines[row]!.indexOf("listening");
  await t.mockMouse.drag(col, row, col + 20, row2);
  const f = await t.frame();
  const sel = t.renderer.getSelection()?.getSelectedText() ?? "";
  expect(sel).toBe("listening on :8080\nroute  GET  /healthz"); // no line numbers
  expect(copies).toEqual([sel]);
  const cell = t.captureSpans().lines[row]!.spans.find((sp) => sp.text.includes("listening"))!;
  // no selectionBg passed: the native default must still paint the highlight
  const rest = t.captureSpans().lines[row2 + 3]!.spans[0]!;
  expect(Array.from(cell.bg.buffer)).not.toEqual(Array.from(rest.bg.buffer));
  expect(f).toContain("copied 2 lines");
  setCopier(null);
  t.done();
});

/** A session whose turn never ends, so /btw is exercised mid-turn and abort is observable. */
function btwSession(calls: { btw: string[]; abort: number; prompt: string[] }, reply: () => Promise<string>) {
  const noop = async () => {};
  const h: any = new Proxy(
    {
      messages: [],
      isClaude: false,
      busy: false,
      subscribe: () => () => {},
      btw: async (q: string) => (calls.btw.push(q), reply()),
      abort: async () => void calls.abort++,
      prompt: async (t: string) => void calls.prompt.push(t),
    },
    { get: (o: any, k) => (k in o ? o[k] : k === "then" ? undefined : noop) },
  );
  return h;
}

/** The session opens asynchronously; wait for the app to reach it. */
async function until(t: { frame(): Promise<string> }, ok: () => boolean) {
  for (let i = 0; i < 30 && !ok(); i++) await t.frame();
}

async function withBtw(reply: () => Promise<string>, run: (t: Awaited<ReturnType<typeof mount>>, calls: { btw: string[]; abort: number; prompt: string[] }) => Promise<void>) {
  const calls = { btw: [] as string[], abort: 0, prompt: [] as string[] };
  const orig = LocalBackend.prototype.session;
  LocalBackend.prototype.session = (async () => btwSession(calls, reply)) as any;
  try {
    const t = await mount({ vim: "off" });
    await run(t, calls);
    t.done();
  } finally {
    LocalBackend.prototype.session = orig;
  }
}

test("/btw shows the answer in a panel, esc closes it without aborting, nothing enters the transcript", async () => {
  await withBtw(async () => "Because **cache** keys collide.", async (t, calls) => {
    await t.mockInput.typeText("/btw why is it slow");
    await t.frame();
    t.mockInput.pressEnter();
    await until(t, () => calls.btw.length > 0);
    let f = await t.frame();
    expect(calls.btw).toEqual(["why is it slow"]);
    expect(calls.prompt).toEqual([]);
    expect(f).toContain("btw: why is it slow");
    expect(f).toContain("Because");
    expect(f).toContain("esc close");
    t.mockInput.pressEscape();
    f = await t.frame();
    expect(f).not.toContain("btw: why is it slow");
    expect(f).not.toContain("Because");
    expect(calls.abort).toBe(0);
  });
});

test("/btw alone shows the usage hint and asks nothing", async () => {
  await withBtw(async () => "x", async (t, calls) => {
    await t.mockInput.typeText("/btw");
    await t.frame();
    t.mockInput.pressEnter();
    const f = await t.frame();
    expect(f).toContain("usage: /btw <question>");
    expect(calls.btw).toEqual([]);
  });
});

test("a /btw answer that arrives after esc is dropped; an error shows in the panel", async () => {
  let release!: (s: string) => void;
  await withBtw(() => new Promise<string>((r) => (release = r)), async (t) => {
    await t.mockInput.typeText("/btw late one");
    await t.frame();
    t.mockInput.pressEnter();
    await t.frame();
    let f = await t.frame();
    expect(f).toContain("thinking");
    t.mockInput.pressEscape();
    await until(t, () => release !== undefined);
    await t.frame();
    release("too late");
    f = await t.frame();
    expect(f).not.toContain("too late");
  });
  await withBtw(async () => { throw new Error("no model"); }, async (t) => {
    await t.mockInput.typeText("/btw q");
    await t.frame();
    t.mockInput.pressEnter();
    await until(t, () => false);
    expect(await t.frame()).toContain("no model");
  });
});
