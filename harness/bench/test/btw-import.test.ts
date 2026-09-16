import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";

const mk = () => new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-bi-")), readOnly: false, model: "fake/m", bin: FAKE });
/** btw needs s-1's file, which the bench learns from pi's get_state. */
const filed = (b: Bench) => until(() => b.sessions.get("s-1")?.file, 5_000, "s-1's file");

test("btw answers from a fork, is kept under btw/, and its child is gone", async () => {
  const b = mk();
  try {
    await b.start();
    await filed(b);
    const argvFile = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "bench-argv-")), "argv.json");
    process.env.FAKE_PI_ARGV_FILE = argvFile;
    let a: { id: string; question: string; entries: unknown[]; at: number };
    try {
      a = await b.btw("s-1", "what is this");
    } finally {
      delete process.env.FAKE_PI_ARGV_FILE;
    }
    assert.equal(a.id, "btw-1");
    assert.deepEqual((a.entries as { role: string }[]).map((m) => m.role), ["user", "assistant"]);
    assert.deepEqual(b.listBtw("s-1").map((x) => x.question), ["what is this"]);
    // the ruling: the btw fork has no tools at all — it answers one question from the transcript.
    const argv = JSON.parse(fs.readFileSync(argvFile, "utf8")) as string[];
    assert.ok(argv.includes("--no-tools"), argv.join(" "));
    assert.equal(argv.indexOf("--tools"), -1);
    // Loaded only so the fork is told what it is; `--no-tools` means it registers nothing.
    assert.deepEqual(argv.filter((_, i) => argv[i - 1] === "-e").map((x) => path.basename(x)), ["kloudlite.ts"]);
  } finally {
    await b.stop();
  }
});

test("btw stops a hung fork and rejects once its bounded wait expires", async () => {
  const b = mk();
  try {
    await b.start();
    await filed(b);
    await assert.rejects(b.btw("s-1", "hang", 50), /timed out/);
  } finally {
    await b.stop();
  }
});

test("import copies files, merges rows, and a re-run is a no-op", async () => {
  const b = mk();
  try {
    const row = { id: "bench", name: "fix login", seq: 1, created: 1, lastActive: 1, archived: false, file: "/Users/x/.pi/agent/sessions/--h--/a.jsonl" };
    const content = JSON.stringify({ type: "session", version: 3, id: "a", timestamp: new Date().toISOString(), cwd: "/Users/x" }) + "\n";
    const first = b.import([{ row, name: "a.jsonl", content }], [{ name: "loose.jsonl", content }]);
    assert.deepEqual(first, { added: ["bench"], files: 2 });
    assert.equal(b.sessions.get("bench")!.file, path.join((b as unknown as { opts: { dir: string } }).opts.dir, "sessions", "a.jsonl"));
    assert.deepEqual(b.import([{ row, name: "a.jsonl", content }], [{ name: "loose.jsonl", content }]), { added: [], files: 0 });
    await b.start();
    assert.ok(!b.sessions.all().some((s) => s.id === "s-1"), "an imported live session means no fresh one");
  } finally {
    await b.stop();
  }
});

test("concurrent btw calls get distinct ids, and no fork file is left behind", async () => {
  const b = mk();
  try {
    await b.start();
    await filed(b);
    const [x, y] = await Promise.all([b.btw("s-1", "one"), b.btw("s-1", "two")]);
    assert.notEqual(x.id, y.id);
    assert.deepEqual(b.listBtw("s-1").map((a) => a.question).sort(), ["one", "two"]);
    const forks = path.join((b as unknown as { opts: { dir: string } }).opts.dir, "btw", ".forks");
    assert.deepEqual(fs.existsSync(forks) ? fs.readdirSync(forks) : [], []);
  } finally {
    await b.stop();
  }
});
