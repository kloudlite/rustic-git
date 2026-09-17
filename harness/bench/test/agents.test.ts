import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";

/**
 * An agent is an ask to a session that did not exist a moment ago: fresh context, one task, its
 * answer back to whoever started it. Several run at once because each has its own session.
 */
const up = async (name: string) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), name));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  const srv = await serve(bench, 0);
  await bench.start();
  return { dir, bench, srv, base: `http://127.0.0.1:${srv.port}`, down: async () => (await srv.close(), await bench.stop(), fs.rmSync(dir, { recursive: true, force: true })) };
};
const post = (base: string, p: string, body: unknown) => fetch(base + p, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });

test("an agent runs in its own ephemeral session and reports back to whoever started it", async () => {
  const t = await up("bench-agent-");
  try {
    const caller = t.bench.sessions.all().find((s) => !s.archived)!.id;
    const r = await post(t.base, "/agents", { task: "audit the routes", workspace: "api", name: "audit-1", from: caller });
    assert.equal(r.status, 202);
    const started = (await r.json()) as { session: string; name: string };
    assert.equal(started.session, "e-audit-1", "its own session, not the caller's");
    assert.equal(t.bench.sessions.get("e-audit-1")!.kind, "ephemeral");

    // No tag and no history: an agent is given the task and nothing else.
    const said = (await t.bench.messages("e-audit-1")).messages as { role: string; content: unknown }[];
    assert.equal(said[0].content, "audit the routes");

    await until(() => t.bench.exchanges.bySession(caller).some((e) => e.dir === "in"), 5_000, "its report");
    const back = (await t.bench.messages(caller)).messages as { role: string; content: string }[];
    assert.ok(back.some((m) => String(m.content).startsWith("[from agent audit-1]")), JSON.stringify(back));

    // Closing one takes its transcript with it.
    assert.equal((await fetch(`${t.base}/agents/audit-1`, { method: "DELETE" })).status, 204);
    assert.equal(t.bench.sessions.get("e-audit-1"), undefined);

    // A caller that is not a live session cannot start one.
    assert.equal((await post(t.base, "/agents", { task: "x", workspace: "api", name: "a", from: "s-999" })).status, 400);
  } finally {
    await t.down();
  }
});

test("two agents run at once, each answering its own caller", async () => {
  const t = await up("bench-agents2-");
  try {
    const one = t.bench.sessions.all().find((s) => !s.archived)!.id;
    const two = (await t.bench.create()).id;
    await Promise.all([
      post(t.base, "/agents", { task: "first job", workspace: "api", name: "a-one", from: one }),
      post(t.base, "/agents", { task: "second job", workspace: "api", name: "a-two", from: two }),
    ]);
    await until(() => t.bench.exchanges.bySession(one).some((e) => e.dir === "in") && t.bench.exchanges.bySession(two).some((e) => e.dir === "in"), 5_000, "both reports");
    assert.match(t.bench.exchanges.bySession(one).find((e) => e.dir === "in")!.text, /first job/);
    assert.match(t.bench.exchanges.bySession(two).find((e) => e.dir === "in")!.text, /second job/);
    // Two sessions, not one queue: they were never waiting on each other.
    assert.ok(t.bench.sessions.get("e-a-one") && t.bench.sessions.get("e-a-two"));
  } finally {
    await t.down();
  }
});

test("the plan is kept per session, ticked by text, and published to the desktop", async () => {
  const t = await up("bench-plan-");
  try {
    const session = t.bench.sessions.all().find((s) => !s.archived)!.id;
    const seen: unknown[] = [];
    t.bench.onEvent((ev) => ev.type === "plan" && seen.push(ev.items));
    const widget = (v: unknown) =>
      (t.bench as any).foldRow(session, { type: "extension_ui_request", method: "setWidget", widgetKey: "harness:plan", widgetLines: [JSON.stringify(v)] });

    widget({ set: [{ text: "clone the repo" }, { text: "add the endpoint" }, { text: "open a pull request" }] });
    assert.deepEqual(t.bench.plans.get(session).map((x) => [x.text, x.state]), [["clone the repo", "todo"], ["add the endpoint", "todo"], ["open a pull request", "todo"]]);

    // Moved by exact text, or by the words the model happens to quote back.
    widget({ doing: "clone the repo" });
    assert.deepEqual(t.bench.plans.get(session).map((x) => x.state), ["doing", "todo", "todo"]);
    widget({ done: "clone the repo" });
    widget({ done: "endpoint" });
    widget({ later: { text: "pull request", why: "the API is not merged yet" } });
    assert.deepEqual(t.bench.plans.get(session).map((x) => x.state), ["done", "done", "later"]);
    assert.equal(t.bench.plans.get(session)[2].why, "the API is not merged yet");
    assert.deepEqual((seen.at(-1) as { state: string }[]).map((x) => x.state), ["done", "done", "later"]);

    // Rewriting the plan keeps what is already done: only the wording changed.
    widget({ set: [{ text: "clone the repo" }, { text: "add the endpoint" }, { text: "write the tests" }] });
    assert.deepEqual(t.bench.plans.get(session).map((x) => x.state), ["done", "done", "todo"]);

    // What a window that opened late reads.
    assert.deepEqual((await (await fetch(`${t.base}/plans`)).json()), [{ session, items: t.bench.plans.get(session) }]);
  } finally {
    await t.down();
  }
});
