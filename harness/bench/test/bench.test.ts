import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";

function mk() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-"));
  return new Bench({ dir, readOnly: false, model: "fake/m", turn: async () => "ok", platform: undefined });
}

test("create(top) twice refuses a second top session", async () => {
  const bench = mk();
  await bench.start();
  await bench.create("top");
  await assert.rejects(bench.create("top"), /this bench already has a top session/);
  await bench.stop();
});

test("create + send runs a turn and logs user then turn.end", async () => {
  const bench = mk();
  await bench.start();
  const s = await bench.create();
  const r = await bench.send(s.id, "hi");
  assert.equal(typeof r.turn, "number");
  await new Promise((res) => setTimeout(res, 60));
  const rows = (await bench.rows(s.id)).rows as { kind: string; answer?: string }[];
  assert.ok(rows.some((row) => row.kind === "user"));
  assert.ok(rows.some((row) => row.kind === "turn.end" && row.answer === "ok"));
  await bench.stop();
});

test("send to a closed session throws 'is closed'", async () => {
  const bench = mk();
  await bench.start();
  const s = await bench.create();
  await bench.sessions.update(s.id, { state: "closed" });
  await assert.rejects(bench.send(s.id, "hi"), /is closed/);
  await bench.stop();
});

test("children and abort routes work", async () => {
  const bench = mk();
  await bench.start();
  const top = await bench.create("top");
  assert.deepEqual(await bench.children(top.id), []);
  await bench.abort(top.id); // no running turn: a no-op, must not throw
  await bench.stop();
});

test("messages answers {messages, rows, total}", async () => {
  const bench = mk();
  await bench.start();
  const s = await bench.create();
  await bench.send(s.id, "hi");
  await new Promise((res) => setTimeout(res, 60));
  const m = await bench.messages(s.id);
  assert.ok(Array.isArray(m.messages));
  assert.ok(Array.isArray(m.rows));
  assert.equal(typeof m.total, "number");
  await bench.stop();
});

test("rpc with an unknown command type throws 'is gone'", async () => {
  const bench = mk();
  await bench.start();
  const s = await bench.create();
  await assert.rejects(bench.rpc(s.id, { type: "whatever" }), /rpc whatever is gone/);
  await bench.stop();
});

test("boot repairs a stale open turn without losing the pending user row", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-"));
  // First bench: create a session and hand-write an open turn.start with no matching turn.end,
  // as if the process died mid-turn. No real child process/PID anywhere in this test.
  const bench1 = new Bench({ dir, readOnly: false, model: "fake/m", turn: async () => "ok", platform: undefined });
  await bench1.start();
  const s = await bench1.create();
  const file = bench1.sessions.logFile(bench1.sessions.get(s.id)!);
  fs.appendFileSync(file, JSON.stringify({ kind: "user", ts: Date.now(), from: "person", text: "still pending" }) + "\n");
  fs.appendFileSync(file, JSON.stringify({ kind: "turn.start", ts: Date.now(), turn: 1 }) + "\n");
  await bench1.stop();

  // A fresh Bench over the same folder simulates the restart; its boot() must repair the log.
  const bench2 = new Bench({ dir, readOnly: false, model: "fake/m", turn: async () => "ok", platform: undefined });
  await bench2.start();
  const rows = fs.readFileSync(file, "utf8").trim().split("\n").map((l) => JSON.parse(l));
  assert.ok(rows.some((r) => r.kind === "interrupted" && r.turn === 1));
  const unreadUser = rows.filter((r) => r.kind === "user");
  assert.equal(unreadUser.length, 1); // the pending user row is still there, not consumed
  await bench2.stop();
});
