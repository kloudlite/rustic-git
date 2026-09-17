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

test("every provider pi lists is offered, and only DeepSeek is wired", async () => {
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
