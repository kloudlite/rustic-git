import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import workspaceTools, { askLine, askPreview, toIde, fromIde, forbidden, gitSshHost, joinCursor, mutates, onlyWaits, procTitle, shellNote, splitCursor, ToolServer, resolveFromApi } from "../../pi/workspace-tools.ts";
import kloudlite, { call } from "../../pi/kloudlite.ts";

test("pi's tools become the tool server's calls", () => {
  assert.deepEqual(toIde("edit", { path: "a.rs", edits: [{ oldText: "x", newText: "y" }] }), { tool: "edit", args: { path: "a.rs", edits: [{ old: "x", new: "y" }] } });
  assert.deepEqual(toIde("bash", { command: "make", timeout: 5 }), { tool: "exec", args: { cmd: "make", timeout_ms: 5000 } });
  assert.equal((toIde("bash", { command: "make", timeout: 3600 }).args as { timeout_ms: number }).timeout_ms, 600_000);
  assert.equal(toIde("grep", { pattern: "a.b", literal: true }).args.pattern, "a\\.b");
  assert.deepEqual(toIde("find", { pattern: "**/*.ts", path: "src" }), { tool: "glob", args: { pattern: "**/*.ts", cwd: "src" } });
  assert.deepEqual(toIde("ls", {}), { tool: "exec", args: { cmd: ["ls", "-1Ap", "--", "."], head: 500 } });
  assert.throws(() => toIde("kl_workspaces", {}), /no workspace tool kl_workspaces/);
});

test("an answer becomes text; a refusal is only its error", () => {
  assert.deepEqual(fromIde("bash", 200, { exit_code: 2, stdout: "out", stderr: "err", timed_out: false }), { content: [{ type: "text", text: "out\nerr\n[exit 2]" }], isError: true });
  assert.deepEqual(fromIde("read", 403, { error: "../x: outside the home" }), { content: [{ type: "text", text: "../x: outside the home" }], isError: true });
  assert.equal(fromIde("grep", 200, { matches: [{ path: "a.rs", line: 3, text: "fn a()" }], truncated: false }).content[0].text, "a.rs:3: fn a()");
  assert.equal(fromIde("grep", 200, { matches: [{ path: "a.rs", line: 3, text: "fn a()", context: "fn a() {\n  x()\n}" }] }).content[0].text, "a.rs:3: fn a()\nfn a() {\n  x()\n}");
  assert.equal(fromIde("find", 200, { paths: ["a", "b", "c"] }, 2).content[0].text, "a\nb");
  assert.equal(fromIde("edit", 200, { path: "/home/kl/workspaces/api/a.rs", applied: 1 }).isError, false);
});

test("a call goes to the looked-up address, a dead address is looked up once more, and a refusal names why", async () => {
  const seen: string[] = [];
  const srv = http.createServer((req, res) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      seen.push(`${req.url} ${b}`);
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ exit_code: 0, stdout: "ide-kl", stderr: "" }));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const live = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  const answers = ["127.0.0.1:1", live]; // the first address is a pod that has gone
  let asked = 0;
  const s = new ToolServer("api", async () => answers[asked++]);
  const r = await s.call(toIde("bash", { command: "echo ide-$(id -un)" }));
  assert.equal(r.status, 200);
  assert.equal(asked, 2);
  assert.deepEqual(seen, ['/tools/exec {"cmd":"echo ide-$(id -un)","timeout_ms":120000}']);

  const stopped = new ToolServer("api", async () => { throw new Error("workspace api is stopped; start it to run tools"); });
  await assert.rejects(stopped.call(toIde("ls", {})), /is stopped; start it to run tools/);
  const gone = new ToolServer("api", async () => "127.0.0.1:1");
  // Never the address: the model repeated "did not answer at 127.0.0.1:7788: fetch failed" to the
  // person (owner, 2026-09-18). What it tried is stderr's business.
  await assert.rejects(gone.call(toIde("ls", {})), /the workspace's tools did not answer; is it running\?/);
  srv.close();
});

test("with neither KL_TOOLS_ADDRESS nor KL_TEAM set, resolving refuses before any api call", async () => {
  const savedTeam = process.env.KL_TEAM;
  const savedAddr = process.env.KL_TOOLS_ADDRESS;
  delete process.env.KL_TEAM;
  delete process.env.KL_TOOLS_ADDRESS;
  try {
    await assert.rejects(resolveFromApi("api"), /KL_TEAM is not set/);
  } finally {
    if (savedTeam !== undefined) process.env.KL_TEAM = savedTeam;
    if (savedAddr !== undefined) process.env.KL_TOOLS_ADDRESS = savedAddr;
  }
});

test("a malformed address, from KL_TOOLS_ADDRESS or from /v1, is refused", async () => {
  const saved = process.env.KL_TOOLS_ADDRESS;
  process.env.KL_TOOLS_ADDRESS = "not-an-address";
  try {
    await assert.rejects(resolveFromApi("api"), /not a host:port address/);
  } finally {
    if (saved === undefined) delete process.env.KL_TOOLS_ADDRESS;
    else process.env.KL_TOOLS_ADDRESS = saved;
  }
});

test("a 409 from the tool server clears the cached address and is looked up once more", async () => {
  let hits = 0;
  const srv = http.createServer((req, res) => {
    hits++;
    if (hits === 1) {
      res.writeHead(409, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: "workspace not ready" }));
      return;
    }
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ exit_code: 0, stdout: "ok", stderr: "" }));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const addr = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  let asked = 0;
  const s = new ToolServer("api", async () => {
    asked++;
    return addr;
  });
  const r = await s.call(toIde("ls", {}));
  assert.equal(r.status, 200);
  assert.equal(asked, 2);
  srv.close();
});

test("a 409 from /v1 is re-asked once against a fake api, then the workspace's tool server is used", async () => {
  const toolSrv = http.createServer((_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ exit_code: 0, stdout: "ok", stderr: "" }));
  });
  await new Promise<void>((r) => toolSrv.listen(0, "127.0.0.1", r));
  const toolAddr = `127.0.0.1:${(toolSrv.address() as { port: number }).port}`;

  let getCalls = 0;
  const api = http.createServer((_req, res) => {
    getCalls++;
    if (getCalls === 1) {
      res.writeHead(409, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: "workspace api is starting" }));
      return;
    }
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ address: toolAddr }));
  });
  await new Promise<void>((r) => api.listen(0, "127.0.0.1", r));
  const apiUrl = `http://127.0.0.1:${(api.address() as { port: number }).port}`;

  const cfgDir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-cfg-"));
  const restore = withToken(path.join(cfgDir, "token"), "t", apiUrl);
  const savedTeam = process.env.KL_TEAM;
  const savedAddr = process.env.KL_TOOLS_ADDRESS;
  process.env.KL_TEAM = "acme";
  delete process.env.KL_TOOLS_ADDRESS;
  try {
    const server = new ToolServer("api", resolveFromApi);
    const r = await server.call(toIde("ls", {}));
    assert.equal(r.status, 200);
    assert.equal(getCalls, 2);
  } finally {
    restore();
    if (savedTeam === undefined) delete process.env.KL_TEAM;
    else process.env.KL_TEAM = savedTeam;
    if (savedAddr !== undefined) process.env.KL_TOOLS_ADDRESS = savedAddr;
    toolSrv.close();
    api.close();
    fs.rmSync(cfgDir, { recursive: true, force: true });
  }
});

/** Points kloudlite.ts at a token file and an api; returns the undo. */
function withToken(file: string | undefined, tok: string | undefined, api: string) {
  const saved = { f: process.env.KL_TOOL_TOKEN_FILE, a: process.env.KL_API_URL };
  if (file && tok !== undefined) fs.writeFileSync(file, tok);
  if (file) process.env.KL_TOOL_TOKEN_FILE = file;
  else delete process.env.KL_TOOL_TOKEN_FILE;
  process.env.KL_API_URL = api;
  return () => {
    for (const [k, v] of [["KL_TOOL_TOKEN_FILE", saved.f], ["KL_API_URL", saved.a]] as const) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  };
}

async function fakeApi(handler: http.RequestListener) {
  const srv = http.createServer(handler);
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  return { srv, url: `http://127.0.0.1:${(srv.address() as { port: number }).port}` };
}

test("call re-reads the token file between calls", async () => {
  const seen: string[] = [];
  const { srv, url } = await fakeApi((req, res) => (seen.push(String(req.headers.authorization)), res.end("{}")));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-tok-"));
  const file = path.join(dir, "token");
  const restore = withToken(file, "one", url);
  try {
    await call("GET", "/v1/quota");
    fs.writeFileSync(file, "two\n");
    await call("GET", "/v1/quota");
    assert.deepEqual(seen, ["Bearer one", "Bearer two"]);
  } finally {
    restore();
    srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a missing token file throws the sign-in sentence without a request", async () => {
  let asked = 0;
  const { srv, url } = await fakeApi((_req, res) => (asked++, res.end("{}")));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-tok-"));
  for (const restore of [withToken(path.join(dir, "absent"), undefined, url), withToken(path.join(dir, "empty"), "", url), withToken(undefined, undefined, url)]) {
    try {
      await assert.rejects(call("GET", "/v1/quota"), { message: "sign in on the Kloudlite desktop app" });
    } finally {
      restore();
    }
  }
  assert.equal(asked, 0);
  srv.close();
  fs.rmSync(dir, { recursive: true, force: true });
});

test("a 401 maps to the sign-in sentence", async () => {
  const { srv, url } = await fakeApi((_req, res) => (res.writeHead(401), res.end("expired")));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-tok-"));
  const restore = withToken(path.join(dir, "token"), "t", url);
  try {
    assert.deepEqual(await call("GET", "/v1/quota"), { status: 401, data: "sign in on the Kloudlite desktop app (your desktop session ended or the bench was stopped)" });
  } finally {
    restore();
    srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a background command and the process tool are the tool server's own process calls", () => {
  assert.deepEqual(toIde("bash", { command: "npm run dev", background: true }), { tool: "exec", args: { cmd: "npm run dev", detach: true } });
  assert.deepEqual(toIde("process", { action: "start", command: "vite" }), { tool: "exec", args: { cmd: "vite", detach: true } });
  assert.deepEqual(toIde("process", { action: "list" }), { tool: "process_list", args: {} });
  assert.deepEqual(toIde("process", { action: "logs", id: "p1", since: 40 }), { tool: "process_output", args: { id: "p1", since: 40, since_err: 0 } });
  assert.deepEqual(toIde("process", { action: "logs", id: "p1" }), { tool: "process_output", args: { id: "p1", since: 0, since_err: 0 } });
  assert.deepEqual(toIde("process", { action: "stop", id: "p1", signal: "KILL" }), { tool: "process_kill", args: { id: "p1", signal: "KILL" } });
  assert.deepEqual(toIde("process", { action: "write", id: "p1", data: "y\n" }), { tool: "process_write", args: { id: "p1", data: "y\n" } });
  assert.throws(() => toIde("process", { action: "restart" }), /no process action restart/);

  // A detached answer is an id, never output; a page of logs says where to read from next.
  assert.match(fromIde("bash", 200, { id: "p3" }).content[0].text, /^started in the background as process p3/);
  assert.equal(fromIde("process", 200, { processes: [{ id: "p1", state: "running", cmd: "vite", exit_code: null }] }).content[0].text, "p1 running vite");
  assert.equal(fromIde("process", 200, { processes: [] }).content[0].text, "nothing running");
  assert.equal(fromIde("process", 200, { stdout: "ready", stderr: "", next: 120, state: "running" }).content[0].text, "ready\n[running; next 120]");
  assert.equal(fromIde("process", 200, { state: "exited" }).content[0].text, "the process is exited");
  assert.equal(fromIde("process", 200, { bytes: 2 }).content[0].text, "wrote 2 bytes to its stdin");
});

/**
 * A bench that answers every proposal the way a test needs. Since 2026-09-18 a mutating ide tool
 * asks first, exactly as a `kl_*` write does, so a test that runs one has to stand in for the
 * person: `yes` for the ordinary path, `no` for a refusal.
 */
async function fakeAsker(answer: "yes" | "no" = "yes") {
  const srv = http.createServer((_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ answer }));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const saved = process.env.KL_BENCH_URL;
  process.env.KL_BENCH_URL = `http://127.0.0.1:${(srv.address() as { port: number }).port}`;
  return () => {
    if (saved === undefined) delete process.env.KL_BENCH_URL; else process.env.KL_BENCH_URL = saved;
    srv.close();
  };
}

test("a background command reaches the tool server and is mirrored into the harness's process table", async () => {
  const seen: { tool: string; body: any }[] = [];
  const procs = [{ id: "p1", cmd: "npm run dev", started_at: "2026-09-17T06:00:00Z", state: "running", exit_code: null }];
  const srv = http.createServer((req, res) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      const tool = req.url!.split("/").pop()!;
      seen.push({ tool, body: JSON.parse(b || "{}") });
      const answer =
        tool === "process_list" ? { processes: procs }
        : tool === "exec" ? { id: "p1" }
        : tool === "process_output" ? { stdout: "listening", stderr: "", next: 9, state: "running" }
        : { state: "exited" };
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(answer));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const stopAsking = await fakeAsker("yes");
  const saved = { a: process.env.KL_TOOLS_ADDRESS, w: process.env.KL_TOOLS_WORKSPACE };
  process.env.KL_TOOLS_ADDRESS = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  process.env.KL_TOOLS_WORKSPACE = "api";
  const tools: Record<string, { execute: (...a: any[]) => Promise<any> }> = {};
  const widgets: Record<string, string[]> = {};
  const ctx = { ui: { setWidget: (k: string, lines: string[]) => (widgets[k] = lines) } };
  try {
    workspaceTools({ registerTool: (t: { name: string }) => (tools[t.name] = t as never), on: () => undefined } as never);
    const started = await tools.bash.execute("c1", { command: "npm run dev", background: true }, undefined, undefined, ctx);
    assert.match(started.content[0].text, /process p1/);
    assert.deepEqual(seen.map((x) => x.tool), ["exec", "process_list"], "the table is re-read after a detach");
    assert.equal(seen[0].body.detach, true);
    // The row the desktop's Processes panel draws, straight from the tool server's list.
    assert.deepEqual(JSON.parse(widgets["harness:procs"][0]), [{ id: "p1", name: "npm run dev", command: "npm run dev", started: Date.parse("2026-09-17T06:00:00Z") }]);

    // Logs page by `since`, and a stop both kills and re-reads the table.
    const logs = await tools.process.execute("c2", { action: "logs", id: "p1", since: 9 }, undefined, undefined, ctx);
    assert.equal(logs.content[0].text, "listening\n[running; next 9]");
    assert.deepEqual(seen.at(-1), { tool: "process_output", body: { id: "p1", since: 9, since_err: 0 } });
    procs[0] = { ...procs[0], state: "exited", exit_code: 0 };
    await tools.process.execute("c3", { action: "stop", id: "p1" }, undefined, undefined, ctx);
    assert.deepEqual(seen.map((x) => x.tool).slice(-2), ["process_kill", "process_list"]);
    assert.equal(JSON.parse(widgets["harness:procs"][0])[0].code, 0, "an exited process settles in the table");
  } finally {
    for (const [k, v] of [["KL_TOOLS_ADDRESS", saved.a], ["KL_TOOLS_WORKSPACE", saved.w]] as const) if (v === undefined) delete process.env[k]; else process.env[k] = v;
    stopAsking();
    srv.close();
  }
});

test("the shell may not go behind the tools at the platform", () => {
  // Ordinary work, CLI included: nothing here is about the platform's own back doors.
  for (const ok of [
    "kl pkg list",
    "kl env switch devstack",
    "kl container ls",
    "mongosh mongodb://db:27017 --eval 'db.stats()'",
    "npm install && npm run build",
    "grep -rn 'v1' src/api.ts",
    "curl http://api:8080/v1/orders",
    "src/lib/kl.ts",
  ]) assert.equal(forbidden(ok), undefined, ok);

  // Each one of these was watched on the fleet.
  for (const no of [
    "cat $KL_TOOL_TOKEN_FILE",
    "ls /etc/kloudlite",
    "read /opt/harness/pi/catalog.ts",
    "strings /usr/local/bin/kl | grep v1",
    "grep -a '/v1/' /usr/local/bin/kl",
    "xxd /usr/local/bin/kl | head",
    "grep -r 'workspaces' .bench/workspaces/api/thread.jsonl",
    "node -e 'fetch(process.env.KL_API_URL)'",
    "curl https://api.kloudlite.io/v1/workspaces",
  ]) assert.match(forbidden(no) ?? "", /^refused: the platform is reached only through kl_\* tools/, no);
});

test("a refused command never reaches the tool server", async () => {
  const saved = { a: process.env.KL_TOOLS_ADDRESS, w: process.env.KL_TOOLS_WORKSPACE };
  // An address nothing listens on: if the gate let it through, the call would fail differently.
  process.env.KL_TOOLS_ADDRESS = "127.0.0.1:1";
  process.env.KL_TOOLS_WORKSPACE = "api";
  const tools: Record<string, { execute: (...a: any[]) => Promise<any> }> = {};
  try {
    workspaceTools({ registerTool: (t: { name: string }) => (tools[t.name] = t as never), on: () => undefined } as never);
    for (const [tool, args] of [
      ["bash", { command: "cat $KL_TOOL_TOKEN_FILE" }],
      ["read", { path: "/opt/harness/pi/kloudlite.ts" }],
      ["grep", { pattern: "token", path: ".bench/sessions" }],
      ["process", { action: "start", command: "strings /usr/local/bin/kl" }],
    ] as const) {
      const r = await tools[tool].execute("c1", args, undefined, undefined, undefined);
      assert.equal(r.isError, true, tool);
      assert.match(r.content[0].text, /^refused: the platform is reached only through kl_\* tools/, tool);
    }
  } finally {
    for (const [k, v] of [["KL_TOOLS_ADDRESS", saved.a], ["KL_TOOLS_WORKSPACE", saved.w]] as const) if (v === undefined) delete process.env[k]; else process.env[k] = v;
  }
});

test("a command that only waits is refused; a sleep inside real work is not", () => {
  for (const no of ["sleep 20", "  sleep 30; echo waited", "sleep 5 && echo done", "timeout 60 sleep 60", "sleep 2; date"]) {
    assert.equal(onlyWaits(no), true, no);
    assert.equal(forbidden(no), "refused: tools wait for you; ask the tool again instead of sleeping", no);
  }
  for (const ok of ["npm test", "npm run dev & sleep 2; curl localhost:3000", "sleep 1 && npm test", "echo hi", "kl pkg list"]) {
    assert.equal(forbidden(ok), undefined, ok);
  }
});

test("code and containers run in this machine, as argv the shell cannot splice", () => {
  const saved = process.env.KL_GIT_SSH_HOST;
  process.env.KL_GIT_SSH_HOST = "git.khost.dev";
  try {
    assert.deepEqual(toIde("kl_repo_clone", { repo: "kloudlite/rustic-git" }), {
      tool: "exec",
      args: { cmd: ["git", "clone", "ssh://git@git.khost.dev/kloudlite/rustic-git.git"], timeout_ms: 600_000 },
    });
    assert.deepEqual(toIde("kl_repo_clone", { repo: "ada/api", dir: "svc" }).args.cmd.slice(-1), ["svc"]);
    assert.throws(() => toIde("kl_repo_clone", { repo: "notowner" }), /is not owner\/name/);
    // A build is long: it detaches, and `process logs` is how it is watched.
    assert.deepEqual(toIde("kl_container_build", { context: ".", tag: "api:1" }), { tool: "exec", args: { cmd: ["kl", "container", "build", "-t", "api:1", "."], detach: true } });
    assert.deepEqual(toIde("kl_container_build", { context: "svc", tag: "api:1", dockerfile: "Dockerfile.dev" }).args.cmd, ["kl", "container", "build", "-t", "api:1", "-f", "Dockerfile.dev", "svc"]);
    assert.deepEqual(toIde("kl_container_push", { from: "api:1", to: "api:latest" }).args.cmd, ["kl", "container", "push", "api:1", "api:latest"]);
    assert.deepEqual(toIde("kl_images", {}).args.cmd, ["kl", "container", "images"]);
    assert.deepEqual(toIde("kl_images", { owner: "acme" }).args.cmd, ["kl", "container", "images", "acme"]);
    // The detached build answers an id, like every other background command.
    assert.match(fromIde("kl_container_build", 200, { id: "p7" }).content[0].text, /process p7/);

    delete process.env.KL_GIT_SSH_HOST;
    assert.throws(() => gitSshHost({} as NodeJS.ProcessEnv), /was not told where git lives/);
  } finally {
    if (saved === undefined) delete process.env.KL_GIT_SSH_HOST; else process.env.KL_GIT_SSH_HOST = saved;
  }
});

test("a kl CLI verb in the shell is allowed, and points at the tool that does it properly", () => {
  // It is their machine and their CLI: the work happens. The note is where the tool is.
  assert.equal(shellNote("kl env switch devstack"), "note: tool_search 'env switch' has a tool for this");
  assert.equal(shellNote("cd app && kl container build -t api:1 ."), "note: tool_search 'container build' has a tool for this");
  assert.equal(shellNote("kl pkg add ripgrep"), "note: tool_search 'pkg add' has a tool for this");
  // Not a platform verb, and not the word kl inside something else.
  assert.equal(shellNote("npm run dev"), undefined);
  assert.equal(shellNote("./scripts/klingon.sh"), undefined);
  assert.equal(shellNote("kl ide serve"), undefined);
  assert.equal(forbidden("kl env switch devstack"), undefined, "allowed, not refused");
});

test("a process row reads as a name, not an argv", () => {
  // What the model gives, when it gives one.
  assert.equal(procTitle("npm run dev", "svelte dev server"), "svelte dev server");
  // Otherwise: where it runs and what it runs, with the `cd` that a shell needs and a person does not.
  assert.equal(procTitle("cd /home/kl/workspaces/svelte-app && npm run dev"), "svelte-app: npm run dev");
  assert.equal(procTitle("cd api/ ; go run ./cmd/server"), "api: go run ./cmd/server");
  assert.equal(procTitle("npm run dev"), "npm run dev");
  assert.equal(procTitle("  cargo watch -x test  "), "cargo watch -x test");
  assert.equal(procTitle("cd x && " + "a".repeat(200)).length, 60, "a row is a row, not a paragraph");
});

test("long output reminds the model that it is the only one reading it", async () => {
  const many = Array.from({ length: 60 }, (_, i) => `line ${i}`).join("\n");
  const srv = http.createServer((req, res) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(req.url!.endsWith("/exec") ? { exit_code: 0, stdout: many, stderr: "" } : { content: "short\n" }));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const stopAsking = await fakeAsker("yes");
  const saved = { a: process.env.KL_TOOLS_ADDRESS, w: process.env.KL_TOOLS_WORKSPACE };
  process.env.KL_TOOLS_ADDRESS = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  process.env.KL_TOOLS_WORKSPACE = "api";
  const tools: Record<string, { execute: (...a: any[]) => Promise<any> }> = {};
  try {
    workspaceTools({ registerTool: (t: { name: string }) => (tools[t.name] = t as never), on: () => undefined } as never);
    const big = await tools.bash.execute("c1", { command: "cat log" }, undefined, undefined, undefined);
    assert.equal(big.content.at(-1).text, "only you see this output; relay what the person needs");
    // A short answer gets no lecture.
    const small = await tools.read.execute("c2", { path: "a.ts" }, undefined, undefined, undefined);
    assert.ok(!small.content.some((c: { text: string }) => c.text.includes("only you see this")), JSON.stringify(small));
  } finally {
    for (const [k, v] of [["KL_TOOLS_ADDRESS", saved.a], ["KL_TOOLS_WORKSPACE", saved.w]] as const) if (v === undefined) delete process.env[k]; else process.env[k] = v;
    stopAsking();
    srv.close();
  }
});

test("the tool server client does not serialise: pi runs sibling calls at the same time", async () => {
  // extensions.md: sibling tool calls of one assistant message are "preflighted sequentially, then
  // executed concurrently". Nothing in our client may put them back in a queue.
  let inFlight = 0;
  let most = 0;
  const srv = http.createServer((req, res) => {
    inFlight++;
    most = Math.max(most, inFlight);
    setTimeout(() => {
      inFlight--;
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ exit_code: 0, stdout: "ok", stderr: "" }));
    }, 60);
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const at = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  try {
    const server = new ToolServer("api", async () => at);
    const began = Date.now();
    await Promise.all(Array.from({ length: 5 }, (_, i) => server.call(toIde("bash", { command: `job ${i}` }))));
    assert.ok(most >= 4, `only ${most} were in flight at once`);
    assert.ok(Date.now() - began < 250, "five 60ms jobs took as long as five serial ones");
  } finally {
    srv.close();
  }
});

/**
 * A process has a cursor per stream — `since`/`next` for stdout, `since_err`/`next_err` for stderr
 * (81621d02). The model carries ONE number: two would be two things to get wrong, and before this
 * the stderr half was read from 0 on every poll, so a build's log came back whole every time
 * (owner, 2026-09-18).
 */
test("process logs: one cursor for the model, both offsets underneath", () => {
  // The first read starts at the start of both.
  assert.deepEqual(toIde("process", { action: "logs", id: "p1" }), { tool: "process_output", args: { id: "p1", since: 0, since_err: 0 } });
  assert.deepEqual(toIde("process", { action: "logs", id: "p1", since: 0 }).args, { id: "p1", since: 0, since_err: 0 });

  // What a read answers with is what the next read asks for, and neither stream is re-read.
  const answered = fromIde("process", 200, { stdout: "a\n", stderr: "b\n", next: 64, next_err: 40, state: "running", exit_code: null });
  const next = Number(/next (\d+)/.exec(answered.content[0].text as string)![1]);
  assert.deepEqual(splitCursor(next), { since: 64, since_err: 40 });
  assert.deepEqual(toIde("process", { action: "logs", id: "p1", since: next }).args, { id: "p1", since: 64, since_err: 40 });

  // Both streams' dropped bytes are one number to the person reading it.
  assert.match(fromIde("process", 200, { stdout: "", stderr: "", next: 1, next_err: 2, dropped: 10, dropped_err: 5, state: "running" }).content[0].text as string, /15 bytes dropped/);

  // The packing survives a large stdout offset (a 4 MiB ring is well inside the low half).
  assert.deepEqual(splitCursor(joinCursor(4_194_304, 1_048_576)), { since: 4_194_304, since_err: 1_048_576 });
  // Rubbish from a model is read as "from the start", never as NaN.
  assert.deepEqual(splitCursor("nonsense"), { since: 0, since_err: 0 });
  assert.deepEqual(splitCursor(-5), { since: 0, since_err: 0 });
});

/**
 * A session's HANDS ask the way its platform writes always have (owner, 2026-09-18: "it should
 * follow the same rules when mutating states and editing files"). A file written, a command run and
 * a workspace created are one rule now; a read is still a read.
 */
test("what mutates asks first; what reads does not", () => {
  for (const [name, p] of [
    ["write", { path: "a.ts" }],
    ["edit", { path: "a.ts" }],
    ["patch", { diff: "--- a" }],
    ["bash", { command: "rm -rf build" }],
    ["process", { action: "start", command: "npm run dev" }],
    ["process", { action: "stop", id: "p1" }],
    ["process", { action: "write", id: "p1", data: "y" }],
  ] as [string, Record<string, unknown>][])
    assert.equal(mutates(name, p), true, `${name} ${JSON.stringify(p)}`);

  for (const [name, p] of [
    ["read", { path: "a.ts" }],
    ["grep", { pattern: "x" }],
    ["find", { pattern: "*.ts" }],
    ["ls", {}],
    ["process", { action: "logs", id: "p1" }],
    ["process", { action: "list" }],
  ] as [string, Record<string, unknown>][])
    assert.equal(mutates(name, p), false, `${name} ${JSON.stringify(p)}`);
});

test("the card says what it would do, and shows enough to judge it by", () => {
  // The path for a file, the command for anything that runs — never the tool's own name.
  assert.equal(askLine("api", "write", { path: "src/main.ts" }), "Write src/main.ts in api");
  assert.equal(askLine("api", "edit", { path: "src/main.ts" }), "Edit src/main.ts in api");
  assert.equal(askLine("api", "bash", { command: "npm test\nnext line" }), "Run in api: npm test");
  assert.equal(askLine("api", "process", { action: "start", command: "npm run dev" }), "Run in api: npm run dev");
  assert.equal(askLine("api", "process", { action: "stop", id: "p1" }), "Stop process p1 in api");

  // The body: a write shows what it would write, an edit shows it as a diff, a command is itself.
  assert.equal(askPreview("write", { content: "hello" }), "hello");
  assert.equal(askPreview("edit", { edits: [{ oldText: "a", newText: "b" }] }), "- a\n+ b");
  assert.equal(askPreview("bash", { command: "npm test" }), "npm test");
  assert.equal(askPreview("read", { path: "a.ts" }), undefined);
  // A huge file is cut: the card is something to read, not the file itself.
  const long = askPreview("write", { content: "x".repeat(5_000) })!;
  assert.ok(long.length < 2_100 && long.endsWith("…"), String(long.length));
});

/**
 * End to end: an edit in BUILD mode raises the card and waits on it, and a no is a no — the tool
 * server is never called at all.
 */
test("an edit waits for the person, and a refusal never reaches the workspace", async () => {
  const reached: string[] = [];
  const srv = http.createServer((req, res) => {
    reached.push(req.url!);
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ path: "a.ts", bytes: 5 }));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const saved = { a: process.env.KL_TOOLS_ADDRESS, w: process.env.KL_TOOLS_WORKSPACE };
  process.env.KL_TOOLS_ADDRESS = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  process.env.KL_TOOLS_WORKSPACE = "api";
  const tools: Record<string, { execute: (...a: any[]) => Promise<any> }> = {};
  try {
    workspaceTools({ registerTool: (t: { name: string }) => (tools[t.name] = t as never), on: () => undefined } as never);

    // NO: the card is drawn, the person declines, and nothing is written.
    const stopNo = await fakeAsker("no");
    const cards: unknown[] = [];
    const ctx = { ui: { setWidget: (_k: string, lines: string[]) => cards.push(JSON.parse(lines[0])) } };
    const declined = await tools.write.execute("c1", { path: "a.ts", content: "hello" }, undefined, undefined, ctx);
    stopNo();
    assert.equal(declined.isError, true);
    assert.match(declined.content[0].text, /declined by the person/);
    assert.deepEqual(reached, [], "a refused write never reaches the workspace");
    const card = cards[0] as { tool: string; summary: string; preview: string };
    assert.equal(card.tool, "write");
    assert.equal(card.summary, "Write a.ts in api");
    assert.equal(card.preview, "hello", "the card shows what it would write");

    // YES: it runs, once.
    const stopYes = await fakeAsker("yes");
    const done = await tools.write.execute("c2", { path: "a.ts", content: "hello" }, undefined, undefined, ctx);
    stopYes();
    assert.ok(!done.isError, JSON.stringify(done));
    assert.deepEqual(reached, ["/tools/write"]);

    // A READ is never asked about: no card, and it reaches the workspace with no bench at all.
    const before = cards.length;
    await tools.read.execute("c3", { path: "a.ts" }, undefined, undefined, ctx);
    assert.equal(cards.length, before, "a read draws no card");
    assert.deepEqual(reached, ["/tools/write", "/tools/read"]);
  } finally {
    for (const [k, v] of [["KL_TOOLS_ADDRESS", saved.a], ["KL_TOOLS_WORKSPACE", saved.w]] as const) if (v === undefined) delete process.env[k]; else process.env[k] = v;
    srv.close();
  }
});
