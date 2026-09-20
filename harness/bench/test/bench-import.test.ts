import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import path from "node:path";

test("importing the bench does not load the capability runtime or the platform tools", () => {
  const probePath = path.join(import.meta.dirname, "fixtures", "bench-import-probe.mjs");
  const result = spawnSync(process.execPath, [probePath], { encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
  const lastLine = result.stdout.trim().split("\n").filter(Boolean).pop() ?? "";
  const counts = JSON.parse(lastLine) as { piKloudlite: number; piCatalog: number; typebox: number; capabilities: number; adapters: number };
  // typebox and pi/catalog.ts are still resolved through reader.ts (the SDK's SessionManager,
  // which reads session files) and rpc-child.ts (a pure data table); both predate the capability
  // runtime and are the bench's own job, so they are deliberately not asserted here.
  assert.equal(counts.piKloudlite, 0, JSON.stringify(counts));
  assert.equal(counts.capabilities, 0, JSON.stringify(counts));
  assert.ok(counts.adapters >= 1, JSON.stringify(counts));
});
