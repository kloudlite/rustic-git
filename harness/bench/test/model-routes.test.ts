import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";

async function startBench() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-model-"));
  const cmds = fs.mkdtempSync(path.join(os.tmpdir(), "bench-cmds-"));
  process.env.FAKE_PI_CMD_DIR = cmds;
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  const srv = await serve(bench, 0);
  const base = `http://127.0.0.1:${srv.port}`;
  return { dir, cmds, bench, base, down: async () => (await bench.stop(), await srv.close()) };
}
const post = async (b: { base: string }, p: string, body: unknown) => (await fetch(b.base + p, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) })).json();
const get = async (b: { base: string }, p: string) => (await fetch(b.base + p)).json();

test("a person's pick moves the default; a dispatch's pick does not", async () => {
  const b = await startBench();
  try {
    const a = (await post(b, "/sessions", {})) as { id: string };
    await post(b, `/sessions/${a.id}/model`, { model: "deepseek/deepseek-chat", thinking: "low" });
    const c = (await post(b, "/sessions", {})) as Record<string, unknown>;
    assert.equal(c.model, "deepseek/deepseek-chat");
    assert.equal(c.thinking, "low");
    const d = (await post(b, "/sessions", { model: "deepseek/deepseek-reasoner", default: false })) as Record<string, unknown>;
    assert.equal(((await get(b, "/defaults")) as Record<string, unknown>).model, "deepseek/deepseek-chat");
    assert.equal(d.model, "deepseek/deepseek-reasoner");
  } finally {
    await b.down();
  }
});

test("the triple is re-sent on session_start", async () => {
  const b = await startBench();
  try {
    const a = (await post(b, "/sessions", { model: "deepseek/deepseek-chat", thinking: "high" })) as { id: string };
    const sent = () => JSON.parse(fs.readFileSync(path.join(b.cmds, `commands-${a.id}.json`), "utf8")) as Record<string, unknown>[];
    await until(() => fs.existsSync(path.join(b.cmds, `commands-${a.id}.json`)) && sent().some((c) => c.type === "set_thinking_level"), 5_000, "the triple reaching pi");
    assert.ok(sent().some((c) => c.type === "set_model" && c.modelId === "deepseek-chat" && c.provider === "deepseek"));
    assert.ok(sent().some((c) => c.type === "set_thinking_level" && c.level === "high"));
  } finally {
    await b.down();
  }
});

/**
 * `GET /models` carries EVERY provider pi supports, wired or not — that is Settings' surface, where
 * a key is added. The `/model` DIALOG is a different question: it lists only the configured ones
 * (`pickerRows`, renderer-rows.test.ts), because the owner does not want a wall of providers he
 * cannot pick.
 */
test("the route offers every provider pi lists, and only DeepSeek is wired", async () => {
  const b = await startBench();
  try {
    const m = (await get(b, "/models")) as { providers: { id: string; wired: boolean }[] };
    assert.ok(m.providers.some((p) => p.id === "deepseek" && p.wired));
    assert.ok(m.providers.some((p) => p.id === "anthropic" && !p.wired));
    assert.ok(m.providers.some((p) => p.id === "github-copilot" && !p.wired));
  } finally {
    await b.down();
  }
});

test("an effort-only pick keeps the session's model", async () => {
  const b = await startBench();
  const a = await post(b, "/sessions", {});
  await post(b, `/sessions/${a.id}/model`, { model: "deepseek/deepseek-chat" });
  const row = await post(b, `/sessions/${a.id}/model`, { effort: "low" });
  assert.equal(row.model, "deepseek/deepseek-chat");
  assert.equal(row.effort, "low");
  await b.down();
});

/**
 * Owner: "don't spoil the session with this data". A person stopping a process or cancelling a task
 * is an HTTP call to the bench; pi must never be sent a prompt/steer/follow_up for it, because such
 * a message lands in its context AND its session file, and the reopen replays it forever.
 */
test("stopping and cancelling never speak to the model", async () => {
  const b = await startBench();
  try {
    const a = (await post(b, "/sessions", {})) as { id: string };
    const log = path.join(b.cmds, `commands-${a.id}.json`);
    await until(() => fs.existsSync(log), 5_000, "the child to start");
    const before = JSON.parse(fs.readFileSync(log, "utf8")) as { type: string }[];
    assert.equal(before.filter((c) => ["prompt", "steer", "follow_up"].includes(c.type)).length, 0);

    const r = await fetch(`${b.base}/tasks/nope/cancel`, { method: "POST", headers: { "content-type": "application/json" }, body: "{}" });
    assert.equal(r.status, 400, "an unknown task is an error, not a prompt");

    const after = JSON.parse(fs.readFileSync(log, "utf8")) as { type: string }[];
    assert.deepEqual(
      after.filter((c) => ["prompt", "steer", "follow_up"].includes(c.type)),
      [],
      "no command of these actions is ever spoken to the model",
    );
  } finally {
    await b.down();
  }
});
