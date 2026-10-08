import { expect, test } from "bun:test";
import { testRender } from "@opentui/react/test-utils";
import type { ToolDef } from "@kloudlite-tui/tools";
import { backend, boot, hello } from "./hello.ts";

// App reads its settings from hello() once at mount, so tests seed them by re-booting.
function writeSettings(patch: Record<string, unknown>) {
  boot(backend(), { ...hello(), settings: { ...hello().settings, ...patch } });
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

test("s searches file contents from the files view", async () => {
  const t = await mount();
  t.mockInput.pressKey("1"); // enter a workspace
  await t.frame();
  t.mockInput.pressKey("f"); // files view
  expect(await t.frame()).toContain("CHANGES");

  t.mockInput.pressKey("s");
  await t.frame();
  await t.mockInput.typeText("sessionKey");
  await t.frame();
  t.mockInput.pressKey("RETURN");
  const f = await t.frame();
  expect(f).toContain("search sessionKey");
  expect(f).toContain("src/sessions.ts:"); // a hit, with its line number

  t.mockInput.pressKey("ESCAPE"); // clears the search, stays in the view
  expect(await t.frame()).not.toContain("search sessionKey");
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

test("processes view lists what the workspace runs, with its logs", async () => {
  const t = await mount();
  t.mockInput.pressKey("1"); // api-gateway
  await t.frame();
  t.mockInput.pressKey("f"); // files
  await t.frame();
  t.mockInput.pressKey("f"); // processes
  const f = await t.frame();
  expect(f).toContain("2/3 running");
  expect(f).toContain("server:8080");
  expect(f).toContain("crashed (1)");
  expect(f).toContain("listening on :8080"); // selected process's log

  t.mockInput.pressKey("j"); // metrics
  await t.frame();
  t.mockInput.pressKey("j"); // tests (crashed)
  expect(await t.frame()).toContain("--- FAIL: TestRateLimit");
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
  expect(f).toContain("current snapshot: pre-rate-limits"); // labelled, on its own line
  expect(f).toContain("→ api-gateway"); // interception, in the right-hand column
  expect(f).toContain("http:8080"); // ports stay in their own column
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

// The env tools are defined once but close over a ref, so the danger is a tool
// that acts on the state of the render that defined it. Create through the tool
// and look for the workspace in the frame.
test("the env tools act on live state and reach the UI", async () => {
  const tools: ToolDef[] = [];
  const t = await testRender(<App tools={tools} />, { width: 160, height: 40 });
  const run = (name: string, input: unknown) => tools.find((t) => t.name === name)!.run(input);
  await new Promise((r) => setTimeout(r, 400));

  expect(tools.map((t) => t.name)).toContain("env_status");
  expect(await run("env_status", {})).toContain("environment:");

  expect(await run("workspace_create", { name: "probe-ws" })).toContain("created");
  await new Promise((r) => setTimeout(r, 200));
  await t.renderOnce();
  expect(await run("env_status", {})).toContain("probe-ws");
  expect(t.captureCharFrame()).toContain("probe-ws");

  // a name that does not exist has to come back with the names that do
  const bad = await run("env_connect", { name: "nope" });
  expect(bad).toContain("error");
  expect(bad).toContain("production");
}, 20000);

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
        seen.push(`open ${opts.codemode}`);
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
