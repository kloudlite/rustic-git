import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { RpcChild, type PiEvent } from "../src/rpc-child.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";

test("a command resolves on its response; events stream in order", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-rpc-"));
  const seen: PiEvent[] = [];
  const c = new RpcChild("s-1", { dir, model: "fake/model", bin: FAKE }, (ev) => seen.push(ev));
  try {
    c.start();
    const st = await c.send({ type: "get_state" });
    assert.match((st.data as { sessionFile: string }).sessionFile, /\.jsonl$/);
    await c.send({ type: "prompt", message: "hi" });
    await until(() => seen.some((e) => e.type === "agent_end"), 5_000, "agent_end");
    assert.deepEqual(seen.filter((e) => e.type !== "response" && e.type !== "started").map((e) => e.type), ["agent_start", "message_update", "agent_end"]);
  } finally {
    await c.stop();
  }
});

test("a dead child rejects waiting and later sends", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-rpc-"));
  const seen: PiEvent[] = [];
  const c = new RpcChild("s-1", { dir, model: "m", bin: FAKE }, (ev) => seen.push(ev));
  try {
    c.start();
    await assert.rejects(c.send({ type: "prompt", message: "crash" }), /pi exited \(3\)/);
    assert.equal(c.running(), false);
    assert.ok(seen.some((e) => e.type === "exit" && e.code === 3));
  } finally {
    await c.stop();
  }
});

test("HARNESS_PI_BIN and HARNESS_PI_EXT_DIR override where pi and its extensions are found", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-rpc-"));
  const ext = path.join(dir, "ext");
  const argvFile = path.join(dir, "argv.json");
  const saved = { bin: process.env.HARNESS_PI_BIN, ext: process.env.HARNESS_PI_EXT_DIR, argv: process.env.FAKE_PI_ARGV_FILE };
  Object.assign(process.env, { HARNESS_PI_BIN: FAKE, HARNESS_PI_EXT_DIR: ext, FAKE_PI_ARGV_FILE: argvFile });
  const c = new RpcChild("s-1", { dir, model: "m" }, () => undefined);
  try {
    c.start();
    await c.send({ type: "get_state" });
    const argv = JSON.parse(fs.readFileSync(argvFile, "utf8")) as string[];
    const exts = argv.filter((_, i) => argv[i - 1] === "-e");
    assert.deepEqual(exts, ["workspace-tools.ts", "kloudlite.ts"].map((f) => path.join(ext, f)));
    // A bench session's hands are its own workspace's, never the bench container's: the built-ins
    // are gone, and what is left is exactly what the two extensions register — no allow-list.
    assert.ok(argv.includes("--no-builtin-tools"), argv.join(" "));
    assert.equal(argv.indexOf("--tools"), -1);
  } finally {
    await c.stop();
    for (const [k, v] of [["HARNESS_PI_BIN", saved.bin], ["HARNESS_PI_EXT_DIR", saved.ext], ["FAKE_PI_ARGV_FILE", saved.argv]] as const) if (v === undefined) delete process.env[k]; else process.env[k] = v;
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("tools() reads the argv: every session its own machine's plus the platform, a fork none", () => {
  const dir = "/tmp/bench-tools";
  const bench = new RpcChild("s-1", { dir, model: "m" }, () => undefined).tools();
  // A bench session has NO machine (spec §3.1): not the built-ins, which would run in the bench
  // container, and not the tool server's seven either — it was handed its own pod's loopback as
  // "its machine", and the model then asked the person to start the bench so a build could run
  // (owner, 2026-09-18).
  for (const none of ["bash", "read", "write", "process", "kl_repo_clone"]) assert.ok(!bench.includes(none), `${none}: ${bench.join(",")}`);
  // `kl_container_build` stays, as an ASK to the workspace holding the context — never an exec.
  assert.ok(bench.includes("kl_container_build") && bench.includes("kl_images"), bench.join(","));
  assert.ok(bench.includes("ask") && bench.includes("tool_search"), bench.join(","));
  assert.equal(new RpcChild("s-1", { dir, model: "m" }, () => undefined).hands().toolsAddress, undefined, "and no address to reach one at");
  const ws = new RpcChild("w-1", { dir, model: "m", file: "/tmp/x.jsonl", tools: "ws-1" }, () => undefined).tools();
  // No `--tools` allow-list any more: `tool_search` turns a deferred tool on at runtime, and an
  // allow-list would have to be kept equal to the whole catalogue by hand to admit it.
  assert.ok(ws.includes("bash") && ws.includes("kl_pkg_add") && ws.includes("tool_search"), ws.join(","));
  assert.deepEqual(new RpcChild("b-1", { dir, model: "m", fork: "/tmp/x.jsonl" }, () => undefined).tools(), []);
});

/**
 * D2 (api-test-report 1.6). A prompt sent while the child was still spawning was dropped without a
 * trace: ALPHA landed, BETA one second later left no transcript row, no queue row and no error, and
 * the caller's promise resolved empty. A warm child kept order correctly, which isolated the window
 * to the cold start — commands were written to stdin before pi had a session to answer with.
 */
test("two prompts on a cold child both land, in order", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rpc-cold-"));
  const seen: PiEvent[] = [];
  const c = new RpcChild("s-cold", { dir, model: "fake/m", bin: FAKE }, (ev) => seen.push(ev));
  try {
    c.start();
    // Straight away, with no wait for readiness: this is the window BETA fell into.
    const a = c.send({ type: "prompt", message: "say ALPHA" });
    await new Promise((r) => setTimeout(r, 1000));
    const b = c.send({ type: "prompt", message: "say BETA" });
    await Promise.all([a, b]);
    // The command is ACKED before its turn streams: wait for both turns to end, or the second
    // message_update has simply not arrived yet and the assertion is about timing, not delivery.
    await until(() => seen.filter((e) => e.type === "agent_end").length >= 2, 5_000, "both turns to end");
    const said = seen.filter((e) => e.type === "message_update").map((e) => JSON.stringify(e));
    assert.ok(said.some((t) => t.includes("ALPHA")), "ALPHA landed");
    assert.ok(said.some((t) => t.includes("BETA")), "and so did BETA: neither is dropped");
    const order = seen.filter((e) => e.type === "message_update").map((e) => (JSON.stringify(e).includes("ALPHA") ? "A" : "B"));
    assert.deepEqual(order, ["A", "B"], "in the order they were sent");

    // The stand-in pi attaches its stdin listener synchronously, so it cannot lose an early write
    // the way the real one does — this half of the test would pass without the buffer. What the
    // buffer guarantees is asserted directly: nothing is written before pi has answered once, and
    // the child primes ITSELF so a fork (created outside `open()`) is never held forever.
    const src = fs.readFileSync(new URL("../src/rpc-child.ts", import.meta.url), "utf8");
    assert.match(src, /if \(this\.ready \|\| cmd\.type === "get_state"\)/);
    assert.match(src, /if \(!this\.write\(line\)\) this\.rejectWaiting/);
    assert.match(src, /this\.pending\.push\(line\);/, "everything else is held");
    assert.match(src, /if \(ev\.type === "response" && !this\.ready\)/, "and released when pi answers");
    assert.match(src, /if \(!this\.ready\) this\.write\(JSON\.stringify\(\{ type: "get_state"/, "every child primes itself");
  } finally {
    await c.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
