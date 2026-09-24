import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { reachable } from "../src/engine/ai-sdk.ts";
import type { Turn } from "../src/session.ts";

async function up(turn: Turn) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-"));
  const bench = new Bench({ dir, readOnly: false, model: "deepseek/deepseek-v4-pro", turn, platform: undefined });
  await bench.start();
  const { port, close } = await serve(bench, 0);
  const base = `http://127.0.0.1:${port}`;
  return { bench, base, close: async () => { await close(); await bench.stop(); } };
}

test("a session with no model set runs with the bench default", async () => {
  const seen: string[] = [];
  const turn: Turn = async (c) => { seen.push(c.model); return "ok"; };
  const { base, close } = await up(turn);
  const created = await (await fetch(`${base}/sessions`, { method: "POST" })).json();
  await fetch(`${base}/sessions/${created.id}/send`, { method: "POST", body: JSON.stringify({ text: "hi" }) });
  await new Promise((res) => setTimeout(res, 60));
  assert.deepEqual(seen, ["deepseek/deepseek-v4-pro"]);
  await close();
});

test("POST /sessions/{id}/model then the next turn's ctx.model is the new spec", async () => {
  const seen: string[] = [];
  const turn: Turn = async (c) => { seen.push(c.model); return "ok"; };
  const { base, close } = await up(turn);
  const priorKey = process.env.ANTHROPIC_API_KEY;
  process.env.ANTHROPIC_API_KEY = "test-key";
  try {
    const created = await (await fetch(`${base}/sessions`, { method: "POST" })).json();
    const set = await fetch(`${base}/sessions/${created.id}/model`, { method: "POST", body: JSON.stringify({ model: "anthropic/claude-sonnet-5" }) });
    assert.equal(set.status, 200);
    assert.equal((await set.json()).model, "anthropic/claude-sonnet-5");
    await fetch(`${base}/sessions/${created.id}/send`, { method: "POST", body: JSON.stringify({ text: "hi" }) });
    await new Promise((res) => setTimeout(res, 60));
    assert.deepEqual(seen, ["anthropic/claude-sonnet-5"]);
  } finally {
    if (priorKey === undefined) delete process.env.ANTHROPIC_API_KEY; else process.env.ANTHROPIC_API_KEY = priorKey;
    await close();
  }
});

test("an unreachable model spec is 422; an unknown session is 404", async () => {
  const { base, close } = await up(async () => "ok");
  const created = await (await fetch(`${base}/sessions`, { method: "POST" })).json();
  const bad = await fetch(`${base}/sessions/${created.id}/model`, { method: "POST", body: JSON.stringify({ model: "nope" }) });
  assert.equal(bad.status, 422);
  assert.ok((await bad.json()).error);
  const missing = await fetch(`${base}/sessions/does-not-exist/model`, { method: "POST", body: JSON.stringify({ model: "anthropic/claude-sonnet-5" }) });
  assert.equal(missing.status, 404);
  await close();
});

test("GET /models reports the bench default and the reachable providers", async () => {
  const { base, close } = await up(async () => "ok");
  const r = await fetch(`${base}/models`);
  assert.equal(r.status, 200);
  const j = await r.json();
  assert.equal(j.default, "deepseek/deepseek-v4-pro");
  assert.ok(Array.isArray(j.providers));
  await close();
});

test("reachable: deepseek without a key is a sentence, with a key is undefined", () => {
  assert.ok(reachable("deepseek/deepseek-v4-pro", {}));
  assert.equal(reachable("deepseek/deepseek-v4-pro", { DEEPSEEK_API_KEY: "k" }), undefined);
});

test("reachable: an OpenAI-compatible spec without JEVHARN_BASE_URL is a sentence", () => {
  assert.ok(reachable("x/y", {}));
  assert.equal(reachable("x/y", { JEVHARN_BASE_URL: "http://x" }), undefined);
});

test("reachable: a malformed spec is a sentence", () => {
  assert.ok(reachable("nope", {}));
  assert.ok(reachable("/y", {}));
});
