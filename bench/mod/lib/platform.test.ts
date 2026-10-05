import { test } from "node:test";
import assert from "node:assert/strict";
import { callTool, toolsAt } from "./platform.ts";

const res = (status: number, body: unknown) => new Response(JSON.stringify(body), { status });

test("a stopped workspace is a tool error naming it, not a hang", async () => {
  const f = async () => res(409, { error: "workspace api is Stopped; start it to run tools" });
  await assert.rejects(toolsAt(f as any, "http://api", "t", "ws-1"), /is Stopped; start it/);
});

test("a rotated workspace token refetches the address once and retries once", async () => {
  let fetches = 0, refetched = 0;
  const f = async (_u: string, init: any) => (fetches++, init.headers.authorization === "Bearer new" ? res(200, { output: "ok" }) : res(401, { error: "expired" }));
  const getAt = async (fresh?: boolean) => (fresh && refetched++, { address: "10.0.0.1:7788", token: fresh ? "new" : "old" });
  const r = await callTool(f as any, getAt, "ws-1", "read", { path: "a" });
  assert.deepEqual(r.ok, true);
  assert.equal(refetched, 1);
  assert.equal(fetches, 2);
});

test("a refused connection says unreachable and start it", async () => {
  const f = async () => { throw new TypeError("fetch failed"); };
  const r = await callTool(f as any, async () => ({ address: "x:7788" }), "ws-1", "read", {});
  assert.equal(r.ok, false);
  assert.match((r as any).error, /ws-1 is unreachable.*start it/);
});

test("a tool error body passes through as the error", async () => {
  const f = async () => res(400, { error: "path escapes home" });
  const r = await callTool(f as any, async () => ({ address: "x:7788", token: "t" }), "ws-1", "read", {});
  assert.deepEqual(r, { ok: false, error: "path escapes home" });
});

test("a timed-out remote call says unreachable, not an AbortError", async () => {
  const f = async () => { throw new DOMException("The operation was aborted", "TimeoutError"); };
  const r = await callTool(f as any, async () => ({ address: "x:7788" }), "ws-1", "read", {});
  assert.equal(r.ok, false);
  assert.match((r as any).error, /ws-1 is unreachable.*start it/);
});
