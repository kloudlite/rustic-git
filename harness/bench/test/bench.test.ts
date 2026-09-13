import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { FAKE } from "./fake-pi.ts";

const mk = (dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-core-")), readOnly = false) => new Bench({ dir, readOnly, model: "fake/m", bin: FAKE });
const settle = () => new Promise((r) => setTimeout(r, 150));

test("start on an empty folder opens one session and records its file", async () => {
  const b = mk();
  await b.start();
  // Wait for pi's get_state rather than a fixed delay: a loaded suite outran 150 ms, and a failed assertion skipped stop() and hung the file.
  for (let i = 0; i < 100 && !b.sessions.get("s-1")?.file; i++) await settle();
  const [s] = b.sessions.all();
  assert.equal(s.id, "s-1");
  assert.match(s.file ?? "", /\.jsonl$/);
  b.stop();
});

test("a prompt names the session and a widget exchange lands in both views", async () => {
  const b = mk();
  await b.start();
  const seen: string[] = [];
  b.onEvent((e) => seen.push(e.type));
  await b.rpc("s-1", { type: "prompt", message: "exchange" });
  await settle();
  assert.equal(b.sessions.get("s-1")!.name, "exchange");
  assert.deepEqual(b.exchanges.bySession("s-1").map((e) => e.id), ["e1"]);
  assert.deepEqual(b.exchanges.byWorkspace("api").map((e) => e.session), ["s-1"]);
  assert.ok(seen.includes("exchange") && seen.includes("agent_end"));
  b.stop();
});

test("a restart reopens sessions with the same ids and files, and running tasks read lost", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-core-"));
  const a = mk(dir);
  await a.start();
  await a.create();
  await settle();
  const before = a.sessions.all().map((s) => `${s.id}:${s.file}`);
  a.stop();
  await settle();
  // written after the old children exited, as a crash would leave it
  a.tasks.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "make", state: "running", started: 1 });
  const b = mk(dir);
  await b.start();
  await settle();
  assert.deepEqual(b.sessions.all().map((s) => `${s.id}:${s.file}`), before);
  assert.equal(b.tasks.all().find((t) => t.id === "t1")!.state, "lost");
  assert.equal((await b.rpc("s-2", { type: "get_state" })).success, true, "the reopened session answers");
  b.stop();
});

test("a pi that exits loses only its own session's tasks", async () => {
  const b = mk();
  await b.start();
  await b.create();
  await settle();
  b.tasks.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "a", state: "running", started: 1 });
  b.tasks.transition({ id: "t2", session: "s-2", tool: "Bash", arg: "b", state: "background", started: 1 });
  await assert.rejects(b.rpc("s-1", { type: "prompt", message: "crash" }));
  await settle();
  assert.equal(b.tasks.all().find((t) => t.id === "t1")!.state, "lost");
  assert.equal(b.tasks.all().find((t) => t.id === "t2")!.state, "background");
  assert.equal(b.busy(), true, "s-2's background task still holds the bench up");
  b.tasks.transition({ id: "t2", state: "done", ended: 2 });
  assert.equal(b.busy(), false);
  b.stop();
});

test("a replaced child's late exit folds nothing into its successor", async () => {
  const b = mk();
  await b.start();
  await b.create();
  await settle();
  b.tasks.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "make", state: "running", started: 1 });
  await b.archive("s-1");
  await b.restore("s-1");
  await settle();
  assert.equal(b.tasks.all().find((t) => t.id === "t1")!.state, "running");
  assert.equal(b.busy(), true);
  b.stop();
});

test("read-only serves history and refuses work", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-core-"));
  fs.mkdirSync(path.join(dir, "sessions"));
  fs.writeFileSync(path.join(dir, "sessions.json"), JSON.stringify([{ id: "s-1", name: "old", seq: 1, created: 1, lastActive: 1, archived: false, file: path.join(dir, "sessions", "x.jsonl") }]));
  fs.writeFileSync(path.join(dir, "sessions", "x.jsonl"), [
    { type: "session", version: 3, id: "x", timestamp: new Date().toISOString(), cwd: "/home/kl" },
    { type: "message", id: "m1", parentId: null, timestamp: new Date().toISOString(), message: { role: "user", content: "earlier", timestamp: 1 } },
  ].map((l) => JSON.stringify(l)).join("\n") + "\n");
  const b = mk(dir, true);
  await b.start();
  assert.equal((await b.messages("s-1")).total, 1);
  await assert.rejects(b.rpc("s-1", { type: "prompt", message: "x" }), /read-only/);
  await assert.rejects(b.create(), /read-only/);
  b.stop();
});

test("busy covers a turn in flight, a background task and a live process, and nothing else", async () => {
  const b = mk();
  await b.start();
  await settle();
  assert.equal(b.busy(), false, "an open session with nothing running is not busy");
  const turn = new Promise((r) => b.onEvent((e) => e.type === "agent_start" && r(b.busy())));
  await b.rpc("s-1", { type: "prompt", message: "hi" });
  assert.equal(await turn, true, "a turn is busy from agent_start");
  await settle();
  assert.equal(b.busy(), false, "and idle again after agent_end");
  b.tasks.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "make", state: "background", started: 1 });
  assert.equal(b.busy(), true);
  b.tasks.transition({ id: "t1", state: "done", ended: 2 });
  b.procs.snapshot("s-1", [{ id: "p1", name: "vite", command: "npm run dev", pid: process.pid, started: 1 }]);
  assert.equal(b.busy(), true);
  b.procs.snapshot("s-1", []);
  assert.equal(b.busy(), false);
  b.stop();
});

test("delete refuses while something runs unless stop, then discards exchanges", async () => {
  const b = mk();
  await b.start();
  await b.rpc("s-1", { type: "prompt", message: "exchange" });
  await settle();
  b.tasks.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "make", state: "background", started: 1 });
  await assert.rejects(b.remove("s-1", false), /in flight: Bash make/);
  await b.remove("s-1", true);
  assert.equal(b.sessions.get("s-1"), undefined);
  assert.deepEqual(b.sessions.all().map((s) => s.id), ["s-2"], "the last session is replaced by a fresh id, never zero");
  assert.deepEqual(b.exchanges.byWorkspace("api"), []);
  await settle();
  assert.equal(b.writable.ok(), true, "the removed session's exit folds nothing into a row that is gone");
  b.stop();
});

test("a prompt is refused while the folder is not writable", async () => {
  const b = mk();
  await b.start();
  assert.throws(() => b.writable.run(() => { throw new Error("EIO"); }));
  await assert.rejects(b.rpc("s-1", { type: "prompt", message: "x" }), /not writable: EIO/);
  b.stop();
});
