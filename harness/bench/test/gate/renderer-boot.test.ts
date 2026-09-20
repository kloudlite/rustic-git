import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import WebSocket from "ws";

/**
 * The desktop must START. A typecheck and a build both passed while the renderer threw
 * `Cannot access 'Bl' before initialization` at load and the window sat on the "Connecting" card
 * (owner, 2026-09-17) — a `const` read before its line ran, which only a real evaluation finds.
 *
 * So this runs the real thing: Electron, the built renderer, and CDP's own
 * `Runtime.exceptionThrown`. It uses an isolated profile and an authenticated fixture view.
 */
const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));
test("the desktop's renderer loads with no uncaught exception", { timeout: 60_000 }, async () => {
  assert.ok(fs.existsSync(path.resolve("dist/src/main.js")) && fs.existsSync(path.resolve("dist/renderer/index.html")), "build first: the harness gate runs npm run build before this test");
  const electron = path.resolve("node_modules/.bin/electron");
  assert.ok(fs.existsSync(electron), "Electron is required for the renderer boot gate");
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), "kloudlite-boot-profile-"));
  const output: string[] = [];

  const args = [".", "--remote-debugging-port=0"];
  if (typeof process.getuid === "function" && process.getuid() === 0) args.push("--no-sandbox");
  const child = spawn(electron, args, { detached: true, env: { ...process.env, NODE_ENV: "production", KL_BOOT_TEST: "1", KL_BOOT_TEST_PROFILE: profile } });
  child.stdout.on("data", (d) => output.push(String(d)));
  child.stderr.on("data", (d) => output.push(String(d)));
  let ws: WebSocket | undefined;
  const stop = () => {
    if (!child.pid) return;
    try {
      process.kill(-child.pid, "SIGKILL");
    } catch {
      child.kill("SIGKILL");
    }
  };
  try {
    let page: { webSocketDebuggerUrl: string } | undefined;
    let port: number | undefined;
    for (let i = 0; i < 30 && !page; i++) {
      await wait(500);
      const match = output.join("").match(/DevTools listening on ws:\/\/[^:]+:(\d+)\//);
      port = match ? Number(match[1]) : undefined;
      if (port) {
        page = await fetch(`http://127.0.0.1:${port}/json/list`)
          .then((r) => r.json() as Promise<{ type: string; webSocketDebuggerUrl: string }[]>)
          .then((rows) => rows.find((p) => p.type === "page"))
          .catch(() => undefined);
      }
    }
    assert.ok(page, `Electron did not open a window:\n${output.join("")}`);

    const thrown: string[] = [];
    ws = new WebSocket(page.webSocketDebuggerUrl);
    let id = 0;
    const send = (method: string, params: unknown = {}) => ws.send(JSON.stringify({ id: ++id, method, params }));
    await new Promise((r) => ws.once("open", r));
    ws.on("message", (d) => {
      const m = JSON.parse(d.toString()) as { method?: string; params?: any };
      if (m.method === "Runtime.exceptionThrown") thrown.push(String(m.params.exceptionDetails.exception?.description ?? m.params.exceptionDetails.text));
    });
    send("Runtime.enable");
    send("Page.enable");
    send("Page.reload", { ignoreCache: true });
    await wait(6000);

    /**
     * The transcript must actually SCROLL. `justify-end` on the reversed scroller made a short
     * thread fill from the top but packed the overflow past the start edge, so a long session
     * could not be scrolled at all and its last line sat under the composer (owner, on the fleet).
     * Measured in the real layout, because that bug is invisible to any unit test.
     */
    const answers = new Map<number, unknown>();
    ws.on("message", (d) => {
      const m = JSON.parse(d.toString()) as { id?: number; result?: { result?: { value?: unknown } } };
      if (m.id && m.result) answers.set(m.id, m.result.result?.value);
    });
    const evaluate = async (expression: string) => {
      const mine = ++id;
      ws.send(JSON.stringify({ id: mine, method: "Runtime.evaluate", params: { expression, returnByValue: true, awaitPromise: true } }));
      for (let i = 0; i < 40 && !answers.has(mine); i++) await wait(50);
      return answers.get(mine);
    };
    assert.equal(await evaluate("document.body.innerText.includes('boot-workspace')"), true, "the authenticated mock workspace must be rendered");
    // Fill the scroller past its own height, then try to scroll it the way a person would.
    const probe = await evaluate(`(() => {
      const el = [...document.querySelectorAll('div')].find((d) => {
        const s = getComputedStyle(d);
        return s.flexDirection === 'column-reverse' && s.overflowY === 'auto' && d.clientHeight > 0;
      });
      if (!el) return { found: false };
      const col = el.firstElementChild;
      // Fill it well past its own height, in the column the rows actually live in.
      const tall = document.createElement('div');
      tall.style.height = (el.clientHeight * 3 + 600) + 'px';
      (col || el).appendChild(tall);
      const overflows = el.scrollHeight > el.clientHeight + 8;
      const before = el.scrollTop;
      el.scrollTop = -400;                       // a reversed column scrolls to negative
      const moved = el.scrollTop !== before;
      el.scrollTop = 0;                          // back to the newest row
      tall.remove();
      return { found: true, overflows, moved, atBottom: el.scrollTop === 0, justify: getComputedStyle(el).justifyContent };
    })()`);
    const p = probe as { found?: boolean; overflows?: boolean; moved?: boolean; atBottom?: boolean; justify?: string } | undefined;
    assert.ok(p?.found, "the authenticated mock view must expose its transcript scroller");
    assert.notEqual(p.justify, "flex-end", "`justify-end` on the scroller silently kills scrolling");
    assert.ok(p.overflows, "the filled transcript must overflow its pane");
    assert.ok(p.moved, "and it must scroll");
    assert.ok(p.atBottom, "and come back to the newest row");

    ws.close();
    assert.deepEqual(thrown, [], `the renderer threw while loading:\n${thrown.join("\n---\n")}`);
  } finally {
    ws?.terminate();
    stop();
    fs.rmSync(profile, { recursive: true, force: true });
  }
});
