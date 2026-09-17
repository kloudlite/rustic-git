import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";
import WebSocket from "ws";

/**
 * The desktop must START. A typecheck and a build both passed while the renderer threw
 * `Cannot access 'Bl' before initialization` at load and the window sat on the "Connecting" card
 * (owner, 2026-09-17) — a `const` read before its line ran, which only a real evaluation finds.
 *
 * So this runs the real thing: Electron, the built renderer, and CDP's own
 * `Runtime.exceptionThrown`. It skips where Electron cannot start (no display, no binary), because
 * a gate that cannot run must not pretend to pass.
 */
const PORT = 9411;
const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

test("the desktop's renderer loads with no uncaught exception", { timeout: 60_000 }, async (t) => {
  if (!fs.existsSync(path.resolve("dist/renderer/index.html"))) return t.skip("no build — run `npm run build`");
  const electron = path.resolve("node_modules/.bin/electron");
  if (!fs.existsSync(electron)) return t.skip("electron is not installed here");

  const child = spawn(electron, [".", `--remote-debugging-port=${PORT}`], { stdio: "ignore", env: { ...process.env, KL_BOOT_TEST: "1" } });
  const stop = () => child.kill("SIGKILL");
  try {
    let page: { webSocketDebuggerUrl: string } | undefined;
    for (let i = 0; i < 30 && !page; i++) {
      await wait(500);
      page = await fetch(`http://127.0.0.1:${PORT}/json/list`)
        .then((r) => r.json() as Promise<{ type: string; webSocketDebuggerUrl: string }[]>)
        .then((rows) => rows.find((p) => p.type === "page"))
        .catch(() => undefined);
    }
    if (!page) return t.skip("electron did not open a window here (no display?)");

    const thrown: string[] = [];
    const ws = new WebSocket(page.webSocketDebuggerUrl);
    let id = 0;
    const send = (method: string, params: unknown = {}) => ws.send(JSON.stringify({ id: ++id, method, params }));
    await new Promise((r) => ws.once("open", r));
    send("Runtime.enable");
    send("Page.enable");
    send("Page.reload", { ignoreCache: true });
    ws.on("message", (d) => {
      const m = JSON.parse(d.toString()) as { method?: string; params?: any };
      if (m.method === "Runtime.exceptionThrown")
        thrown.push(String(m.params.exceptionDetails.exception?.description ?? m.params.exceptionDetails.text));
    });
    await wait(6000);
    ws.close();
    assert.deepEqual(thrown, [], `the renderer threw while loading:\n${thrown.join("\n---\n")}`);
  } finally {
    stop();
  }
});
