import { test, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import http from "node:http";
import { fakeTools } from "./fake-tools.ts";
import { setBackend, httpBackend } from "../src/engine/remote.ts";
import { Subs } from "../src/sub.ts";
import { Platform } from "../src/platform.ts";
import { Scheduler } from "../src/scheduler.ts";
import { Session, type Hooks } from "../src/session.ts";
import { SessionList } from "../src/sessions.ts";
import { readRows } from "../src/rows.ts";

// main's pod answers rev-parse; the clone's pod answers git push
let pushes = 0, rejectFirst = false;
const mainPod = await fakeTools((cmd) => cmd.includes("rev-parse") ? { exit_code: 0, stdout: "feat/x\n", stderr: "" } : { exit_code: 0, stdout: "", stderr: "" });
const clonePod = await fakeTools((cmd) => {
  if (cmd.startsWith("git push")) { pushes++; if (rejectFirst && pushes === 1) return { exit_code: 1, stdout: "", stderr: "! [rejected] non-fast-forward" }; return { exit_code: 0, stdout: "", stderr: "" }; }
  if (cmd.includes("rev-parse HEAD")) return { exit_code: 0, stdout: "abc123\n", stderr: "" };
  return { exit_code: 0, stdout: "", stderr: "" };
});
const deleted: string[] = [];
const api = http.createServer((req, res) => {
  if (req.method === "GET" && /\/v1\/workspaces\/m$/.test(req.url!)) return res.end(JSON.stringify({ id: "m", name: "main-tree" }));
  if (req.url!.includes("/tools")) return res.end(JSON.stringify({ address: req.url!.includes("/m/") ? mainPod.address : clonePod.address }));
  if (req.url!.includes("/clone")) { res.statusCode = 202; return res.end(JSON.stringify({ id: "c" })); }
  if (req.method === "DELETE") { deleted.push(req.url!); res.statusCode = 204; return res.end(); }
  res.statusCode = 404; res.end("{}");
});
await new Promise<void>((r) => api.listen(0, "127.0.0.1", r));
after(() => { api.close(); mainPod.close(); clonePod.close(); });
const platform = new Platform(`http://127.0.0.1:${(api.address() as { port: number }).port}`, "t");

const world = (childAnswer = "done: feature landed") => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "sub-"));
  const list = new SessionList(dir);
  const hooks: Hooks = { delegate: (s, t, i) => subs.delegate(s, t, i), tell: () => {}, askPerson: async () => "" };
  // main delegates only on its first turn; once it has a child it just reports what came back,
  // so a delivered answer does not re-trigger a fresh spawn on every kick
  const sched = new Scheduler(list, (row) => new Session(row, list.logFile(row), async (c) =>
    row.tier === "sub" ? childAnswer : list.children(row.seq).length === 0
      ? c.tools.find((t) => t.name === "delegate")!.run(c.cwd, { target: "", instruction: "build it" }, {})
      : "ok, got the child's answer"
  , hooks));
  const subs = new Subs(list, sched, platform, async (ws) => setBackend(ws, httpBackend(await platform.tools(ws))));
  const main = list.create(undefined, { tier: "main", state: "open", workspace: "m" });
  return { list, sched, subs, main };
};
const settle = () => new Promise((r) => setTimeout(r, 60));

test("delegate clones, writes child user row then parent delegate row, and the child runs", async () => {
  pushes = 0; deleted.length = 0;
  const { list, sched, main } = world();
  sched.boot();
  sched.get(main.seq)!.receive("person", "go");
  sched.kick();
  await settle();
  const sub = list.children(main.seq)[0];
  assert.ok(sub, "a sub row exists");
  assert.equal(sub.workspace, "c");
  assert.equal(sub.target, "feat/x");
  const childRows = readRows(list.logFile(sub));
  assert.equal(childRows[0].kind, "user");
  const parentRows = readRows(list.logFile(main));
  assert.ok(parentRows.some((r) => r.kind === "delegate" && r.child === sub.seq));
  await settle();
  // the child answered: pushed once, parent got the answer with the commit, clone deleted, child closed
  assert.equal(pushes, 1);
  assert.match(clonePod.execs.find((c) => c.startsWith("git push"))!, /ssh:\/\/kl@127\.0\.0\.1\/home\/kl\/workspaces\/main-tree HEAD:feat\/x/);
  const got = readRows(list.logFile(main)).find((r) => r.kind === "user" && r.from === sub.seq) as { text: string };
  assert.match(got.text, /feature landed[\s\S]*abc123/);
  assert.deepEqual(deleted, ["/v1/workspaces/c"]);
  assert.equal(list.bySeq(sub.seq)?.state, "closed");
});

test("a non-fast-forward push is retried once after a rebase instruction", async () => {
  pushes = 0; rejectFirst = true; deleted.length = 0;
  const { list, sched, main } = world();
  sched.boot();
  sched.get(main.seq)!.receive("person", "go");
  sched.kick();
  await settle(); await settle(); await settle();
  const sub = list.children(main.seq)[0];
  const childRows = readRows(list.logFile(sub));
  assert.ok(childRows.some((r) => r.kind === "user" && /rebase onto main/.test(r.text)));
  assert.equal(pushes, 2);
  assert.equal(list.bySeq(sub.seq)?.state, "closed");
  rejectFirst = false;
});

test("a top-tier session delegates to a main by workspace name", async () => {
  const { list, sched, subs, main } = world();
  const top = list.create(undefined, { tier: "top", state: "open" });
  const topSession = sched.get(top.seq) ?? (sched.boot(), sched.get(top.seq)!);
  const got = await subs.delegate(topSession, "m", "review the pr");
  assert.match(got, /delegated to m, waiting/);
  const topRows = readRows(list.logFile(top));
  assert.ok(topRows.some((r) => r.kind === "delegate" && r.child === main.seq));
  const mainRows = readRows(list.logFile(main));
  assert.ok(mainRows.some((r) => r.kind === "user" && r.from === top.seq && r.text === "review the pr"));

  const missing = await subs.delegate(topSession, "no-such-workspace", "do it");
  assert.match(missing, /error: no main named no-such-workspace/);
});
