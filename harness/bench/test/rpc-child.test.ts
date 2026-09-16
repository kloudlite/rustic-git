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
    assert.deepEqual(exts, [path.join(ext, "kloudlite.ts")]);
    // A bench session has no hands in its own pod: no builtin tools, and no allow-list to widen.
    assert.ok(argv.includes("--no-builtin-tools"), argv.join(" "));
    assert.equal(argv.indexOf("--tools"), -1);
  } finally {
    await c.stop();
    for (const [k, v] of [["HARNESS_PI_BIN", saved.bin], ["HARNESS_PI_EXT_DIR", saved.ext], ["FAKE_PI_ARGV_FILE", saved.argv]] as const) if (v === undefined) delete process.env[k]; else process.env[k] = v;
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
