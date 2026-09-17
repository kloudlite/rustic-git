import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { Memories } from "../src/memory.ts";
import { identity } from "../../pi/kloudlite.ts";
import { FAKE } from "./fake-pi.ts";

test("a memory is a file and a line in the index; forgetting takes both", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-mem-"));
  const m = new Memories(dir);
  assert.deepEqual(m.all(), []);
  assert.equal(m.index(), "", "nothing remembered is nothing in the prompt, not an empty heading");

  m.save({ name: "deploys-from-the-pod", description: "Never run cargo on the laptop; build in the dev pod", type: "project", body: "**Why:** the laptop disk fills.\n**How to apply:** ssh to the pod first." });
  const file = fs.readFileSync(path.join(dir, "memory", "deploys-from-the-pod.md"), "utf8");
  assert.match(file, /^---\nname: deploys-from-the-pod\ndescription: Never run cargo on the laptop; build in the dev pod\ntype: project\n---\n/);
  assert.match(file, /\*\*Why:\*\* the laptop disk fills\./);
  assert.match(m.index(), /- \*\*deploys-from-the-pod\*\* \(project\) — Never run cargo on the laptop/);

  m.save({ name: "prefers-short-answers", description: "Wants one line, then the facts", type: "user", body: "No preamble." });
  assert.deepEqual(m.all().map((x) => x.name), ["deploys-from-the-pod", "prefers-short-answers"]);

  assert.deepEqual(m.forget("deploys-from-the-pod").map((x) => x.name), ["prefers-short-answers"]);
  assert.ok(!fs.existsSync(path.join(dir, "memory", "deploys-from-the-pod.md")));
  assert.ok(!m.index().includes("deploys-from-the-pod"), m.index());

  // A name becomes a file name: it is a slug and nothing else.
  for (const bad of ["../escape", "with/slash", "UPPER", ""]) assert.throws(() => m.save({ name: bad, description: "d", type: "user", body: "b" }), /named in lowercase words/);
  assert.throws(() => m.save({ name: "ok", description: "", type: "user", body: "b" }), /needs a one-line description/);
  assert.throws(() => m.save({ name: "ok", description: "d", type: "guess" as never, body: "b" }), /a memory is user, feedback, project, reference/);
  fs.rmSync(dir, { recursive: true, force: true });
});

test("the index rides in every session's identity, and a session with no memory carries none", () => {
  assert.match(identity("You are the harness.", false, "- **a** (user) — likes short answers"), /What you already know about this person:\n\n- \*\*a\*\* \(user\) — likes short answers/);
  assert.ok(!identity("You are the harness.", false, "").includes("What you already know"), "nothing remembered adds nothing");
});

test("a workspace session saves through the bench, which owns the files", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-mem-srv-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  const srv = await serve(bench, 0);
  const base = `http://127.0.0.1:${srv.port}`;
  try {
    // This is the door a workspace session uses: it has no bench filesystem of its own.
    const saved = await (await fetch(`${base}/memory`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ name: "uses-fish", description: "Their shell is fish, not bash", type: "user", body: "Write fish syntax in examples." }) })).json();
    assert.deepEqual((saved as { name: string }[]).map((m) => m.name), ["uses-fish"]);
    assert.ok(fs.existsSync(path.join(dir, "memory", "uses-fish.md")));

    const one = (await (await fetch(`${base}/memory/uses-fish`)).json()) as { text: string };
    assert.match(one.text, /Their shell is fish/);

    assert.deepEqual(await (await fetch(`${base}/memory/uses-fish`, { method: "DELETE" })).json(), []);
    assert.deepEqual(await (await fetch(`${base}/memory`)).json(), []);
  } finally {
    await srv.close();
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
