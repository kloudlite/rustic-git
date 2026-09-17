import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { TOOLS } from "../../pi/catalog.ts";
import kloudlite, { ALWAYS_ON, BENCH_ALWAYS_ON, identity, sanitizeError, settle, thrown, BENCH_HANDS } from "../../pi/kloudlite.ts";
import workspaceTools from "../../pi/workspace-tools.ts";
import http from "node:http";
import { Bench } from "../src/bench.ts";
import { IDE_TOOLS } from "../src/rpc-child.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";

type Registered = { name: string; description: string; parameters: any };
/** Just enough of pi's extension API to see what an extension registers and hooks. */
function fakePi() {
  const tools: Registered[] = [];
  const commands: Record<string, { handler: (a: string, ctx: any) => Promise<void> }> = {};
  const hooks: Record<string, ((ev: any) => Promise<any>)[]> = {};
  let active: string[] = [];
  const pi = {
    registerTool: (t: Registered) => tools.push(t),
    on: (name: string, fn: (ev: any) => Promise<any>) => ((hooks[name] ??= []).push(fn), undefined),
    getAllTools: () => tools,
    getActiveTools: () => active,
    setActiveTools: (names: string[]) => void (active = names),
    registerCommand: (name: string, def: { handler: (a: string, ctx: any) => Promise<void> }) => void (commands[name] = def),
  } as any;
  // The extension applies its active set on session_start, never at load: `start()` is that event.
  const start = async () => { for (const fn of hooks["session_start"] ?? []) await fn({}); };
  return { pi, tools, hooks, commands, start, active: () => active };
}
const withEnv = (vars: Record<string, string | undefined>) => {
  const saved = Object.fromEntries(Object.keys(vars).map((k) => [k, process.env[k]]));
  Object.assign(process.env, vars);
  for (const [k, v] of Object.entries(vars)) if (v === undefined) delete process.env[k];
  return () => {
    for (const [k, v] of Object.entries(saved)) if (v === undefined) delete process.env[k]; else process.env[k] = v;
  };
};

test("a bench session registers exactly the catalogue; a workspace session only its own packages", async () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined });
  try {
    const { pi, tools, active: activeNow, start } = fakePi();
    kloudlite(pi);
    await start();
    const registered = tools.map((t) => t.name).sort();
    // `kloudlite.ts` registers the catalogue except the entries that run ON this machine —
    // those are `workspace-tools.ts`'s, because they go to a tool server rather than to /v1.
    // `kl_pkg_*` and `kl_env_switch`/`_clear` are a WORKSPACE's own machine's; a bench session has
    // no machine, so it does not register them either (spec §3.1).
    assert.deepEqual(registered, TOOLS.map((t) => t.name).filter((n) => !IDE_TOOLS.includes(n)).sort());
    // Registered is not active: a session starts with what it needs and searches for the rest.
    assert.deepEqual(activeNow().slice().sort(), BENCH_ALWAYS_ON.slice().sort());
    assert.equal(new Set(registered).size, registered.length, "no tool is registered twice");
    // The shell is gone: nothing a bench session can call runs in the bench pod.
    for (const gone of ["bash", "read", "write", "edit", "grep", "find", "ls", "process"]) assert.ok(!registered.includes(gone), gone);
    // Another workspace is asked, never driven — and never re-packaged by id.
    assert.ok(registered.includes("ask"));
    assert.ok(!registered.includes("kl_workspace_packages"));
  } finally {
    restore();
  }

  // A workspace session: its own machine's packages and its space's environment, nothing else —
  // and exactly what `--tools` admits, or it would register tools it cannot call.
  const back = withEnv({ KL_TOOLS_WORKSPACE: "api", KL_TEAM: "acme", KL_WORKSPACE_ID: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const registered = tools.map((t) => t.name).sort();
    // A workspace session registers the same catalogue minus what only the bench does with it.
    for (const yes of ["kl_pkg_add", "kl_env_switch", "kl_intercept", "kl_environment_service_add", "ask", "plan", "tool_search"]) assert.ok(registered.includes(yes), yes);
    for (const no of ["kl_workspace_delete", "kl_environment_delete", "kl_workspace_create"]) assert.ok(!registered.includes(no), no);
  } finally {
    back();
  }
});

test("the system prompt is the harness's own, and says only what the model must know", async () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined });
  try {
    const { pi, hooks } = fakePi();
    kloudlite(pi);
    const handler = hooks["before_agent_start"]?.[0];
    assert.ok(handler, "no before_agent_start hook");
    const out = await handler({ prompt: "add nats to the env", systemPrompt: "You are pi, a coding agent. Your cwd is /opt/harness." });
    const prompt = out.systemPrompt as string;
    // A REPLACEMENT, not an append: whatever the agent CLI would have said is gone.
    assert.equal(prompt, identity(BENCH_HANDS));
    assert.doesNotMatch(prompt, /\bpi\b/i);
    assert.doesNotMatch(prompt, /\/opt\/harness/);
    assert.match(prompt, /^You are the Kloudlite harness, the person's bench\./);

    // What it must know: the five things, in the person's words.
    for (const rule of [
      /Before acting in one of these areas, load its skill with `skill \{name\}` once per session, then tool_search the verb\./,
      /Every platform tool is one `tool_search` away/,
      // Spec §3.1: there is no bash here to reach for, and the prompt says where work happens.
      /You have no files and no shell here\. Anything that reads, writes or runs happens in a WORKSPACE/,
      /You have no filesystem or shell where you run\./,
      /You have no working directory\. Name a workspace\./,
      /Another workspace is asked, not touched: `ask \{to: "<workspace>", task\}`/,
      /Something new \(a backend, a service, a project\) gets a new workspace/,
      /When the person corrects you, states a preference, or tells you a fact about their setup you will need again, save a memory\./,
      /never save a conclusion about the harness's own behaviour — report that instead\./,
      /Do what is asked, directly\. No checks first\./,
      /Only the tools reach the platform\. Never change anything the person did not ask for\./,
      /Answer in one line, then only the facts needed — eight lines at most, no code blocks and no tables\./,
    ]) assert.match(prompt, rule);

    // Mechanism is NOT prompt text: it happens whether the model knows about it or not.
    const before = prompt.slice(0, prompt.indexOf("Speak in the caveman style"));
    for (const word of ["proposal", "render", "exchange", "widget", "quota", "region"]) assert.ok(!new RegExp(word, "i").test(before), `${word} is mechanism, not prompt: ${before}`);
    // The style still rides along.
    assert.match(prompt, /Speak in the caveman style below\. Chat text only/);
    assert.match(prompt, /Respond terse like smart caveman\./);
  } finally {
    restore();
  }
});

test("an ask opens the workspace's own session, queues there, and the answer comes back to the asker", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-ask-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  const srv = await serve(bench, 0);
  try {
    await bench.start();
    const asker = bench.sessions.all().find((s) => !s.archived)!.id;
    const post = (b: unknown) => fetch(`http://127.0.0.1:${srv.port}/workspaces/api/ask`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(b) });

    // `from` has to be a session of this bench: a tool call is trusted no further than the id it carries.
    assert.equal((await post({ text: "run the tests", from: "s-999" })).status, 400);

    const r = await post({ text: "run the tests", from: asker });
    assert.equal(r.status, 202);
    assert.equal((await r.json()).session, "w-api", "the workspace's own session does the work");
    assert.ok(bench.sessions.get("w-api"), "created if the workspace had none");

    // The exchange settles, and the answer is delivered into the asking session as a message.
    await until(() => bench.exchanges.bySession(asker).some((e) => e.dir === "in"), 5_000, "the answer");
    const rows = bench.exchanges.bySession(asker);
    assert.deepEqual(rows.map((e) => [e.workspace, e.dir, e.state]), [["api", "out", "done"], ["api", "in", "done"]]);
    assert.match(rows[1].text, /^echo \[ask .+\] run the tests$/);
    const back = (await bench.messages(asker)).messages as { role: string; content: string }[];
    assert.ok(back.some((m) => m.role === "user" && String(m.content).startsWith("[from workspace api] echo ")), JSON.stringify(back));
    // The workspace session is told which ask it is answering and who asked.
    const asked = (await bench.messages("w-api")).messages as { role: string; content: string }[];
    assert.match(String(asked[0].content), /^\[ask ask-\d+-\w+ from .+\] run the tests$/);
  } finally {
    await srv.close();
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a btw fork registers nothing and is still told what it is", async () => {
  const restore = withEnv({ KL_FORK: "1", KL_TOOLS_WORKSPACE: undefined, KL_WORKSPACE_ID: undefined });
  try {
    const { pi, tools, hooks } = fakePi();
    kloudlite(pi);
    assert.deepEqual(tools, []);
    const prompt = (await hooks["before_agent_start"][0]({})).systemPrompt as string;
    assert.doesNotMatch(prompt, /\bpi\b/i);
    assert.match(prompt, /Kloudlite harness/);
    assert.match(prompt, /one question/);
    // It has no tools, so it is not told what the tools do — that list would be a list of lies.
    assert.ok(!prompt.includes("kl_workspace_ask"), prompt);
  } finally {
    restore();
  }
});

test("adding a service keeps every other service exactly as it was", async () => {
  // The whole point: PATCH takes the whole list, and mongodb's mounts/env/command are not in the
  // add tool's schema — they survive only by being passed through untouched.
  const mongodb = { name: "mongodb", image: "mongo:7", command: ["mongod", "--bind_ip_all"], env: { MONGO_INITDB_ROOT_USERNAME: "root" }, mounts: [{ folder: "mongodb", path: "/data/db" }], ports: [27017] };
  let patched: any;
  let live: any[] = [mongodb];
  const srv = http.createServer((req, res) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      // The same server stands in for the bench too: a write is proposed first, and this person
      // says yes. Without an answer the tool would decline itself, which is the point of §9.
      if (req.url!.startsWith("/proposals")) {
        res.writeHead(200, { "content-type": "application/json" });
        return void res.end(JSON.stringify({ answer: "yes" }));
      }
      // A real api: the next GET sees what the last PATCH wrote.
      if (req.method === "PATCH") live = (patched = JSON.parse(b)).services;
      res.writeHead(200, { "content-type": "application/json" });
      // The GET is both the read before the patch and the wait after it: every service ready, so
      // the add settles at once rather than polling out its cap.
      res.end(JSON.stringify(req.method === "GET" ? { id: "devstack", state: "running", services: live, service_status: live.map((x: any) => ({ name: x.name, ready: true })) } : { ok: true }));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-svc-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const base = `http://127.0.0.1:${(srv.address() as { port: number }).port}`;
  const restore = withEnv({ KL_TOOL_TOKEN_FILE: path.join(dir, "token"), KL_API_URL: base, KL_BENCH_URL: base, KL_FORK: undefined, KL_TOOLS_WORKSPACE: undefined, KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme" });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const tool = (n: string) => tools.find((t) => t.name === n)! as unknown as { execute: (...a: any[]) => Promise<any> };
    const added = await tool("kl_environment_service_add").execute("c1", { id: "devstack", service: { name: "nats", image: "nats:2", ports: [4222] } }, undefined, undefined, undefined);
    assert.notEqual(added.content[0].text, "declined by the person", JSON.stringify(added));
    assert.deepEqual(patched.services[0], mongodb, "mongodb passed through verbatim");
    // The api has no serde default for these three: an omitted one is a 422, not an empty list.
    assert.deepEqual(patched.services[1], { name: "nats", image: "nats:2", command: [], env: {}, mounts: [], ports: [4222] });

    await tool("kl_environment_service_rm").execute("c2", { id: "devstack", name: "nats" }, undefined, undefined, undefined);
    assert.deepEqual(patched.services, [mongodb]);
    const miss = await tool("kl_environment_service_rm").execute("c3", { id: "devstack", name: "redis" }, undefined, undefined, undefined);
    assert.equal(miss.isError, true);
  } finally {
    restore();
    srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("asks queue per workspace: never refused, and a [reply id] answers the ask it names", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-queue-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  try {
    await bench.start();
    const one = bench.sessions.all().find((s) => !s.archived)!.id;
    const two = (await bench.create()).id;

    // The first ask leaves the workspace session mid-turn ("hang" never ends its turn).
    const a1 = await bench.ask("api", "hang", one);
    await until(() => bench.exchanges.bySession(one)[0].state === "running", 5_000, "the head running");
    // The second is taken anyway — an ask is never refused because that workspace is busy — and
    // waits in pi's own queue behind the turn already running.
    const a2 = await bench.ask("api", "hang too", two);
    assert.notEqual(a1.exchange, a2.exchange);
    assert.deepEqual(bench.exchanges.bySession(two).map((e) => [e.dir, e.state]), [["out", "queued"]]);

    // A turn that names the FIRST ask answers that one, whichever order they arrived in.
    await bench.rpc("w-api", { type: "prompt", message: `[reply ${a1.exchange}] the first one is done` });
    await until(() => bench.exchanges.bySession(one).some((e) => e.dir === "in"), 5_000, "session one's answer");
    const back = bench.exchanges.bySession(one);
    assert.equal(back.find((e) => e.id === a1.exchange)!.state, "done", "the ask it named settled");
    assert.match(back.find((e) => e.dir === "in")!.text, /the first one is done/);
    assert.deepEqual(bench.exchanges.bySession(two).map((e) => [e.dir, e.state]), [["out", "queued"]], "the other is still waiting");
    const said = (await bench.messages(one)).messages as { role: string; content: string }[];
    assert.ok(said.some((m) => String(m.content).startsWith("[from workspace api]")), JSON.stringify(said));
  } finally {
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a child that dies fails every ask it was still holding", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-queue-x-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  try {
    await bench.start();
    const asker = bench.sessions.all().find((s) => !s.archived)!.id;
    // "hang" never answers, so the ask stays outstanding; "crash" takes the child with it.
    const held = await bench.ask("api", "hang", asker);
    await bench.rpc("w-api", { type: "prompt", message: "crash" }).catch(() => undefined);
    await until(() => bench.exchanges.bySession(asker).find((e) => e.id === held.exchange)?.state === "failed", 5_000, "the held ask fails");
  } finally {
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a person's own turn in the workspace tab answers nobody, and an ask is answered by id even when it is not the head", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-turn-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  try {
    await bench.start();
    const one = bench.sessions.all().find((s) => !s.archived)!.id;
    const two = (await bench.create()).id;
    // Two asks left outstanding: "hang" ends no turn, so neither is answered by its own arrival.
    const a1 = await bench.ask("api", "hang", one);
    const a2 = await bench.ask("api", "hang", two);
    // The head is `running` once its turn has actually begun; the second waits behind it.
    await until(() => bench.exchanges.bySession(one)[0].state === "running", 5_000, "the head running");

    // The person types in that workspace's own tab. It ends a turn like any other.
    await bench.rpc("w-api", { type: "prompt", message: "what is in this repo" });
    const answered = async (what: string) => {
      const m = (await bench.messages("w-api")).messages as { role: string; content: unknown }[];
      return m.some((x) => x.role === "assistant" && JSON.stringify(x.content).includes(`echo ${what}`));
    };
    await until(() => answered("what is in this repo"), 5_000, "the person's turn to end");
    assert.deepEqual([...bench.exchanges.bySession(one), ...bench.exchanges.bySession(two)].map((e) => [e.dir, e.state]), [["out", "running"], ["out", "queued"]], "nobody's ask was touched");

    // Now a turn that carries the SECOND ask's tag: it answers that one, not the head.
    await bench.rpc("w-api", { type: "prompt", message: `[ask ${a2.exchange} from session 2] done now` });
    await until(() => bench.exchanges.bySession(two).some((e) => e.dir === "in"), 5_000, "the second ask's answer");
    assert.equal(bench.exchanges.bySession(two).find((e) => e.id === a2.exchange)!.state, "done");
    assert.deepEqual(bench.exchanges.bySession(one).map((e) => [e.dir, e.state]), [["out", "running"]], "the head is still waiting");
    assert.notEqual(a1.exchange, a2.exchange);
  } finally {
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("/proc-stop kills on the session's own tool server and ends the row", async () => {
  const killed: unknown[] = [];
  const srv = http.createServer((req, res) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      killed.push({ url: req.url, body: JSON.parse(b || "{}") });
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ state: "exited" }));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const at = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-proc-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE, resolveTools: async () => at });
  try {
    await bench.start();
    const ws = await bench.openWorkspace("api");
    bench.procs.snapshot(ws.id, [{ id: "p1", name: "vite", command: "npm run dev", started: 1 }]);
    // What the desktop sends: the harness answers it itself now, and no model ever sees it.
    await bench.rpc(ws.id, { type: "prompt", message: "/proc-stop p1" });
    assert.deepEqual(killed, [{ url: "/tools/process_kill", body: { id: "p1" } }]);
    assert.notEqual(bench.procs.all().find((p) => p.id === "p1")!.ended, undefined);
  } finally {
    await bench.stop();
    srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("kl_capabilities answers this session's own catalogue, and says what is not there", async () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const out = (await (tools.find((t) => t.name === "kl_capabilities") as any).execute("c1", {}, undefined, undefined, undefined)).content[0].text as string;
    // A bench session has no files and no shell to offer (spec §3.1).
    assert.match(out, /^you have no files and no shell of your own: name a workspace/);
    for (const g of ["workspace:", "environment:", "platform:"]) assert.ok(out.includes(g), g);
    assert.match(out, /ask \[write\]/);
    assert.match(out, /anything not listed is not something you can do — say so\.$/);
  } finally {
    restore();
  }
  // A workspace session lists its own, narrower set — answering with the bench's would be a lie.
  const back = withEnv({ KL_TOOLS_WORKSPACE: "api", KL_TEAM: "acme", KL_WORKSPACE_ID: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const out = (await (tools.find((t) => t.name === "kl_capabilities") as any).execute("c1", {}, undefined, undefined, undefined)).content[0].text as string;
    assert.ok(!out.includes("kl_workspace_ask"), out);
    assert.match(out, /kl_pkg_add \[write\]/);
  } finally {
    back();
  }
});

test("kl_workspace_progress reads the bench's own routes and says what that workspace is up to", async () => {
  const seen: string[] = [];
  const srv = http.createServer((req, res) => {
    seen.push(req.url!);
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(
      req.url!.startsWith("/exchanges")
        ? [{ dir: "out", state: "running", text: "[ask ask-1-x from session 1] add a health endpoint" }, { dir: "in", state: "done", text: "done" }]
        : { total: 2, messages: [{ role: "user", content: "[ask ask-1-x from session 1] add a health endpoint" }, { role: "assistant", content: [{ type: "toolCall", name: "edit" }, { type: "text", text: "added /healthz" }] }] },
    ));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const restore = withEnv({ KL_BENCH_URL: `http://127.0.0.1:${(srv.address() as { port: number }).port}`, KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const out = (await (tools.find((t) => t.name === "kl_workspace_progress") as any).execute("c1", { id: "api" }, undefined, undefined, undefined)).content[0].text as string;
    assert.deepEqual(seen.sort(), ["/exchanges?workspace=api", "/procs", "/workspaces/api/messages?limit=10"]);
    assert.equal(
      out,
      [
        "asked of api:",
        "  running: add a health endpoint",
        // What is RUNNING there is part of the answer: the bench could not see a build it had asked
        // for and started building again (owner, 2026-09-18).
        "running there:",
        "  nothing running",
        "its session, latest last:",
        "  asked: [ask ask-1-x from session 1] add a health endpoint",
        "  ran edit",
        "  said: added /healthz",
      ].join("\n"),
    );
  } finally {
    restore();
    srv.close();
  }
});

test("a process that exits on its own is noticed within a poll", async () => {
  let live: unknown[] = [{ id: "p1", cmd: "npm run dev", started_at: new Date().toISOString(), state: "running", exit_code: null }];
  const srv = http.createServer((req, res) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(req.url === "/tools/process_list" ? { processes: live } : { state: "exited" }));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const at = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-poll-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE, resolveTools: async () => at });
  const seen: unknown[] = [];
  bench.onEvent((ev) => ev.type === "procs" && seen.push(ev.rows));
  try {
    await bench.start();
    const ws = await bench.openWorkspace("api");
    // What the extension publishes after a background command: the bench starts watching from here.
    (bench as any).foldRow(ws.id, { type: "extension_ui_request", method: "setWidget", widgetKey: "harness:procs", widgetLines: [JSON.stringify([{ id: "p1", name: "npm run dev", command: "npm run dev", started: Date.now() }])] });
    assert.equal(bench.procs.all().find((p) => p.id === "p1")!.ended, undefined);

    // A pi exit says NOTHING about it: the process runs on the tool server, not inside pi. The
    // owner watched a live dev server marked "lost 13m" because a restart used to mark it so.
    (bench as any).foldRow(ws.id, { type: "exit", code: 0 });
    assert.equal(bench.procs.all().find((p) => p.id === "p1")!.ended, undefined, "a pi exit is not a process exit");

    // It dies between tool calls: nothing would ever say so without the poll.
    live = [{ id: "p1", cmd: "npm run dev", started_at: new Date().toISOString(), state: "exited", exit_code: 1 }];
    await (bench as any).sweepProcs();
    const row = bench.procs.all().find((p) => p.id === "p1")!;
    assert.notEqual(row.ended, undefined, "ended");
    assert.equal(row.code, 1, "with the code the tool server gave");
    assert.equal(row.lost, undefined, "it said how it ended, so it is not lost");
    assert.ok(seen.length, "and the desktop is told");

    // Gone from the tool server's list without ever reporting an exit: THAT is lost.
    (bench as any).foldRow(ws.id, { type: "extension_ui_request", method: "setWidget", widgetKey: "harness:procs", widgetLines: [JSON.stringify([{ id: "p2", name: "vite", command: "npm run dev", started: Date.now() }])] });
    live = [];
    await (bench as any).sweepProcs();
    assert.equal(bench.procs.all().find((p) => p.id === "p2")!.lost, true);
  } finally {
    await bench.stop();
    srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("settle waits for the platform instead of the model sleeping", async () => {
  const done = (d: any) => d?.state === "running";
  const slept: number[] = [];
  let clock = 0;
  const sleep = async (ms: number) => void (slept.push(ms), (clock += ms));
  const now = () => clock;

  // Ready first ask: nothing is waited at all.
  let asks = 0;
  let r = await settle(async () => (asks++, { status: 200, data: { state: "running" } }), done, 10_000, undefined, 2_000, now, sleep);
  assert.deepEqual([asks, slept.length, r.settled], [1, 0, true]);

  // Ready on the third: two waits, and the final document is what comes back.
  asks = 0;
  slept.length = 0;
  clock = 0;
  r = await settle(async () => ({ status: 200, data: { state: ++asks < 3 ? "creating" : "running", id: "api" } }), done, 10_000, undefined, 2_000, now, sleep);
  assert.deepEqual([asks, slept, r.settled], [3, [2_000, 2_000], true]);
  assert.deepEqual(r.data, { state: "running", id: "api" });

  // The cap: it stops, says how long it waited, and still hands back what it saw — not an error.
  asks = 0;
  slept.length = 0;
  clock = 0;
  r = await settle(async () => (asks++, { status: 200, data: { state: "creating" } }), done, 5_000, undefined, 2_000, now, sleep);
  assert.equal(r.settled, false);
  assert.equal(r.waitedMs, 4_000);
  assert.deepEqual(r.data, { state: "creating" });

  // A refusal is an answer: asking again cannot change it.
  asks = 0;
  r = await settle(async () => (asks++, { status: 404, data: "no such workspace" }), done, 10_000, undefined, 2_000, now, sleep);
  assert.deepEqual([asks, r.settled], [1, false]);

  // An aborted tool call stops at the next look, never mid-sleep forever.
  const ac = new AbortController();
  ac.abort();
  asks = 0;
  r = await settle(async () => (asks++, { status: 200, data: { state: "creating" } }), done, 60_000, ac.signal, 2_000, now, sleep);
  assert.deepEqual([asks, r.settled], [1, false]);
});

/** A fake /v1 that records every call, for the tools that now fill in what a person would not type. */
function fakeApi(routes: (m: string, url: string, body: any) => unknown, missing = 200) {
  const seen: { m: string; url: string; body: any }[] = [];
  const srv = http.createServer((req, res) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      const body = b ? JSON.parse(b) : undefined;
      seen.push({ m: req.method!, url: req.url!, body });
      if (req.url!.startsWith("/proposals")) {
        res.writeHead(200, { "content-type": "application/json" });
        return void res.end(JSON.stringify({ answer: "yes" }));
      }
      // A route that answers nothing is whatever `missing` says it is: 200 {} by default, or the
      // status a test wants for a thing that is not there.
      const answered = routes(req.method!, req.url!, body);
      res.writeHead(answered === undefined ? missing : 200, { "content-type": "application/json" });
      res.end(JSON.stringify(answered ?? (missing === 200 ? {} : { error: "not found" })));
    });
  });
  return { srv, seen, listen: async () => (await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r)), `http://127.0.0.1:${(srv.address() as { port: number }).port}`) };
}

test("a create fills in the region and the owner, and takes a name where an id is expected", async () => {
  const api = fakeApi((m, url) => {
    if (url === "/v1/workspaces/bench-ada") return { id: "bench-ada", state: "running", region: "centralindia-k3s" };
    if (url === "/v1/workspaces" && m === "GET") return [{ id: "ws-abc123", name: "svelte-frontend", state: "running" }, { id: "ws-def456", name: "api", state: "running" }, { id: "ws-ghi789", name: "api", state: "stopped" }];
    if (url === "/v1/workspaces" && m === "POST") return { id: "ws-new", state: "running" };
    if (url.startsWith("/v1/workspaces/ws-")) return { id: "ws-abc123", state: "running" };
    return {};
  });
  const base = await api.listen();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-user-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restore = withEnv({ KL_TOOL_TOKEN_FILE: path.join(dir, "token"), KL_API_URL: base, KL_BENCH_URL: base, KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_OWNER: "ada", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const tool = (n: string) => tools.find((t) => t.name === n)! as unknown as { execute: (...a: any[]) => Promise<any> };
    assert.equal(tool("kl_workspace_create").parameters?.properties?.region, undefined, "a person does not type a region");
    assert.equal(tool("kl_workspace_create").parameters?.properties?.owner, undefined, "nor an owner");

    await tool("kl_workspace_create").execute("c1", { name: "svelte-backend", packages: ["go"] }, undefined, undefined, undefined);
    const created = api.seen.find((x) => x.m === "POST" && x.url === "/v1/workspaces")!;
    // Region from this machine's own document; owner from the space it is in.
    assert.equal(created.body.region, "centralindia-k3s");
    assert.equal(created.body.team, "acme");
    assert.equal(created.body.name, "svelte-backend");

    // A name is as good as an id, and the id is what goes on the wire.
    api.seen.length = 0;
    await tool("kl_workspace_stop").execute("c2", { id: "svelte-frontend" }, undefined, undefined, undefined);
    assert.ok(api.seen.some((x) => x.url === "/v1/workspaces/ws-abc123/stop"), JSON.stringify(api.seen));
    // An id still works, and costs the same one listing.
    api.seen.length = 0;
    await tool("kl_workspace_stop").execute("c3", { id: "ws-def456" }, undefined, undefined, undefined);
    assert.ok(api.seen.some((x) => x.url === "/v1/workspaces/ws-def456/stop"));
    // Two of a name is refused, never guessed.
    const clash = await tool("kl_workspace_stop").execute("c4", { id: "api" }, undefined, undefined, undefined);
    assert.equal(clash.isError, true);
    assert.match(clash.content[0].text, /2 workspaces are called api; name it by id \(ws-def456, ws-ghi789\)/);
  } finally {
    restore();
    api.srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a snapshot is a snapshot: create from one, list them, and never the word volume", async () => {
  const api = fakeApi((m, url) => {
    if (url === "/v1/workspaces/bench-ada") return { id: "bench-ada", state: "running", region: "r1" };
    if (url === "/v1/workspaces" && m === "GET") return [{ id: "ws-1", name: "api", state: "running" }];
    if (url === "/v1/volumes") return [{ name: "ws-1", volume: "vol-9", kind: "workspace" }];
    if (url === "/v1/volumes/vol-9/history") return [{ id: "snap-2", message: "before the refactor", createdAt: "2026-09-17T10:00:00Z", phase: "Ready" }];
    if (url === "/v1/workspaces/restore") return { id: "ws-new", state: "running" };
    return { id: "ws-new", state: "running" };
  });
  const base = await api.listen();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-snap-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restore = withEnv({ KL_TOOL_TOKEN_FILE: path.join(dir, "token"), KL_API_URL: base, KL_BENCH_URL: base, KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_OWNER: "ada", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const tool = (n: string) => tools.find((t) => t.name === n)! as unknown as { execute: (...a: any[]) => Promise<any> };

    // from_snapshot is the same verb, not a second one.
    await tool("kl_workspace_create").execute("c1", { name: "api-copy", from_snapshot: "snap-2" }, undefined, undefined, undefined);
    const r = api.seen.find((x) => x.m === "POST" && x.url === "/v1/workspaces/restore")!;
    assert.deepEqual([r.body.name, r.body.snapshot_id], ["api-copy", "snap-2"]);
    assert.ok(!api.seen.some((x) => x.m === "POST" && x.url === "/v1/workspaces"), "an empty workspace was not created as well");

    const listed = await tool("kl_workspace_snapshots").execute("c2", { id: "api" }, undefined, undefined, undefined);
    const out = listed.content[0].text as string;
    assert.match(out, /snap-2/);
    assert.match(out, /before the refactor/);
    // The volume is the platform's business; a person snapshots their workspace.
    assert.ok(!/volume/i.test(out), out);
  } finally {
    restore();
    api.srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a bench session starts with what it can use, the rest one search away, and the six skills read", async () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools, active, start } = fakePi();
    kloudlite(pi);
    await start();
    const tool = (n: string) => tools.find((t) => t.name === n)! as unknown as { execute: (...a: any[]) => Promise<any> };
    const run = (n: string, a: any) => tool(n).execute("c1", a, undefined, undefined, undefined);

    // 43 tools in front of a model is a menu it reads instead of working — and a bench session's
    // set has no local hands in it at all (spec §3.1).
    assert.deepEqual(active().slice().sort(), BENCH_ALWAYS_ON.slice().sort());
    assert.ok(tools.some((t) => t.name === "kl_intercept"), "still registered, just not active");

    // A search finds it, says what it takes, and turns it on for the rest of the session.
    const found = await run("tool_search", { query: "intercept" });
    // Every match, best-named first is not promised — what matters is that the tool is in there
    // with what it takes, and that it is on afterwards.
    assert.match(found.content[0].text, /kl_intercept — .*\[write\]; params: id, service, workspace, ports/);
    assert.ok(active().includes("kl_intercept"), active().join(","));
    // What the model should say when there is no tool, in the words it should use.
    // A miss says so AND says not to come back with the same words: "list workspaces" was searched
    // six times across six sessions (transcripts, 2026-09-18).
    assert.equal((await run("tool_search", { query: "reboot the datacentre" })).content[0].text, "no tool for that here; say so to the person, and do not search again for the same thing");
    // A hit says the names are armed for the session, which is the other half of the same lesson.
    assert.match((await run("tool_search", { query: "intercept" })).content[0].text, /these are on for the rest of this session; call them, do not search for them again$/);

    // The skills are product words, not tool lists, and each one loads from beside the extension.
    for (const name of ["workspaces", "environments", "snapshots", "repos", "images", "agents"]) {
      const r = await run("skill", { name });
      // A skill says WHEN to load it, in its own frontmatter — a list of names is a list a model skips.
      assert.match(r.content[0].text, new RegExp(`^---\nname: ${name}\ndescription: Use \\w`));
      assert.match(r.content[0].text, new RegExp(`# ${name[0].toUpperCase()}`, "i"));
      // And the identity carries that same sentence, so it is read before anything is decided.
      assert.ok(identity(BENCH_HANDS).includes(`- ${name} — ${/^description:\s*(.*)$/m.exec(r.content[0].text)![1]}`), name);
    }
    // No name lists them, with what each is for.
    const listed = (await run("skill", {})).content[0].text as string;
    assert.equal(listed.split("\n").length, 6);
    assert.match(listed, /^workspaces — Use when the person wants a new machine/m);
    assert.equal((await run("skill", { name: "nope" })).isError, true);
  } finally {
    restore();
  }
});

test("ask routes to a workspace's own session or to a fresh agent", async () => {
  const seen: { url: string; body: any }[] = [];
  const srv = http.createServer((req, res) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      seen.push({ url: req.url!, body: b ? JSON.parse(b) : undefined });
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ ok: true }));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const restore = withEnv({ KL_BENCH_URL: `http://127.0.0.1:${(srv.address() as { port: number }).port}`, KL_SESSION: "s-1", KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const ask = tools.find((t) => t.name === "ask")! as unknown as { execute: (...a: any[]) => Promise<any> };

    // A workspace REMEMBERS: its own session, which has done everything it has done before.
    const teammate = await ask.execute("c1", { to: "svelte-frontend", task: "run the tests" }, undefined, undefined, undefined);
    assert.equal(seen[0].url, "/workspaces/svelte-frontend/ask");
    assert.deepEqual(seen[0].body, { text: "run the tests", kind: "work", from: "s-1" });
    assert.match(teammate.content[0].text, /queued in svelte-frontend's session/);

    // A QUESTION is not work: it says so on the wire, and says what it means in the answer.
    const info = await ask.execute("c1b", { to: "svelte-frontend", task: "which routes have no auth?", kind: "info" }, undefined, undefined, undefined);
    assert.equal(seen[1].body.kind, "info");
    assert.match(info.content[0].text, /answers from a read-only copy without stopping/);

    // An agent starts clean, is named, and works on this machine unless told otherwise.
    // `shared` keeps it in the caller's own workspace; the default (its own clone) needs /v1 and
    // is covered where a fake api exists.
    const agent = await ask.execute("c2", { to: "agent", task: "audit the routes", name: "audit", shared: true }, undefined, undefined, undefined);
    assert.equal(seen[2].url, "/agents");
    assert.match(seen[2].body.name, /^audit-[a-z0-9]{6}$/);
    assert.deepEqual([seen[2].body.task, seen[2].body.workspace, seen[2].body.from], ["audit the routes", "bench-ada", "s-1"]);
    assert.match(agent.content[0].text, /^agent audit-[a-z0-9]{6} started$/);
  } finally {
    restore();
    srv.close();
  }
});

test("an agent cannot start agents", async () => {
  const restore = withEnv({ KL_EPHEMERAL: "1", KL_TOOLS_WORKSPACE: "api", KL_TEAM: "acme", KL_WORKSPACE_ID: undefined, KL_FORK: undefined });
  try {
    const { pi, tools, active, start } = fakePi();
    kloudlite(pi);
    await start();
    // Its child would have nobody to report to and no tab to be seen in.
    assert.ok(!tools.some((t) => t.name === "ask"), tools.map((t) => t.name).join(","));
    assert.ok(!active().includes("ask"), active().join(","));
    assert.ok(active().includes("tool_search"), "it can still find a tool it needs");
  } finally {
    restore();
  }
});

test("plan mode turns the writes off in pi itself, and build turns them back on", async () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, commands, active } = fakePi();
    // Both extensions, as a real session loads them: the machine's own hands come from
    // workspace-tools.ts, and plan mode has to turn those off too.
    const back = withEnv({ KL_TOOLS_ADDRESS: "127.0.0.1:7788" });
    workspaceTools(pi);
    back();
    kloudlite(pi);
    const said: string[] = [];
    const ctx = { ui: { notify: (m: string) => said.push(m) } };

    await commands.mode.handler("plan", ctx);
    // A plan cannot quietly become a change: every tool that writes is off.
    for (const gone of ["write", "edit", "bash", "ask", "kl_workspace_create", "kl_environment_service_add"]) assert.ok(!active().includes(gone), `${gone}: ${active().join(",")}`);
    for (const kept of ["read", "grep", "plan", "tool_search"]) assert.ok(active().includes(kept), kept);
    assert.match(said[0], /plan mode/);

    await commands.mode.handler("build", ctx);
    assert.ok(active().includes("bash") && active().includes("ask"), active().join(","));
  } finally {
    restore();
  }
});

test("loading the extension calls no action method; the active set is applied on session_start", async () => {
  // The fleet's own failure (2026-09-17): `setActiveTools` at load threw "Extension runtime not
  // initialized" and every bench session exited 1. Registering is describing; activating is doing.
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    let started = false;
    let active: string[] = [];
    const hooks: Record<string, ((ev: any) => Promise<any>)[]> = {};
    const tools: { name: string }[] = [];
    const boom = (what: string) => {
      if (!started) throw new Error(`Extension runtime not initialized. Action methods cannot be called during extension loading (${what})`);
    };
    const pi = {
      registerTool: (t: { name: string }) => tools.push(t),
      registerCommand: () => undefined,
      on: (name: string, fn: (ev: any) => Promise<any>) => ((hooks[name] ??= []).push(fn), undefined),
      getAllTools: () => tools,
      getActiveTools: () => (boom("getActiveTools"), active),
      setActiveTools: (names: string[]) => (boom("setActiveTools"), void (active = names)),
      sendMessage: () => boom("sendMessage"),
      ui: { notify: () => boom("ui.notify"), setWidget: () => boom("ui.setWidget") },
    } as any;

    // Loading must not throw, and must not have decided anything yet.
    kloudlite(pi);
    workspaceTools(pi);
    assert.ok(tools.length > 10, "tools are registered at load, which is allowed");
    assert.deepEqual(active, [], "nothing activated during load");
    assert.ok(hooks["session_start"]?.length, "it waits for the session");

    started = true;
    for (const fn of hooks["session_start"]) await fn({});
    assert.deepEqual(active.slice().sort(), BENCH_ALWAYS_ON.slice().sort());
  } finally {
    restore();
  }
});

test("a background command that ends tells its session, and a watch sends the lines that match", async () => {
  let live: any[] = [{ id: "p1", cmd: "npm run dev", started_at: new Date().toISOString(), state: "running", exit_code: null }];
  let out: Record<string, unknown> = { stdout: "", stderr: "", next: 0 };
  /** What each `process_output` read asked for, so a test can say the cursors advance. */
  const asked: { since: number; since_err: number }[] = [];
  const srv = http.createServer((req, res) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      if (req.url === "/tools/process_output") {
        const body = JSON.parse(b || "{}") as { since?: number; since_err?: number };
        asked.push({ since: body.since ?? 0, since_err: body.since_err ?? 0 });
      }
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(req.url === "/tools/process_list" ? { processes: live } : req.url === "/tools/process_output" ? out : { state: "exited" }));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const at = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-notify-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE, resolveTools: async () => at });
  try {
    await bench.start();
    const ws = await bench.openWorkspace("api");
    (bench as any).foldRow(ws.id, { type: "extension_ui_request", method: "setWidget", widgetKey: "harness:procs", widgetLines: [JSON.stringify([{ id: "p1", name: "svelte dev server", command: "npm run dev", started: Date.now() }])] });

    // A WATCH: only the matching lines, and only once each.
    bench.watchProc(ws.id, "p1", "error");
    out = { stdout: "listening on 3000\nerror: cannot find module\nready", stderr: "", next: 64, next_err: 0 };
    await (bench as any).sweepWatches();
    let said = (await bench.messages(ws.id)).messages as { role: string; content: string }[];
    const watched = said.filter((m) => String(m.content).startsWith("[watch"));
    assert.equal(watched.length, 1, JSON.stringify(said));
    assert.match(watched[0].content, /\[watch svelte dev server \/error\/\]\nerror: cannot find module/);
    assert.ok(!watched[0].content.includes("listening on 3000"), "only what matched");

    // Each stream has its own cursor (81621d02). A watch keeps both, so stderr is never re-read —
    // a build's progress is all on stderr, and it used to come back whole on every fire, sending the
    // same `#N DONE` lines again and again (owner, 2026-09-18).
    out = { stdout: "", stderr: "error: cannot find module\n#5 DONE", next: 64, next_err: 40 };
    await (bench as any).sweepWatches();
    assert.deepEqual(asked.at(-1), { since: 64, since_err: 0 }, "the first read of stderr starts at 0");
    await (bench as any).sweepWatches();
    assert.deepEqual(asked.at(-1), { since: 64, since_err: 40 }, "and the next carries what the server answered");
    said = (await bench.messages(ws.id)).messages as { role: string; content: string }[];
    assert.equal(said.filter((m) => String(m.content).startsWith("[watch")).length, 1, "nothing is said twice");

    // A line that IS new still gets through.
    out = { stdout: "", stderr: "error: cannot find module\nerror: and another", next: 64, next_err: 80 };
    await (bench as any).sweepWatches();
    said = (await bench.messages(ws.id)).messages as { role: string; content: string }[];
    const all = said.filter((m) => String(m.content).startsWith("[watch"));
    assert.equal(all.length, 2, JSON.stringify(all));
    assert.match(all[1].content, /error: and another/);
    assert.ok(!all[1].content.includes("cannot find module"), "and only the new one");

    // It ends: the session is TOLD, with the tail of what it printed.
    out = { stdout: "error: cannot find module\nexiting", stderr: "", next: 99, next_err: 0 };
    live = [{ id: "p1", cmd: "npm run dev", started_at: new Date().toISOString(), state: "exited", exit_code: 1 }];
    await (bench as any).sweepProcs();
    await until(async () => ((await bench.messages(ws.id)).messages as { content: string }[]).some((m) => String(m.content).startsWith("[task ")), 5_000, "the finish notice");
    said = (await bench.messages(ws.id)).messages as { role: string; content: string }[];
    const done = said.find((m) => String(m.content).startsWith("[task "))!;
    assert.match(done.content, /^\[task svelte dev server finished: exit 1\]/);
    assert.match(done.content, /exiting/);
  } finally {
    await bench.stop();
    srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a process the tool server still runs comes back, whatever the ledger said", async () => {
  // The fleet's own state: two live dev servers marked lost by the old rule, and nothing that
  // could ever correct it — a sweep that only ends rows cannot revive one.
  const live = [{ id: "p1", cmd: "npm run dev", started_at: new Date().toISOString(), state: "running", exit_code: null }];
  const srv = http.createServer((req, res) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(req.url === "/tools/process_list" ? { processes: live } : {}));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const at = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-revive-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE, resolveTools: async () => at });
  try {
    await bench.start();
    const ws = await bench.openWorkspace("api");
    // A ledger that already believes it is gone.
    bench.procs.snapshot(ws.id, [{ id: "p1", name: "svelte dev server", command: "npm run dev", started: 1, ended: 2, lost: true }]);
    assert.equal(bench.procs.all().find((p) => p.id === "p1")!.lost, true);

    await (bench as any).sweepProcs();
    const row = bench.procs.all().find((p) => p.id === "p1")!;
    assert.equal(row.lost, undefined, "it is running; nothing about it is lost");
    assert.equal(row.ended, undefined);

    // And the other direction still holds: gone from the list, without an exit, is lost.
    live.length = 0;
    await (bench as any).sweepProcs();
    assert.equal(bench.procs.all().find((p) => p.id === "p1")!.lost, true);
  } finally {
    await bench.stop();
    srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * A workspace session must be able to CALL what `tool_search` hands it. The owner's backend-rust
 * session searched, got `kl_workspaces` and friends from the catalogue, and pi answered "Tool not
 * found" on every one — they were never registered in that mode (2026-09-17).
 */
test("in a workspace, tool_search offers only what is registered — and it is callable", async () => {
  const restore = withEnv({ KL_TOOLS_WORKSPACE: "ws-1", KL_WORKSPACE_ID: "ws-1", KL_TEAM: "acme", KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools, active, start } = fakePi();
    kloudlite(pi);
    await start();
    const run = (n: string, a: any) => (tools.find((t) => t.name === n)! as unknown as { execute: (...x: any[]) => Promise<any> }).execute("c1", a, undefined, undefined, undefined);

    const found = (await run("tool_search", { query: "workspace" })).content[0].text as string;
    // The last line is the session note, not a tool.
    const names = found.split("\n").filter((l) => l.includes(" — ")).map((l) => l.split(" — ")[0]);
    assert.ok(names.length, found);
    // Everything offered is registered here, and on after the search.
    for (const name of names) {
      assert.ok(tools.some((t) => t.name === name), `${name} was offered but never registered`);
      assert.ok(active().includes(name), `${name} was offered but not turned on`);
    }
    // A workspace may LOOK at the platform.
    assert.ok(tools.some((t) => t.name === "kl_workspaces"), "a workspace can list workspaces");
    assert.ok(tools.some((t) => t.name === "kl_environments"), "and environments");
    // But not change somebody else's machine.
    assert.ok(!tools.some((t) => t.name === "kl_workspace_delete"), "writes on other workspaces stay with the bench");
    assert.ok(!tools.some((t) => t.name === "kl_workspace_create"));
  } finally {
    restore();
  }
});

/**
 * Only handing work to somebody else is an exchange. A platform call is not: the owner's queue
 * showed `kl_workspace_create {"name":"backend-rust","packages":["rust"]}` as though a workspace
 * were working on it (2026-09-17).
 */
test("a platform call publishes no exchange; an ask does", async () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools, start } = fakePi();
    kloudlite(pi);
    await start();
    const widgets: [string, string[]][] = [];
    const run = (n: string, a: any) =>
      (tools.find((t) => t.name === n)! as unknown as { execute: (...x: any[]) => Promise<any> }).execute("c1", a, undefined, undefined, {
        ui: { setWidget: (k: string, v: string[]) => widgets.push([k, v]) },
      });
    await run("kl_workspaces", {}).catch(() => undefined);
    assert.deepEqual(widgets.filter(([k]) => k === "harness:exchange"), [], "a platform call is a tool row, not an exchange");
  } finally {
    restore();
  }
});

test("packages are named as nixpkgs attributes wherever they are asked for", async () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools, start } = fakePi();
    kloudlite(pi);
    await start();
    const param = (tool: string) =>
      String((tools.find((t) => t.name === tool) as unknown as { parameters?: { properties?: Record<string, { description?: string }> } })?.parameters?.properties?.packages?.description ?? "");
    // On the bench, the only packages parameter is the one that CREATES a workspace with them.
    assert.match(param("kl_workspace_create"), /nixpkgs ATTRIBUTE names/);
    assert.match(param("kl_workspace_create"), /rustc cargo \(Rust\)/);
    assert.match(param("kl_workspace_create"), /attr@version/);
    // And the identity says it once, so a model that never opens the skill still knows.
    assert.match(identity(BENCH_HANDS), /Packages are nixpkgs attributes, not language names/);
  } finally {
    restore();
  }
  // A workspace session owns `kl_pkg_*`, and they say the same thing.
  const inWs = withEnv({ KL_TOOLS_WORKSPACE: "ws-1", KL_WORKSPACE_ID: "ws-1", KL_TEAM: "acme", KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools, start } = fakePi();
    kloudlite(pi);
    await start();
    const param = (tool: string) =>
      String((tools.find((t) => t.name === tool) as unknown as { parameters?: { properties?: Record<string, { description?: string }> } })?.parameters?.properties?.packages?.description ?? "");
    for (const tool of ["kl_pkg_add", "kl_pkg_rm"]) assert.match(param(tool), /nixpkgs ATTRIBUTE names/, tool);
  } finally {
    inWs();
  }
});

/**
 * A session has no hands where it runs (spec §3.1). The bench session's ide tools on this
 * container are gone with `ownTools`: there is no tool server in the sessions container, no shell
 * on the model's path, and no package tool bound to "here".
 */
test("a bench session has no filesystem, no shell and no machine of its own", async () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools, active, start } = fakePi();
    kloudlite(pi);
    await start();
    const names = tools.map((t) => t.name);
    // Not merely inactive: the names do not exist, so a model cannot call one and be told "not found".
    for (const hand of ["read", "write", "edit", "bash", "grep", "find", "ls", "process"]) assert.ok(!names.includes(hand), `${hand} is registered on the bench`);
    // Packages ARE reachable — but only by naming a workspace, never "here" (spec §3.1).
    for (const pkg of ["kl_pkg_list", "kl_pkg_add", "kl_pkg_rm"]) {
      const takes = (tools.find((t) => t.name === pkg) as unknown as { parameters?: { properties?: Record<string, unknown> } })?.parameters?.properties ?? {};
      assert.ok("workspace" in takes, `${pkg} does not take a workspace`);
    }
    assert.deepEqual(active().slice().sort(), BENCH_ALWAYS_ON.slice().sort());
    // Searching for a shell finds nothing, in the words the model should use with the person.
    const run = (n: string, a: any) => (tools.find((t) => t.name === n)! as unknown as { execute: (...x: any[]) => Promise<any> }).execute("c1", a, undefined, undefined, undefined);
    assert.equal((await run("tool_search", { query: "shell" })).content[0].text, "no tool for that here; say so to the person, and do not search again for the same thing");
    // And what it says it can do never promises files or a shell.
    const can = (await run("kl_capabilities", {})).content[0].text as string;
    assert.match(can, /you have no files and no shell of your own/);
  } finally {
    restore();
  }
});

test("every identity says where paths are relative to, and the bench that it has no directory", () => {
  const paragraph =
    "You work in one working directory. Every path you give or receive is relative to it. Do not explore, describe or depend on where that directory sits on a machine, what is beside it, or how the machine is laid out; none of that is yours, and tools refuse it. If a task seems to need a path outside your directory, say so in your reply instead.";
  // The bench: no hands, no directory (spec §3.1, §3.5).
  const bench = identity(BENCH_HANDS);
  assert.match(bench, /You have no filesystem or shell where you run\./);
  assert.match(bench, /You have no working directory\. Name a workspace\./);
  assert.match(bench, /A package is installed in a workspace, never "on the bench": name the workspace\./);
  // A workspace session: one directory, and nothing about the machine under it.
  const restore = withEnv({ KL_TOOLS_WORKSPACE: "ws-1", KL_WORKSPACE_ID: "ws-1", KL_TEAM: "acme", KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, hooks } = fakePi();
    workspaceTools(pi);
    const said = hooks["before_agent_start"]?.[0];
    assert.ok(said, "no before_agent_start hook in workspace mode");
    return said({ prompt: "", systemPrompt: "" }).then((out: { systemPrompt: string }) => {
      assert.ok(out.systemPrompt.includes(paragraph), "the workspace identity carries §3.5 verbatim");
      restore();
    });
  } catch (e) {
    restore();
    throw e;
  }
});

/**
 * A package is installed in a WORKSPACE (spec §3.1). A bench session has no machine of its own, so
 * the tool takes the workspace and refuses in the spec's own words when it is not named.
 */
test("packages from the bench name a workspace, or are refused", async () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools, start } = fakePi();
    kloudlite(pi);
    await start();
    const run = (n: string, a: any) => (tools.find((t) => t.name === n)! as unknown as { execute: (...x: any[]) => Promise<any> }).execute("c1", a, undefined, undefined, undefined);
    for (const tool of ["kl_pkg_list", "kl_pkg_add", "kl_pkg_rm"]) {
      const r = await run(tool, { packages: ["ripgrep"] });
      assert.equal(r.isError, true, tool);
      assert.match(r.content[0].text, /^name the workspace: packages are installed in a workspace/, tool);
    }
  } finally {
    restore();
  }
});

/**
 * A tool error says what could not be done, never where anything runs. The model was handed
 * "502: not an api answer for /v1/repos — the route is not published on https://dev.kloudlite.io"
 * and repeated all of it to the person (owner, 2026-09-18): a host, a route and a status code they
 * can do nothing with, about a machine the session is not supposed to know exists (spec §3.5).
 */
test("a failed platform call says what failed, not where", () => {
  const leaky = "not an api answer for /v1/repos — the route is not published on https://dev.kloudlite.io (got a web page)";
  const said = sanitizeError("kl_repos", 502, leaky);
  for (const leak of ["http", "https", "/v1", "dev.kloudlite.io", "502"]) assert.ok(!said.includes(leak), `${leak} leaked: ${said}`);
  assert.match(said, /repositories could not be reached just now/);

  // Each class of refusal reads as the person's own business.
  assert.match(sanitizeError("kl_workspace_start", 404, "no workspace api"), /does not exist/);
  assert.match(sanitizeError("kl_workspace_create", 403, "forbidden"), /not allowed/);
  assert.match(sanitizeError("kl_environment_stop", 409, "still running"), /current state/);
  // A 422's own sentence survives, minus anything naming the platform's shape.
  const refused = sanitizeError("kl_workspace_create", 422, '{"error":"rust: unknown attribute at https://search.devbox.sh/v1/x"}');
  assert.match(refused, /unknown attribute/);
  for (const leak of ["http", "/v1", "search.devbox.sh"]) assert.ok(!refused.includes(leak), `${leak} leaked: ${refused}`);

  // Signing in IS the person's business, and says nothing about a machine.
  assert.equal(sanitizeError("kl_repos", 401, "anything"), "sign in on the Kloudlite desktop app");

  // A thrown error is cleaned the same way.
  const threw = thrown("kl_repo_create", new Error("connect ECONNREFUSED 10.42.3.190:7788"));
  for (const leak of ["10.42.3.190", "7788"]) assert.ok(!threw.includes(leak), `${leak} leaked: ${threw}`);
});

test("no identity tells a model to talk about where it runs", () => {
  const rule = "Never mention hosts, URLs, routes, ports, status codes, commands you ran or where you run — not even when reporting a failure. Say what you could not do for the person and what you need from them.";
  assert.ok(identity(BENCH_HANDS).includes(rule), "the bench identity");
  const restore = withEnv({ KL_TOOLS_WORKSPACE: "ws-1", KL_WORKSPACE_ID: "ws-1", KL_TEAM: "acme", KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, hooks } = fakePi();
    workspaceTools(pi);
    return hooks["before_agent_start"][0]({ prompt: "", systemPrompt: "" }).then((out: { systemPrompt: string }) => {
      assert.ok(out.systemPrompt.includes(rule), "the workspace identity");
      restore();
    });
  } catch (e) {
    restore();
    throw e;
  }
});

/**
 * A tool `tool_search` found stays found for the rest of the SESSION, the way a deferred tool does
 * in Claude Code — and a session outlives the process serving it. The owner's bench restarted
 * mid-session and the next turn answered `tool kl_workspace_create not found` for a tool the model
 * had already searched for and called (2026-09-18). The names live on the bench, and a starting
 * session arms itself from them.
 */
test("a tool found by tool_search survives the turn, and the restart", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-found-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  const srv = await serve(bench, 0);
  await bench.start();
  const id = bench.sessions.all().find((s) => !s.archived)!.id;
  const restore = withEnv({
    KL_WORKSPACE_ID: "bench-ada",
    KL_TEAM: "acme",
    KL_TOOLS_WORKSPACE: undefined,
    KL_FORK: undefined,
    KL_EPHEMERAL: undefined,
    KL_SESSION: id,
    KL_BENCH_URL: `http://127.0.0.1:${srv.port}`,
  });
  try {
    const first = fakePi();
    kloudlite(first.pi);
    await first.start();
    assert.ok(!first.active().includes("kl_workspace_create"), "it starts with the twelve");

    const search = first.tools.find((t) => t.name === "tool_search")! as unknown as { execute: (...a: any[]) => Promise<any> };
    await search.execute("c1", { query: "create workspace" }, undefined, undefined, undefined);
    assert.ok(first.active().includes("kl_workspace_create"), first.active().join(","));
    await until(() => bench.sessions.found(id).includes("kl_workspace_create"), 5_000, "the bench to have recorded it");

    // The bench restarts: a NEW child, loading the extension from scratch, for the same session.
    const next = fakePi();
    kloudlite(next.pi);
    await next.start();
    assert.ok(next.active().includes("kl_workspace_create"), `a found tool is armed again: ${next.active().join(",")}`);
    // And what it starts with is still there beside it.
    for (const n of BENCH_ALWAYS_ON) assert.ok(next.active().includes(n), n);
  } finally {
    restore();
    await srv.close();
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * A create ANSWERS when the workspace is ready. `ready` is `/v1`'s own word for a workspace whose
 * pod is up (`crd::Phase::as_str`), and it was not in the set of states this app treats as settled —
 * so `kl_workspace_create backend` polled for 129 s over a workspace `/v1` had called ready with a
 * running pod at about 100 s (owner, 2026-09-18). A state we do not know is a state we wait out.
 */
test("a create answers as soon as the workspace is ready, not once it happens to say running", async () => {
  let reads = 0;
  const api = fakeApi((m, url) => {
    if (url === "/v1/workspaces/bench-ada") return { id: "bench-ada", state: "running", region: "r1" };
    if (url === "/v1/workspaces" && m === "POST") return { id: "ws-632cf9f2", state: "creating" };
    if (url === "/v1/workspaces/ws-632cf9f2") return { id: "ws-632cf9f2", state: ++reads === 1 ? "creating" : "ready", access: "ready" };
    return {};
  });
  const base = await api.listen();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-ready-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restore = withEnv({ KL_TOOL_TOKEN_FILE: path.join(dir, "token"), KL_API_URL: base, KL_BENCH_URL: base, KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_OWNER: "ada", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const create = tools.find((t) => t.name === "kl_workspace_create")! as unknown as { execute: (...a: any[]) => Promise<any> };
    const r = await create.execute("c1", { name: "backend", packages: ["go"] }, undefined, undefined, undefined);
    assert.ok(!r.isError, JSON.stringify(r));
    // The final document, not "still creating after 129s": the tool settled on `ready`.
    assert.match(r.content[0].text, /ready/);
    assert.doesNotMatch(r.content[0].text, /still /);
    assert.equal(reads, 2, "it stopped asking the moment the workspace was ready");
  } finally {
    restore();
    api.srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * What nineteen sessions of transcripts caught the model doing, fixed where the owner asked the
 * weight to sit: in a schema, a description or one identity line (2026-09-18).
 */
test("a tool says what it needs: the attribute rule, the key, the question rule", () => {
  const create = TOOLS.find((t) => t.name === "kl_workspace_create")!;
  // Two creates passed language names — `kl_pkg_add` says the rule and `kl_workspace_create` did not.
  assert.match(create.summary, /nixpkgs attributes \(rustc, cargo, nodejs_22, go, python3\), never language names/);
  const question = TOOLS.find((t) => t.name === "question")!;
  // Three questions were asked with `architecture` and `memory` sitting unread and always on.
  assert.match(question.summary, /Read `architecture` and `memory` first; if either answers it, do not ask\./);

  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const params = (n: string) => (tools.find((t) => t.name === n)!.parameters as any).properties;
    // One key for a workspace: 20 calls used `workspace` where the schema wanted `id`.
    assert.ok(params("kl_workspace_progress").id, "kl_workspace_progress takes id");
    assert.equal(params("kl_workspace_progress").workspace, undefined, "and not a second name for it");
    for (const n of ["kl_workspace", "kl_workspace_start", "kl_workspace_stop", "kl_workspace_delete"]) assert.ok(params(n).id, n);
    // An environment is named the way every other environment tool names it.
    assert.match(params("kl_env_switch").environment.description, /environment id or name/);
    // A step a person can read, and `later` as the object it is.
    assert.equal(params("plan").set.items.properties.text.minLength, 3);
    assert.match(params("plan").later.description, /as \{text, why\} — not a sentence/);
  } finally {
    restore();
  }
});

test("the identity carries the tree-relative rule and the length ceiling", () => {
  const id = identity(BENCH_HANDS);
  // §3.5's paragraph was in the workspace identity and missing from the bench's, and 53 rows named
  // a host, a port or a container path to the person.
  assert.match(id, /Every path you give or receive is relative to a working directory\./);
  assert.match(id, /Never repeat a path a tool printed that starts with a slash\./);
  // 29 bench replies ran over eight lines, five with fenced code or tables.
  assert.match(id, /eight lines at most, no code blocks and no tables/);
  // Plan-first is its own instruction, not a clause: every one of seven sessions had to be nudged.
  assert.match(id, /More than one step\? The plan tool is the FIRST call, before any other\./);
});

test("a plan step needs its words, and a confirmation is refused before the tool is armed", async () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools, start } = fakePi();
    kloudlite(pi);
    await start();
    const run = (n: string, a: any) => (tools.find((t) => t.name === n)! as unknown as { execute: (...x: any[]) => Promise<any> }).execute("c1", a, undefined, undefined, { ui: {} });
    const blank = await run("plan", { doing: "" });
    assert.equal(blank.isError, true);
    assert.match(blank.content[0].text, /doing needs the step's text, the same words the plan has/);

    // `kl_workspace_delete` is registered and NOT armed — which is exactly when a confirmation was
    // slipping through, five of ten questions in the transcripts.
    const confirm = await run("question", { header: "Confirm delete", question: "Delete workspace new-workspace?", options: [{ label: "yes", description: "" }, { label: "no", description: "" }] });
    assert.equal(confirm.isError, true);
    assert.match(confirm.content[0].text, /call the tool, the harness will ask the person for you/);
  } finally {
    restore();
  }
});

/**
 * §4.1's own case, from `ws-408ff2ea30c161a9/thread.jsonl`: `kl_workspaces` was searched, called,
 * and answered `Tool kl_workspaces not found` four times. Fixed in 8242f488; held here so the
 * transcript's own sequence cannot regress.
 */
test("the transcripts' own case: search kl_workspaces, call it, and it is still there next turn", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-armed-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  const srv = await serve(bench, 0);
  await bench.start();
  const id = bench.sessions.all().find((s) => !s.archived)!.id;
  const restore = withEnv({ KL_TOOLS_WORKSPACE: "api", KL_TOOLS_ADDRESS: undefined, KL_WORKSPACE_ID: "ws-408ff2ea30c161a9", KL_TEAM: "acme", KL_FORK: undefined, KL_EPHEMERAL: undefined, KL_SESSION: id, KL_BENCH_URL: `http://127.0.0.1:${srv.port}` });
  try {
    const first = fakePi();
    kloudlite(first.pi);
    await first.start();
    const search = first.tools.find((t) => t.name === "tool_search")! as unknown as { execute: (...a: any[]) => Promise<any> };
    const r = await search.execute("c1", { query: "list workspaces" }, undefined, undefined, undefined);
    assert.match(r.content[0].text as string, /kl_workspaces/);
    assert.ok(first.active().includes("kl_workspaces"), first.active().join(","));
    await until(() => bench.sessions.found(id).includes("kl_workspaces"), 5_000, "the bench to have recorded it");

    const next = fakePi();
    kloudlite(next.pi);
    await next.start();
    assert.ok(next.active().includes("kl_workspaces"), `still armed for the session: ${next.active().join(",")}`);
  } finally {
    restore();
    await srv.close();
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * `kl_env_switch {"environment":"devstack"}` answered 404 and had to be retried by id, while every
 * other environment tool took either (transcripts, 2026-09-18). It resolves a name now, and refuses
 * an ambiguous one rather than picking.
 */
test("switching environments takes the name a person uses", async () => {
  const api = fakeApi((m, url) => {
    if (url === "/v1/environments" && m === "GET") return [{ id: "env-1", name: "devstack" }, { id: "env-2", name: "twin" }, { id: "env-3", name: "twin" }];
    return { ok: true };
  });
  const base = await api.listen();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-env-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restore = withEnv({ KL_TOOL_TOKEN_FILE: path.join(dir, "token"), KL_API_URL: base, KL_BENCH_URL: base, KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_OWNER: "ada", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const run = (a: any) => (tools.find((t) => t.name === "kl_env_switch")! as unknown as { execute: (...x: any[]) => Promise<any> }).execute("c1", a, undefined, undefined, undefined);
    await run({ environment: "devstack" });
    assert.equal(api.seen.find((x) => x.m === "PUT")!.body.environment, "env-1", "the name became the id on the wire");
    api.seen.length = 0;
    await run({ environment: "env-2" });
    assert.equal(api.seen.find((x) => x.m === "PUT")!.body.environment, "env-2", "an id still works");
    // Two of a name is refused, never guessed — the same rule every other tool follows.
    const clash = await run({ environment: "twin" });
    assert.equal(clash.isError, true);
    assert.match(clash.content[0].text, /2 environments are called twin/);
  } finally {
    restore();
    api.srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * `skill {name:"workspaces"}` was answered `no skill workspaces; there are workspaces, …` ten times
 * — a refusal that names the thing it is refusing (transcripts, 2026-09-18). A skill is asked for
 * the way a person says it, and a name that IS installed can never come back as "no skill".
 */
test("a skill is found however it is named, and an unreadable one says so as itself", async () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const run = (a: any) => (tools.find((t) => t.name === "skill")! as unknown as { execute: (...x: any[]) => Promise<any> }).execute("c1", a, undefined, undefined, undefined);
    for (const said of ["workspaces", "Workspaces", " workspaces ", "workspaces.md"]) {
      const r = await run({ name: said });
      assert.ok(!r.isError, `${said}: ${r.content[0].text}`);
      assert.match(r.content[0].text as string, /^---\nname: workspaces\n/);
    }
    // A name nothing installs is still refused, with what there is.
    const no = await run({ name: "kubernetes" });
    assert.equal(no.isError, true);
    assert.match(no.content[0].text, /no skill kubernetes; there are workspaces/);
    // And no refusal ever lists the name it just refused.
    const listed = /there are ([^\n]*)/.exec(no.content[0].text)?.[1] ?? "";
    assert.ok(!listed.split(", ").includes("kubernetes"), listed);
  } finally {
    restore();
  }
});

/**
 * A workspace deleted under a session that stayed open answered `404: not found` to `kl_pkg_list`,
 * four times in one session (transcripts, 2026-09-18). A machine that is gone says so, once.
 */
test("a machine that no longer exists says so rather than answering 404", async () => {
  const api = fakeApi((_m, url) => (url.startsWith("/v1/workspaces/") ? undefined : { ok: true }), 404);
  const base = await api.listen();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-gone-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restore = withEnv({ KL_TOOL_TOKEN_FILE: path.join(dir, "token"), KL_API_URL: base, KL_BENCH_URL: base, KL_TOOLS_ADDRESS: "127.0.0.1:7788", KL_TOOLS_WORKSPACE: "api", KL_WORKSPACE_ID: "api", KL_TEAM: "acme", KL_OWNER: "ada", KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools } = fakePi();
    workspaceTools(pi);
    kloudlite(pi);
    const r = await (tools.find((t) => t.name === "kl_pkg_list")! as unknown as { execute: (...x: any[]) => Promise<any> }).execute("c1", {}, undefined, undefined, undefined);
    assert.equal(r.isError, true);
    assert.match(r.content[0].text, /this machine no longer exists/);
    assert.match(r.content[0].text, /asking again will not change that/);
    assert.ok(!/404/.test(r.content[0].text), r.content[0].text);
  } finally {
    restore();
    api.srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * A wait that ran out printed `still ready after 182s` — a contradiction, and the state it named
 * was one it should have settled on (transcripts, 2026-09-18). `ready` settles now (99ddf73d); a
 * wait that ends on any state we call settled never says "still".
 */
test("a lifecycle verb never reports a settled state as still going", async () => {
  const api = fakeApi((m, url) => {
    if (url === "/v1/workspaces/bench-ada") return { id: "bench-ada", state: "running", region: "r1" };
    if (url === "/v1/workspaces" && m === "POST") return { id: "ws-slow", state: "creating" };
    // Every read says a state the tool treats as settled: it must answer, not wait it out.
    if (url === "/v1/workspaces/ws-slow") return { id: "ws-slow", state: "ready" };
    return {};
  });
  const base = await api.listen();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-still-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restore = withEnv({ KL_TOOL_TOKEN_FILE: path.join(dir, "token"), KL_API_URL: base, KL_BENCH_URL: base, KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_OWNER: "ada", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const r = await (tools.find((t) => t.name === "kl_workspace_create")! as unknown as { execute: (...x: any[]) => Promise<any> }).execute("c1", { name: "slow" }, undefined, undefined, undefined);
    assert.doesNotMatch(r.content[0].text, /still/);
    assert.match(r.content[0].text, /ready/);
  } finally {
    restore();
    api.srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * `kl_workspace_progress` reported "nothing outstanding / nothing yet" for a workspace that had
 * just been asked for work and had already written files (transcripts, 2026-09-18): the bench keys
 * a thread and its exchanges by the workspace ID, and the tool was handed the name.
 */
test("progress asked by name reads the workspace's own thread", async () => {
  const seen: string[] = [];
  const srv = http.createServer((req, res) => {
    seen.push(req.url!);
    res.writeHead(200, { "content-type": "application/json" });
    if (req.url!.startsWith("/v1/workspaces")) return void res.end(JSON.stringify([{ id: "ws-632cf9f23d9f", name: "backend" }]));
    if (req.url!.startsWith("/exchanges")) return void res.end(JSON.stringify([{ dir: "out", state: "running", text: "write the service" }]));
    if (req.url === "/procs")
      return void res.end(JSON.stringify([
        { id: "p1", session: "w-1", workspace: "ws-632cf9f23d9f", name: "build", command: "kl container build -t backend:0.1 .", started: 1789600000000 },
        { id: "p2", session: "w-2", workspace: "ws-other", name: "dev", command: "npm run dev", started: 1789600000000 },
      ]));
    res.end(JSON.stringify({ total: 1, messages: [{ role: "assistant", content: [{ type: "toolCall", name: "write" }] }] }));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const base = `http://127.0.0.1:${(srv.address() as { port: number }).port}`;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-prog-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restore = withEnv({ KL_TOOL_TOKEN_FILE: path.join(dir, "token"), KL_API_URL: base, KL_BENCH_URL: base, KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const out = (await (tools.find((t) => t.name === "kl_workspace_progress") as any).execute("c1", { id: "backend" }, undefined, undefined, undefined)).content[0].text as string;
    assert.ok(seen.includes("/exchanges?workspace=ws-632cf9f23d9f"), seen.join(" "));
    assert.ok(seen.includes("/workspaces/ws-632cf9f23d9f/messages?limit=10"), seen.join(" "));
    assert.match(out, /running: write the service/);
    assert.doesNotMatch(out, /nothing yet/);
    assert.match(out, /kl container build -t backend:0\.1 \./, "what is running there, so it is not started twice");
  } finally {
    restore();
    srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * The tool declared `ports: [{from, to}]` while `/v1` takes `crd::PortMap` — `{service, workspace}`
 * — so every remap was a 422 naming a field the model could not see, and it guessed three shapes in
 * a row (transcripts, 2026-09-18). The schema is `/v1`'s, and the description says which is which.
 */
test("an intercept's port remap is named the way /v1 names it", async () => {
  const api = fakeApi(() => ({ ok: true }));
  const base = await api.listen();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-ports-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restore = withEnv({ KL_TOOL_TOKEN_FILE: path.join(dir, "token"), KL_API_URL: base, KL_BENCH_URL: base, KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_OWNER: "ada", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const t = tools.find((x) => x.name === "kl_intercept")!;
    const port = (t.parameters as any).properties.ports.items.properties;
    assert.ok(port.service && port.workspace, JSON.stringify(Object.keys(port)));
    assert.equal(port.from, undefined, "`from`/`to` is not what /v1 takes");
    assert.match((t.parameters as any).properties.ports.description, /\{service, workspace\}/);
    assert.match(TOOLS.find((x) => x.name === "kl_intercept")!.summary, /\{service, workspace\}/);
    // And what goes on the wire is exactly what was given.
    await (t as any).execute("c1", { id: "env-1", service: "api", workspace: "ws-1", ports: [{ service: 8080, workspace: 3000 }] }, undefined, undefined, undefined);
    assert.deepEqual(api.seen.find((x) => x.m === "POST")!.body.ports, [{ service: 8080, workspace: 3000 }]);
  } finally {
    restore();
    api.srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * The bench proposed `Start workspace bench-505f6b8c9d7b` — ITSELF — while its ask to a real
 * workspace was still running (owner, 2026-09-18). A bench is not a workspace: it is never a
 * target, it is never listed, and no tool defaults to the session's own machine any more.
 */
test("the bench is never a workspace target, and never in a listing", async () => {
  const api = fakeApi((m, url) => {
    if (url === "/v1/workspaces" && m === "GET")
      return [
        { id: "ws-632cf9f23d9f2fbf", name: "backend", state: "running" },
        // A row that leaked from anywhere — an older api, a cached answer — must not be nameable.
        { id: "bench-505f6b8c9d7b", name: "bench", state: "running", bench: {} },
      ];
    return { ok: true };
  });
  const base = await api.listen();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-self-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restore = withEnv({ KL_TOOL_TOKEN_FILE: path.join(dir, "token"), KL_API_URL: base, KL_BENCH_URL: base, KL_WORKSPACE_ID: "bench-505f6b8c9d7b", KL_TEAM: "acme", KL_OWNER: "ada", KL_TOOLS_WORKSPACE: undefined, KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const run = (n: string, a: any) => (tools.find((t) => t.name === n)! as unknown as { execute: (...x: any[]) => Promise<any> }).execute("c1", a, undefined, undefined, undefined);

    // Its own id, by either name it goes by.
    for (const self of ["bench-505f6b8c9d7b", "bench-0123456789ab"]) {
      for (const verb of ["kl_workspace_start", "kl_workspace_stop", "kl_workspace_delete", "kl_workspace"]) {
        const r = await run(verb, { id: self });
        assert.equal(r.isError, true, `${verb} ${self}`);
        assert.match(r.content[0].text, /that is you, not a workspace; name a workspace/, `${verb} ${self}`);
      }
      assert.match((await run("kl_pkg_list", { workspace: self })).content[0].text, /that is you, not a workspace/);
      assert.match((await run("kl_workspace_progress", { id: self })).content[0].text, /that is you, not a workspace/);
    }
    // A real workspace still works.
    api.seen.length = 0;
    const ok = await run("kl_workspace", { id: "ws-632cf9f23d9f2fbf" });
    assert.ok(!ok.isError, JSON.stringify(ok));

    // And no listing ever offers a bench to name in the first place.
    const listed = (await run("kl_workspaces", {})).content[0].text;
    const rows = typeof listed === "string" ? listed : JSON.stringify(listed);
    assert.match(rows, /ws-632cf9f23d9f2fbf/);
    assert.ok(!rows.includes("bench-505f6b8c9d7b"), rows);
  } finally {
    restore();
    api.srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("waiting on an ask is waiting, not restarting a machine", () => {
  // The model reached for `start` with an ask in flight; the identity now says what to do instead.
  assert.match(identity(BENCH_HANDS), /An ask you are already waiting on is waited on: read it with kl_workspace_progress, or ask again\. Never start, stop or restart a machine to move work along/);
});

/**
 * The bench was handed its own pod's loopback tool server as "its machine" — the rule that died
 * with spec §3.1 — so a build was attempted against a container that has no tool server, and the
 * model told the person to start `bench-…` to fix it, quoting the address (owner, 2026-09-18).
 */
test("a bench session is handed no machine, and a workspace that will not answer says so plainly", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-hands-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  try {
    await bench.start();
    const id = bench.sessions.all().find((s) => !s.archived)!.id;
    const hands = bench.tools(id) as { toolsAddress?: string; builtinTools: boolean; tools: string[] };
    assert.equal(hands.toolsAddress, undefined, "no tool server anywhere near the bench pod");
    assert.equal(hands.builtinTools, false);
    for (const gone of ["read", "write", "edit", "bash", "process", "kl_repo_clone"]) assert.ok(!hands.tools.includes(gone), gone);
    // `kl_container_build` is still THERE for a bench session — as an ask to the workspace holding
    // the context, not an exec anywhere near the bench (owner, 2026-09-18).
    assert.ok(hands.tools.includes("kl_container_build"), hands.tools.join(","));
  } finally {
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a workspace whose tools are unreachable never says where it tried", async () => {
  const restore = withEnv({ KL_TOOLS_ADDRESS: "127.0.0.1:1", KL_TOOLS_WORKSPACE: "api", KL_WORKSPACE_ID: "api", KL_TEAM: "acme", KL_FORK: undefined, KL_EPHEMERAL: undefined });
  try {
    const { pi, tools } = fakePi();
    workspaceTools(pi);
    const read = tools.find((t) => t.name === "read")! as unknown as { execute: (...x: any[]) => Promise<any> };
    const r = await read.execute("c1", { paths: ["go.mod"] }, undefined, undefined, undefined);
    const said = r.content[0].text as string;
    assert.match(said, /the workspace's tools did not answer; is it running\?/);
    for (const leak of ["127.0.0.1", ":1", "fetch failed", "ECONNREFUSED"]) assert.ok(!said.includes(leak), `${leak} leaked: ${said}`);
  } finally {
    restore();
  }
});
