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
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";

/**
 * An agent is an ask to a session that did not exist a moment ago: fresh context, one task, its
 * answer back to whoever started it. Several run at once because each has its own session.
 */
/**
 * What the platform was asked for, per test: the tree path is the bench's own `/v1` call, and the
 * one assertion that matters most here is a NEGATIVE — an agent never clones a workspace (spec §4.1).
 */
type Seen = { method: string; path: string; body?: unknown };
const up = async (name: string) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), name));
  const seen: Seen[] = [];
  // A stand-in tool server: it answers `/fs/stat` for any tree, which is how the bench learns the
  // node agent has cut one.
  const tools = createServer((req, res) => (res.writeHead(200, { "content-type": "application/json" }), res.end("{}")));
  await new Promise<void>((ok) => tools.listen(0, "127.0.0.1", ok));
  const at = `127.0.0.1:${(tools.address() as AddressInfo).port}`;
  const bench = new Bench({
    dir,
    readOnly: false,
    model: "fake/m",
    bin: FAKE,
    resolveTools: async () => at,
    platform: async (method, p, body) => (seen.push({ method, path: p, body }), { status: method === "POST" ? 202 : 202, data: null }),
  });
  const srv = await serve(bench, 0);
  await bench.start();
  return {
    dir,
    bench,
    srv,
    seen,
    base: `http://127.0.0.1:${srv.port}`,
    down: async () => (await srv.close(), await bench.stop(), await new Promise<void>((ok) => tools.close(() => ok())), fs.rmSync(dir, { recursive: true, force: true })),
  };
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
    assert.deepEqual(await (await fetch(`${t.base}/agents/audit-1`, { method: "DELETE" })).json(), { closed: "audit-1", workspace: "api", tree: "audit-1" });
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

test("an agent works in a TREE of the workspace, and never a clone of it", async () => {
  const t = await up("bench-tree-");
  try {
    const caller = t.bench.sessions.all().find((s) => !s.archived)!.id;
    const r = await post(t.base, "/agents", { task: "upgrade to svelte 5", workspace: "svelte-app", name: "upgrade-1", from: caller });
    assert.equal(r.status, 202);
    const started = (await r.json()) as { session: string; name: string; tree: string };
    assert.deepEqual([started.session, started.name, started.tree], ["e-upgrade-1", "upgrade-1", "upgrade-1"]);

    // The tree was asked for through /v1, and NOTHING was cloned: no second workspace, no pod.
    assert.deepEqual(t.seen, [{ method: "POST", path: "/v1/workspaces/svelte-app/trees", body: { name: "upgrade-1" } }]);
    assert.ok(!t.seen.some((x) => x.path.includes("/clone")), JSON.stringify(t.seen));

    // Its hands are the WORKSPACE's — the same pod, the same tool server — confined to its tree.
    const row = t.bench.sessions.get("e-upgrade-1")!;
    assert.equal(row.target, "svelte-app");
    assert.equal(row.workspace, "svelte-app");
    assert.equal(row.tree, "upgrade-1");
    assert.deepEqual(t.bench.treeOf("upgrade-1"), { workspace: "svelte-app", tree: "upgrade-1" });

    // Two at once, each in its own tree of the one workspace.
    await post(t.base, "/agents", { task: "audit the routes", workspace: "svelte-app", name: "audit-2", from: caller });
    assert.deepEqual(t.bench.agentsOf(caller).sort(), ["audit-2", "upgrade-1"]);

    // Closing one gives its tree back through /v1; the other's is untouched.
    assert.deepEqual(await (await fetch(`${t.base}/agents/upgrade-1`, { method: "DELETE" })).json(), { closed: "upgrade-1", workspace: "svelte-app", tree: "upgrade-1" });
    assert.equal(t.bench.treeOf("upgrade-1"), undefined);
    assert.ok(t.seen.some((x) => x.method === "DELETE" && x.path === "/v1/workspaces/svelte-app/trees/upgrade-1"), JSON.stringify(t.seen));
    assert.deepEqual(t.bench.agentsOf(caller), ["audit-2"]);
  } finally {
    await t.down();
  }
});

/**
 * A tree a person closes is gone; one an agent merely FINISHED in stays (spec §4.1). The old clone
 * path deleted it on DONE, which threw away the diff before anybody had read it.
 */
test("a finished agent keeps its tree until the person closes it", async () => {
  const t = await up("bench-tree-life-");
  try {
    const caller = t.bench.sessions.all().find((s) => !s.archived)!.id;
    // The fake echoes its prompt, so the report's first word is the prompt's.
    await post(t.base, "/agents", { task: "DONE — pushed branch fix-login", workspace: "api", name: "done-1", from: caller });
    await until(() => t.bench.exchanges.bySession(caller).some((e) => e.dir === "in"), 5_000, "its report");
    assert.deepEqual(t.bench.treeOf("done-1"), { workspace: "api", tree: "done-1" });
    assert.ok(!t.seen.some((x) => x.method === "DELETE"), "nothing was given back on its own");
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

test("a caller and its agents talk directly: no queue, no triage", async () => {
  const t = await up("bench-direct-");
  try {
    const caller = t.bench.sessions.all().find((s) => !s.archived)!.id;
    // The caller is mid-turn; two agents report while it works.
    await t.bench.rpc(caller, { type: "prompt", message: "hang" });
    await until(() => t.bench.busy(), 2_000, "mid-turn");
    await post(t.base, "/agents", { task: "first", workspace: "api", name: "a-1", from: caller });
    await post(t.base, "/agents", { task: "second", workspace: "api", name: "a-2", from: caller });
    await until(() => t.bench.exchanges.bySession(caller).filter((e) => e.dir === "in").length === 2, 5_000, "both reports");

    // Delivered at once, in arrival order — as steers, not held behind an ordering fork.
    const said = ((await t.bench.rpc(caller, { type: "clear_queue" })).data as { steering: string[]; followUp: string[] });
    assert.equal(said.followUp.length, 0, "nothing was queued");
    assert.equal(said.steering.filter((m) => m.startsWith("[from agent")).length, 2, JSON.stringify(said));
    assert.match(said.steering[0], /first/);
    assert.match(said.steering[1], /second/);
  } finally {
    await t.down();
  }
});

/**
 * An ask addressed by NAME must reach the workspace by ID. The owner's bench sent
 * `ask {to:"svelte-frontend", kind:"info"}` and the fork came up with `KL_TOOLS_WORKSPACE` set to
 * the name: every tool answered `workspace svelte-frontend: not found`, and because the session
 * file is keyed by id the fork had no transcript to read either (2026-09-17).
 */
test("an ask by name resolves to the workspace's id before anything is spawned", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-wsname-"));
  const bench = new Bench({
    dir,
    readOnly: false,
    model: "fake/m",
    bin: FAKE,
    listWorkspaces: async () => [{ id: "ws-30b60ec83f5ff77f", name: "svelte-frontend" }, { id: "ws-other", name: "api" }],
  });
  const srv = await serve(bench, 0);
  await bench.start();
  const base = `http://127.0.0.1:${srv.port}`;
  try {
    const caller = bench.sessions.all().find((s) => !s.archived)!.id;
    assert.deepEqual(await bench.resolveWorkspace("svelte-frontend"), { id: "ws-30b60ec83f5ff77f", name: "svelte-frontend" });
    assert.deepEqual(await bench.resolveWorkspace("ws-30b60ec83f5ff77f"), { id: "ws-30b60ec83f5ff77f", name: "svelte-frontend" });
    await assert.rejects(() => bench.resolveWorkspace("nope"), /no workspace nope/);

    // A work ask: the session it opens is keyed by the ID, and so is the exchange that routes it.
    const work = await post(base, "/workspaces/svelte-frontend/ask", { text: "run the tests", from: caller });
    assert.equal(work.status, 202);
    assert.equal(((await work.json()) as { session: string }).session, "w-ws-30b60ec83f5ff77f");
    assert.equal(bench.exchanges.bySession(caller)[0].workspace, "ws-30b60ec83f5ff77f");
    // And the hands that session runs with are that workspace's, by id — this is the env the child
    // gets as `KL_TOOLS_WORKSPACE`, and the name is what used to land here.
    assert.equal(bench.sessions.get("w-ws-30b60ec83f5ff77f")?.target, "ws-30b60ec83f5ff77f");

    // An info ask: the fork's tools are bound to the ID, and it forks that thread's own file.
    const argvFile = path.join(dir, "argv.json");
    process.env.FAKE_PI_ARGV_FILE = argvFile;
    try {
      const info = await post(base, "/workspaces/svelte-frontend/ask", { text: "which routes have no auth?", kind: "info", from: caller });
      assert.equal(info.status, 202);
    } finally {
      delete process.env.FAKE_PI_ARGV_FILE;
    }
    await until(() => fs.existsSync(argvFile), 5_000, "the fork started");
    const argv = JSON.parse(fs.readFileSync(argvFile, "utf8")) as string[];
    const forked = argv[argv.indexOf("--fork") + 1];
    assert.equal(forked, path.join(dir, "workspaces", "ws-30b60ec83f5ff77f", "thread.jsonl"), "the id's own thread, which is where the history is");
    assert.ok(fs.existsSync(forked), "and it exists, so the fork actually has a transcript");

    // What a person reads is still the NAME.
    await until(() => bench.exchanges.bySession(caller).some((e) => e.id.startsWith("info-") && e.dir === "in"), 5_000, "the answer");
    const back = (await bench.messages(caller)).messages as { content: string }[];
    assert.ok(back.some((m) => String(m.content).startsWith("[info from svelte-frontend] ")), JSON.stringify(back));
  } finally {
    await srv.close();
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
