import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";

const mk = (dir: string) => new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), "bench-threads-"));
type State = { argv: string[]; tools: string; team: string; sessionFile: string };

test("a workspace thread runs its own pi on the workspace's tools, with its file under workspaces/", async () => {
  const dir = tmp();
  process.env.KL_TEAM = "acme";
  const b = mk(dir);
  try {
    await b.start();
    const s = await b.openWorkspace("api");
    assert.equal(s.id, "w-api");
    assert.equal(s.kind, "workspace");
    assert.equal(s.file, path.join(dir, "workspaces", "api", "thread.jsonl"));
    const st = (await b.rpc("w-api", { type: "get_state" })).data as State;
    assert.equal(st.tools, "api");
    assert.equal(st.team, "acme");
    assert.equal(st.sessionFile, s.file);
    assert.equal(st.argv[st.argv.indexOf("--tools") + 1], "read,write,edit,bash,grep,find,ls,kl_pkg_list,kl_pkg_add,kl_pkg_rm,kl_pkg_update");
    const exts = st.argv.filter((_, i) => st.argv[i - 1] === "-e");
    assert.deepEqual(exts.map((x) => path.basename(x)), ["workspace-tools.ts", "kloudlite.ts"], "the workspace's own tools, and its own packages");
    assert.equal(st.argv.includes("--no-builtin-tools"), false, "a workspace session keeps its allow-listed tools");
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
    await until(() => b.busy(), 5_000, "the thread's turn to start");
  } finally {
    await b.stop();
  }
});

test("an ephemeral id already held under one workspace is refused under another", async () => {
  const dir = tmp();
  const b = mk(dir);
  try {
    await b.start();
    const first = await b.openEphemeral("a", "x");
    await assert.rejects(b.openEphemeral("b", "x"), /ephemeral x belongs to a/);
    assert.equal((await b.openEphemeral("a", "x")).file, first.file, "the same workspace reopens it");
    assert.ok(!fs.existsSync(path.join(dir, "workspaces", "b", "eph", "x.jsonl")));
  } finally {
    await b.stop();
  }
});

test("deleting a workspace's last thread removes its empty directory", async () => {
  const dir = tmp();
  const b = mk(dir);
  try {
    await b.start();
    await b.openWorkspace("api");
    await b.openEphemeral("api", "e1");
    const ws = path.join(dir, "workspaces", "api");
    await b.remove("e-e1", true);
    assert.ok(fs.existsSync(ws), "the workspace's own thread still lives there");
    await b.remove("w-api", true);
    assert.ok(!fs.existsSync(ws));
    assert.deepEqual(b.sessions.all().map((s) => s.id), ["s-1"]);
  } finally {
    await b.stop();
  }
});

test("a thread reopens after a restart and never counts as the bench's open session", async () => {
  const dir = tmp();
  const a = mk(dir);
  let file: string | undefined;
  try {
    await a.start();
    file = (await a.openWorkspace("api")).file;
  } finally {
    await a.stop();
  }
  const b = mk(dir);
  try {
    await b.start();
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
