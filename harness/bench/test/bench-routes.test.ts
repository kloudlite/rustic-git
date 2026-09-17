import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/**
 * Every bench route the renderer calls must be admitted by main's allow-list. The list is a trust
 * boundary — the renderer may reach the bench only through it — so a route added on one side and
 * not the other is a feature that fails with "not a bench route" the first time somebody uses it
 * (the owner's screenshot: `GET /plans`).
 */
const SID = "(bench|btw-\\d+|[swe]-[a-z0-9]([a-z0-9-]*[a-z0-9])?)";
const ROUTES = new RegExp(
  fs
    .readFileSync(path.resolve("src/main.ts"), "utf8")
    .match(/const BENCH_ROUTES = new RegExp\(\s*`([^`]+)`/)![1]
    .replace(/\$\{SID\}/g, SID)
    .replace(/\\\\/g, "\\"),
);

/** `bench("GET", `/procs/${id}/output?since=${n}`)` → `GET /procs/p1/output?since=0`. */
function callsIn(src: string): string[] {
  const out: string[] = [];
  for (const m of src.matchAll(/bench(?:<[^>]*>)?\(\s*"(GET|POST|DELETE|PUT)",\s*(`[^`]*`|"[^"]*")/g)) {
    const raw = m[2].slice(1, -1);
    if (raw.includes("${route")) continue; // built elsewhere and checked there
    const filled = raw
      .replace(/\$\{encodeURIComponent\([^)]*\)\}/g, "sample")
      .replace(/\$\{[^}]*since[^}]*\}/g, "0")
      .replace(/\$\{[^}]*\}/g, "s-1");
    out.push(`${m[1]} ${filled}`);
  }
  return out;
}

test("every bench route the renderer calls is admitted by main's allow-list", () => {
  const files = ["src/renderer/App.tsx", "src/renderer/live.ts", ...fs.readdirSync("src/renderer/components").filter((f) => f.endsWith(".tsx")).map((f) => `src/renderer/components/${f}`)];
  const seen: string[] = [];
  for (const f of files) seen.push(...callsIn(fs.readFileSync(path.resolve(f), "utf8")));
  assert.ok(seen.length > 5, `found ${seen.length} bench calls — the grep stopped working`);
  for (const call of [...new Set(seen)]) assert.ok(ROUTES.test(call), `${call} is not in main.ts's allow-list`);
});

test("the allow-list admits what the bench actually serves, and nothing wider", () => {
  for (const ok of ["GET /plans", "GET /memory", "DELETE /memory/uses-fish", "GET /procs/p1/output?since=0", "POST /proposals/p-1", "GET /sessions/s-1/tools", "GET /exchanges?session=s-1"]) {
    assert.ok(ROUTES.test(ok), ok);
  }
  // The renderer has no business reaching these, whatever it asks.
  for (const no of ["DELETE /sessions", "GET /", "POST /agents", "GET /memory/../secrets", "POST /procs/p1/watch"]) {
    assert.ok(!ROUTES.test(no), `${no} should not be admitted`);
  }
});

/**
 * The Files tab reads a workspace's own files through the bench: `/fs/tree` and `/fs/changes`,
 * scope-resolved to that workspace's tool server. The desktop drew an empty FILES section because
 * nothing ever asked for them (owner, 2026-09-17).
 */
test("the fs reads are admitted, and only reads", () => {
  const admits = (route: string) => ROUTES.test(route);
  assert.ok(admits("GET /fs/tree?scope=ws-30b60ec83f5ff77f"));
  assert.ok(admits("GET /fs/tree?scope=ws-1&path=src"));
  assert.ok(admits("GET /fs/changes?scope=bench"));
  assert.ok(!admits("POST /fs/tree?scope=ws-1"), "the console reads a workspace; it never writes one");
  assert.ok(!admits("GET /fs/../etc/passwd"));
});

/**
 * The file view reads the workspace's own files the way the tree does: read-only `GET` on the
 * `/fs/*` routes, nothing else. Clicking a file said "this build does not reach" the tool server
 * because nothing ever asked it (owner, 2026-09-18).
 */
test("the file, diff and stat reads are admitted, and only as reads", () => {
  const admits = (route: string) => ROUTES.test(route);
  assert.ok(admits("GET /fs/file?scope=ws-30b60ec83f5ff77f&path=src/index.ts"));
  assert.ok(admits("GET /fs/file?scope=ws-1&path=src/a.ts&etag=W/%22123%22"));
  assert.ok(admits("GET /fs/diff?scope=ws-1&path=src/a.ts"));
  assert.ok(admits("GET /fs/stat?scope=ws-1&path=src/a.ts"));
  for (const no of ["POST /fs/file?scope=ws-1&path=a", "DELETE /fs/file?scope=ws-1&path=a", "PUT /fs/diff?scope=ws-1"])
    assert.ok(!admits(no), no);
});
