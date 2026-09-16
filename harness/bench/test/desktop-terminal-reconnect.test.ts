import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { BenchClient } from "../../src/bench-client.ts";
import { DELAYS, GIVE_UP_MS, STEADY, banner, step, type Reconnect } from "../../src/renderer/components/terminal/reconnect.ts";

/** The view's own loop, on a clock we own: drop, beat every second, dial when it says to. */
function run(opts: { connected: (t: number) => boolean; forMs: number; dataAt?: number }) {
  let now = 0;
  let s: Reconnect | undefined;
  const dials: number[] = [];
  ({ state: s } = step(s, { type: "drop", now }));
  for (now = 1_000; now <= opts.forMs; now += 1_000) {
    if (opts.dataAt === now) {
      ({ state: s } = step(s, { type: "data" }));
      continue;
    }
    const r = step(s, { type: "tick", now, connected: opts.connected(now) });
    s = r.state;
    // A dial that fails drops again, exactly as the socket does.
    if (r.open) (dials.push(now), ({ state: s } = step(s, { type: "drop", now })));
  }
  return { dials, state: s };
}

test("reconnect: 1, 2, 4, 8 s then every 15 s", () => {
  const { dials } = run({ connected: () => true, forMs: 60_000 });
  assert.deepEqual(dials.slice(0, 4), [1_000, 3_000, 7_000, 15_000]);
  // From there the gaps are the steady interval, not a doubling one.
  for (let i = 4; i < dials.length; i++) assert.equal(dials[i] - dials[i - 1], STEADY, `gap ${i}`);
  assert.equal(DELAYS[0], 1_000);
});

test("reconnect: a bench that is asleep is waited for, never retried against", () => {
  const { dials } = run({ connected: (t) => t > 30_000, forMs: 45_000 });
  assert.ok(dials.every((t) => t > 30_000), `dialled while offline: ${dials}`);
  // The wait spent no attempts, so the bench coming back is met at once and at
  // the first backoff, not at the fifteen-second one a half hour asleep would reach.
  assert.equal(dials[0], 31_000);
  assert.equal(dials[1] - dials[0], DELAYS[1]);
});

test("reconnect: data clears the banner and the machine", () => {
  const { state } = run({ connected: () => true, forMs: 10_000, dataAt: 4_000 });
  assert.equal(state, undefined);
});

test("reconnect: after five minutes it stops and asks for Enter", () => {
  const { state } = run({ connected: () => true, forMs: GIVE_UP_MS + 30_000 });
  assert.equal(state!.gaveUp, true);
  assert.equal(banner(state!), "[disconnected — press Enter to retry]");
  // And it really stopped: no dial in the last minute.
  const late = run({ connected: () => true, forMs: GIVE_UP_MS + 60_000 }).dials.filter((t) => t > GIVE_UP_MS + 5_000);
  assert.deepEqual(late, []);
});

test("reconnect: Enter on a given-up terminal dials at once and starts over", () => {
  const { state } = run({ connected: () => true, forMs: GIVE_UP_MS + 10_000 });
  const r = step(state, { type: "retry", now: 400_000 });
  assert.equal(r.open, true);
  assert.equal(r.state!.gaveUp, false);
  assert.equal(r.state!.firstAt, 400_000);
  assert.equal(banner(r.state!), "[disconnected — reconnecting…]");
  // Enter while it is still trying changes nothing.
  assert.deepEqual(step(r.state, { type: "retry", now: 401_000 }), { state: r.state, open: false });
});

/**
 * The listing and kill routes as the desktop dials them. The bench's own halves
 * are Task 2's; what is held here is the client's method, path and query, since
 * a wrong one 404s in a place nothing else would notice.
 */
test("BenchClient: pty sessions are listed and killed by name within a scope", async () => {
  const seen: string[] = [];
  const srv = http.createServer((req, res) => {
    seen.push(`${req.method} ${req.url}`);
    if (req.url?.startsWith("/pty/sessions?")) return void res.end(JSON.stringify([{ name: "kl-m1-1", windows: 2, attached: 1, created: 17 }]));
    if (req.method === "DELETE" && !req.url?.includes("gone")) return void ((res.statusCode = 204), res.end());
    res.statusCode = 404;
    res.end(JSON.stringify({ error: "no route" }));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const port = (srv.address() as { port: number }).port;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "desk-ptysess-"));
  const c = new BenchClient(`http://127.0.0.1:${port}`, () => undefined, path.join(dir, "cache.json"));
  try {
    assert.deepEqual(await c.ptySessions("bench"), [{ name: "kl-m1-1", windows: 2, attached: 1, created: 17 }]);
    await c.killPtySession("ws-0123456789abcdef", "kl-ws-0123456789abcdef-2");
    assert.deepEqual(seen, ["GET /pty/sessions?scope=bench", "DELETE /pty/sessions/kl-ws-0123456789abcdef-2?scope=ws-0123456789abcdef"]);
    // A 404 is reported rather than swallowed here; main decides that a session already gone is fine.
    await assert.rejects(c.killPtySession("bench", "kl-gone-9"), /no route/);
  } finally {
    c.close();
    srv.close();
  }
});
