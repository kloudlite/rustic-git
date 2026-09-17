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

/**
 * The QUEUED panel showed the owner's own card answers ("We should create a new project",
 * "TypeScript (Node)"): the answer was ALSO sent to pi, and mid-turn that is a `follow_up`, so it
 * sat in pi's queue as if it were a new instruction. The wake is the whole delivery.
 */
test("a card answer wakes the tool and queues nothing", async () => {
  const b = await startBench();
  try {
    const session = b.bench.sessions.all().find((s) => !s.archived)!.id;
    const log = path.join(b.cmds, `commands-${session}.json`);
    await until(() => fs.existsSync(log), 5_000, "the child to start");
    (b.bench as unknown as { foldRow: (id: string, ev: unknown) => void }).foldRow(session, {
      type: "extension_ui_request",
      method: "setWidget",
      widgetKey: "harness:proposal",
      widgetLines: [JSON.stringify({ id: "q-9", tool: "question", args: {}, summary: "Which language?", question: { header: "Stack", options: [{ label: "TypeScript (Node)", description: "" }] } })],
    });
    // The tool call is awaiting this; the answer is what it returns.
    const waiting = fetch(`${b.base}/proposals/q-9/wait`).then((r) => r.json() as Promise<{ answer: string }>);
    await new Promise((r) => setTimeout(r, 30));
    await post(b, "/proposals/q-9", { answer: "TypeScript (Node)" });
    assert.deepEqual(await waiting, { answer: "TypeScript (Node)" }, "the wake carries the answer");
    await new Promise((r) => setTimeout(r, 150));
    const sent = JSON.parse(fs.readFileSync(log, "utf8")) as { type: string }[];
    assert.deepEqual(sent.filter((c) => ["prompt", "steer", "follow_up"].includes(c.type)), [], "nothing is queued for the model");
  } finally {
    await b.down();
  }
});

/** A card answered after its tool stopped waiting is a 409, and still never a prompt. */
test("a late card answer is refused, not spoken", async () => {
  const b = await startBench();
  try {
    const r = await fetch(`${b.base}/proposals/gone`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ answer: "yes" }) });
    assert.equal(r.status, 409);
  } finally {
    await b.down();
  }
});

/** Answering a proposal wakes the waiting tool; the model is never sent a prompt for it. */
test("answering a proposal speaks no prompt to the model", async () => {
  const b = await startBench();
  try {
    const session = b.bench.sessions.all().find((s) => !s.archived)!.id;
    const log = path.join(b.cmds, `commands-${session}.json`);
    await until(() => fs.existsSync(log), 5_000, "the child to start");
    (b.bench as unknown as { foldRow: (id: string, ev: unknown) => void }).foldRow(session, {
      type: "extension_ui_request",
      method: "setWidget",
      widgetKey: "harness:proposal",
      widgetLines: [JSON.stringify({ id: "p-1", tool: "question", args: {}, summary: "Which database?", question: { header: "Storage", options: [{ label: "postgres", description: "" }] } })],
    });
    await post(b, "/proposals/p-1", { answer: "postgres" });
    await new Promise((r) => setTimeout(r, 150));
    const sent = JSON.parse(fs.readFileSync(log, "utf8")) as { type: string }[];
    assert.deepEqual(sent.filter((c) => ["prompt", "steer", "follow_up"].includes(c.type)), [], "the answer is the tool's result, not a message");
  } finally {
    await b.down();
  }
});

/**
 * D1 (CRITICAL, api-test-report 7.3). `POST /sessions/{id}/model {"model":"deepseek/nope-9000"}`
 * answered 200, the next turn died with an empty `agent_end` (the provider's 400 only in the
 * session file), and the bad pick wrote THROUGH to `/defaults` — so every session created after it
 * was born dead, and correcting the row did not revive one. A model nobody can answer with is not
 * a pick: it is refused, and nothing is written.
 */
test("a model that does not exist is refused, and never reaches the row or the defaults", async () => {
  const b = await startBench();
  try {
    const before = (await get(b, "/defaults")) as Record<string, unknown>;
    const a = (await post(b, "/sessions", {})) as { id: string };
    const r = await fetch(`${b.base}/sessions/${a.id}/model`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "deepseek/nope-9000" }),
    });
    assert.equal(r.status, 409, "an unknown model is refused");
    const said = ((await r.json()) as { error?: string }).error ?? "";
    assert.match(said, /no such model/i);
    // Nothing was written: not the session's row, and above all not the bench-wide default.
    const row = ((await get(b, "/sessions")) as { id: string; model?: string }[]).find((x) => x.id === a.id)!;
    assert.notEqual(row.model, "deepseek/nope-9000", "the row keeps the model that works");
    assert.deepEqual(await get(b, "/defaults"), before, "the default is untouched: new sessions are not born dead");
  } finally {
    await b.down();
  }
});

/** A model the catalogue DOES carry is still accepted, so the guard is not a wall. */
test("a known model is still picked", async () => {
  const b = await startBench();
  try {
    const a = (await post(b, "/sessions", {})) as { id: string };
    const row = (await post(b, `/sessions/${a.id}/model`, { model: "deepseek/deepseek-chat" })) as Record<string, unknown>;
    assert.equal(row.model, "deepseek/deepseek-chat");
    assert.equal(((await get(b, "/defaults")) as Record<string, unknown>).model, "deepseek/deepseek-chat");
  } finally {
    await b.down();
  }
});

/**
 * D1, second half: a turn the PROVIDER refused must never look like an empty answer. pi reports it
 * as `stopReason: "error"` with `errorMessage` on the message, not as `agent_end.error`, so reading
 * only the latter left the caller and the person with silence and an empty bench log.
 */
test("a provider error on a turn is surfaced, never silence", async () => {
  const b = await startBench();
  try {
    const seen: Record<string, unknown>[] = [];
    b.bench.onEvent((ev) => void seen.push(ev as Record<string, unknown>));
    const session = b.bench.sessions.all().find((s) => !s.archived)!.id;
    (b.bench as unknown as { foldRow: (id: string, ev: unknown) => void }).foldRow(session, {
      type: "agent_end",
      messages: [{ role: "assistant", content: [], stopReason: "error", errorMessage: "400: The supported API model names are deepseek-flash, deepseek-v4-pro, but you passed nope-9000." }],
    });
    const err = seen.find((e) => e.type === "turn_error");
    assert.ok(err, "the refusal reaches the person as a turn error");
    assert.match(String(err!.text), /nope-9000/, "and says what the provider actually said");
    assert.equal(err!.session, session);
  } finally {
    await b.down();
  }
});

/**
 * D5. Slash text arriving over the RPC socket was forwarded to the model verbatim: `/model`,
 * `/clear` and `/help` each burned a turn explaining the bench has no slash commands, and `/help`
 * enumerated the internal tool names to the person. No slash line reaches pi.
 */
test("a slash line over rpc never reaches the model", async () => {
  const b = await startBench();
  try {
    const session = b.bench.sessions.all().find((s) => !s.archived)!.id;
    const log = path.join(b.cmds, `commands-${session}.json`);
    await until(() => fs.existsSync(log), 5_000, "the child to start");
    const r = (await b.bench.rpc(session, { type: "prompt", message: "/help" })) as { success?: boolean; error?: string };
    assert.equal(r.success, false, "refused, not forwarded");
    assert.match(String(r.error), /does not take slash commands/);
    const sent = JSON.parse(fs.readFileSync(log, "utf8")) as { type: string; message?: string }[];
    assert.ok(!sent.some((c) => typeof c.message === "string" && c.message.startsWith("/")), "nothing starting with / was sent to pi");
    // `/clear` and `/compact` are the two the bench honours itself.
    await b.bench.rpc(session, { type: "prompt", message: "/compact" });
    const after = JSON.parse(fs.readFileSync(log, "utf8")) as { type: string; message?: string }[];
    assert.ok(after.some((c) => c.type === "compact"), "/compact is honoured as the command it is");
    assert.ok(!after.some((c) => c.message === "/compact"), "and never said to the model");
  } finally {
    await b.down();
  }
});

/** D10. A broken body was read as an empty one, so `{not json` created a real session. */
test("a malformed JSON body is refused, not read as empty", async () => {
  const b = await startBench();
  try {
    const before = ((await get(b, "/sessions")) as unknown[]).length;
    const r = await fetch(`${b.base}/sessions`, { method: "POST", headers: { "content-type": "application/json" }, body: "{not json" });
    assert.equal(r.status, 400);
    assert.match(String(((await r.json()) as { error?: string }).error), /json/i);
    assert.equal(((await get(b, "/sessions")) as unknown[]).length, before, "and nothing was created");
    // An EMPTY body is still a valid create: it means "no triple of my own".
    assert.equal((await fetch(`${b.base}/sessions`, { method: "POST" })).status, 201);
  } finally {
    await b.down();
  }
});

/** D11. `/exchanges?session=<unknown>` answered `200 []`, which reads as "nothing yet". */
test("exchanges for a session that does not exist is a 404", async () => {
  const b = await startBench();
  try {
    const session = b.bench.sessions.all().find((s) => !s.archived)!.id;
    assert.equal((await fetch(`${b.base}/exchanges?session=${session}`)).status, 200, "a real session still answers");
    const r = await fetch(`${b.base}/exchanges?session=s-nope`);
    assert.equal(r.status, 404);
  } finally {
    await b.down();
  }
});

/** D12. A card answered twice answered 200 twice; the second is a conflict, not a second answer. */
test("answering a card twice is a conflict", async () => {
  const b = await startBench();
  try {
    const session = b.bench.sessions.all().find((s) => !s.archived)!.id;
    (b.bench as unknown as { foldRow: (id: string, ev: unknown) => void }).foldRow(session, {
      type: "extension_ui_request",
      method: "setWidget",
      widgetKey: "harness:proposal",
      widgetLines: [JSON.stringify({ id: "p-twice", tool: "question", args: {}, summary: "Which?", question: { header: "x", options: [{ label: "a", description: "" }] } })],
    });
    const once = await fetch(`${b.base}/proposals/p-twice`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ answer: "a" }) });
    assert.equal(once.status, 200);
    const twice = await fetch(`${b.base}/proposals/p-twice`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ answer: "b" }) });
    assert.equal(twice.status, 409, "the first answer stands");
  } finally {
    await b.down();
  }
});
