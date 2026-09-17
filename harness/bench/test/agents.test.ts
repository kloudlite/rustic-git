import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { INFO_TOOLS } from "../src/rpc-child.ts";
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
    // The exchange row keeps the whole reply; what crossed over is the standup version of it.
    assert.match(t.bench.exchanges.bySession(caller).find((e) => e.dir === "in")!.text, /audit the routes/);

    // Closing one takes its transcript with it.
    assert.deepEqual(await (await fetch(`${t.base}/agents/audit-1`, { method: "DELETE" })).json(), { closed: "audit-1" });
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

test("an isolated agent works in a clone, and closing it says which clone to delete", async () => {
  const t = await up("bench-iso-");
  try {
    const caller = t.bench.sessions.all().find((s) => !s.archived)!.id;
    // The extension clones first and hands the clone's id here; the session then targets the CLONE's
    // tool server, which is what keeps two agents changing files at once out of each other's way.
    const r = await post(t.base, "/agents", { task: "upgrade to svelte 5", workspace: "svelte-app", clone: "svelte-app-eph-9f2a", name: "upgrade-1", from: caller });
    assert.equal(r.status, 202);
    const started = (await r.json()) as { session: string; name: string; clone?: string };
    assert.deepEqual([started.session, started.name, started.clone], ["e-upgrade-1", "upgrade-1", "svelte-app-eph-9f2a"]);
    assert.equal(t.bench.sessions.get("e-upgrade-1")!.target, "upgrade-1");
    assert.equal(t.bench.sessions.get("e-upgrade-1")!.workspace, "svelte-app-eph-9f2a", "its session lives under the clone");
    assert.equal(t.bench.cloneOf("upgrade-1"), "svelte-app-eph-9f2a");

    // Two at once, each in its own clone.
    await post(t.base, "/agents", { task: "audit the routes", workspace: "svelte-app", clone: "svelte-app-eph-77bb", name: "audit-2", from: caller });
    assert.deepEqual(t.bench.agentsOf(caller).sort(), ["audit-2", "upgrade-1"]);

    // Closing one names its clone, which is the agent's scratch and goes with it.
    assert.deepEqual(await (await fetch(`${t.base}/agents/upgrade-1`, { method: "DELETE" })).json(), { closed: "upgrade-1", clone: "svelte-app-eph-9f2a" });
    assert.equal(t.bench.cloneOf("upgrade-1"), undefined);
    assert.deepEqual(t.bench.agentsOf(caller), ["audit-2"]);
  } finally {
    await t.down();
  }
});

test("a live agent is resumed by name, with everything it has done", async () => {
  const t = await up("bench-resume-");
  try {
    const caller = t.bench.sessions.all().find((s) => !s.archived)!.id;
    await post(t.base, "/agents", { task: "first pass", workspace: "api", name: "impl-1", from: caller });
    await until(() => t.bench.exchanges.bySession(caller).some((e) => e.dir === "in"), 5_000, "its first report");

    // "one more thing" — the same session, not a second agent with no memory of the first pass.
    const again = await post(t.base, "/workspaces/impl-1/ask", { text: "fix round: the tests fail", from: caller });
    assert.equal(again.status, 202);
    assert.equal((await again.json() as { session: string }).session, "e-impl-1");
    assert.equal(t.bench.sessions.all().filter((s) => s.kind === "ephemeral").length, 1, "resumed, not replaced");

    const said = (await t.bench.messages("e-impl-1")).messages as { role: string; content: unknown }[];
    assert.equal(said[0].content, "first pass", "its first brief is still there");
    assert.ok(said.some((m) => String(m.content).includes("fix round: the tests fail")));
  } finally {
    await t.down();
  }
});

test("an agent can be given its own model", async () => {
  const t = await up("bench-model-");
  try {
    const caller = t.bench.sessions.all().find((s) => !s.archived)!.id;
    await post(t.base, "/agents", { task: "a big refactor", workspace: "api", name: "big-1", model: "anthropic/opus", from: caller });
    assert.equal(t.bench.sessions.get("e-big-1")!.model, "anthropic/opus");
  } finally {
    await t.down();
  }
});

test("closing a working agent lets it stop its own turn first", async () => {
  const t = await up("bench-abort-");
  try {
    const caller = t.bench.sessions.all().find((s) => !s.archived)!.id;
    // "hang" never ends its turn: the agent is still working when the close arrives.
    await post(t.base, "/agents", { task: "hang", workspace: "api", name: "slow-1", from: caller });
    await until(() => t.bench.busy(), 5_000, "it is working");

    const closed = await fetch(`${t.base}/agents/slow-1`, { method: "DELETE" });
    assert.equal(closed.status, 200);
    assert.equal(t.bench.sessions.get("e-slow-1"), undefined, "its session goes with it");
    assert.equal(t.bench.busy(), false, "and nothing of it is left running");
  } finally {
    await t.down();
  }
});

test("an agent that is not working is closed without waiting for anything", async () => {
  const t = await up("bench-abort-idle-");
  try {
    const caller = t.bench.sessions.all().find((s) => !s.archived)!.id;
    await post(t.base, "/agents", { task: "quick job", workspace: "api", name: "fast-1", from: caller });
    await until(() => t.bench.exchanges.bySession(caller).some((e) => e.dir === "in"), 5_000, "its report");
    const began = Date.now();
    await fetch(`${t.base}/agents/fast-1`, { method: "DELETE" });
    assert.ok(Date.now() - began < 2_000, "an idle agent is not waited on");
  } finally {
    await t.down();
  }
});

test("an information ask is answered by a read-only fork, and never touches the workspace's queue", async () => {
  const t = await up("bench-info-");
  try {
    const caller = t.bench.sessions.all().find((s) => !s.archived)!.id;
    // The workspace is working on something of its own, and stays working on it.
    await post(t.base, "/workspaces/api/ask", { text: "hang", from: caller });
    await until(() => t.bench.exchanges.bySession(caller)[0].state === "running", 5_000, "its work started");
    const before = (await t.bench.messages("w-api")).messages.length;

    const argvFile = path.join(t.dir, "argv.json");
    process.env.FAKE_PI_ARGV_FILE = argvFile;
    let r: Response;
    try {
      r = await post(t.base, "/workspaces/api/ask", { text: "which routes have no auth?", kind: "info", from: caller });
    } finally {
      delete process.env.FAKE_PI_ARGV_FILE;
    }
    assert.equal(r.status, 202);

    await until(() => t.bench.exchanges.bySession(caller).some((e) => e.id.startsWith("info-") && e.dir === "in"), 5_000, "the answer");
    const back = (await t.bench.messages(caller)).messages as { content: string }[];
    assert.ok(back.some((m) => String(m.content).startsWith("[info from api] ")), JSON.stringify(back));

    // The fork read the workspace, not the bench: read-only tools, on that workspace's tool server.
    const argv = JSON.parse(fs.readFileSync(argvFile, "utf8")) as string[];
    assert.equal(argv[argv.indexOf("--tools") + 1], INFO_TOOLS);
    assert.ok(argv.includes("--fork"), argv.join(" "));

    // Its own session was never prompted, and its work is still outstanding.
    assert.equal((await t.bench.messages("w-api")).messages.length, before, "the queue was not touched");
    assert.equal(t.bench.exchanges.bySession(caller)[0].state, "running", "its work is still running");
    // The WORK is a plan item; the question is not — nobody is waiting on a question to finish anything.
    assert.deepEqual(t.bench.plans.get(caller).length, 1);
    assert.ok(!t.bench.plans.get(caller).some((x) => x.text.includes("which routes")), JSON.stringify(t.bench.plans.get(caller)));
  } finally {
    await t.down();
  }
});

test("a workspace with no session still answers a question, from the tools without the history", async () => {
  const t = await up("bench-info-fresh-");
  try {
    const caller = t.bench.sessions.all().find((s) => !s.archived)!.id;
    assert.equal(t.bench.sessions.get("w-cold"), undefined);
    await post(t.base, "/workspaces/cold/ask", { text: "what is in here?", kind: "info", from: caller });
    await until(() => t.bench.exchanges.bySession(caller).some((e) => e.dir === "in"), 5_000, "the answer");
    // Answering a question never opened a session for it.
    assert.equal(t.bench.sessions.get("w-cold"), undefined, "no session was created to answer a question");
  } finally {
    await t.down();
  }
});
