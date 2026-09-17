import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import type { RpcChild } from "../src/rpc-child.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";

const mk = (dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-core-")), readOnly = false) => new Bench({ dir, readOnly, model: "fake/m", bin: FAKE });
/** Every session has told the bench its file (pi's get_state has landed). */
const filed = (b: Bench) => until(() => b.sessions.all().every((s) => s.file), 5_000, "every session's file");
/** A session's current child, so a test can wait for that exact process to exit. */
const childOf = (b: Bench, id: string) => (b as unknown as { children: Map<string, RpcChild> }).children.get(id)!;

test("start on an empty folder opens one session and records its file", async () => {
  const b = mk();
  try {
    await b.start();
    await filed(b);
    const [s] = b.sessions.all();
    assert.equal(s.id, "s-1");
    assert.match(s.file ?? "", /\.jsonl$/);
  } finally {
    await b.stop();
  }
});

test("a prompt names the session and a widget exchange lands in both views", async () => {
  const b = mk();
  try {
    await b.start();
    const seen: string[] = [];
    b.onEvent((e) => seen.push(e.type));
    await b.rpc("s-1", { type: "prompt", message: "exchange" });
    await until(() => seen.includes("agent_end"), 5_000, "agent_end");
    assert.equal(b.sessions.get("s-1")!.name, "exchange");
    assert.deepEqual(b.exchanges.bySession("s-1").map((e) => e.id), ["e1"]);
    assert.deepEqual(b.exchanges.byWorkspace("api").map((e) => e.session), ["s-1"]);
    assert.ok(seen.includes("exchange"));
  } finally {
    await b.stop();
  }
});

test("a restart reopens sessions with the same ids and files, and running tasks read lost", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-core-"));
  const a = mk(dir);
  let before: string[] = [];
  try {
    await a.start();
    await a.create();
    await filed(a);
    before = a.sessions.all().map((s) => `${s.id}:${s.file}`);
  } finally {
    await a.stop();
  }
  // written after the old children exited, as a crash would leave it
  a.tasks.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "make", state: "running", started: 1 });
  const b = mk(dir);
  try {
    await b.start();
    assert.deepEqual(b.sessions.all().map((s) => `${s.id}:${s.file}`), before);
    assert.equal(b.tasks.all().find((t) => t.id === "t1")!.state, "lost");
    assert.equal((await b.rpc("s-2", { type: "get_state" })).success, true, "the reopened session answers");
  } finally {
    await b.stop();
  }
});

test("a pi that exits loses only its own session's tasks", async () => {
  const b = mk();
  try {
    await b.start();
    await b.create();
    await filed(b);
    b.tasks.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "a", state: "running", started: 1 });
    b.tasks.transition({ id: "t2", session: "s-2", tool: "Bash", arg: "b", state: "background", started: 1 });
    await assert.rejects(b.rpc("s-1", { type: "prompt", message: "crash" }));
    await until(() => b.tasks.all().find((t) => t.id === "t1")!.state === "lost", 5_000, "t1 lost");
    assert.equal(b.tasks.all().find((t) => t.id === "t2")!.state, "background");
    assert.equal(b.busy(), true, "s-2's background task still holds the bench up");
    b.tasks.transition({ id: "t2", state: "done", ended: 2 });
    assert.equal(b.busy(), false);
  } finally {
    await b.stop();
  }
});

test("a replaced child's late exit folds nothing into its successor", async () => {
  const b = mk();
  try {
    await b.start();
    await b.create();
    await filed(b);
    b.tasks.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "make", state: "running", started: 1 });
    const old = childOf(b, "s-1");
    await b.archive("s-1");
    await b.restore("s-1");
    // RpcChild's own exit listener was registered first, so its exit has been folded (or dropped) by the time this resolves.
    await old.stop();
    assert.equal(b.tasks.all().find((t) => t.id === "t1")!.state, "running");
    assert.equal(b.busy(), true);
  } finally {
    await b.stop();
  }
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
  try {
    await b.start();
    assert.equal((await b.messages("s-1")).total, 1);
    await assert.rejects(b.rpc("s-1", { type: "prompt", message: "x" }), /read-only/);
    await assert.rejects(b.create(), /read-only/);
  } finally {
    await b.stop();
  }
});

test("busy covers a turn in flight, a background task and a live process, and nothing else", async () => {
  const b = mk();
  try {
    await b.start();
    assert.equal(b.busy(), false, "an open session with nothing running is not busy");
    const turn = new Promise((r) => b.onEvent((e) => e.type === "agent_start" && r(b.busy())));
    await b.rpc("s-1", { type: "prompt", message: "hi" });
    assert.equal(await turn, true, "a turn is busy from agent_start");
    await until(() => !b.busy(), 5_000, "idle again after agent_end");
    b.tasks.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "make", state: "background", started: 1 });
    assert.equal(b.busy(), true);
    b.tasks.transition({ id: "t1", state: "done", ended: 2 });
    b.procs.snapshot("s-1", [{ id: "p1", name: "vite", command: "npm run dev", pid: process.pid, started: 1 }]);
    assert.equal(b.busy(), true);
    b.procs.snapshot("s-1", []);
    assert.equal(b.busy(), false);
  } finally {
    await b.stop();
  }
});

test("delete refuses while something runs unless stop, then discards exchanges", async () => {
  const b = mk();
  try {
    await b.start();
    await b.rpc("s-1", { type: "prompt", message: "exchange" });
    await until(() => b.exchanges.bySession("s-1").length > 0 && !b.busy(), 5_000, "the exchange and agent_end");
    b.tasks.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "make", state: "background", started: 1 });
    await assert.rejects(b.remove("s-1", false), /in flight: Bash make/);
    const old = childOf(b, "s-1");
    await b.remove("s-1", true);
    assert.equal(b.sessions.get("s-1"), undefined);
    assert.deepEqual(b.sessions.all().map((s) => s.id), ["s-2"], "the last session is replaced by a fresh id, never zero");
    assert.deepEqual(b.exchanges.byWorkspace("api"), []);
    await old.stop();
    assert.equal(b.writable.ok(), true, "the removed session's exit folds nothing into a row that is gone");
  } finally {
    await b.stop();
  }
});

test("a prompt is refused while the folder is not writable", async () => {
  const b = mk();
  try {
    await b.start();
    assert.throws(() => b.writable.run(() => { throw new Error("EIO"); }));
    await assert.rejects(b.rpc("s-1", { type: "prompt", message: "x" }), /not writable: EIO/);
  } finally {
    await b.stop();
  }
});

test("a process nobody can ask about is lost, so the bench can idle — but a pi exit alone is not that", async () => {
  const b = mk();
  try {
    await b.start();
    await filed(b);
    b.procs.snapshot("s-1", [{ id: "p1", name: "vite", command: "npm run dev", pid: process.pid, started: 1 }]);
    assert.equal(b.busy(), true);

    // Its pi goes. The process runs on a tool server, not inside pi, so the row stands.
    await assert.rejects(b.rpc("s-1", { type: "prompt", message: "crash" }));
    assert.equal(b.procs.all().find((p) => p.id === "p1")!.ended, undefined);

    // Three sweeps with nothing answering: gone, with nobody able to say how it ended.
    for (let i = 0; i < 3; i++) await (b as unknown as { sweepProcs: () => Promise<void> }).sweepProcs();
    assert.equal(b.procs.all().find((p) => p.id === "p1")!.lost, true);
    assert.equal(b.busy(), false);
  } finally {
    await b.stop();
  }
});

test("boot skips a thread row with no file instead of crash-looping", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-core-"));
  fs.writeFileSync(path.join(dir, "sessions.json"), JSON.stringify({ nextSeq: 2, rows: [
    { id: "s-1", name: "session 1", seq: 1, created: 1, lastActive: 1, archived: false },
    { id: "w-api", name: "api", seq: 0, created: 1, lastActive: 1, archived: false, kind: "workspace", workspace: "api", target: "api" },
  ] }));
  const b = mk(dir);
  try {
    await b.start();
    await filed({ sessions: { all: () => b.sessions.all().filter((s) => s.id === "s-1") } } as unknown as Bench);
    assert.equal(childOf(b, "w-api"), undefined, "the fileless thread is not opened");
  } finally {
    await b.stop();
  }
});

test("opening a long thread reads the newest page, not the whole history", async () => {
  const b = mk();
  try {
    await b.start();
    const id = b.sessions.all().find((s) => !s.archived)!.id;
    for (let i = 0; i < 40; i++) await b.rpc(id, { type: "prompt", message: `prompt ${i}` });

    const all = await b.messages(id);
    assert.equal(all.messages.length, all.total, "everything, when everything is asked for");

    // What a window asks for when it opens a thread it has never seen.
    const page = await b.messages(id, 0, undefined, 20);
    assert.equal(page.messages.length, 20);
    assert.equal(page.total, all.total);
    assert.equal(page.from, all.total - 20, "and where they begin, so the older ones can be asked for");
    assert.deepEqual(page.messages, all.messages.slice(-20));
  } finally {
    await b.stop();
  }
});

test("a new session tells every window to forget what it cached about the old one", async () => {
  const b = mk();
  try {
    await b.start();
    await filed(b);
    const id = b.sessions.all().find((s) => !s.archived)!.id;
    await b.rpc(id, { type: "prompt", message: "something" });
    const before = b.sessions.get(id)!.file;

    const said: unknown[] = [];
    b.onEvent((ev) => ev.type === "cleared" && said.push(ev));
    // What `/clear` sends. The fake answers with a NEW file, as pi does.
    await b.rpc(id, { type: "new_session" });
    await until(() => said.length > 0, 5_000, "the cleared event");
    assert.deepEqual(said, [{ type: "cleared", session: id }]);
    // And the row follows the child to its new file, so the next read is not the old transcript.
    await until(() => b.sessions.get(id)!.file !== before, 5_000, "the row's new file");
  } finally {
    await b.stop();
  }
});
