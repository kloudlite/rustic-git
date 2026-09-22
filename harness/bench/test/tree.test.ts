import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";

test("top delegates to a main by workspace name; rows flow on events", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-"));
  const events: unknown[] = [];
  const bench = new Bench({
    dir, readOnly: false, model: "fake/m",
    turn: async (c) => (c.tools.some((t) => t.name === "read") ? "main did it" : c.tools.find((t) => t.name === "delegate")!.run(c.cwd, { target: "api", instruction: "ship" }, {})),
    platform: undefined,
  });
  bench.onEvent((e) => events.push(e));
  await bench.start();
  const top = await bench.create("top");
  const main = await bench.openWorkspace("api");
  await bench.send(top.id, "ship the api");
  await new Promise((r) => setTimeout(r, 60));
  const rows = await bench.rows(main.id);
  assert.ok(rows.rows.some((r: { kind: string; from?: unknown }) => r.kind === "user" && r.from === top.seq));
  assert.ok(rows.rows.some((r: { kind: string; answer?: string }) => r.kind === "turn.end" && r.answer === "main did it"));
  assert.ok((await bench.rows(top.id)).rows.some((r: { kind: string; from?: unknown }) => r.kind === "user" && r.from === main.seq));
  assert.ok(events.some((e) => (e as { type: string }).type === "row"));
  assert.deepEqual((await bench.children(top.id)).map((s) => s.tier), []); // delegation to a main is not parentage
  await bench.stop();
});
