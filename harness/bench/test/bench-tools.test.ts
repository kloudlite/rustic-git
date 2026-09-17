import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { TOOLS } from "../../pi/catalog.ts";
import kloudlite, { identity, BENCH_HANDS } from "../../pi/kloudlite.ts";
import http from "node:http";
import { Bench } from "../src/bench.ts";
import { WORKSPACE_TOOLS } from "../src/rpc-child.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";

type Registered = { name: string; description: string; parameters: any };
/** Just enough of pi's extension API to see what an extension registers and hooks. */
function fakePi() {
  const tools: Registered[] = [];
  const hooks: Record<string, ((ev: any) => Promise<any>)[]> = {};
  const pi = {
    registerTool: (t: Registered) => tools.push(t),
    on: (name: string, fn: (ev: any) => Promise<any>) => ((hooks[name] ??= []).push(fn), undefined),
  } as any;
  return { pi, tools, hooks };
}
const withEnv = (vars: Record<string, string | undefined>) => {
  const saved = Object.fromEntries(Object.keys(vars).map((k) => [k, process.env[k]]));
  Object.assign(process.env, vars);
  for (const [k, v] of Object.entries(vars)) if (v === undefined) delete process.env[k];
  return () => {
    for (const [k, v] of Object.entries(saved)) if (v === undefined) delete process.env[k]; else process.env[k] = v;
  };
};

test("a bench session registers exactly the catalogue; a workspace session only its own packages", () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const registered = tools.map((t) => t.name).sort();
    assert.deepEqual(registered, TOOLS.map((t) => t.name).sort());
    assert.equal(new Set(registered).size, registered.length, "no tool is registered twice");
    // The shell is gone: nothing a bench session can call runs in the bench pod.
    for (const gone of ["bash", "read", "write", "edit", "grep", "find", "ls", "process"]) assert.ok(!registered.includes(gone), gone);
    // Another workspace is asked, never driven — and never re-packaged by id.
    assert.ok(registered.includes("kl_workspace_ask"));
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
    assert.deepEqual(registered, WORKSPACE_TOOLS.split(",").filter((t) => t.startsWith("kl_")).sort());
    for (const no of ["kl_workspace_ask", "kl_environment_delete", "kl_workspace_delete", "kl_quota"]) assert.ok(!registered.includes(no), no);
  } finally {
    back();
  }
});

test("the system prompt is the harness's own, not the agent CLI's", async () => {
  const restore = withEnv({ KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme", KL_TOOLS_WORKSPACE: undefined });
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
    assert.match(prompt, /Kloudlite harness/);
    assert.match(prompt, /kl_workspace_ask/);
    assert.match(prompt, /kl_pkg_add/);
    // The rule the fleet needed: a model asked what it could do ran a destroy to find out.
    assert.match(prompt, /Never call a tool whose effect is write or destroy unless the person asked/);
    assert.match(prompt, /answer from kl_capabilities and describe the tools by name; do not run them to find out/);
    assert.match(prompt, /Never go behind the tools for it/);
    // A new component gets its own workspace: one was installed into a running svelte frontend.
    assert.match(prompt, /gets its OWN workspace \(kl_workspace_create, then kl_workspace_ask\)/);
    assert.match(prompt, /use kl_workspace_progress/);
    // Short answers, because the owner reads the id and the error, not the plan.
    assert.match(prompt, /Answer short\. Lead with the result in one line\./);
    assert.match(identity("its own hands here"), /Answer short/);
    // Every mode is told it: a workspace session's writes land on somebody's real machine too.
    assert.match(identity("its own hands here"), /Never call a tool whose effect is write or destroy/);
    assert.match(identity("its own hands here"), /Never go behind the tools/);
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
      // A real api: the next GET sees what the last PATCH wrote.
      if (req.method === "PATCH") live = (patched = JSON.parse(b)).services;
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(req.method === "GET" ? { id: "devstack", services: live } : { ok: true }));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-svc-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restore = withEnv({ KL_TOOL_TOKEN_FILE: path.join(dir, "token"), KL_API_URL: `http://127.0.0.1:${(srv.address() as { port: number }).port}`, KL_FORK: undefined, KL_TOOLS_WORKSPACE: undefined, KL_WORKSPACE_ID: "bench-ada", KL_TEAM: "acme" });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const tool = (n: string) => tools.find((t) => t.name === n)! as unknown as { execute: (...a: any[]) => Promise<any> };
    await tool("kl_environment_service_add").execute("c1", { id: "devstack", service: { name: "nats", image: "nats:2", ports: [4222] } }, undefined, undefined, undefined);
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
    // The second is taken anyway — an ask is never refused because that workspace is busy — and
    // its turn answers the FIRST one by name, which is the only thing that can route it there.
    const a2 = await bench.ask("api", `[reply ${a1.exchange}] the first one is done`, two);
    assert.notEqual(a1.exchange, a2.exchange);

    await until(() => bench.exchanges.bySession(one).some((e) => e.dir === "in"), 5_000, "session one's answer");
    const back = bench.exchanges.bySession(one);
    assert.equal(back.find((e) => e.id === a1.exchange)!.state, "done", "the ask it named settled");
    assert.match(back.find((e) => e.dir === "in")!.text, /the first one is done/);
    // The asker of the answering turn is still waiting: its own ask was not the one answered.
    assert.deepEqual(bench.exchanges.bySession(two).map((e) => [e.dir, e.state]), [["out", "queued"]]);
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
    assert.match(out, /^this machine \(its own files and shell, nowhere else\):/);
    for (const g of ["workspace:", "environment:", "platform:"]) assert.ok(out.includes(g), g);
    assert.match(out, /kl_workspace_ask \[write\]/);
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
    const out = (await (tools.find((t) => t.name === "kl_workspace_progress") as any).execute("c1", { workspace: "api" }, undefined, undefined, undefined)).content[0].text as string;
    assert.deepEqual(seen.sort(), ["/exchanges?workspace=api", "/workspaces/api/messages?limit=10"]);
    assert.equal(out, ["asked of api:", "  running: add a health endpoint", "its session, latest last:", "  asked: [ask ask-1-x from session 1] add a health endpoint", "  ran edit", "  said: added /healthz"].join("\n"));
  } finally {
    restore();
    srv.close();
  }
});
