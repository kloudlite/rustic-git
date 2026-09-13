import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { FAKE } from "./fake-pi.ts";

const mk = (dir: string) => new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), "bench-threads-"));
const settle = () => new Promise((r) => setTimeout(r, 150));
type State = { argv: string[]; tools: string; team: string; sessionFile: string };

test("a workspace thread runs its own pi on the workspace's tools, with its file under workspaces/", async () => {
  const dir = tmp();
  process.env.KL_TEAM = "acme";
  const b = mk(dir);
  await b.start();
  try {
    const s = await b.openWorkspace("api");
    assert.equal(s.id, "w-api");
    assert.equal(s.kind, "workspace");
    assert.equal(s.file, path.join(dir, "workspaces", "api", "thread.jsonl"));
    const st = (await b.rpc("w-api", { type: "get_state" })).data as State;
    assert.equal(st.tools, "api");
    assert.equal(st.team, "acme");
    assert.equal(st.sessionFile, s.file);
    assert.equal(st.argv[st.argv.indexOf("--tools") + 1], "read,write,edit,bash,grep,find,ls");
    const exts = st.argv.filter((_, i) => st.argv[i - 1] === "-e");
    assert.deepEqual(exts.map((x) => path.basename(x)), ["workspace-tools.ts"], "only the workspace's tools are loaded");
    assert.ok(fs.existsSync(s.file!));
    assert.equal((await b.openWorkspace("api")).id, "w-api", "opening twice is one thread");

    const e = await b.openEphemeral("api", "api-eph-1");
    assert.equal(e.id, "e-api-eph-1");
    assert.equal(e.file, path.join(dir, "workspaces", "api", "eph", "api-eph-1.jsonl"));
    assert.equal(e.target, "api-eph-1");
    assert.equal(((await b.rpc("e-api-eph-1", { type: "get_state" })).data as State).tools, "api-eph-1");
    for (const bad of ["../x", "A_B", "a/b", "", "-a"]) await assert.rejects(b.openWorkspace(bad), /not a workspace id/);
    await assert.rejects(b.openEphemeral("api", "a/b"), /not a workspace id/);
    await assert.rejects(b.openEphemeral("api", "A_B"), /not a workspace id/);
    assert.ok(!fs.existsSync(path.join(dir, "x")));

    // A thread's turn holds the bench up like a bench session's.
    assert.equal(b.busy(), false);
    await b.rpc("w-api", { type: "prompt", message: "hang" });
    await settle();
    assert.equal(b.busy(), true);
  } finally {
    await b.stop();
  }
});

test("a thread reopens after a restart and never counts as the bench's open session", async () => {
  const dir = tmp();
  const a = mk(dir);
  await a.start();
  const file = (await a.openWorkspace("api")).file;
  await a.stop();
  const b = mk(dir);
  await b.start();
  try {
    await settle();
    const st = (await b.rpc("w-api", { type: "get_state" })).data as State;
    assert.equal(st.sessionFile, file);
    assert.equal(st.tools, "api");
    const bench = () => b.sessions.all().filter((x) => (x.kind ?? "bench") === "bench").map((x) => x.id);
    assert.deepEqual(bench(), ["s-1"]);
    await assert.rejects(b.archive("s-1"), /only open session/);
    await b.remove("s-1", true);
    assert.deepEqual(bench(), ["s-2"], "deleting the last bench session still creates s-N");
    assert.ok(b.sessions.get("w-api"));
  } finally {
    await b.stop();
  }
});
