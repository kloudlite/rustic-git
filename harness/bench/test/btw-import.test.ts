import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { BTW_TOOLS } from "../src/rpc-child.ts";
import { FAKE } from "./fake-pi.ts";

const mk = () => new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-bi-")), readOnly: false, model: "fake/m", bin: FAKE });
const settle = () => new Promise((r) => setTimeout(r, 150));

test("btw answers from a fork, is kept under btw/, and its child is gone", async () => {
  const b = mk();
  await b.start();
  await settle();
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
  // the ruling: the btw fork gets exactly read,grep,find,ls — no kl_* tools.
  const argv = JSON.parse(fs.readFileSync(argvFile, "utf8")) as string[];
  const at = argv.indexOf("--tools");
  assert.notEqual(at, -1);
  assert.equal(argv[at + 1], BTW_TOOLS);
  assert.equal(BTW_TOOLS, "read,grep,find,ls");
  b.stop();
});

test("btw stops a hung fork and rejects once its bounded wait expires", async () => {
  const b = mk();
  await b.start();
  await settle();
  await assert.rejects(b.btw("s-1", "hang", 50), /timed out/);
  b.stop();
});

test("import copies files, merges rows, and a re-run is a no-op", async () => {
  const b = mk();
  const row = { id: "bench", name: "fix login", seq: 1, created: 1, lastActive: 1, archived: false, file: "/Users/x/.pi/agent/sessions/--h--/a.jsonl" };
  const content = JSON.stringify({ type: "session", version: 3, id: "a", timestamp: new Date().toISOString(), cwd: "/Users/x" }) + "\n";
  const first = b.import([{ row, name: "a.jsonl", content }], [{ name: "loose.jsonl", content }]);
  assert.deepEqual(first, { added: ["bench"], files: 2 });
  assert.equal(b.sessions.get("bench")!.file, path.join((b as unknown as { opts: { dir: string } }).opts.dir, "sessions", "a.jsonl"));
  assert.deepEqual(b.import([{ row, name: "a.jsonl", content }], [{ name: "loose.jsonl", content }]), { added: [], files: 0 });
  await b.start();
  assert.ok(!b.sessions.all().some((s) => s.id === "s-1"), "an imported live session means no fresh one");
  b.stop();
});
